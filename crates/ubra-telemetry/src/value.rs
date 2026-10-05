//! Field values an event may carry.
//!
//! Types constrain the field shape; call sites enforce content policy: there is no constructor that accepts an
//! arbitrary runtime string verbatim. Strings are either `&'static str`
//! (authored in code, so enum-like), an [`Id`] (restricted alphabet, bounded,
//! no path separators or spaces), or a [`Text`] (scrubbed of home paths,
//! user names, e-mail addresses and token-like runs, then truncated).

use std::time::Duration;

use serde::ser::{Serialize, SerializeMap, Serializer};
use sha2::{Digest, Sha256};

use crate::redact;

const ID_MAX: usize = 96;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Static(&'static str),
    Id(Id),
    Text(Text),
    List(Vec<Value>),
    Obj(Vec<(&'static str, Value)>),
}

/// A machine identifier: session ids, conversation UUIDs, agent ids, RPC
/// method names, error codes. Anything outside `[A-Za-z0-9_.:-]{1,96}` is
/// replaced by `"!invalid"` rather than recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Id(String);

/// Free-form text that went through [`redact::scrub`]. Use only for strings
/// carrying diagnostic symbols or OS crash facts. Never for arbitrary error
/// `Display` output, panic payloads, terminal output, prompts,
/// environment, file contents, or clipboard contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text(String);

impl Id {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Text {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validates a machine identifier. See [`Id`].
#[must_use]
pub fn id(value: impl AsRef<str>) -> Id {
    let value = value.as_ref();
    let valid = !value.is_empty()
        && value.len() <= ID_MAX
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    Id(if valid {
        value.to_owned()
    } else {
        "!invalid".to_owned()
    })
}

/// A closed classification of agent manifest ids, for metric names that must
/// be `&'static str` and must not grow with the agent catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentClass {
    Claude,
    Codex,
    Cursor,
    /// A login shell or a user command (`shell`, `generic`).
    Shell,
    Other,
}

impl AgentClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Shell => "shell",
            Self::Other => "other",
        }
    }
}

/// The [`AgentClass`] of a manifest id (`claude-code`, `codex`, ...). Ids this
/// enum has no arm for classify as `Other`.
#[must_use]
pub fn agent_class(manifest_id: &str) -> AgentClass {
    match manifest_id {
        "claude-code" => AgentClass::Claude,
        "codex" => AgentClass::Codex,
        "cursor" => AgentClass::Cursor,
        "shell" | "generic" => AgentClass::Shell,
        _ => AgentClass::Other,
    }
}

/// Scrubs and bounds free-form text. See [`Text`].
#[must_use]
pub fn text(value: impl AsRef<str>) -> Text {
    Text(redact::scrub(value.as_ref()))
}

/// A stable, non-reversible handle for a filesystem path (a project root, a
/// cwd), so events about the same place correlate without revealing it.
#[must_use]
pub fn path_hash(path: impl AsRef<std::path::Path>) -> Id {
    let digest = Sha256::digest(path.as_ref().as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Id(format!("p{hex}"))
}

/// The class of an error for grouping: the `ErrorKind` of an `io::Error`,
/// e.g. `"NotFound"`, plus the raw OS errno when there is one.
#[must_use]
pub fn io_error(error: &std::io::Error) -> Value {
    let mut fields = vec![("kind", Value::Id(id(format!("{:?}", error.kind()))))];
    if let Some(code) = error.raw_os_error() {
        fields.push(("errno", Value::I64(i64::from(code))));
    }
    Value::Obj(fields)
}

macro_rules! from_int {
    ($variant:ident, $target:ty: $($source:ty),*) => {
        $(impl From<$source> for Value {
            fn from(value: $source) -> Self {
                Value::$variant(<$target>::try_from(value).unwrap_or(<$target>::MAX))
            }
        })*
    };
}
from_int!(I64, i64: i8, i16, i32, i64, isize);
from_int!(U64, u64: u8, u16, u32, u64, usize);

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}
impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Value::F64(f64::from(value))
    }
}
impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::F64(value)
    }
}
impl From<&'static str> for Value {
    fn from(value: &'static str) -> Self {
        Value::Static(value)
    }
}
impl From<Id> for Value {
    fn from(value: Id) -> Self {
        Value::Id(value)
    }
}
impl From<Text> for Value {
    fn from(value: Text) -> Self {
        Value::Text(value)
    }
}
/// Durations are recorded as fractional milliseconds.
impl From<Duration> for Value {
    fn from(value: Duration) -> Self {
        Value::F64((value.as_secs_f64() * 100_000.0).round() / 100.0)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Value::Null, Into::into)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(value: Vec<T>) -> Self {
        Value::List(value.into_iter().map(Into::into).collect())
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::I64(value) => serializer.serialize_i64(*value),
            Value::U64(value) => serializer.serialize_u64(*value),
            Value::F64(value) if value.is_finite() => serializer.serialize_f64(*value),
            Value::F64(_) => serializer.serialize_unit(),
            Value::Static(value) => serializer.serialize_str(value),
            Value::Id(value) => serializer.serialize_str(&value.0),
            Value::Text(value) => serializer.serialize_str(&value.0),
            Value::List(values) => values.serialize(serializer),
            Value::Obj(fields) => Fields(fields).serialize(serializer),
        }
    }
}

/// Serializes `(key, value)` pairs as a JSON object.
pub(crate) struct Fields<'a>(pub &'a [(&'static str, Value)]);

impl Serialize for Fields<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_paths_spaces_and_oversize() {
        assert_eq!(id("s_26bf32debd4c").as_str(), "s_26bf32debd4c");
        assert_eq!(
            id("0a40e747-fa0c-4e9a-b755-c195ab079cda").as_str(),
            "0a40e747-fa0c-4e9a-b755-c195ab079cda"
        );
        assert_eq!(id("session.spawn").as_str(), "session.spawn");
        assert_eq!(id("/Users/alex/x").as_str(), "!invalid");
        assert_eq!(id("hello world").as_str(), "!invalid");
        assert_eq!(id("").as_str(), "!invalid");
        assert_eq!(id("a".repeat(97)).as_str(), "!invalid");
    }

    #[test]
    fn path_hash_is_stable_and_opaque() {
        let a = path_hash("/Users/alex/fun/anara");
        assert_eq!(a, path_hash("/Users/alex/fun/anara"));
        assert_ne!(a, path_hash("/Users/alex/fun/other"));
        assert!(!a.as_str().contains("alex"));
    }

    #[test]
    fn values_serialize_as_plain_json() {
        let fields = [
            ("a", Value::from(3u32)),
            ("b", Value::from("resume")),
            ("c", Value::from(Duration::from_micros(1500))),
            ("d", Value::from(None::<u8>)),
            ("e", Value::from(f64::NAN)),
        ];
        let json = serde_json::to_string(&Fields(&fields)).unwrap();
        assert_eq!(json, r#"{"a":3,"b":"resume","c":1.5,"d":null,"e":null}"#);
    }
}
