//! Structured classification of OpenSSH's own failures (exit status 255).
//!
//! OpenSSH reports connect, authentication, host-key and configuration
//! failures only as free text on stderr. That text names hosts, users and
//! paths, so it never reaches telemetry: it is classified here, locally, and
//! only the class leaves the process. The user sees an actionable message
//! plus OpenSSH's own last line, which stays on this Mac.

use std::fmt;
use std::io;

/// One class of OpenSSH failure. Each maps to a stable control error code
/// that the app can key copy on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SshFailureClass {
    UnresolvedHost,
    Refused,
    Unreachable,
    Timeout,
    AuthFailed,
    HostKey,
    HostKeyChanged,
    ConnectionClosed,
    Config,
    ControlMaster,
    Other,
}

impl SshFailureClass {
    /// Wire code for `ControlError::code`, also the telemetry `class`.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnresolvedHost => "ssh_unresolved_host",
            Self::Refused => "ssh_refused",
            Self::Unreachable => "ssh_unreachable",
            Self::Timeout => "ssh_timeout",
            Self::AuthFailed => "ssh_auth_failed",
            Self::HostKey => "ssh_host_key",
            Self::HostKeyChanged => "ssh_host_key_changed",
            Self::ConnectionClosed => "ssh_connection_closed",
            Self::Config => "ssh_config",
            Self::ControlMaster => "ssh_control_master",
            Self::Other => "ssh_failed",
        }
    }

    /// What went wrong and what to check, without host-specific detail.
    #[must_use]
    pub const fn advice(self) -> &'static str {
        match self {
            Self::UnresolvedHost => {
                "SSH could not resolve the host name. Check the SSH destination for typos, or add a matching Host entry to ~/.ssh/config."
            }
            Self::Refused => {
                "The host refused the SSH connection. Check that sshd is running there and that the port is right (Port in ~/.ssh/config)."
            }
            Self::Unreachable => {
                "The host is not reachable from this Mac. Check the network, VPN or Tailscale connection, and that the host is awake."
            }
            Self::Timeout => {
                "The SSH connection timed out. Check that the host is online and reachable, and that no firewall is blocking the SSH port."
            }
            Self::AuthFailed => {
                "SSH authentication was rejected. Check the user name and that your key is loaded (ssh-add -l) or listed as IdentityFile in ~/.ssh/config; try `ssh <host>` in a terminal."
            }
            Self::HostKey => {
                "The host key could not be verified. Connect once with `ssh <host>` in a terminal to review and accept the host key, then retry."
            }
            Self::HostKeyChanged => {
                "The host's key changed since you last connected. If that is expected (the host was reinstalled), remove the old key with `ssh-keygen -R <host>`; otherwise do not connect."
            }
            Self::ConnectionClosed => {
                "The host closed the SSH connection during setup. Check sshd's AllowUsers/MaxStartups settings and any fail2ban rules, then retry."
            }
            Self::Config => {
                "OpenSSH rejected its configuration. Check ~/.ssh/config for a bad option, and that it and your keys are owned by you and not group- or world-writable."
            }
            Self::ControlMaster => {
                "SSH's shared connection to this host closed mid-request. Retry; ubra opens a fresh connection."
            }
            Self::Other => {
                "SSH failed before reaching the host. Try `ssh <host>` in a terminal to see what OpenSSH reports."
            }
        }
    }

    /// Classifies OpenSSH's stderr. The last recognisable line wins: earlier
    /// lines are commonly benign warnings (multiplexing fallbacks, "added
    /// host to known hosts").
    #[must_use]
    pub fn classify(stderr: &[u8]) -> Self {
        let text = String::from_utf8_lossy(stderr);
        let class = text
            .lines()
            .rev()
            .find_map(classify_line)
            .unwrap_or(Self::Other);
        // A changed key ends with the same "verification failed" line as an
        // unknown one; the banner above it is what the user must act on.
        if class == Self::HostKey
            && text
                .lines()
                .any(|l| classify_line(l) == Some(Self::HostKeyChanged))
        {
            return Self::HostKeyChanged;
        }
        class
    }
}

fn classify_line(line: &str) -> Option<SshFailureClass> {
    use SshFailureClass as C;
    let line = line.trim();
    if line.is_empty() || is_benign(line) {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("identification has changed") || has("host key for") && has("has changed") {
        return Some(C::HostKeyChanged);
    }
    if has("host key verification failed")
        || has("host key is known for")
        || has("no matching host key type")
    {
        return Some(C::HostKey);
    }
    if has("could not resolve hostname")
        || has("nodename nor servname")
        || has("name or service not known")
        || has("temporary failure in name resolution")
    {
        return Some(C::UnresolvedHost);
    }
    if has("permission denied (")
        || has("too many authentication failures")
        || has("no more authentication methods")
        || has("authentication failed")
    {
        return Some(C::AuthFailed);
    }
    if has("bad configuration option")
        || has("bad owner or permissions")
        || has("bad configuration options")
        || has("can't open user config file")
        || has("controlpath too long")
        || has("unsupported option")
        || has("keyword") && has("extra arguments")
    {
        return Some(C::Config);
    }
    if has("mux_client") || has("control socket") || has("controlsocket") || has("multiplex") {
        return Some(C::ControlMaster);
    }
    if has("connection refused") {
        return Some(C::Refused);
    }
    if has("timed out") {
        return Some(C::Timeout);
    }
    if has("no route to host") || has("network is unreachable") || has("host is down") {
        return Some(C::Unreachable);
    }
    if has("connection closed by")
        || has("connection reset by")
        || has("kex_exchange_identification")
        || has("broken pipe")
    {
        return Some(C::ConnectionClosed);
    }
    None
}

/// OpenSSH lines that accompany a successful or recoverable connection and
/// must not decide the class of the real failure that follows them.
fn is_benign(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("warning: permanently added")
        || lower.contains("disabling multiplexing")
        // "Control socket connect(<path>): Connection refused" is a stale
        // master; ssh unlinks it and connects directly.
        || lower.starts_with("control socket connect")
}

/// An OpenSSH-level failure: carried inside the `io::Error` the executor
/// returns so the control layer can map it to a structured code.
#[derive(Debug)]
pub struct SshFailure {
    pub class: SshFailureClass,
    phase: &'static str,
    /// OpenSSH's last non-empty stderr line, for the local UI only.
    detail: Option<String>,
}

impl SshFailure {
    #[must_use]
    pub fn new(class: SshFailureClass, phase: &'static str, stderr: &[u8]) -> Self {
        let detail = String::from_utf8_lossy(stderr)
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty() && !is_benign(line))
            .map(|line| line.chars().take(300).collect());
        Self {
            class,
            phase,
            detail,
        }
    }

    pub(crate) fn into_io_error(self) -> io::Error {
        let kind = match self.class {
            SshFailureClass::Timeout => io::ErrorKind::TimedOut,
            SshFailureClass::Refused => io::ErrorKind::ConnectionRefused,
            SshFailureClass::AuthFailed
            | SshFailureClass::HostKey
            | SshFailureClass::HostKeyChanged => io::ErrorKind::PermissionDenied,
            SshFailureClass::ConnectionClosed => io::ErrorKind::ConnectionAborted,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, self)
    }

    /// The text for `ControlError::message`: advice first, then what OpenSSH
    /// itself said, so a user can search it.
    #[must_use]
    pub fn user_message(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} (ssh: {detail})", self.class.advice()),
            None => self.class.advice().to_owned(),
        }
    }

    /// The `SshFailure` inside an `io::Error`, if it carries one.
    #[must_use]
    pub fn from_io(error: &io::Error) -> Option<&Self> {
        error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<Self>())
    }
}

impl fmt::Display for SshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} failed: {}", self.phase, self.user_message())
    }
}

impl std::error::Error for SshFailure {}

#[cfg(test)]
mod tests {
    use super::SshFailureClass as C;
    use super::*;

    #[test]
    fn real_openssh_messages_map_to_actionable_classes() {
        let cases: &[(&str, C)] = &[
            (
                "ssh: Could not resolve hostname hogwarts: nodename nor servname provided, or not known",
                C::UnresolvedHost,
            ),
            (
                "ssh: Could not resolve hostname x: Name or service not known",
                C::UnresolvedHost,
            ),
            (
                "ssh: connect to host 10.0.0.2 port 22: Connection refused",
                C::Refused,
            ),
            (
                "ssh: connect to host 10.0.0.2 port 22: Operation timed out",
                C::Timeout,
            ),
            (
                "ssh: connect to host 10.0.0.2 port 22: Connection timed out",
                C::Timeout,
            ),
            ("Connection timed out during banner exchange", C::Timeout),
            (
                "ssh: connect to host 10.0.0.2 port 22: No route to host",
                C::Unreachable,
            ),
            (
                "ssh: connect to host h port 22: Network is unreachable",
                C::Unreachable,
            ),
            (
                "user@h: Permission denied (publickey,password).",
                C::AuthFailed,
            ),
            (
                "Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures",
                C::AuthFailed,
            ),
            ("Host key verification failed.", C::HostKey),
            (
                "No ED25519 host key is known for h and you have requested strict checking.\nHost key verification failed.",
                C::HostKey,
            ),
            (
                "@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@\nIT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\nHost key for h has changed and you have requested strict checking.\nHost key verification failed.",
                C::HostKeyChanged,
            ),
            (
                "kex_exchange_identification: read: Connection reset by peer",
                C::ConnectionClosed,
            ),
            ("Connection closed by 1.2.3.4 port 22", C::ConnectionClosed),
            (
                "/Users/u/.ssh/config: line 3: Bad configuration option: foo\n/Users/u/.ssh/config: terminating, 1 bad configuration options",
                C::Config,
            ),
            (
                "Bad owner or permissions on /Users/u/.ssh/config",
                C::Config,
            ),
            (
                "unix_listener: path \"/very/long/path\" too long for Unix domain socket",
                C::Other,
            ),
            (
                "mux_client_request_session: session request failed: Session open refused by peer",
                C::ControlMaster,
            ),
            (
                "mux_client_request_session: read from master failed: Broken pipe",
                C::ControlMaster,
            ),
            ("", C::Other),
            ("something unexpected", C::Other),
        ];
        for (stderr, expected) in cases {
            assert_eq!(
                SshFailureClass::classify(stderr.as_bytes()),
                *expected,
                "{stderr}"
            );
        }
    }

    #[test]
    fn benign_preamble_does_not_decide_the_class() {
        let stderr = b"Control socket connect(/x/ssh-control/abc): Connection refused\n\
            Warning: Permanently added 'h' (ED25519) to the list of known hosts.\n\
            u@h: Permission denied (publickey).\n";
        assert_eq!(SshFailureClass::classify(stderr), C::AuthFailed);
        let failure = SshFailure::new(C::AuthFailed, "remote platform probe", stderr);
        assert!(
            failure
                .user_message()
                .ends_with("(ssh: u@h: Permission denied (publickey).)")
        );
    }

    #[test]
    fn codes_are_stable_and_distinct() {
        let all = [
            C::UnresolvedHost,
            C::Refused,
            C::Unreachable,
            C::Timeout,
            C::AuthFailed,
            C::HostKey,
            C::HostKeyChanged,
            C::ConnectionClosed,
            C::Config,
            C::ControlMaster,
            C::Other,
        ];
        let codes: std::collections::BTreeSet<_> = all.iter().map(|c| c.code()).collect();
        assert_eq!(codes.len(), all.len());
        assert!(codes.iter().all(|code| code.starts_with("ssh_")));
    }

    #[test]
    fn io_round_trip_keeps_the_class() {
        let error = SshFailure::new(C::Timeout, "phase", b"").into_io_error();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            SshFailure::from_io(&error).map(|f| f.class),
            Some(C::Timeout)
        );
    }
}
