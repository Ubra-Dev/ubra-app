//! Hover highlights that arrive at once and leave softly.
//!
//! GPUI's declarative `.hover()` style switches a fill on and off in a single
//! frame, so sweeping the pointer down a list reads as rows flickering. The
//! fix is asymmetric on purpose: hover-in stays instant (a highlight that
//! ramps in reads as lag), and only the row the pointer just left keeps a
//! fading copy of its fill for [`Motion::HOVER_LINGER_TIME`]. A fast sweep then
//! leaves a short soft trail instead of a strobe.
//!
//! This is a small keyed model owned by the view that paints the rows. The
//! view reports which key is hovered (from its own hover state or from an
//! element's `on_hover`), asks [`HoverTrail::linger`] for the fading level of
//! each row it paints, and keeps frames coming only while
//! [`HoverTrail::is_fading`] says a fade is still running. At rest it holds no
//! entries and costs nothing.
//!
//! Rows that stop being hover candidates (selected, removed, scrolled out of
//! the list) are dropped with [`HoverTrail::forget`] / [`HoverTrail::retain`],
//! so a trail can never outlive the row it belongs to. Under reduce motion no
//! trail is ever started.

use std::time::Instant;

use crate::Motion;

/// Which row is hovered, and which rows are still fading out after the
/// pointer left them.
#[derive(Debug)]
pub struct HoverTrail<K> {
    hovered: Option<K>,
    /// Rows the pointer has left, with the moment it left. A sweep leaves a
    /// handful of these at most: each expires after one short fade.
    fading: Vec<(K, Instant)>,
}

impl<K> Default for HoverTrail<K> {
    fn default() -> Self {
        Self {
            hovered: None,
            fading: Vec::new(),
        }
    }
}

impl<K: Clone + PartialEq> HoverTrail<K> {
    /// Records the currently hovered key. When it changes, the previous key
    /// starts fading from `now` (unless `reduce_motion`), and the new key stops
    /// fading at once: re-entering a fading row snaps it back to full hover.
    pub fn set_hovered(&mut self, hovered: Option<&K>, now: Instant, reduce_motion: bool) {
        if self.hovered.as_ref() == hovered {
            return;
        }
        if let Some(previous) = self.hovered.take() {
            self.fading.retain(|(key, _)| key != &previous);
            if !reduce_motion {
                self.fading.push((previous, now));
            }
        }
        if let Some(next) = hovered {
            self.fading.retain(|(key, _)| key != next);
        }
        self.hovered = hovered.cloned();
    }

    /// Adapter for an element's `on_hover(bool)` callback. A leave only
    /// clears hover when it belongs to the hovered key, because GPUI can
    /// deliver the previous row's leave after the next row's enter.
    pub fn hover_event(&mut self, key: &K, hovering: bool, now: Instant, reduce_motion: bool) {
        if hovering {
            self.set_hovered(Some(key), now, reduce_motion);
        } else if self.hovered.as_ref() == Some(key) {
            self.set_hovered(None, now, reduce_motion);
        }
    }

    pub fn hovered(&self) -> Option<&K> {
        self.hovered.as_ref()
    }

    /// Fraction (0..=1) of the hover fill a row that is *not* hovered should
    /// still wear at `now`. The hovered row itself returns 0: its full fill is
    /// painted by whatever already paints hover, so the trail never delays or
    /// doubles it.
    #[must_use]
    pub fn linger(&self, key: &K, now: Instant) -> f32 {
        self.fading
            .iter()
            .find(|(fading, _)| fading == key)
            .map_or(0.0, |(_, left_at)| {
                let elapsed = now.saturating_duration_since(*left_at);
                Motion::hover_linger(
                    elapsed.as_secs_f32() / Motion::HOVER_LINGER_TIME.as_secs_f32(),
                )
            })
    }

    /// Drops finished fades and reports whether any is still running, which
    /// is the only time the owning view needs another frame.
    pub fn prune(&mut self, now: Instant) -> bool {
        self.fading.retain(|(_, left_at)| {
            now.saturating_duration_since(*left_at) < Motion::HOVER_LINGER_TIME
        });
        !self.fading.is_empty()
    }

    #[must_use]
    pub fn is_fading(&self) -> bool {
        !self.fading.is_empty()
    }

    /// Ends any trail on `key` immediately, for a row whose selection fill
    /// replaces hover at once.
    pub fn forget(&mut self, key: &K) {
        self.fading.retain(|(fading, _)| fading != key);
    }

    /// Keeps trails (and hover) only for keys still on screen, so a removed
    /// row cannot leave a ghost fill on whichever row reuses its slot.
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.fading.retain(|(key, _)| keep(key));
        if self.hovered.as_ref().is_some_and(|key| !keep(key)) {
            self.hovered = None;
        }
    }

    /// Stops every fade at once (reduce motion switched on, list replaced).
    pub fn clear_fades(&mut self) {
        self.fading.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn hover_in_is_immediate_and_carries_no_trail() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.set_hovered(Some(&1), start, false);
        assert_eq!(trail.hovered(), Some(&1));
        assert_eq!(trail.linger(&1, start), 0.0);
        assert!(!trail.is_fading());
    }

    #[test]
    fn leaving_a_row_fades_it_out_over_the_token_duration() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.set_hovered(Some(&1), start, false);
        trail.set_hovered(Some(&2), start, false);
        assert_eq!(trail.linger(&1, start), 1.0);
        let mid = trail.linger(&1, at(start, 50));
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        let end = start + Motion::HOVER_LINGER_TIME;
        assert_eq!(trail.linger(&1, end), 0.0);
        assert!(trail.prune(at(start, 10)));
        assert!(!trail.prune(end));
        assert_eq!(trail.linger(&1, at(start, 10)), 0.0);
    }

    #[test]
    fn a_sweep_leaves_a_trail_that_fades_monotonically() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        for (row, ms) in [(1, 0), (2, 16), (3, 32), (4, 48)] {
            trail.set_hovered(Some(&row), at(start, ms), false);
        }
        let now = at(start, 60);
        let (a, b, c) = (
            trail.linger(&1, now),
            trail.linger(&2, now),
            trail.linger(&3, now),
        );
        assert!(a < b && b < c, "{a} {b} {c}");
    }

    #[test]
    fn re_entering_a_fading_row_snaps_it_back() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.set_hovered(Some(&1), start, false);
        trail.set_hovered(None, start, false);
        trail.set_hovered(Some(&1), at(start, 30), false);
        assert_eq!(trail.linger(&1, at(start, 30)), 0.0);
        assert!(!trail.is_fading());
    }

    #[test]
    fn a_late_leave_from_the_previous_row_is_ignored() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.hover_event(&1, true, start, false);
        trail.hover_event(&2, true, start, false);
        trail.hover_event(&1, false, start, false);
        assert_eq!(trail.hovered(), Some(&2));
    }

    #[test]
    fn selection_and_removal_end_the_trail_at_once() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.set_hovered(Some(&1), start, false);
        trail.set_hovered(Some(&2), start, false);
        trail.forget(&1);
        assert_eq!(trail.linger(&1, start), 0.0);

        trail.set_hovered(Some(&3), start, false);
        trail.retain(|key| *key != 2 && *key != 3);
        assert_eq!(trail.linger(&2, start), 0.0);
        assert_eq!(trail.hovered(), None);
    }

    #[test]
    fn reduce_motion_never_starts_a_trail() {
        let start = Instant::now();
        let mut trail = HoverTrail::default();
        trail.set_hovered(Some(&1), start, true);
        trail.set_hovered(Some(&2), start, true);
        assert_eq!(trail.linger(&1, start), 0.0);
        assert!(!trail.is_fading());
    }
}
