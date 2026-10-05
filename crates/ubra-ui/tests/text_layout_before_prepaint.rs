//! The vendored GPUI's `TextLayout` lookups must answer "nothing yet" before
//! a frame's layout has run. A view that asks during its own render (Ubra's
//! note editor placing a menu at the caret) otherwise panicked inside an
//! AppKit callback, which aborts the whole app (`crash.report` SIGABRT,
//! 2026-10-01). The vendored crate's own test target can't build here, so
//! the regression lives in a crate that links it.

use gpui::{Point, TextLayout};

#[test]
fn lookups_before_layout_answer_empty_instead_of_panicking() {
    let layout = TextLayout::default();
    assert_eq!(layout.position_for_index(0), None);
    assert_eq!(layout.index_for_position(Point::default()), Err(0));
    assert!(layout.line_layout_for_index(0).is_none());
    assert!(layout.line_layouts().is_empty());
}
