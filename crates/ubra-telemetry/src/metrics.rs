//! In-memory counters and latency histograms, drained into the periodic
//! `metrics` event by the health sampler.
//!
//! Each call takes one uncontended mutex. That is fine per frame, per RPC or
//! per keystroke; it is not fine per PTY read or per byte. Aggregate those
//! locally and report a total.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::value::Value;

/// Upper bounds, in milliseconds, of the histogram buckets.
const BOUNDS_MS: [f64; 20] = [
    0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 12.0, 16.0, 24.0, 33.0, 50.0, 75.0, 100.0, 150.0, 250.0, 500.0,
    1000.0, 2500.0, 5000.0, 10000.0,
];

#[derive(Default)]
struct Histogram {
    count: u64,
    sum: f64,
    max: f64,
    buckets: [u64; BOUNDS_MS.len() + 1],
}

impl Histogram {
    fn observe(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.max = self.max.max(value);
        let bucket = BOUNDS_MS
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(BOUNDS_MS.len());
        self.buckets[bucket] += 1;
    }

    /// The bucket upper bound at or above `q`, clamped to the observed max.
    fn quantile(&self, q: f64) -> f64 {
        let target = (q * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                return BOUNDS_MS
                    .get(index)
                    .copied()
                    .unwrap_or(self.max)
                    .min(self.max);
            }
        }
        self.max
    }

    fn summary(&self) -> Value {
        let round = |v: f64| (v * 100.0).round() / 100.0;
        Value::Obj(vec![
            ("n", Value::from(self.count)),
            (
                "avg",
                Value::from(round(self.sum / self.count.max(1) as f64)),
            ),
            ("p50", Value::from(self.quantile(0.5))),
            ("p90", Value::from(self.quantile(0.9))),
            ("p99", Value::from(self.quantile(0.99))),
            ("max", Value::from(round(self.max))),
        ])
    }
}

#[derive(Default)]
struct Registry {
    counters: HashMap<&'static str, u64>,
    histograms: HashMap<&'static str, Histogram>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

/// Adds `n` to a counter reported (and reset) every sample interval.
pub fn count(name: &'static str, n: u64) {
    if !crate::is_enabled() {
        return;
    }
    if let Ok(mut registry) = REGISTRY.lock() {
        *registry
            .get_or_insert_with(Registry::default)
            .counters
            .entry(name)
            .or_default() += n;
    }
}

/// Records one latency observation in milliseconds.
pub fn observe_ms(name: &'static str, ms: f64) {
    if !crate::is_enabled() || !ms.is_finite() {
        return;
    }
    if let Ok(mut registry) = REGISTRY.lock() {
        registry
            .get_or_insert_with(Registry::default)
            .histograms
            .entry(name)
            .or_default()
            .observe(ms.max(0.0));
    }
}

/// Records one latency observation from a `Duration`.
pub fn observe(name: &'static str, elapsed: std::time::Duration) {
    observe_ms(name, elapsed.as_secs_f64() * 1000.0);
}

/// Takes everything accumulated since the last drain.
type Named = Vec<(&'static str, Value)>;

pub(crate) fn drain() -> Option<(Named, Named)> {
    let registry = REGISTRY.lock().ok()?.take()?;
    let mut counters: Vec<_> = registry
        .counters
        .into_iter()
        .map(|(name, n)| (name, Value::from(n)))
        .collect();
    counters.sort_by_key(|(name, _)| *name);
    let mut histograms: Vec<_> = registry
        .histograms
        .into_iter()
        .map(|(name, histogram)| (name, histogram.summary()))
        .collect();
    histograms.sort_by_key(|(name, _)| *name);
    Some((counters, histograms))
}

#[cfg(test)]
mod tests {
    use super::Histogram;

    #[test]
    fn quantiles_come_from_bucket_bounds() {
        let mut histogram = Histogram::default();
        for _ in 0..90 {
            histogram.observe(3.0);
        }
        for _ in 0..10 {
            histogram.observe(120.0);
        }
        assert_eq!(histogram.quantile(0.5), 4.0);
        assert_eq!(histogram.quantile(0.9), 4.0);
        assert_eq!(histogram.quantile(0.99), 120.0);
    }
}
