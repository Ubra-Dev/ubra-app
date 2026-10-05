//! Cold usage scan of this machine's Claude and Codex transcripts, read-only.
//!
//! Scans `$HOME/.claude/projects` and `$HOME/.codex/sessions` into a private
//! temporary ledger (the app's own `usage-cache.json` is never touched), then
//! prints wall time, peak RSS and the scan statistics. Point `HOME` at an
//! APFS clone (`cp -cR`) of those two directories to compare builds on
//! identical bytes while agents keep writing.
//!
//! `cargo run --release -p ubra-usage --example usage_scan_bench`

use ubra_usage::transcripts::{ScanPaths, SystemClock, UsageStore};

fn main() {
    let home = std::env::var("HOME").expect("HOME");
    let ledger = tempfile_path();
    let mut paths = ScanPaths::for_home(&home);
    paths.cache_file = ledger.clone();
    let mut store = UsageStore::with_paths_and_clock(paths, SystemClock);
    let start = std::time::Instant::now();
    let _ = store.refresh();
    let elapsed = start.elapsed();
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let _ = std::fs::remove_file(&ledger);
    // ru_maxrss is bytes on macOS and KiB on Linux.
    let peak_mib = if cfg!(target_os = "macos") {
        usage.ru_maxrss as f64 / (1024.0 * 1024.0)
    } else {
        usage.ru_maxrss as f64 / 1024.0
    };
    println!(
        "cold scan {:.2} s, peak RSS {peak_mib:.0} MiB, {:?}",
        elapsed.as_secs_f64(),
        store.last_stats()
    );
}

fn tempfile_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ubra-usage-scan-bench-{}.json", std::process::id()))
}
