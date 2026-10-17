//! External subcommand discovery and dispatch.
//!
//! Mirrors cargo's own extension mechanism: any executable named
//! `armature-<name>` found on `$PATH` becomes available as `armature <name>`.
//! This lets other crates in (or outside) the workspace ship their own
//! standalone CLI tools — e.g. `armature-graphql-sdl` — that plug into the
//! `armature` command without armature-cli needing to know about them at
//! compile time.
//!
//! This is discovery-only: there's no install/registry step. If a binary
//! named `armature-foo` is on `$PATH`, `armature foo` runs it, forwarding
//! all remaining arguments and the process's stdio, and exits with its exit
//! code. Nothing here downloads, builds, or manages those binaries — that's
//! left to `cargo install`, a package manager, or however the user got the
//! binary onto `$PATH` in the first place.

use crate::error::{CliError, CliResult};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prefix every discoverable external subcommand binary must start with.
const PREFIX: &str = "armature-";

/// Names that must NOT be treated as external subcommands even though they
/// start with the prefix — this crate's own binary, and its lib-crate name
/// (which some packaging setups place a same-named binary for).
const RESERVED: &[&str] = &["armature-cli"];

/// Scan every directory on `$PATH` for executables named `armature-<name>`
/// and return a map of `<name>` → full path to the binary.
///
/// If more than one matching binary is found on `$PATH` for the same name,
/// the first one found (in `$PATH` order, matching how the shell itself
/// resolves commands) wins — later duplicates are silently shadowed, same
/// as normal `$PATH` lookup semantics.
pub fn discover_external_commands() -> BTreeMap<String, PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return BTreeMap::new();
    };
    let cwd = std::env::current_dir().ok();
    discover_in(&search_dirs(&path_var, cwd.as_deref()))
}

/// The `$PATH` entries that may be searched for extension binaries.
///
/// Only absolute directories qualify. An empty entry (`PATH=":/usr/bin"`, a
/// trailing `:`) or a relative one (`.`, `bin`) resolves against whatever
/// directory `armature` happens to be run from — typically a freshly cloned,
/// untrusted repository — so a checked-in `armature-build` would be executed
/// by `armature build`. Absolute entries that point at the current directory
/// are dropped for the same reason.
fn search_dirs(path_var: &std::ffi::OsStr, cwd: Option<&Path>) -> Vec<PathBuf> {
    let cwd = cwd.and_then(|c| c.canonicalize().ok());
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
        .filter(|dir| match (&cwd, dir.canonicalize()) {
            (Some(cwd), Ok(resolved)) => &resolved != cwd,
            _ => true,
        })
        .collect()
}

/// Scan `dirs` (in order) for `armature-<name>` executables.
fn discover_in(dirs: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };

            // Strip a platform executable extension (Windows) before
            // matching, so `armature-foo.exe` is discovered as `foo`.
            let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);

            let Some(subcommand_name) = stem.strip_prefix(PREFIX) else {
                continue;
            };
            if subcommand_name.is_empty() || RESERVED.contains(&stem) {
                continue;
            }

            let full_path = entry.path();
            if !is_executable(&full_path) {
                continue;
            }

            // First match found on PATH wins (PATH is walked in order).
            found
                .entry(subcommand_name.to_string())
                .or_insert(full_path);
        }
    }

    found
}

/// Best-effort executable check. On Unix, requires at least one execute bit
/// to be set (and that the entry isn't a directory). On other platforms,
/// any regular file matching the naming convention is treated as a
/// candidate, since executability isn't exposed the same way.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Look up and run `armature-<name>`, forwarding `args` and inheriting the
/// current process's stdio. Returns once the child exits; the caller is
/// responsible for propagating its exit code (this function itself only
/// signals whether the external command was *found and launched*, not
/// whether it succeeded — a nonzero exit from a real, found tool is not a
/// `CliError`, it's just that tool's own result).
///
/// Returns `Ok(exit_code)` if the binary was found and run (exit_code may be
/// nonzero), or `Err(CliError::Tool(..))` if no matching binary exists on
/// `$PATH`, with a message listing what *was* discovered to help the user
/// spot a typo.
pub fn run_external_command(name: &str, args: &[String]) -> CliResult<i32> {
    let discovered = discover_external_commands();

    let Some(binary_path) = discovered.get(name) else {
        let suggestion = if discovered.is_empty() {
            "No armature-* extension binaries were found on $PATH.".to_string()
        } else {
            let names: Vec<&str> = discovered.keys().map(String::as_str).collect();
            format!(
                "No binary named '{PREFIX}{name}' was found on $PATH. Available extensions: {}",
                names.join(", ")
            )
        };
        return Err(CliError::Tool(format!(
            "unknown command '{name}'. {suggestion}"
        )));
    };

    let status = Command::new(binary_path)
        .args(args)
        .status()
        .map_err(|e| CliError::Tool(format!("failed to launch {}: {e}", binary_path.display())))?;

    // On Unix a process killed by a signal has no exit code at all; report
    // a conventional 128+signal value in that case rather than silently
    // defaulting to 0 (which would look like success).
    Ok(status.code().unwrap_or(128))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_external_commands_never_includes_the_cli_itself() {
        // Whatever's actually on this machine's PATH, the CLI's own binary
        // name must never appear as a discovered *external* subcommand,
        // since that would create `armature cli` shadowing nothing useful
        // and potentially recursing.
        let discovered = discover_external_commands();
        assert!(!discovered.contains_key("cli"));
    }

    #[test]
    fn run_external_command_reports_a_helpful_error_for_an_unknown_name() {
        let result = run_external_command("this-definitely-does-not-exist-as-a-binary", &[]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("this-definitely-does-not-exist-as-a-binary"));
    }

    #[test]
    fn search_dirs_skips_empty_relative_and_current_directory_entries() {
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([
            PathBuf::new(),
            PathBuf::from("."),
            PathBuf::from("bin"),
            cwd.path().to_path_buf(),
            other.path().to_path_buf(),
        ])
        .unwrap();
        let dirs = search_dirs(&path, Some(cwd.path()));
        assert_eq!(dirs, vec![other.path().to_path_buf()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_in_the_working_directory_is_not_discovered() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = tempfile::tempdir().unwrap();
        let planted = cwd.path().join("armature-build");
        std::fs::write(&planted, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Reached through ".", "" and the absolute cwd: none may count.
        let path =
            std::env::join_paths([PathBuf::from("."), PathBuf::new(), cwd.path().to_path_buf()])
                .unwrap();
        let found = discover_in(&search_dirs(&path, Some(cwd.path())));
        assert!(found.is_empty(), "{found:?}");

        // The same directory reached from elsewhere is a legitimate PATH entry.
        let elsewhere = tempfile::tempdir().unwrap();
        let found = discover_in(&search_dirs(
            &std::env::join_paths([cwd.path()]).unwrap(),
            Some(elsewhere.path()),
        ));
        assert_eq!(found.get("build"), Some(&planted));
    }

    #[test]
    fn is_executable_rejects_directories() {
        assert!(!is_executable(Path::new(".")));
    }
}
