//! Pins the vendored GPUI view-caching behavior the sidebar relies on (see
//! `vendor/gpui/UBRA_PATCHES.md`). When a cached view re-renders, cached views
//! nested inside it that are not dirty are reused. That holds even after an
//! ancestor was itself reused wholesale in between, which moves every index the
//! nested view recorded. Reused views must keep their hitboxes, mouse
//! listeners, key dispatch, and paint, and must still re-render on their own
//! notify, on an opacity change above them, and on `window.refresh()`.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Modifiers, StyleRefinement, TestAppContext,
    VisualTestContext, Window, actions, div, px,
};

actions!(gpui_view_cache_test, [Poke]);

#[derive(Default)]
struct Counters {
    leaf_renders: Cell<usize>,
    middle_renders: Cell<usize>,
    clicks: Cell<usize>,
    pokes: Cell<usize>,
}

struct Leaf {
    counters: Rc<Counters>,
    focus: FocusHandle,
}

impl Render for Leaf {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let counters = &self.counters;
        counters.leaf_renders.set(counters.leaf_renders.get() + 1);
        let clicks = Rc::clone(counters);
        let pokes = Rc::clone(counters);
        div()
            .id("leaf")
            .debug_selector(|| "leaf".into())
            .key_context("Leaf")
            .track_focus(&self.focus)
            .size_full()
            .bg(gpui::red())
            .on_action(move |_: &Poke, _, _| pokes.pokes.set(pokes.pokes.get() + 1))
            .on_click(move |_, _, _| clicks.clicks.set(clicks.clicks.get() + 1))
    }
}

/// Absolutely positioned probes with hitboxes and paint ahead of the nested
/// view: changing their count shifts every frame index the view recorded
/// without moving its bounds.
fn index_shifters(prefix: &str, count: usize) -> impl Iterator<Item = gpui::AnyElement> {
    let prefix = prefix.to_owned();
    (0..count).map(move |index| {
        div()
            .id(format!("{prefix}-{index}"))
            .debug_selector({
                let selector = format!("{prefix}-{index}");
                move || selector
            })
            .absolute()
            .top(px(0.0))
            .left(px(index as f32 * 3.0))
            .size(px(2.0))
            .bg(gpui::blue())
            .on_click(|_, _, _| {})
            .into_any_element()
    })
}

struct Middle {
    counters: Rc<Counters>,
    leaf: Entity<Leaf>,
    shifters: usize,
    leaf_opacity: f32,
    force_leaf: bool,
}

impl Render for Middle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.counters
            .middle_renders
            .set(self.counters.middle_renders.get() + 1);
        div()
            .relative()
            .size_full()
            .children(index_shifters("middle", self.shifters))
            .child(
                div()
                    .absolute()
                    .top(px(100.0))
                    .left(px(100.0))
                    .size(px(80.0))
                    .opacity(self.leaf_opacity)
                    .child(
                        self.leaf
                            .clone()
                            .cached(StyleRefinement::default().size_full())
                            .force_render_if(self.force_leaf),
                    ),
            )
    }
}

struct Top {
    middle: Entity<Middle>,
    shifters: usize,
    cache_middle: bool,
}

impl Render for Top {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let middle = self.middle.clone();
        div()
            .relative()
            .size_full()
            .children(index_shifters("top", self.shifters))
            .map(|top| {
                if self.cache_middle {
                    top.child(middle.cached(StyleRefinement::default().size_full()))
                } else {
                    top.child(middle)
                }
            })
    }
}

fn harness(
    cx: &mut TestAppContext,
    cache_middle: bool,
) -> (
    Entity<Top>,
    Entity<Middle>,
    Entity<Leaf>,
    Rc<Counters>,
    &mut VisualTestContext,
) {
    cx.update(|cx: &mut App| {
        cx.bind_keys([gpui::KeyBinding::new("p", Poke, Some("Leaf"))]);
    });
    let counters = Rc::new(Counters::default());
    let (top, cx) = cx.add_window_view({
        let counters = Rc::clone(&counters);
        move |_, cx| {
            let leaf = cx.new(|cx| Leaf {
                counters: Rc::clone(&counters),
                focus: cx.focus_handle(),
            });
            let middle = cx.new(|_| Middle {
                counters,
                leaf,
                shifters: 0,
                leaf_opacity: 1.0,
                force_leaf: false,
            });
            Top {
                middle,
                shifters: 0,
                cache_middle,
            }
        }
    });
    cx.run_until_parked();
    let middle = top.read_with(cx, |top, _| top.middle.clone());
    let leaf = middle.read_with(cx, |middle, _| middle.leaf.clone());
    (top, middle, leaf, counters, cx)
}

/// The leaf still hit-tests, dispatches keys, and paints its debug bounds.
fn assert_leaf_live(cx: &mut VisualTestContext, counters: &Counters, step: &str) {
    let bounds = cx
        .debug_bounds("leaf")
        .unwrap_or_else(|| panic!("{step}: the leaf lost its painted bounds"));
    assert_eq!(bounds.origin, gpui::point(px(100.0), px(100.0)), "{step}");
    let clicks = counters.clicks.get();
    cx.simulate_click(bounds.center(), Modifiers::default());
    assert_eq!(counters.clicks.get(), clicks + 1, "{step}: click missed");
    let pokes = counters.pokes.get();
    cx.simulate_keystrokes("p");
    assert_eq!(
        counters.pokes.get(),
        pokes + 1,
        "{step}: key dispatch missed"
    );
}

#[gpui::test]
fn nested_cached_view_is_reused_when_its_cached_parent_rerenders(cx: &mut TestAppContext) {
    for cache_middle in [true, false] {
        let (_, middle, _, counters, cx) = harness(cx, cache_middle);
        let before = counters.leaf_renders.get();
        middle.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(
            counters.leaf_renders.get(),
            before,
            "cache_middle={cache_middle}: an undirtied leaf is reused under a re-rendered parent"
        );
    }
}

#[gpui::test]
fn nested_reuse_survives_ancestor_reuse_and_shifted_indices(cx: &mut TestAppContext) {
    let (top, middle, leaf, counters, cx) = harness(cx, true);
    leaf.update_in(cx, |leaf, window, cx| leaf.focus.focus(window, cx));
    cx.run_until_parked();
    assert_leaf_live(cx, &counters, "initial");

    // Pointer and key input refresh the window, so each round first drives
    // the structural steps without input, asserting the leaf is reused
    // throughout, and only then checks that the reused leaf is still live.
    for round in 0..4_usize {
        let leaf_renders = counters.leaf_renders.get();
        // The parent re-renders with a different number of elements ahead of
        // the leaf.
        middle.update(cx, |middle, cx| {
            middle.shifters = (round * 2 + 1) % 5;
            cx.notify();
        });
        cx.run_until_parked();
        // The root re-renders with more elements ahead of the parent, which is
        // reused wholesale: nothing inside it is visited, and every index
        // moves.
        let middle_renders = counters.middle_renders.get();
        top.update(cx, |top, cx| {
            top.shifters = round + 1;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            counters.middle_renders.get(),
            middle_renders,
            "round {round}"
        );
        // The parent re-renders again: the leaf's ranges are rebased onto
        // where the parent was drawn in the reused frame.
        middle.update(cx, |middle, cx| {
            middle.shifters = round % 3;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            counters.leaf_renders.get(),
            leaf_renders,
            "round {round}: nothing dirtied the leaf"
        );
        assert_leaf_live(cx, &counters, &format!("round {round}"));
    }
    for selector in ["top-0", "top-1", "top-2"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
}

#[gpui::test]
fn nested_cached_view_still_rerenders_when_it_must(cx: &mut TestAppContext) {
    let (_, middle, leaf, counters, cx) = harness(cx, true);

    // Its own notify dirties it and every ancestor.
    let renders = counters.leaf_renders.get();
    leaf.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 1);

    // Opacity is baked into painted primitives, so a change above the leaf
    // must not replay the old alpha.
    middle.update(cx, |middle, cx| {
        middle.leaf_opacity = 0.5;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 2);

    // An unchanged opacity reuses it again.
    middle.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 2);

    // A parent that hands the leaf new inputs while drawing forces it for
    // that frame only.
    middle.update(cx, |middle, cx| {
        middle.force_leaf = true;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 3);
    middle.update(cx, |middle, cx| {
        middle.force_leaf = false;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 3);
    let renders = renders + 1;

    // `window.refresh()` still re-renders everything.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(counters.leaf_renders.get(), renders + 3);
    assert_leaf_live(cx, &counters, "after refresh");
}

/// Ubra's vendored-GPUI scene patch: one huge frame must not pin its peak
/// primitive storage forever, and steady frames must never reallocate.
#[test]
fn a_scene_gives_back_capacity_a_single_large_frame_left_behind() {
    let quad = gpui::Quad::default();
    let mut scene = gpui::Scene::default();
    scene.quads.extend(std::iter::repeat_n(quad, 50_000));
    scene.clear();
    let peak = scene.quads.capacity();
    assert!(peak >= 50_000);

    // Steady large frames keep their storage.
    for _ in 0..300 {
        scene.quads.extend(std::iter::repeat_n(quad, 40_000));
        scene.clear();
    }
    assert_eq!(scene.quads.capacity(), peak);

    // A long run of small frames eventually releases it.
    for _ in 0..119 {
        scene.quads.extend(std::iter::repeat_n(quad, 10));
        scene.clear();
    }
    assert_eq!(
        scene.quads.capacity(),
        peak,
        "not before two seconds of sparse frames"
    );
    scene.quads.extend(std::iter::repeat_n(quad, 10));
    scene.clear();
    assert!(
        scene.quads.capacity() <= 64,
        "shrunk to about twice the last frame"
    );
}

/// Ubra's vendored-GPUI sprite sort must order sprites exactly as the stable
/// sort it replaced: by key, ties kept in the order they were painted. Sprites
/// sharing an order and a tile are indistinguishable on screen only if they
/// stay put, so ties are checked by payload.
#[test]
fn the_sprite_sort_matches_a_stable_sort_by_key() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for len in [0usize, 1, 5, 31, 32, 33, 100, 1_000, 20_000] {
        for distinct in [1u64, 3, 40, 1 << 40] {
            let items: Vec<(u64, usize)> =
                (0..len).map(|index| (next() % distinct, index)).collect();
            let mut expected = items.clone();
            expected.sort_by_key(|item| item.0);
            let mut sorted = items.clone();
            gpui::sort_sprites_for_test(&mut sorted, |item| item.0);
            assert_eq!(sorted, expected, "len {len}, {distinct} distinct keys");
        }
        // Already in order: nothing moves.
        let mut ordered: Vec<(u64, usize)> =
            (0..len).map(|index| (index as u64 / 3, index)).collect();
        let expected = ordered.clone();
        gpui::sort_sprites_for_test(&mut ordered, |item| item.0);
        assert_eq!(ordered, expected);
    }
}
