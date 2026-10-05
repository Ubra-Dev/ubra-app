//! Records panics as incidents before the previous hook runs.

use std::panic::PanicHookInfo;
use std::time::Duration;

use crate::value::{Value, id, text};

const MAX_FRAMES: usize = 48;

/// Chains a hook that records a `panic` incident (location,
/// symbolized frames) and flushes it to disk, then calls the previous hook.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info);
        previous(info);
    }));
}

fn record_panic(info: &PanicHookInfo<'_>) {
    if !crate::is_enabled() {
        return;
    }
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    let frames = frames(&backtrace);
    let signature = frames
        .iter()
        .find(|frame| !is_runtime_frame(frame))
        .cloned()
        .unwrap_or_default();

    crate::record(
        "panic",
        crate::Severity::Incident,
        vec![
            ("location", Value::from(location.map(text))),
            ("signature", Value::from(text(&signature))),
            (
                "frames",
                Value::List(frames.iter().map(|f| Value::from(text(f))).collect()),
            ),
        ],
    );
    crate::flush(Duration::from_secs(1));
}

/// Function names from `std::backtrace` output, hash suffixes removed.
fn frames(backtrace: &str) -> Vec<String> {
    backtrace
        .lines()
        .filter_map(|line| {
            let (index, name) = line.trim_start().split_once(": ")?;
            index.parse::<u32>().ok()?;
            let name = name.trim();
            let name = match name.rsplit_once("::h") {
                Some((head, hash))
                    if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    head
                }
                _ => name,
            };
            Some(name.to_owned())
        })
        .take(MAX_FRAMES)
        .collect()
}

fn is_runtime_frame(frame: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "std::",
        "core::",
        "alloc::",
        "rust_begin_unwind",
        "<alloc::",
        "<core::",
        "<std::",
        "ubra_telemetry::",
    ];
    PREFIXES.iter().any(|prefix| frame.starts_with(prefix)) || frame.starts_with("__")
}

/// A stable identifier for grouping panics that share a first frame.
#[must_use]
pub fn signature_id(signature: &str) -> crate::Id {
    id(signature
        .replace(
            [
                '<', '>', ' ', '&', '(', ')', ',', '\'', '{', '}', '[', ']', '*', ';',
            ],
            "",
        )
        .chars()
        .take(96)
        .collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_function_names_and_signature() {
        let backtrace = "   0: std::backtrace::Backtrace::force_capture\n             at /rustc/x/library/std/src/backtrace.rs:312:9\n   1: ubra_telemetry::panic::record_panic::h0123456789abcdef\n   2: core::panicking::panic_fmt\n   3: ubra_app::terminal_pane::TerminalPane::paint::hfedcba9876543210\n             at ./src/terminal_pane.rs:88:5\n";
        let frames = frames(backtrace);
        assert_eq!(
            frames,
            [
                "std::backtrace::Backtrace::force_capture",
                "ubra_telemetry::panic::record_panic",
                "core::panicking::panic_fmt",
                "ubra_app::terminal_pane::TerminalPane::paint",
            ]
        );
        let first = frames.iter().find(|f| !is_runtime_frame(f)).unwrap();
        assert_eq!(first, "ubra_app::terminal_pane::TerminalPane::paint");
        assert_eq!(
            signature_id(first).as_str(),
            "ubra_app::terminal_pane::TerminalPane::paint"
        );
    }
}
