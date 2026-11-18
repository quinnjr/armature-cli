//! Child processes that can be stopped *together with their descendants*.
//!
//! Killing only the direct child is not enough: when the child is `cargo
//! run`, the application keeps running (and keeps its port) after Cargo is
//! killed. Every child spawned by `armature dev` is therefore isolated:
//!
//! - Unix: in its own process group; the whole group receives `SIGTERM`,
//!   then `SIGKILL` once the grace period is over.
//! - Windows: in a new console process group (for a polite `CTRL_BREAK`)
//!   and in a Job Object configured with `KILL_ON_JOB_CLOSE`, terminated as
//!   a whole.
//!
//! `Drop` force-kills whatever is still alive, so an early return, an error
//! or a panic cannot leave orphans behind.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Outcome of [`ManagedChild::terminate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    /// Exit status of the direct child, if it could be collected.
    pub status: Option<ExitStatus>,
    /// The polite request was not enough and a forced kill was needed.
    pub forced: bool,
    /// Some descendants were still alive when we gave up.
    pub leftovers: bool,
}

pub struct ManagedChild {
    child: Child,
    pid: u32,
    status: Option<ExitStatus>,
    terminated: bool,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

impl ManagedChild {
    /// Spawn `command` in its own process group / job.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows::CREATE_NEW_PROCESS_GROUP);
        }

        let child = command.spawn()?;
        let pid = child.id();
        #[cfg(windows)]
        let job = windows::Job::for_child(&child).ok();

        Ok(Self {
            child,
            pid,
            status: None,
            terminated: false,
            #[cfg(windows)]
            job,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Non-blocking check of the direct child.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    /// Blocking wait for the direct child.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self.child.wait()?;
        self.status = Some(status);
        Ok(status)
    }

    /// Stop the child and all its descendants: polite request, wait up to
    /// `graceful`, then forced kill and wait up to `force`. Idempotent.
    pub fn terminate(&mut self, graceful: Duration, force: Duration) -> StopReport {
        if self.terminated {
            return StopReport {
                status: self.status,
                forced: false,
                leftovers: false,
            };
        }
        self.terminated = true;

        if !graceful.is_zero() && self.signal(false) && self.wait_all(graceful) {
            return StopReport {
                status: self.status,
                forced: false,
                leftovers: false,
            };
        }
        if self.all_gone() {
            return StopReport {
                status: self.status,
                forced: false,
                leftovers: false,
            };
        }

        self.signal(true);
        let gone = self.wait_all(force);
        StopReport {
            status: self.status,
            forced: true,
            leftovers: !gone,
        }
    }

    /// Wait until the child and its descendants are gone, or the timeout.
    fn wait_all(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.try_wait();
            if self.all_gone() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Whether the direct child and every descendant have exited.
    fn all_gone(&mut self) -> bool {
        let _ = self.try_wait();
        self.status.is_some() && !self.group_alive()
    }

    #[cfg(unix)]
    fn group_alive(&self) -> bool {
        unix::group_alive(self.pid)
    }

    #[cfg(windows)]
    fn group_alive(&self) -> bool {
        self.job.as_ref().is_some_and(windows::Job::has_processes)
    }

    #[cfg(not(any(unix, windows)))]
    fn group_alive(&self) -> bool {
        false
    }

    /// Send the polite (`force == false`) or forced stop request. Returns
    /// whether a polite request could be delivered.
    #[cfg(unix)]
    fn signal(&mut self, force: bool) -> bool {
        unix::signal_group(self.pid, force)
    }

    #[cfg(windows)]
    fn signal(&mut self, force: bool) -> bool {
        if force {
            match &self.job {
                Some(job) => job.terminate(),
                None => {
                    let _ = self.child.kill();
                }
            }
            true
        } else {
            windows::ctrl_break(self.pid)
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn signal(&mut self, force: bool) -> bool {
        if force {
            let _ = self.child.kill();
        }
        force
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.all_gone() {
            self.signal(true);
            let _ = self.wait_all(Duration::from_secs(2));
        }
    }
}

#[cfg(unix)]
mod unix {
    /// Signal the whole process group led by `pgid`.
    pub fn signal_group(pgid: u32, force: bool) -> bool {
        let Ok(pgid) = libc::pid_t::try_from(pgid) else {
            return false;
        };
        if pgid <= 1 {
            return false;
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: plain syscall; a negative pid targets the process group
        // created for this child (never our own group: see `process_group(0)`).
        unsafe { libc::kill(-pgid, signal) == 0 }
    }

    /// Whether any process (including zombies not yet reaped by their
    /// parent) still belongs to the group.
    pub fn group_alive(pgid: u32) -> bool {
        let Ok(pgid) = libc::pid_t::try_from(pgid) else {
            return false;
        };
        if pgid <= 1 {
            return false;
        }
        // SAFETY: signal 0 only checks for existence/permission.
        let rc = unsafe { libc::kill(-pgid, 0) };
        let exists = rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists && !only_zombies_left(pgid)
    }

    /// On Linux, orphaned descendants that already exited stay zombies
    /// until PID 1 reaps them — which never happens in containers without a
    /// real init. They hold no resources (no port, no CPU), so they must not
    /// make us wait for the full timeout.
    #[cfg(target_os = "linux")]
    fn only_zombies_left(pgid: libc::pid_t) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // Format: "pid (comm) state ppid pgrp ..."; comm may contain spaces.
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let pgrp = fields.nth(1).and_then(|p| p.parse::<libc::pid_t>().ok());
            if pgrp == Some(pgid) && state != Some("Z") && state != Some("X") {
                return false;
            }
        }
        true
    }

    #[cfg(not(target_os = "linux"))]
    fn only_zombies_left(_pgid: libc::pid_t) -> bool {
        false
    }

    /// Whether `pid` is a live (non-zombie) process. Used by tests.
    #[cfg(test)]
    pub fn pid_alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat
                .rsplit_once(')')
                .and_then(|(_, r)| r.split_whitespace().next());
            return !matches!(state, Some("Z" | "X"));
        }
        // SAFETY: existence check only.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

#[cfg(all(test, unix))]
pub(crate) use unix::pid_alive;

#[cfg(windows)]
mod windows {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };

    pub use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    /// Job Object owning a child and every process it creates.
    pub struct Job(HANDLE);

    impl Job {
        pub fn for_child(child: &Child) -> std::io::Result<Self> {
            // SAFETY: FFI calls with valid arguments; the handle is owned by
            // `Job` and closed in `Drop`.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: valid job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        pub fn has_processes(&self) -> bool {
            // SAFETY: valid job handle and correctly sized out-buffer.
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                );
                ok != 0 && info.ActiveProcesses > 0
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the last handle kills remaining processes
            // (KILL_ON_JOB_CLOSE).
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Polite stop request (the child was started with
    /// `CREATE_NEW_PROCESS_GROUP`, so its group id is its pid).
    pub fn ctrl_break(pid: u32) -> bool {
        // SAFETY: plain FFI call.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        // Give the reaper a moment, then require the process to be really gone.
        std::thread::sleep(Duration::from_millis(50));
        pid_alive(pid)
    }

    /// Shell that starts a grandchild (`sleep`) and prints its pid.
    fn spawn_with_grandchild(script: &str) -> (ManagedChild, u32) {
        use std::io::{BufRead, BufReader};
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdout(std::process::Stdio::piped());
        let mut child = ManagedChild::spawn(&mut cmd).unwrap();
        let stdout = child.child_mut().stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        (child, line.trim().parse().unwrap())
    }

    #[test]
    fn terminate_stops_descendants_gracefully() {
        let (mut child, grandchild) = spawn_with_grandchild("sleep 60 & echo $!; wait");
        assert!(alive(grandchild));
        let report = child.terminate(Duration::from_secs(3), Duration::from_secs(2));
        assert!(!report.forced);
        assert!(!report.leftovers);
        assert!(!alive(grandchild), "grandchild must not be orphaned");
        // Idempotent.
        assert!(
            !child
                .terminate(Duration::from_secs(1), Duration::from_secs(1))
                .forced
        );
    }

    #[test]
    fn terminate_escalates_to_sigkill() {
        let (mut child, grandchild) =
            spawn_with_grandchild("trap '' TERM; sleep 60 & echo $!; while :; do sleep 1; done");
        let started = Instant::now();
        let report = child.terminate(Duration::from_millis(300), Duration::from_secs(2));
        assert!(report.forced);
        assert!(!report.leftovers);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!alive(grandchild));
    }

    #[test]
    fn drop_kills_the_group() {
        let (child, grandchild) = spawn_with_grandchild("sleep 60 & echo $!; wait");
        let pid = child.pid();
        drop(child);
        assert!(!alive(pid));
        assert!(!alive(grandchild));
    }
}
