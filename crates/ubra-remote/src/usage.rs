//! Short-lived transcript collection. Never linked into the Holder event loop.
use std::{
    io,
    path::{Component, Path, PathBuf},
};
use ubra_proto::remote_pty::{
    EnvironmentCaptureRequest, EnvironmentVariable, TranscriptUsageRequest, TranscriptUsageResult,
};
use ubra_usage::transcripts::{ScanPaths, SystemClock, UsageProvider, UsageStore};

pub fn collect(
    executable: &Path,
    request: &TranscriptUsageRequest,
) -> io::Result<TranscriptUsageResult> {
    request.validate().map_err(io::Error::other)?;
    let cache_root = crate::paths::StatePaths::resolve()?.root.join("usage-v1");
    crate::paths::ensure_private_dir(&cache_root)?;
    let shell = crate::environment::account_shell()?;
    let process_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("remote usage HOME is unavailable"))?;
    let environment = login_environment(
        &cache_root.join(LOGIN_CACHE_FILE),
        &shell,
        &process_home,
        unix_now(),
        || {
            crate::environment::capture_with_shell(
                &EnvironmentCaptureRequest {
                    cwd: Some("~".into()),
                    timeout_millis: 5_000,
                },
                executable,
                &shell,
            )
            .map(|captured| captured.environment)
        },
    )?;
    let home = environment
        .iter()
        .find(|v| v.name == "HOME")
        .map(|v| PathBuf::from(&v.value))
        .ok_or_else(|| io::Error::other("remote usage HOME is unavailable"))?;
    let roots = roots(&home, &environment, request)?;
    collect_at(roots, &cache_root)
}

/// Login-environment values the scan needs, reused across five-minute
/// polls. Two login shells (account + `~`) dominate a warm poll on a host
/// with a real shell configuration, while only these values select roots.
const LOGIN_CACHE_FILE: &str = "login-environment.json";
const LOGIN_CACHE_VERSION: u32 = 1;
/// Bounds staleness for startup files the stamps cannot see (a sourced
/// dotfile, `ZDOTDIR` elsewhere, an edited `conf.d` entry).
const LOGIN_CACHE_TTL_SECONDS: u64 = 60 * 60;
const LOGIN_CACHE_MAX_BYTES: u64 = 64 * 1024;
const LOGIN_VARIABLES: [&str; 3] = ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];
/// Startup files of the shells a login capture runs, relative to HOME.
const HOME_STARTUP_FILES: [&str; 12] = [
    ".profile",
    ".bash_profile",
    ".bash_login",
    ".bashrc",
    ".zshenv",
    ".zprofile",
    ".zshrc",
    ".zlogin",
    ".pam_environment",
    ".config/fish/config.fish",
    ".config/fish/conf.d",
    ".config/fish/fish_variables",
];
const SYSTEM_STARTUP_FILES: [&str; 15] = [
    "/etc/profile",
    "/etc/profile.d",
    "/etc/environment",
    "/etc/bash.bashrc",
    "/etc/bashrc",
    "/etc/zshenv",
    "/etc/zprofile",
    "/etc/zshrc",
    "/etc/zlogin",
    "/etc/zsh/zshenv",
    "/etc/zsh/zprofile",
    "/etc/zsh/zshrc",
    "/etc/zsh/zlogin",
    "/etc/fish/config.fish",
    "/etc/fish/conf.d",
];

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct LoginCache {
    version: u32,
    shell: PathBuf,
    home: PathBuf,
    captured_at: u64,
    startup_files: Vec<StartupStamp>,
    environment: Vec<EnvironmentVariable>,
}

/// `None` records an absent file, so creating one invalidates the cache.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct StartupStamp {
    path: PathBuf,
    /// `(dev, ino, mtime, mtime_nsec, len)`.
    stamp: Option<(u64, u64, i64, i64, u64)>,
}

fn startup_stamps(home: &Path) -> Vec<StartupStamp> {
    use std::os::unix::fs::MetadataExt as _;
    HOME_STARTUP_FILES
        .iter()
        .map(|relative| home.join(relative))
        .chain(SYSTEM_STARTUP_FILES.iter().map(PathBuf::from))
        .map(|path| StartupStamp {
            // Following a symlinked dotfile is intended: only metadata is read.
            stamp: std::fs::metadata(&path).ok().map(|metadata| {
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.len(),
                )
            }),
            path,
        })
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Returns the login environment's usage-relevant values: from the cache
/// when its shell, HOME, startup-file stamps and age all still match, else
/// from a fresh `capture` whose result replaces the cache. Stamps are taken
/// before capturing, so an edit racing the capture invalidates next time.
fn login_environment(
    cache_file: &Path,
    shell: &Path,
    home: &Path,
    now: u64,
    capture: impl FnOnce() -> io::Result<Vec<EnvironmentVariable>>,
) -> io::Result<Vec<EnvironmentVariable>> {
    let startup_files = startup_stamps(home);
    if let Some(cached) = read_login_cache(cache_file)
        && cached.version == LOGIN_CACHE_VERSION
        && cached.shell == shell
        && cached.home == home
        && now >= cached.captured_at
        && now - cached.captured_at < LOGIN_CACHE_TTL_SECONDS
        && cached.startup_files == startup_files
    {
        return Ok(cached.environment);
    }
    let environment = capture()?
        .into_iter()
        .filter(|variable| LOGIN_VARIABLES.contains(&variable.name.as_str()))
        .collect::<Vec<_>>();
    // A cache that cannot be written only costs the next poll a capture.
    let _ = write_login_cache(
        cache_file,
        &LoginCache {
            version: LOGIN_CACHE_VERSION,
            shell: shell.to_path_buf(),
            home: home.to_path_buf(),
            captured_at: now,
            startup_files,
            environment: environment.clone(),
        },
    );
    Ok(environment)
}

fn read_login_cache(path: &Path) -> Option<LoginCache> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > LOGIN_CACHE_MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_login_cache(path: &Path, cache: &LoginCache) -> io::Result<()> {
    use std::io::Write;
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("login cache has no parent"))?;
    let temporary = directory.join(format!(
        ".{LOGIN_CACHE_FILE}.{}.tmp",
        crate::state::random_hex(8)?
    ));
    let result = crate::paths::create_private_file(&temporary).and_then(|mut file| {
        file.write_all(&serde_json::to_vec(cache).map_err(io::Error::other)?)?;
        crate::paths::reject_symlink(path)?;
        std::fs::rename(&temporary, path)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn roots(
    home: &Path,
    environment: &[EnvironmentVariable],
    request: &TranscriptUsageRequest,
) -> io::Result<Vec<(PathBuf, UsageProvider)>> {
    let mut roots = ScanPaths::for_home(home).roots;
    roots.push((home.join(".codex/archived_sessions"), UsageProvider::Codex));
    for (name, provider, suffixes) in [
        (
            "CLAUDE_CONFIG_DIR",
            UsageProvider::Claude,
            &["projects"][..],
        ),
        (
            "CODEX_HOME",
            UsageProvider::Codex,
            &["sessions", "archived_sessions"][..],
        ),
    ] {
        if let Some(value) = environment
            .iter()
            .find(|v| v.name == name && !v.value.is_empty())
        {
            let base = PathBuf::from(&value.value);
            if !base.is_absolute() || base.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(io::Error::other(
                    "remote usage provider directory must be absolute",
                ));
            }
            for suffix in suffixes {
                roots.push((base.join(suffix), provider));
            }
        }
    }
    for profile in &request.profiles {
        let base = profile.config_home.strip_prefix("~/").map_or_else(
            || PathBuf::from(&profile.config_home),
            |suffix| home.join(suffix),
        );
        match profile.provider.as_str() {
            "claude" => roots.push((base.join("projects"), UsageProvider::Claude)),
            "codex" => {
                roots.push((base.join("sessions"), UsageProvider::Codex));
                roots.push((base.join("archived_sessions"), UsageProvider::Codex));
            }
            _ => return Err(io::Error::other("invalid usage provider")),
        }
    }
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    roots.dedup();
    // Validate every existing ancestor, not just the projects/sessions leaf.
    for (path, _) in &roots {
        for ancestor in path.ancestors() {
            crate::paths::reject_symlink(ancestor)?;
        }
    }
    Ok(roots)
}

fn collect_at(
    roots: Vec<(PathBuf, UsageProvider)>,
    cache_root: &Path,
) -> io::Result<TranscriptUsageResult> {
    crate::paths::ensure_private_dir(cache_root)?;
    let _lock = crate::state::acquire_lock(&cache_root.join("scan.lock"))
        .map_err(|_| io::Error::other("remote usage scan is already running"))?;
    let identity = cache_root.join("source-id");
    crate::paths::reject_symlink(&identity)?;
    let source_id = if identity.exists() {
        use std::io::Read;
        let mut value = String::new();
        std::fs::File::open(&identity)?
            .take(33)
            .read_to_string(&mut value)?;
        value
    } else {
        use std::io::Write;
        let value = crate::state::random_hex(16)?;
        let mut file = crate::paths::create_private_file(&identity)?;
        file.write_all(value.as_bytes())?;
        value
    };
    let cache_file = cache_root.join("transcripts.json");
    crate::paths::reject_symlink(&cache_file)?;
    if let Ok(metadata) = std::fs::metadata(&cache_file)
        && (!metadata.is_file() || metadata.len() > 64 * 1024 * 1024)
    {
        return Err(io::Error::other("remote usage cache size limit exceeded"));
    }
    let mut store = UsageStore::with_paths_and_clock(ScanPaths { roots, cache_file }, SystemClock);
    let snapshot = store.refresh_remote()?;
    snapshot
        .history
        .remote_summary(snapshot.updated_at, source_id)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn collector_reuses_the_shared_ledger_and_returns_only_usage() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let transcripts = root.join("sessions");
        std::fs::create_dir(&transcripts).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Derive a current ISO date from the public dashboard projection.
        let date = ubra_usage::transcripts::dashboard::date_label((now / 86_400) as i64);
        let context = serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.4"}});
        let usage = serde_json::json!({"timestamp":format!("{date}T12:00:00Z"),"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}}});
        std::fs::write(
            transcripts.join("a.jsonl"),
            format!("{context}\n{usage}\n{{\"prompt\":\"DO_NOT_EXPORT\"}}\n"),
        )
        .unwrap();
        let cache = root.join("cache");
        let roots = vec![(transcripts, UsageProvider::Codex)];
        let first = collect_at(roots.clone(), &cache).unwrap();
        let second = collect_at(roots, &cache).unwrap();
        assert_eq!(first.buckets, second.buckets);
        assert_eq!(first.buckets.len(), 1);
        assert_eq!(first.buckets[0].input, 60);
        assert_eq!(first.buckets[0].cache_read, 40);
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("DO_NOT_EXPORT")
        );
        assert_eq!(
            std::fs::metadata(cache.join("transcripts.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn provider_overrides_are_remote_and_symlinks_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let custom = home.join("work");
        let env = vec![EnvironmentVariable {
            name: "CODEX_HOME".into(),
            value: custom.to_str().unwrap().into(),
        }];
        let paths = roots(&home, &env, &TranscriptUsageRequest::default()).unwrap();
        assert!(paths.contains(&(custom.join("sessions"), UsageProvider::Codex)));
        std::os::unix::fs::symlink(&custom, home.join(".codex")).unwrap();
        assert!(roots(&home, &env, &TranscriptUsageRequest::default()).is_err());
    }

    #[test]
    fn login_environment_is_reused_until_a_startup_file_shell_or_age_changes() {
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let cache = home.join("state").join(LOGIN_CACHE_FILE);
        std::fs::create_dir(home.join("state")).unwrap();
        let shell = Path::new("/bin/zsh");
        let captures = std::cell::Cell::new(0);
        let variable = |name: &str, value: &str| EnvironmentVariable {
            name: name.into(),
            value: value.into(),
        };
        let capture = |codex: &'static str| {
            let captures = &captures;
            let home = home.clone();
            move || {
                captures.set(captures.get() + 1);
                Ok(vec![
                    variable("HOME", home.to_str().unwrap()),
                    variable("CODEX_HOME", codex),
                    variable("SECRET_TOKEN", "never-cached"),
                ])
            }
        };
        let now = 1_790_000_000;
        let first = login_environment(&cache, shell, &home, now, capture("/a")).unwrap();
        assert_eq!(captures.get(), 1);
        assert!(first.iter().all(|v| v.name != "SECRET_TOKEN"));
        assert!(
            !std::fs::read_to_string(&cache)
                .unwrap()
                .contains("never-cached")
        );
        assert_eq!(
            std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Warm poll: no login shell runs and the values are identical.
        let warm = login_environment(&cache, shell, &home, now + 300, capture("/b")).unwrap();
        assert_eq!((captures.get(), &warm), (1, &first));

        // Editing (here: creating) a startup file forces a fresh capture.
        std::fs::write(home.join(".zshrc"), "export CODEX_HOME=/b\n").unwrap();
        let edited = login_environment(&cache, shell, &home, now + 301, capture("/b")).unwrap();
        assert_eq!(captures.get(), 2);
        assert!(edited.contains(&variable("CODEX_HOME", "/b")));
        login_environment(&cache, shell, &home, now + 302, capture("/c")).unwrap();
        assert_eq!(captures.get(), 2);

        // A different account shell, an expired entry, or a clock step back.
        login_environment(
            &cache,
            Path::new("/bin/bash"),
            &home,
            now + 303,
            capture("/c"),
        )
        .unwrap();
        assert_eq!(captures.get(), 3);
        login_environment(
            &cache,
            Path::new("/bin/bash"),
            &home,
            now + 303 + LOGIN_CACHE_TTL_SECONDS,
            capture("/c"),
        )
        .unwrap();
        assert_eq!(captures.get(), 4);
        login_environment(&cache, Path::new("/bin/bash"), &home, now, capture("/c")).unwrap();
        assert_eq!(captures.get(), 5);

        // A failed capture is an error, never a stale or empty environment.
        std::fs::write(home.join(".zshrc"), "changed\n").unwrap();
        let failed = login_environment(&cache, Path::new("/bin/bash"), &home, now + 1, || {
            Err(io::Error::other("capture timed out"))
        });
        assert!(failed.is_err());

        // A symlinked cache is never read or replaced through.
        std::fs::remove_file(&cache).unwrap();
        let elsewhere = home.join("elsewhere.json");
        std::fs::write(&elsewhere, "{}").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &cache).unwrap();
        login_environment(&cache, shell, &home, now, capture("/d")).unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "{}");
    }
}
