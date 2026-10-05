//! Platform path construction and shared environment names.
//!
//! This module is the seam for every local process. The GUI, Engine, Holder,
//! CLI, and MCP bridge must derive the same endpoints without knowing which
//! desktop convention produced them.

use std::path::{Path, PathBuf};

pub const APP_SUPPORT_RELATIVE_PATH: &str = "Library/Application Support/Ubra";
pub const LINUX_DATA_RELATIVE_PATH: &str = ".local/share/ubra";
pub const LINUX_STATE_RELATIVE_PATH: &str = ".local/state/ubra";
pub const LINUX_CONFIG_RELATIVE_PATH: &str = ".config/ubra";
pub const LINUX_CACHE_RELATIVE_PATH: &str = ".cache/ubra";
pub const SOCKET_FILE_NAME: &str = "daemon.sock";
pub const STATE_FILE_NAME: &str = "state.json";
pub const LOGS_DIR_NAME: &str = "logs";
pub const INJECT_DIR_NAME: &str = "inject";
pub const BIN_DIR_NAME: &str = "bin";
pub const MANIFEST_OVERRIDES_RELATIVE_PATH: &str = "manifests/overrides";
pub const DAEMON_LOG_FILE_NAME: &str = "ubrad.log";
pub const HOSTS_CONFIG_FILE_NAME: &str = "hosts.json";
pub const PREFS_FILE_NAME: &str = "prefs.json";
pub const UBRA_INCLUDE_FILE_NAME: &str = ".ubra-include";
pub const QUICK_OPEN_CACHE_FILE_NAME: &str = "quick-open-index.json";
pub const USAGE_CACHE_FILE_NAME: &str = "usage-cache.json";
pub const ACTIVITY_LOG_FILE_NAME: &str = "activity-log.jsonl";
pub const SESSION_RECOVERY_DIR_NAME: &str = "sessions";

pub const ENV_SESSION_ID: &str = "UBRA_SESSION_ID";
pub const ENV_SOCKET: &str = "UBRA_SOCKET";
pub const ENV_CLI: &str = "UBRA_CLI";
pub const ENV_APP_SUPPORT: &str = "UBRA_APP_SUPPORT";
/// Exact per-session recovery directory exported to hook/notify shims.
///
/// The directory is chosen by the Engine rather than reconstructed by the CLI,
/// so macOS and XDG layouts share one contract and tests never need HOME.
pub const ENV_SESSION_RECOVERY_DIR: &str = "UBRA_SESSION_RECOVERY_DIR";
/// The notes directory, exported by the Engine to every Agent it starts so
/// the MCP bridge and CLI read and write the notes the Engine serves, not a
/// directory they reconstruct from their own environment.
pub const ENV_NOTES_DIR: &str = "UBRA_NOTES_DIR";

/// Passed by a launcher that spawns the Engine detached and expects to ask it
/// to leave when it quits. With it, an Engine left with no live session and no
/// client retires itself; without it, as under a service manager, it stays up.
pub const EXIT_WHEN_ORPHANED_FLAG: &str = "--exit-when-orphaned";

pub struct UbraPaths;

impl UbraPaths {
    /// Platform package resources derived from one installed executable.
    /// Loose development binaries may not have this directory; callers can
    /// continue to their source-tree fallback in that case.
    pub fn packaged_resources(executable: impl AsRef<Path>) -> PathBuf {
        let executable = executable.as_ref();
        let executable_dir = executable.parent().unwrap_or_else(|| Path::new("."));

        #[cfg(target_os = "macos")]
        {
            if executable_dir.file_name().and_then(|name| name.to_str()) == Some("MacOS")
                && let Some(contents) = executable_dir.parent()
            {
                return contents.join("Resources");
            }
            if executable_dir.file_name().and_then(|name| name.to_str()) == Some("bin")
                && let Some(resources) = executable_dir.parent()
                && resources.file_name().and_then(|name| name.to_str()) == Some("Resources")
            {
                return resources.to_path_buf();
            }
        }

        #[cfg(target_os = "linux")]
        if executable_dir.file_name().and_then(|name| name.to_str()) == Some("bin")
            && let Some(prefix) = executable_dir.parent()
        {
            return prefix.join("lib/ubra");
        }

        executable_dir.to_path_buf()
    }

    pub fn app_support(home: impl AsRef<Path>) -> PathBuf {
        layout(home.as_ref()).data
    }

    pub fn state_dir(home: impl AsRef<Path>) -> PathBuf {
        layout(home.as_ref()).state
    }

    pub fn config_dir(home: impl AsRef<Path>) -> PathBuf {
        layout(home.as_ref()).config
    }

    pub fn runtime_dir(home: impl AsRef<Path>) -> PathBuf {
        layout(home.as_ref()).runtime
    }

    pub fn cache_dir(home: impl AsRef<Path>) -> PathBuf {
        layout(home.as_ref()).cache
    }

    pub fn socket(home: impl AsRef<Path>) -> PathBuf {
        Self::runtime_dir(home).join(SOCKET_FILE_NAME)
    }

    pub fn state_file(home: impl AsRef<Path>) -> PathBuf {
        Self::state_dir(home).join(STATE_FILE_NAME)
    }

    pub fn logs_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::state_dir(home).join(LOGS_DIR_NAME)
    }

    pub fn session_recovery_root(home: impl AsRef<Path>) -> PathBuf {
        Self::state_dir(home).join(SESSION_RECOVERY_DIR_NAME)
    }

    pub fn inject_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(INJECT_DIR_NAME)
    }

    pub fn bin_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(BIN_DIR_NAME)
    }

    pub fn manifest_overrides_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::config_dir(home).join(MANIFEST_OVERRIDES_RELATIVE_PATH)
    }

    pub fn daemon_log_file(home: impl AsRef<Path>) -> PathBuf {
        Self::logs_dir(home).join(DAEMON_LOG_FILE_NAME)
    }

    pub fn hosts_config_file(home: impl AsRef<Path>) -> PathBuf {
        Self::config_dir(home).join(HOSTS_CONFIG_FILE_NAME)
    }

    pub fn prefs_file(home: impl AsRef<Path>) -> PathBuf {
        let home = home.as_ref();
        #[cfg(target_os = "macos")]
        if let Some(root) = absolute_env_path(ENV_APP_SUPPORT) {
            return root.join(PREFS_FILE_NAME);
        }
        #[cfg(target_os = "macos")]
        return home
            .join("Library/Application Support/ubra")
            .join(PREFS_FILE_NAME);
        #[cfg(not(target_os = "macos"))]
        Self::config_dir(home).join(PREFS_FILE_NAME)
    }

    /// Gitignore-style extra folders for ⌘P. Lives in `$HOME` so it can be
    /// edited like a user gitignore, including from Settings.
    pub fn ubra_include_file(home: impl AsRef<Path>) -> PathBuf {
        home.as_ref().join(UBRA_INCLUDE_FILE_NAME)
    }

    pub fn quick_open_cache_file(home: impl AsRef<Path>) -> PathBuf {
        let home = home.as_ref();
        #[cfg(target_os = "macos")]
        if let Some(root) = absolute_env_path(ENV_APP_SUPPORT) {
            return root.join(QUICK_OPEN_CACHE_FILE_NAME);
        }
        #[cfg(target_os = "macos")]
        return home
            .join("Library/Application Support/ubra")
            .join(QUICK_OPEN_CACHE_FILE_NAME);
        #[cfg(not(target_os = "macos"))]
        Self::cache_dir(home).join(QUICK_OPEN_CACHE_FILE_NAME)
    }

    pub fn usage_cache_file(home: impl AsRef<Path>) -> PathBuf {
        let home = home.as_ref();
        #[cfg(target_os = "macos")]
        if let Some(root) = absolute_env_path(ENV_APP_SUPPORT) {
            return root.join(USAGE_CACHE_FILE_NAME);
        }
        #[cfg(target_os = "macos")]
        return home
            .join("Library/Application Support/ubra")
            .join(USAGE_CACHE_FILE_NAME);
        #[cfg(not(target_os = "macos"))]
        Self::cache_dir(home).join(USAGE_CACHE_FILE_NAME)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathLayout {
    data: PathBuf,
    state: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
    cache: PathBuf,
}

fn layout(home: &Path) -> PathLayout {
    if let Some(root) = absolute_env_path(ENV_APP_SUPPORT) {
        return single_root(root);
    }

    #[cfg(target_os = "macos")]
    {
        single_root(home.join(APP_SUPPORT_RELATIVE_PATH))
    }

    #[cfg(target_os = "linux")]
    {
        linux_layout(
            home,
            absolute_env_path("XDG_DATA_HOME"),
            absolute_env_path("XDG_STATE_HOME"),
            absolute_env_path("XDG_CONFIG_HOME"),
            absolute_env_path("XDG_RUNTIME_DIR"),
            absolute_env_path("XDG_CACHE_HOME"),
        )
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        single_root(home.join(".ubra"))
    }
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn single_root(root: PathBuf) -> PathLayout {
    PathLayout {
        data: root.clone(),
        state: root.clone(),
        config: root.clone(),
        runtime: root.clone(),
        cache: root,
    }
}

#[cfg(target_os = "linux")]
fn linux_layout(
    home: &Path,
    data_home: Option<PathBuf>,
    state_home: Option<PathBuf>,
    config_home: Option<PathBuf>,
    runtime_home: Option<PathBuf>,
    cache_home: Option<PathBuf>,
) -> PathLayout {
    let data = data_home
        .unwrap_or_else(|| home.join(".local/share"))
        .join("ubra");
    let state = state_home
        .unwrap_or_else(|| home.join(".local/state"))
        .join("ubra");
    let config = config_home
        .unwrap_or_else(|| home.join(".config"))
        .join("ubra");
    let runtime = runtime_home
        .map(|root| root.join("ubra"))
        .unwrap_or_else(|| state.join("run"));
    let cache = cache_home
        .unwrap_or_else(|| home.join(".cache"))
        .join("ubra");
    PathLayout {
        data,
        state,
        config,
        runtime,
        cache,
    }
}

pub struct UbraEnv;

impl UbraEnv {
    pub const SESSION_ID: &'static str = ENV_SESSION_ID;
    pub const SOCKET: &'static str = ENV_SOCKET;
    pub const CLI: &'static str = ENV_CLI;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn linux_layout_follows_xdg_roots_and_keeps_runtime_separate() {
        let paths = linux_layout(
            Path::new("/home/dev"),
            Some(PathBuf::from("/data")),
            Some(PathBuf::from("/state")),
            Some(PathBuf::from("/config")),
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/cache")),
        );
        assert_eq!(paths.data, Path::new("/data/ubra"));
        assert_eq!(paths.state, Path::new("/state/ubra"));
        assert_eq!(paths.config, Path::new("/config/ubra"));
        assert_eq!(paths.runtime, Path::new("/run/user/1000/ubra"));
        assert_eq!(paths.cache, Path::new("/cache/ubra"));
    }

    #[test]
    fn linux_layout_has_spec_compliant_home_fallbacks() {
        let paths = linux_layout(Path::new("/home/dev"), None, None, None, None, None);
        assert_eq!(paths.data, Path::new("/home/dev/.local/share/ubra"));
        assert_eq!(paths.state, Path::new("/home/dev/.local/state/ubra"));
        assert_eq!(paths.config, Path::new("/home/dev/.config/ubra"));
        assert_eq!(paths.runtime, Path::new("/home/dev/.local/state/ubra/run"));
        assert_eq!(paths.cache, Path::new("/home/dev/.cache/ubra"));
    }

    #[test]
    fn linux_package_resources_follow_the_usr_prefix() {
        assert_eq!(
            UbraPaths::packaged_resources("/opt/ubra/usr/bin/ubrad-rs"),
            Path::new("/opt/ubra/usr/lib/ubra")
        );
    }
}
