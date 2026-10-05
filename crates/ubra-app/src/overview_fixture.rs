//! Design-exploration fleet for the Session Overview screenshots: realistic,
//! colored terminal screens parsed by the shared `ubra-terminal-state` parser
//! into the same `GridBuffer` a resident pane paints.
#![allow(dead_code)]

use ubra_term::buffer::GridBuffer;

pub(crate) const COLS: u16 = 132;
pub(crate) const ROWS: u16 = 38;

/// Agents pick lighter diff tints on a light terminal; so does the fixture.
pub(crate) static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `«x»` markup to ANSI: a short name, or `«[…]»` for a raw SGR body.
fn ansi(markup: &str) -> String {
    let light = LIGHT.load(std::sync::atomic::Ordering::Relaxed);
    let mut out = String::new();
    let mut rest = markup;
    while let Some(start) = rest.find('«') {
        out.push_str(&rest[..start]);
        let after = &rest[start + '«'.len_utf8()..];
        let end = after.find('»').expect("closed token");
        let token = &after[..end];
        let sgr = match token {
            "0" => "0".to_owned(),
            "b" => "1".to_owned(),
            "d" => "2".to_owned(),
            "i" => "3".to_owned(),
            "I" => "7".to_owned(),
            "r" => "31".to_owned(),
            "g" => "32".to_owned(),
            "y" => "33".to_owned(),
            "B" => "34".to_owned(),
            "m" => "35".to_owned(),
            "c" => "36".to_owned(),
            "w" => "37".to_owned(),
            "k" => "90".to_owned(),
            "o" => "38;2;215;119;87".to_owned(),
            "p" => "38;2;177;151;252".to_owned(),
            "s" => "38;2;140;140;140".to_owned(),
            "+" if light => "48;2;209;242;214".to_owned(),
            "-" if light => "48;2;253;219;219".to_owned(),
            "+" => "48;2;28;66;38".to_owned(),
            "-" => "48;2;86;32;36".to_owned(),
            "/" => "49".to_owned(),
            raw if raw.starts_with('[') => raw[1..raw.len() - 1].to_owned(),
            other => panic!("unknown token {other}"),
        };
        out.push_str(&format!("\x1b[{sgr}m"));
        rest = &after[end + '»'.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// Agent transcripts sit at the bottom of a screen that has scrolled.
pub(crate) fn screen_bottom(markup: &str) -> GridBuffer {
    let lines = markup.trim_end().lines().count();
    let pad = "\n".repeat((ROWS as usize).saturating_sub(lines));
    screen(&format!("{pad}{}", markup.trim_end()))
}

pub(crate) fn screen(markup: &str) -> GridBuffer {
    let mut screen = ubra_terminal_state::HeadlessScreen::new(COLS.into(), ROWS.into());
    let text = ansi(markup).replace('\n', "\r\n");
    screen.feed(text.trim_end_matches(['\r', '\n']).as_bytes());
    let mut buffer = GridBuffer::new(COLS, ROWS);
    buffer.apply(screen.full_snapshot());
    buffer
}

fn claude_box(width: usize, body: &str) -> String {
    let inner = width - 2;
    let pad = inner.saturating_sub(body.chars().count() + 1);
    format!(
        "«s»╭{}╮«0»\n«s»│«0» {body}{}«s»│«0»\n«s»╰{}╯«0»",
        "─".repeat(inner),
        " ".repeat(pad),
        "─".repeat(inner)
    )
}

pub(crate) fn claude_working() -> String {
    format!(
        "{}\n{}",
        r#"«o»⏺«0» «b»Read«0»(crates/ubra-app/src/session_surfaces.rs)
  ⎿  Read «b»2529«0» lines

«o»⏺«0» The overview builds its cards from «c»read_screen«0» text, then crops twelve lines at 160 columns. That is why
  every preview is cut off mid-line on both edges. I'll paint the resident grid instead, scaled to the card,
  so a card is the pane itself at a smaller font size.

«o»⏺«0» «b»Update«0»(crates/ubra-app/src/session_surfaces.rs)
  ⎿  Updated «b»session_surfaces.rs«0» with «b»18«0» additions and «b»41«0» removals
     «s»1178«0» «-»-        let mut thumbnail = div()                                                          «/»
     «s»1179«0» «-»-            .relative()                                                                    «/»
     «s»1180«0» «-»-            .h(px(178.0))                                                                  «/»
     «s»1178«0» «+»+        let aspect = grid_aspect(window, buffer.grid_cols(), buffer.grid_rows());          «/»
     «s»1179«0» «+»+        let height = width / aspect;                                                     «/»
     «s»1180«0» «+»+        let mut thumbnail = div().relative().w(px(width)).h(px(height))                   «/»
     «s»1181«0»              .rounded(px(Radius::CARD))
     «s»1182«0»              .overflow_hidden()

«o»⏺«0» «b»Bash«0»(cargo test -p ubra-app overview -- --nocapture)
  ⎿  running 6 tests
     test gallery_columns_follow_available_width ... «g»ok«0»
     test overview_keeps_boundary_keys_away_from_the_terminal ... «g»ok«0»
     «s»… +4 lines (ctrl+r to expand)«0»

«o»⏺«0» Tests pass. Rendering the three layout variants in light and dark next, then the contact sheets.

«o»✻«0» «o»Churning…«0» «s»(2m 14s · ↓ 4.1k tokens · esc to interrupt)«0»
"#,
        claude_box(128, "«s»Try \"render the overview at 1440 wide\"«0»")
            + "\n  «p»⏵⏵ auto-accept edits on«0» «s»(shift+tab to cycle)«0»"
    )
}

pub(crate) fn claude_permission() -> String {
    let rule = "─".repeat(130);
    let code = [
        (
            "41",
            " ",
            "  export async function handleCallback(request: Request) {",
        ),
        ("42", " ", "    const url = new URL(request.url);"),
        (
            "43",
            "-",
            "    const next = url.searchParams.get(\"next\") ?? \"/\";",
        ),
        ("44", "-", "    return redirect(next);"),
        (
            "43",
            "+",
            "    const next = url.searchParams.get(\"next\") ?? \"/\";",
        ),
        (
            "44",
            "+",
            "    const state = url.searchParams.get(\"state\");",
        ),
        (
            "45",
            "+",
            "    if (!state || !(await verifyState(request, state))) {",
        ),
        (
            "46",
            "+",
            "      return new Response(\"Invalid login state\", { status: 400 });",
        ),
        ("47", "+", "    }"),
        ("48", "+", "    return redirect(withState(next, state));"),
        ("49", " ", "  }"),
    ];
    let mut lines = vec![
        "«k»>«0» users get bounced back to /login after signing in with Google, only in production".to_owned(),
        String::new(),
        "«o»⏺«0» «b»Search«0»(pattern: \"handleCallback\", path: \"src/auth\")".to_owned(),
        "  ⎿  Found 3 files".to_owned(),
        String::new(),
        "«o»⏺«0» «b»Read«0»(src/auth/callback.ts)".to_owned(),
        "  ⎿  Read «b»88«0» lines".to_owned(),
        String::new(),
        "«o»⏺«0» The callback drops the «c»state«0» parameter when it redirects, so the CSRF check on the way back always".to_owned(),
        "  compares against an empty string. Preserving it in the redirect URL fixes the login loop.".to_owned(),
        String::new(),
        "«o»⏺«0» «b»Update«0»(src/auth/callback.ts)".to_owned(),
        String::new(),
        format!("«s»{rule}«0»"),
        " «b»Edit file«0»".to_owned(),
        " «b»src/auth/callback.ts«0»".to_owned(),
        String::new(),
    ];
    for (n, sign, text) in code {
        let body = format!("{sign} {text}");
        let line = format!("{body:<110}");
        lines.push(match sign {
            "-" => format!("  «s»{n:>3}«0» «-»{line}«/»"),
            "+" => format!("  «s»{n:>3}«0» «+»{line}«/»"),
            _ => format!("  «s»{n:>3}«0» {line}"),
        });
    }
    lines.extend([
        String::new(),
        " Do you want to make this edit to «b»callback.ts«0»?".to_owned(),
        " «p»❯ 1. Yes«0»".to_owned(),
        "   2. Yes, allow all edits during this session «s»(shift+tab)«0»".to_owned(),
        "   3. No, and tell Claude what to do differently «s»(esc)«0»".to_owned(),
        String::new(),
    ]);
    lines.join("\n")
}

pub(crate) fn codex_done() -> String {
    r#"«k»›«0» bump the holder protocol minor and regenerate the fixtures

«b»•«0» Ran «c»cargo run -p ubra-proto --example regen_fixtures«0»
  «k»└«0» wrote 14 fixtures to crates/ubra-proto/tests/fixtures

«b»•«0» Protocol minor is now 7. Old Helpers keep attaching; new capabilities are additive.

«k»›«0» the reconnect test flakes about one run in twenty on CI, find out why

«b»•«0» Explored
  «k»└«0» Read holder_session.rs, reconnect.rs
    Search «c»controller_epoch«0» in crates/ubra-engine

«b»•«0» Ran «c»cargo test -p ubra-engine reconnect -- --test-threads 1 --nocapture«0»
  «k»└«0» running 3 tests
    test preserves_session_identity ... «g»ok«0»
    test restores_terminal_snapshot ... «g»ok«0»
    test rejects_stale_controller ... «r»FAILED«0» «k»(1 of 20 runs)«0»

«b»•«0» The attach loop reads the controller epoch before the Holder acknowledges the lease, so a fast
  second attach can observe the previous epoch and be rejected as stale.

«b»•«0» Edited «c»crates/ubra-engine/src/remote/client.rs«0» «g»(+14«0» «r»-6)«0»
    «k»212«0»  «r»-    let epoch = self.lease.epoch();«0»
    «k»212«0»  «g»+    let epoch = self.lease.acknowledged().await?.epoch();«0»

«b»•«0» Ran «c»for i in $(seq 200); do cargo test -q reconnect || break; done«0»
  «k»└«0» test result: ok. 3 passed; 0 failed «k»(200 of 200 runs)«0»

«k»─ Worked for 4m 12s ──────────────────────────────────────────────────────────────────────────────────────────────────────«0»

«b»•«0» Fixed. The stale-controller rejection was a real race, not a slow runner: the test now waits for the
  lease acknowledgement, and 200 consecutive runs pass locally.


«k»›«0» «k»Ask Codex to do anything«0»

  «k»⏎ send   ⇧⏎ newline   ⌃T transcript   ⌃C quit                                                       84% context left«0»
"#
    .to_owned()
}

pub(crate) fn vite_server() -> String {
    r#"«g»~/work/anara/apps/web«0» «k»on«0» «m»main«0» «k»via«0» «g»⬢ v24.3.0«0»
«b»❯«0» pnpm dev

> @anara/web@2.14.0 dev /Users/giga/work/anara/apps/web
> vite --port 5173


  «g»«b»VITE«0» «g»v7.1.4«0»  «k»ready in«0» «b»412«0» «k»ms«0»

  «g»➜«0»  «b»Local«0»:   «c»http://localhost:«b»5173«0»«c»/«0»
  «g»➜«0»  «k»Network: use«0» «b»--host«0» «k»to expose«0»
  «g»➜«0»  «k»press«0» «b»h + enter«0» «k»to show help«0»

«k»10:42:07 AM«0» «c»«b»[vite]«0» «g»page reload«0» «k»src/routes/reader.tsx«0»
«k»10:42:31 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/components/Toolbar.tsx, /src/styles/toolbar.css«0»
«k»10:43:02 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/components/Toolbar.tsx«0»
«k»10:44:15 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/routes/reader.tsx«0»
«k»10:44:16 AM«0» «c»«b»[vite]«0» «y»(client)«0» «y»warning:«0» «k»React does not recognize the `docked` prop on a DOM element.«0»
«k»10:47:52 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/components/Toolbar.tsx«0»
«k»10:51:09 AM«0» «c»«b»[vite]«0» «g»page reload«0» «k»src/lib/pricing.ts«0»
«k»10:51:40 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/routes/pricing.tsx, /src/components/PriceCard.tsx«0»
«k»10:53:12 AM«0» «c»«b»[vite]«0» «g»hmr update«0» «k»/src/components/PriceCard.tsx«0»
"#
    .to_owned()
}

pub(crate) fn cargo_release() -> String {
    r#"«B»~/fun/ubra«0» «k»main*«0»
«m»❯«0» ./scripts/package.sh --release
«b»==>«0» building universal release «k»(aarch64-apple-darwin, x86_64-apple-darwin)«0»
   «g»«b»Compiling«0» ubra-proto v0.1.0 (/Users/giga/fun/ubra/crates/ubra-proto)
   «g»«b»Compiling«0» ubra-terminal-state v0.1.0 (/Users/giga/fun/ubra/crates/ubra-terminal-state)
   «g»«b»Compiling«0» ubra-client v0.1.0 (/Users/giga/fun/ubra/crates/ubra-client)
   «g»«b»Compiling«0» ubra-term v0.1.0 (/Users/giga/fun/ubra/crates/ubra-term)
   «g»«b»Compiling«0» ubra-engine v0.1.0 (/Users/giga/fun/ubra/crates/ubra-engine)
   «g»«b»Compiling«0» ubra-ui v0.1.0 (/Users/giga/fun/ubra/crates/ubra-ui)
«y»«b»warning«0»«b»: unused import: `std::path::Path`«0»
 «B»«b»-->«0» crates/ubra-app/src/session_surfaces.rs:4:5
  «B»«b»|«0»
«B»«b»4«0» «B»«b»|«0» use std::path::Path;
  «B»«b»|«0»     «y»«b»^^^^^^^^^^^^^^^«0»
  «B»«b»|«0»
  «B»«b»=«0» «b»note«0»: `#[warn(unused_imports)]` on by default

   «g»«b»Compiling«0» ubra-web v0.1.0 (/Users/giga/fun/ubra/crates/ubra-web)
   «g»«b»Compiling«0» ubra-remote v0.1.0 (/Users/giga/fun/ubra/crates/ubra-remote)
   «g»«b»Compiling«0» ubra-app v0.1.0 (/Users/giga/fun/ubra/crates/ubra-app)
    «g»«b»Finished«0» `release` profile [optimized] target(s) in 3m 41s
«b»==>«0» verifying universal binaries
    ubra              «g»✓«0» arm64 x86_64   48.2 MB
    ubrad-rs       «g»✓«0» arm64 x86_64   21.7 MB
    ubra-remote       «g»✓«0» linux x86_64 aarch64   3.1 MB
«b»==>«0» signing «k»Developer ID Application: Example Corp (EXAMPLE1234)«0»
«b»==>«0» notarizing ubra-0.1.0.dmg
    submission 7f3c0a1e-44b2-4c1d-9a0e-2b61f0d3e5a8
    status: «y»In Progress«0» «k»(1m 12s)«0» ▍
"#
    .to_owned()
}

pub(crate) fn htop() -> String {
    let bar = |label: &str, fill: usize, used: &str| {
        format!(
            "  «c»{label}«0»«b»[«0»«g»{}«0»«r»{}«0»{}«k»{used:>9}«0»«b»]«0»",
            "|".repeat(fill * 2 / 3),
            "|".repeat(fill / 3),
            " ".repeat(40usize.saturating_sub(fill)),
        )
    };
    let mut lines = vec![
        format!("{}   {}", bar("0", 31, "78.4%"), bar("4", 12, "29.0%")),
        format!("{}   {}", bar("1", 24, "61.2%"), bar("5", 9, "22.8%")),
        format!("{}   {}", bar("2", 36, "90.1%"), bar("6", 6, "14.3%")),
        format!("{}   {}", bar("3", 18, "44.9%"), bar("7", 4, "9.6%")),
        format!(
            "  «c»Mem«0»«b»[«0»«g»{}«0»«B»{}«0»«y»{}«0»{}«k»21.3G/32.0G«0»«b»]«0»   «c»Tasks:«0» «b»412«0», «g»1893 thr«0»; «b»6«0» running",
            "|".repeat(20),
            "|".repeat(4),
            "|".repeat(5),
            " ".repeat(8)
        ),
        format!(
            "  «c»Swp«0»«b»[«0»«r»{}«0»{}«k»1.12G/4.00G«0»«b»]«0»   «c»Load average:«0» «b»4.82«0» 3.97 3.10",
            "|".repeat(8),
            " ".repeat(29)
        ),
        "                                                     «c»Uptime:«0» «b»6 days, 04:12:55«0»".to_owned(),
        String::new(),
        "«[30;42]»    PID USER       PRI  NI  VIRT   RES   SHR S  CPU%▽ MEM%   TIME+  Command                                                    «0»".to_owned(),
    ];
    let rows = [
        (
            "71204",
            "giga",
            "R",
            "92.4",
            "3.1",
            "12:41.07",
            "cargo build --release -p ubra-app",
        ),
        (
            "71388",
            "giga",
            "R",
            "88.0",
            "2.4",
            "9:02.33",
            "rustc --crate-name ubra_app --edition=2024",
        ),
        (
            "5321",
            "giga",
            "S",
            "41.7",
            "4.8",
            "2h14:08",
            "claude --resume 8f1c…",
        ),
        ("5519", "giga", "S", "22.3", "1.9", "58:17.40", "codex"),
        (
            "412",
            "giga",
            "S",
            "12.9",
            "6.2",
            "4h01:55",
            "/Applications/ubra.app/Contents/MacOS/ubra",
        ),
        (
            "413",
            "giga",
            "S",
            "6.1",
            "0.7",
            "1h12:20",
            "ubrad-rs --socket /Users/giga/.ubra/daemon.sock",
        ),
        (
            "8810",
            "giga",
            "S",
            "4.4",
            "2.1",
            "18:09.61",
            "node vite --port 5173",
        ),
        (
            "617",
            "root",
            "S",
            "0.8",
            "0.2",
            "3:21.88",
            "/usr/libexec/logd",
        ),
        ("702", "giga", "S", "0.5", "0.3", "2:10.04", "fish"),
        (
            "1188",
            "giga",
            "S",
            "0.3",
            "1.1",
            "7:55.29",
            "Safari Web Content",
        ),
        (
            "233",
            "root",
            "S",
            "0.2",
            "0.1",
            "1:02.77",
            "/usr/sbin/cfprefsd daemon",
        ),
        (
            "1402",
            "giga",
            "S",
            "0.1",
            "0.0",
            "0:03.21",
            "ssh -T forge ubra-remote attach",
        ),
    ];
    for (i, (pid, user, state, cpu, mem, time, cmd)) in rows.iter().enumerate() {
        let line = format!(
            "{pid:>7} {user:<9}  24   0  412G  {:>4}M  {:>4}M {} {cpu:>5} {mem:>4} {time:>9}  {cmd}",
            180 + i * 37,
            40 + i * 11,
            if *state == "R" && i > 0 {
                "«g»R«0»"
            } else {
                state
            },
        );
        if i == 0 {
            lines.push(format!("«[30;46]»{line:<131}«0»"));
        } else {
            lines.push(line);
        }
    }
    for _ in 0..(ROWS as usize - 1 - lines.len()) {
        lines.push(String::new());
    }
    lines.push(
        "«[30;46]»F1«0»Help  «[30;46]»F2«0»Setup «[30;46]»F3«0»Search«[30;46]»F4«0»Filter«[30;46]»F5«0»Tree  «[30;46]»F6«0»SortBy«[30;46]»F7«0»Nice -«[30;46]»F8«0»Nice +«[30;46]»F9«0»Kill  «[30;46]»F10«0»Quit"
            .to_owned(),
    );
    lines.join("\n")
}

pub(crate) fn git_graph() -> String {
    r#"«b»•«0» Pushed «c»feat/calm-overview«0» to origin «k»(5 commits)«0»

«k»›«0» squash the fixups on this branch and tidy the history before review

«b»•«0» Ran «c»git log --graph --oneline --decorate -n 18«0»

«r»*«0» «y»a41c9e2«0» «y»(«0»«c»«b»HEAD«0» «y»->«0» «g»«b»feat/calm-overview«0»«y»)«0» Paint overview cards from the resident grid
«r»*«0» «y»7d20b11«0» Size cards to the pane's own aspect ratio
«r»*«0» «y»e98f3c4«0» Replace the overview header with a single search field
«r»*«0» «y»0c55a7e«0» Draw the card title strip above the miniature
«r»*«0»   «y»5b6e1d0«0» Merge branch 'main' into feat/calm-overview
«g»|«0»«r»\«0»
«g»|«0» «r»*«0» «y»89653ec«0» «y»(«0»«r»«b»origin/main«0»«y»,«0» «g»«b»main«0»«y»)«0» Re-render only the sidebar rows that change (#517)
«g»|«0» «r»*«0» «y»d3c952f«0» Add a glass transparency slider to Settings › Appearance (#537)
«g»|«0» «r»*«0» «y»cbf4fc8«0» Never drop a session record over an unreadable file (#535)
«g»|«0»«g»/«0»
«g»*«0» «y»3fa01b9«0» fixup! Size cards to the pane's own aspect ratio
«g»*«0» «y»12bb7c0«0» fixup! Paint overview cards from the resident grid
«g»*«0» «y»ee4d0a3«0» Keep arrow navigation working without visible hints

«b»•«0» Two fixups belong to earlier commits and the merge can go. Rebasing onto «g»origin/main«0»:

«b»•«0» Ran «c»git rebase --autosquash --rebase-merges=no origin/main«0»
  «k»└«0» Successfully rebased and updated refs/heads/feat/calm-overview.

«b»•«0» History is now five linear commits on top of «g»main«0», no fixups, no merge commit.

«k»›«0» «k»Ask Codex to do anything«0»

  «k»⏎ send   ⇧⏎ newline   ⌃T transcript   ⌃C quit                                                       61% context left«0»
"#
    .to_owned()
}

pub(crate) fn vim_review() -> String {
    let code = [
        ("1", "«m»use«0» std::collections::HashMap;"),
        ("2", ""),
        ("3", "«m»use«0» ubra_proto::{SessionId, SessionRecord};"),
        ("4", "«m»use«0» gpui::{div, prelude::*, px, Window};"),
        ("5", ""),
        (
            "6",
            "«k»/// Cards are the pane, smaller: same grid, same aspect ratio.«0»",
        ),
        (
            "7",
            "«m»pub«0»(«m»crate«0») «m»fn«0» «B»card_size«0»(width: «y»f32«0», cols: «y»u16«0», rows: «y»u16«0», cell: «y»CellMetrics«0») -> «y»Size«0» {",
        ),
        (
            "8",
            "    «m»let«0» pane_w = cell.cell_width * «y»f32«0»::from(cols) + «r»2.0«0» * PANE_PAD;",
        ),
        (
            "9",
            "    «m»let«0» pane_h = cell.line_height * «y»f32«0»::from(rows) + «r»2.0«0» * PANE_PAD;",
        ),
        ("10", "    «m»let«0» scale = width / pane_w;"),
        ("11", "    size(px(width), px(pane_h * scale))"),
        ("12", "}"),
        ("13", ""),
        (
            "14",
            "«m»pub«0»(«m»crate«0») «m»fn«0» «B»columns_for«0»(width: «y»f32«0») -> «y»usize«0» {",
        ),
        ("15", "    «m»match«0» width {"),
        ("16", "        w «m»if«0» w < «r»900.0«0» => «r»1«0»,"),
        ("17", "        w «m»if«0» w < «r»1500.0«0» => «r»2«0»,"),
        ("18", "        _ => «r»3«0»,"),
        ("19", "    }"),
        ("20", "}"),
    ];
    let mut lines: Vec<String> = code
        .iter()
        .map(|(n, text)| format!("«k»{n:>4}«0» {text}"))
        .collect();
    for _ in lines.len()..(ROWS as usize - 2) {
        lines.push("«B»   ~«0»".to_owned());
    }
    lines.push(format!(
        "«[30;42]» NORMAL «[0;7]» overview_layout.rs «0»«[48;2;60;60;60]»{:<85}«[30;42]» rust  utf-8  7:18 «0»",
        ""
    ));
    lines.push("\"overview_layout.rs\" 20L, 642B written".to_owned());
    lines.join("\n")
}
