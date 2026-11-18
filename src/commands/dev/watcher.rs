//! `FileWatcher`: receives raw filesystem events, filters them by *event
//! kind* and *path*, and hands candidate paths to the debouncer.
//!
//! The watcher never builds the project nor touches the server.
//!
//! Why the kind matters: on Linux, `notify` 8 subscribes to `IN_OPEN`, so
//! every time `rustc` opens a source file during a build an
//! `Access(Open)` event is emitted for that file. The previous
//! implementation used `notify-debouncer-mini`, which drops the event kind,
//! and only looked at the `.rs` extension — so each build triggered the
//! next one, forever.

use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;

use super::scope::{PathVerdict, WatchRegistration, WatchScope};
use crate::error::{CliError, CliResult};

/// Decision about a single path of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDecision {
    /// Normalized absolute path.
    pub path: PathBuf,
    pub accepted: bool,
    /// Why the path was rejected (`None` when accepted).
    pub reason: Option<String>,
}

/// Result of filtering one filesystem event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEvaluation {
    /// Short label of the event kind, e.g. `Modify(Data)`.
    pub kind: String,
    /// The OS dropped events: the whole scope must be re-checked.
    pub rescan: bool,
    pub decisions: Vec<PathDecision>,
}

impl EventEvaluation {
    pub fn accepted_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.decisions
            .iter()
            .filter(|d| d.accepted)
            .map(|d| &d.path)
    }
}

/// Whether an event kind can represent a change of a build input.
///
/// Accepted: creation, content modification, removal, rename/move, and the
/// ambiguous `Any`/`Other` kinds some backends emit (Windows reports most
/// writes as `Modify(Any)`); ambiguous events are confirmed later by
/// comparing file fingerprints.
///
/// Rejected: every `Access` (open/read/close — emitted by the compiler
/// itself while building) and metadata-only changes (permissions, owner,
/// timestamps, xattrs).
pub fn kind_is_relevant(kind: &EventKind) -> Result<(), &'static str> {
    match kind {
        EventKind::Access(_) => Err("access event (no content change)"),
        EventKind::Modify(ModifyKind::Metadata(_)) => Err("metadata-only change"),
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Name(_))
        | EventKind::Modify(ModifyKind::Any)
        | EventKind::Modify(ModifyKind::Other)
        | EventKind::Any
        | EventKind::Other => Ok(()),
    }
}

/// Short label of an event kind, e.g. `Access(Open)`, `Modify(Name(To))`.
pub fn kind_label(kind: &EventKind) -> String {
    match kind {
        EventKind::Any => "Any".into(),
        EventKind::Other => "Other".into(),
        EventKind::Access(a) => format!(
            "Access({})",
            match a {
                AccessKind::Any => "Any",
                AccessKind::Read => "Read",
                AccessKind::Open(_) => "Open",
                AccessKind::Close(_) => "Close",
                AccessKind::Other => "Other",
            }
        ),
        EventKind::Create(c) => format!(
            "Create({})",
            match c {
                CreateKind::Any => "Any",
                CreateKind::File => "File",
                CreateKind::Folder => "Folder",
                CreateKind::Other => "Other",
            }
        ),
        EventKind::Remove(r) => format!(
            "Remove({})",
            match r {
                RemoveKind::Any => "Any",
                RemoveKind::File => "File",
                RemoveKind::Folder => "Folder",
                RemoveKind::Other => "Other",
            }
        ),
        EventKind::Modify(m) => match m {
            ModifyKind::Any => "Modify(Any)".into(),
            ModifyKind::Other => "Modify(Other)".into(),
            ModifyKind::Data(_) => "Modify(Data)".into(),
            ModifyKind::Metadata(_) => "Modify(Metadata)".into(),
            ModifyKind::Name(mode) => format!(
                "Modify(Name({}))",
                match mode {
                    RenameMode::Any => "Any",
                    RenameMode::To => "To",
                    RenameMode::From => "From",
                    RenameMode::Both => "Both",
                    RenameMode::Other => "Other",
                }
            ),
        },
    }
}

/// Filter one event: check its kind, then every path it carries (rename
/// events carry both the old and the new path; some events carry none).
pub fn evaluate(event: &Event, scope: &WatchScope) -> EventEvaluation {
    let kind = kind_label(&event.kind);
    let rescan = event.need_rescan();
    let kind_verdict = kind_is_relevant(&event.kind);

    let decisions = event
        .paths
        .iter()
        .map(|raw| {
            let path = scope.normalize(raw);
            let (accepted, reason) = match kind_verdict {
                Err(reason) => (false, Some(reason.to_string())),
                Ok(()) => match scope.classify(&path) {
                    PathVerdict::Relevant => (true, None),
                    PathVerdict::Ignored(reason) => (false, Some(reason.to_string())),
                },
            };
            PathDecision {
                path,
                accepted,
                reason,
            }
        })
        .collect();

    EventEvaluation {
        kind,
        rescan,
        decisions,
    }
}

/// Thin owner of the OS watcher. Dropping it stops the watch.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    registrations: Vec<WatchRegistration>,
}

impl FileWatcher {
    /// Register every directory of the scope. Events are delivered to
    /// `handler` on the watcher's thread.
    pub fn start<F>(scope: &WatchScope, handler: F) -> CliResult<Self>
    where
        F: Fn(notify::Result<Event>) + Send + 'static,
    {
        let mut watcher = notify::recommended_watcher(handler)
            .map_err(|e| CliError::Watch(format!("cannot start file watcher: {e}")))?;
        let registrations = scope.registrations();
        if registrations.is_empty() {
            return Err(CliError::Watch(
                "nothing to watch: no src/ directory or manifest found".to_string(),
            ));
        }
        for reg in &registrations {
            let mode = if reg.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            watcher.watch(&reg.path, mode).map_err(|e| {
                CliError::Watch(format!("cannot watch {}: {e}", reg.path.display()))
            })?;
        }
        Ok(Self {
            _watcher: watcher,
            registrations,
        })
    }

    pub fn registrations(&self) -> &[WatchRegistration] {
        &self.registrations
    }
}

#[cfg(test)]
mod tests {
    use super::super::scope::tests::sample_project;
    use super::*;
    use notify::event::{AccessMode, DataChange, MetadataKind};

    fn event(kind: EventKind, paths: &[PathBuf]) -> Event {
        paths
            .iter()
            .fold(Event::new(kind), |e, p| e.add_path(p.clone()))
    }

    #[test]
    fn access_events_are_rejected() {
        let (_tmp, scope) = sample_project();
        let main = scope.root().join("src/main.rs");
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            let eval = evaluate(&event(kind, std::slice::from_ref(&main)), &scope);
            assert!(!eval.decisions[0].accepted, "{}", eval.kind);
            assert_eq!(eval.accepted_paths().count(), 0);
        }
    }

    #[test]
    fn metadata_events_are_rejected() {
        let (_tmp, scope) = sample_project();
        let main = scope.root().join("src/main.rs");
        for meta in [
            MetadataKind::Any,
            MetadataKind::Permissions,
            MetadataKind::AccessTime,
            MetadataKind::WriteTime,
        ] {
            let eval = evaluate(
                &event(
                    EventKind::Modify(ModifyKind::Metadata(meta)),
                    std::slice::from_ref(&main),
                ),
                &scope,
            );
            assert_eq!(eval.kind, "Modify(Metadata)");
            assert!(!eval.decisions[0].accepted);
            assert_eq!(
                eval.decisions[0].reason.as_deref(),
                Some("metadata-only change")
            );
        }
    }

    #[test]
    fn data_create_remove_on_sources_are_accepted() {
        let (_tmp, scope) = sample_project();
        let main = scope.root().join("src/main.rs");
        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Any,
        ] {
            let eval = evaluate(&event(kind, std::slice::from_ref(&main)), &scope);
            assert!(eval.decisions[0].accepted, "{}", eval.kind);
        }
    }

    #[test]
    fn rename_checks_every_path() {
        let (_tmp, scope) = sample_project();
        let root = scope.root();
        // vim-style save: main.rs -> main.rs~ ; both paths are inspected.
        let eval = evaluate(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[root.join("src/main.rs"), root.join("src/main.rs~")],
            ),
            &scope,
        );
        assert_eq!(eval.kind, "Modify(Name(Both))");
        assert_eq!(eval.decisions.len(), 2);
        assert!(eval.decisions[0].accepted);
        assert!(!eval.decisions[1].accepted);
        assert_eq!(
            eval.decisions[1].reason.as_deref(),
            Some("editor temporary file")
        );
    }

    #[test]
    fn ignored_locations_are_rejected_even_for_writes() {
        let (_tmp, scope) = sample_project();
        let root = scope.root();
        let write = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        for path in ["target/debug/demo", ".git/index", "README.md"] {
            let eval = evaluate(&event(write, &[root.join(path)]), &scope);
            assert!(!eval.decisions[0].accepted, "{path}");
        }
    }

    #[test]
    fn events_without_paths_are_harmless() {
        let (_tmp, scope) = sample_project();
        let eval = evaluate(&Event::new(EventKind::Any), &scope);
        assert!(eval.decisions.is_empty());
        assert!(!eval.rescan);

        let rescan = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(evaluate(&rescan, &scope).rescan);
    }

    #[test]
    fn real_watcher_ignores_reads() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (_tmp, scope) = sample_project();
        let (tx, rx) = mpsc::channel();
        let _watcher = FileWatcher::start(&scope, move |res| {
            let _ = tx.send(res);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // Reading a source file (what rustc does) must produce nothing accepted.
        let main = scope.root().join("src/main.rs");
        let _ = std::fs::read_to_string(&main).unwrap();
        let mut accepted = 0;
        while let Ok(Ok(ev)) = rx.recv_timeout(Duration::from_millis(300)) {
            accepted += evaluate(&ev, &scope).accepted_paths().count();
        }
        assert_eq!(accepted, 0, "reading a file must not be a change");

        // Writing it must.
        std::fs::write(&main, "fn main() { println!(); }\n").unwrap();
        let mut accepted = 0;
        while let Ok(Ok(ev)) = rx.recv_timeout(Duration::from_millis(500)) {
            accepted += evaluate(&ev, &scope).accepted_paths().count();
        }
        assert!(accepted > 0, "writing a file must be seen");
    }
}
