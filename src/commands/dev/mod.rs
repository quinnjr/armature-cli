//! `armature dev`: development server with file watching.
//!
//! The built-in watcher is split into independent components:
//!
//! | Component          | Role                                                        |
//! |--------------------|-------------------------------------------------------------|
//! | [`scope`]          | which paths are build inputs, which are ignored             |
//! | [`watcher`]        | OS events → filtered by kind and path → candidate paths     |
//! | [`debounce`]       | groups/de-duplicates candidates over a stability window     |
//! | [`snapshot`]       | confirms that a candidate's content really changed          |
//! | [`coordinator`]    | one build at a time, pending changes, restart sequencing    |
//! | [`cargo_build`]    | `cargo build` (JSON messages give the executable path)      |
//! | [`server`]         | runs the executable itself, stops it with its descendants  |
//! | [`logger`]         | concise output, `--verbose` explains every decision         |
//!
//! Everything runs on one thread fed by a single channel (filesystem events,
//! "build output closed", Ctrl+C), so there is no shared mutable state and
//! no build can ever be started from the watcher callback.

mod cargo_build;
mod config;
mod coordinator;
mod debounce;
mod logger;
mod process;
mod scope;
mod server;
mod snapshot;
mod watcher;

use colored::Colorize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::error::{CliError, CliResult};
use crate::generators::{find_project_root, has_cargo_watch};

use cargo_build::{BuildProcess, DevArgsSplit, Executable, select_executable};
use config::{CliOverrides, DevConfig};
use coordinator::{Action, BuildCoordinator, BuildOutcome, BuildState};
use debounce::ChangeDebouncer;
use logger::DevLogger;
use scope::{ProjectLayout, WatchScope};
use server::{ServerProcess, ServerSpec};
use snapshot::{ChangeSet, FileSnapshot};
use watcher::FileWatcher;

/// Upper bound on how long the loop sleeps (server liveness checks).
const TICK: Duration = Duration::from_millis(200);

/// Options of `armature dev`.
#[derive(Debug, Clone)]
pub struct DevOptions {
    pub port: u16,
    pub host: String,
    pub cargo_args: Vec<String>,
    pub verbose: bool,
    pub debounce_ms: Option<u64>,
    /// Delegate to cargo-watch when it is installed.
    pub use_cargo_watch: bool,
}

/// Run development server with file watching.
pub async fn run(options: DevOptions) -> CliResult<()> {
    let project_root = find_project_root()?;

    if options.use_cargo_watch && has_cargo_watch() {
        print_banner();
        println!("  {} Using cargo-watch for file watching", "→".green());
        return run_with_cargo_watch(
            options.port,
            &options.host,
            &options.cargo_args,
            &project_root,
        );
    }

    let overrides = CliOverrides {
        verbose: options.verbose,
        debounce_ms: options.debounce_ms,
    };
    let config = DevConfig::load(&project_root, &overrides)?;
    // The loop blocks on a channel; keep it off the async executor.
    tokio::task::block_in_place(|| run_builtin(&project_root, &options, config))
}

fn print_banner() {
    DevLogger::new(false).banner();
}

/// Run using cargo-watch, when it is installed.
fn run_with_cargo_watch(
    port: u16,
    host: &str,
    cargo_args: &[String],
    project_root: &Path,
) -> CliResult<()> {
    // cargo-watch takes the command to run as a single `-x` string, so extra
    // cargo arguments have to be folded into it rather than appended to the
    // cargo-watch invocation itself.
    let exec = if cargo_args.is_empty() {
        "run".to_string()
    } else {
        format!("run {}", cargo_args.join(" "))
    };

    let args = vec![
        "watch".to_string(),
        "-x".to_string(),
        exec,
        "-w".to_string(),
        "src".to_string(),
        "-c".to_string(), // Clear screen
        "-q".to_string(), // Quiet cargo-watch output
    ];

    println!(
        "  {} Starting server on http://{}:{}",
        "→".green(),
        host,
        port
    );
    println!("  {} Watching for changes in src/", "→".green());
    println!("  {} Press {} to stop", "→".dimmed(), "Ctrl+C".yellow());
    println!();

    let status = Command::new("cargo")
        .args(&args)
        .current_dir(project_root)
        .env("PORT", port.to_string())
        .env("HOST", host)
        .status()?;

    if !status.success() {
        return Err(CliError::Command(
            "cargo-watch exited with error".to_string(),
        ));
    }

    Ok(())
}

/// Messages processed by the dev loop.
enum DevEvent {
    Fs(notify::Result<notify::Event>),
    BuildOutputClosed(u64),
    Interrupt,
}

fn run_builtin(project_root: &Path, options: &DevOptions, config: DevConfig) -> CliResult<()> {
    let logger = DevLogger::new(config.verbose);
    logger.banner();
    println!("  {} Using built-in file watcher", "→".green());

    let (layout, fallback) = ProjectLayout::discover(project_root);
    if let Some(reason) = fallback {
        logger.layout_fallback(&reason);
    }

    let (tx, rx) = mpsc::channel();
    let interrupt_tx = tx.clone();
    ctrlc::set_handler(move || {
        let _ = interrupt_tx.send(DevEvent::Interrupt);
    })
    .map_err(|e| CliError::Command(format!("Failed to set Ctrl+C handler: {}", e)))?;

    let scope = WatchScope::new(&layout, &config);
    let mut session = DevSession {
        args: DevArgsSplit::parse(&options.cargo_args),
        env: vec![
            ("PORT".to_string(), options.port.to_string()),
            ("HOST".to_string(), options.host.clone()),
            (
                "CARGO_MANIFEST_DIR".to_string(),
                layout.project_root.to_string_lossy().into_owned(),
            ),
        ],
        snapshot: FileSnapshot::default(),
        debouncer: ChangeDebouncer::new(config.debounce),
        coordinator: BuildCoordinator::new(),
        watcher: None,
        build: None,
        server: None,
        ready: None,
        building: ChangeSet::new(),
        last_seen: BTreeMap::new(),
        tx,
        layout,
        scope,
        config,
        logger,
    };
    let result = session.run(rx, &options.host, options.port);
    session.shutdown();
    result
}

struct DevSession {
    config: DevConfig,
    logger: DevLogger,
    layout: ProjectLayout,
    scope: WatchScope,
    snapshot: FileSnapshot,
    debouncer: ChangeDebouncer,
    coordinator: BuildCoordinator,
    watcher: Option<FileWatcher>,
    build: Option<BuildProcess>,
    server: Option<ServerProcess>,
    /// Executable of the last successful build, waiting to be started.
    ready: Option<Executable>,
    /// Changes the running build was started for.
    building: ChangeSet,
    /// When each changed path was last confirmed.
    last_seen: BTreeMap<PathBuf, Instant>,
    args: DevArgsSplit,
    env: Vec<(String, String)>,
    tx: Sender<DevEvent>,
}

impl DevSession {
    fn run(&mut self, rx: Receiver<DevEvent>, host: &str, port: u16) -> CliResult<()> {
        if self.config.watch_enabled {
            // Watch *before* the first build, so edits made while it runs
            // are not missed.
            self.start_watching()?;
            self.logger
                .watching(&self.scope.summary(), self.config.debounce);
        } else {
            self.logger.watching_disabled();
        }
        self.logger.server_address(host, port);

        let actions = self.coordinator.start();
        self.execute(actions);

        loop {
            let now = Instant::now();
            let timeout = self
                .debouncer
                .deadline()
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(TICK)
                .min(TICK);
            match rx.recv_timeout(timeout) {
                Ok(DevEvent::Fs(Ok(event))) => self.on_fs_event(&event),
                Ok(DevEvent::Fs(Err(error))) => self.logger.watch_error(&error),
                Ok(DevEvent::BuildOutputClosed(id)) => self.on_build_finished(id),
                Ok(DevEvent::Interrupt) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.flush_debouncer();
            self.poll_server();
        }
        Ok(())
    }

    fn start_watching(&mut self) -> CliResult<()> {
        // Drop the previous watcher first (re-scoping).
        self.watcher = None;
        let tx = self.tx.clone();
        let watcher = FileWatcher::start(&self.scope, move |res| {
            let _ = tx.send(DevEvent::Fs(res));
        })?;
        self.logger
            .registrations(&self.scope, watcher.registrations());
        self.watcher = Some(watcher);
        self.snapshot = FileSnapshot::scan(&self.scope);
        Ok(())
    }

    fn on_fs_event(&mut self, event: &notify::Event) {
        let eval = watcher::evaluate(event, &self.scope);
        self.logger.event(&self.scope, &eval);
        let now = Instant::now();
        if eval.rescan {
            self.debouncer.push_rescan(now);
        }
        for path in eval.accepted_paths() {
            self.debouncer.push(path.clone(), now);
        }
    }

    fn flush_debouncer(&mut self) {
        let Some(batch) = self.debouncer.poll(Instant::now()) else {
            return;
        };
        self.logger.batch(
            batch.paths.len(),
            batch.events,
            batch.stable_for,
            batch.rescan,
        );

        let mut changes = if batch.rescan {
            self.snapshot.rescan(&self.scope)
        } else {
            ChangeSet::new()
        };
        changes.extend(self.snapshot.refresh(&self.scope, &batch.paths));
        for path in &batch.paths {
            if !changes.keys().any(|c| c.starts_with(path)) {
                self.logger.unchanged(&self.scope, path);
            }
        }
        if changes.is_empty() {
            return;
        }
        if changes.keys().any(|p| WatchScope::is_manifest(p)) {
            self.refresh_layout();
        }

        let now = Instant::now();
        for path in changes.keys() {
            self.last_seen.insert(path.clone(), now);
        }
        let actions = self.coordinator.on_changes(changes.clone());
        let queued_behind = actions.iter().find_map(|a| match a {
            Action::Queued { behind, .. } => Some(*behind),
            _ => None,
        });
        let lockfile = self.layout.workspace_root.join("Cargo.lock");
        match queued_behind {
            // Cargo itself writes the lockfile while building; whether this
            // is a real change is decided when the build ends.
            Some(id) if self.coordinator.pending().keys().all(|p| *p == lockfile) => {
                self.logger.lockfile_pending(&self.scope, &lockfile, id);
            }
            Some(_) => {
                let pending = self.coordinator.pending().clone();
                self.logger.changes(&self.scope, &pending, queued_behind);
            }
            None => self.logger.changes(&self.scope, &changes, None),
        }
        self.execute(actions);
    }

    /// A manifest changed: workspace members or path dependencies may have
    /// been added or removed, so the scope is recomputed.
    fn refresh_layout(&mut self) {
        let (layout, _) = ProjectLayout::discover(&self.layout.project_root);
        if layout == self.layout {
            return;
        }
        let scope = WatchScope::new(&layout, &self.config);
        self.layout = layout;
        if scope == self.scope {
            return;
        }
        self.scope = scope;
        if !self.config.watch_enabled {
            return;
        }
        match self.start_watching() {
            Ok(()) => self.logger.layout_changed(&self.scope.summary()),
            Err(error) => eprintln!("  {} {error}", "⚠".yellow()),
        }
    }

    fn execute(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::StartBuild { id, changes } => self.start_build(id, &changes),
                Action::RestartServer { .. } => {
                    self.restart_server();
                    let next = self.coordinator.on_server_restarted();
                    let idle = next.is_empty();
                    self.execute(next);
                    if idle {
                        self.logger.waiting();
                    }
                }
                Action::ReportFailure { .. } => {
                    if self.coordinator.state() == BuildState::Idle {
                        self.logger.waiting();
                    }
                }
                Action::Queued { .. } => {}
            }
        }
    }

    fn start_build(&mut self, id: u64, changes: &ChangeSet) {
        let reason = if changes.is_empty() {
            "initial".to_string()
        } else {
            changes
                .keys()
                .map(|p| self.scope.display(p))
                .collect::<Vec<_>>()
                .join(",")
        };
        let tx = self.tx.clone();
        match BuildProcess::spawn(id, &self.layout.project_root, &self.args, move || {
            let _ = tx.send(DevEvent::BuildOutputClosed(id));
        }) {
            Ok(build) => {
                self.logger.build_started(id, build.pid(), &reason);
                self.build = Some(build);
                self.building = changes.clone();
            }
            Err(error) => {
                self.logger.build_spawn_failed(&error);
                let actions = self
                    .coordinator
                    .on_build_finished(id, BuildOutcome::Failure);
                self.execute(actions);
            }
        }
    }

    fn on_build_finished(&mut self, id: u64) {
        if self.build.as_ref().map(BuildProcess::id) != Some(id) {
            return;
        }
        let Some(build) = self.build.take() else {
            return;
        };
        let result = build.finish();
        self.absorb_lockfile();

        let outcome = if result.success() {
            match select_executable(
                &result.executables,
                &self.args,
                self.layout.root_package_id.as_deref(),
                self.layout.default_run.as_deref(),
            ) {
                Ok(executable) => {
                    self.logger.build_succeeded(id, result.duration);
                    self.ready = Some(executable);
                    BuildOutcome::Success
                }
                Err(message) => {
                    self.logger.build_succeeded(id, result.duration);
                    self.logger.no_executable(&message);
                    BuildOutcome::Failure
                }
            }
        } else {
            let running = self.server.as_mut().is_some_and(ServerProcess::is_running);
            self.logger
                .build_failed(id, result.duration, result.status, running);
            BuildOutcome::Failure
        };

        let actions = self.coordinator.on_build_finished(id, outcome);
        self.execute(actions);
    }

    /// `cargo build` writes `Cargo.lock` itself (created on the first build,
    /// updated after a manifest change). Such a write is an output of the
    /// build, not a new change: re-baseline it and drop it from the pending
    /// changes, otherwise every lockfile update would cause one more build.
    fn absorb_lockfile(&mut self) {
        let lockfile = self.layout.workspace_root.join("Cargo.lock");
        let absorbed = self.snapshot.refresh(&self.scope, [&lockfile]);
        if self.coordinator.pending().contains_key(&lockfile) || !absorbed.is_empty() {
            self.logger.lockfile_absorbed(&self.scope, &lockfile);
        }
        self.coordinator.discard_pending(&lockfile);
    }

    /// The binary did not change and the running server was started after
    /// every change of this build was seen (so it already runs with them,
    /// including files it reads at runtime): restarting would be useless.
    fn server_is_up_to_date(&mut self, executable: &Executable) -> bool {
        if !executable.fresh || self.building.is_empty() {
            return false;
        }
        let Some(server) = self.server.as_mut() else {
            return false;
        };
        if !server.is_running() {
            return false;
        }
        let started = server.started_at();
        self.building
            .keys()
            .all(|p| self.last_seen.get(p).is_some_and(|seen| *seen < started))
    }

    /// Replace the server: the old one is fully stopped (descendants
    /// included) before the new one starts, so the port is free.
    fn restart_server(&mut self) {
        let Some(executable) = self.ready.take() else {
            return;
        };
        if self.server_is_up_to_date(&executable) {
            self.logger.restart_skipped();
            return;
        }
        let restarted = self.server.is_some();
        self.stop_server();

        let spec = ServerSpec {
            executable: executable.path.clone(),
            args: self.args.app.clone(),
            cwd: self.layout.project_root.clone(),
            env: self.env.clone(),
            scratch_dir: self.layout.target_dir.join("armature-dev"),
        };
        match ServerProcess::start(&spec) {
            Ok(server) => {
                self.logger
                    .server_started(server.pid(), &executable.path, restarted);
                self.server = Some(server);
            }
            Err(error) => self.logger.server_start_failed(&error),
        }
    }

    fn stop_server(&mut self) {
        if let Some(server) = self.server.take() {
            let pid = server.pid();
            self.logger.server_stopping(pid);
            let report = server.stop(
                self.config.graceful_shutdown_timeout,
                self.config.force_kill_timeout,
            );
            self.logger.server_stopped(pid, &report);
        }
    }

    fn poll_server(&mut self) {
        if let Some(server) = self.server.as_mut()
            && let Some(status) = server.poll_exit()
        {
            self.logger
                .server_exited(server.pid(), status, server.uptime());
        }
    }

    /// Stop watching, refuse new builds, cancel the running build, stop the
    /// server and its descendants, and wait for all of them.
    fn shutdown(&mut self) {
        self.coordinator.shutdown();
        self.watcher = None;
        self.logger.shutting_down();
        if let Some(build) = self.build.take() {
            let id = build.id();
            let report = build.cancel();
            self.logger.build_cancelled(id, &report);
        }
        self.stop_server();
        self.logger.stopped();
    }
}
