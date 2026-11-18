//! Where a generated element goes, NestJS style.
//!
//! The name argument carries the path: `armature generate controller
//! modules/products` puts the controller in `src/modules/products/`.
//!
//! | Argument            | Flag     | Directory              | File                       |
//! |---------------------|----------|------------------------|----------------------------|
//! | `products`          |          | `src/products/`        | `products_controller.rs`   |
//! | `modules/products`  |          | `src/modules/products/`| `products_controller.rs`   |
//! | `modules/products`  | `--flat` | `src/modules/`         | `products_controller.rs`   |
//! | `products`          | `--flat` | `src/`                 | `products_controller.rs`   |
//!
//! Tests go to `tests/<file>_test.rs` next to the element. Directory names are
//! Rust module names, so every segment is converted to `snake_case`.

use heck::ToSnakeCase;
use std::path::{Component, Path, PathBuf};

use super::utils::NameCases;
use crate::error::{CliError, CliResult};

/// Rust keywords that cannot be used as module names.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield", "main", "lib", "tests",
];

/// Resolved location of a generated element.
#[derive(Debug, Clone)]
pub struct Target {
    /// Directories under `src/` given in the argument (`modules/products` →
    /// `["modules"]`), already `snake_case`.
    pub path: Vec<String>,
    /// The element name (last segment).
    pub names: NameCases,
    /// `--flat`: no dedicated directory for the element.
    pub flat: bool,
}

impl Target {
    /// Parse the name argument. Accepts `/` or `\` separators and an optional
    /// leading `src/` or `./`.
    pub fn parse(arg: &str, flat: bool) -> CliResult<Self> {
        let normalized = arg.replace('\\', "/");
        if normalized.starts_with('/') || Path::new(arg).is_absolute() {
            return Err(CliError::InvalidArgument(format!(
                "`{arg}`: give a path relative to src/, e.g. modules/products"
            )));
        }
        let mut parts: Vec<&str> = normalized
            .split('/')
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.first() == Some(&"src") && parts.len() > 1 {
            parts.remove(0);
        }
        if parts.contains(&"..") {
            return Err(CliError::InvalidArgument(format!(
                "`{arg}`: `..` is not allowed, the element must stay inside src/"
            )));
        }
        let Some((name, dirs)) = parts.split_last() else {
            return Err(CliError::InvalidArgument(
                "the name cannot be empty".to_string(),
            ));
        };
        let path = dirs
            .iter()
            .map(|d| module_name(d, arg))
            .collect::<CliResult<Vec<_>>>()?;
        let names = NameCases::from(name);
        module_name(&names.snake, arg)?;
        Ok(Self { path, names, flat })
    }

    /// Module path segments of the element's directory, relative to `src/`.
    pub fn dir_segments(&self) -> Vec<String> {
        let mut segments = self.path.clone();
        if !self.flat {
            segments.push(self.names.snake.clone());
        }
        segments
    }

    /// The element's directory.
    pub fn dir(&self, src: &Path) -> PathBuf {
        segments_dir(src, &self.dir_segments())
    }

    /// File stem of an element of `kind` (`products_controller`; just
    /// `products` when `kind` is empty).
    pub fn stem(&self, kind: &str) -> String {
        if kind.is_empty() {
            self.names.snake.clone()
        } else {
            format!("{}_{kind}", self.names.snake)
        }
    }
}

/// `src/<segments…>`.
pub fn segments_dir(src: &Path, segments: &[String]) -> PathBuf {
    segments
        .iter()
        .fold(src.to_path_buf(), |dir, s| dir.join(s))
}

/// Module path of a source file relative to `src/` (`src/a/b/mod.rs` →
/// `[a, b]`, `src/a/c.rs` → `[a, c]`, `src/main.rs` → `[]`).
pub fn module_segments(src: &Path, file: &Path) -> Vec<String> {
    let Ok(rel) = file.strip_prefix(src) else {
        return Vec::new();
    };
    let mut segments: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if let Some(last) = segments.pop() {
        let stem = last.strip_suffix(".rs").unwrap_or(&last).to_string();
        let is_root = segments.is_empty() && (stem == "main" || stem == "lib");
        if stem != "mod" && !is_root {
            segments.push(stem);
        }
    }
    segments
}

fn module_name(segment: &str, arg: &str) -> CliResult<String> {
    let snake = segment.to_snake_case();
    let valid = !snake.is_empty()
        && snake
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && snake.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid || KEYWORDS.contains(&snake.as_str()) {
        return Err(CliError::InvalidArgument(format!(
            "`{arg}`: `{segment}` cannot be used as a Rust module name"
        )));
    }
    Ok(snake)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> PathBuf {
        PathBuf::from("/p/src")
    }

    #[test]
    fn name_without_path_gets_its_own_directory() {
        let t = Target::parse("products", false).unwrap();
        assert!(t.path.is_empty());
        assert_eq!(t.dir(&src()), PathBuf::from("/p/src/products"));
        assert_eq!(t.stem("controller"), "products_controller");
        assert_eq!(t.stem(""), "products");
    }

    #[test]
    fn path_comes_from_the_argument() {
        let t = Target::parse("modules/products", false).unwrap();
        assert_eq!(t.path, vec!["modules"]);
        assert_eq!(t.dir(&src()), PathBuf::from("/p/src/modules/products"));
        assert_eq!(t.dir_segments(), vec!["modules", "products"]);

        let flat = Target::parse("modules/products", true).unwrap();
        assert_eq!(flat.dir(&src()), PathBuf::from("/p/src/modules"));
        assert_eq!(Target::parse("products", true).unwrap().dir(&src()), src());
    }

    #[test]
    fn segments_are_normalized() {
        let t = Target::parse("./src/Api/V1/order-items", false).unwrap();
        assert_eq!(t.path, vec!["api", "v1"]);
        assert_eq!(t.names.snake, "order_items");
        assert_eq!(t.names.pascal, "OrderItems");
        assert_eq!(t.names.kebab, "order-items");
        assert_eq!(
            Target::parse("api\\users", false).unwrap().path,
            vec!["api"]
        );
    }

    #[test]
    fn invalid_names_are_rejected() {
        for bad in [
            "",
            "/abs/x",
            "../x",
            "a/../b",
            "mod",
            "modules/self",
            "1st",
            "a/é/x",
            "main",
        ] {
            assert!(
                Target::parse(bad, false).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn module_segments_of_files() {
        let s = src();
        assert!(module_segments(&s, &s.join("main.rs")).is_empty());
        assert!(module_segments(&s, &s.join("lib.rs")).is_empty());
        assert_eq!(
            module_segments(&s, &s.join("modules/mod.rs")),
            vec!["modules"]
        );
        assert_eq!(
            module_segments(&s, &s.join("modules/products/products_controller.rs")),
            vec!["modules", "products", "products_controller"]
        );
        assert_eq!(module_segments(&s, &s.join("modules.rs")), vec!["modules"]);
    }
}
