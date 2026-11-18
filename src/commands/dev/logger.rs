//! `DevLogger`: concise output by default, one line per decision with
//! `--verbose` (event received, accepted/ignored and why, batches, builds,
//! process stops and starts), so that every rebuild can be explained.

use colored::Colorize;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use super::process::StopReport;
use super::scope::WatchScope;
use super::snapshot::ChangeSet;
use super::watcher::EventEvaluation;

/// Maximum number of changed files listed in normal mode.
const MAX_LISTED: usize = 8;

pub struct DevLogger {
    verbose: bool,
}

impl DevLogger {
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    fn diag(&self, component: &str, message: std::fmt::Arguments<'_>) {
        if self.verbose {
            println!("  {}", format!("[{component}] {message}").dimmed());
        }
    }

    pub fn banner(&self) {
        println!();
        println!(
            "  {}",
            "🚀 Armature Development Server".bright_cyan().bold()
        );
        println!();
    }

    pub fn watching(&self, entries: &[String], debounce: Duration) {
        println!("  {} Watching: {}", "→".green(), entries.join(", "));
        self.diag(
            "watcher",
            format_args!("debounce={}ms", debounce.as_millis()),
        );
    }

    pub fn watching_disabled(&self) {
        println!(
            "  {} File watching disabled (dev.watcher.enabled = false)",
            "→".yellow()
        );
    }

    pub fn registrations(&self, scope: &WatchScope, regs: &[super::scope::WatchRegistration]) {
        for reg in regs {
            self.diag(
                "watcher",
                format_args!(
                    "watch path={} recursive={}",
                    scope.display(&reg.path),
                    reg.recursive
                ),
            );
        }
    }

    pub fn layout_fallback(&self, reason: &str) {
        println!(
            "  {} Using the default project layout ({reason})",
            "⚠".yellow()
        );
    }

    pub fn server_address(&self, host: &str, port: u16) {
        println!("  {} Server: http://{}:{}", "→".green(), host, port);
        println!("  {} Press {} to stop", "→".dimmed(), "Ctrl+C".yellow());
        println!();
    }

    pub fn waiting(&self) {
        println!("  {} Waiting for changes...", "→".dimmed());
    }

    pub fn event(&self, scope: &WatchScope, eval: &EventEvaluation) {
        if !self.verbose {
            return;
        }
        if eval.rescan {
            self.diag(
                "watcher",
                format_args!("event={} rescan=true (events were dropped)", eval.kind),
            );
        }
        if eval.decisions.is_empty() && !eval.rescan {
            self.diag(
                "watcher",
                format_args!(
                    "event={} path=<none> accepted=false reason=no path",
                    eval.kind
                ),
            );
        }
        for d in &eval.decisions {
            match &d.reason {
                None => self.diag(
                    "watcher",
                    format_args!(
                        "event={} path={} accepted=true",
                        eval.kind,
                        scope.display(&d.path)
                    ),
                ),
                Some(reason) => self.diag(
                    "watcher",
                    format_args!(
                        "event={} path={} accepted=false reason={}",
                        eval.kind,
                        scope.display(&d.path),
                        reason
                    ),
                ),
            }
        }
    }

    pub fn watch_error(&self, error: &notify::Error) {
        eprintln!("  {} Watch error: {}", "⚠".yellow(), error);
    }

    pub fn batch(&self, size: usize, events: usize, stable_for: Duration, rescan: bool) {
        self.diag(
            "debounce",
            format_args!(
                "batch_size={size} events={events} stable_for={}ms rescan={rescan}",
                stable_for.as_millis()
            ),
        );
    }

    pub fn unchanged(&self, scope: &WatchScope, path: &Path) {
        self.diag(
            "validate",
            format_args!("path={} changed=false (same content)", scope.display(path)),
        );
    }

    pub fn changes(&self, scope: &WatchScope, changes: &ChangeSet, queued_behind: Option<u64>) {
        let n = changes.len();
        let noun = if n == 1 { "file" } else { "files" };
        println!();
        match queued_behind {
            None => println!("  {} Changes detected ({n} {noun})", "↻".yellow().bold()),
            Some(id) => println!(
                "  {} Changes detected during build #{id} ({n} {noun} pending) — will rebuild once it finishes",
                "↻".yellow().bold()
            ),
        }
        for (path, kind) in changes.iter().take(MAX_LISTED) {
            println!(
                "    • {} {}",
                scope.display(path),
                format!("({kind})").dimmed()
            );
        }
        if n > MAX_LISTED {
            println!("    • … and {} more", n - MAX_LISTED);
        }
    }

    pub fn build_started(&self, id: u64, pid: u32, reason: &str) {
        if id == 1 {
            println!("  {} Building...", "→".green());
        }
        self.diag(
            "build",
            format_args!("started id={id} pid={pid} reason={reason}"),
        );
    }

    pub fn build_spawn_failed(&self, error: &std::io::Error) {
        eprintln!("  {} Could not start cargo build: {error}", "✗".red());
    }

    pub fn build_succeeded(&self, id: u64, duration: Duration) {
        self.diag(
            "build",
            format_args!("completed id={id} status=success duration={:.2?}", duration),
        );
        println!(
            "  {} Build completed in {:.2}s",
            "✓".green(),
            duration.as_secs_f64()
        );
    }

    pub fn build_failed(
        &self,
        id: u64,
        duration: Duration,
        status: Option<ExitStatus>,
        server_running: bool,
    ) {
        self.diag(
            "build",
            format_args!(
                "completed id={id} status=failed exit={} duration={:.2?}",
                status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                duration
            ),
        );
        println!(
            "  {} Build failed after {:.2}s — fix the errors above and save again",
            "✗".red().bold(),
            duration.as_secs_f64()
        );
        if server_running {
            println!("  {} The previous server keeps running", "→".dimmed());
        }
    }

    pub fn lockfile_absorbed(&self, scope: &WatchScope, path: &Path) {
        self.diag(
            "build",
            format_args!(
                "{} updated by cargo itself: not a new change",
                scope.display(path)
            ),
        );
    }

    pub fn lockfile_pending(&self, scope: &WatchScope, path: &Path, build: u64) {
        self.diag(
            "build",
            format_args!(
                "{} changed during build #{build}: checked when the build ends",
                scope.display(path)
            ),
        );
    }

    pub fn restart_skipped(&self) {
        println!(
            "  {} Binary unchanged and the server already runs with these changes — not restarted",
            "✓".green()
        );
    }

    pub fn no_executable(&self, message: &str) {
        println!("  {} {message}", "✗".red());
    }

    pub fn server_stopping(&self, pid: u32) {
        self.diag("server", format_args!("stopping pid={pid}"));
    }

    pub fn server_stopped(&self, pid: u32, report: &StopReport) {
        self.diag(
            "server",
            format_args!(
                "stopped pid={pid} forced={} status={}",
                report.forced,
                report
                    .status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ),
        );
        if report.leftovers {
            eprintln!(
                "  {} Some processes started by the server (pid {pid}) did not exit",
                "⚠".yellow()
            );
        }
    }

    pub fn server_started(&self, pid: u32, executable: &Path, restarted: bool) {
        self.diag(
            "server",
            format_args!("started pid={pid} exe={}", executable.display()),
        );
        if restarted {
            println!("  {} Server restarted", "✓".green());
        } else {
            println!("  {} Server started", "✓".green());
        }
    }

    pub fn server_start_failed(&self, error: &std::io::Error) {
        eprintln!("  {} Could not start the server: {error}", "✗".red());
    }

    pub fn server_exited(&self, pid: u32, status: ExitStatus, uptime: Duration) {
        self.diag(
            "server",
            format_args!("exited pid={pid} status={status} uptime={:.2?}", uptime),
        );
        if status.success() {
            println!(
                "  {} Server exited — waiting for changes to restart...",
                "→".yellow()
            );
        } else {
            println!(
                "  {} Server crashed ({status}) — waiting for changes to restart...",
                "⚠".red()
            );
        }
    }

    pub fn layout_changed(&self, entries: &[String]) {
        println!(
            "  {} Watch scope updated: {}",
            "→".green(),
            entries.join(", ")
        );
    }

    pub fn shutting_down(&self) {
        println!();
        println!("  {} Shutting down...", "→".yellow());
    }

    pub fn build_cancelled(&self, id: u64, report: &StopReport) {
        self.diag(
            "build",
            format_args!("cancelled id={id} forced={}", report.forced),
        );
    }

    pub fn stopped(&self) {
        println!("  {} Stopped", "✓".green());
    }
}
