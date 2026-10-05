use std::fmt;
use std::io;

#[derive(Debug)]
pub enum UpdateError {
    /// The running binary is not inside a `.app`, or the bundle is unsigned —
    /// a `cargo run` build has nothing to update and no signature to pin to.
    NotUpdatable(String),
    /// The request failed before the server sent anything usable, or the
    /// server answered with an HTTP error. `failure` says which, so the UI
    /// and telemetry never call a 404 "can't reach the host".
    Network {
        failure: NetworkFailure,
        detail: String,
    },
    /// The feed parsed as JSON but is not a feed we understand.
    Feed(String),
    /// A download URL that failed the origin/shape checks in `crate::net`.
    UntrustedUrl(String),
    Integrity(String),
    /// The downloaded bundle is not a notarized build of *this* app.
    Signature(String),
    /// The installed bundle sits somewhere this user cannot write.
    NotWritable(String),
    Io(io::Error),
    /// A helper (`curl`, `ditto`, `codesign`, …) exited non-zero.
    Tool {
        tool: &'static str,
        detail: String,
    },
}

impl UpdateError {
    pub(crate) fn network(failure: NetworkFailure, detail: impl Into<String>) -> Self {
        Self::Network {
            failure,
            detail: detail.into(),
        }
    }

    pub(crate) fn tool(tool: &'static str, detail: impl Into<String>) -> Self {
        Self::Tool {
            tool,
            detail: detail.into(),
        }
    }

    /// One line, safe to show in the sidebar or settings pane.
    pub fn user_facing(&self) -> String {
        match self {
            Self::NotUpdatable(_) => "Updates are off for this build".to_owned(),
            Self::Network { failure, .. } => failure.user_facing(),
            Self::Feed(_) => "The update feed looks malformed".to_owned(),
            Self::UntrustedUrl(_) => "The update feed pointed somewhere unexpected".to_owned(),
            Self::Integrity(_) => "The download was incomplete or corrupt".to_owned(),
            Self::Signature(_) => "The download failed its signature check".to_owned(),
            Self::NotWritable(_) => "ubra can't write to its own folder".to_owned(),
            Self::Io(_) | Self::Tool { .. } => "The update couldn't be installed".to_owned(),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUpdatable(detail) => write!(formatter, "not updatable: {detail}"),
            Self::Network { failure, detail } => {
                write!(formatter, "network error ({}): {detail}", failure.kind())
            }
            Self::Feed(detail) => write!(formatter, "bad update feed: {detail}"),
            Self::UntrustedUrl(detail) => write!(formatter, "untrusted update URL: {detail}"),
            Self::Integrity(detail) => write!(formatter, "integrity check failed: {detail}"),
            Self::Signature(detail) => write!(formatter, "signature check failed: {detail}"),
            Self::NotWritable(detail) => write!(formatter, "install location: {detail}"),
            Self::Io(error) => write!(formatter, "io error: {error}"),
            Self::Tool { tool, detail } => write!(formatter, "{tool} failed: {detail}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Why a request to the releases host failed, read from curl's exit code and
/// the HTTP status it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkFailure {
    /// The host name did not resolve: offline, captive portal, broken DNS.
    Dns,
    /// Resolved, but no TCP connection (refused, unreachable, connect timeout).
    Connect,
    /// Connected, but the transfer did not finish within its time limit.
    Timeout,
    /// The TLS handshake or certificate check failed (often a proxy).
    Tls,
    /// HTTP 404: the release asset is not there.
    NotFound,
    /// HTTP 403 or 429: GitHub is throttling this address.
    RateLimited(u16),
    /// Any other HTTP error status.
    Http(u16),
    /// Connection dropped mid-transfer, or a curl failure not classified above.
    Other,
}

impl NetworkFailure {
    /// Stable telemetry label (`error_kind`).
    pub fn kind(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::NotFound => "not_found",
            Self::RateLimited(_) => "rate_limited",
            Self::Http(_) => "http_error",
            Self::Other => "network",
        }
    }

    pub fn http_status(self) -> Option<u16> {
        match self {
            Self::NotFound => Some(404),
            Self::RateLimited(status) | Self::Http(status) => Some(status),
            _ => None,
        }
    }

    /// Worth one quick retry: the cause is plausibly a blip, not a verdict.
    pub(crate) fn is_transient(self) -> bool {
        match self {
            Self::Dns | Self::Connect | Self::Tls | Self::Other => true,
            Self::Http(status) => status >= 500,
            Self::Timeout | Self::NotFound | Self::RateLimited(_) => false,
        }
    }

    /// Short enough for the sidebar footer, which truncates.
    fn user_facing(self) -> String {
        match self {
            Self::Dns => "Couldn't look up github.com".to_owned(),
            Self::Connect => "Couldn't connect to github.com".to_owned(),
            Self::Timeout => "github.com took too long".to_owned(),
            Self::Tls => "Secure connection to GitHub failed".to_owned(),
            Self::NotFound => "Update file missing on GitHub".to_owned(),
            Self::RateLimited(_) => "GitHub rate limit, try again later".to_owned(),
            Self::Http(status) => format!("GitHub error (HTTP {status})"),
            Self::Other => "Connection to github.com dropped".to_owned(),
        }
    }
}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;
