//! Confirmation that a candidate path really changed.
//!
//! A filesystem event alone does not prove that a file's content changed
//! (`touch`, an editor re-saving identical content, `git checkout` of the
//! same blob, ambiguous `Modify(Any)` events on Windows…). The snapshot keeps
//! a fingerprint (size, modification time, content hash) of every build
//! input and compares it when the debouncer emits a batch — once per path
//! and per batch, not once per event.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::scope::{PathVerdict, WatchScope};

/// Files larger than this are compared by size and mtime only.
const MAX_HASHED_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    hash: Option<u64>,
}

impl Fingerprint {
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let len = meta.len();
        let hash = if len <= MAX_HASHED_SIZE {
            let mut file = std::fs::File::open(path)?;
            let mut hasher = std::hash::DefaultHasher::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.write(&buf[..n]);
            }
            Some(hasher.finish())
        } else {
            None
        };
        Ok(Self {
            len,
            modified: meta.modified().ok(),
            hash,
        })
    }

    /// Same content? The hash is authoritative when both sides have one;
    /// the modification time alone is never treated as proof of a change
    /// unless the file is too large to hash.
    fn same_content(&self, other: &Self) -> bool {
        match (self.hash, other.hash) {
            (Some(a), Some(b)) => self.len == other.len && a == b,
            _ => self.len == other.len && self.modified == other.modified,
        }
    }
}

/// Nature of a confirmed change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Created,
    Modified,
    Removed,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Removed => "removed",
        })
    }
}

/// Confirmed changes of build inputs, keyed (and sorted) by path.
pub type ChangeSet = BTreeMap<PathBuf, ChangeKind>;

/// Fingerprints of every known build input.
#[derive(Debug, Default)]
pub struct FileSnapshot {
    entries: BTreeMap<PathBuf, Fingerprint>,
}

impl FileSnapshot {
    /// Fingerprint every build input currently in the scope.
    pub fn scan(scope: &WatchScope) -> Self {
        let mut snapshot = Self::default();
        for (path, fp) in current_files(scope) {
            snapshot.entries.insert(path, fp);
        }
        snapshot
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Re-check the given candidate paths (files or directories, existing or
    /// not) and return the confirmed changes. The snapshot is updated.
    pub fn refresh<'a, I>(&mut self, scope: &WatchScope, paths: I) -> ChangeSet
    where
        I: IntoIterator<Item = &'a PathBuf>,
    {
        let mut changes = ChangeSet::new();
        for path in paths {
            self.refresh_one(scope, path, &mut changes);
        }
        changes
    }

    /// Compare the whole scope (after the OS reported dropped events, or
    /// after the scope itself changed).
    pub fn rescan(&mut self, scope: &WatchScope) -> ChangeSet {
        let current: BTreeMap<PathBuf, Fingerprint> = current_files(scope).collect();
        let mut changes = ChangeSet::new();
        for (path, fp) in &current {
            self.record(path, Some(fp.clone()), &mut changes);
        }
        let gone: Vec<PathBuf> = self
            .entries
            .keys()
            .filter(|p| !current.contains_key(*p))
            .cloned()
            .collect();
        for path in gone {
            self.record(&path, None, &mut changes);
        }
        changes
    }

    fn refresh_one(&mut self, scope: &WatchScope, path: &Path, changes: &mut ChangeSet) {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                // A directory appeared, disappeared or was moved: compare
                // everything below it.
                let below: BTreeMap<PathBuf, Fingerprint> = walk(scope, path).collect();
                for (file, fp) in &below {
                    self.record(file, Some(fp.clone()), changes);
                }
                let gone: Vec<PathBuf> = self
                    .entries
                    .keys()
                    .filter(|p| p.starts_with(path) && !below.contains_key(*p))
                    .cloned()
                    .collect();
                for file in gone {
                    self.record(&file, None, changes);
                }
            }
            Ok(_) => {
                if scope.classify(path) == PathVerdict::Relevant {
                    let fp = Fingerprint::of(path).ok();
                    self.record(path, fp, changes);
                }
            }
            Err(_) => {
                // Gone: the file itself, or a whole directory.
                let gone: Vec<PathBuf> = self
                    .entries
                    .keys()
                    .filter(|p| p.starts_with(path))
                    .cloned()
                    .collect();
                for file in gone {
                    self.record(&file, None, changes);
                }
            }
        }
    }

    fn record(&mut self, path: &Path, now: Option<Fingerprint>, changes: &mut ChangeSet) {
        let kind = match (self.entries.get(path), &now) {
            (None, None) => None,
            (None, Some(_)) => Some(ChangeKind::Created),
            (Some(_), None) => Some(ChangeKind::Removed),
            (Some(before), Some(after)) if before.same_content(after) => None,
            (Some(_), Some(_)) => Some(ChangeKind::Modified),
        };
        match now {
            Some(fp) => {
                self.entries.insert(path.to_path_buf(), fp);
            }
            None => {
                self.entries.remove(path);
            }
        }
        if let Some(kind) = kind {
            changes.insert(path.to_path_buf(), kind);
        }
    }
}

fn current_files(scope: &WatchScope) -> impl Iterator<Item = (PathBuf, Fingerprint)> + '_ {
    let dirs = scope.recursive_dirs().flat_map(move |d| walk(scope, d));
    let files = scope.files().filter_map(move |f| {
        (f.is_file() && scope.classify(f) == PathVerdict::Relevant)
            .then(|| Fingerprint::of(f).ok().map(|fp| (f.clone(), fp)))
            .flatten()
    });
    dirs.chain(files)
}

fn walk<'a>(
    scope: &'a WatchScope,
    dir: &Path,
) -> impl Iterator<Item = (PathBuf, Fingerprint)> + 'a {
    walkdir::WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_entry(move |e| !e.file_type().is_dir() || scope.may_contain_relevant(e.path()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(move |e| scope.classify(e.path()) == PathVerdict::Relevant)
        .filter_map(|e| {
            let path = e.into_path();
            Fingerprint::of(&path).ok().map(|fp| (path, fp))
        })
}

#[cfg(test)]
mod tests {
    use super::super::scope::tests::sample_project;
    use super::*;
    use std::fs;

    #[test]
    fn initial_scan_covers_sources_and_manifests_only() {
        let (_tmp, scope) = sample_project();
        fs::write(scope.root().join("target/debug/demo"), "bin").unwrap();
        let snapshot = FileSnapshot::scan(&scope);
        // src/main.rs, src/controllers/health.rs, Cargo.toml, Cargo.lock
        assert_eq!(snapshot.len(), 4);
    }

    #[test]
    fn identical_content_is_not_a_change() {
        let (_tmp, scope) = sample_project();
        let mut snapshot = FileSnapshot::scan(&scope);
        let main = scope.root().join("src/main.rs");
        // Rewrite the same bytes (new mtime, same content), like `touch` or
        // an editor saving an unmodified buffer.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&main, "fn main() {}\n").unwrap();
        assert!(snapshot.refresh(&scope, [&main]).is_empty());
    }

    #[test]
    fn content_change_create_and_remove_are_detected() {
        let (_tmp, scope) = sample_project();
        let mut snapshot = FileSnapshot::scan(&scope);
        let root = scope.root().to_path_buf();
        let main = root.join("src/main.rs");
        let new = root.join("src/new.rs");
        let health = root.join("src/controllers/health.rs");

        fs::write(&main, "fn main() { todo!() }\n").unwrap();
        fs::write(&new, "pub fn f() {}\n").unwrap();
        fs::remove_file(&health).unwrap();

        let changes = snapshot.refresh(&scope, [&main, &new, &health, &main]);
        let got: Vec<(String, ChangeKind)> = changes
            .iter()
            .map(|(path, kind)| (scope.display(path), *kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/controllers/health.rs".to_string(), ChangeKind::Removed),
                ("src/main.rs".to_string(), ChangeKind::Modified),
                ("src/new.rs".to_string(), ChangeKind::Created),
            ]
        );
        // A second look at the same paths reports nothing more.
        assert!(snapshot.refresh(&scope, [&main, &new, &health]).is_empty());
    }

    #[test]
    fn directory_moves_are_expanded() {
        let (_tmp, scope) = sample_project();
        let mut snapshot = FileSnapshot::scan(&scope);
        let root = scope.root().to_path_buf();
        fs::rename(root.join("src/controllers"), root.join("src/handlers")).unwrap();
        let changes = snapshot.refresh(
            &scope,
            [&root.join("src/controllers"), &root.join("src/handlers")],
        );
        let got: Vec<(String, ChangeKind)> = changes
            .iter()
            .map(|(path, kind)| (scope.display(path), *kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/controllers/health.rs".to_string(), ChangeKind::Removed),
                ("src/handlers/health.rs".to_string(), ChangeKind::Created),
            ]
        );
    }

    #[test]
    fn ignored_files_never_count() {
        let (_tmp, scope) = sample_project();
        let mut snapshot = FileSnapshot::scan(&scope);
        let swap = scope.root().join("src/.main.rs.swp");
        fs::write(&swap, "x").unwrap();
        assert!(snapshot.refresh(&scope, [&swap]).is_empty());
    }

    #[test]
    fn rescan_finds_everything() {
        let (_tmp, scope) = sample_project();
        let mut snapshot = FileSnapshot::scan(&scope);
        fs::write(scope.root().join("Cargo.lock"), "# changed").unwrap();
        let changes = snapshot.rescan(&scope);
        assert_eq!(changes.len(), 1);
        assert!(snapshot.rescan(&scope).is_empty());
    }
}
