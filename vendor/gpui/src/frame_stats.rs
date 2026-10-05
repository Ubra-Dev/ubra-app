//! UBRA PATCH (frame statistics): where one window frame's CPU time went.
//!
//! `Window::draw` stamps each phase of the frame it builds and counts the
//! views it renders and replays. Ubra reads the finished frame's numbers for
//! telemetry and, from inside the frame's own paint pass, the phases that have
//! already finished. It costs a few `Instant::now()` calls and integer
//! increments per frame, never per element.

use std::time::{Duration, Instant};

/// One window frame, by phase. Phases are contiguous: `layout` runs from the
/// start of `Window::draw` to the end of the root's layout request, `prepaint`
/// from there to the end of prepaint (deferred draws and tooltips included),
/// `paint` to the end of the scene, `a11y` to the end of the accessibility
/// update and `finish` to the end of `Window::draw` (sorting the scene,
/// swapping frames, focus listeners).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    /// Render of the root and of every uncached view, plus layout requests.
    pub layout: Duration,
    /// Taffy layout, renders of cached views that missed their cache, and
    /// element prepaint.
    pub prepaint: Duration,
    /// Scene construction.
    pub paint: Duration,
    /// Building and sending the accessibility tree (zero while inactive).
    pub a11y: Duration,
    /// Sorting the scene and swapping frames.
    pub finish: Duration,
    /// `Render::render` calls this frame (cached views that missed included).
    pub views_rendered: u32,
    /// Cached views replayed from the previous frame.
    pub views_reused: u32,
    /// Assistive technology was attached, which also makes every cached view
    /// nested in a re-rendering one render again.
    pub a11y_active: bool,
}

impl FrameStats {
    /// The whole frame.
    pub fn total(&self) -> Duration {
        self.layout + self.prepaint + self.paint + self.a11y + self.finish
    }
}

/// The frame being built.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameStatsBuilder {
    pub(crate) started: Option<Instant>,
    pub(crate) laid_out: Option<Instant>,
    pub(crate) prepainted: Option<Instant>,
    pub(crate) painted: Option<Instant>,
    pub(crate) a11y_done: Option<Instant>,
    pub(crate) views_rendered: u32,
    pub(crate) views_reused: u32,
    pub(crate) a11y_active: bool,
}

impl Default for FrameStatsBuilder {
    fn default() -> Self {
        Self {
            started: None,
            laid_out: None,
            prepainted: None,
            painted: None,
            a11y_done: None,
            views_rendered: 0,
            views_reused: 0,
            a11y_active: false,
        }
    }
}

impl FrameStatsBuilder {
    pub(crate) fn start(&mut self, at: Instant) {
        *self = Self {
            started: Some(at),
            ..Self::default()
        };
    }

    /// The phases finished so far; an unfinished phase runs to `now`.
    pub(crate) fn snapshot(&self, now: Instant) -> FrameStats {
        let Some(started) = self.started else {
            return FrameStats::default();
        };
        let span =
            |from: Instant, to: Option<Instant>| to.unwrap_or(now).saturating_duration_since(from);
        let laid_out = self.laid_out.unwrap_or(now);
        let prepainted = self.prepainted.unwrap_or(now).max(laid_out);
        let painted = self.painted.unwrap_or(now).max(prepainted);
        let a11y_done = self.a11y_done.unwrap_or(now).max(painted);
        FrameStats {
            layout: span(started, self.laid_out),
            prepaint: if self.laid_out.is_some() {
                span(laid_out, self.prepainted)
            } else {
                Duration::ZERO
            },
            paint: if self.prepainted.is_some() {
                span(prepainted, self.painted)
            } else {
                Duration::ZERO
            },
            a11y: if self.painted.is_some() {
                span(painted, self.a11y_done)
            } else {
                Duration::ZERO
            },
            finish: if self.a11y_done.is_some() {
                now.saturating_duration_since(a11y_done)
            } else {
                Duration::ZERO
            },
            views_rendered: self.views_rendered,
            views_reused: self.views_reused,
            a11y_active: self.a11y_active,
        }
    }
}
