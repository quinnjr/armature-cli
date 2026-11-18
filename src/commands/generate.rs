//! Code generation commands, following the NestJS CLI conventions.
//!
//! The name argument carries the path (`armature generate controller
//! modules/products` → `src/modules/products/products_controller.rs`), each
//! element gets its own directory unless `--flat`, tests go to `tests/`, and
//! the element is declared and registered in the nearest `#[module(…)]`
//! (`--skip-import` to opt out). Every generator builds a [`Plan`] first, so
//! `--dry-run` shows the changes and a conflict never writes anything.
//!
//! | NestJS schematic | `armature generate` | file                          | registered in |
//! |------------------|---------------------|-------------------------------|---------------|
//! | application      | application         | new project (`armature new`)  |               |
//! | class (cl)       | class               | `<name>.rs`                   |               |
//! | configuration    | config              | `<name>_config.rs`            |               |
//! | controller (co)  | controller          | `<name>_controller.rs`        | controllers   |
//! | decorator (d)    | decorator           | `<name>_decorator.rs`         |               |
//! | filter (f)       | filter              | `<name>_filter.rs`            |               |
//! | gateway (ga)     | gateway             | `<name>_gateway.rs`           |               |
//! | guard (gu)       | guard               | `<name>_guard.rs`             |               |
//! | interceptor (itc)| interceptor         | `<name>_interceptor.rs`       |               |
//! | interface (itf)  | interface           | `<name>_interface.rs`         |               |
//! | library (lib)    | library             | `libs/<name>/` crate          | Cargo.toml    |
//! | middleware (mi)  | middleware          | `<name>_middleware.rs`        |               |
//! | module (mo)      | module              | `<dir>/mod.rs`                | imports       |
//! | pipe (pi)        | pipe                | `<name>_pipe.rs`              |               |
//! | provider (pr)    | provider            | `<name>_provider.rs`          | providers     |
//! | resolver (r)     | resolver            | `<name>_resolver.rs`          |               |
//! | resource (res)   | resource            | module + controller + service | imports       |
//! | service (s)      | service             | `<name>_service.rs`           | providers     |
//! | sub-app (app)    | sub-app             | `apps/<name>/` crate          | Cargo.toml    |

use colored::Colorize;
use heck::ToSnakeCase;
use std::path::PathBuf;

use crate::error::{CliError, CliResult};
use crate::generators::cargo_manifest::{self, DepEdit};
use crate::generators::find_project_root;
use crate::generators::plan::{Plan, Slot, relative_path};
use crate::generators::target::{Target, module_segments};
use crate::generators::wiring;
use crate::templates::{
    ComponentData, ControllerData, DtoData, EntityData, ModelData, ModuleData, ResourceData,
    TemplateRegistry,
};

/// Options shared by every generator (NestJS: `--flat`, `--no-spec`,
/// `--skip-import`, `--dry-run`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GenOptions {
    /// Do not create a directory for the element.
    pub flat: bool,
    /// Do not generate the test file.
    pub skip_tests: bool,
    /// Do not register the element in the nearest module.
    pub skip_import: bool,
    /// Print the changes without writing anything.
    pub dry_run: bool,
}

/// One generator invocation: the project, the plan and the templates.
struct Gen {
    plan: Plan,
    templates: TemplateRegistry,
    opts: GenOptions,
}

impl Gen {
    fn new(opts: GenOptions) -> CliResult<Self> {
        let root = find_project_root()?;
        Ok(Self {
            plan: Plan::new(&root),
            templates: TemplateRegistry::new(),
            opts,
        })
    }

    fn render<T: serde::Serialize>(&self, template: &str, data: &T) -> CliResult<String> {
        self.templates
            .render(template, data)
            .map_err(CliError::Template)
    }

    fn finish(self, summary: impl AsRef<str>) -> CliResult<()> {
        let dry_run = self.opts.dry_run;
        self.plan.apply(dry_run)?;
        if !dry_run {
            println!("\n{} {}", "✓".green().bold(), summary.as_ref());
        }
        Ok(())
    }

    /// Write one element (`<dir>/<name>_<kind>.rs`), its test, declare both
    /// and register the element. Returns the element file.
    fn element(&mut self, target: &Target, element: Element) -> CliResult<PathBuf> {
        let dir = target.dir(self.plan.src());
        let stem = target.stem(element.kind);
        let file = dir.join(format!("{stem}.rs"));
        self.plan.create(&file, element.content)?;
        self.plan.declare_file(&dir, &stem)?;

        if let Some(test) = element.test
            && !self.opts.skip_tests
        {
            let test_stem = format!("{stem}_test");
            let test_file = dir.join("tests").join(format!("{test_stem}.rs"));
            let test = retarget_test(&test, &stem, &self.plan.read(&file)?.unwrap_or_default());
            self.plan.create(&test_file, test)?;
            self.plan.declare_test(&dir, &test_stem)?;
        }

        if let Some(slot) = element.slot
            && !self.opts.skip_import
        {
            let mut item = module_segments(self.plan.src(), &file);
            item.push(element.type_name);
            self.plan
                .register(&item, slot, &dir, element.module.as_deref())?;
        }
        Ok(file)
    }

    /// Write a module: the directory's own `mod.rs` (or `<name>_module.rs`
    /// with `--flat`), imported by the nearest parent module. Returns the
    /// module file and its module path.
    fn module(&mut self, target: &Target, body: String) -> CliResult<PathBuf> {
        let src = self.plan.src().to_path_buf();
        let dir = target.dir(&src);
        let module_name = format!("{}Module", target.names.pascal);

        let (file, parent_dir) = if target.flat {
            let stem = target.stem("module");
            let file = dir.join(format!("{stem}.rs"));
            self.plan.create(&file, body)?;
            self.plan.declare_file(&dir, &stem)?;
            (file, dir.clone())
        } else {
            let file = self.plan.module_file(&dir)?;
            if let Some(existing) = self.plan.read(&file)?
                && let Some(name) = wiring::module_struct(&existing)
            {
                return Err(CliError::InvalidArgument(format!(
                    "{} already defines {name}",
                    self.plan.display(&file)
                )));
            }
            self.plan
                .declare_dirs(&module_segments(&src, &dir.join("mod.rs")))?;
            let existing = self.plan.read(&file)?.unwrap_or_default();
            self.plan.write(&file, merge_module(&existing, &body));
            (file, dir.parent().unwrap_or(&src).to_path_buf())
        };

        if !self.opts.skip_import {
            let mut item = module_segments(&src, &file);
            item.push(module_name);
            self.plan
                .register(&item, Slot::Imports, &parent_dir, None)?;
        }
        Ok(file)
    }

    /// Make sure the project's `Cargo.toml` has what the generated code
    /// needs: `armature` features and extra crates.
    fn require(&mut self, features: &[&str], crates: &[(&str, &str)]) -> CliResult<()> {
        let path = self.plan.root().join("Cargo.toml");
        let Some(mut manifest) = self.plan.read(&path)? else {
            return Ok(());
        };
        let mut changed = false;
        for feature in features {
            match cargo_manifest::ensure_armature_feature(&manifest, feature)? {
                DepEdit::Updated(m) => {
                    manifest = m;
                    changed = true;
                    self.plan.note(format!(
                        "enabled the `{feature}` feature of armature in Cargo.toml"
                    ));
                }
                DepEdit::AlreadyThere => {}
                DepEdit::Manual(why) => self.plan.note(format!(
                    "enable the `{feature}` feature of armature in Cargo.toml ({why})"
                )),
            }
        }
        for (name, spec) in crates {
            match cargo_manifest::ensure_dependency(&manifest, name, spec)? {
                DepEdit::Updated(m) => {
                    manifest = m;
                    changed = true;
                    self.plan
                        .note(format!("added `{name} = {spec}` to Cargo.toml"));
                }
                DepEdit::AlreadyThere => {}
                DepEdit::Manual(why) => self
                    .plan
                    .note(format!("add `{name} = {spec}` to Cargo.toml ({why})")),
            }
        }
        if changed {
            self.plan.write(&path, manifest);
        }
        Ok(())
    }

    fn component(&self, target: &Target) -> ComponentData {
        ComponentData {
            name_pascal: target.names.pascal.clone(),
            name_snake: target.names.snake.clone(),
            name_kebab: target.names.kebab.clone(),
        }
    }

    /// Element rendered from `template` (+ `test_template`) with the
    /// standard component data.
    fn simple(
        &self,
        target: &Target,
        kind: &'static str,
        type_suffix: &str,
        template: &str,
        test_template: Option<&str>,
        slot: Option<Slot>,
    ) -> CliResult<Element> {
        let data = self.component(target);
        Ok(Element {
            kind,
            type_name: format!("{}{type_suffix}", target.names.pascal),
            content: self.render(template, &data)?,
            test: test_template.map(|t| self.render(t, &data)).transpose()?,
            slot,
            module: None,
        })
    }
}

/// A generated source file.
struct Element {
    /// File suffix: `controller` → `<name>_controller.rs` (empty: `<name>.rs`).
    kind: &'static str,
    /// Type registered in the module.
    type_name: String,
    content: String,
    test: Option<String>,
    slot: Option<Slot>,
    /// Module to register in first (resource: the module just generated).
    module: Option<PathBuf>,
}

/// Test templates start with `use super::*;`, written for a test placed
/// next to the element. In `tests/<stem>_test.rs` the element is
/// `super::super::<stem>`: import its public items, and repeat its own
/// top-level `use` declarations (traits such as `Guard` or `Middleware`
/// must be in scope to call their methods).
fn retarget_test(test: &str, stem: &str, element: &str) -> String {
    let mut header = vec![
        "#[allow(unused_imports)]".to_string(),
        format!("use super::super::{stem}::*;"),
    ];
    let mut imports = top_level_uses(element);
    if !imports.iter().any(|u| u == "use armature::prelude::*;") {
        imports.push("use armature::prelude::*;".to_string());
    }
    let already: Vec<&str> = test.lines().map(str::trim).collect();
    for import in imports {
        if already.contains(&import.as_str()) {
            continue;
        }
        header.push("#[allow(unused_imports)]".to_string());
        // The element's `super` is the test's `super::super`.
        header.push(import.replacen("use super::", "use super::super::", 1));
    }
    let header = header.join("\n");

    let mut done = false;
    let lines: Vec<String> = test
        .lines()
        .map(|line| {
            if !done && line == "use super::*;" {
                done = true;
                header.clone()
            } else {
                line.to_string()
            }
        })
        .collect();
    let mut out = lines.join("\n");
    if test.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Top-level (column 0) `use …;` declarations of a source file, multi-line
/// ones included, `pub use` excluded.
fn top_level_uses(source: &str) -> Vec<String> {
    let mut uses = Vec::new();
    let mut current: Option<String> = None;
    for line in source.lines() {
        if let Some(acc) = current.as_mut() {
            acc.push('\n');
            acc.push_str(line);
            if line.trim_end().ends_with(';') {
                uses.push(current.take().unwrap_or_default());
            }
        } else if line.starts_with("use ") {
            if line.trim_end().ends_with(';') {
                uses.push(line.trim_end().to_string());
            } else {
                current = Some(line.to_string());
            }
        }
    }
    uses
}

/// Add a module definition to an existing module file (inner doc comments
/// must stay at the top).
fn merge_module(existing: &str, module: &str) -> String {
    if existing.trim().is_empty() {
        return module.to_string();
    }
    let header: String = module
        .lines()
        .take_while(|l| l.starts_with("//!") || l.trim().is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    let body = &module[header.len()..];
    let mut out = String::new();
    if !existing.trim_start().starts_with("//!") {
        out.push_str(&header);
    }
    out.push_str(existing.trim_end());
    out.push_str("\n\n");
    out.push_str(body);
    out
}

/// Parse the name argument with the shared options.
fn target(name: &str, opts: &GenOptions) -> CliResult<Target> {
    Target::parse(name, opts.flat)
}

/// Comma-separated `-c`/`-p` lists.
fn split_list(list: Option<&str>) -> Vec<String> {
    list.map(|s| {
        s.split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

// =============================================================================
// NestJS schematics
// =============================================================================

/// `generate controller` (NestJS `co`).
pub async fn controller(name: &str, crud: bool, auth: bool, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let element = controller_element(&g, &t, crud, auth)?;
    g.element(&t, element)?;
    g.finish(format!(
        "Generated {}Controller{}",
        t.names.pascal,
        if crud { " with CRUD endpoints" } else { "" }
    ))
}

fn controller_element(g: &Gen, t: &Target, crud: bool, auth: bool) -> CliResult<Element> {
    let guard_attr = if auth {
        // The guard admits only requests carrying a *verified* `UserContext`,
        // so the generated controller documents what must run before it.
        "// Requires `armature::armature_auth::JwtAuthMiddleware` (app-wide or via\n\
         // `#[middleware(...)]`, which runs before guards) and an `AuthGuard` in\n\
         // scope, e.g. from `armature generate guard auth --guard-type auth`.\n\
         #[guard(AuthGuard)]\n"
            .to_string()
    } else {
        String::new()
    };
    let data = ControllerData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        name_kebab: t.names.kebab.clone(),
        base_path: t.names.kebab.clone(),
        guard_attr,
    };
    Ok(Element {
        kind: "controller",
        type_name: format!("{}Controller", t.names.pascal),
        content: g.render(
            if crud {
                "controller_crud"
            } else {
                "controller"
            },
            &data,
        )?,
        test: Some(g.render("controller_test", &data)?),
        slot: Some(Slot::Controllers),
        module: None,
    })
}

/// `generate service` (NestJS `s`).
pub async fn service(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(
        &t,
        "service",
        "Service",
        "service",
        Some("service_test"),
        Some(Slot::Providers),
    )?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}Service", t.names.pascal))
}

/// `generate provider` (NestJS `pr`): an injectable provider.
pub async fn provider(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(
        &t,
        "provider",
        "Provider",
        "provider",
        Some("provider_test"),
        Some(Slot::Providers),
    )?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}Provider", t.names.pascal))
}

/// `generate class` (NestJS `cl`): a plain struct.
pub async fn class(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(&t, "", "", "class", Some("class_test"), None)?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}", t.names.pascal))
}

/// `generate interface` (NestJS `itf`): a trait.
pub async fn interface(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(&t, "interface", "", "interface", None, None)?;
    g.element(&t, e)?;
    g.finish(format!("Generated trait {}", t.names.pascal))
}

/// `generate decorator` (NestJS `d`): a custom request extractor, the
/// equivalent of `createParamDecorator`.
pub async fn decorator(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(
        &t,
        "decorator",
        "",
        "decorator",
        Some("decorator_test"),
        None,
    )?;
    g.element(&t, e)?;
    g.finish(format!("Generated {} extractor", t.names.pascal))
}

/// `generate module` (NestJS `mo`). `-c`/`-p` list existing controllers and
/// services by their path (`-c modules/users` → `modules/users/users_controller.rs`).
pub async fn module(
    name: &str,
    controllers: Option<&str>,
    providers: Option<&str>,
    opts: GenOptions,
) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let src = g.plan.src().to_path_buf();
    let module_segments_of_new = if t.flat {
        let mut s = t.dir_segments();
        s.push(t.stem("module"));
        s
    } else {
        t.dir_segments()
    };

    let mut resolve = |list: Option<&str>, kind: &str, suffix: &str| -> CliResult<String> {
        let mut paths = Vec::new();
        for entry in split_list(list) {
            let other = Target::parse(&entry, false)?;
            let stem = other.stem(kind);
            let file = other.dir(&src).join(format!("{stem}.rs"));
            if !g.plan.exists(&file) {
                g.plan.note(format!(
                    "{} does not exist yet: generate it with `armature generate {kind} {entry}`",
                    g.plan.display(&file)
                ));
            }
            let mut item = other.dir_segments();
            item.push(stem);
            item.push(format!("{}{suffix}", other.names.pascal));
            paths.push(relative_path(&module_segments_of_new, &item));
        }
        Ok(paths.join(", "))
    };
    let controller_list = resolve(controllers, "controller", "Controller")?;
    let provider_list = resolve(providers, "service", "Service")?;

    let data = ModuleData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        controller_list,
        provider_list,
    };
    let body = g.render("module", &data)?;
    g.module(&t, body)?;
    g.finish(format!("Generated {}Module", t.names.pascal))
}

/// `generate resource` (NestJS `res`): module + controller + service, with
/// tests, in one directory.
pub async fn resource(name: &str, crud: bool, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;

    let entity_snake = wiring::singularize(&t.names.snake);
    let data = ResourceData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        base_path: t.names.kebab.clone(),
        entity_pascal: heck::AsPascalCase(&entity_snake).to_string(),
        entity_snake,
        crud,
    };
    let module = ModuleData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        controller_list: String::new(),
        provider_list: String::new(),
    };

    let module_file = g.module(&t, g.render("module", &module)?)?;
    let service = Element {
        kind: "service",
        type_name: format!("{}Service", t.names.pascal),
        content: g.render("resource_service", &data)?,
        test: Some(g.render("resource_service_test", &data)?),
        slot: Some(Slot::Providers),
        module: Some(module_file.clone()),
    };
    let controller = Element {
        kind: "controller",
        type_name: format!("{}Controller", t.names.pascal),
        content: g.render("resource_controller", &data)?,
        test: Some(g.render("resource_controller_test", &data)?),
        slot: Some(Slot::Controllers),
        module: Some(module_file),
    };
    // Even with --skip-import the resource's own module lists its parts.
    let skip_import = g.opts.skip_import;
    g.opts.skip_import = false;
    g.element(&t, controller)?;
    g.element(&t, service)?;
    g.opts.skip_import = skip_import;

    g.finish(format!(
        "Generated {}Module ({}Controller{}, {}Service)",
        t.names.pascal,
        t.names.pascal,
        if crud { " with CRUD endpoints" } else { "" },
        t.names.pascal
    ))
}

/// `generate middleware` (NestJS `mi`).
pub async fn middleware(name: &str, opts: GenOptions) -> CliResult<()> {
    simple_command(
        name,
        opts,
        "middleware",
        "Middleware",
        "middleware",
        Some("middleware_test"),
        None,
    )
    .await
}

/// `generate guard` (NestJS `gu`), implementation chosen by `--guard-type`.
pub async fn guard(name: &str, guard_type: &str, opts: GenOptions) -> CliResult<()> {
    let template = match guard_type {
        "auth" => "guard_auth",
        "role" => "guard_role",
        "permission" => "guard_permission",
        "apikey" => "guard_apikey",
        "ratelimit" => "guard_ratelimit",
        _ => "guard",
    };
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    // Identity-based guards read the verified `UserContext` of armature-auth.
    if matches!(
        template,
        "guard" | "guard_auth" | "guard_role" | "guard_permission"
    ) {
        g.require(&["auth"], &[])?;
    }
    // The shared guard test builds the guard with `new()` and a verified
    // user: it fits the identity guards without constructor arguments.
    let test = matches!(template, "guard" | "guard_auth").then_some("guard_test");
    let e = g.simple(&t, "guard", "Guard", template, test, None)?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}Guard", t.names.pascal))
}

/// `generate interceptor` (NestJS `itc`).
pub async fn interceptor(name: &str, opts: GenOptions) -> CliResult<()> {
    simple_command(
        name,
        opts,
        "interceptor",
        "Interceptor",
        "interceptor",
        Some("interceptor_test"),
        None,
    )
    .await
}

/// Crates some templates need, with the versions they were checked against.
const VALIDATOR: (&str, &str) = (
    "validator",
    r#"{ version = "0.20", features = ["derive"] }"#,
);
const CHRONO: (&str, &str) = ("chrono", r#"{ version = "0.4", features = ["serde"] }"#);
const ASYNC_GRAPHQL: (&str, &str) = ("async-graphql", r#""7""#);
const ARMATURE_WEBSOCKET: (&str, &str) = ("armature-websocket", r#""0.5""#);
// Same versions as armature-framework 0.8 uses, so nothing is built twice.
const ARMATURE_QUEUE: (&str, &str) = ("armature-queue", r#""0.7""#);
const ARMATURE_CRON: (&str, &str) = ("armature-cron", r#""0.6""#);
const ARMATURE_CACHE: (&str, &str) = (
    "armature-cache",
    r#"{ version = "0.5", default-features = false }"#,
);
const ARMATURE_EVENTS: (&str, &str) = ("armature-events", r#""0.3""#);
const ARMATURE_HTTP_CLIENT: (&str, &str) = ("armature-http-client", r#""0.3""#);
const UUID: (&str, &str) = ("uuid", r#""1""#);

/// Generator whose code needs `features` of armature and extra `crates`.
#[allow(clippy::too_many_arguments)]
async fn command_with_deps(
    name: &str,
    opts: GenOptions,
    kind: &'static str,
    type_suffix: &str,
    template: &str,
    test_template: Option<&str>,
    slot: Option<Slot>,
    features: &[&str],
    crates: &[(&str, &str)],
) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    g.require(features, crates)?;
    let e = g.simple(&t, kind, type_suffix, template, test_template, slot)?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}{type_suffix}", t.names.pascal))
}

/// `generate pipe` (NestJS `pi`).
pub async fn pipe(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "pipe",
        "Pipe",
        "pipe",
        Some("pipe_test"),
        None,
        &[],
        &[VALIDATOR],
    )
    .await
}

/// `generate filter` (NestJS `f`).
pub async fn exception_filter(name: &str, opts: GenOptions) -> CliResult<()> {
    simple_command(
        name,
        opts,
        "filter",
        "ExceptionFilter",
        "exception_filter",
        Some("exception_filter_test"),
        None,
    )
    .await
}

/// `generate gateway` (NestJS `ga`): a WebSocket handler.
pub async fn websocket(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "gateway",
        "WebSocket",
        "websocket",
        Some("websocket_test"),
        None,
        &[],
        &[ARMATURE_WEBSOCKET],
    )
    .await
}

/// `generate resolver` (NestJS `r`): a GraphQL resolver.
pub async fn graphql_resolver(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "resolver",
        "Query",
        "graphql_resolver",
        Some("graphql_resolver_test"),
        None,
        &[],
        &[ASYNC_GRAPHQL, CHRONO],
    )
    .await
}

/// `generate config` (NestJS `config`).
pub async fn config(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let data = serde_json::json!({
        "name_pascal": t.names.pascal,
        "name_snake": t.names.snake,
        "name_kebab": t.names.kebab,
        "name_upper": t.names.snake.to_uppercase(),
    });
    let e = Element {
        kind: "config",
        type_name: format!("{}Config", t.names.pascal),
        content: g.render("config", &data)?,
        test: None,
        slot: None,
        module: None,
    };
    g.element(&t, e)?;
    g.finish(format!("Generated {}Config", t.names.pascal))
}

/// `generate library` (NestJS `lib`): a workspace library crate in
/// `libs/<name>`, added to the workspace and as a dependency of the app.
pub async fn library(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = Target::parse(name, false)?;
    let root = g.plan.root().to_path_buf();
    let member = std::iter::once("libs".to_string())
        .chain(t.path.iter().cloned())
        .chain([t.names.kebab.clone()])
        .collect::<Vec<_>>()
        .join("/");
    let dir = root.join(&member);
    let crate_name = t.names.kebab.clone();

    let manifest_path = root.join("Cargo.toml");
    let manifest = g.plan.read(&manifest_path)?.unwrap_or_default();
    let deps = cargo_manifest::inherited_dependencies(&manifest)?;
    let data = serde_json::json!({
        "name_kebab": crate_name,
        "name_snake": t.names.snake,
        "name_pascal": t.names.pascal,
        "dependencies": deps,
    });
    g.plan.create(
        &dir.join("Cargo.toml"),
        g.render("library_cargo_toml", &data)?,
    )?;
    g.plan.create(
        &dir.join("src").join("lib.rs"),
        g.render("library_lib", &data)?,
    )?;

    let manifest = cargo_manifest::add_workspace_member(&manifest, &member)?;
    let manifest = cargo_manifest::add_path_dependency(&manifest, &crate_name, &member)?;
    g.plan.write(&manifest_path, manifest);
    g.plan.note(format!(
        "use it with `use {}::{{{}Module, {}Service}};` and add the module to your imports",
        t.names.snake, t.names.pascal, t.names.pascal
    ));
    g.finish(format!("Generated library {crate_name} in {member}/"))
}

/// `generate sub-app` (NestJS `app`): another Armature application of the
/// workspace, in `apps/<name>`.
pub async fn sub_app(name: &str, opts: GenOptions) -> CliResult<()> {
    let root = find_project_root()?;
    let t = Target::parse(name, false)?;
    let member = format!("apps/{}", t.names.kebab);
    let manifest_path = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)?;
    let updated = cargo_manifest::add_workspace_member(&manifest, &member)?;
    if root.join(&member).exists() {
        return Err(CliError::FileExists(member));
    }
    if opts.dry_run {
        println!(
            "  {} {member}/ (armature new {})",
            "CREATE".green().bold(),
            t.names.kebab
        );
        println!(
            "  {} Cargo.toml (workspace member)",
            "UPDATE".yellow().bold()
        );
        println!(
            "\n  {} Dry run: no files were written.",
            "NOTE".yellow().bold()
        );
        return Ok(());
    }
    let apps = root.join("apps");
    std::fs::create_dir_all(&apps)?;
    let previous = std::env::current_dir()?;
    std::env::set_current_dir(&apps)?;
    let result = Box::pin(crate::commands::new::run(
        &t.names.kebab,
        "minimal",
        true,
        true,
        &Default::default(),
    ))
    .await;
    std::env::set_current_dir(previous)?;
    result?;
    std::fs::write(&manifest_path, updated)?;
    println!(
        "  {} Cargo.toml (workspace member {member})",
        "UPDATE".yellow().bold()
    );
    Ok(())
}

/// `generate application` (NestJS `application`): a new project, like
/// `armature new <name>`.
pub async fn application(name: &str) -> CliResult<()> {
    Box::pin(crate::commands::new::run(
        name,
        "minimal",
        false,
        false,
        &Default::default(),
    ))
    .await
}

// =============================================================================
// Armature generators (no NestJS schematic)
// =============================================================================

async fn simple_command(
    name: &str,
    opts: GenOptions,
    kind: &'static str,
    type_suffix: &str,
    template: &str,
    test_template: Option<&str>,
    slot: Option<Slot>,
) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let e = g.simple(&t, kind, type_suffix, template, test_template, slot)?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}{type_suffix}", t.names.pascal))
}

/// `generate repository`.
pub async fn repository(name: &str, opts: GenOptions) -> CliResult<()> {
    // `<Name>Repository` is the trait; `<Name>RepositoryImpl` is the provider.
    command_with_deps(
        name,
        opts,
        "repository",
        "RepositoryImpl",
        "repository",
        Some("repository_test"),
        Some(Slot::Providers),
        &[],
        &[CHRONO],
    )
    .await
}

/// `generate job`, implementation chosen by `--job-type`.
pub async fn job(name: &str, job_type: &str, opts: GenOptions) -> CliResult<()> {
    let (template, test_template): (&str, Option<&str>) = match job_type {
        "scheduled" => ("job_scheduled", None),
        "recurring" => ("job_recurring", None),
        // "async" (default) is a queue processor and ships with a test.
        _ => ("job", Some("job_test")),
    };
    let deps: &[(&str, &str)] = if template == "job" {
        &[ARMATURE_QUEUE]
    } else {
        &[ARMATURE_CRON]
    };
    command_with_deps(
        name,
        opts,
        "job",
        "Job",
        template,
        test_template,
        None,
        &[],
        deps,
    )
    .await
}

/// `generate event`: an event and its handler.
pub async fn event_handler(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "event",
        "Event",
        "event_handler",
        Some("event_handler_test"),
        None,
        &[],
        &[ARMATURE_EVENTS, CHRONO, UUID],
    )
    .await
}

/// `generate scheduler`: a cron-scheduled task (same as `job --job-type scheduled`).
pub async fn scheduler(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "task",
        "Job",
        "job_scheduled",
        None,
        None,
        &[],
        &[ARMATURE_CRON],
    )
    .await
}

/// `generate cache-service`: a typed cache over an armature-cache store.
pub async fn cache_service(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "cache",
        "CacheService",
        "cache_service",
        Some("cache_service_test"),
        None,
        &[],
        &[ARMATURE_CACHE],
    )
    .await
}

/// `generate api-client`.
pub async fn api_client(name: &str, opts: GenOptions) -> CliResult<()> {
    command_with_deps(
        name,
        opts,
        "client",
        "Client",
        "api_client",
        None,
        None,
        &[],
        &[ARMATURE_HTTP_CLIENT],
    )
    .await
}

/// `generate health [name]`: health-check controller (default `health`).
pub async fn health_controller(name: Option<&str>, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name.unwrap_or("health"), &opts)?;
    let data = ComponentData {
        name_pascal: "Health".to_string(),
        name_snake: "health".to_string(),
        name_kebab: "health".to_string(),
    };
    let e = Element {
        kind: "controller",
        type_name: "HealthController".to_string(),
        content: g.render("health_controller", &data)?,
        test: None,
        slot: Some(Slot::Controllers),
        module: None,
    };
    g.element(&t, e)?;
    g.finish("Generated HealthController")
}

/// `generate dto`, optionally injecting `--fields`.
pub async fn dto(name: &str, fields: Option<&str>, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    g.require(&[], &[VALIDATOR])?;
    let e = dto_element(&g, &t, fields)?;
    g.element(&t, e)?;
    g.finish(format!("Generated {}Dto", t.names.pascal))
}

fn dto_element(g: &Gen, t: &Target, fields: Option<&str>) -> CliResult<Element> {
    let parsed = parse_fields(fields);

    // Response struct: extra fields between `id` and the timestamps.
    let response_fields = parsed
        .iter()
        .map(|f| format!("    pub {}: {},\n", f.snake, f.rust_type))
        .collect::<String>();

    // Create request: validated fields (defaults to a single `name` field).
    let create_fields = if parsed.is_empty() {
        "    #[validate(length(min = 1, max = 255, message = \"Name must be between 1 and 255 characters\"))]\n    pub name: String,\n".to_string()
    } else {
        parsed
            .iter()
            .map(|f| {
                if f.rust_type == "String" {
                    format!(
                        "    #[validate(length(min = 1, message = \"{} is required\"))]\n    pub {}: {},\n",
                        f.snake, f.snake, f.rust_type
                    )
                } else {
                    format!("    pub {}: {},\n", f.snake, f.rust_type)
                }
            })
            .collect::<String>()
    };

    // Update request: every field optional.
    let update_fields = if parsed.is_empty() {
        "    pub name: Option<String>,\n".to_string()
    } else {
        parsed
            .iter()
            .map(|f| format!("    pub {}: Option<{}>,\n", f.snake, f.rust_type))
            .collect::<String>()
    };

    let data = DtoData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        response_fields,
        create_fields,
        update_fields,
    };
    Ok(Element {
        kind: "dto",
        type_name: format!("{}Response", t.names.pascal),
        content: g.render("dto", &data)?,
        test: None,
        slot: None,
        module: None,
    })
}

/// `generate model`, optionally injecting `--fields` and, when `migration`
/// is set, a paired up/down SQL migration.
pub async fn model(
    name: &str,
    fields: Option<&str>,
    migration: bool,
    opts: GenOptions,
) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    let parsed = parse_fields(fields);
    check_reserved_field_names(&parsed)?;
    let model_fields = parsed
        .iter()
        .map(|f| format!("    pub {}: {},\n", f.snake, f.rust_type))
        .collect::<String>();
    let data = ModelData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        name_kebab: t.names.kebab.clone(),
        model_fields,
    };
    let e = Element {
        kind: "model",
        type_name: t.names.pascal.clone(),
        content: g.render("model", &data)?,
        test: None,
        slot: None,
        module: None,
    };
    g.element(&t, e)?;
    if migration {
        plan_migration(&mut g.plan, &t.names.snake, &parsed)?;
    }
    g.finish(format!("Generated {} model", t.names.pascal))
}

/// Plan a Diesel-style up/down SQL migration for a model.
///
/// Mirrors the directory layout `diesel_migrations::embed_migrations!` expects
/// (see `docs/diesel-guide.md`): a `migrations/<timestamp>_create_<table>/`
/// directory at the project root containing `up.sql` and `down.sql`.
fn plan_migration(plan: &mut Plan, snake: &str, fields: &[FieldDef]) -> CliResult<()> {
    let table = format!("{snake}s");
    let timestamp = chrono::Utc::now().format("%Y-%m-%d-%H%M%S");
    let migration_dir = plan
        .root()
        .join("migrations")
        .join(format!("{timestamp}_create_{table}"));

    let columns = fields
        .iter()
        .map(|f| format!("    {} {} NOT NULL,\n", f.snake, sql_type_for(f.rust_type)))
        .collect::<String>();
    let up_sql = format!(
        "-- Create {table} table\nCREATE TABLE {table} (\n    id BIGSERIAL PRIMARY KEY,\n    name VARCHAR NOT NULL,\n{columns}    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),\n    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()\n);\n",
    );
    let down_sql = format!("-- Drop {table} table\nDROP TABLE IF EXISTS {table};\n");
    plan.create(&migration_dir.join("up.sql"), up_sql)?;
    plan.create(&migration_dir.join("down.sql"), down_sql)?;
    Ok(())
}

/// ORM type for entity generation.
#[derive(Debug, Clone, Copy, Default)]
pub enum OrmType {
    #[default]
    Generic,
    Diesel,
    SeaOrm,
    Prax,
}

impl std::str::FromStr for OrmType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "generic" | "none" => Ok(OrmType::Generic),
            "diesel" => Ok(OrmType::Diesel),
            "seaorm" | "sea-orm" | "sea_orm" => Ok(OrmType::SeaOrm),
            "prax" | "prax-orm" | "prax_orm" => Ok(OrmType::Prax),
            _ => Err(format!(
                "Unknown ORM type: {}. Valid options: generic, diesel, seaorm, prax",
                s
            )),
        }
    }
}

/// `generate entity` with ORM support, optionally injecting `--fields`.
pub async fn entity_with_orm(
    name: &str,
    orm: OrmType,
    fields: Option<&str>,
    opts: GenOptions,
) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    if !matches!(orm, OrmType::Prax) {
        g.require(&[], &[CHRONO])?;
    }
    let e = entity_element(&g, &t, orm, fields)?;
    g.element(&t, e)?;
    let orm_name = match orm {
        OrmType::Generic => "generic",
        OrmType::Diesel => "Diesel",
        OrmType::SeaOrm => "SeaORM",
        OrmType::Prax => "Prax",
    };
    g.finish(format!("Generated {} entity ({orm_name})", t.names.pascal))
}

fn entity_element(g: &Gen, t: &Target, orm: OrmType, fields: Option<&str>) -> CliResult<Element> {
    // The Prax ORM sources fields from `schema.prax`, so it uses the plain template.
    let content = if matches!(orm, OrmType::Prax) {
        g.render("entity_prax", &g.component(t))?
    } else {
        let parsed = parse_fields(fields);
        let entity_fields = parsed
            .iter()
            .map(|f| format!("    pub {}: {},\n", f.snake, f.rust_type))
            .collect::<String>();
        let update_fields = parsed
            .iter()
            .map(|f| format!("    pub {}: Option<{}>,\n", f.snake, f.rust_type))
            .collect::<String>();
        let diesel_cols = parsed
            .iter()
            .map(|f| format!("        {} -> {},\n", f.snake, diesel_type_for(f.rust_type)))
            .collect::<String>();
        let seaorm_fields = parsed
            .iter()
            .map(|f| format!("        pub {}: {},\n", f.snake, f.rust_type))
            .collect::<String>();
        let data = EntityData {
            name_pascal: t.names.pascal.clone(),
            name_snake: t.names.snake.clone(),
            name_kebab: t.names.kebab.clone(),
            entity_fields: entity_fields.clone(),
            new_fields: entity_fields,
            update_fields,
            diesel_cols,
            seaorm_fields,
        };
        g.render("entity", &data)?
    };
    Ok(Element {
        kind: "entity",
        type_name: t.names.pascal.clone(),
        content,
        test: None,
        slot: None,
        module: None,
    })
}

/// `generate scaffold`: entity, repository, DTOs, service and CRUD
/// controller of one resource, in one directory.
pub async fn scaffold(name: &str, fields: Option<&str>, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    g.require(&[], &[CHRONO, VALIDATOR])?;
    let elements = vec![
        entity_element(&g, &t, OrmType::Generic, fields)?,
        g.simple(
            &t,
            "repository",
            "RepositoryImpl",
            "repository",
            Some("repository_test"),
            Some(Slot::Providers),
        )?,
        dto_element(&g, &t, fields)?,
        g.simple(
            &t,
            "service",
            "Service",
            "service",
            Some("service_test"),
            Some(Slot::Providers),
        )?,
        controller_element(&g, &t, true, false)?,
    ];
    for e in elements {
        g.element(&t, e)?;
    }
    g.finish(format!("Generated the {} scaffold", t.names.pascal))
}

/// `generate prax-schema`: `schema.prax` at the project root.
pub async fn prax_schema(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = Target::parse(name, false)?;
    plan_prax_schema(&mut g, &t)?;
    g.plan.note("run `prax generate` to generate the Rust code");
    g.finish(format!("Generated Prax schema for {}", t.names.pascal))
}

fn plan_prax_schema(g: &mut Gen, t: &Target) -> CliResult<()> {
    let content = g.render("prax_schema", &g.component(t))?;
    let file = g.plan.root().join("schema.prax");
    g.plan.create(&file, content)
}

/// `generate prax-repository`.
pub async fn prax_repository(name: &str, opts: GenOptions) -> CliResult<()> {
    simple_command(
        name,
        opts,
        "repository",
        "Repository",
        "prax_repository",
        Some("prax_repository_test"),
        Some(Slot::Providers),
    )
    .await
}

/// `generate prax-module`: module + Prax entity, repository and service in
/// one directory, plus `schema.prax`.
pub async fn prax_module(name: &str, opts: GenOptions) -> CliResult<()> {
    let mut g = Gen::new(opts)?;
    let t = target(name, &opts)?;
    plan_prax_schema(&mut g, &t)?;
    let module = ModuleData {
        name_pascal: t.names.pascal.clone(),
        name_snake: t.names.snake.clone(),
        controller_list: String::new(),
        provider_list: String::new(),
    };
    let module_file = g.module(&t, g.render("module", &module)?)?;
    let mut elements = vec![
        entity_element(&g, &t, OrmType::Prax, None)?,
        g.simple(
            &t,
            "repository",
            "Repository",
            "prax_repository",
            Some("prax_repository_test"),
            Some(Slot::Providers),
        )?,
        g.simple(
            &t,
            "service",
            "Service",
            "service",
            Some("service_test"),
            Some(Slot::Providers),
        )?,
    ];
    for e in &mut elements {
        e.module = Some(module_file.clone());
    }
    for e in elements {
        g.element(&t, e)?;
    }
    g.plan
        .note("add `prax-armature` to Cargo.toml, then run `prax generate`");
    g.finish(format!("Generated the {} Prax module", t.names.pascal))
}

/// A single user-specified field parsed from a `--fields` spec.
struct FieldDef {
    snake: String,
    rust_type: &'static str,
}

/// Map a field type token (e.g. `string`, `i32`, `bool`) to a Rust type.
fn rust_type_for(spec: &str) -> &'static str {
    match spec.trim().to_lowercase().as_str() {
        "string" | "str" | "text" | "varchar" | "uuid" => "String",
        "int" | "integer" | "i32" => "i32",
        "i64" | "long" | "bigint" => "i64",
        "u32" => "u32",
        "u64" => "u64",
        "float" | "f64" | "double" | "decimal" => "f64",
        "f32" => "f32",
        "bool" | "boolean" => "bool",
        "date" | "datetime" | "timestamp" => "String",
        "json" => "serde_json::Value",
        _ => "String",
    }
}

/// Single source-of-truth table mapping a Rust type (as produced by
/// [`rust_type_for`]) to its Diesel type name and plain SQL column type.
///
/// Keeping both mappings in one table prevents the two from silently
/// diverging (e.g. missing arms like the `serde_json::Value` -> `JSONB` case).
const TYPE_MAPPINGS: &[(&str, &str, &str)] = &[
    // (rust_type, diesel_type, sql_type)
    ("String", "Text", "VARCHAR"),
    ("i32", "Integer", "INTEGER"),
    ("i64", "BigInt", "BIGINT"),
    ("u32", "BigInt", "BIGINT"),
    ("u64", "BigInt", "BIGINT"),
    ("f64", "Double", "DOUBLE PRECISION"),
    ("f32", "Double", "DOUBLE PRECISION"),
    ("bool", "Bool", "BOOLEAN"),
    ("serde_json::Value", "Text", "JSONB"),
];

/// Default Diesel type for a Rust type with no explicit mapping.
const DEFAULT_DIESEL_TYPE: &str = "Text";
/// Default SQL type for a Rust type with no explicit mapping.
const DEFAULT_SQL_TYPE: &str = "TEXT";

/// Map a Rust type to a Diesel SQL column type.
fn diesel_type_for(rust_type: &str) -> &'static str {
    TYPE_MAPPINGS
        .iter()
        .find(|(rt, _, _)| *rt == rust_type)
        .map(|(_, diesel, _)| *diesel)
        .unwrap_or(DEFAULT_DIESEL_TYPE)
}

/// Map a Rust type to a plain SQL column type (used for generated migrations).
fn sql_type_for(rust_type: &str) -> &'static str {
    TYPE_MAPPINGS
        .iter()
        .find(|(rt, _, _)| *rt == rust_type)
        .map(|(_, _, sql)| *sql)
        .unwrap_or(DEFAULT_SQL_TYPE)
}

/// Field names already declared by the base model struct / migration SQL
/// (see `MODEL_TEMPLATE` and `generate_migration` in `templates.rs`).
///
/// A user-supplied `--fields` entry that collides with one of these produces
/// a duplicate struct field / SQL column, so it must be rejected up front.
const RESERVED_FIELD_NAMES: &[&str] = &["id", "name", "created_at", "updated_at"];

/// Parse a `--fields` spec like `name:string,email:string` into field definitions.
///
/// Invalid or empty entries are skipped; a bare `name` (no type) defaults to `String`.
fn parse_fields(spec: Option<&str>) -> Vec<FieldDef> {
    let Some(spec) = spec else {
        return Vec::new();
    };
    spec.split(',')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, ':');
            let name = it.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let ty = it.next().unwrap_or("string");
            Some(FieldDef {
                snake: name.to_snake_case(),
                rust_type: rust_type_for(ty),
            })
        })
        .collect()
}

/// Reject `--fields` entries that collide with the reserved column/field
/// names already hardcoded into the base model template and migration SQL
/// (`id`, `name`, `created_at`, `updated_at`). Without this check, a field
/// like `--fields name:string` produces a struct/table with a duplicate
/// field and fails to compile/run.
fn check_reserved_field_names(fields: &[FieldDef]) -> CliResult<()> {
    for field in fields {
        if RESERVED_FIELD_NAMES.contains(&field.snake.as_str()) {
            return Err(CliError::InvalidArgument(format!(
                "field name '{}' collides with a column already provided by the base model template; choose a different name",
                field.snake
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reserved_field_names_rejects_collisions_with_base_template() {
        // `MODEL_TEMPLATE` (templates.rs) hardcodes `id`, `name`, `created_at`,
        // and `updated_at`; user-supplied fields with these names must be
        // rejected instead of silently producing a struct/table with a
        // duplicate field.
        for reserved in RESERVED_FIELD_NAMES {
            let fields = parse_fields(Some(&format!("{reserved}:string")));
            let result = check_reserved_field_names(&fields);
            assert!(
                result.is_err(),
                "expected field name '{reserved}' to be rejected as reserved, got: {result:?}"
            );
            assert!(
                matches!(result, Err(CliError::InvalidArgument(_))),
                "expected a CliError::InvalidArgument for reserved field '{reserved}', got: {result:?}"
            );
        }
    }

    #[test]
    fn tests_are_retargeted_to_the_element() {
        let test =
            "//! Tests.\n\nuse super::*;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n";
        let element = "use armature::prelude::*;\nuse armature::{Guard,\n    GuardContext};\nuse super::other::Thing;\npub use x::Y;\n\nfn f() { use inner::Z; }\n";
        let out = retarget_test(test, "products_service", element);
        assert!(out.contains("use super::super::products_service::*;\n"));
        assert!(out.contains("use armature::prelude::*;\n"));
        assert!(
            out.contains("use armature::{Guard,\n    GuardContext};\n"),
            "{out}"
        );
        assert!(out.contains("use super::super::other::Thing;\n"));
        assert!(!out.contains("use x::Y;") && !out.contains("use inner::Z;"));
        assert!(
            out.contains("mod tests {\n    use super::*;\n}"),
            "inner import untouched"
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn modules_merge_into_existing_mod_files() {
        let module = "//! Products module.\n\nuse armature::prelude::*;\n\n#[module()]\npub struct ProductsModule;\n";
        assert_eq!(merge_module("", module), module);
        let merged = merge_module("pub mod products_controller;\n", module);
        assert!(
            merged.starts_with(
                "//! Products module.\n\npub mod products_controller;\n\nuse armature::prelude::*;"
            ),
            "{merged}"
        );
    }

    #[test]
    fn check_reserved_field_names_allows_non_colliding_names() {
        let fields = parse_fields(Some("email:string,age:i32,active:bool"));
        assert!(check_reserved_field_names(&fields).is_ok());
    }
}
