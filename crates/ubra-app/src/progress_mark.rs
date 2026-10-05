//! Terminal progress (`OSC 9;4`) as a ring in a session's leading mark slot,
//! on its sidebar row and its strip tab, where activity already lives.
//!
//! The ring has four states: normal (the theme's ink), error (red), paused or
//! warning (amber), and indeterminate (a short arc that laps the ring, and
//! holds still as a dashed ring under reduce motion).
//!
//! Like the activity mark, nothing here owns a clock: the caller hands in the
//! shared activity frame and repaints on its cadence.

use std::f32::consts::TAU;

use gpui::{
    AnyElement, Bounds, IntoElement, PathBuilder, Pixels, Point, Rgba, Window, canvas, div, point,
    prelude::*, px,
};
use ubra_proto::{AgentKind, SessionRecord, TerminalProgress, TerminalProgressState as State};
use ubra_ui::{Ink, SemanticColors, StatusState};

/// One progress indicator as a row or tab draws it. Part of their cached
/// props, so a report that did not move the percent re-renders nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ProgressFace {
    pub progress: TerminalProgress,
    /// The shared activity frame, 0–7. Zero unless the face animates.
    pub frame: usize,
    /// Reduce motion: an indeterminate face holds a still pose.
    pub still: bool,
}

impl ProgressFace {
    /// Whether this face needs the activity tick.
    pub(crate) fn animates(&self) -> bool {
        self.progress.state == State::Indeterminate && !self.still
    }
}

/// The progress a session's row shows, if any.
///
/// Agents such as Claude Code send `9;4;3` as their own "working" signal,
/// which the activity mark already shows, so an Agent's indeterminate report
/// is not drawn twice. Anything with a percent, and every report from a plain
/// terminal, is shown.
pub(crate) fn shown(session: &SessionRecord) -> Option<TerminalProgress> {
    let progress = session.terminal_progress?;
    let agent = session.effective_kind() != &AgentKind::SHELL;
    (!(agent && progress.state == State::Indeterminate)).then_some(progress)
}

pub(crate) fn face(session: &SessionRecord, frame: usize, still: bool) -> Option<ProgressFace> {
    shown(session).map(|progress| {
        let mut face = ProgressFace {
            progress,
            frame: 0,
            still,
        };
        if face.animates() {
            face.frame = frame % 8;
        }
        face
    })
}

fn ink(state: State, colors: SemanticColors) -> Rgba {
    match state {
        State::Error => Ink::on_surface(Ink::DANGER, colors),
        State::Paused => Ink::on_surface(Ink::ATTENTION, colors),
        State::Normal | State::Indeterminate | State::Unknown => colors.primary,
    }
}

/// The unfilled part of the ring: the ink, barely there.
fn track(state: State, colors: SemanticColors) -> Rgba {
    let hue = ink(state, colors);
    match state {
        State::Error | State::Paused => hue.alpha(0.26),
        _ => hue.alpha(0.16),
    }
}

/// How much of the ring is filled, 0–1.
fn filled(progress: TerminalProgress) -> f32 {
    f32::from(progress.percent.min(100)) / 100.0
}

/// Where the indeterminate sweep starts, 0–1, one lap per eight frames.
fn lap(face: &ProgressFace) -> f32 {
    (face.frame % 8) as f32 / 8.0
}

/// The ring in an 18 pt mark slot. A tab's agent `logo` sits inside it; a
/// sidebar row, whose identity glyph is at its other end, has none.
pub(crate) fn progress_mark(
    face: ProgressFace,
    colors: SemanticColors,
    logo: Option<AnyElement>,
) -> AnyElement {
    let around_logo = logo.is_some();
    div()
        .size(px(18.0))
        .flex_none()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| paint_ring(bounds, face, colors, around_logo, window),
            )
            .absolute()
            .inset_0(),
        )
        .children(logo)
        .into_any_element()
}

/// A sidebar row's leading mark: the progress ring while a program
/// reports progress, else the activity mark. A question the session asks,
/// and sleep, still win: they are what the user has to act on.
pub(crate) fn leading_mark(
    state: StatusState,
    frame: usize,
    progress: Option<ProgressFace>,
    colors: SemanticColors,
) -> AnyElement {
    if !matches!(
        state,
        StatusState::NeedsInput { .. } | StatusState::Hibernated
    ) && let Some(face) = progress
    {
        return progress_mark(face, colors, None);
    }
    crate::session_presentation::activity_mark(state, frame, colors)
}

/// A point `turn` of the way clockwise around a circle from 12 o'clock.
fn on_circle(center: Point<Pixels>, radius: f32, turn: f32) -> Point<Pixels> {
    let angle = turn * TAU;
    point(
        center.x + px(radius * angle.sin()),
        center.y - px(radius * angle.cos()),
    )
}

/// Adds a clockwise arc from `start` for `turns` of a lap, in quarter-lap
/// pieces so a full circle is still well formed.
fn arc(path: &mut PathBuilder, center: Point<Pixels>, radius: f32, start: f32, turns: f32) {
    path.move_to(on_circle(center, radius, start));
    let pieces = (turns * 4.0).ceil().max(1.0) as usize;
    for piece in 1..=pieces {
        let at = start + turns * piece as f32 / pieces as f32;
        path.arc_to(
            point(px(radius), px(radius)),
            px(0.0),
            false,
            true,
            on_circle(center, radius, at),
        );
    }
}

fn stroke_arc(
    window: &mut Window,
    center: Point<Pixels>,
    radius: f32,
    width: f32,
    (start, turns): (f32, f32),
    dashes: Option<f32>,
    color: Rgba,
) {
    if turns <= 0.0 {
        return;
    }
    let mut path = PathBuilder::stroke(px(width));
    if let Some(dash) = dashes {
        path = path.dash_array(&[px(dash), px(dash)]);
    }
    arc(&mut path, center, radius, start, turns);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
    // Round caps, which the stroke options cannot name from here.
    if dashes.is_none() && turns < 1.0 {
        for turn in [start, start + turns] {
            dot(window, on_circle(center, radius, turn), width / 2.0, color);
        }
    }
}

fn dot(window: &mut Window, center: Point<Pixels>, radius: f32, color: Rgba) {
    let mut path = PathBuilder::fill();
    arc(&mut path, center, radius, 0.0, 1.0);
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_ring(
    bounds: Bounds<Pixels>,
    face: ProgressFace,
    colors: SemanticColors,
    around_logo: bool,
    window: &mut Window,
) {
    let state = face.progress.state;
    let center = bounds.center();
    // Alone, the ring matches the 14 px activity mark; around a tab's logo
    // it takes the whole slot so the logo keeps room to read.
    let (diameter, width) = if around_logo {
        (17.0, 1.5)
    } else {
        (13.5, 1.75)
    };
    let radius = (diameter - width) / 2.0;
    let hue = ink(state, colors);
    if state == State::Indeterminate && face.still {
        stroke_arc(
            window,
            center,
            radius,
            width,
            (0.0, 1.0),
            Some(radius * TAU / 12.0),
            hue.alpha(0.7),
        );
        return;
    }
    stroke_arc(
        window,
        center,
        radius,
        width,
        (0.0, 1.0),
        None,
        track(state, colors),
    );
    let span = if state == State::Indeterminate {
        (lap(&face), 0.28)
    } else {
        (0.0, filled(face.progress))
    };
    stroke_arc(window, center, radius, width, span, None, hue);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(kind: AgentKind, state: State) -> SessionRecord {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .find(|session| session.kind == AgentKind::SHELL)
            .expect("shell fixture");
        session.kind = kind;
        session.terminal_progress = Some(TerminalProgress { state, percent: 40 });
        session
    }

    #[test]
    fn an_agents_own_working_signal_is_not_drawn_twice() {
        assert!(shown(&session(AgentKind::SHELL, State::Indeterminate)).is_some());
        assert!(shown(&session(AgentKind::CLAUDE_CODE, State::Indeterminate)).is_none());
        assert!(shown(&session(AgentKind::CLAUDE_CODE, State::Normal)).is_some());
        // Claude typed at a shell prompt is an Agent for this purpose.
        let mut typed = session(AgentKind::SHELL, State::Indeterminate);
        typed.foreground_agent = Some(AgentKind::CLAUDE_CODE);
        assert!(shown(&typed).is_none());
    }

    #[test]
    fn only_a_moving_sweep_reads_the_frame() {
        let shell = session(AgentKind::SHELL, State::Normal);
        assert_eq!(face(&shell, 5, false).unwrap().frame, 0);
        let busy = session(AgentKind::SHELL, State::Indeterminate);
        let moving = face(&busy, 13, false).unwrap();
        assert_eq!(moving.frame, 5);
        assert!(moving.animates());
        let still = face(&busy, 5, true).unwrap();
        assert_eq!(still.frame, 0);
        assert!(!still.animates());
    }
}
