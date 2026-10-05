//! Whether an installed Agent already has a login on this Mac.
//!
//! A newcomer's first Claude Code or Codex launch opens on the CLI's own
//! sign-in screens. Knowing that ahead of time lets the welcome say "Sign in
//! to Claude Code" instead of promising a session that starts with a login
//! menu. The answer comes from the same stores the CLIs read, without reading
//! a secret: file existence, a Keychain item's attributes, and non-secret
//! JSON fields.
//!
//! It is deliberately one-sided. Any sign of another way to authenticate
//! (an API key in the environment, a key helper, or a cloud provider) makes
//! the answer `None`, so a signed-in user is never told to sign in.
use super::*;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment that authenticates Claude Code without its OAuth store.
const CLAUDE_ALTERNATE_AUTH: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
];

/// Environment that authenticates Codex without `auth.json`, or moves it.
const CODEX_ALTERNATE_AUTH: &[&str] = &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_HOME"];

/// Files the probe reads are small JSON; anything larger is not ours to parse.
const LIMIT: u64 = 1024 * 1024;

impl ControlServer {
    /// The sign-in fact for one local Agent, or `None` when Ubra cannot tell.
    pub(super) fn agent_signed_in(&self, id: &str) -> Option<bool> {
        let home = PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?);
        let env = |key: &str| std::env::var_os(key).is_some_and(|value| !value.is_empty());
        match id {
            ubra_proto::AgentKind::CLAUDE_CODE_ID => {
                claude_signed_in(&home, &env, default_login_present)
            }
            ubra_proto::AgentKind::CODEX_ID => codex_signed_in(&home, &env),
            _ => None,
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn claude_signed_in(
    home: &Path,
    env: &dyn Fn(&str) -> bool,
    keychain_login: impl FnOnce(&Path) -> Option<bool>,
) -> Option<bool> {
    if CLAUDE_ALTERNATE_AUTH.iter().any(|key| env(key)) {
        return None;
    }
    let config_home = home.join(".claude");
    if let Some(global) = read_json(&home.join(".claude.json")) {
        // `oauthAccount` is the identity `/status` shows once signed in, and
        // `primaryApiKey` is a Console key Claude stored itself.
        if global
            .get("oauthAccount")
            .is_some_and(|value| !value.is_null())
            || global
                .get("primaryApiKey")
                .is_some_and(|value| !value.is_null())
        {
            return Some(true);
        }
    }
    if let Some(settings) = read_json(&config_home.join("settings.json")) {
        let routed = settings.get("apiKeyHelper").is_some()
            || settings
                .get("env")
                .and_then(|env| env.as_object())
                .is_some_and(|env| {
                    CLAUDE_ALTERNATE_AUTH
                        .iter()
                        .any(|key| env.contains_key(*key))
                });
        if routed {
            return None;
        }
    }
    keychain_login(&config_home)
}

fn codex_signed_in(home: &Path, env: &dyn Fn(&str) -> bool) -> Option<bool> {
    if CODEX_ALTERNATE_AUTH.iter().any(|key| env(key)) {
        return None;
    }
    let codex_home = home.join(".codex");
    // A keyring credential store keeps the login out of auth.json.
    if fs::read_to_string(codex_home.join("config.toml"))
        .is_ok_and(|config| config.contains("cli_auth_credentials_store"))
    {
        return None;
    }
    if let Some(auth) = read_json(&codex_home.join("auth.json")) {
        let present = |value: &serde_json::Value| value.as_str().is_some_and(|s| !s.is_empty());
        return Some(present(&auth["OPENAI_API_KEY"]) || present(&auth["tokens"]["refresh_token"]));
    }
    Some(false)
}

const SECURITY: &str = "/usr/bin/security";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const NOT_FOUND: i32 = 44;

fn failure() -> ControlError {
    ControlError::bad_request(
        "Cannot safely read the Claude login. Check file ownership and permissions.",
    )
}

/// Whether the ambient Claude login exists: the credentials file or the
/// default Keychain entry. `None` means the check itself failed.
fn default_login_present(config_home: &Path) -> Option<bool> {
    has_login(config_home).ok()
}

/// Whether the default store holds a login, without reading any secret.
/// Claude Code falls back to the credentials file when the Keychain is
/// unavailable, so a file counts on every platform; on macOS the Keychain
/// item is the norm.
fn has_login(config_home: &Path) -> Result<bool, ControlError> {
    if read_private(&config_home.join(".credentials.json"))?.is_some() {
        return Ok(true);
    }
    if cfg!(target_os = "macos") {
        let (code, _) = security(
            &[
                "find-generic-password",
                "-a",
                &keychain_account(),
                "-s",
                KEYCHAIN_SERVICE,
            ],
            None,
        )?;
        return match code {
            0 => Ok(true),
            NOT_FOUND => Ok(false),
            _ => Err(ControlError::bad_request(
                "The Keychain refused the lookup. Unlock the login keychain and retry.",
            )),
        };
    }
    Ok(false)
}

fn keychain_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "claude-code-user".to_owned())
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>, ControlError> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure()),
    };
    let m = file.metadata().map_err(|_| failure())?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.len() > LIMIT {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure());
    }
    Ok(Some(bytes))
}

fn security(args: &[&str], stdin: Option<&str>) -> Result<(i32, String), ControlError> {
    let mut command = Command::new(SECURITY);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| failure())?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
        && pipe.write_all(input.as_bytes()).is_err()
    {
        drop(pipe);
        kill_and_reap(&mut child);
        return Err(failure());
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    match wait_until(&mut child, deadline, Duration::from_millis(25)) {
        Ok(Some(status)) => {
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut out);
            }
            Ok((status.code().unwrap_or(1), out))
        }
        Ok(None) => Err(ControlError::bad_request(
            "The Keychain did not answer. Unlock the login keychain and retry.",
        )),
        Err(_) => Err(failure()),
    }
}

/// Waits for `child` until `deadline`. `Ok(None)` is a timeout.
///
/// Whatever the outcome, the child has been reaped by the time this returns:
/// the Engine lives for days, and a helper that was killed but never waited
/// for stays in the process table as a zombie for all of them.
fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() > deadline => {
                kill_and_reap(child);
                return Ok(None);
            }
            Ok(None) => std::thread::sleep(poll),
            Err(error) => {
                kill_and_reap(child);
                return Err(error);
            }
        }
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn a_fresh_mac_reads_as_signed_out_for_both_agents() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            claude_signed_in(home.path(), &none, |_| Some(false)),
            Some(false)
        );
        assert_eq!(codex_signed_in(home.path(), &none), Some(false));
    }

    #[test]
    fn claude_logins_are_found_in_the_keychain_or_its_global_config() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            claude_signed_in(home.path(), &none, |_| Some(true)),
            Some(true)
        );
        fs::write(
            home.path().join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"a@example.com"}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_signed_in(home.path(), &none, |_| Some(false)),
            Some(true)
        );
    }

    #[test]
    fn any_other_way_to_authenticate_claude_is_never_called_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let api_key = |key: &str| key == "ANTHROPIC_API_KEY";
        assert_eq!(
            claude_signed_in(home.path(), &api_key, |_| Some(false)),
            None
        );
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#,
        )
        .unwrap();
        assert_eq!(claude_signed_in(home.path(), &none, |_| Some(false)), None);
        assert_eq!(
            claude_signed_in(home.path(), &none, |_| None),
            None,
            "a Keychain that would not answer is unknown, not signed out"
        );
    }

    #[test]
    fn codex_reads_auth_json_and_stays_unknown_for_a_keyring_store() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("auth.json"),
            r#"{"tokens":{"access_token":"a","refresh_token":"r"}}"#,
        )
        .unwrap();
        assert_eq!(codex_signed_in(home.path(), &none), Some(true));
        fs::write(codex.join("auth.json"), r#"{"OPENAI_API_KEY":null}"#).unwrap();
        assert_eq!(codex_signed_in(home.path(), &none), Some(false));
        fs::write(
            codex.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        assert_eq!(codex_signed_in(home.path(), &none), None);
        let key = |key: &str| key == "OPENAI_API_KEY";
        fs::remove_file(codex.join("config.toml")).unwrap();
        assert_eq!(codex_signed_in(home.path(), &key), None);
    }
}
