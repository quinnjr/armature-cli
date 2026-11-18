//! `ServerProcess`: the application binary itself (never Cargo).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::process::{ManagedChild, StopReport};

/// How to launch the application.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Where run copies are placed on Windows (see [`ServerProcess::start`]).
    pub scratch_dir: PathBuf,
}

pub struct ServerProcess {
    process: ManagedChild,
    started_at: Instant,
    exit_reported: bool,
    /// Copy of the executable that is actually running (Windows only).
    run_copy: Option<PathBuf>,
}

impl ServerProcess {
    /// Start the application in its own process group / job.
    ///
    /// On Windows a running `.exe` is locked, which would make the *next*
    /// `cargo build` fail while the old server still runs; a copy is run
    /// instead, so the old server can keep serving until the new build
    /// succeeded. Unix has no such lock.
    pub fn start(spec: &ServerSpec) -> std::io::Result<Self> {
        let run_copy = if cfg!(windows) {
            Some(copy_for_run(&spec.executable, &spec.scratch_dir)?)
        } else {
            None
        };
        let program = run_copy.as_deref().unwrap_or(&spec.executable);

        let mut command = Command::new(program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        // Same dynamic-library search path additions as `cargo run`.
        if let Some(dir) = spec.executable.parent() {
            let var = library_path_var();
            let mut paths = vec![dir.join("deps"), dir.to_path_buf()];
            if let Some(existing) = std::env::var_os(var) {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                command.env(var, joined);
            }
        }

        let process = match ManagedChild::spawn(&mut command) {
            Ok(p) => p,
            Err(e) => {
                if let Some(copy) = &run_copy {
                    let _ = std::fs::remove_file(copy);
                }
                return Err(e);
            }
        };
        Ok(Self {
            process,
            started_at: Instant::now(),
            exit_reported: false,
            run_copy,
        })
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Returns the exit status the first time the server is found dead.
    pub fn poll_exit(&mut self) -> Option<ExitStatus> {
        if self.exit_reported {
            return None;
        }
        let status = self.process.try_wait().ok().flatten()?;
        self.exit_reported = true;
        Some(status)
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(None))
    }

    /// Stop the server and its descendants, waiting for them to exit.
    pub fn stop(mut self, graceful: Duration, force: Duration) -> StopReport {
        let report = self.process.terminate(graceful, force);
        if let Some(copy) = self.run_copy.take() {
            let _ = std::fs::remove_file(copy);
        }
        report
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // `ManagedChild::drop` kills the group; the copy can go afterwards.
        if let Some(copy) = self.run_copy.take() {
            let _ = self
                .process
                .terminate(Duration::ZERO, Duration::from_secs(2));
            let _ = std::fs::remove_file(copy);
        }
    }
}

fn library_path_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

fn copy_for_run(executable: &Path, scratch_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(scratch_dir)?;
    let stem = executable
        .file_stem()
        .map(OsString::from)
        .unwrap_or_else(|| "server".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut name = stem;
    name.push(format!("-{}-{nanos}", std::process::id()));
    if let Some(ext) = executable.extension() {
        name.push(".");
        name.push(ext);
    }
    let copy = scratch_dir.join(name);
    std::fs::copy(executable, &copy)?;
    Ok(copy)
}
