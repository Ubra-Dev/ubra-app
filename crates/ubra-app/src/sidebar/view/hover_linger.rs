//! Hover-out trails for the session rows and the horizontal tabs.
//!
//! Hover-in is untouched: rows still take their full hover fill the frame the
//! pointer arrives. What this adds is the row the pointer just *left*, which
//! keeps a fading copy of the hover fill for `Motion::HOVER_LINGER_TIME`, so a
//! sweep down the list reads as a short soft trail instead of rows blinking.
//! See [`ubra_ui::hover_trail`] for the model itself.
use super::*;

impl Sidebar {
    /// Brings the session-row trail up to date before rows are built: follows
    /// `ui.hovered_session`, and drops any trail whose row is gone or has just
    /// become the selection (selection replaces hover at once).
    pub(super) fn observe_session_hover(
        &mut self,
        visible: &HashSet<&SessionId>,
        selected: Option<&SessionId>,
        reduce_motion: bool,
    ) {
        let now = Instant::now();
        let trail = &mut self.hover_trails.sessions;
        if reduce_motion {
            trail.clear_fades();
        }
        trail.set_hovered(self.ui.hovered_session.as_ref(), now, reduce_motion);
        trail.retain(|id| visible.contains(id));
        if let Some(selected) = selected {
            trail.forget(selected);
        }
        trail.prune(now);
    }

    /// The same bookkeeping for the horizontal strip, whose tabs report hover
    /// through their own `on_hover` rather than `ui.hovered_session`.
    pub(super) fn observe_tab_hover(
        &mut self,
        tabs: &[Arc<SessionRecord>],
        selected: Option<&SessionId>,
        reduce_motion: bool,
    ) {
        let trail = &mut self.hover_trails.tabs;
        if reduce_motion {
            trail.clear_fades();
        }
        trail.retain(|id| tabs.iter().any(|session| &session.id == id));
        if let Some(selected) = selected {
            trail.forget(selected);
        }
        trail.prune(Instant::now());
    }

    /// Fraction of the hover fill a non-hovered, unselected row still wears.
    pub(super) fn session_hover_linger(&self, id: &SessionId) -> f32 {
        let trail = &self.hover_trails.sessions;
        if trail.is_fading() {
            trail.linger(id, Instant::now())
        } else {
            0.0
        }
    }

    pub(super) fn tab_hover_linger(&self, id: &SessionId) -> f32 {
        let trail = &self.hover_trails.tabs;
        if trail.is_fading() {
            trail.linger(id, Instant::now())
        } else {
            0.0
        }
    }
}

/// One trail per surface: the rows and the strip can both be on screen, and a
/// row hovered in one must not fade in the other.
#[derive(Default)]
pub(super) struct HoverTrails {
    pub(super) sessions: ubra_ui::HoverTrail<SessionId>,
    pub(super) tabs: ubra_ui::HoverTrail<SessionId>,
}

impl HoverTrails {
    /// Whether a row or tab is still fading, so the painting surface asks
    /// the display link for its next frame (`request_motion_frame`). The
    /// request lapses with the last trail, so a still pointer costs no
    /// frames. A fading row's or tab's `hover_linger` prop changes each
    /// frame, so only it re-renders; every other cached view is reused.
    pub(super) fn is_fading(&self) -> bool {
        self.sessions.is_fading() || self.tabs.is_fading()
    }
}
