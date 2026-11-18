//! End-to-end tests of `armature dev` with the built-in watcher.
//!
//! Each test creates a tiny dependency-free Cargo project whose binary:
//! - binds `HOST:PORT` (so a server that was not stopped makes the next one
//!   fail with `APP_BIND_FAILED`),
//! - starts a grandchild process (`sleep`), to check that descendants are
//!   stopped too,
//! - prints its pid, the grandchild pid and a version string.
//!
//! The tests drive `armature --verbose dev` and read its output.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long "nothing must happen" is observed.
const QUIET_PERIOD: Duration = Duration::from_millis(2500);

const MAIN_RS: &str = r#"
mod version;

use std::io::Read;

fn main() {
    let host = std::env::var("HOST").unwrap();
    let port = std::env::var("PORT").unwrap();
    let listener = match std::net::TcpListener::bind(format!("{host}:{port}")) {
        Ok(l) => l,
        Err(e) => {
            println!("APP_BIND_FAILED {e}");
            std::process::exit(1);
        }
    };
    let grandchild = std::process::Command::new("sleep").arg("300").spawn().unwrap();
    println!("APP_CHILD pid={}", grandchild.id());
    println!("APP_STARTED pid={} version={}", std::process::id(), version::VERSION);
    for stream in listener.incoming() {
        let mut buf = [0u8; 16];
        let _ = stream.map(|mut s| s.read(&mut buf));
    }
}
"#;

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Project {
    fn new(build_rs: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let build_line = if build_rs.is_some() {
            "build = \"build.rs\"\n"
        } else {
            ""
        };
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"devtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{build_line}\
                 # armature project\n\n[dependencies]\n\n[workspace]\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), MAIN_RS).unwrap();
        fs::write(
            root.join("src/version.rs"),
            "pub const VERSION: &str = \"v1\";\n",
        )
        .unwrap();
        if let Some(build_rs) = build_rs {
            fs::write(root.join("build.rs"), build_rs).unwrap();
        }
        Self { _dir: dir, root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn set_version(&self, version: &str) {
        fs::write(
            self.path("src/version.rs"),
            format!("pub const VERSION: &str = \"{version}\";\n"),
        )
        .unwrap();
    }
}

struct Dev {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Dev {
    fn start(project: &Project) -> Self {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_armature"))
            .args([
                "--no-color",
                "--verbose",
                "dev",
                "--no-cargo-watch",
                "--debounce-ms",
                "200",
                "--port",
                &port.to_string(),
            ])
            .current_dir(&project.root)
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        spawn_reader(stdout, lines.clone());
        spawn_reader(stderr, lines.clone());
        let dev = Self { child, lines };
        dev.wait_for("APP_STARTED", 1, STARTUP_TIMEOUT);
        dev
    }

    fn output(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }

    fn count(&self, needle: &str) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    fn wait_for(&self, needle: &str, occurrences: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.count(needle) < occurrences {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {occurrences}× {needle:?}\n--- output ---\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn builds(&self) -> usize {
        self.count("[build] started")
    }

    /// pid and grandchild pid of the n-th (1-based) server instance.
    fn app_pids(&self, n: usize) -> (u32, u32) {
        let lines = self.lines.lock().unwrap();
        let grab = |prefix: &str| -> u32 {
            lines
                .iter()
                .filter_map(|l| l.split_once(prefix).map(|(_, r)| r.to_string()))
                .nth(n - 1)
                .and_then(|r| r.split_whitespace().next().map(str::to_string))
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("no {prefix} #{n}"))
        };
        (grab("APP_STARTED pid="), grab("APP_CHILD pid="))
    }

    fn last_version(&self) -> String {
        let lines = self.lines.lock().unwrap();
        lines
            .iter()
            .rev()
            .find_map(|l| l.split_once("version=").map(|(_, v)| v.trim().to_string()))
            .unwrap()
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: plain syscall on our own child.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, signal);
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "armature dev did not exit\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.signal(libc::SIGINT);
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(source: R, lines: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(source).lines().map_while(Result::ok) {
            lines.lock().unwrap().push(line);
        }
    });
}

/// Whether a process is alive (zombies count as dead).
fn alive(pid: u32) -> bool {
    std::thread::sleep(Duration::from_millis(50));
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().next().map(str::to_string));
        return !matches!(state.as_deref(), Some("Z" | "X"));
    }
    // SAFETY: existence check only.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn quiet() {
    std::thread::sleep(QUIET_PERIOD);
}

fn touch(path: &Path) {
    let file = fs::File::options().append(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now()).unwrap();
}

#[test]
fn idle_project_builds_once_and_stays_quiet() {
    let project = Project::new(None);
    let dev = Dev::start(&project);
    dev.wait_for("Waiting for changes", 1, STEP_TIMEOUT);

    // Reading sources (what rustc does), touching them (metadata only) and
    // rewriting identical content must not rebuild.
    for _ in 0..3 {
        let _ = fs::read_to_string(project.path("src/main.rs")).unwrap();
        touch(&project.path("src/version.rs"));
    }
    fs::write(
        project.path("src/version.rs"),
        "pub const VERSION: &str = \"v1\";\n",
    )
    .unwrap();
    quiet();

    assert_eq!(dev.builds(), 1, "{}", dev.output());
    assert_eq!(dev.count("APP_STARTED"), 1);
    if cfg!(target_os = "linux") {
        // The original loop: rustc opening sources produced `Access(Open)`
        // events. They are still received, but rejected.
        assert!(
            dev.count("event=Access(Open) path=src/main.rs accepted=false") > 0,
            "{}",
            dev.output()
        );
    }
}

#[test]
fn single_save_rebuilds_once_and_replaces_the_server() {
    let project = Project::new(None);
    let dev = Dev::start(&project);
    let (old_pid, old_grandchild) = dev.app_pids(1);

    project.set_version("v2");
    dev.wait_for("APP_STARTED", 2, STEP_TIMEOUT);
    quiet();

    assert_eq!(dev.builds(), 2, "{}", dev.output());
    assert_eq!(dev.count("APP_STARTED"), 2);
    assert_eq!(
        dev.count("APP_BIND_FAILED"),
        0,
        "old server still held the port"
    );
    assert_eq!(dev.last_version(), "v2");
    assert!(!alive(old_pid), "old server still running");
    assert!(!alive(old_grandchild), "old server's child was orphaned");
    let (new_pid, _) = dev.app_pids(2);
    assert!(alive(new_pid));
    assert_eq!(dev.count("Changes detected (1 file)"), 1);
}

#[test]
fn editor_style_saves_rebuild_once() {
    let project = Project::new(None);
    let dev = Dev::start(&project);
    let version = project.path("src/version.rs");

    // vim (backup + rename + write + delete backup), then an atomic
    // "write temp file, rename over" save, all within one window.
    fs::rename(&version, project.path("src/version.rs~")).unwrap();
    fs::write(project.path("src/4913"), "").unwrap();
    fs::remove_file(project.path("src/4913")).unwrap();
    fs::write(&version, "pub const VERSION: &str = \"v2\";\n").unwrap();
    fs::remove_file(project.path("src/version.rs~")).unwrap();
    let tmp = project.path("src/.version.rs.tmp");
    fs::write(&tmp, "pub const VERSION: &str = \"v3\";\n").unwrap();
    fs::rename(&tmp, &version).unwrap();
    fs::write(&version, "pub const VERSION: &str = \"v3\";\n").unwrap();

    dev.wait_for("APP_STARTED", 2, STEP_TIMEOUT);
    quiet();
    assert_eq!(dev.builds(), 2, "{}", dev.output());
    assert_eq!(dev.last_version(), "v3");
}

#[test]
fn failed_build_does_not_loop_and_keeps_the_old_server() {
    let project = Project::new(None);
    let dev = Dev::start(&project);
    let (old_pid, _) = dev.app_pids(1);

    fs::write(
        project.path("src/version.rs"),
        "pub const VERSION: &str = ;\n",
    )
    .unwrap();
    dev.wait_for("Build failed", 1, STEP_TIMEOUT);
    quiet();
    assert_eq!(
        dev.builds(),
        2,
        "a failed build must not be retried\n{}",
        dev.output()
    );
    assert_eq!(dev.count("Build failed"), 1);
    assert!(alive(old_pid), "the previous server must keep running");
    assert_eq!(dev.count("APP_STARTED"), 1);

    project.set_version("v2");
    dev.wait_for("APP_STARTED", 2, STEP_TIMEOUT);
    quiet();
    assert_eq!(dev.builds(), 3, "{}", dev.output());
    assert_eq!(dev.last_version(), "v2");
    assert!(!alive(old_pid));
}

#[test]
fn manifest_and_lockfile_changes_trigger_a_rebuild() {
    let project = Project::new(None);
    let dev = Dev::start(&project);

    let manifest = fs::read_to_string(project.path("Cargo.toml")).unwrap();
    fs::write(project.path("Cargo.toml"), format!("{manifest}# edited\n")).unwrap();
    dev.wait_for("APP_STARTED", 2, STEP_TIMEOUT);
    quiet();
    assert_eq!(dev.builds(), 2, "{}", dev.output());

    let lock = fs::read_to_string(project.path("Cargo.lock")).unwrap();
    fs::write(project.path("Cargo.lock"), format!("{lock}\n")).unwrap();
    dev.wait_for("APP_STARTED", 3, STEP_TIMEOUT);
    quiet();
    // Building may rewrite Cargo.lock; that must not cause yet another build
    // unless its content really changed.
    let builds = dev.builds();
    assert!((3..=4).contains(&builds), "{}", dev.output());
    quiet();
    assert_eq!(dev.builds(), builds, "{}", dev.output());

    // Files outside the build inputs are ignored.
    fs::write(project.path("README.md"), "# hi\n").unwrap();
    quiet();
    assert_eq!(dev.builds(), builds);
}

#[test]
fn change_during_build_is_queued_and_built_once_afterwards() {
    // A build script that makes every rebuild of version.rs slow.
    let project = Project::new(Some(
        "fn main() {\n    println!(\"cargo:rerun-if-changed=src/version.rs\");\n    \
         std::thread::sleep(std::time::Duration::from_millis(2500));\n}\n",
    ));
    let dev = Dev::start(&project);

    project.set_version("v2");
    dev.wait_for("[build] started id=2", 1, STEP_TIMEOUT);
    std::thread::sleep(Duration::from_millis(500));
    project.set_version("v3");
    std::thread::sleep(Duration::from_millis(300));
    project.set_version("v4");

    dev.wait_for("Changes detected during build #2", 1, STEP_TIMEOUT);
    dev.wait_for("[build] started id=3", 1, STEP_TIMEOUT);
    dev.wait_for("[build] completed id=3", 1, STEP_TIMEOUT);
    dev.wait_for("APP_STARTED", 3, STEP_TIMEOUT);
    quiet();

    assert_eq!(dev.builds(), 3, "{}", dev.output());
    assert_eq!(dev.last_version(), "v4", "the queued change was lost");
    // Builds never overlap: every start is followed by its completion
    // before the next start.
    let lines = dev.lines.lock().unwrap().clone();
    let mut running = false;
    for line in lines.iter().filter(|l| l.contains("[build] ")) {
        if line.contains("[build] started") {
            assert!(!running, "concurrent builds:\n{}", lines.join("\n"));
            running = true;
        } else if line.contains("[build] completed") {
            running = false;
        }
    }
}

#[test]
fn ctrl_c_stops_the_server_and_its_descendants() {
    let project = Project::new(None);
    let mut dev = Dev::start(&project);
    let (pid, grandchild) = dev.app_pids(1);
    assert!(alive(pid) && alive(grandchild));

    dev.signal(libc::SIGINT);
    let status = dev.wait_exit(Duration::from_secs(15));
    assert!(status.success(), "{}", dev.output());
    assert!(!alive(pid), "server orphaned");
    assert!(!alive(grandchild), "server's child orphaned");
    assert!(dev.output().contains("Stopped"));
}

#[test]
fn sigterm_also_cleans_up() {
    let project = Project::new(None);
    let mut dev = Dev::start(&project);
    let (pid, grandchild) = dev.app_pids(1);

    dev.signal(libc::SIGTERM);
    dev.wait_exit(Duration::from_secs(15));
    assert!(!alive(pid));
    assert!(!alive(grandchild));
}

#[test]
fn runtime_config_change_restarts_even_if_the_binary_is_unchanged() {
    let project = Project::new(None);
    let dev = Dev::start(&project);

    // armature.toml is read by the application, not compiled: cargo has
    // nothing to rebuild, but the server must still be restarted.
    fs::write(project.path("armature.toml"), "[server]\nport = 1\n").unwrap();
    dev.wait_for("APP_STARTED", 2, STEP_TIMEOUT);
    quiet();
    assert_eq!(dev.builds(), 2, "{}", dev.output());
    assert_eq!(dev.count("APP_STARTED"), 2);
}
