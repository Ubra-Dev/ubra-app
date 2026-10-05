//! Digit-reel number animation, matching barvian/number-flow.
#[cfg(test)]
use crate::usage::UsageFormat;
use gpui::{
    AnyElement, FontFeatures, FontWeight, IntoElement, Rgba, SharedString, div, prelude::*, px,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DIGIT_EM: f32 = 0.62;
const SPIN: Duration = Duration::from_millis(900);

#[derive(Default)]
pub struct Bank {
    slots: RefCell<HashMap<String, Slot>>,
}

struct Slot {
    from: f64,
    to: f64,
    from_text: String,
    to_text: String,
    start: Option<Instant>,
}

impl Bank {
    pub fn show(
        &self,
        id: impl Into<String>,
        value: f64,
        text: impl Into<String>,
        size: f32,
        color: Rgba,
        weight: FontWeight,
    ) -> AnyElement {
        self.show_font(id, value, text, size, color, weight, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show_font(
        &self,
        id: impl Into<String>,
        value: f64,
        text: impl Into<String>,
        size: f32,
        color: Rgba,
        weight: FontWeight,
        font: Option<SharedString>,
    ) -> AnyElement {
        let text = text.into();
        let mut slots = self.slots.borrow_mut();
        let slot = slots.entry(id.into()).or_insert_with(|| Slot {
            from: value,
            to: value,
            from_text: text.clone(),
            to_text: text.clone(),
            start: None,
        });
        if format_kind(&slot.to_text) != format_kind(&text) {
            slot.from = value;
            slot.to = value;
            slot.from_text = text.clone();
            slot.to_text = text.clone();
            slot.start = None;
        } else if (value - slot.to).abs() > 0.000_5 {
            let t = progress(slot);
            let was = trend(slot.from, slot.to);
            slot.from_text = snapshot(&slot.from_text, &slot.to_text, was, t);
            slot.from = slot.to;
            slot.to = value;
            slot.to_text = text.clone();
            slot.start = Some(Instant::now());
        } else {
            slot.to_text = text.clone();
            if slot.start.is_none() {
                slot.from_text = text.clone();
            }
        }
        // Settled slots render the static target text: skip the
        // progress/trend/snapshot work and reuse one clone of `to_text`
        // for both `flow_row` inputs (`t >= 1.0` renders `tabular(to)`).
        if done(slot) && slot.to == value && slot.to_text == text {
            slot.from = slot.to;
            slot.from_text = slot.to_text.clone();
            slot.start = None;
            let settled = slot.to_text.clone();
            drop(slots);
            let mut row = flow_row(&settled, &settled, 1.0, 0, size, color, weight);
            if let Some(family) = font {
                row = row.font_family(family);
            }
            return row.into_any_element();
        }
        let t = progress(slot);
        let dir = trend(slot.from, slot.to);
        let from_text = slot.from_text.clone();
        let to_text = slot.to_text.clone();
        if done(slot) {
            slot.from = slot.to;
            slot.from_text = to_text.clone();
            slot.start = None;
        }
        drop(slots);
        let mut row = flow_row(&from_text, &to_text, t, dir, size, color, weight);
        if let Some(family) = font {
            row = row.font_family(family);
        }
        row.into_any_element()
    }
}

fn progress(slot: &Slot) -> f32 {
    let Some(start) = slot.start else {
        return 1.0;
    };
    ease((start.elapsed().as_secs_f32() / SPIN.as_secs_f32()).clamp(0.0, 1.0))
}

fn done(slot: &Slot) -> bool {
    slot.start
        .is_none_or(|start| start.elapsed() >= SPIN || (slot.from - slot.to).abs() <= 0.000_5)
}

fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(5)
}

fn trend(from: f64, to: f64) -> i8 {
    if to > from {
        1
    } else if to < from {
        -1
    } else {
        0
    }
}

fn digit_delta(from: u8, to: u8, dir: i8) -> i32 {
    let from = i32::from(from);
    let to = i32::from(to);
    if dir < 0 && to > from {
        to - 10 - from
    } else if dir > 0 && to < from {
        10 - from + to
    } else {
        to - from
    }
}

fn glyph(n: f32) -> char {
    char::from(b'0' + (n as i32).rem_euclid(10) as u8)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormatKind {
    Money,
    Tokens,
    Percent,
    Int,
}

fn format_kind(sample: &str) -> FormatKind {
    if sample.starts_with('$') {
        FormatKind::Money
    } else if sample.ends_with('%') {
        FormatKind::Percent
    } else if sample.chars().any(|ch| ch.is_ascii_alphabetic()) {
        FormatKind::Tokens
    } else {
        FormatKind::Int
    }
}

struct Parts<'a> {
    prefix: &'a str,
    integer: &'a str,
    fraction: &'a str,
    suffix: &'a str,
    dot: bool,
}

fn split_format(s: &str) -> Parts<'_> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    let (dot, frac_at) = if j < bytes.len() && bytes[j] == b'.' {
        (true, j + 1)
    } else {
        (false, j)
    };
    let mut k = frac_at;
    while k < bytes.len() && bytes[k].is_ascii_digit() {
        k += 1;
    }
    Parts {
        prefix: &s[..i],
        integer: &s[i..j],
        fraction: &s[frac_at..k],
        suffix: &s[k..],
        dot,
    }
}

fn int_digit(digits: &str, width: usize, i: usize) -> Option<u8> {
    let skip = width.saturating_sub(digits.len());
    if i < skip {
        None
    } else {
        digits.as_bytes().get(i - skip).map(|b| b - b'0')
    }
}

fn frac_digit(digits: &str, _width: usize, i: usize) -> Option<u8> {
    (i < digits.len()).then(|| digits.as_bytes()[i] - b'0')
}

fn snapshot_digit(from: Option<u8>, to: Option<u8>, dir: i8, t: f32) -> Option<char> {
    match (from, to) {
        (None, None) => None,
        (Some(a), Some(b)) => Some(glyph(f32::from(a) + digit_delta(a, b, dir) as f32 * t)),
        (None, Some(b)) => {
            if t <= 0.0 {
                None
            } else {
                Some(glyph(digit_delta(0, b, dir) as f32 * t))
            }
        }
        (Some(a), None) => {
            if t >= 1.0 {
                None
            } else {
                Some(glyph(f32::from(a) + digit_delta(a, 0, dir) as f32 * t))
            }
        }
    }
}

fn snapshot(from: &str, to: &str, dir: i8, t: f32) -> String {
    if t <= 0.0 {
        return from.to_owned();
    }
    if t >= 1.0 {
        return to.to_owned();
    }
    let a = split_format(from);
    let b = split_format(to);
    let mut out = String::new();
    out.push_str(if t < 0.5 { a.prefix } else { b.prefix });
    let int_w = a.integer.len().max(b.integer.len());
    for i in 0..int_w {
        if let Some(ch) = snapshot_digit(
            int_digit(a.integer, int_w, i),
            int_digit(b.integer, int_w, i),
            dir,
            t,
        ) {
            out.push(ch);
        }
    }
    if a.dot || b.dot {
        out.push('.');
    }
    let frac_w = a.fraction.len().max(b.fraction.len());
    for i in 0..frac_w {
        if let Some(ch) = snapshot_digit(
            frac_digit(a.fraction, frac_w, i),
            frac_digit(b.fraction, frac_w, i),
            dir,
            t,
        ) {
            out.push(ch);
        }
    }
    out.push_str(if t < 0.5 { a.suffix } else { b.suffix });
    out
}

fn flow_row(
    from: &str,
    to: &str,
    t: f32,
    dir: i8,
    size: f32,
    color: Rgba,
    weight: FontWeight,
) -> gpui::Div {
    if t >= 1.0 || from == to {
        return tabular(to, size, color, weight);
    }
    let a = split_format(from);
    let b = split_format(to);
    let digit_w = px(size * DIGIT_EM);
    let mut row = div()
        .flex()
        .items_baseline()
        .text_size(px(size))
        .text_color(color)
        .font_weight(weight)
        .line_height(px(size))
        .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    row = push_symbol(row, a.prefix, b.prefix, t);
    let int_w = a.integer.len().max(b.integer.len());
    for i in 0..int_w {
        row = row.child(digit_cell(
            int_digit(a.integer, int_w, i),
            int_digit(b.integer, int_w, i),
            dir,
            t,
            size,
            digit_w,
        ));
    }
    if a.dot || b.dot {
        row = row.child(".");
    }
    let frac_w = a.fraction.len().max(b.fraction.len());
    for i in 0..frac_w {
        row = row.child(digit_cell(
            frac_digit(a.fraction, frac_w, i),
            frac_digit(b.fraction, frac_w, i),
            dir,
            t,
            size,
            digit_w,
        ));
    }
    push_symbol(row, a.suffix, b.suffix, t)
}

fn push_symbol(row: gpui::Div, from: &str, to: &str, t: f32) -> gpui::Div {
    if from == to {
        if from.is_empty() {
            row
        } else {
            row.child(SharedString::from(from.to_owned()))
        }
    } else if to.is_empty() {
        row.child(
            div()
                .opacity((1.0 - t).clamp(0.0, 1.0))
                .child(SharedString::from(from.to_owned())),
        )
    } else {
        row.child(
            div()
                .opacity(t.clamp(0.0, 1.0))
                .child(SharedString::from(to.to_owned())),
        )
    }
}

fn cell_scale(from: Option<u8>, to: Option<u8>, t: f32) -> f32 {
    match (from, to) {
        (None, None) => 0.0,
        (Some(_), Some(_)) => 1.0,
        (None, Some(_)) => t.clamp(0.0, 1.0),
        (Some(_), None) => (1.0 - t).clamp(0.0, 1.0),
    }
}

fn digit_cell(
    from: Option<u8>,
    to: Option<u8>,
    dir: i8,
    t: f32,
    size: f32,
    digit_w: gpui::Pixels,
) -> gpui::Div {
    let scale = cell_scale(from, to, t);
    if scale <= 0.0 {
        return div().w(px(0.0));
    }
    let (a, b, fade) = match (from, to) {
        (None, None) => return div().w(px(0.0)),
        (Some(a), Some(b)) => (a, b, 1.0),
        (None, Some(b)) => (0, b, t),
        (Some(a), None) => (a, 0, 1.0 - t),
    };
    let width = px(f32::from(digit_w) * scale);
    reel(a, b, dir, t, size, width).opacity(fade.clamp(0.0, 1.0))
}

fn reel(from: u8, to: u8, dir: i8, t: f32, size: f32, digit_w: gpui::Pixels) -> gpui::Div {
    let c = f32::from(from) + digit_delta(from, to, dir) as f32 * t;
    let n0 = c.floor();
    let frac = c - n0;
    div()
        .relative()
        .overflow_hidden()
        .w(digit_w)
        .h(px(size))
        .child(reel_glyph(glyph(n0), -frac * size, size, digit_w))
        .child(reel_glyph(
            glyph(n0 + 1.0),
            (1.0 - frac) * size,
            size,
            digit_w,
        ))
}

fn reel_glyph(ch: char, top: f32, size: f32, digit_w: gpui::Pixels) -> gpui::Div {
    div()
        .absolute()
        .top(px(top))
        .w(digit_w)
        .h(px(size))
        .flex()
        .justify_center()
        .child(SharedString::from(ch.to_string()))
}

pub(crate) fn tabular(
    text: impl AsRef<str>,
    size: f32,
    color: Rgba,
    weight: FontWeight,
) -> gpui::Div {
    let digit_w = px(size * DIGIT_EM);
    let mut row = div()
        .flex()
        .items_baseline()
        .text_size(px(size))
        .text_color(color)
        .font_weight(weight)
        .line_height(px(size))
        .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    for ch in text.as_ref().chars() {
        if ch.is_ascii_digit() {
            row = row.child(
                div()
                    .w(digit_w)
                    .flex()
                    .justify_center()
                    .child(SharedString::from(ch.to_string())),
            );
        } else {
            row = row.child(SharedString::from(ch.to_string()));
        }
    }
    row
}

#[cfg(test)]
fn digit_count(text: &str) -> usize {
    text.chars().filter(|ch| ch.is_ascii_digit()).count()
}

#[cfg(test)]
fn format_like(sample: &str, value: f64) -> String {
    match format_kind(sample) {
        FormatKind::Money => format!("${value:.2}"),
        FormatKind::Percent => format!("{}%", value.round() as i64),
        FormatKind::Tokens => UsageFormat::tokens(value.round() as i64),
        FormatKind::Int => (value.round() as i64).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_like_keeps_money_and_compact_tokens() {
        assert_eq!(format_like("$41.65", 1000.0), "$1000.00");
        assert_eq!(format_like("$0.00", 41.65), "$41.65");
        assert_eq!(format_like("41.7M", 1_000_000_000.0), "1.0B");
        assert_eq!(format_like("81%", 12.4), "12%");
        assert_eq!(format_like("20", 19.6), "20");
    }

    #[test]
    fn reel_follows_number_flow_trend() {
        assert_eq!(digit_delta(4, 5, 1), 1);
        assert_eq!(digit_delta(5, 4, -1), -1);
        assert_eq!(digit_delta(9, 0, 1), 1);
        assert_eq!(digit_delta(0, 9, -1), -1);
        assert_eq!(digit_delta(1, 8, 1), 7);
        assert_eq!(digit_delta(8, 1, -1), -7);
    }

    #[test]
    fn snapshot_keeps_place_values() {
        assert_eq!(snapshot("$41.65", "$42.10", 1, 0.0), "$41.65");
        assert_eq!(snapshot("$41.65", "$42.10", 1, 1.0), "$42.10");
        let mid = snapshot("$9.00", "$10.00", 1, 0.5);
        assert!(mid.starts_with('$'), "{mid}");
        assert!(mid.contains('.'), "{mid}");
    }

    #[test]
    fn leaving_digit_width_eases_out() {
        assert!((cell_scale(Some(1), None, 0.0) - 1.0).abs() < 0.001);
        assert!((cell_scale(Some(1), None, 0.5) - 0.5).abs() < 0.001);
        assert!(cell_scale(Some(1), None, 1.0) < 0.001);
        assert!((cell_scale(None, Some(1), 0.5) - 0.5).abs() < 0.001);
        assert!((cell_scale(Some(4), Some(2), 0.3) - 1.0).abs() < 0.001);
    }

    #[test]
    fn width_holds_until_a_new_digit() {
        assert_eq!(digit_count("$44.10"), digit_count("$45.10"));
        assert!(digit_count("$1000.00") > digit_count("$999.00"));
    }
}
