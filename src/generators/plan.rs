//! In-memory plan of the files a generator creates or updates.
//!
//! Generators never write directly: they record their changes here (reads
//! see the planned content), then [`Plan::apply`] checks for conflicts and
//! writes everything — or, with `--dry-run`, only prints what would happen,
//! like `nest generate --dry-run`. A conflict therefore never leaves a
//! half-generated element behind.
//!
//! The plan also implements the NestJS wiring rules on top of
//! [`super::wiring`]: declaring modules up to the crate root, declaring
//! tests, and registering elements in the nearest `#[module(…)]`.

use colored::Colorize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::target::{module_segments, segments_dir};
use super::wiring::{self, ListEdit, TestEdit};
use crate::error::{CliError, CliResult};

/// List of a `#[module(…)]` attribute an element is registered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Controllers,
    Providers,
    Imports,
}

impl Slot {
    pub fn key(self) -> &'static str {
        match self {
            Slot::Controllers => "controllers",
            Slot::Providers => "providers",
            Slot::Imports => "imports",
        }
    }
}

#[derive(Debug)]
struct Entry {
    content: String,
    /// The file does not exist on disk yet.
    created: bool,
}

/// Planned changes for one `armature generate` invocation.
#[derive(Debug)]
pub struct Plan {
    root: PathBuf,
    src: PathBuf,
    files: BTreeMap<PathBuf, Entry>,
    order: Vec<PathBuf>,
    notes: Vec<String>,
}

impl Plan {
    /// Plan for the project at `root` (sources in `root/src`).
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            src: root.join("src"),
            files: BTreeMap::new(),
            order: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn src(&self) -> &Path {
        &self.src
    }

    /// Content of `path` as it will be after the plan (planned or on disk).
    pub fn read(&self, path: &Path) -> CliResult<Option<String>> {
        if let Some(entry) = self.files.get(path) {
            return Ok(Some(entry.content.clone()));
        }
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::Command(format!(
                "failed to read {}: {}",
                path.display(),
                e
            ))),
        }
    }

    pub fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path) || path.exists()
    }

    /// Plan a new file. Fails if it already exists (on disk or in the plan).
    pub fn create(&mut self, path: &Path, content: impl Into<String>) -> CliResult<()> {
        if self.exists(path) {
            return Err(CliError::FileExists(self.display(path)));
        }
        self.set(path, content.into());
        Ok(())
    }

    /// Plan the new content of a file (created if it does not exist).
    pub fn write(&mut self, path: &Path, content: impl Into<String>) {
        self.set(path, content.into());
    }

    fn set(&mut self, path: &Path, content: String) {
        match self.files.get_mut(path) {
            Some(entry) => entry.content = content,
            None => {
                self.order.push(path.to_path_buf());
                self.files.insert(
                    path.to_path_buf(),
                    Entry {
                        content,
                        created: !path.exists(),
                    },
                );
            }
        }
    }

    /// Message printed after the file list.
    pub fn note(&mut self, message: impl Into<String>) {
        self.notes.push(message.into());
    }

    /// `src/main.rs`, else `src/lib.rs`.
    pub fn crate_root(&self) -> CliResult<PathBuf> {
        ["main.rs", "lib.rs"]
            .iter()
            .map(|f| self.src.join(f))
            .find(|p| self.exists(p))
            .ok_or_else(|| {
                CliError::Command("no src/main.rs or src/lib.rs in this project".to_string())
            })
    }

    /// File that defines the Rust module of `dir`: the crate root for
    /// `src/`, `<dir>.rs` if the project uses that layout, else `<dir>/mod.rs`.
    pub fn module_file(&self, dir: &Path) -> CliResult<PathBuf> {
        if dir == self.src {
            return self.crate_root();
        }
        let sibling = dir.with_extension("rs");
        if self.exists(&sibling) {
            return Ok(sibling);
        }
        Ok(dir.join("mod.rs"))
    }

    /// Visibility of a child declared in `file`: private in `main.rs`
    /// (nothing outside the binary can use it), `pub` everywhere else.
    fn child_visibility(&self, file: &Path) -> &'static str {
        if file == self.src.join("main.rs") {
            ""
        } else {
            "pub "
        }
    }

    /// Declare `child` in the module defined by `file` (created if missing).
    pub fn declare_child(&mut self, file: &Path, child: &str) -> CliResult<()> {
        let content = self.read(file)?.unwrap_or_default();
        let vis = self.child_visibility(file);
        if let Some(updated) = wiring::ensure_mod_declaration(&content, child, vis) {
            self.write(file, updated);
        } else if !self.exists(file) {
            self.write(file, content);
        }
        Ok(())
    }

    /// Make `src/<segments…>` a declared module tree, up to the crate root.
    pub fn declare_dirs(&mut self, segments: &[String]) -> CliResult<()> {
        for i in 0..segments.len() {
            let parent_dir = segments_dir(&self.src, &segments[..i]);
            let parent_file = self.module_file(&parent_dir)?;
            self.declare_child(&parent_file, &segments[i])?;
            let child_file = self.module_file(&segments_dir(&self.src, &segments[..=i]))?;
            if !self.exists(&child_file) {
                self.write(&child_file, String::new());
            }
        }
        Ok(())
    }

    /// Declare `stem` (file `<dir>/<stem>.rs`) and every parent module.
    pub fn declare_file(&mut self, dir: &Path, stem: &str) -> CliResult<()> {
        let segments = module_segments(&self.src, &dir.join("mod.rs"));
        self.declare_dirs(&segments)?;
        let file = self.module_file(dir)?;
        self.declare_child(&file, stem)
    }

    /// Declare the test file `<dir>/tests/<test_stem>.rs`.
    pub fn declare_test(&mut self, dir: &Path, test_stem: &str) -> CliResult<()> {
        let file = self.module_file(dir)?;
        let content = self.read(&file)?.unwrap_or_default();
        match wiring::ensure_test_declaration(&content, test_stem) {
            TestEdit::Updated(updated) => self.write(&file, updated),
            TestEdit::AlreadyDeclared => {}
            TestEdit::ExternalTestsModule => {
                let tests_dir = dir.join("tests");
                let tests_file = self.module_file(&tests_dir)?;
                self.declare_child(&tests_file, test_stem)?;
            }
        }
        Ok(())
    }

    /// Register `item` (module path from `src/`, type name last) in the
    /// `slot` of the nearest `#[module(…)]`: first `preferred` if given, then
    /// the module of `start_dir`, then each parent up to the crate root.
    /// Like NestJS, without a module to update, the user is told what to add.
    pub fn register(
        &mut self,
        item: &[String],
        slot: Slot,
        start_dir: &Path,
        preferred: Option<&Path>,
    ) -> CliResult<()> {
        let mut candidates: Vec<PathBuf> = preferred.map(Path::to_path_buf).into_iter().collect();
        let mut dir = start_dir.to_path_buf();
        loop {
            candidates.push(self.module_file(&dir)?);
            if dir == self.src || !dir.starts_with(&self.src) {
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => break,
            }
        }

        let type_name = item.last().cloned().unwrap_or_default();
        for file in candidates {
            let Some(content) = self.read(&file)? else {
                continue;
            };
            let Some(module) = wiring::module_struct(&content) else {
                continue;
            };
            let path = relative_path(&module_segments(&self.src, &file), item);
            match wiring::add_to_module_list(&content, slot.key(), &path) {
                ListEdit::Added(updated) => {
                    self.write(&file, updated);
                    self.note(format!(
                        "{type_name} registered in {module} ({}: [...]) — {}",
                        slot.key(),
                        self.display(&file)
                    ));
                }
                ListEdit::AlreadyPresent => {}
                ListEdit::NoModule => continue,
            }
            return Ok(());
        }
        self.note(format!(
            "No #[module(...)] found from {} up to the crate root: add `crate::{}` to the {} of your module",
            self.display(start_dir),
            item.join("::"),
            slot.key()
        ));
        Ok(())
    }

    /// Path relative to the project root, with `/` separators.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Check for conflicts, then write every file (or only print them with
    /// `dry_run`).
    pub fn apply(self, dry_run: bool) -> CliResult<()> {
        for path in &self.order {
            let entry = &self.files[path];
            if entry.created && path.exists() {
                return Err(CliError::FileExists(self.display(path)));
            }
        }
        for path in &self.order {
            let entry = &self.files[path];
            let label = if entry.created {
                "CREATE".green().bold()
            } else {
                "UPDATE".yellow().bold()
            };
            if !dry_run {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        CliError::Command(format!(
                            "failed to create directory {}: {}",
                            parent.display(),
                            e
                        ))
                    })?;
                }
                std::fs::write(path, &entry.content).map_err(|e| {
                    CliError::Command(format!("failed to write {}: {}", path.display(), e))
                })?;
            }
            println!(
                "  {} {} ({} bytes)",
                label,
                self.display(path),
                entry.content.len()
            );
        }
        for note in &self.notes {
            println!("  {} {}", "→".cyan(), note);
        }
        if dry_run {
            println!(
                "\n  {} Dry run: no files were written.",
                "NOTE".yellow().bold()
            );
        }
        Ok(())
    }
}

/// `item` as seen from the module `from`: relative when it is a descendant,
/// `crate::…` otherwise.
pub fn relative_path(from: &[String], item: &[String]) -> String {
    if item.len() > from.len() && item.starts_with(from) {
        item[from.len()..].join("::")
    } else {
        format!("crate::{}", item.join("::"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "use armature::prelude::*;\n\nmod controllers;\n\n#[module(controllers: [HealthController])]\n#[derive(Default)]\nstruct AppModule;\n\nfn main() {}\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative_path(&[], &s(&["a", "B"])), "a::B");
        assert_eq!(relative_path(&s(&["a"]), &s(&["a", "b", "C"])), "b::C");
        assert_eq!(relative_path(&s(&["x"]), &s(&["a", "C"])), "crate::a::C");
    }

    #[test]
    fn declaring_a_file_declares_every_parent() {
        let (_tmp, root) = project();
        let mut plan = Plan::new(&root);
        let dir = root.join("src/modules/products");
        plan.declare_file(&dir, "products_controller").unwrap();

        let main = plan.read(&root.join("src/main.rs")).unwrap().unwrap();
        assert!(main.contains("mod controllers;\nmod modules;\n"), "{main}");
        assert_eq!(
            plan.read(&root.join("src/modules/mod.rs"))
                .unwrap()
                .unwrap(),
            "pub mod products;\n"
        );
        assert_eq!(
            plan.read(&dir.join("mod.rs")).unwrap().unwrap(),
            "pub mod products_controller;\n"
        );
        // Nothing is written before apply.
        assert!(!root.join("src/modules").exists());
    }

    #[test]
    fn registration_goes_to_the_nearest_module() {
        let (_tmp, root) = project();
        let mut plan = Plan::new(&root);
        let dir = root.join("src/modules/products");
        plan.write(
            &dir.join("mod.rs"),
            "#[module(controllers: [])]\n#[derive(Default)]\npub struct ProductsModule;\n",
        );
        let item = s(&[
            "modules",
            "products",
            "products_controller",
            "ProductsController",
        ]);
        plan.register(&item, Slot::Controllers, &dir, None).unwrap();
        let module = plan.read(&dir.join("mod.rs")).unwrap().unwrap();
        assert!(module.contains("controllers: [products_controller::ProductsController]"));

        // Without a module in its directory, it goes up to AppModule.
        let other = root.join("src/orders");
        let item = s(&["orders", "orders_controller", "OrdersController"]);
        plan.register(&item, Slot::Controllers, &other, None)
            .unwrap();
        let main = plan.read(&root.join("src/main.rs")).unwrap().unwrap();
        assert!(
            main.contains(
                "controllers: [HealthController, orders::orders_controller::OrdersController]"
            ),
            "{main}"
        );
    }

    #[test]
    fn apply_refuses_conflicts_and_dry_run_writes_nothing() {
        let (_tmp, root) = project();
        let mut plan = Plan::new(&root);
        assert!(plan.create(&root.join("src/main.rs"), "x").is_err());
        plan.create(&root.join("src/a/b.rs"), "x").unwrap();
        assert!(plan.create(&root.join("src/a/b.rs"), "y").is_err());
        plan.apply(true).unwrap();
        assert!(!root.join("src/a").exists());

        let mut plan = Plan::new(&root);
        plan.create(&root.join("src/a/b.rs"), "x").unwrap();
        plan.apply(false).unwrap();
        assert_eq!(fs::read_to_string(root.join("src/a/b.rs")).unwrap(), "x");
    }

    #[test]
    fn sibling_module_files_are_respected() {
        let (_tmp, root) = project();
        fs::write(root.join("src/modules.rs"), "pub mod users;\n").unwrap();
        let mut plan = Plan::new(&root);
        plan.declare_file(&root.join("src/modules/products"), "products_service")
            .unwrap();
        let modules = plan.read(&root.join("src/modules.rs")).unwrap().unwrap();
        assert_eq!(modules, "pub mod users;\npub mod products;\n");
        assert!(!plan.exists(&root.join("src/modules/mod.rs")));
    }
}
