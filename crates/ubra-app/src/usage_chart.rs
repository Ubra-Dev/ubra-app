//! Liveline-style usage chart: monotone spline over the selected window.
use gpui::{Bounds, PathBuilder, Pixels, Point, Rgba, Window, fill, point, px, size};

const Y_MARGIN: f32 = 0.12;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChartSample {
    pub time: i64,
    pub value: f64,
}

pub(crate) fn window_hours(window_days: f32) -> f32 {
    window_days.max(1.0) * 24.0
}

pub(crate) fn compute_range(values: &[f64]) -> (f32, f32) {
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    if !max.is_finite() {
        return (0.0, 1.0);
    }
    (0.0, (max * (1.0 + f64::from(Y_MARGIN))).max(0.4) as f32)
}

#[cfg(test)]
fn visible_samples(samples: &[ChartSample], window_days: f32, end: i64) -> Vec<&ChartSample> {
    let hours = window_hours(window_days);
    let left = end as f32 + 1.0 - hours;
    samples
        .iter()
        .filter(|sample| sample.time as f32 + 1.0 > left && sample.time <= end)
        .collect()
}

pub(crate) fn displayed_series(
    samples: &[ChartSample],
    window_days: f32,
    end: i64,
) -> Vec<ChartSample> {
    let mut total = 0.0;
    let cumulative: Vec<ChartSample> = samples
        .iter()
        .map(|sample| {
            total += sample.value;
            ChartSample {
                time: sample.time,
                value: total,
            }
        })
        .collect();
    let left = end as f32 + 1.0 - window_hours(window_days);
    let origin = origin_at(&cumulative, left);
    let mut series: Vec<ChartSample> = cumulative
        .into_iter()
        .filter(|sample| sample.time as f32 + 1.0 > left && sample.time <= end)
        .map(|sample| ChartSample {
            time: sample.time,
            value: (sample.value - origin).max(0.0),
        })
        .collect();
    smooth_cumulative(&mut series, smooth_radius(window_days));
    cap_points(series, 72)
}

fn origin_at(cumulative: &[ChartSample], time: f32) -> f64 {
    if cumulative.is_empty() || time <= cumulative[0].time as f32 {
        return 0.0;
    }
    let mut prev_t = cumulative[0].time as f32;
    let mut prev_v = 0.0;
    for sample in cumulative {
        let t = sample.time as f32 + 1.0;
        if time <= t {
            let span = t - prev_t;
            let frac = if span <= 0.0 {
                1.0
            } else {
                (time - prev_t) / span
            };
            return prev_v + (sample.value - prev_v) * f64::from(frac);
        }
        prev_t = t;
        prev_v = sample.value;
    }
    cumulative[cumulative.len() - 1].value
}

fn smooth_radius(window_days: f32) -> f32 {
    (window_hours(window_days) / 36.0 - 0.6).max(0.0)
}

fn smooth_cumulative(points: &mut [ChartSample], radius: f32) {
    if radius < 1.0 || points.len() < 4 {
        return;
    }
    let values: Vec<f64> = points.iter().map(|point| point.value).collect();
    let first = values[0];
    let last = values[values.len() - 1];
    let reach = radius.ceil() as usize;
    for (index, point) in points.iter_mut().enumerate() {
        let mut weight_sum = 0.0;
        let mut value_sum = 0.0;
        let lo = index.saturating_sub(reach);
        let hi = (index + reach).min(values.len() - 1);
        for (other, &value) in values.iter().enumerate().take(hi + 1).skip(lo) {
            let weight = (radius + 1.0 - (other as f32 - index as f32).abs()).max(0.0);
            if weight == 0.0 {
                continue;
            }
            weight_sum += f64::from(weight);
            value_sum += value * f64::from(weight);
        }
        if weight_sum > 0.0 {
            point.value = value_sum / weight_sum;
        }
    }
    if let Some(point) = points.first_mut() {
        point.value = first;
    }
    if let Some(point) = points.last_mut() {
        point.value = last;
    }
}

fn cap_points(points: Vec<ChartSample>, max: usize) -> Vec<ChartSample> {
    let len = points.len();
    if len <= max || max < 3 {
        return points;
    }
    let last = max - 1;
    (0..max)
        .map(|index| {
            if index == last {
                points[len - 1]
            } else {
                let src = (index as f32 * (len - 1) as f32 / last as f32).round() as usize;
                points[src.min(len - 1)]
            }
        })
        .collect()
}

pub(crate) fn combined_series(providers: &[Vec<ChartSample>; 3]) -> Vec<ChartSample> {
    providers[0]
        .iter()
        .enumerate()
        .map(|(index, sample)| ChartSample {
            time: sample.time,
            value: sample.value
                + providers[1]
                    .get(index)
                    .map(|item| item.value)
                    .unwrap_or(0.0)
                + providers[2]
                    .get(index)
                    .map(|item| item.value)
                    .unwrap_or(0.0),
        })
        .collect()
}

pub(crate) fn series_range(lines: &[Vec<ChartSample>]) -> (f32, f32) {
    compute_range(
        &lines
            .iter()
            .flatten()
            .map(|sample| sample.value)
            .collect::<Vec<_>>(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    lines: &[(Vec<ChartSample>, Rgba)],
    window_days: f32,
    y_min: f32,
    y_max: f32,
    end: i64,
    scrub: Option<f32>,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width < 8.0 || height < 8.0 {
        return;
    }
    let pad_top = 22.0;
    let pad_bottom = 2.0;
    let chart_h = (height - pad_top - pad_bottom).max(8.0);
    let hours = window_hours(window_days);
    let left = end as f32 + 1.0 - hours;
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let range = (y_max - y_min).max(0.001);
    let to_x = |time: f32| origin_x + ((time + 0.5 - left) / hours).clamp(0.0, 1.0) * width;
    let to_y =
        |value: f32| origin_y + pad_top + (1.0 - (value - y_min) / range).clamp(0.0, 1.0) * chart_h;
    let base_y = origin_y + pad_top + chart_h;
    let filled = lines.len() == 1;
    let scrub_x = scrub.map(|t| origin_x + t.clamp(0.0, 1.0) * width);
    let mut dots: Vec<(Rgba, [f32; 2])> = Vec::new();
    for (samples, color) in lines {
        let mut pts: Vec<[f32; 2]> = Vec::with_capacity(samples.len() + 1);
        if samples.len() == 1
            && let Some(single) = samples.first()
        {
            // Single-sample flat line, as before: the anchor check below
            // applies to the expanded pair, so push it first when needed.
            let y = to_y(single.value as f32);
            if (y - base_y).abs() > 0.5 {
                pts.push([origin_x, base_y]);
            }
            pts.push([origin_x, y]);
            pts.push([origin_x + width, y]);
        } else {
            // Anchor first so the fill closes to the baseline; identical vec
            // to the old map-then-insert(0) build without the O(n) shift.
            if let Some(first) = samples.first() {
                let mapped = [to_x(first.time as f32), to_y(first.value as f32)];
                if (mapped[0] - origin_x).abs() > 0.5 || (mapped[1] - base_y).abs() > 0.5 {
                    pts.push([origin_x, base_y]);
                }
            }
            pts.extend(
                samples
                    .iter()
                    .map(|sample| [to_x(sample.time as f32), to_y(sample.value as f32)]),
            );
        }
        if pts.len() < 2 {
            continue;
        }
        let (fill_path, line_path) = spline_paths(&pts, base_y, 2.0);
        if filled && let Some(fill_path) = fill_path {
            window.paint_path(fill_path, color.alpha(0.16));
        }
        if let Some(line) = line_path {
            window.paint_path(line, *color);
        }
        let last = pts[pts.len() - 1];
        dots.push((*color, last));
        if let Some(x) = scrub_x {
            let hit = interpolate_y_at(&pts, x);
            if (hit[0] - last[0]).abs() > 12.0 {
                dots.push((*color, hit));
            }
        }
    }
    if let (Some(x), Some((_, color))) = (scrub_x, lines.first()) {
        window.paint_quad(fill(
            Bounds::new(
                point(px(x - 0.5), px(origin_y + pad_top)),
                size(px(1.0), px(chart_h)),
            ),
            color.alpha(0.5),
        ));
    }
    let radius = px(3.5);
    for (color, last) in dots {
        let center = point(px(last[0]), px(last[1]));
        window.paint_quad(
            fill(
                Bounds::new(
                    point(center.x - radius, center.y - radius),
                    size(radius * 2.0, radius * 2.0),
                ),
                color,
            )
            .corner_radii(radius),
        );
    }
}

fn interpolate_y_at(pts: &[[f32; 2]], x: f32) -> [f32; 2] {
    let last = pts[pts.len() - 1];
    if x <= pts[0][0] {
        return pts[0];
    }
    if x >= last[0] {
        return last;
    }
    for i in 0..pts.len() - 1 {
        let a = pts[i];
        let b = pts[i + 1];
        if x > b[0] {
            continue;
        }
        let span = (b[0] - a[0]).max(0.0001);
        let t = (x - a[0]) / span;
        return [x, a[1] + (b[1] - a[1]) * t];
    }
    last
}

fn spline_paths(
    pts: &[[f32; 2]],
    base_y: f32,
    width: f32,
) -> (Option<gpui::Path<Pixels>>, Option<gpui::Path<Pixels>>) {
    if pts.len() < 2 {
        return (None, None);
    }
    // One Fritsch-Carlson pass shared by fill and stroke; previously each
    // built its own triple vec per line per frame.
    let triples = if pts.len() == 2 {
        Vec::new()
    } else {
        fritsch_carlson(pts)
    };
    let mut fill = PathBuilder::fill();
    fill.move_to(pt(pts[0][0], base_y));
    fill.line_to(pt(pts[0][0], pts[0][1]));
    append_spline(&mut fill, pts, &triples);
    fill.line_to(pt(pts[pts.len() - 1][0], base_y));
    fill.close();
    let mut stroke = PathBuilder::stroke(px(width));
    stroke.move_to(pt(pts[0][0], pts[0][1]));
    append_spline(&mut stroke, pts, &triples);
    (fill.build().ok(), stroke.build().ok())
}

fn append_spline(
    path: &mut PathBuilder,
    pts: &[[f32; 2]],
    triples: &[([f32; 2], [f32; 2], [f32; 2])],
) {
    if pts.len() == 2 {
        path.line_to(pt(pts[1][0], pts[1][1]));
        return;
    }
    for (ctrl_a, ctrl_b, to) in triples {
        path.cubic_bezier_to(
            pt(to[0], to[1]),
            pt(ctrl_a[0], ctrl_a[1]),
            pt(ctrl_b[0], ctrl_b[1]),
        );
    }
}

fn fritsch_carlson(pts: &[[f32; 2]]) -> Vec<([f32; 2], [f32; 2], [f32; 2])> {
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }
    let mut h = vec![0.0; n - 1];
    let mut delta = vec![0.0; n - 1];
    for i in 0..n - 1 {
        h[i] = pts[i + 1][0] - pts[i][0];
        delta[i] = if h[i] == 0.0 {
            0.0
        } else {
            (pts[i + 1][1] - pts[i][1]) / h[i]
        };
    }
    let mut m = vec![0.0; n];
    m[0] = delta[0];
    m[n - 1] = delta[n - 2];
    for i in 1..n - 1 {
        m[i] = if delta[i - 1] * delta[i] <= 0.0 {
            0.0
        } else {
            (delta[i - 1] + delta[i]) / 2.0
        };
    }
    for i in 0..n - 1 {
        if delta[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
            continue;
        }
        let alpha = m[i] / delta[i];
        let beta = m[i + 1] / delta[i];
        let s2 = alpha * alpha + beta * beta;
        if s2 > 9.0 {
            let s = 3.0 / s2.sqrt();
            m[i] = s * alpha * delta[i];
            m[i + 1] = s * beta * delta[i];
        }
    }
    (0..n - 1)
        .map(|i| {
            let hi = h[i];
            (
                [pts[i][0] + hi / 3.0, pts[i][1] + m[i] * hi / 3.0],
                [
                    pts[i + 1][0] - hi / 3.0,
                    pts[i + 1][1] - m[i + 1] * hi / 3.0,
                ],
                pts[i + 1],
            )
        })
        .collect()
}

fn pt(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longer_window_smooths_hourly_spikes() {
        let samples: Vec<ChartSample> = (0..168)
            .map(|hour| ChartSample {
                time: hour,
                value: if hour % 8 == 0 { 8.0 } else { 0.0 },
            })
            .collect();
        let raw: Vec<f64> = {
            let mut total = 0.0;
            samples
                .iter()
                .map(|sample| {
                    total += sample.value;
                    total
                })
                .collect()
        };
        let mut raw_jump = 0.0_f64;
        for pair in raw.windows(2) {
            raw_jump = raw_jump.max(pair[1] - pair[0]);
        }
        let series = displayed_series(&samples, 7.0, 167);
        let last_raw = raw[raw.len() - 1];
        assert!((series.last().unwrap().value - last_raw).abs() < 0.01);
        let mut smooth_jump = 0.0_f64;
        for pair in series.windows(2) {
            smooth_jump = smooth_jump.max(pair[1].value - pair[0].value);
        }
        assert!(smooth_jump < raw_jump);
        assert!(series.len() <= 72);
    }

    #[test]
    fn today_window_keeps_hourly_samples() {
        let samples: Vec<ChartSample> = (100..124)
            .map(|time| ChartSample { time, value: 1.0 })
            .collect();
        let visible = visible_samples(&samples, 1.0, 123);
        assert_eq!(visible.len(), 24);
        assert_eq!(window_hours(1.0), 24.0);
    }

    #[test]
    fn index_holds_flat_then_only_rises() {
        let samples = [
            ChartSample {
                time: 10,
                value: 2.0,
            },
            ChartSample {
                time: 11,
                value: 0.0,
            },
            ChartSample {
                time: 12,
                value: 5.0,
            },
        ];
        let series = displayed_series(&samples, 1.0, 12);
        assert_eq!(series.len(), 3);
        assert!((series[0].value - 2.0).abs() < 0.01);
        assert!((series[1].value - 2.0).abs() < 0.01);
        assert!((series[2].value - 7.0).abs() < 0.01);
        let (y_min, y_max) = compute_range(&[2.0, 2.0, 7.0]);
        assert_eq!(y_min, 0.0);
        assert!(y_max > 7.0);
    }

    #[test]
    fn origin_at_moves_through_an_hour() {
        let mut total = 0.0;
        let cumulative: Vec<ChartSample> = (0..12)
            .map(|hour| {
                total += 2.0;
                ChartSample {
                    time: hour,
                    value: total,
                }
            })
            .collect();
        assert!((origin_at(&cumulative, 10.0) - 20.0).abs() < 0.01);
        assert!((origin_at(&cumulative, 10.5) - 21.0).abs() < 0.01);
        assert!((origin_at(&cumulative, 11.0) - 22.0).abs() < 0.01);
    }

    #[test]
    fn expensive_hour_entering_window_does_not_snap() {
        let mut samples: Vec<ChartSample> = (0..48)
            .map(|hour| ChartSample {
                time: hour,
                value: 1.0,
            })
            .collect();
        samples[23].value = 50.0;
        let value_at = |days: f32, hour: i64| {
            displayed_series(&samples, days, 47)
                .into_iter()
                .find(|sample| sample.time == hour)
                .map(|sample| sample.value)
                .unwrap()
        };
        let before = value_at(1.0, 40);
        let mid = value_at(1.02, 40);
        let after = value_at(1.04, 40);
        assert!(mid > before);
        assert!(after > mid);
        assert!(after - before < 50.0);
    }

    #[test]
    fn combined_series_sums_providers() {
        let hour = |value| ChartSample { time: 1, value };
        let providers = [
            vec![hour(1.0), hour(2.0)],
            vec![hour(3.0), hour(0.0)],
            vec![hour(4.0), hour(5.0)],
        ];
        let combined = combined_series(&providers);
        assert!((combined[0].value - 8.0).abs() < 0.01);
        assert!((combined[1].value - 7.0).abs() < 0.01);
    }

    #[test]
    fn scrub_hit_interpolates_between_points() {
        let pts = [[0.0, 10.0], [10.0, 0.0], [20.0, 20.0]];
        let hit = interpolate_y_at(&pts, 5.0);
        assert!((hit[0] - 5.0).abs() < 0.01);
        assert!((hit[1] - 5.0).abs() < 0.01);
        assert_eq!(interpolate_y_at(&pts, -1.0), pts[0]);
        assert_eq!(interpolate_y_at(&pts, 99.0), pts[pts.len() - 1]);
    }
}
