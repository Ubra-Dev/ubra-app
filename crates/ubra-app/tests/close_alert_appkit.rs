//! Ignored, direct AppKit close-sheet smoke running on the process main thread.
//!
//! Run: cargo test -p ubra-app --test close_alert_appkit -- --ignored --nocapture
#![allow(dead_code)]

#[path = "../src/alerts.rs"]
mod alerts;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let run_ignored = args
        .iter()
        .any(|arg| arg == "--ignored" || arg == "--include-ignored");
    if !run_ignored || args.iter().any(|arg| arg == "--list") {
        println!("native_close_sheet_appkit: test (ignored; requires a macOS desktop)");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        alerts::native_close_smoke();
        println!("native_close_sheet_appkit ... ok");
    }
    #[cfg(not(target_os = "macos"))]
    println!("native_close_sheet_appkit ... ignored (requires macOS)");
}
