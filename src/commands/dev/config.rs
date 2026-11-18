//! Configuration of `armature dev`.
//!
//! Every setting has a default, so `armature dev` works without any
//! configuration. Projects can override the defaults in the `dev` section of
//! `armature.toml` (the project configuration file, at the project root):
//!
//! ```toml
//! [dev.watcher]
//! enabled = true                  # false: build and run once, no watching
//! debounce_ms = 400               # stability window before a rebuild
//! watch_paths = ["config"]        # extra files/dirs, relative to the project root
//! exclude_paths = ["src/generated"]
//! verbose = false                 # same as `armature --verbose dev`
//!
//! [dev.server]
//! graceful_shutdown_timeout_ms = 3000
//! force_kill_timeout_ms = 2000
//! ```
//!
//! Other sections of `armature.toml` are ignored here. Command-line flags
//! win over the file. The file is read once, when `armature dev` starts.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CliError, CliResult};

/// Project configuration file.
pub const CONFIG_FILE: &str = "armature.toml";

/// Default stability window (spec: 300–500 ms).
pub const DEFAULT_DEBOUNCE_MS: u64 = 400;
/// Default time the server gets to exit after SIGTERM / CTRL_BREAK.
pub const DEFAULT_GRACEFUL_SHUTDOWN_MS: u64 = 3000;
/// Default time allowed for a forced kill to take effect.
pub const DEFAULT_FORCE_KILL_MS: u64 = 2000;

/// Effective configuration of the development server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevConfig {
    /// Whether file watching is enabled at all.
    pub watch_enabled: bool,
    /// Stability window: a batch is emitted once no relevant event arrived
    /// for this long.
    pub debounce: Duration,
    /// Extra paths to watch, relative to the project root (or absolute).
    pub extra_watch_paths: Vec<PathBuf>,
    /// Paths to ignore, relative to the project root (or absolute).
    pub exclude_paths: Vec<PathBuf>,
    /// Print one diagnostic line per filesystem event, build and process.
    pub verbose: bool,
    /// Grace period between the polite stop request and the forced kill.
    pub graceful_shutdown_timeout: Duration,
    /// Time allowed for the forced kill to complete.
    pub force_kill_timeout: Duration,
}

impl Default for DevConfig {
    fn default() -> Self {
        Self {
            watch_enabled: true,
            debounce: Duration::from_millis(DEFAULT_DEBOUNCE_MS),
            extra_watch_paths: Vec::new(),
            exclude_paths: Vec::new(),
            verbose: false,
            graceful_shutdown_timeout: Duration::from_millis(DEFAULT_GRACEFUL_SHUTDOWN_MS),
            force_kill_timeout: Duration::from_millis(DEFAULT_FORCE_KILL_MS),
        }
    }
}

/// Overrides coming from the command line.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub verbose: bool,
    pub debounce_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDev {
    #[serde(default)]
    watcher: RawWatcher,
    #[serde(default)]
    server: RawServer,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatcher {
    enabled: Option<bool>,
    debounce_ms: Option<u64>,
    watch_paths: Option<Vec<PathBuf>>,
    exclude_paths: Option<Vec<PathBuf>>,
    verbose: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    graceful_shutdown_timeout_ms: Option<u64>,
    force_kill_timeout_ms: Option<u64>,
}

impl DevConfig {
    /// Load the configuration from `<project_root>/armature.toml` (if
    /// present), then apply the command-line overrides.
    pub fn load(project_root: &Path, overrides: &CliOverrides) -> CliResult<Self> {
        let file = project_root.join(CONFIG_FILE);
        let mut config = match std::fs::read_to_string(&file) {
            Ok(text) => Self::from_toml_str(&text)
                .map_err(|e| CliError::Config(format!("{}: {e}", file.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        config.apply(overrides);
        Ok(config)
    }

    /// Parse the `[dev]` section of an `armature.toml` document.
    pub fn from_toml_str(text: &str) -> CliResult<Self> {
        let value: toml::Value = toml::from_str(text)?;
        let raw: RawDev = match value.get("dev").cloned() {
            Some(section) => section.try_into().map_err(|e: toml::de::Error| {
                CliError::Config(format!("invalid [dev] section: {e}"))
            })?,
            None => RawDev::default(),
        };

        let defaults = Self::default();
        let config = Self {
            watch_enabled: raw.watcher.enabled.unwrap_or(defaults.watch_enabled),
            debounce: raw
                .watcher
                .debounce_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.debounce),
            extra_watch_paths: raw.watcher.watch_paths.unwrap_or_default(),
            exclude_paths: raw.watcher.exclude_paths.unwrap_or_default(),
            verbose: raw.watcher.verbose.unwrap_or(defaults.verbose),
            graceful_shutdown_timeout: raw
                .server
                .graceful_shutdown_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.graceful_shutdown_timeout),
            force_kill_timeout: raw
                .server
                .force_kill_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.force_kill_timeout),
        };
        config.validate()?;
        Ok(config)
    }

    fn apply(&mut self, overrides: &CliOverrides) {
        if overrides.verbose {
            self.verbose = true;
        }
        if let Some(ms) = overrides.debounce_ms {
            self.debounce = Duration::from_millis(ms);
        }
    }

    fn validate(&self) -> CliResult<()> {
        if self.debounce > Duration::from_secs(60) {
            return Err(CliError::Config(
                "dev.watcher.debounce_ms must be at most 60000".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_dev_section() {
        let config =
            DevConfig::from_toml_str("[project]\nname = \"demo\"\n[server]\nport = 3000\n")
                .unwrap();
        assert_eq!(config, DevConfig::default());
        assert_eq!(config.debounce, Duration::from_millis(400));
    }

    #[test]
    fn reads_dev_section() {
        let text = r#"
[project]
name = "demo"

[dev.watcher]
enabled = true
debounce_ms = 250
watch_paths = ["config"]
exclude_paths = ["src/generated"]
verbose = true

[dev.server]
graceful_shutdown_timeout_ms = 1000
force_kill_timeout_ms = 500
"#;
        let config = DevConfig::from_toml_str(text).unwrap();
        assert!(config.watch_enabled);
        assert_eq!(config.debounce, Duration::from_millis(250));
        assert_eq!(config.extra_watch_paths, vec![PathBuf::from("config")]);
        assert_eq!(config.exclude_paths, vec![PathBuf::from("src/generated")]);
        assert!(config.verbose);
        assert_eq!(config.graceful_shutdown_timeout, Duration::from_secs(1));
        assert_eq!(config.force_kill_timeout, Duration::from_millis(500));
    }

    #[test]
    fn watching_can_be_disabled() {
        let config = DevConfig::from_toml_str("[dev.watcher]\nenabled = false\n").unwrap();
        assert!(!config.watch_enabled);
    }

    #[test]
    fn rejects_unknown_keys_and_absurd_values() {
        assert!(DevConfig::from_toml_str("[dev.watcher]\ndebounce = 10\n").is_err());
        assert!(DevConfig::from_toml_str("[dev.watcher]\ndebounce_ms = 600000\n").is_err());
    }

    #[test]
    fn missing_file_means_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DevConfig::load(tmp.path(), &CliOverrides::default()).unwrap();
        assert_eq!(config, DevConfig::default());
    }

    #[test]
    fn cli_overrides_win() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[dev.watcher]\ndebounce_ms = 300\n",
        )
        .unwrap();
        let config = DevConfig::load(
            tmp.path(),
            &CliOverrides {
                verbose: true,
                debounce_ms: Some(100),
            },
        )
        .unwrap();
        assert!(config.verbose);
        assert_eq!(config.debounce, Duration::from_millis(100));
    }
}
