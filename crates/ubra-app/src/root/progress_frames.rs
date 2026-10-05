//! Renders the terminal progress ring (`OSC 9;4`) in the sidebar and the tab
//! strip, because an agent session cannot record the screen.
//!
//! ```sh
//! UBRA_VISUAL_OUTPUT=/tmp/progress \
//!   cargo test -p ubra-app --bin ubra-gui -- --ignored render_progress_frames
//! ```
//!
//! Writes `states-{sidebar,strip}-{dark,light}.png` (every state at once),
//! `still-{sidebar,strip}-dark.png` (the same under reduce motion) and
//! `build-{sidebar,strip}/frame_NNNN.png`: a build climbing to 100 % and
//! clearing, a test run that fails at 62 %, and an indeterminate job, one
//! frame per 125 ms activity tick.

use std::time::Duration;

use gpui::{HeadlessAppContext, size};
use ubra_proto::{TerminalProgress, TerminalProgressState as State, TitleSource};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;

/// What a program last reported: a state and percent, or nothing.
type Report = Option<(State, u8)>;

/// Terminals the Engine has named after their foreground program, each with
/// the progress it last reported.
const STATES: [(&str, &str, Report); 7] = [
    ("progress-0", "cargo build", Some((State::Normal, 0))),
    ("progress-35", "cargo test", Some((State::Normal, 35))),
    ("progress-80", "pnpm install", Some((State::Normal, 80))),
    ("progress-100", "make release", Some((State::Normal, 100))),
    ("progress-error", "cargo clippy", Some((State::Error, 62))),
    ("progress-paused", "brew upgrade", Some((State::Paused, 48))),
    (
        "progress-busy",
        "docker build",
        Some((State::Indeterminate, 0)),
    ),
];

/// The build GIF's script, one entry per frame: the build's percent (`None`
/// once cleared) and the test run's report.
fn build_script() -> Vec<(Report, Report)> {
    let mut frames = Vec::new();
    for step in 0..=24u8 {
        let build = (State::Normal, (u32::from(step) * 100 / 24) as u8);
        let test = if step <= 15 {
            (State::Normal, step * 4)
        } else {
            (State::Error, 62)
        };
        frames.push((Some(build), Some(test)));
    }
    for _ in 0..4 {
        frames.push((Some((State::Normal, 100)), Some((State::Error, 62))));
    }
    for _ in 0..6 {
        frames.push((None, Some((State::Error, 62))));
    }
    frames
}

fn progress(report: Report) -> Option<TerminalProgress> {
    report.map(|(state, percent)| TerminalProgress { state, percent })
}

struct Scene {
    horizontal: bool,
    light: bool,
    still: bool,
}

fn open(
    cx: &mut HeadlessAppContext,
    scene: &Scene,
    sessions: &[(&str, &str, Report)],
) -> (gpui::AnyWindowHandle, Arc<crate::store::StoreRuntime>) {
    cx.update(|cx| cx.set_reduce_motion(scene.still));
    let services = test_services();
    let store = services.store.clone();
    {
        let mut store = store.store.write().unwrap();
        store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
        let base = (**store
            .sessions()
            .get(&SessionId::new("preview-shell"))
            .unwrap())
        .clone();
        for (id, title, report) in sessions {
            let mut terminal = base.clone();
            terminal.id = SessionId::new(*id);
            terminal.title = (*title).into();
            terminal.title_source = TitleSource::TerminalTitle;
            terminal.listening_ports = None;
            terminal.status = ubra_proto::SessionStatus::Working;
            terminal.terminal_progress = progress(*report);
            store.upsert_session(terminal);
        }
        store.select(SessionId::new(sessions[1].0));
        store
            .update_preferences(|prefs| {
                prefs.terminal_theme = if scene.light {
                    "github-light"
                } else {
                    "rose-pine"
                }
                .into();
            })
            .unwrap();
    }
    let horizontal = scene.horizontal;
    let window = cx
        .open_window(size(px(1480.0), px(1000.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .set_tab_orientation(
                            if horizontal {
                                crate::store::TabOrientation::Horizontal
                            } else {
                                crate::store::TabOrientation::Vertical
                            },
                            cx,
                        )
                        .unwrap();
                });
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    // Entrance motion runs on the wall clock; let it finish.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        refresh(cx, window.into(), false);
    }
    (window.into(), store)
}

/// Repaints, first stepping the shared activity frame when `tick`.
fn refresh(cx: &mut HeadlessAppContext, window: gpui::AnyWindowHandle, tick: bool) {
    cx.update_window(window, |view, window, cx| {
        view.downcast::<RootView>().unwrap().update(cx, |root, cx| {
            root.sidebar.update(cx, |sidebar, cx| {
                if tick {
                    sidebar.advance_activity_frame_for_test(cx);
                }
                cx.notify();
            });
        });
        window.refresh();
    })
    .unwrap();
    cx.run_until_parked();
}

fn save(cx: &mut HeadlessAppContext, window: gpui::AnyWindowHandle, path: std::path::PathBuf) {
    cx.capture_screenshot(window).unwrap().save(path).unwrap();
}

fn close(cx: &mut HeadlessAppContext, window: gpui::AnyWindowHandle) {
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}

#[test]
#[ignore = "writes progress stills and frames to the UBRA_VISUAL_OUTPUT directory"]
fn render_progress_frames() {
    let output = std::path::PathBuf::from(std::env::var("UBRA_VISUAL_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(ubra_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));
    // The shipped raster marks, which honour their size; the vector fallback
    // fills whatever box holds it.
    ubra_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark);

    for (horizontal, place) in [(false, "sidebar"), (true, "strip")] {
        for (light, still, name) in [
            (false, false, "states-{}-dark"),
            (true, false, "states-{}-light"),
            (false, true, "still-{}-dark"),
        ] {
            let scene = Scene {
                horizontal,
                light,
                still,
            };
            let (window, _) = open(&mut cx, &scene, &STATES);
            // Two ticks so the indeterminate sweep is mid-lap, not at rest.
            refresh(&mut cx, window, true);
            refresh(&mut cx, window, true);
            save(
                &mut cx,
                window,
                output.join(format!("{}.png", name.replace("{}", place))),
            );
            close(&mut cx, window);
        }

        let scene = Scene {
            horizontal,
            light: false,
            still: false,
        };
        let script = build_script();
        let sessions = [
            ("build", "cargo build", script[0].0),
            ("test", "cargo test", script[0].1),
            ("busy", "docker build", Some((State::Indeterminate, 0))),
        ];
        let (window, store) = open(&mut cx, &scene, &sessions);
        let frames = output.join(format!("build-{place}"));
        std::fs::create_dir_all(&frames).unwrap();
        for (index, (build, test)) in script.into_iter().enumerate() {
            {
                let mut store = store.store.write().unwrap();
                for (id, report) in [("build", build), ("test", test)] {
                    let mut session =
                        (**store.sessions().get(&SessionId::new(id)).unwrap()).clone();
                    session.terminal_progress = progress(report);
                    store.upsert_session(session);
                }
            }
            refresh(&mut cx, window, true);
            save(
                &mut cx,
                window,
                frames.join(format!("frame_{index:04}.png")),
            );
        }
        close(&mut cx, window);
    }
}
