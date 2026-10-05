//! Safari's pinch into Tab Overview, as presentation state only.
//!
//! One number describes the whole transition: `progress`, 0 with the session
//! filling the workbench and 1 with it sitting in its overview card.
//! While fingers are down the page scale is the truth -- it is the product of
//! every magnification delta, so the page tracks the fingers 1:1 -- and
//! progress is derived from it against the card's size. After release,
//! progress itself travels on the shared settle curve. Nothing here schedules
//! frames, reads a clock, or touches the store; the surface owns all of that.
use std::time::{Duration, Instant};
use ubra_ui::Motion;

/// Where a release lands when the fingers carry no velocity. Safari commits
/// at roughly half way; so does this.
pub(crate) const COMMIT: f32 = 0.5;
/// How far (in seconds of travel) a flick is projected before deciding.
const FLICK_PROJECTION: f32 = 0.12;
/// The most a pinch-in past the card may shrink it, as progress.
const RUBBER_BAND: f32 = 0.06;
/// Thumbnail-to-page width. Never 1, or progress would divide by zero.
const MIN_SLOT_SCALE: f32 = 0.02;
const MAX_SLOT_SCALE: f32 = 0.95;
/// Reduce Motion replaces the flight with a cross-fade of overlay length.
const FADE: Duration = Duration::from_millis((Motion::OVERLAY_FADE * 1000.0) as u64);
/// How long a released pinch (or ⇧⌘O, Esc, Return, a click) takes to land,
/// on the shared settle curve. Safari's zoom is quick; `Motion::SETTLE`'s
/// 550 ms read as sluggish here, so the zoom keeps its own, shorter length.
pub(crate) const FLIGHT: Duration = Duration::from_millis(380);
/// The miniatures' font ladder: every card font is a multiple of this.
pub(crate) const FONT_STEP: f32 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    Page,
    Overview,
}

impl Landing {
    fn progress(self) -> f32 {
        match self {
            Self::Page => 0.0,
            Self::Overview => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Tracking {
    /// Page width over workbench width, straight from the fingers.
    scale: f32,
    started_from: Landing,
    velocity: f32,
    last_progress: f32,
    last_at: Instant,
    ticked: bool,
}

#[derive(Clone, Copy, Debug)]
struct Flight {
    from: f32,
    to: Landing,
    started: Instant,
    duration: Duration,
    /// Released by the fingers, so the grip hands over along the flight.
    from_fingers: bool,
    eased: f32,
}

impl Flight {
    fn sample(&mut self, now: Instant) -> (f32, bool) {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0);
        self.eased = Motion::SETTLE.settle(t);
        let to = self.to.progress();
        (self.from + (to - self.from) * self.eased, t >= 1.0)
    }
}

#[derive(Debug)]
pub(crate) struct OverviewZoom {
    progress: f32,
    slot_scale: f32,
    tracking: Option<Tracking>,
    flight: Option<Flight>,
    feedback_pending: bool,
}

impl Default for OverviewZoom {
    fn default() -> Self {
        Self {
            progress: 0.0,
            slot_scale: 0.3,
            tracking: None,
            flight: None,
            feedback_pending: false,
        }
    }
}

impl OverviewZoom {
    /// The card width over the page width. A pinch keeps its scale when
    /// this changes (the fingers own the scale); only derived progress moves.
    pub(crate) fn set_slot_scale(&mut self, slot_scale: f32) {
        if slot_scale.is_finite() {
            self.slot_scale = slot_scale.clamp(MIN_SLOT_SCALE, MAX_SLOT_SCALE);
        }
    }

    /// Grab the presentation where it is, including mid-flight, so a second
    /// pinch never jumps. `at_rest` is where the surface sits when idle.
    pub(crate) fn begin(&mut self, at_rest: Landing, now: Instant) {
        self.advance(now);
        let started_from = self.flight.map_or(at_rest, |flight| flight.to);
        let progress = if self.is_active() {
            self.progress
        } else {
            at_rest.progress()
        };
        self.flight = None;
        self.progress = progress;
        self.tracking = Some(Tracking {
            scale: 1.0 - progress * (1.0 - self.slot_scale),
            started_from,
            velocity: 0.0,
            last_progress: progress,
            last_at: now,
            ticked: false,
        });
    }

    /// One `magnifyWithEvent:` delta. Positive spreads the fingers.
    pub(crate) fn pinch(&mut self, magnification: f32, now: Instant) {
        let slot_scale = self.slot_scale;
        let Some(tracking) = self.tracking.as_mut() else {
            return;
        };
        if !magnification.is_finite() {
            return;
        }
        // Spreading past the full page does nothing: Ubra has no page zoom
        // to hand over to, so the page simply stops at its full size.
        tracking.scale = (tracking.scale * (1.0 + magnification)).clamp(0.0, 1.0);
        let raw = (1.0 - tracking.scale) / (1.0 - slot_scale);
        let progress = if raw > 1.0 {
            // Pinching in past the card only gives way a little.
            1.0 + RUBBER_BAND * (1.0 - 1.0 / (1.0 + (raw - 1.0) * 3.0))
        } else {
            raw
        };
        let dt = now
            .saturating_duration_since(tracking.last_at)
            .as_secs_f32();
        if dt > 0.001 {
            let instant = (progress - tracking.last_progress) / dt;
            tracking.velocity = tracking.velocity * 0.3 + instant * 0.7;
            tracking.last_progress = progress;
            tracking.last_at = now;
        }
        let crossed = (self.progress < COMMIT) != (progress < COMMIT);
        if crossed && !tracking.ticked {
            // One tick per gesture, however often the fingers cross back.
            tracking.ticked = true;
            self.feedback_pending = true;
        }
        self.progress = progress;
    }

    /// Fingers lifted: pick a side from position plus projected velocity and
    /// settle there from exactly where the page is.
    pub(crate) fn release(&mut self, now: Instant, reduced_motion: bool) -> Option<Landing> {
        let tracking = self.tracking.take()?;
        // A pinch that is still moving decides with its velocity, the way
        // Safari lets a quick flick commit before halfway.
        let still = now.saturating_duration_since(tracking.last_at) > Duration::from_millis(80);
        let velocity = if still { 0.0 } else { tracking.velocity };
        let projected = self.progress + velocity * FLICK_PROJECTION;
        let landing = if projected >= COMMIT {
            Landing::Overview
        } else {
            Landing::Page
        };
        self.fly_to(landing, now, reduced_motion, true);
        Some(landing)
    }

    /// The system took the gesture away: go back where it started.
    pub(crate) fn cancel(&mut self, now: Instant, reduced_motion: bool) -> Option<Landing> {
        let tracking = self.tracking.take()?;
        self.fly_to(tracking.started_from, now, reduced_motion, true);
        Some(tracking.started_from)
    }

    /// The non-interactive version of the same transition (keyboard, menus,
    /// clicks). Starts from the current pose so it can interrupt a flight.
    pub(crate) fn animate_to(&mut self, landing: Landing, now: Instant, reduced_motion: bool) {
        self.advance(now);
        self.tracking = None;
        self.fly_to(landing, now, reduced_motion, false);
    }

    fn fly_to(&mut self, landing: Landing, now: Instant, reduced_motion: bool, from_fingers: bool) {
        let from = self.progress;
        self.flight = (from != landing.progress()).then_some(Flight {
            from,
            to: landing,
            started: now,
            duration: if reduced_motion { FADE } else { FLIGHT },
            from_fingers,
            eased: 0.0,
        });
        if self.flight.is_none() {
            self.progress = landing.progress();
        }
    }

    /// Stop everything and rest at `landing` with no animation.
    pub(crate) fn reset(&mut self, landing: Landing) {
        self.tracking = None;
        self.flight = None;
        self.progress = landing.progress();
    }

    /// Advances a flight. Returns where it landed on the frame it finishes.
    pub(crate) fn advance(&mut self, now: Instant) -> Option<Landing> {
        let flight = self.flight.as_mut()?;
        let (progress, done) = flight.sample(now);
        let flight = *flight;
        self.progress = progress;
        if done {
            self.flight = None;
            self.progress = flight.to.progress();
            return Some(flight.to);
        }
        None
    }

    pub(crate) fn take_feedback(&mut self) -> bool {
        std::mem::take(&mut self.feedback_pending)
    }

    pub(crate) fn is_tracking(&self) -> bool {
        self.tracking.is_some()
    }

    pub(crate) fn is_flying(&self) -> bool {
        self.flight.is_some()
    }

    /// Tracking or flying: the surface must paint the transition.
    pub(crate) fn is_active(&self) -> bool {
        self.tracking.is_some() || self.flight.is_some()
    }

    /// Where the current flight is heading, if any.
    pub(crate) fn heading(&self) -> Option<Landing> {
        self.flight.map(|flight| flight.to)
    }

    /// 0 at the page, 1 in the grid, slightly above 1 while rubber-banding.
    pub(crate) fn progress(&self) -> f32 {
        self.progress
    }

    /// How much the fingers still own the page's position: all of it while
    /// they are down, handing over to the flight's own path after release.
    pub(crate) fn finger_weight(&self) -> f32 {
        if self.tracking.is_some() {
            return 1.0;
        }
        self.flight
            .filter(|flight| flight.from_fingers)
            .map_or(0.0, |flight| 1.0 - flight.eased)
    }

    /// Page width over workbench width for the current progress.
    pub(crate) fn page_scale(&self) -> f32 {
        1.0 - self.progress.max(0.0) * (1.0 - self.slot_scale)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ZoomRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl ZoomRect {
    pub(crate) fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }

    fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width > 1.0
            && self.height > 1.0
    }
}

/// The point of the page the fingers took hold of, as a fraction of the page,
/// and where the fingers are now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Grip {
    pub fraction: (f32, f32),
    pub fingers: (f32, f32),
}

impl Grip {
    /// Take hold of whatever frame is on screen, so grabbing never jumps.
    pub(crate) fn take(frame: ZoomRect, fingers: (f32, f32)) -> Self {
        let fraction = |at: f32, origin: f32, size: f32| {
            if size > 1.0 {
                ((at - origin) / size).clamp(0.0, 1.0)
            } else {
                0.5
            }
        };
        Self {
            fraction: (
                fraction(fingers.0, frame.x, frame.width),
                fraction(fingers.1, frame.y, frame.height),
            ),
            fingers,
        }
    }
}

/// Where the shrinking page is painted.
///
/// Width is exactly `page_scale` of the workbench, so content never stretches.
/// Height morphs from the workbench aspect to the card's crop, keeping the
/// top of the page, which is where Safari crops too. The gripped point of the
/// page sits under the fingers while they own it (`finger_weight` 1) and on
/// its natural straight path between workbench and slot otherwise, so a
/// release hands the page from the fingers to the flight without a jump and
/// progress 0 and 1 are the workbench and the slot exactly.
pub(crate) fn page_frame(
    page: ZoomRect,
    slot: ZoomRect,
    grip: Option<Grip>,
    finger_weight: f32,
    progress: f32,
    page_scale: f32,
) -> ZoomRect {
    if !page.valid() || !slot.valid() {
        return page;
    }
    let weight = if grip.is_some() {
        finger_weight.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // The ends of the path are the two rects exactly, not up to float error,
    // so a landed flight paints the card's own pixels.
    if weight == 0.0 && progress == 1.0 {
        return slot;
    }
    if weight == 0.0 && progress == 0.0 {
        return page;
    }
    let p = progress.clamp(0.0, 1.0);
    let width = page.width * page_scale;
    let height = if progress > 1.0 {
        // Rubber band: keep the slot's crop while it gives way.
        slot.height * width / slot.width
    } else {
        page.height + (slot.height - page.height) * p
    };
    let (u, v) = grip.map_or((0.5, 0.5), |grip| grip.fraction);
    let natural = (
        page.x + u * page.width + (slot.x + u * slot.width - page.x - u * page.width) * p,
        page.y + v * page.height + (slot.y + v * slot.height - page.y - v * page.height) * p,
    );
    let fingers = grip.map_or(natural, |grip| grip.fingers);
    let held = (
        natural.0 + (fingers.0 - natural.0) * weight,
        natural.1 + (fingers.1 - natural.1) * weight,
    );
    ZoomRect {
        x: held.0 - u * width,
        y: held.1 - v * height,
        width,
        height,
    }
}

/// How opaque the grid around the page is. It leads the page slightly so the
/// other cards are readable before the page lands, as in Safari.
pub(crate) fn grid_alpha(progress: f32) -> f32 {
    Motion::SETTLE.settle((progress * 1.25).clamp(0.0, 1.0))
}

/// The mini-window card's inside, relative to the card's frame: the title
/// strip above the grid, the terminal font, and where the grid sits inside
/// the area below the strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CardPose {
    pub strip: f32,
    pub font: f32,
    /// Distance from the left edge of the area below the strip to the grid.
    pub grid_left: f32,
    /// Distance from the bottom edge of that area to the grid's last row.
    pub grid_bottom: f32,
    pub radius: f32,
}

/// The flying frame's chrome at `t`: the title region grows from the page's
/// title bar to the card's strip and the corners round, while everything
/// inside stays one bitmap. 0 and 1 are the two ends exactly.
pub(crate) fn chrome_pose(page_strip: f32, card: CardPose, t: f32) -> (f32, f32) {
    let t = t.max(0.0);
    if t == 1.0 {
        return (card.strip, card.radius);
    }
    let mix = |a: f32, b: f32| a + (b - a) * t;
    (
        mix(page_strip, card.strip).max(0.0),
        mix(0.0, card.radius).max(0.0),
    )
}

/// A terminal grid's top-left corner and cell width, in points, relative to
/// whatever frame it is painted in (the page, a card, or a snapshot of one).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridAnchor {
    pub x: f32,
    pub y: f32,
    pub cell: f32,
}

/// Where the snapshot bitmap paints, in window coordinates.
///
/// The bitmap never re-lays out: it is scaled by one factor about its own
/// grid. That factor follows the fingers (`page_scale`), bent just enough
/// that the grid's cells land on the card's cells at progress 1, and the
/// grid's corner rides from the page's grid position to the card's. So a
/// page snapshot at progress 0 and a card snapshot at progress 1 paint
/// exactly over what they were taken from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_rect(
    frame: ZoomRect,
    page: GridAnchor,
    card: GridAnchor,
    snapshot: GridAnchor,
    snapshot_size: (f32, f32),
    progress: f32,
    page_scale: f32,
    slot_scale: f32,
) -> ZoomRect {
    let t = progress.clamp(0.0, 1.0);
    let slot_scale = slot_scale.max(MIN_SLOT_SCALE);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    let to_card = page_scale / slot_scale;
    let cell = mix(page.cell * page_scale, card.cell * to_card);
    let x = mix(page.x * page_scale, card.x * to_card);
    let y = mix(page.y * page_scale, card.y * to_card);
    let scale = if snapshot.cell > 0.0 {
        cell / snapshot.cell
    } else {
        page_scale
    };
    ZoomRect {
        x: frame.x + x - snapshot.x * scale,
        y: frame.y + y - snapshot.y * scale,
        width: snapshot_size.0 * scale,
        height: snapshot_size.1 * scale,
    }
}

/// Where the card's own rendering starts to show through the flying bitmap.
/// The two only line up exactly at the card, so the hand-over happens where
/// the flight is already almost there; see `card_fade`.
pub(crate) const CARD_FADE_FROM: f32 = 0.9;
/// A flight home without a fresh page snapshot hands over to the live pane
/// below this progress.
pub(crate) const LIVE_FADE_UNTIL: f32 = 0.15;

fn smoothstep(from: f32, to: f32, value: f32) -> f32 {
    let t = ((value - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Opacity of the real card over the flying bitmap: 0 until
/// `CARD_FADE_FROM`, exactly 1 at the card (and past it, rubber-banding).
pub(crate) fn card_fade(progress: f32) -> f32 {
    if progress >= 1.0 {
        return 1.0;
    }
    smoothstep(CARD_FADE_FROM, 1.0, progress)
}

/// How far the flying frame has been pulled onto the card's own size, so the
/// picture and the card under it line up before the card shows through.
/// It finishes early in the cross-fade: a picture fading over a card of a
/// different size reads as a double image.
pub(crate) fn card_snap(progress: f32) -> f32 {
    if progress >= 1.0 {
        return 1.0;
    }
    smoothstep(
        CARD_FADE_FROM,
        CARD_FADE_FROM + (1.0 - CARD_FADE_FROM) * 0.4,
        progress,
    )
}

/// `frame` pulled `weight` of the way onto a `card`-sized rect centred on it.
pub(crate) fn snap_frame(frame: ZoomRect, card: (f32, f32), weight: f32) -> ZoomRect {
    let target = ZoomRect {
        x: frame.x + (frame.width - card.0) / 2.0,
        y: frame.y + (frame.height - card.1) / 2.0,
        width: card.0,
        height: card.1,
    };
    if weight >= 1.0 {
        return target;
    }
    let mix = |a: f32, b: f32| a + (b - a) * weight;
    ZoomRect {
        x: mix(frame.x, target.x),
        y: mix(frame.y, target.y),
        width: mix(frame.width, target.width),
        height: mix(frame.height, target.height),
    }
}

/// How far the card's own chrome (title, hairlines, shadow) has come in.
/// It waits until the page is well on its way, so the card's title never
/// reads over the page's own title bar in the picture.
pub(crate) fn chrome_fade(progress: f32) -> f32 {
    smoothstep(0.35, 1.0, progress)
}

/// Opacity of the flying bitmap near the page when it is not a picture of
/// the page as it is now: it hands over to the live pane underneath.
pub(crate) fn live_fade(progress: f32) -> f32 {
    smoothstep(0.0, LIVE_FADE_UNTIL, progress)
}

/// Trilinear filtering by hand: the mip level whose pixels are at most as
/// dense as twice the screen's, the next coarser one, and how much of the
/// coarser one to show. `source_px` is level 0's width, `screen_px` the
/// painted width, both in device pixels.
pub(crate) fn mip_blend(source_px: f32, screen_px: f32, levels: usize) -> (usize, usize, f32) {
    if levels <= 1 || screen_px <= 0.0 || source_px <= screen_px {
        return (0, 0, 0.0);
    }
    let lod = (source_px / screen_px).log2().max(0.0);
    let finest = (lod.floor() as usize).min(levels - 1);
    if finest + 1 >= levels {
        return (finest, finest, 0.0);
    }
    (finest, finest + 1, lod - finest as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn zoom(slot_scale: f32) -> OverviewZoom {
        let mut zoom = OverviewZoom::default();
        zoom.set_slot_scale(slot_scale);
        zoom
    }

    #[test]
    fn page_scale_is_the_product_of_the_fingers_deltas() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        zoom.pinch(-0.1, t + Duration::from_millis(8));
        zoom.pinch(-0.1, t + Duration::from_millis(16));
        assert!(
            (zoom.page_scale() - 0.81).abs() < 1e-4,
            "{}",
            zoom.page_scale()
        );
        // Spreading back returns along the same path.
        zoom.pinch(1.0 / 0.9 - 1.0, t + Duration::from_millis(24));
        assert!((zoom.page_scale() - 0.9).abs() < 1e-4);
    }

    #[test]
    fn spreading_on_the_page_does_nothing() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        zoom.pinch(0.5, t + Duration::from_millis(8));
        assert_eq!(zoom.progress(), 0.0);
        assert_eq!(
            zoom.release(t + Duration::from_millis(200), false),
            Some(Landing::Page)
        );
        assert!(!zoom.is_active());
    }

    #[test]
    fn a_slow_release_decides_by_position_alone() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        // Scale 0.7 is progress 3/7 < 0.5: springs back.
        zoom.pinch(-0.3, t + Duration::from_millis(10));
        let late = t + Duration::from_millis(400);
        assert_eq!(zoom.release(late, false), Some(Landing::Page));
        assert!(zoom.is_flying());
        assert_eq!(zoom.advance(late + FLIGHT), Some(Landing::Page));
        assert_eq!(zoom.progress(), 0.0);

        zoom.begin(Landing::Page, late + FLIGHT);
        zoom.pinch(-0.5, late + FLIGHT + Duration::from_millis(10));
        assert_eq!(
            zoom.release(late + FLIGHT + Duration::from_millis(400), false),
            Some(Landing::Overview)
        );
    }

    #[test]
    fn a_quick_flick_commits_before_halfway() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        for step in 1..=4 {
            zoom.pinch(-0.06, t + Duration::from_millis(8 * step));
        }
        assert!(zoom.progress() < COMMIT, "{}", zoom.progress());
        assert_eq!(
            zoom.release(t + Duration::from_millis(40), false),
            Some(Landing::Overview)
        );
    }

    #[test]
    fn release_flies_from_the_exact_pose_without_a_jump() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        zoom.pinch(-0.5, t + Duration::from_millis(10));
        let pose = zoom.progress();
        let released = t + Duration::from_millis(300);
        zoom.release(released, false);
        zoom.advance(released);
        assert!((zoom.progress() - pose).abs() < 1e-6);
        zoom.advance(released + FLIGHT / 2);
        assert!(zoom.progress() > pose && zoom.progress() < 1.0);
    }

    #[test]
    fn a_new_pinch_grabs_a_flight_in_place() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.animate_to(Landing::Overview, t, false);
        let mid = t + FLIGHT / 3;
        zoom.advance(mid);
        let pose = zoom.progress();
        zoom.begin(Landing::Page, mid);
        assert!(zoom.is_tracking() && !zoom.is_flying());
        assert!((zoom.progress() - pose).abs() < 1e-6);
        assert!((zoom.page_scale() - (1.0 - pose * 0.7)).abs() < 1e-5);
        // A cancel returns to where the interrupted flight was heading.
        assert_eq!(zoom.cancel(mid, false), Some(Landing::Overview));
    }

    #[test]
    fn pinching_out_of_the_grid_grows_from_the_card() {
        let t = now();
        let mut zoom = zoom(0.25);
        zoom.reset(Landing::Overview);
        zoom.begin(Landing::Overview, t);
        assert!((zoom.page_scale() - 0.25).abs() < 1e-6);
        zoom.pinch(2.0, t + Duration::from_millis(10));
        assert!((zoom.page_scale() - 0.75).abs() < 1e-5);
        assert_eq!(
            zoom.release(t + Duration::from_millis(400), false),
            Some(Landing::Page)
        );
    }

    #[test]
    fn pinching_in_on_the_grid_only_rubber_bands() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.reset(Landing::Overview);
        zoom.begin(Landing::Overview, t);
        for step in 1..=40 {
            zoom.pinch(-0.2, t + Duration::from_millis(8 * step));
        }
        assert!(zoom.progress() > 1.0 && zoom.progress() <= 1.0 + RUBBER_BAND);
        assert_eq!(
            zoom.release(t + Duration::from_millis(900), false),
            Some(Landing::Overview)
        );
        zoom.advance(t + Duration::from_millis(900) + FLIGHT);
        assert_eq!(zoom.progress(), 1.0);
    }

    #[test]
    fn the_threshold_ticks_once_per_gesture() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        assert!(!zoom.take_feedback(), "starting a pinch is not a threshold");
        let mut at = t;
        for _ in 0..3 {
            at += Duration::from_millis(10);
            zoom.pinch(-0.5, at);
            at += Duration::from_millis(10);
            zoom.pinch(1.0, at);
        }
        assert!(zoom.take_feedback());
        assert!(!zoom.take_feedback());
    }

    #[test]
    fn reduced_motion_uses_the_short_fade() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.animate_to(Landing::Overview, t, true);
        assert_eq!(zoom.advance(t + FADE), Some(Landing::Overview));
    }

    const PAGE: ZoomRect = ZoomRect {
        x: 240.0,
        y: 0.0,
        width: 1000.0,
        height: 800.0,
    };
    const SLOT: ZoomRect = ZoomRect {
        x: 300.0,
        y: 240.0,
        width: 300.0,
        height: 178.0,
    };

    fn close(a: ZoomRect, b: ZoomRect) -> bool {
        [
            (a.x, b.x),
            (a.y, b.y),
            (a.width, b.width),
            (a.height, b.height),
        ]
        .iter()
        .all(|(a, b)| (a - b).abs() < 1e-3)
    }

    #[test]
    fn the_frame_starts_at_the_page_and_ends_in_the_slot() {
        let grip = Some(Grip::take(PAGE, (700.0, 400.0)));
        assert!(close(page_frame(PAGE, SLOT, grip, 0.0, 0.0, 1.0), PAGE));
        assert!(close(page_frame(PAGE, SLOT, grip, 0.0, 1.0, 0.3), SLOT));
        assert!(close(page_frame(PAGE, SLOT, None, 1.0, 1.0, 0.3), SLOT));
    }

    #[test]
    fn the_gripped_point_stays_under_the_fingers_as_they_move() {
        let grip = Grip::take(PAGE, (840.0, 500.0));
        for (scale, fingers) in [(0.95, (840.0, 500.0)), (0.6, (800.0, 460.0))] {
            let progress = (1.0 - scale) / 0.7;
            let grip = Grip { fingers, ..grip };
            let frame = page_frame(PAGE, SLOT, Some(grip), 1.0, progress, scale);
            let under = (
                (fingers.0 - frame.x) / frame.width,
                (fingers.1 - frame.y) / frame.height,
            );
            assert!((under.0 - grip.fraction.0).abs() < 1e-4, "{under:?}");
            assert!((under.1 - grip.fraction.1).abs() < 1e-4, "{under:?}");
            assert!((frame.width - PAGE.width * scale).abs() < 1e-3);
        }
    }

    #[test]
    fn a_card_grows_about_the_fingers_that_grabbed_it() {
        let grip = Grip::take(SLOT, (500.0, 300.0));
        let frame = page_frame(PAGE, SLOT, Some(grip), 1.0, 1.0, 0.3);
        assert!(close(frame, SLOT), "grabbing must not move the card");
        let frame = page_frame(PAGE, SLOT, Some(grip), 1.0, 0.5, 0.65);
        assert!(((500.0 - frame.x) / frame.width - grip.fraction.0).abs() < 1e-4);
    }

    #[test]
    fn rubber_band_shrinks_about_the_grip() {
        let grip = Grip::take(SLOT, (SLOT.x + 150.0, SLOT.y + 89.0));
        let frame = page_frame(PAGE, SLOT, Some(grip), 1.0, 1.05, 0.28);
        assert!(frame.width < SLOT.width);
        let centre = |r: ZoomRect| (r.x + r.width / 2.0, r.y + r.height / 2.0);
        let (a, b) = (centre(frame), centre(SLOT));
        assert!((a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3);
    }

    #[test]
    fn the_grip_hands_over_to_the_flight_without_a_jump() {
        let t = now();
        let mut zoom = zoom(0.3);
        zoom.begin(Landing::Page, t);
        zoom.pinch(-0.5, t + Duration::from_millis(10));
        assert_eq!(zoom.finger_weight(), 1.0);
        let released = t + Duration::from_millis(400);
        zoom.release(released, false);
        zoom.advance(released);
        assert!((zoom.finger_weight() - 1.0).abs() < 1e-6);
        zoom.advance(released + FLIGHT);
        assert_eq!(zoom.finger_weight(), 0.0);
        zoom.animate_to(Landing::Page, released + FLIGHT, false);
        assert_eq!(
            zoom.finger_weight(),
            0.0,
            "keyboard flights follow the path"
        );
    }

    const CARD_POSE: CardPose = CardPose {
        strip: 26.0,
        font: 4.75,
        grid_left: 4.1,
        grid_bottom: 4.1,
        radius: 10.0,
    };

    #[test]
    fn the_chrome_is_the_page_and_then_exactly_the_card() {
        assert_eq!(chrome_pose(42.0, CARD_POSE, 0.0), (42.0, 0.0));
        assert_eq!(chrome_pose(42.0, CARD_POSE, 1.0), (26.0, 10.0));
    }

    const PAGE_GRID: GridAnchor = GridAnchor {
        x: 12.0,
        y: 44.0,
        cell: 7.8,
    };
    const CARD_GRID: GridAnchor = GridAnchor {
        x: 4.1,
        y: 31.0,
        cell: 2.1,
    };

    #[test]
    fn a_page_snapshot_starts_on_the_page_and_its_grid_lands_on_the_cards() {
        let slot_scale = SLOT.width / PAGE.width;
        let size = (PAGE.width, PAGE.height);
        let start = snapshot_rect(PAGE, PAGE_GRID, CARD_GRID, PAGE_GRID, size, 0.0, 1.0, 0.3);
        assert!(close(start, PAGE), "{start:?}");
        let end = snapshot_rect(
            SLOT, PAGE_GRID, CARD_GRID, PAGE_GRID, size, 1.0, slot_scale, slot_scale,
        );
        let scale = end.width / PAGE.width;
        assert!((scale * PAGE_GRID.cell - CARD_GRID.cell).abs() < 1e-4);
        assert!((end.x + PAGE_GRID.x * scale - SLOT.x - CARD_GRID.x).abs() < 1e-3);
        assert!((end.y + PAGE_GRID.y * scale - SLOT.y - CARD_GRID.y).abs() < 1e-3);
        // One scale factor: the aspect never changes along the way.
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let page_scale = 1.0 - t * (1.0 - slot_scale);
            let frame = page_frame(PAGE, SLOT, None, 0.0, t, page_scale);
            let rect = snapshot_rect(
                frame, PAGE_GRID, CARD_GRID, PAGE_GRID, size, t, page_scale, slot_scale,
            );
            assert!((rect.width / rect.height - size.0 / size.1).abs() < 1e-4);
        }
    }

    #[test]
    fn a_card_snapshot_sits_exactly_on_its_card() {
        let slot_scale = SLOT.width / PAGE.width;
        let rect = snapshot_rect(
            SLOT,
            PAGE_GRID,
            CARD_GRID,
            CARD_GRID,
            (SLOT.width, SLOT.height),
            1.0,
            slot_scale,
            slot_scale,
        );
        assert!(close(rect, SLOT), "{rect:?}");
    }

    #[test]
    fn the_card_takes_over_only_at_the_end() {
        assert_eq!(card_fade(0.0), 0.0);
        assert_eq!(card_fade(CARD_FADE_FROM), 0.0);
        assert_eq!(card_fade(1.0), 1.0);
        assert_eq!(card_fade(1.04), 1.0);
        assert_eq!(card_snap(CARD_FADE_FROM), 0.0);
        assert_eq!(card_snap(1.0), 1.0);
        // The picture is on the card's size before the card is half shown.
        let halfway = CARD_FADE_FROM + (1.0 - CARD_FADE_FROM) * 0.5;
        assert!(card_snap(halfway) == 1.0 && card_fade(halfway) <= 0.5);
        let snapped = snap_frame(SLOT, (SLOT.width, SLOT.height), 0.5);
        assert!(close(snapped, SLOT));
        assert_eq!(live_fade(0.0), 0.0);
        assert_eq!(live_fade(LIVE_FADE_UNTIL), 1.0);
    }

    #[test]
    fn mip_levels_keep_minification_under_two() {
        assert_eq!(mip_blend(2400.0, 2400.0, 4), (0, 0, 0.0));
        assert_eq!(mip_blend(2400.0, 1200.0, 4), (1, 2, 0.0));
        let (fine, coarse, mix) = mip_blend(2400.0, 650.0, 4);
        assert_eq!((fine, coarse), (1, 2));
        assert!(mix > 0.8 && mix < 1.0, "{mix}");
        assert_eq!(mip_blend(2400.0, 100.0, 4), (3, 3, 0.0));
    }

    #[test]
    fn the_flight_is_safari_quick() {
        assert!(
            (350..=400).contains(&FLIGHT.as_millis()),
            "{}",
            FLIGHT.as_millis()
        );
    }

    #[test]
    fn grid_alpha_is_monotonic_and_complete() {
        assert_eq!(grid_alpha(0.0), 0.0);
        assert_eq!(grid_alpha(1.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = grid_alpha(step as f32 / 100.0);
            assert!(value >= previous);
            previous = value;
        }
    }
}
