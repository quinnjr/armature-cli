//! Build phase: `cargo build`, distinct from running the server.
//!
//! The old implementation ran `cargo run`, so "the server" was really Cargo,
//! and killing it left the application running. Now Cargo only builds; the
//! path of the produced executable is read from Cargo's JSON messages and the
//! executable is started directly by `ServerProcess`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::process::{ManagedChild, StopReport};

/// `armature dev -- <cargo args> [-- <app args>]` split in two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevArgsSplit {
    /// Arguments for `cargo build` (`--release`, `--bin api`, `-F x`…).
    pub build: Vec<String>,
    /// Arguments for the application itself.
    pub app: Vec<String>,
}

impl DevArgsSplit {
    /// Same convention as `cargo run`: everything after the first `--` goes
    /// to the program.
    pub fn parse(cargo_args: &[String]) -> Self {
        match cargo_args.iter().position(|a| a == "--") {
            Some(i) => Self {
                build: cargo_args[..i].to_vec(),
                app: cargo_args[i + 1..].to_vec(),
            },
            None => Self {
                build: cargo_args.to_vec(),
                app: Vec::new(),
            },
        }
    }

    /// Target explicitly requested with `--bin NAME` or `--example NAME`.
    pub fn requested_target(&self) -> Option<(&'static str, &str)> {
        let mut iter = self.build.iter();
        while let Some(arg) = iter.next() {
            for (flag, kind) in [("--bin", "bin"), ("--example", "example")] {
                if arg == flag {
                    return iter.next().map(|n| (kind, n.as_str()));
                }
                if let Some(name) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
                    return Some((kind, name));
                }
            }
        }
        None
    }
}

/// An executable produced by the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    pub name: String,
    pub kind: String,
    pub package_id: String,
    pub path: PathBuf,
    /// Cargo did not have to rebuild it (identical to the previous build).
    pub fresh: bool,
}

/// Parse one line of `cargo build --message-format=json`.
pub fn parse_message(line: &str) -> Option<Executable> {
    let json: serde_json::Value = serde_json::from_str(line).ok()?;
    if json.get("reason")?.as_str()? != "compiler-artifact" {
        return None;
    }
    if json
        .get("profile")
        .and_then(|p| p.get("test"))
        .and_then(|t| t.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    let path = json.get("executable")?.as_str()?;
    let target = json.get("target")?;
    let kind = target
        .get("kind")?
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str())
        .find(|k| *k == "bin" || *k == "example")?;
    Some(Executable {
        name: target.get("name")?.as_str()?.to_string(),
        kind: kind.to_string(),
        package_id: json.get("package_id")?.as_str()?.to_string(),
        path: PathBuf::from(path),
        fresh: json.get("fresh").and_then(|f| f.as_bool()).unwrap_or(false),
    })
}

/// Pick the executable to run, with the same rules as `cargo run`.
pub fn select_executable(
    executables: &[Executable],
    args: &DevArgsSplit,
    root_package_id: Option<&str>,
    default_run: Option<&str>,
) -> Result<Executable, String> {
    if let Some((kind, name)) = args.requested_target() {
        return executables
            .iter()
            .find(|e| e.kind == kind && e.name == name)
            .cloned()
            .ok_or_else(|| format!("cargo did not produce the requested {kind} `{name}`"));
    }
    let bins: Vec<&Executable> = executables.iter().filter(|e| e.kind == "bin").collect();
    if let [only] = bins.as_slice() {
        return Ok((*only).clone());
    }
    let own: Vec<&Executable> = match root_package_id {
        Some(id) => bins
            .iter()
            .copied()
            .filter(|e| e.package_id == id)
            .collect(),
        None => bins.clone(),
    };
    if let [only] = own.as_slice() {
        return Ok((*only).clone());
    }
    if let Some(default) = default_run
        && let Some(exe) = own.iter().find(|e| e.name == default)
    {
        return Ok((*exe).clone());
    }
    if bins.is_empty() {
        return Err("the build produced no binary to run".to_string());
    }
    let mut names: Vec<&str> = bins.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    Err(format!(
        "several binaries were built ({}); choose one with `armature dev -- --bin <name>` \
         or set `default-run` in Cargo.toml",
        names.join(", ")
    ))
}

/// Result of a finished build.
#[derive(Debug)]
pub struct BuildResult {
    pub id: u64,
    pub status: Option<ExitStatus>,
    pub duration: Duration,
    pub executables: Vec<Executable>,
}

impl BuildResult {
    pub fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }
}

/// A running `cargo build`.
pub struct BuildProcess {
    id: u64,
    process: ManagedChild,
    started: Instant,
    reader: Option<JoinHandle<Vec<Executable>>>,
}

impl BuildProcess {
    /// Start `cargo build`. `on_output_closed` is called from the reader
    /// thread once Cargo closed its stdout, i.e. when the build is over.
    pub fn spawn<F>(
        id: u64,
        project_root: &Path,
        args: &DevArgsSplit,
        on_output_closed: F,
    ) -> std::io::Result<Self>
    where
        F: FnOnce() + Send + 'static,
    {
        let mut command = Command::new("cargo");
        command
            .arg("build")
            .arg("--message-format=json-render-diagnostics")
            .args(&args.build)
            .current_dir(project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Progress and rendered diagnostics go straight to the terminal.
            .stderr(Stdio::inherit());
        let mut process = ManagedChild::spawn(&mut command)?;
        let stdout = process
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("cargo stdout not captured"))?;

        let reader = std::thread::Builder::new()
            .name(format!("armature-dev-build-{id}"))
            .spawn(move || {
                let executables = BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|line| parse_message(&line))
                    .collect();
                on_output_closed();
                executables
            })?;

        Ok(Self {
            id,
            process,
            started: Instant::now(),
            reader: Some(reader),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// Collect the result. Call after `on_output_closed` fired.
    pub fn finish(mut self) -> BuildResult {
        let status = self.process.wait().ok();
        let executables = self
            .reader
            .take()
            .and_then(|r| r.join().ok())
            .unwrap_or_default();
        BuildResult {
            id: self.id,
            status,
            duration: self.started.elapsed(),
            executables,
        }
    }

    /// Abort the build (shutdown).
    pub fn cancel(mut self) -> StopReport {
        let report = self
            .process
            .terminate(Duration::from_secs(2), Duration::from_secs(2));
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn exe(name: &str, kind: &str, pkg: &str) -> Executable {
        Executable {
            name: name.into(),
            kind: kind.into(),
            package_id: pkg.into(),
            path: PathBuf::from(format!("/t/{name}")),
            fresh: false,
        }
    }

    #[test]
    fn args_split_like_cargo_run() {
        let split = DevArgsSplit::parse(&strings(&[
            "--release",
            "--bin",
            "api",
            "--",
            "--port",
            "1",
        ]));
        assert_eq!(split.build, strings(&["--release", "--bin", "api"]));
        assert_eq!(split.app, strings(&["--port", "1"]));
        assert_eq!(split.requested_target(), Some(("bin", "api")));
        assert_eq!(
            DevArgsSplit::parse(&strings(&["--example=demo"])).requested_target(),
            Some(("example", "demo"))
        );
        assert_eq!(DevArgsSplit::parse(&[]), DevArgsSplit::default());
    }

    #[test]
    fn parses_bin_artifacts_only() {
        let bin = r#"{"reason":"compiler-artifact","package_id":"p","target":{"kind":["bin"],"name":"api"},"profile":{"test":false},"executable":"/t/debug/api","fresh":false}"#;
        let lib = r#"{"reason":"compiler-artifact","package_id":"p","target":{"kind":["lib"],"name":"api"},"profile":{"test":false},"executable":null}"#;
        let test = r#"{"reason":"compiler-artifact","package_id":"p","target":{"kind":["bin"],"name":"api"},"profile":{"test":true},"executable":"/t/debug/deps/api-123"}"#;
        assert_eq!(
            parse_message(bin),
            Some(exe("api", "bin", "p")).map(|mut e| {
                e.path = PathBuf::from("/t/debug/api");
                e
            })
        );
        assert_eq!(parse_message(lib), None);
        assert_eq!(parse_message(test), None);
        assert_eq!(parse_message("not json"), None);
        assert_eq!(
            parse_message(r#"{"reason":"build-finished","success":true}"#),
            None
        );
    }

    #[test]
    fn selects_like_cargo_run() {
        let none = DevArgsSplit::default();
        assert_eq!(
            select_executable(&[exe("api", "bin", "p")], &none, None, None)
                .unwrap()
                .name,
            "api"
        );

        let several = [
            exe("api", "bin", "p"),
            exe("worker", "bin", "p"),
            exe("tool", "bin", "q"),
        ];
        assert!(select_executable(&several, &none, Some("p"), None).is_err());
        assert_eq!(
            select_executable(&several, &none, Some("p"), Some("worker"))
                .unwrap()
                .name,
            "worker"
        );
        let wanted = DevArgsSplit::parse(&strings(&["--bin", "tool"]));
        assert_eq!(
            select_executable(&several, &wanted, Some("p"), None)
                .unwrap()
                .name,
            "tool"
        );

        let other_pkg = [exe("api", "bin", "p"), exe("tool", "bin", "q")];
        assert_eq!(
            select_executable(&other_pkg, &none, Some("p"), None)
                .unwrap()
                .name,
            "api"
        );
        assert!(select_executable(&[], &none, None, None).is_err());
    }
}
