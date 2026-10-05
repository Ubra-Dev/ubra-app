//! Cross-revision equivalence probe for the terminal feed path.
//!
//! Drives one `HeadlessScreen` through a long deterministic stream (styled
//! text, wide and combining characters, links, prompts, scroll regions,
//! erase, insert mode, charsets, alternate screen, synchronized output,
//! resizes and more than 10,000 lines of history) with random read splits.
//! Every observable output is hashed: diffs, snapshots, history, scrollback,
//! `content_seq`, `filled_cells`, cursor, title and progress.
//!
//! Run the same file on two revisions and compare the printed digests:
//!
//! ```sh
//! cargo test --release -p ubra-terminal-state --test transcript_digest -- --ignored --nocapture
//! ```
//!
//! It stays below the 4 MiB history byte budget: a history codec change can
//! legitimately change how many rows fit under that budget.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ubra_terminal_state::HeadlessScreen;

#[test]
#[ignore = "cross-revision probe; compare its output between builds"]
fn transcript_digest() {
    let actions: &[&str] = &[
        "\x1b[31;1m",
        "\x1b[38;2;1;2;3;48;5;17m",
        "\x1b[0m",
        "\x1b[7m",
        "\x1b[4:3m\x1b[58;5;9m",
        "\x1b[44m\x1b[K\x1b[0m",
        "\x1b[42m\x1b[2K\r\n",
        "\x1b]8;id=a;https://example.invalid/a\x07",
        "\x1b]8;;\x07",
        "\x1b]133;A\x07$ ",
        "\x1b]0;title\x07",
        "\x1b]9;4;1;40\x07",
        "\x1b[2;6r",
        "\x1b[r",
        "\x1b[3S",
        "\x1b[2T",
        "\x1b[2L",
        "\x1b[1M",
        "\x1b[3@",
        "\x1b[2P",
        "\x1b[4X",
        "\x1b[2J",
        "\x1b[H",
        "\x1b[5;7H",
        "\x1b[99;99H",
        "\x1b[4h",
        "\x1b[4l",
        "\x1b[?7l",
        "\x1b[?7h",
        "\x1b(0lqqk\x1b(B",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b[?2026h",
        "\x1b[?2026l",
        "\x1b7",
        "\x1b8",
        "\x1bD",
        "\x1bM",
        "\t",
        "\x08",
        "界面 e\u{301} 🦀",
        "abc\x1b[3b",
    ];
    let mut screen = HeadlessScreen::new(40, 10);
    let mut seed = 0x6a09_e667_f3bc_c908u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut hasher = DefaultHasher::new();
    let mut pending = Vec::new();
    for step in 0..60_000u32 {
        let random = next();
        match random % 8 {
            0..=3 => pending.extend_from_slice(
                format!(
                    "\x1b[3{}m{step:06}\x1b[0m {}\r\n",
                    random % 8,
                    "log text ".repeat((random >> 8) as usize % 12)
                )
                .as_bytes(),
            ),
            4 => pending.extend_from_slice("x".repeat((random >> 8) as usize % 90).as_bytes()),
            5 | 6 => pending
                .extend_from_slice(actions[(random >> 16) as usize % actions.len()].as_bytes()),
            _ => {
                pending.extend_from_slice("\r\n".repeat(1 + (random >> 8) as usize % 30).as_bytes())
            }
        }
        // Random read boundaries split escapes and UTF-8.
        if random % 4 == 0 {
            let mut rest = &pending[..];
            while !rest.is_empty() {
                let take = (1 + next() as usize % 700).min(rest.len());
                screen.feed(&rest[..take]);
                rest = &rest[take..];
            }
            pending.clear();
            format!("{:?}", screen.grid_update(false)).hash(&mut hasher);
            (screen.content_seq(), screen.filled_cells(), screen.cursor()).hash(&mut hasher);
            (screen.title(), screen.is_alt_screen(), screen.size()).hash(&mut hasher);
            format!("{:?}", screen.progress()).hash(&mut hasher);
        }
        if step % 25_000 == 24_999 {
            // Erase scrollback rarely, so history fills and recycles rows.
            pending.extend_from_slice(b"\x1b[3J");
        }
        if random % 211 == 0 {
            let cols = [2, 13, 40, 80, 133, 300][(random >> 20) as usize % 6];
            let rows = [1, 3, 10, 24, 50][(random >> 28) as usize % 5];
            screen.resize(cols, rows);
            (screen.content_seq(), screen.filled_cells()).hash(&mut hasher);
        }
        if step % 1500 == 0 {
            format!("{:?}", screen.full_snapshot()).hash(&mut hasher);
            format!("{:?}", screen.history_snapshot()).hash(&mut hasher);
            format!("{:?}", screen.scrollback()).hash(&mut hasher);
            println!(
                "step {step:6}: {:016x} history {}",
                hasher.finish(),
                screen.history_snapshot().len()
            );
        }
    }
    println!("final: {:016x}", hasher.finish());
}

/// Cross-revision probe for column-change reflow of long history. Every
/// scenario keeps more than 10,000 rows of history (the row limit binds, the
/// 4 MiB byte budget does not) and resizes it through drags and jumps while
/// output arrives, the alternate screen comes and goes, and history is read
/// in pages, in full, and not at all for long stretches. Every observable
/// output is hashed; base and branch must print the same digests.
///
/// ```sh
/// cargo test --release -p ubra-terminal-state --test transcript_digest reflow_digest -- --ignored --nocapture
/// ```
#[test]
#[ignore = "cross-revision probe; compare its output between builds"]
fn reflow_digest() {
    fn log_line(line: usize, flavor: u8) -> String {
        let color = 31 + line % 6;
        let module = "x".repeat(10 + line % 50);
        let tail = if line.is_multiple_of(4) {
            " with an extra long explanation that wraps on narrow panes and splits"
        } else {
            ""
        };
        match flavor {
            // Wide characters on every third line.
            1 if line.is_multiple_of(3) => format!(
                "\x1b[{color}m[{line:010}] 構築中 crate_{} 🦀\x1b[0m  モジュール {module}{tail}\r\n",
                line % 997
            ),
            // Rows whose reflow is not a plain re-chunking: trailing styled
            // blanks, background erases, tabs, zero-width marks, empty
            // lines, exact-width lines and lines far longer than a block.
            2 => match line % 11 {
                0 => format!("\x1b[1m{line:06} bold tail   \x1b[0m\r\n"),
                1 => format!("\x1b[44m{line:06} erased to blue\x1b[K\x1b[0m\r\n"),
                2 => format!("{line:06}\tt\tab\ts{tail}\r\n"),
                3 => "\r\n".to_string(),
                4 => format!("{}\r\n", "=".repeat(160)),
                5 => format!("{line:06} {}\r\n", "long ".repeat(200 + line % 700)),
                6 => format!("{line:06} e\u{301} combining{tail}\r\n"),
                7 => format!("\x1b[4m{line:06} underlined blanks      \x1b[0m\r\n"),
                8 => format!("{line:06} {}\x1b[31m   \x1b[0m\r\n", "y".repeat(line % 150)),
                9 => format!("{line:06} 界{}\r\n", "z".repeat(line % 170)),
                _ => format!("\x1b[3{}m{line:06} plain{tail}\x1b[0m\r\n", line % 8),
            },
            _ => format!(
                "\x1b[{color}m[{line:010}] building crate_{} v0.{}.0\x1b[0m  Compiling module {module}{tail}\r\n",
                line % 997,
                line % 9
            ),
        }
    }

    struct Probe {
        hasher: DefaultHasher,
        seed: u64,
    }
    impl Probe {
        fn next(&mut self) -> u64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            self.seed
        }
        fn step(&mut self, screen: &mut HeadlessScreen) {
            format!("{:?}", screen.grid_update(false)).hash(&mut self.hasher);
            (screen.content_seq(), screen.filled_cells(), screen.cursor()).hash(&mut self.hasher);
            (screen.is_alt_screen(), screen.size()).hash(&mut self.hasher);
            // History length without reading any history row.
            let page = screen.scrollback_cells(0, 0);
            (page.total_rows, page.live_start_row).hash(&mut self.hasher);
        }
        fn page(&mut self, screen: &mut HeadlessScreen) {
            let total = screen.scrollback_cells(0, 0).total_rows.max(1);
            let first = (self.next() % total as u64) as i64;
            format!("{:?}", screen.scrollback_cells(first, 50)).hash(&mut self.hasher);
        }
        fn everything(&mut self, screen: &mut HeadlessScreen) {
            format!("{:?}", screen.full_snapshot()).hash(&mut self.hasher);
            format!("{:?}", screen.history_snapshot()).hash(&mut self.hasher);
            format!("{:?}", screen.scrollback()).hash(&mut self.hasher);
        }
    }

    let mut probe = Probe {
        hasher: DefaultHasher::new(),
        seed: 0x3c6e_f372_fe94_f82b,
    };
    for (name, flavor) in [("ascii", 0u8), ("wide", 1), ("edge", 2)] {
        let mut screen = HeadlessScreen::new(160, 50);
        let mut line = 0;
        let mut feed = |screen: &mut HeadlessScreen, count: usize| {
            let mut bytes = String::new();
            for _ in 0..count {
                bytes.push_str(&log_line(line, flavor));
                line += 1;
            }
            screen.feed(bytes.as_bytes());
        };
        feed(&mut screen, 12_000);
        probe.step(&mut screen);
        let mut widths: Vec<(usize, usize)> = Vec::new();
        // A drag in and back out, one column per step, twice.
        for _ in 0..2 {
            widths.extend((100..160).rev().map(|cols| (cols, 50)));
            widths.extend((101..=160).map(|cols| (cols, 50)));
        }
        // Narrow -> wide -> narrow jumps with row changes.
        widths.extend([
            (40, 50),
            (300, 24),
            (13, 10),
            (2, 3),
            (133, 50),
            (80, 24),
            (240, 66),
            (161, 49),
            (160, 50),
        ]);
        for (index, &(cols, rows)) in widths.iter().enumerate() {
            screen.resize(cols, rows);
            probe.step(&mut screen);
            let random = probe.next();
            match random % 16 {
                // Output between resizes scrolls full history.
                0..=2 => feed(&mut screen, 1 + (random >> 8) as usize % 5),
                // A redraw that does not scroll: the cursor moves up into a
                // wrapped line and rewrites it.
                3 => screen.feed(b"\x1b[3A\r\x1b[2Kredrawn prompt $ \x1b[3B"),
                4 => probe.page(&mut screen),
                5 if index % 7 == 0 => {
                    screen.feed(b"\x1b[?1049halternate \xe7\x95\x8c\r\nscreen");
                    probe.step(&mut screen);
                }
                6 if screen.is_alt_screen() => screen.feed(b"\x1b[?1049l"),
                // Scrolling with a coloured template resets recycled rows
                // to it.
                7 => screen.feed(b"\x1b[44mblue\r\n\r\n\x1b[0m"),
                _ => {}
            }
            probe.step(&mut screen);
            if index % 60 == 59 {
                probe.everything(&mut screen);
            }
        }
        if screen.is_alt_screen() {
            screen.feed(b"\x1b[?1049l");
        }
        probe.everything(&mut screen);
        // Resizes with no history read in between, then one full read.
        for &(cols, rows) in widths.iter().rev().take(80) {
            screen.resize(cols, rows);
            probe.step(&mut screen);
        }
        probe.everything(&mut screen);
        println!("{name}: {:016x}", probe.hasher.finish());
    }
    println!("final: {:016x}", probe.hasher.finish());
}
