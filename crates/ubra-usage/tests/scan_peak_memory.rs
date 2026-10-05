//! A transcript is scanned a line at a time. Codex rollouts reach hundreds of
//! MiB (757 MB on the machine that found this), and reading a whole tail into
//! one `Vec` made the desktop app's footprint peak at several times the file.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};

use ubra_usage::transcripts::{ScanPaths, SystemClock, UsageProvider, UsageStore};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let current = CURRENT.fetch_add(layout.size(), Relaxed) + layout.size();
        PEAK.fetch_max(current, Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            let current =
                CURRENT.fetch_add(new_size - layout.size(), Relaxed) + new_size - layout.size();
            PEAK.fetch_max(current, Relaxed);
        } else {
            CURRENT.fetch_sub(layout.size() - new_size, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1024 * 1024;

#[test]
fn scanning_a_large_rollout_buffers_one_line_not_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join("rollout.jsonl");
    let mut file = std::io::BufWriter::new(fs::File::create(&rollout).unwrap());
    let filler = "x".repeat(1_000);
    let mut written = 0;
    while written < 64 * MIB {
        let line = format!(
            "{{\"timestamp\":\"2026-09-28T09:00:00Z\",\"type\":\"response_item\",\"payload\":\"{filler}\"}}\n"
        );
        file.write_all(line.as_bytes()).unwrap();
        written += line.len();
    }
    file.flush().unwrap();
    drop(file);

    let mut store = UsageStore::with_paths_and_clock(
        ScanPaths {
            roots: vec![(temp.path().join(".codex/sessions"), UsageProvider::Codex)],
            cache_file: temp.path().join("usage-cache.json"),
        },
        SystemClock,
    );
    let before = CURRENT.load(Relaxed);
    PEAK.store(before, Relaxed);
    let _ = store.refresh();
    let peak = PEAK.load(Relaxed) - before;

    assert_eq!(
        store.last_stats().bytes_parsed,
        fs::metadata(&rollout).unwrap().len()
    );
    assert!(
        peak < 8 * MIB,
        "scanning a 64 MiB rollout allocated {:.1} MiB at once",
        peak as f64 / MIB as f64
    );
}

#[test]
fn a_huge_line_that_only_mentions_a_usage_tag_is_not_materialized() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join("rollout.jsonl");
    // Compacted history nests earlier records, so the tag appears unescaped
    // inside a line that is not itself a usage event.
    let filler = "y".repeat(16 * MIB);
    let line = format!(
        "{{\"timestamp\":\"2026-09-28T09:00:00Z\",\"type\":\"compacted\",\"payload\":{{\"history\":[{{\"type\":\"token_count\",\"text\":\"{filler}\"}},{{\"type\":\"turn_context\"}}]}}}}\n"
    );
    fs::write(&rollout, &line).unwrap();

    let mut store = UsageStore::with_paths_and_clock(
        ScanPaths {
            roots: vec![(temp.path().join(".codex/sessions"), UsageProvider::Codex)],
            cache_file: temp.path().join("usage-cache.json"),
        },
        SystemClock,
    );
    let before = CURRENT.load(Relaxed);
    PEAK.store(before, Relaxed);
    let _ = store.refresh();
    let peak = PEAK.load(Relaxed) - before;

    assert_eq!(store.last_stats().bytes_parsed, line.len() as u64);
    // The line buffer may reach twice the line while it grows; a parsed
    // `Value` copy would add the whole line again on top.
    assert!(
        peak < 2 * line.len() + 4 * MIB,
        "a {:.1} MiB line allocated {:.1} MiB at once",
        line.len() as f64 / MIB as f64,
        peak as f64 / MIB as f64
    );
}
