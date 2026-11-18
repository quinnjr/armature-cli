//! Watch scope: which paths are build inputs of the project and which are
//! ignored.
//!
//! The scope is derived from `cargo metadata` so that it reflects the real
//! compilation inputs (package sources, manifests, lockfile, build scripts,
//! workspace path dependencies) instead of a hard-coded `src/**/*.rs`.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::config::DevConfig;

/// Directory names that never contain build inputs, wherever they appear.
///
/// `target/` is deliberately *not* in this list: a module may legitimately be
/// called `target`. The real Cargo target directories are excluded by path.
const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    ".idea",
    ".vscode",
    ".fleet",
    ".direnv",
    ".cache",
    "node_modules",
    "__pycache__",
];

/// Files at the workspace root that influence a Cargo build.
const WORKSPACE_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config",
    ".cargo/config.toml",
];

/// One `cargo metadata` package that is an input of the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLayout {
    pub name: String,
    pub id: String,
    pub manifest_dir: PathBuf,
    /// Source entry points of the lib/bin/proc-macro/build-script targets.
    pub target_sources: Vec<PathBuf>,
}

/// Layout of the project, as reported by `cargo metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLayout {
    pub project_root: PathBuf,
    pub workspace_root: PathBuf,
    pub target_dir: PathBuf,
    pub packages: Vec<PackageLayout>,
    /// Package whose manifest is `<project_root>/Cargo.toml`, if any.
    pub root_package_id: Option<String>,
    /// `default-run` of the root package.
    pub default_run: Option<String>,
}

impl ProjectLayout {
    /// Ask Cargo for the layout; fall back to the conventional layout if
    /// `cargo metadata` is unavailable or fails (e.g. broken manifest).
    pub fn discover(project_root: &Path) -> (Self, Option<String>) {
        let root = canonical_or_self(project_root);
        match Self::from_cargo_metadata(&root) {
            Ok(layout) => (layout, None),
            Err(reason) => (Self::conventional(&root), Some(reason)),
        }
    }

    /// Single package at the root with the default Cargo layout.
    pub fn conventional(project_root: &Path) -> Self {
        let root = project_root.to_path_buf();
        Self {
            project_root: root.clone(),
            workspace_root: root.clone(),
            target_dir: root.join("target"),
            packages: vec![PackageLayout {
                name: String::new(),
                id: String::new(),
                manifest_dir: root.clone(),
                target_sources: Vec::new(),
            }],
            root_package_id: None,
            default_run: None,
        }
    }

    fn from_cargo_metadata(root: &Path) -> Result<Self, String> {
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(root.join("Cargo.toml"))
            .current_dir(root)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run `cargo metadata`: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "`cargo metadata` failed: {}",
                stderr.lines().last().unwrap_or("unknown error")
            ));
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("invalid `cargo metadata` output: {e}"))?;
        Self::from_metadata_json(root, &json)
    }

    /// Build the layout from `cargo metadata --no-deps` JSON.
    pub fn from_metadata_json(root: &Path, json: &serde_json::Value) -> Result<Self, String> {
        let str_field = |v: &serde_json::Value, key: &str| -> Option<String> {
            v.get(key).and_then(|x| x.as_str()).map(str::to_string)
        };
        let workspace_root = str_field(json, "workspace_root")
            .map(|p| canonical_or_self(Path::new(&p)))
            .ok_or("missing workspace_root")?;
        let target_dir = str_field(json, "target_directory")
            .map(|p| canonical_or_self(Path::new(&p)))
            .unwrap_or_else(|| workspace_root.join("target"));
        let packages = json
            .get("packages")
            .and_then(|p| p.as_array())
            .ok_or("missing packages")?;

        struct Pkg<'a> {
            raw: &'a serde_json::Value,
            id: String,
            dir: PathBuf,
        }
        let all: Vec<Pkg> = packages
            .iter()
            .filter_map(|raw| {
                let id = str_field(raw, "id")?;
                let manifest = str_field(raw, "manifest_path")?;
                let dir = canonical_or_self(Path::new(&manifest).parent()?);
                Some(Pkg { raw, id, dir })
            })
            .collect();

        let root_pkg = all.iter().position(|p| p.dir == root);

        // Packages that feed the build: the root package and its path
        // dependencies (normal and build), transitively. Without a root
        // package (virtual workspace) every default member is an input.
        let mut selected: Vec<usize> = Vec::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        match root_pkg {
            Some(i) => queue.push_back(i),
            None => {
                let defaults: Vec<String> = json
                    .get("workspace_default_members")
                    .or_else(|| json.get("workspace_members"))
                    .and_then(|m| m.as_array())
                    .map(|m| {
                        m.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for (i, p) in all.iter().enumerate() {
                    if defaults.is_empty() || defaults.contains(&p.id) {
                        queue.push_back(i);
                    }
                }
            }
        }
        while let Some(i) = queue.pop_front() {
            if selected.contains(&i) {
                continue;
            }
            selected.push(i);
            let deps = all[i]
                .raw
                .get("dependencies")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            for dep in deps {
                if dep.get("kind").and_then(|k| k.as_str()) == Some("dev") {
                    continue;
                }
                let Some(path) = str_field(&dep, "path") else {
                    continue;
                };
                let dir = canonical_or_self(Path::new(&path));
                if let Some(j) = all.iter().position(|p| p.dir == dir) {
                    queue.push_back(j);
                }
            }
        }

        let packages = selected
            .iter()
            .map(|&i| {
                let p = &all[i];
                let target_sources = p
                    .raw
                    .get("targets")
                    .and_then(|t| t.as_array())
                    .map(|targets| {
                        targets
                            .iter()
                            .filter(|t| {
                                t.get("kind")
                                    .and_then(|k| k.as_array())
                                    .is_some_and(|kinds| {
                                        kinds.iter().any(|k| {
                                            matches!(
                                                k.as_str(),
                                                Some(
                                                    "lib"
                                                        | "rlib"
                                                        | "dylib"
                                                        | "cdylib"
                                                        | "staticlib"
                                                        | "proc-macro"
                                                        | "bin"
                                                        | "custom-build"
                                                )
                                            )
                                        })
                                    })
                            })
                            .filter_map(|t| str_field(t, "src_path"))
                            .map(|s| canonical_or_self(Path::new(&s)))
                            .collect()
                    })
                    .unwrap_or_default();
                PackageLayout {
                    name: str_field(p.raw, "name").unwrap_or_default(),
                    id: p.id.clone(),
                    manifest_dir: p.dir.clone(),
                    target_sources,
                }
            })
            .collect();

        Ok(Self {
            project_root: root.to_path_buf(),
            workspace_root,
            target_dir,
            packages,
            root_package_id: root_pkg.map(|i| all[i].id.clone()),
            default_run: root_pkg.and_then(|i| str_field(all[i].raw, "default_run")),
        })
    }
}

/// Why a path was ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Inside a Cargo target directory.
    TargetDir,
    /// Inside a VCS, IDE or cache directory (`.git/`, `node_modules/`, …).
    IgnoredDir(String),
    /// Matched `exclude_paths`.
    UserExcluded,
    /// Editor swap, backup or temporary file.
    EditorTemp,
    /// Not one of the build inputs.
    OutOfScope,
}

impl fmt::Display for IgnoreReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IgnoreReason::TargetDir => write!(f, "inside target directory"),
            IgnoreReason::IgnoredDir(name) => write!(f, "inside ignored directory {name}/"),
            IgnoreReason::UserExcluded => write!(f, "matches exclude_paths"),
            IgnoreReason::EditorTemp => write!(f, "editor temporary file"),
            IgnoreReason::OutOfScope => write!(f, "not a build input"),
        }
    }
}

/// Classification of a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathVerdict {
    Relevant,
    Ignored(IgnoreReason),
}

/// A directory to register with the OS watcher.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WatchRegistration {
    pub path: PathBuf,
    pub recursive: bool,
}

/// The set of watched and relevant paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchScope {
    root: PathBuf,
    /// Every file below these directories is a build input (minus exclusions).
    recursive_dirs: BTreeSet<PathBuf>,
    /// Individual build-input files (watched through their parent directory,
    /// so atomic "write temp + rename" saves are still seen).
    files: BTreeSet<PathBuf>,
    target_dirs: BTreeSet<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl WatchScope {
    pub fn new(layout: &ProjectLayout, config: &DevConfig) -> Self {
        let root = layout.project_root.clone();
        let mut recursive_dirs = BTreeSet::new();
        let mut files = BTreeSet::new();
        let mut target_dirs = BTreeSet::new();
        target_dirs.insert(layout.target_dir.clone());

        for name in WORKSPACE_FILES {
            files.insert(layout.workspace_root.join(name));
        }
        // Project configuration read by the application at runtime.
        files.insert(root.join(super::config::CONFIG_FILE));

        for package in &layout.packages {
            let dir = &package.manifest_dir;
            files.insert(dir.join("Cargo.toml"));
            files.insert(dir.join("build.rs"));
            recursive_dirs.insert(dir.join("src"));
            target_dirs.insert(dir.join("target"));
            for source in &package.target_sources {
                match source.parent() {
                    // Entry point at the package root (e.g. `build.rs`,
                    // `path = "main.rs"`): never watch the package root
                    // recursively, it contains `target/`.
                    Some(parent) if parent == dir => {
                        files.insert(source.clone());
                    }
                    Some(parent) => {
                        recursive_dirs.insert(parent.to_path_buf());
                    }
                    None => {}
                }
            }
        }

        for extra in &config.extra_watch_paths {
            let path = canonical_or_self(&absolutize(&root, extra));
            if path.is_dir() {
                recursive_dirs.insert(path);
            } else {
                files.insert(path);
            }
        }

        let excluded = config
            .exclude_paths
            .iter()
            .map(|p| canonical_or_self(&absolutize(&root, p)))
            .collect();

        // Drop recursive dirs nested in another one.
        let all: Vec<PathBuf> = recursive_dirs.iter().cloned().collect();
        recursive_dirs.retain(|d| !all.iter().any(|o| o != d && d.starts_with(o)));

        Self {
            root,
            recursive_dirs,
            files,
            target_dirs,
            excluded,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn recursive_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.recursive_dirs.iter()
    }

    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter()
    }

    /// Directories to register with the OS watcher. Directories that do not
    /// exist yet cannot be watched and are skipped.
    pub fn registrations(&self) -> Vec<WatchRegistration> {
        let mut regs: BTreeSet<WatchRegistration> = self
            .recursive_dirs
            .iter()
            .filter(|d| d.is_dir())
            .map(|d| WatchRegistration {
                path: d.clone(),
                recursive: true,
            })
            .collect();
        for file in &self.files {
            let Some(parent) = file.parent() else {
                continue;
            };
            if !parent.is_dir() || self.recursive_dirs.iter().any(|d| parent.starts_with(d)) {
                continue;
            }
            regs.insert(WatchRegistration {
                path: parent.to_path_buf(),
                recursive: false,
            });
        }
        regs.into_iter().collect()
    }

    /// Short human-readable list of what is watched.
    pub fn summary(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .recursive_dirs
            .iter()
            .filter(|d| d.is_dir())
            .map(|d| format!("{}/", self.display(d)))
            .collect();
        out.extend(
            self.files
                .iter()
                .filter(|f| f.is_file())
                .map(|f| self.display(f)),
        );
        out
    }

    /// Classify a (normalized, absolute) path.
    pub fn classify(&self, path: &Path) -> PathVerdict {
        if self.target_dirs.iter().any(|t| path.starts_with(t)) {
            return PathVerdict::Ignored(IgnoreReason::TargetDir);
        }
        if self.excluded.iter().any(|e| path.starts_with(e)) {
            return PathVerdict::Ignored(IgnoreReason::UserExcluded);
        }
        // Only look at components below the watched base, so that a project
        // living in e.g. `~/.cache/…` is not ignored as a whole.
        let base = self
            .recursive_dirs
            .iter()
            .find(|d| path.starts_with(d))
            .map(PathBuf::as_path)
            .or_else(|| path.starts_with(&self.root).then_some(self.root.as_path()))
            .or_else(|| path.parent());
        let relative = base.and_then(|b| path.strip_prefix(b).ok()).unwrap_or(path);
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue;
            };
            let is_last = components.peek().is_none();
            if !is_last && let Some(dir) = name.to_str().filter(|n| IGNORED_DIR_NAMES.contains(n)) {
                return PathVerdict::Ignored(IgnoreReason::IgnoredDir(dir.to_string()));
            }
        }
        if path.file_name().is_some_and(is_editor_temp_file) {
            return PathVerdict::Ignored(IgnoreReason::EditorTemp);
        }
        if self.files.contains(path) || self.recursive_dirs.iter().any(|d| path.starts_with(d)) {
            return PathVerdict::Relevant;
        }
        PathVerdict::Ignored(IgnoreReason::OutOfScope)
    }

    /// Whether a directory may contain relevant files (used to prune walks).
    pub fn may_contain_relevant(&self, dir: &Path) -> bool {
        !matches!(
            self.classify(&dir.join("x")),
            PathVerdict::Ignored(
                IgnoreReason::TargetDir | IgnoreReason::UserExcluded | IgnoreReason::IgnoredDir(_)
            )
        )
    }

    /// Normalize a path reported by the OS: make it absolute, resolve `.`
    /// and `..`, and resolve symlinks of the longest existing ancestor (the
    /// file itself may already be gone).
    pub fn normalize(&self, raw: &Path) -> PathBuf {
        let absolute = lexical_clean(&absolutize(&self.root, raw));
        let mut existing = absolute.as_path();
        let mut rest: Vec<&OsStr> = Vec::new();
        loop {
            if let Ok(canonical) = existing.canonicalize() {
                let mut out = canonical;
                out.extend(rest.iter().rev());
                return out;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name);
                    existing = parent;
                }
                _ => return absolute,
            }
        }
    }

    /// Path relative to the project root, with `/` separators.
    pub fn display(&self, path: &Path) -> String {
        let shown = path.strip_prefix(&self.root).unwrap_or(path);
        let text = shown.to_string_lossy().replace('\\', "/");
        if text.is_empty() {
            ".".to_string()
        } else {
            text
        }
    }

    /// Whether `path` is a Cargo manifest (a change may alter the layout).
    pub fn is_manifest(path: &Path) -> bool {
        path.file_name() == Some(OsStr::new("Cargo.toml"))
    }
}

/// Editor swap/backup/temporary file names (vim, emacs, JetBrains, Kate,
/// GNOME, VS Code, macOS…). Hidden files in general are *not* ignored.
pub fn is_editor_temp_file(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    name == "4913" // vim's write-permission probe
        || name == ".DS_Store"
        || name.ends_with('~')
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name.contains("___jb_tmp___")
        || name.contains("___jb_old___")
        || name.starts_with(".goutputstream-")
        || [
            ".swp", ".swo", ".swx", ".swn", ".tmp", ".temp", ".bak", ".orig", ".rej",
            ".kate-swp", ".crswap", ".part",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn absolutize(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| lexical_clean(path))
}

/// Resolve `.` and `..` without touching the filesystem.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    /// Project with `src/`, `Cargo.toml`, `Cargo.lock` and `target/`.
    pub(crate) fn sample_project() -> (tempfile::TempDir, WatchScope) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/controllers")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("Cargo.lock"), "").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/controllers/health.rs"), "// health\n").unwrap();
        let layout = ProjectLayout::conventional(&root);
        let scope = WatchScope::new(&layout, &DevConfig::default());
        (tmp, scope)
    }

    #[test]
    fn source_files_are_relevant() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        assert_eq!(
            scope.classify(&root.join("src/main.rs")),
            PathVerdict::Relevant
        );
        assert_eq!(
            scope.classify(&root.join("src/controllers/health.rs")),
            PathVerdict::Relevant
        );
        // Non-Rust inputs (include_str!, templates) are build inputs too.
        assert_eq!(
            scope.classify(&root.join("src/schema.sql")),
            PathVerdict::Relevant
        );
        // A module may be called `target`.
        assert_eq!(
            scope.classify(&root.join("src/target/mod.rs")),
            PathVerdict::Relevant
        );
    }

    #[test]
    fn manifests_lockfile_and_build_script_are_relevant() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        for file in [
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            ".cargo/config.toml",
            "armature.toml",
        ] {
            assert_eq!(
                scope.classify(&root.join(file)),
                PathVerdict::Relevant,
                "{file}"
            );
        }
        assert_eq!(
            scope.classify(&root.join("README.md")),
            PathVerdict::Ignored(IgnoreReason::OutOfScope)
        );
    }

    #[test]
    fn target_and_git_are_ignored() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        assert_eq!(
            scope.classify(&root.join("target/debug/demo")),
            PathVerdict::Ignored(IgnoreReason::TargetDir)
        );
        assert_eq!(
            scope.classify(&root.join(".git/index")),
            PathVerdict::Ignored(IgnoreReason::IgnoredDir(".git".into()))
        );
        assert_eq!(
            scope.classify(&root.join("src/.git/HEAD")),
            PathVerdict::Ignored(IgnoreReason::IgnoredDir(".git".into()))
        );
    }

    #[test]
    fn editor_temporary_files_are_ignored() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        for name in [
            "src/.main.rs.swp",
            "src/main.rs~",
            "src/4913",
            "src/.#main.rs",
            "src/#main.rs#",
            "src/main.rs___jb_tmp___",
        ] {
            assert_eq!(
                scope.classify(&root.join(name)),
                PathVerdict::Ignored(IgnoreReason::EditorTemp),
                "{name}"
            );
        }
        // Hidden files are not blindly ignored.
        assert_eq!(
            scope.classify(&root.join("src/.env.sql")),
            PathVerdict::Relevant
        );
    }

    #[test]
    fn user_exclusions_and_extra_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        let config = DevConfig {
            extra_watch_paths: vec!["config".into(), "schema.graphql".into()],
            exclude_paths: vec!["src/generated".into()],
            ..DevConfig::default()
        };
        let scope = WatchScope::new(&ProjectLayout::conventional(&root), &config);
        assert_eq!(
            scope.classify(&root.join("src/generated/api.rs")),
            PathVerdict::Ignored(IgnoreReason::UserExcluded)
        );
        assert_eq!(
            scope.classify(&root.join("config/app.toml")),
            PathVerdict::Relevant
        );
        assert_eq!(
            scope.classify(&root.join("schema.graphql")),
            PathVerdict::Relevant
        );
    }

    #[test]
    fn registrations_never_watch_the_root_recursively() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        let regs = scope.registrations();
        assert!(regs.contains(&WatchRegistration {
            path: root.join("src"),
            recursive: true
        }));
        assert!(regs.contains(&WatchRegistration {
            path: root.clone(),
            recursive: false
        }));
        assert!(!regs.iter().any(|r| r.recursive && r.path == root));
    }

    #[test]
    fn normalize_resolves_dot_segments_and_missing_files() {
        let (_tmp, scope) = sample_project();
        let root = scope.root().to_path_buf();
        assert_eq!(
            scope.normalize(&root.join("src/./controllers/../main.rs")),
            root.join("src/main.rs")
        );
        assert_eq!(
            scope.normalize(&root.join("src/deleted.rs")),
            root.join("src/deleted.rs")
        );
        assert_eq!(
            scope.normalize(Path::new("src/main.rs")),
            root.join("src/main.rs")
        );
    }

    #[test]
    fn metadata_selects_root_package_and_path_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        for dir in ["api/src", "shared/src", "unrelated/src", "tools/src"] {
            fs::create_dir_all(ws.join(dir)).unwrap();
        }
        let s = |p: &Path| p.to_string_lossy().to_string();
        let json = serde_json::json!({
            "workspace_root": s(&ws),
            "target_directory": s(&ws.join("target")),
            "workspace_members": ["api-id", "shared-id", "unrelated-id"],
            "packages": [
                {
                    "name": "api", "id": "api-id", "default_run": "api",
                    "manifest_path": s(&ws.join("api/Cargo.toml")),
                    "targets": [
                        {"kind": ["bin"], "name": "api", "src_path": s(&ws.join("api/src/main.rs"))},
                        {"kind": ["custom-build"], "name": "build-script-build", "src_path": s(&ws.join("api/build.rs"))},
                        {"kind": ["test"], "name": "it", "src_path": s(&ws.join("api/tests/it.rs"))}
                    ],
                    "dependencies": [
                        {"name": "shared", "kind": null, "path": s(&ws.join("shared"))},
                        {"name": "tools", "kind": "dev", "path": s(&ws.join("tools"))},
                        {"name": "serde", "kind": null}
                    ]
                },
                {
                    "name": "shared", "id": "shared-id",
                    "manifest_path": s(&ws.join("shared/Cargo.toml")),
                    "targets": [{"kind": ["lib"], "name": "shared", "src_path": s(&ws.join("shared/src/lib.rs"))}],
                    "dependencies": []
                },
                {
                    "name": "unrelated", "id": "unrelated-id",
                    "manifest_path": s(&ws.join("unrelated/Cargo.toml")),
                    "targets": [], "dependencies": []
                },
                {
                    "name": "tools", "id": "tools-id",
                    "manifest_path": s(&ws.join("tools/Cargo.toml")),
                    "targets": [], "dependencies": []
                }
            ]
        });
        let layout = ProjectLayout::from_metadata_json(&ws.join("api"), &json).unwrap();
        let names: Vec<&str> = layout.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["api", "shared"]);
        assert_eq!(layout.root_package_id.as_deref(), Some("api-id"));
        assert_eq!(layout.default_run.as_deref(), Some("api"));

        let scope = WatchScope::new(&layout, &DevConfig::default());
        assert_eq!(
            scope.classify(&ws.join("shared/src/lib.rs")),
            PathVerdict::Relevant
        );
        assert_eq!(
            scope.classify(&ws.join("Cargo.lock")),
            PathVerdict::Relevant
        );
        assert_eq!(
            scope.classify(&ws.join("api/build.rs")),
            PathVerdict::Relevant
        );
        assert_eq!(
            scope.classify(&ws.join("unrelated/src/lib.rs")),
            PathVerdict::Ignored(IgnoreReason::OutOfScope)
        );
        assert_eq!(
            scope.classify(&ws.join("target/debug/api")),
            PathVerdict::Ignored(IgnoreReason::TargetDir)
        );
    }
}
