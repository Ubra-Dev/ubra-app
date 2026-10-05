//! Explicit local execution records; these are not captures of agent tool calls.
use crate::{DateMillis, SessionId};
use serde::{Deserialize, Serialize};

/// Combined on-disk capture bound, including source/sequence frame headers.
pub const MAX_RUN_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_RUN_READ_BYTES: u32 = 64 * 1024;
pub const MAX_RETAINED_RUNS_PER_SESSION: usize = 200;
pub const MAX_ACTIVE_RUNS: usize = 8;
pub const MAX_ACTIVE_RUNS_PER_SESSION: usize = 4;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Build,
    Test,
    Lint,
    Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
impl RunStatus {
    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunProducer {
    LocalCommand,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub request_id: String,
    pub kind: RunKind,
    pub argv: Vec<String>,
    pub cwd: String,
    pub producer: RunProducer,
    pub started_at: DateMillis,
    pub finished_at: Option<DateMillis>,
    /// Measured from an Instant, never subtracted wall-clock timestamps.
    /// None after crash recovery when no trustworthy final measurement exists.
    pub duration_ms: Option<u64>,
    pub revision: u64,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Failed exec / capture / recovery explanation, not a synthetic exit code.
    pub error: Option<String>,
    pub output_bytes: u64,
    pub output_available: bool,
    pub output_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunStartParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub request_id: String,
    pub kind: RunKind,
    /// argv[0] is the executable; each remaining item is one exact argument.
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunListParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque run identity from next_cursor; pagination is newest-first.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunListResult {
    pub runs: Vec<RunRecord>,
    pub next_cursor: Option<String>,
    pub retention_limit: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunGetParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub run_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunReadOutputParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub run_id: String,
    /// Raw captured-byte offset (not the length of the displayed safe text).
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunOutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunOutputPart {
    pub sequence: u64,
    pub stream: RunOutputStream,
    pub offset: u64,
    pub byte_len: u32,
    /// Raw bytes can split UTF-8 characters across parts or pages.
    /// Render only through an incremental inert-text decoder, never a terminal.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunOutputChunk {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub run_id: String,
    pub revision: u64,
    pub offset: u64,
    /// May advance by fewer than max_bytes when the source-part bound is hit.
    pub next_offset: u64,
    pub parts: Vec<RunOutputPart>,
    /// True only once the producer has settled and all retained bytes were read.
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunUpdatedEvent {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub run_id: String,
    pub revision: u64,
}

/// Incrementally decodes each captured stream without interpreting terminal
/// controls. Clone the decoder at a page boundary when rereading a live page.
#[derive(Clone, Debug, Default)]
pub struct RunOutputDecoder {
    stdout: Utf8Tail,
    stderr: Utf8Tail,
}

impl RunOutputDecoder {
    pub fn push(&mut self, stream: RunOutputStream, bytes: &[u8]) -> String {
        let tail = self.tail(stream);
        let mut output = String::with_capacity(bytes.len() + tail.len);
        tail.decode_into(bytes, &mut output);
        output
    }

    /// Flush an unfinished character once, only after the retained stream ends.
    pub fn finish(&mut self, stream: RunOutputStream) -> String {
        let tail = self.tail(stream);
        if tail.len == 0 {
            return String::new();
        }
        tail.len = 0;
        "\u{fffd}".to_owned()
    }

    fn tail(&mut self, stream: RunOutputStream) -> &mut Utf8Tail {
        match stream {
            RunOutputStream::Stdout => &mut self.stdout,
            RunOutputStream::Stderr => &mut self.stderr,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Utf8Tail {
    bytes: [u8; 4],
    len: usize,
}

impl Utf8Tail {
    fn decode_into(&mut self, mut bytes: &[u8], output: &mut String) {
        while self.len > 0 {
            let width = match self.bytes[0] {
                0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            let taken = (width - self.len).min(bytes.len());
            self.bytes[self.len..self.len + taken].copy_from_slice(&bytes[..taken]);
            self.len += taken;
            bytes = &bytes[taken..];
            match std::str::from_utf8(&self.bytes[..self.len]) {
                Ok(text) => {
                    append_inert_text(output, text);
                    self.len = 0;
                }
                Err(error) if error.error_len().is_none() => return,
                Err(_) => {
                    let pending = self.bytes;
                    let len = self.len;
                    self.len = 0;
                    self.decode_into(&pending[..len], output);
                }
            }
        }
        while !bytes.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    append_inert_text(output, text);
                    return;
                }
                Err(error) => {
                    let (valid, remaining) = bytes.split_at(error.valid_up_to());
                    // SAFETY: Utf8Error::valid_up_to guarantees this prefix.
                    append_inert_text(output, unsafe { std::str::from_utf8_unchecked(valid) });
                    if let Some(invalid_len) = error.error_len() {
                        output.push('\u{fffd}');
                        bytes = &remaining[invalid_len..];
                    } else {
                        self.bytes[..remaining.len()].copy_from_slice(remaining);
                        self.len = remaining.len();
                        return;
                    }
                }
            }
        }
    }
}

fn append_inert_text(output: &mut String, text: &str) {
    use std::fmt::Write as _;
    for character in text.chars() {
        if (character.is_control() && character != '\n' && character != '\t')
            || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            write!(output, "\\u{{{:x}}}", u32::from(character))
                .expect("String writes are infallible");
        } else {
            output.push(character);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_start_rejects_unknown_shell_command_fields() {
        assert!(
            serde_json::from_value::<RunStartParams>(serde_json::json!({
                "sessionID": "s_a", "requestId": "once", "kind": "test",
                "argv": ["true"], "shellCommand": "ignored must never happen"
            }))
            .is_err()
        );
    }

    #[test]
    fn output_decoder_keeps_independent_utf8_tails_across_interleaved_streams() {
        let mut decoder = RunOutputDecoder::default();
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xe2"), "");
        assert_eq!(decoder.push(RunOutputStream::Stderr, b"\xc3"), "");
        let page_start = decoder.clone();
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\x82"), "");
        assert_eq!(decoder.push(RunOutputStream::Stderr, b"\xa9"), "é");
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xac"), "€");
        let mut reread = page_start;
        assert_eq!(reread.push(RunOutputStream::Stdout, b"\x82\xac"), "€");
        assert_eq!(reread.push(RunOutputStream::Stderr, b"\xa9"), "é");
    }

    #[test]
    fn output_decoder_preserves_valid_text_after_invalid_prefix_and_flushes_eof_once() {
        let mut decoder = RunOutputDecoder::default();
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xe2"), "");
        assert_eq!(
            decoder.push(RunOutputStream::Stdout, b"ok\xff"),
            "\u{fffd}ok\u{fffd}"
        );
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xe2\x82"), "");
        assert_eq!(decoder.finish(RunOutputStream::Stdout), "\u{fffd}");
        assert_eq!(decoder.finish(RunOutputStream::Stdout), "");
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xe2\x82"), "");
        assert_eq!(
            decoder.push(RunOutputStream::Stdout, b"\xc3\xa9"),
            "\u{fffd}é"
        );
        assert_eq!(
            decoder.push(
                RunOutputStream::Stderr,
                "\x1b]52;c;private\x07\r\n\t\u{061c}\u{200e}\u{200f}\u{202e}".as_bytes()
            ),
            "\\u{1b}]52;c;private\\u{7}\\u{d}\n\t\\u{61c}\\u{200e}\\u{200f}\\u{202e}",
        );
    }

    #[test]
    fn output_decoder_handles_a_full_page_of_invalid_leading_bytes_without_recursion_growth() {
        let mut decoder = RunOutputDecoder::default();
        let bytes = vec![0xe2; MAX_RUN_READ_BYTES as usize];
        assert_eq!(decoder.push(RunOutputStream::Stdout, b"\xe2"), "");
        let mut text = decoder.push(RunOutputStream::Stdout, &bytes);
        text.push_str(&decoder.finish(RunOutputStream::Stdout));
        assert_eq!(text, "\u{fffd}".repeat(bytes.len() + 1));
    }
}
