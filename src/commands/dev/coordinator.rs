//! `BuildCoordinator`: serializes builds and server restarts.
//!
//! Pure state machine, independent of the watching library and of process
//! management, so every rule is unit-tested:
//!
//! ```text
//!             changes                build ok                 restarted
//!   Idle ───────────────▶ Building ───────────▶ Restarting ─────────────▶ Idle
//!    ▲                      │  ▲                     │
//!    │       build failed   │  │ pending changes     │ pending changes
//!    └──────────────────────┘  └─────────────────────┘
//!
//!   changes while Building/Restarting ─▶ merged into `pending` (never lost,
//!   never a second concurrent build); when the current step ends, ONE new
//!   build is started for all of them.
//! ```
//!
//! A failed build is reported once and is never retried on its own: only a
//! new change starts another build. The debouncing phase lives in
//! `ChangeDebouncer`, which feeds this coordinator with stabilized batches.

use super::snapshot::{ChangeKind, ChangeSet};

/// Current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildState {
    Idle,
    Building { id: u64 },
    Restarting { id: u64 },
    ShuttingDown,
}

/// Result of a build, as far as the coordinator cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildOutcome {
    Success,
    Failure,
}

/// What the orchestrator must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start build `id` for these changes (empty for the initial build).
    StartBuild { id: u64, changes: ChangeSet },
    /// Changes were queued behind the running build `behind`.
    Queued { behind: u64, pending: usize },
    /// Build `id` succeeded: stop the old server, start the new one, then
    /// call [`BuildCoordinator::on_server_restarted`].
    RestartServer { id: u64 },
    /// Build `id` failed: report it, keep the old server, keep watching.
    ReportFailure { id: u64 },
}

#[derive(Debug)]
pub struct BuildCoordinator {
    state: BuildState,
    pending: ChangeSet,
    next_id: u64,
}

impl Default for BuildCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildCoordinator {
    pub fn new() -> Self {
        Self {
            state: BuildState::Idle,
            pending: ChangeSet::new(),
            next_id: 1,
        }
    }

    pub fn state(&self) -> BuildState {
        self.state
    }

    /// Number of builds started so far.
    pub fn builds_started(&self) -> u64 {
        self.next_id - 1
    }

    pub fn pending(&self) -> &ChangeSet {
        &self.pending
    }

    /// Initial build when the command starts.
    pub fn start(&mut self) -> Vec<Action> {
        match self.state {
            BuildState::Idle => vec![self.begin_build(ChangeSet::new())],
            _ => Vec::new(),
        }
    }

    /// A stabilized, confirmed set of changes arrived.
    pub fn on_changes(&mut self, changes: ChangeSet) -> Vec<Action> {
        if changes.is_empty() {
            return Vec::new();
        }
        match self.state {
            BuildState::Idle => vec![self.begin_build(changes)],
            BuildState::Building { id } | BuildState::Restarting { id } => {
                merge(&mut self.pending, changes);
                vec![Action::Queued {
                    behind: id,
                    pending: self.pending.len(),
                }]
            }
            BuildState::ShuttingDown => Vec::new(),
        }
    }

    /// The build `id` finished. Stale or unexpected notifications are ignored.
    pub fn on_build_finished(&mut self, id: u64, outcome: BuildOutcome) -> Vec<Action> {
        if self.state != (BuildState::Building { id }) {
            return Vec::new();
        }
        match outcome {
            BuildOutcome::Success => {
                self.state = BuildState::Restarting { id };
                vec![Action::RestartServer { id }]
            }
            BuildOutcome::Failure => {
                let mut actions = vec![Action::ReportFailure { id }];
                actions.extend(self.continue_with_pending());
                actions
            }
        }
    }

    /// The server restart that followed a successful build is done
    /// (whether or not the new server came up).
    pub fn on_server_restarted(&mut self) -> Vec<Action> {
        match self.state {
            BuildState::Restarting { .. } => self.continue_with_pending(),
            _ => Vec::new(),
        }
    }

    /// Forget a pending change that turned out to be produced by the build
    /// itself (Cargo writes `Cargo.lock`).
    pub fn discard_pending(&mut self, path: &std::path::Path) {
        self.pending.remove(path);
    }

    /// Stop accepting work. Pending changes are dropped.
    pub fn shutdown(&mut self) {
        self.state = BuildState::ShuttingDown;
        self.pending.clear();
    }

    fn continue_with_pending(&mut self) -> Vec<Action> {
        self.state = BuildState::Idle;
        if self.pending.is_empty() {
            Vec::new()
        } else {
            let changes = std::mem::take(&mut self.pending);
            vec![self.begin_build(changes)]
        }
    }

    fn begin_build(&mut self, changes: ChangeSet) -> Action {
        let id = self.next_id;
        self.next_id += 1;
        self.state = BuildState::Building { id };
        Action::StartBuild { id, changes }
    }
}

/// Merge newer changes into older ones. A file created then removed before
/// anything was built is still reported (as removed): the build decides.
fn merge(into: &mut ChangeSet, newer: ChangeSet) {
    for (path, kind) in newer {
        let merged = match (into.get(&path), kind) {
            (Some(ChangeKind::Created), ChangeKind::Modified) => ChangeKind::Created,
            (Some(ChangeKind::Removed), ChangeKind::Created) => ChangeKind::Modified,
            (_, kind) => kind,
        };
        into.insert(path, merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn changes(paths: &[&str]) -> ChangeSet {
        paths
            .iter()
            .map(|p| (PathBuf::from(p), ChangeKind::Modified))
            .collect()
    }

    fn started(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::StartBuild { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn initial_build_then_idle() {
        let mut c = BuildCoordinator::new();
        assert_eq!(started(&c.start()), vec![1]);
        assert_eq!(c.state(), BuildState::Building { id: 1 });
        assert_eq!(
            c.on_build_finished(1, BuildOutcome::Success),
            vec![Action::RestartServer { id: 1 }]
        );
        assert!(c.on_server_restarted().is_empty());
        assert_eq!(c.state(), BuildState::Idle);
        assert_eq!(c.builds_started(), 1);
    }

    #[test]
    fn empty_change_sets_do_nothing() {
        let mut c = BuildCoordinator::new();
        assert!(c.on_changes(ChangeSet::new()).is_empty());
        assert_eq!(c.state(), BuildState::Idle);
        assert_eq!(c.builds_started(), 0);
    }

    #[test]
    fn never_two_builds_at_once() {
        let mut c = BuildCoordinator::new();
        c.start();
        for i in 0..10 {
            let actions = c.on_changes(changes(&[&format!("src/f{i}.rs")]));
            assert!(started(&actions).is_empty(), "no concurrent build");
            assert!(matches!(actions[0], Action::Queued { behind: 1, .. }));
        }
        assert_eq!(c.builds_started(), 1);
        assert_eq!(c.pending().len(), 10);
    }

    #[test]
    fn changes_during_build_trigger_exactly_one_follow_up_build() {
        let mut c = BuildCoordinator::new();
        c.start();
        c.on_changes(changes(&["src/a.rs"]));
        c.on_changes(changes(&["src/b.rs", "src/a.rs"]));
        assert_eq!(
            c.on_build_finished(1, BuildOutcome::Success),
            vec![Action::RestartServer { id: 1 }]
        );
        let next = c.on_server_restarted();
        match next.as_slice() {
            [Action::StartBuild { id: 2, changes }] => {
                assert_eq!(changes.len(), 2, "merged, nothing lost");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(c.pending().is_empty());
        c.on_build_finished(2, BuildOutcome::Success);
        assert!(c.on_server_restarted().is_empty());
        assert_eq!(c.state(), BuildState::Idle);
        assert_eq!(c.builds_started(), 2);
    }

    #[test]
    fn changes_during_restart_are_kept() {
        let mut c = BuildCoordinator::new();
        c.start();
        c.on_build_finished(1, BuildOutcome::Success);
        assert!(matches!(
            c.on_changes(changes(&["src/a.rs"]))[0],
            Action::Queued {
                behind: 1,
                pending: 1
            }
        ));
        assert_eq!(started(&c.on_server_restarted()), vec![2]);
    }

    #[test]
    fn failed_build_is_not_retried_without_new_changes() {
        let mut c = BuildCoordinator::new();
        c.start();
        assert_eq!(
            c.on_build_finished(1, BuildOutcome::Failure),
            vec![Action::ReportFailure { id: 1 }]
        );
        assert_eq!(c.state(), BuildState::Idle);
        // Nothing happens by itself…
        assert!(c.on_server_restarted().is_empty());
        assert_eq!(c.builds_started(), 1);
        // …until a new change arrives.
        assert_eq!(started(&c.on_changes(changes(&["src/main.rs"]))), vec![2]);
    }

    #[test]
    fn failed_build_with_pending_changes_rebuilds_once() {
        let mut c = BuildCoordinator::new();
        c.start();
        c.on_changes(changes(&["src/fix.rs"]));
        let actions = c.on_build_finished(1, BuildOutcome::Failure);
        assert_eq!(actions[0], Action::ReportFailure { id: 1 });
        assert_eq!(started(&actions), vec![2]);
    }

    #[test]
    fn build_outputs_can_be_discarded_from_pending() {
        let mut c = BuildCoordinator::new();
        c.start();
        c.on_changes(changes(&["Cargo.lock"]));
        c.discard_pending(std::path::Path::new("Cargo.lock"));
        c.on_build_finished(1, BuildOutcome::Success);
        assert!(c.on_server_restarted().is_empty(), "no follow-up build");
        assert_eq!(c.builds_started(), 1);
    }

    #[test]
    fn stale_build_notifications_are_ignored() {
        let mut c = BuildCoordinator::new();
        c.start();
        assert!(c.on_build_finished(7, BuildOutcome::Success).is_empty());
        assert_eq!(c.state(), BuildState::Building { id: 1 });
    }

    #[test]
    fn shutdown_stops_everything() {
        let mut c = BuildCoordinator::new();
        c.start();
        c.on_changes(changes(&["src/a.rs"]));
        c.shutdown();
        assert!(c.on_changes(changes(&["src/b.rs"])).is_empty());
        assert!(c.on_build_finished(1, BuildOutcome::Success).is_empty());
        assert!(c.on_server_restarted().is_empty());
        assert_eq!(c.state(), BuildState::ShuttingDown);
    }

    #[test]
    fn merge_keeps_the_net_effect() {
        let mut set = ChangeSet::new();
        set.insert("a".into(), ChangeKind::Created);
        set.insert("b".into(), ChangeKind::Removed);
        let mut newer = ChangeSet::new();
        newer.insert("a".into(), ChangeKind::Modified);
        newer.insert("b".into(), ChangeKind::Created);
        merge(&mut set, newer);
        assert_eq!(set[&PathBuf::from("a")], ChangeKind::Created);
        assert_eq!(set[&PathBuf::from("b")], ChangeKind::Modified);
    }
}
