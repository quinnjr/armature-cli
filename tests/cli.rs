//! Integration tests for the `armature` CLI binary.
//!
//! These exercise project scaffolding, code generation, route listing, OpenAPI
//! client generation, and exit-code behavior end-to-end in throwaway tempdirs.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;

/// Build a `Command` for the `armature` binary rooted in `dir`, with color off.
fn armature_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("armature").unwrap();
    cmd.current_dir(dir);
    cmd.arg("--no-color");
    cmd
}

/// Create a minimal Armature-looking project (Cargo.toml + src) inside `dir`.
fn scaffold_project(dir: &Path) {
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\narmature = \"0.1\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src/controllers")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.join("src/controllers/mod.rs"), "pub mod health;\n").unwrap();
}

fn assert_exists(base: &Path, rel: &str) {
    assert!(
        base.join(rel).exists(),
        "expected generated file/dir to exist: {}",
        rel
    );
}

// =============================================================================
// Command tree validity (guards against duplicate clap aliases, which make
// clap's debug asserts panic on EVERY invocation)
// =============================================================================

#[test]
fn command_tree_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path()).arg("--version").assert().success();
    // Exercise nested subcommand help to force the whole tree to build.
    armature_in(tmp.path())
        .args(["openapi", "--help"])
        .assert()
        .success();
    armature_in(tmp.path())
        .args(["generate", "--help"])
        .assert()
        .success();
}

// =============================================================================
// Project templates
// =============================================================================

#[test]
fn new_minimal_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "minimal", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    for f in [
        "Cargo.toml",
        "src/main.rs",
        "src/controllers/mod.rs",
        "src/controllers/health.rs",
        ".env.example",
        "README.md",
        ".gitignore",
    ] {
        assert_exists(&root, f);
    }
}

#[test]
fn new_full_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "full", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    for f in [
        "src/services/mod.rs",
        "src/middleware/mod.rs",
        "src/guards/mod.rs",
        "src/models/mod.rs",
        "Dockerfile",
        "docker-compose.yml",
    ] {
        assert_exists(&root, f);
    }
}

#[test]
fn new_microservice_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "microservice", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    assert_exists(&root, "src/handlers/mod.rs");
    assert_exists(&root, "src/jobs/mod.rs");
    assert_exists(&root, "Dockerfile");
}

#[test]
fn new_graphql_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "graphql", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    assert_exists(&root, "src/graphql/mod.rs");
    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("async-graphql"),
        "graphql template Cargo.toml must depend on async-graphql, got:\n{cargo}"
    );
}

#[test]
fn new_grpc_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "grpc", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    assert_exists(&root, "src/grpc/mod.rs");
    assert_exists(&root, "proto/service.proto");
    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("tonic"),
        "grpc template Cargo.toml must depend on tonic, got:\n{cargo}"
    );
}

#[test]
fn new_lambda_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "lambda", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("lambda_http") || cargo.contains("lambda_runtime"),
        "lambda template Cargo.toml must depend on a lambda crate, got:\n{cargo}"
    );
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        main.contains("lambda"),
        "lambda main.rs should reference lambda runtime"
    );
}

#[test]
fn new_cloudrun_produces_tree() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "cloudrun", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    assert_exists(&root, "Dockerfile");
    // Cloud Run deployment descriptor.
    assert!(
        root.join("service.yaml").exists() || root.join("cloudbuild.yaml").exists(),
        "cloudrun template must emit a Cloud Run deploy descriptor"
    );

    // The embedded Dockerfile's `ARG BIN_NAME=app` placeholder must actually
    // be substituted with the project's kebab-case binary name end-to-end —
    // not just that the file exists (see commands/new.rs's `ARG BIN_NAME`
    // patch in `create_project_structure`).
    let dockerfile_contents = fs::read_to_string(root.join("Dockerfile")).unwrap();
    assert!(
        dockerfile_contents.contains("ARG BIN_NAME=acme"),
        "Dockerfile must have 'ARG BIN_NAME=app' substituted with the \
         project's kebab-case name, got:\n{dockerfile_contents}"
    );
    assert!(
        !dockerfile_contents.contains("ARG BIN_NAME=app"),
        "Dockerfile must not still contain the unsubstituted 'ARG BIN_NAME=app' \
         placeholder, got:\n{dockerfile_contents}"
    );
}

// Note: there is no `new_azure_container_produces_tree` test alongside the
// two above. `templates/azure-container/Dockerfile` is a standalone
// reference example, not an `armature new --template ...` target — there's
// no "azure-container" entry in `KNOWN_TEMPLATES` (see `commands/new.rs`),
// so there's no CLI-generated Dockerfile for a test like this to exercise.
// See the matching note in `armature-cli/src/templates.rs` next to
// `CLOUDRUN_DOCKERFILE_TEMPLATE`.

#[test]
fn clap_rejects_unknown_template_before_new_runs() {
    // `--template` is a clap `ValueEnum` (`ProjectTemplate`), so an unknown
    // value like "bogus" is rejected by clap's own arg parsing before
    // `armature new` ever dispatches into `new::run`/`validate_template`.
    // This test only proves that the CLI-level parse failure has no
    // filesystem side effects; it does NOT exercise `validate_template`'s
    // before-write guarantee (see
    // `commands::new::tests::run_rejects_unknown_template_before_creating_directory`
    // in `src/commands/new.rs` for that).
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "bogus", "--skip-git"])
        .assert()
        .failure();

    assert!(
        !tmp.path().join("acme").exists(),
        "no project directory must be created for an invalid template"
    );
}

#[test]
fn new_database_docker_ci_emits_config() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args([
            "new",
            "acme",
            "--template",
            "minimal",
            "--database",
            "postgres",
            "--docker",
            "--ci",
            "--skip-git",
        ])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("sqlx") || cargo.contains("postgres"),
        "postgres database should add a db dependency, got:\n{cargo}"
    );
    assert_exists(&root, "Dockerfile");
    assert_exists(&root, ".github/workflows/ci.yml");
}

// =============================================================================
// generate --fields
// =============================================================================

#[test]
fn generate_dto_injects_fields() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_project(tmp.path());

    armature_in(tmp.path())
        .args(["g", "dto", "user", "--fields", "name:string,email:string"])
        .assert()
        .success();

    let dto = fs::read_to_string(tmp.path().join("src/user/user_dto.rs")).unwrap();
    assert!(
        dto.contains("pub name: String"),
        "DTO must contain the requested name field, got:\n{dto}"
    );
    assert!(
        dto.contains("pub email: String"),
        "DTO must contain the requested email field, got:\n{dto}"
    );
}

#[test]
fn generate_model_writes_struct_with_fields() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_project(tmp.path());

    armature_in(tmp.path())
        .args([
            "generate",
            "model",
            "user",
            "--fields",
            "email:string,age:i32,active:bool",
        ])
        .assert()
        .success();

    let model_file = tmp.path().join("src/user/user_model.rs");
    assert!(
        model_file.exists(),
        "expected src/user/user_model.rs to be written"
    );

    let model = fs::read_to_string(&model_file).unwrap();
    assert!(
        model.contains("pub struct User"),
        "model must define a User struct, got:\n{model}"
    );
    assert!(
        model.contains("pub email: String"),
        "model must contain the requested email field, got:\n{model}"
    );
    assert!(
        model.contains("pub age: i32"),
        "model must contain the requested age field, got:\n{model}"
    );
    assert!(
        model.contains("pub active: bool"),
        "model must contain the requested active field, got:\n{model}"
    );

    // The module tree must declare the new file up to the crate root.
    let mod_file = fs::read_to_string(tmp.path().join("src/user/mod.rs")).unwrap();
    assert!(
        mod_file.contains("pub mod user_model;"),
        "user/mod.rs must declare the new module, got:\n{mod_file}"
    );
    let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
    assert!(
        main.contains("mod user;"),
        "main.rs must declare `user`, got:\n{main}"
    );

    // No migration was requested, so no migrations directory should exist.
    assert!(
        !tmp.path().join("migrations").exists(),
        "migrations directory should not be created without --migration"
    );
}

#[test]
fn generate_model_with_migration_writes_up_and_down_sql() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_project(tmp.path());

    armature_in(tmp.path())
        .args([
            "generate",
            "model",
            "post",
            "--fields",
            "title:string,published:bool",
            "--migration",
        ])
        .assert()
        .success();

    assert_exists(tmp.path(), "src/post/post_model.rs");

    let migrations_dir = tmp.path().join("migrations");
    assert!(
        migrations_dir.exists(),
        "expected a migrations directory to be created"
    );

    let entries: Vec<_> = fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one migration directory, got: {:?}",
        entries
    );

    let migration_dir = &entries[0];
    let dir_name = migration_dir.file_name().unwrap().to_string_lossy();
    assert!(
        dir_name.ends_with("_create_posts"),
        "migration directory should be named '<timestamp>_create_posts', got: {dir_name}"
    );

    let up_sql = fs::read_to_string(migration_dir.join("up.sql")).unwrap();
    assert!(
        up_sql.contains("CREATE TABLE posts"),
        "up.sql must create the posts table, got:\n{up_sql}"
    );
    assert!(
        up_sql.contains("title VARCHAR NOT NULL"),
        "up.sql must contain the title column, got:\n{up_sql}"
    );
    assert!(
        up_sql.contains("published BOOLEAN NOT NULL"),
        "up.sql must contain the published column, got:\n{up_sql}"
    );

    let down_sql = fs::read_to_string(migration_dir.join("down.sql")).unwrap();
    assert!(
        down_sql.contains("DROP TABLE IF EXISTS posts"),
        "down.sql must drop the posts table, got:\n{down_sql}"
    );
}

// =============================================================================
// routes
// =============================================================================

fn scaffold_routes_project(dir: &Path) {
    scaffold_project(dir);
    fs::write(
        dir.join("src/controllers/users.rs"),
        r#"
#[middleware(LoggerMiddleware)]
#[guard(AuthGuard)]
#[get("/api/users")]
pub async fn list() {}

#[post("/api/users")]
pub async fn create() {}
"#,
    )
    .unwrap();
}

#[test]
fn routes_formats_differ() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_routes_project(tmp.path());

    let json = armature_in(tmp.path())
        .args(["routes", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = String::from_utf8(json).unwrap();
    assert!(json.contains('['), "json format should emit a JSON array");

    let markdown = armature_in(tmp.path())
        .args(["routes", "--format", "markdown"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let markdown = String::from_utf8(markdown).unwrap();
    assert!(
        markdown.contains('|'),
        "markdown format should emit a table with pipes"
    );

    let yaml = armature_in(tmp.path())
        .args(["routes", "--format", "yaml"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let yaml = String::from_utf8(yaml).unwrap();
    assert!(
        yaml.contains("method:") || yaml.contains("- method"),
        "yaml format should emit yaml keys"
    );

    assert_ne!(json, markdown, "json and markdown output must differ");
    assert_ne!(json, yaml, "json and yaml output must differ");
}

#[test]
fn routes_reports_middleware_stats() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_routes_project(tmp.path());

    armature_in(tmp.path())
        .args(["routes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Routes with middleware: 1"));
}

#[test]
fn routes_path_filter() {
    let tmp = tempfile::tempdir().unwrap();
    scaffold_project(tmp.path());
    fs::write(
        tmp.path().join("src/controllers/mixed.rs"),
        "#[get(\"/api/users\")]\npub async fn a() {}\n#[get(\"/health\")]\npub async fn b() {}\n",
    )
    .unwrap();

    armature_in(tmp.path())
        .args(["routes", "--path", "users"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/api/users"))
        .stdout(predicate::str::contains("/health").not());
}

// =============================================================================
// validate exit code
// =============================================================================

#[test]
fn validate_fails_on_missing_src() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    // --config-only avoids invoking cargo; the missing src still fails validation.
    armature_in(tmp.path())
        .args(["validate", "--config-only"])
        .assert()
        .failure()
        // stdout carries the specific check that failed, proving this is a real
        // validation finding and not e.g. a clap arg-parse failure.
        .stdout(predicate::str::contains("Missing src directory"))
        // stderr carries the top-level error surfaced by `main`, which
        // distinguishes an exit-from-validation (`CliError::Validation`) from
        // an exit caused by argument parsing (clap prints its own
        // "error: unexpected argument"/usage text instead of this message).
        .stderr(predicate::str::contains("Validation error"));
}

// =============================================================================
// openapi client generation
// =============================================================================

fn write_petstore_spec(dir: &Path) -> std::path::PathBuf {
    let spec = r#"openapi: 3.0.0
info:
  title: Pet Store
  version: 1.0.0
paths:
  /pets:
    get:
      operationId: listPets
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: '#/components/schemas/Pet'
components:
  schemas:
    Pet:
      type: object
      properties:
        id:
          type: integer
        name:
          type: string
"#;
    let path = dir.join("openapi.yaml");
    fs::write(&path, spec).unwrap();
    path
}

#[test]
fn openapi_rust_client_honors_logging_and_retry() {
    let tmp = tempfile::tempdir().unwrap();
    write_petstore_spec(tmp.path());

    armature_in(tmp.path())
        .args([
            "openapi",
            "client",
            "openapi.yaml",
            "--language",
            "rust",
            "--output",
            "out",
            "--with-logging",
            "--with-retry",
        ])
        .assert()
        .success();

    let client = fs::read_to_string(tmp.path().join("out/client.rs")).unwrap();
    assert!(
        client.contains("retry") || client.contains("Retry"),
        "with-retry must emit retry code, got:\n{client}"
    );
    assert!(
        client.to_lowercase().contains("log")
            || client.contains("eprintln")
            || client.contains("tracing"),
        "with-logging must emit logging code"
    );
}

#[test]
fn openapi_ts_client_honors_logging_and_retry_and_base_url() {
    let tmp = tempfile::tempdir().unwrap();
    write_petstore_spec(tmp.path());

    armature_in(tmp.path())
        .args([
            "openapi",
            "client",
            "openapi.yaml",
            "--language",
            "typescript",
            "--output",
            "out",
            "--with-logging",
            "--with-retry",
            "--base-url",
            "https://api.example.com",
        ])
        .assert()
        .success();

    let client = fs::read_to_string(tmp.path().join("out/client.ts")).unwrap();
    assert!(
        client.contains("retry") || client.contains("Retry"),
        "with-retry must emit retry code in TS"
    );
    assert!(
        client.contains("console."),
        "with-logging must emit console logging in TS"
    );
    assert!(
        client.contains("https://api.example.com"),
        "base-url must be baked into the generated TS client"
    );
}

// =============================================================================
// Removed `db` subcommand
// =============================================================================

#[test]
fn db_subcommand_removed() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["db", "migrate"])
        .assert()
        .failure();
}

// =============================================================================
// Ignored smoke test: cargo-check a generated project
// =============================================================================

#[test]
#[ignore = "compiles a generated project; slow and needs network for crates.io"]
fn generated_minimal_project_cargo_checks() {
    let tmp = tempfile::tempdir().unwrap();
    armature_in(tmp.path())
        .args(["new", "acme", "--template", "minimal", "--skip-git"])
        .assert()
        .success();

    let root = tmp.path().join("acme");
    let status = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "generated minimal project should cargo-check"
    );
}

/// Run `armature new <name> --template minimal` in `dir`, return the project root.
fn new_minimal_project(dir: &Path, name: &str) -> std::path::PathBuf {
    armature_in(dir)
        .args(["new", name, "--template", "minimal", "--skip-git"])
        .assert()
        .success();
    dir.join(name)
}

#[test]
fn generate_resource_takes_its_path_from_the_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");

    armature_in(&root)
        .args(["generate", "resource", "modules/products", "--crud"])
        .assert()
        .success();

    for f in [
        "src/modules/mod.rs",
        "src/modules/products/mod.rs",
        "src/modules/products/products_controller.rs",
        "src/modules/products/products_service.rs",
        "src/modules/products/tests/products_controller_test.rs",
        "src/modules/products/tests/products_service_test.rs",
    ] {
        assert_exists(&root, f);
    }
    for f in ["src/mod.rs", "src/modules/products/tests/mod.rs"] {
        assert!(!root.join(f).exists(), "unexpected {f}");
    }

    let module = fs::read_to_string(root.join("src/modules/products/mod.rs")).unwrap();
    for expected in [
        "pub mod products_controller;",
        "pub mod products_service;",
        "controllers: [products_controller::ProductsController]",
        "providers: [products_service::ProductsService]",
        "pub struct ProductsModule;",
        "mod tests {\n    mod products_controller_test;\n    mod products_service_test;\n}",
    ] {
        assert!(
            module.contains(expected),
            "missing {expected:?} in:\n{module}"
        );
    }

    let controller =
        fs::read_to_string(root.join("src/modules/products/products_controller.rs")).unwrap();
    assert!(controller.contains("#[controller(\"/products\")]"));
    assert!(controller.contains("use super::products_service::"));
    for route in ["#[post(\"/\")]", "#[put(\"/:id\")]", "#[delete(\"/:id\")]"] {
        assert!(controller.contains(route), "missing {route}");
    }

    assert_eq!(
        fs::read_to_string(root.join("src/modules/mod.rs")).unwrap(),
        "pub mod products;\n"
    );
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(main.contains("mod modules;"), "{main}");
    assert!(
        main.contains("imports: [modules::products::ProductsModule]"),
        "{main}"
    );

    // Generating it again fails without touching anything.
    let before = fs::read_to_string(root.join("src/main.rs")).unwrap();
    armature_in(&root)
        .args(["generate", "resource", "modules/products"])
        .assert()
        .failure();
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        before
    );
}

#[test]
fn elements_are_registered_in_the_nearest_module() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");

    armature_in(&root)
        .args(["g", "mo", "modules/catalog"])
        .assert()
        .success();
    // Inside the module's directory: registered in CatalogModule.
    armature_in(&root)
        .args(["g", "co", "modules/catalog/items", "--crud"])
        .assert()
        .success();
    armature_in(&root)
        .args(["g", "s", "modules/catalog/items"])
        .assert()
        .success();
    // Elsewhere: registered in AppModule.
    armature_in(&root)
        .args(["g", "pr", "shared/clock"])
        .assert()
        .success();

    let catalog = fs::read_to_string(root.join("src/modules/catalog/mod.rs")).unwrap();
    assert!(catalog.contains("pub mod items;"));
    assert!(
        catalog.contains("controllers: [items::items_controller::ItemsController]"),
        "{catalog}"
    );
    assert!(
        catalog.contains("providers: [items::items_service::ItemsService]"),
        "{catalog}"
    );
    assert_exists(&root, "src/modules/catalog/items/items_controller.rs");
    assert_exists(
        &root,
        "src/modules/catalog/items/tests/items_service_test.rs",
    );

    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        main.contains("imports: [modules::catalog::CatalogModule]"),
        "{main}"
    );
    assert!(
        main.contains("providers: [shared::clock::clock_provider::ClockProvider]"),
        "{main}"
    );
}

#[test]
fn flat_skip_import_no_spec_and_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");
    let main_before = fs::read_to_string(root.join("src/main.rs")).unwrap();

    // --dry-run writes nothing.
    armature_in(&root)
        .args(["g", "res", "modules/orders", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "CREATE src/modules/orders/orders_service.rs",
        ))
        .stdout(predicate::str::contains("Dry run"));
    assert!(!root.join("src/modules").exists());
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        main_before
    );

    // --flat: no directory for the element; --no-spec: no test.
    armature_in(&root)
        .args(["g", "s", "shared/mailer", "--flat", "--no-spec"])
        .assert()
        .success();
    assert_exists(&root, "src/shared/mailer_service.rs");
    assert!(!root.join("src/shared/mailer").exists());
    assert!(!root.join("src/shared/tests").exists());

    // --skip-import: declared for Rust, not registered in a module.
    armature_in(&root)
        .args(["g", "co", "admin", "--skip-import"])
        .assert()
        .success();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(main.contains("mod admin;"));
    assert!(!main.contains("AdminController"), "{main}");
}

#[test]
fn generate_module_lists_existing_elements_by_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");

    armature_in(&root)
        .args([
            "generate",
            "module",
            "admin",
            "-c",
            "admin/users",
            "-p",
            "admin/users",
        ])
        .assert()
        .success();

    let module = fs::read_to_string(root.join("src/admin/mod.rs")).unwrap();
    assert!(
        module.contains("controllers: [users::users_controller::UsersController]"),
        "{module}"
    );
    assert!(
        module.contains("providers: [users::users_service::UsersService]"),
        "{module}"
    );
}

#[test]
fn nestjs_library_updates_the_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");

    armature_in(&root)
        .args(["g", "lib", "shared-utils"])
        .assert()
        .success();
    assert_exists(&root, "libs/shared-utils/Cargo.toml");
    assert_exists(&root, "libs/shared-utils/src/lib.rs");
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        manifest.contains("shared-utils = { path = \"libs/shared-utils\" }"),
        "{manifest}"
    );
    assert!(
        manifest.contains("members = [\"libs/shared-utils\"]"),
        "{manifest}"
    );
    let lib_manifest = fs::read_to_string(root.join("libs/shared-utils/Cargo.toml")).unwrap();
    assert!(lib_manifest.contains("armature = {"), "{lib_manifest}");
    assert!(lib_manifest.contains("armature-core ="), "{lib_manifest}");
}

/// End-to-end: every NestJS-style element generated into a fresh project
/// builds and its tests pass. Needs crates.io, hence ignored like the check
/// above.
#[test]
#[ignore = "compiles a generated project; slow and needs network for crates.io"]
fn generated_resource_builds_and_its_tests_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let root = new_minimal_project(tmp.path(), "shop");
    armature_in(&root)
        .args(["generate", "resource", "modules/products", "--crud"])
        .assert()
        .success();
    for args in [
        &["generate", "resource", "categories"][..],
        &["generate", "controller", "modules/products/admin"],
        &["generate", "provider", "shared/clock"],
        &["generate", "class", "shared/money"],
        &["generate", "interface", "shared/payment"],
        &["generate", "decorator", "common/tenant"],
        &["generate", "guard", "common/access"],
        &["generate", "middleware", "common/logging"],
        &["generate", "interceptor", "common/timing"],
        &["generate", "filter", "common/http"],
        &["generate", "pipe", "common/validation"],
        &["generate", "config", "app"],
    ] {
        armature_in(&root).args(args).assert().success();
    }

    let output = std::process::Command::new("cargo")
        .args(["test", "--color", "never"])
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "generated project must build and pass its tests:\n{}\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "modules::products::tests::products_controller_test::create_returns_201 ... ok"
        )
    );
    assert!(
        stdout.contains("categories::tests::categories_service_test::find_all_starts_empty ... ok")
    );
}

// =============================================================================
// Unimplemented subcommands must fail, not silently succeed
// =============================================================================

/// Subcommands that are declared but do nothing must exit non-zero.
///
/// They used to print "coming soon!" and return `Ok(())`, so `armature deploy
/// --env prod` in CI reported a successful deploy having deployed nothing, and
/// `armature serve` as a container start-command exited 0 with no server
/// running. They are also `hide = true` now, so they no longer appear in
/// `--help` — but remain invocable (and failing) for anyone who scripted them.
#[test]
fn unimplemented_subcommands_exit_non_zero() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec!["serve"],
        vec!["deploy", "--env", "prod"],
        vec!["upgrade"],
        vec!["upgrade", "--check"],
        vec!["bench"],
        vec!["lint"],
        vec!["config", "show"],
        vec!["config", "set", "k", "v"],
        vec!["config", "init"],
        vec!["plugin", "install", "foo"],
        vec!["plugin", "uninstall", "foo"],
        vec!["plugin", "new", "foo"],
        vec!["openapi", "validate", "openapi.yaml"],
        vec!["openapi", "generate"],
    ] {
        armature_in(tmp.path())
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("not implemented yet"));
    }
}

/// Hidden-but-real commands stay out of the top-level help listing.
#[test]
fn unimplemented_subcommands_are_hidden_from_help() {
    let tmp = tempfile::tempdir().unwrap();
    let out = armature_in(tmp.path())
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(out).unwrap();
    for hidden in ["deploy", "bench"] {
        assert!(
            !help.contains(&format!("  {}", hidden)),
            "`{hidden}` is not implemented and must not be advertised in --help:\n{help}"
        );
    }
}
