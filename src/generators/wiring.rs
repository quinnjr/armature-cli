//! Text edits that wire generated code into an existing project.
//!
//! After writing an element, the generators have to make Rust and Armature
//! aware of it, the way the NestJS CLI updates the nearest module:
//!
//! 1. every parent module declares its child (`pub mod products;`, and
//!    `mod modules;` in the crate root),
//! 2. tests are declared in an inline `#[cfg(test)] mod tests { mod …; }`,
//! 3. controllers/providers are added to the nearest `#[module(…)]`, and a new
//!    module to the `imports` of its parent module.
//!
//! These are pure functions (string in, string out) so they are unit-tested;
//! they never touch anything they do not recognize.

/// Return `content` with a `<vis>mod <name>;` declaration added (`vis` is
/// `""` or `"pub "`), or `None` if the module is already declared (`mod x;`,
/// `pub mod x;`, `pub(crate) mod x;`, or an inline `mod x { … }`).
///
/// The declaration goes after the last top-level `mod …;` line, or else
/// after the leading inner doc comments / attributes.
pub fn ensure_mod_declaration(content: &str, name: &str, vis: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let declares = |line: &str| {
        let rest = line
            .trim_start()
            .strip_prefix("pub(crate) ")
            .or_else(|| line.trim_start().strip_prefix("pub "))
            .unwrap_or_else(|| line.trim_start());
        rest.strip_prefix("mod ")
            .map(|r| r.trim_start())
            .and_then(|r| r.strip_prefix(name))
            .is_some_and(|r| {
                let r = r.trim_start();
                r.starts_with(';') || r.starts_with('{')
            })
    };
    if lines.iter().any(|l| declares(l)) {
        return None;
    }

    let is_top_level_mod = |line: &str| {
        let t = line.trim_end();
        !line.starts_with(char::is_whitespace)
            && t.ends_with(';')
            && (t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("pub(crate) mod "))
    };
    let insert_at = match lines.iter().rposition(|l| is_top_level_mod(l)) {
        Some(i) => i + 1,
        None => {
            // After the header: inner docs (`//!`), inner attributes (`#![…]`)
            // and the blank lines around them.
            let mut i = 0;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.starts_with("//!") || t.starts_with("#![") || t.is_empty() {
                    i += 1;
                } else {
                    break;
                }
            }
            i
        }
    };

    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let needs_blank_after = insert_at < out.len()
        && !out[insert_at].trim().is_empty()
        && !is_top_level_mod(&out[insert_at]);
    if needs_blank_after {
        out.insert(insert_at, String::new());
    }
    out.insert(insert_at, format!("{vis}mod {name};"));
    if insert_at > 0
        && !is_top_level_mod(&out[insert_at - 1])
        && !out[insert_at - 1].trim().is_empty()
    {
        out.insert(insert_at, String::new());
    }
    let mut text = out.join("\n");
    if content.ends_with('\n') || content.is_empty() {
        text.push('\n');
    }
    Some(text)
}

/// Result of [`add_to_module_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListEdit {
    /// The item was added; here is the new content.
    Added(String),
    /// The item is already listed.
    AlreadyPresent,
    /// No `#[module(…)] struct …` in this content: nothing changed.
    NoModule,
}

/// Name of the first `#[module(…)]` struct in `content`.
pub fn module_struct(content: &str) -> Option<String> {
    let (_, _, struct_pos) = find_module_attr(content)?;
    let rest = &content[struct_pos + "struct ".len()..];
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Add `path` (e.g. `modules::products::ProductsModule`) to the `key` list
/// (`imports`, `controllers`, `providers`) of the first `#[module(…)]`
/// struct in `content`.
pub fn add_to_module_list(content: &str, key: &str, path: &str) -> ListEdit {
    let Some((args_start, args_end, _)) = find_module_attr(content) else {
        return ListEdit::NoModule;
    };

    let args = &content[args_start..args_end];
    let type_name = path.rsplit("::").next().unwrap_or(path);
    if let Some((list_start, list_end)) = find_key_list(args, key) {
        let list = &args[list_start..list_end];
        // Same path, or the bare type name already imported with `use`.
        let already = list
            .split(',')
            .map(str::trim)
            .any(|item| item == path || item == type_name);
        if already {
            return ListEdit::AlreadyPresent;
        }
        let trimmed = list.trim_end();
        let insertion = if trimmed.trim().is_empty() {
            path.to_string()
        } else if trimmed.ends_with(',') {
            format!(" {path}")
        } else {
            format!(", {path}")
        };
        let at = args_start + list_start + trimmed.len();
        let mut out = String::with_capacity(content.len() + insertion.len());
        out.push_str(&content[..at]);
        out.push_str(&insertion);
        out.push_str(&content[at..]);
        ListEdit::Added(out)
    } else {
        let separator = if args.trim().is_empty() { "" } else { ", " };
        let mut out = String::with_capacity(content.len() + path.len() + 16);
        out.push_str(&content[..args_start]);
        out.push_str(&format!("{key}: [{path}]{separator}"));
        out.push_str(&content[args_start..]);
        ListEdit::Added(out)
    }
}

/// `(args_start, args_end, struct_pos)` of the first `#[module(…)]`
/// attribute that is directly followed (attributes, comments and whitespace
/// aside) by a `struct`.
fn find_module_attr(content: &str) -> Option<(usize, usize, usize)> {
    let mut from = 0;
    while let Some(i) = content[from..].find("#[module(") {
        let args_start = from + i + "#[module(".len();
        let args_end = matching_paren(content, args_start)?;
        // Skip `)]`, then attributes, comments and whitespace up to the item.
        let mut pos = args_end + 1;
        if content[pos..].starts_with(']') {
            pos += 1;
        }
        loop {
            let rest = &content[pos..];
            let trimmed = rest.trim_start();
            pos += rest.len() - trimmed.len();
            if trimmed.starts_with("#[") || trimmed.starts_with("//") {
                pos += trimmed.find('\n').map_or(trimmed.len(), |n| n + 1);
                continue;
            }
            break;
        }
        let item = &content[pos..];
        let visibility = ["pub(crate) ", "pub ", ""]
            .iter()
            .find(|v| item.starts_with(&format!("{v}struct ")));
        if let Some(v) = visibility {
            return Some((args_start, args_end, pos + v.len()));
        }
        from = args_end;
    }
    None
}

/// Result of [`ensure_test_declaration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestEdit {
    Updated(String),
    AlreadyDeclared,
    /// The file uses an out-of-line `mod tests;`: declare it there instead.
    ExternalTestsModule,
}

/// Declare `mod <test_mod>;` inside the inline `#[cfg(test)] mod tests { … }`
/// of `content`, creating the block if needed. The test file then lives in
/// `tests/<test_mod>.rs` next to the module.
pub fn ensure_test_declaration(content: &str, test_mod: &str) -> TestEdit {
    let declared = content.lines().any(|l| {
        let t = l.trim();
        t == format!("mod {test_mod};") || t == format!("pub mod {test_mod};")
    });
    if declared {
        return TestEdit::AlreadyDeclared;
    }
    if content
        .lines()
        .any(|l| matches!(l.trim(), "mod tests;" | "pub mod tests;"))
    {
        return TestEdit::ExternalTestsModule;
    }
    if let Some(i) = content.find("mod tests {") {
        let body_start = i + "mod tests {".len();
        if let Some(close) = matching_brace(content, body_start) {
            let before = content[..close].trim_end_matches([' ', '\t']);
            let mut out = String::with_capacity(content.len() + test_mod.len() + 16);
            out.push_str(before);
            if !before.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("    mod {test_mod};\n"));
            out.push_str(&content[close..]);
            return TestEdit::Updated(out);
        }
    }
    let mut out = content.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "#[cfg(test)]\nmod tests {{\n    mod {test_mod};\n}}\n"
    ));
    TestEdit::Updated(out)
}

fn matching_brace(content: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in content[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Offset of the `)` closing the parenthesis opened just before `start`.
fn matching_paren(content: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in content[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Inside `#[module(…)]` arguments, the byte range of the list of `key: […]`
/// (between the brackets).
fn find_key_list(args: &str, key: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = args[from..].find(key) {
        let pos = from + i;
        let before_ok = args[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = &args[pos + key.len()..];
        let after_colon = rest.trim_start().strip_prefix(':');
        if before_ok && let Some(after) = after_colon {
            let open = after.trim_start();
            if open.starts_with('[') {
                let list_start = args.len() - open.len() + 1;
                let close = args[list_start..].find(']')?;
                return Some((list_start, list_start + close));
            }
        }
        from = pos + key.len();
    }
    None
}

/// Best-effort English singular of a snake_case name, for the item type of a
/// resource (`products` → `product`, `categories` → `category`). Words that
/// do not look like regular plurals are returned unchanged.
pub fn singularize(word: &str) -> String {
    let (prefix, last) = match word.rsplit_once('_') {
        Some((p, l)) => (format!("{p}_"), l),
        None => (String::new(), word),
    };
    let singular = if last.len() > 3 && last.ends_with("ies") {
        format!("{}y", &last[..last.len() - 3])
    } else if last.len() > 3
        && ["ses", "xes", "zes", "ches", "shes"]
            .iter()
            .any(|suffix| last.ends_with(suffix))
        && !last.ends_with("ases")
    {
        last[..last.len() - 2].to_string()
    } else if last.len() > 2
        && last.ends_with('s')
        && !["ss", "us", "is"].iter().any(|s| last.ends_with(s))
    {
        last[..last.len() - 1].to_string()
    } else {
        last.to_string()
    };
    format!("{prefix}{singular}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_MAIN: &str = r#"//! Shop - Built with Armature Framework

use armature::prelude::*;

mod controllers;

use controllers::health::HealthController;

/// Application module.
#[module(controllers: [HealthController])]
#[derive(Default)]
struct AppModule;

#[tokio::main]
async fn main() {}
"#;

    #[test]
    fn mod_declaration_goes_after_the_last_mod() {
        let out = ensure_mod_declaration(MINIMAL_MAIN, "modules", "").unwrap();
        assert!(out.contains("mod controllers;\nmod modules;\n\nuse controllers"));
        assert_eq!(
            ensure_mod_declaration(&out, "modules", ""),
            None,
            "idempotent"
        );
    }

    #[test]
    fn mod_declaration_without_existing_mods() {
        let src = "//! Docs.\n\nuse armature::prelude::*;\n\nfn main() {}\n";
        let out = ensure_mod_declaration(src, "modules", "pub ").unwrap();
        assert_eq!(
            out,
            "//! Docs.\n\npub mod modules;\n\nuse armature::prelude::*;\n\nfn main() {}\n"
        );
    }

    #[test]
    fn existing_declarations_are_recognized() {
        for src in [
            "pub mod modules;\n",
            "pub(crate) mod modules;\n",
            "mod modules {\n}\n",
            "  mod modules ;\n",
        ] {
            assert_eq!(ensure_mod_declaration(src, "modules", ""), None, "{src}");
        }
        // A longer name is not a match.
        assert!(ensure_mod_declaration("mod modules_old;\n", "modules", "").is_some());
    }

    #[test]
    fn import_is_added_to_app_module() {
        let ListEdit::Added(out) =
            add_to_module_list(MINIMAL_MAIN, "imports", "modules::products::ProductsModule")
        else {
            panic!("expected an edit");
        };
        assert!(out.contains(
            "#[module(imports: [modules::products::ProductsModule], controllers: [HealthController])]"
        ));
        // Second module: appended to the existing list.
        let ListEdit::Added(out) =
            add_to_module_list(&out, "imports", "modules::orders::OrdersModule")
        else {
            panic!("expected an edit");
        };
        assert!(out.contains(
            "imports: [modules::products::ProductsModule, modules::orders::OrdersModule]"
        ));
        assert_eq!(
            add_to_module_list(&out, "imports", "modules::orders::OrdersModule"),
            ListEdit::AlreadyPresent
        );
    }

    #[test]
    fn import_handles_multiline_and_empty_lists() {
        let src = "#[module(\n    imports: [],\n    controllers: [A],\n)]\nstruct AppModule;\n";
        let ListEdit::Added(out) = add_to_module_list(src, "imports", "modules::x::XModule") else {
            panic!()
        };
        assert!(out.contains("imports: [modules::x::XModule],"));

        let src = "#[module(\n    imports: [\n        a::AModule,\n    ],\n)]\nstruct AppModule;\n";
        let ListEdit::Added(out) = add_to_module_list(src, "imports", "modules::x::XModule") else {
            panic!()
        };
        assert!(
            out.contains("a::AModule, modules::x::XModule\n    ]"),
            "{out}"
        );

        let src = "#[module()]\n#[derive(Default)]\nstruct AppModule;\n";
        let ListEdit::Added(out) = add_to_module_list(src, "imports", "m::XModule") else {
            panic!()
        };
        assert!(out.contains("#[module(imports: [m::XModule])]"));
    }

    #[test]
    fn unknown_layouts_are_left_alone() {
        assert_eq!(
            add_to_module_list("fn main() {}\n", "imports", "m::XModule"),
            ListEdit::NoModule
        );
        // `#[module(` that is not followed by a struct.
        let src = "#[module(controllers: [A])]\nfn not_a_module() {}\n";
        assert_eq!(
            add_to_module_list(src, "imports", "m::XModule"),
            ListEdit::NoModule
        );
        assert_eq!(module_struct("struct AppModule;\n"), None);
    }

    #[test]
    fn controllers_and_providers_go_to_any_module() {
        let src = "use armature::prelude::*;\n\n/// Docs.\n#[module(\n    controllers: [],\n    providers: []\n)]\n#[derive(Default)]\npub struct ProductsModule;\n";
        assert_eq!(module_struct(src).as_deref(), Some("ProductsModule"));
        let ListEdit::Added(out) = add_to_module_list(
            src,
            "controllers",
            "products_controller::ProductsController",
        ) else {
            panic!()
        };
        let ListEdit::Added(out) =
            add_to_module_list(&out, "providers", "products_service::ProductsService")
        else {
            panic!()
        };
        assert!(
            out.contains("controllers: [products_controller::ProductsController],"),
            "{out}"
        );
        assert!(
            out.contains("providers: [products_service::ProductsService]\n"),
            "{out}"
        );
        assert_eq!(
            add_to_module_list(&out, "providers", "products_service::ProductsService"),
            ListEdit::AlreadyPresent
        );
        // Same type name at another path is a different type.
        assert!(matches!(
            add_to_module_list(&out, "providers", "crate::x::ProductsService"),
            ListEdit::Added(_)
        ));
        assert_eq!(
            add_to_module_list(
                "#[module(controllers: [HealthController])]\nstruct AppModule;\n",
                "controllers",
                "health::HealthController"
            ),
            ListEdit::AlreadyPresent
        );
        assert_eq!(module_struct(MINIMAL_MAIN).as_deref(), Some("AppModule"));
    }

    #[test]
    fn test_declarations() {
        let TestEdit::Updated(out) =
            ensure_test_declaration("pub mod a_service;\n", "a_service_test")
        else {
            panic!()
        };
        assert_eq!(
            out,
            "pub mod a_service;\n\n#[cfg(test)]\nmod tests {\n    mod a_service_test;\n}\n"
        );
        let TestEdit::Updated(out) = ensure_test_declaration(&out, "a_controller_test") else {
            panic!()
        };
        assert!(
            out.ends_with("mod tests {\n    mod a_service_test;\n    mod a_controller_test;\n}\n"),
            "{out}"
        );
        assert_eq!(
            ensure_test_declaration(&out, "a_service_test"),
            TestEdit::AlreadyDeclared
        );
        assert_eq!(
            ensure_test_declaration("#[cfg(test)]\nmod tests;\n", "x_test"),
            TestEdit::ExternalTestsModule
        );
        assert_eq!(
            ensure_test_declaration("", "x_test"),
            TestEdit::Updated("#[cfg(test)]\nmod tests {\n    mod x_test;\n}\n".into())
        );
    }

    #[test]
    fn singular_names() {
        for (plural, singular) in [
            ("products", "product"),
            ("users", "user"),
            ("categories", "category"),
            ("addresses", "address"),
            ("boxes", "box"),
            ("order_items", "order_item"),
            ("status", "status"),
            ("statuses", "status"),
            ("cases", "case"),
            ("inventory", "inventory"),
            ("bus", "bus"),
        ] {
            assert_eq!(singularize(plural), singular, "{plural}");
        }
    }
}
