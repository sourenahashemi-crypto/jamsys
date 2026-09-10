//! Layer 2: learned baselines.
//!
//! Robust statistics over a bounded reservoir, partitioned by operating context.
//! See `docs/ANOMALY-MODEL.md` for the reasoning; the five guards implemented here are
//! what separate a usable detector from one that cries wolf.

use crate::types::Context;
use crate::util::{mad, median, percentile, robust_z};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-metric tuning. The absolute floors are as important as the z-threshold: idle
/// power moving from 9.0 W to 9.8 W can be a 4-sigma event and still be irrelevant.
#[derive(Clone, Copy, Debug)]
pub struct MetricTuning {
    /// Lower bound on MAD, so a perfectly constant metric cannot produce infinite z.
    pub min_mad: f64,
    /// The deviation must also be at least this large in absolute terms.
    pub min_abs_delta: f64,
    /// And must persist for this long.
    pub dwell_s: i64,
}

impl Default for MetricTuning {
    fn default() -> Self {
        MetricTuning { min_mad: 1.0, min_abs_delta: 0.0, dwell_s: 300 }
    }
}

/// One metric in one context.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Series {
    /// Bounded reservoir of recent values.
    pub samples: Vec<f64>,
    /// Total ever seen, which keeps growing after the reservoir is full.
    pub n: u32,
    pub median: f64,
    pub mad: f64,
    pub p05: f64,
    pub p95: f64,
    /// Recomputing percentiles on every insert is wasteful; recompute every 16th.
    dirty: u32,
}

const RESERVOIR_DEFAULT: usize = 512;

impl Series {
    pub fn push(&mut self, v: f64, window: usize) {
        if !v.is_finite() {
            return;
        }
        self.n = self.n.saturating_add(1);
        if self.samples.len() < window {
            self.samples.push(v);
        } else {
            // Reservoir sampling keeps the retained set representative of the whole
            // history rather than only of the last few minutes. A plain ring buffer
            // would let one busy hour erase a week of learned normality.
            let k = pseudo_random(self.n) as usize % (self.n as usize);
            if k < window {
                self.samples[k] = v;
            }
        }
        self.dirty += 1;
        if self.dirty >= 16 || self.n < 32 {
            self.recompute();
        }
    }

    pub fn recompute(&mut self) {
        self.dirty = 0;
        if self.samples.is_empty() {
            return;
        }
        self.median = median(&self.samples);
        self.mad = mad(&self.samples, self.median);
        self.p05 = percentile(&self.samples, 0.05);
        self.p95 = percentile(&self.samples, 0.95);
    }

    pub fn z(&self, v: f64, min_mad: f64) -> f64 {
        robust_z(v, self.median, self.mad, min_mad)
    }

    pub fn ready(&self, warmup: u32) -> bool {
        self.n >= warmup && self.samples.len() >= 8
    }

    /// Human-readable normal range, for the alert text.
    pub fn range_text(&self, unit: &str) -> String {
        format!("{:.1}–{:.1} {unit} (median {:.1}, from {} samples)", self.p05, self.p95, self.median, self.n)
    }
}

/// Deterministic PRNG (SplitMix64 finaliser). Reproducible across runs, so the same
/// input sequence produces the same reservoir — which makes the tests deterministic.
fn pseudo_random(n: u32) -> u64 {
    let mut z = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1234_5678_9ABC_DEF0);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// All learned series, keyed by `(metric key, context key)`.
pub struct Baselines {
    series: HashMap<(String, &'static str), Series>,
    tuning: HashMap<String, MetricTuning>,
    window: usize,
    warmup: u32,
    z_threshold: f64,
    /// Metrics currently in an alerting state, excluded from learning so an ongoing
    /// fault cannot slowly teach the baseline that it is normal.
    frozen: std::collections::HashSet<String>,
}

impl Baselines {
    pub fn new(window: usize, warmup: u32, z_threshold: f64) -> Self {
        let mut tuning = HashMap::new();
        // Floors chosen from what is physically meaningful on a laptop, not from data.
        tuning.insert("power.system_w".into(), MetricTuning { min_mad: 0.5, min_abs_delta: 5.0, dwell_s: 600 });
        tuning.insert("power.cpu_package_w".into(), MetricTuning { min_mad: 0.5, min_abs_delta: 4.0, dwell_s: 600 });
        tuning.insert("thermal.cpu_package_c".into(), MetricTuning { min_mad: 1.0, min_abs_delta: 10.0, dwell_s: 300 });
        tuning.insert("memory.used_bytes".into(), MetricTuning { min_mad: 1e8, min_abs_delta: 2e9, dwell_s: 600 });
        tuning.insert("cpu.usage_pct".into(), MetricTuning { min_mad: 3.0, min_abs_delta: 25.0, dwell_s: 300 });
        tuning.insert("gpu.power_w".into(), MetricTuning { min_mad: 1.0, min_abs_delta: 5.0, dwell_s: 300 });
        Baselines {
            series: HashMap::new(),
            tuning,
            window: if window == 0 { RESERVOIR_DEFAULT } else { window },
            warmup,
            z_threshold,
            frozen: Default::default(),
        }
    }

    pub fn tuning_for(&self, metric: &str) -> MetricTuning {
        // Fall back on the un-instanced key so `network.rx_bps[wlp108s0]` inherits the
        // tuning defined for `network.rx_bps`.
        let base = metric.split('[').next().unwrap_or(metric);
        self.tuning.get(metric).or_else(|| self.tuning.get(base)).copied().unwrap_or_default()
    }

    pub fn freeze(&mut self, metric: &str) {
        self.frozen.insert(metric.to_string());
    }
    pub fn thaw(&mut self, metric: &str) {
        self.frozen.remove(metric);
    }

    pub fn observe(&mut self, metric: &str, ctx: Context, value: f64) {
        if self.frozen.contains(metric) {
            return;
        }
        let window = self.window;
        self.series
            .entry((metric.to_string(), ctx.key()))
            .or_default()
            .push(value, window);
    }

    pub fn get(&self, metric: &str, ctx: Context) -> Option<&Series> {
        self.series.get(&(metric.to_string(), ctx.key()))
    }

    /// The verdict for one observation.
    pub fn evaluate(&self, metric: &str, ctx: Context, value: f64) -> Verdict {
        let Some(s) = self.get(metric, ctx) else {
            return Verdict::NoBaseline;
        };
        if !s.ready(self.warmup) {
            return Verdict::Learning { have: s.n, need: self.warmup };
        }
        let t = self.tuning_for(metric);
        let z = s.z(value, t.min_mad);
        let delta = value - s.median;
        // Both tests must pass. Statistical significance without practical
        // significance is the main source of nuisance alerts.
        if z.abs() >= self.z_threshold && delta.abs() >= t.min_abs_delta {
            Verdict::Deviating { z, delta, median: s.median, p05: s.p05, p95: s.p95, n: s.n }
        } else {
            Verdict::Normal { z, median: s.median, p05: s.p05, p95: s.p95 }
        }
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Serialise for persistence.
    pub fn export(&self) -> Vec<(String, String, Series)> {
        self.series.iter().map(|((m, c), s)| (m.clone(), c.to_string(), s.clone())).collect()
    }

    pub fn import(&mut self, metric: &str, context: &str, s: Series) {
        // Contexts are a closed set; map back to the 'static keys.
        let ck: &'static str = match context {
            "bat:idle" => "bat:idle",
            "bat:active" => "bat:active",
            "ac:idle" => "ac:idle",
            "ac:active" => "ac:active",
            _ => return,
        };
        self.series.insert((metric.to_string(), ck), s);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Never seen this metric in this context.
    NoBaseline,
    /// Seen, but not enough yet to judge.
    Learning { have: u32, need: u32 },
    Normal { z: f64, median: f64, p05: f64, p95: f64 },
    Deviating { z: f64, delta: f64, median: f64, p05: f64, p95: f64, n: u32 },
}

impl Verdict {
    pub fn is_deviating(&self) -> bool {
        matches!(self, Verdict::Deviating { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bat_idle() -> Context {
        Context::simple(true, true)
    }
    fn ac_active() -> Context {
        Context::simple(false, false)
    }

    fn feed(b: &mut Baselines, metric: &str, ctx: Context, vals: &[f64], repeat: usize) {
        for _ in 0..repeat {
            for v in vals {
                b.observe(metric, ctx, *v);
            }
        }
    }

    #[test]
    fn nothing_fires_before_warmup() {
        let mut b = Baselines::new(512, 120, 3.5);
        feed(&mut b, "power.system_w", bat_idle(), &[10.0, 11.0, 12.0], 5); // 15 samples
        match b.evaluate("power.system_w", bat_idle(), 50.0) {
            Verdict::Learning { have, need } => {
                assert_eq!(have, 15);
                assert_eq!(need, 120);
            }
            other => panic!("must not judge during warm-up, got {other:?}"),
        }
    }

    #[test]
    fn the_worked_example_from_the_spec_fires() {
        // Learned idle draw 8.7-13.5 W; a sustained 28.4 W must be flagged.
        let mut b = Baselines::new(512, 120, 3.5);
        let normal = [9.1, 10.4, 11.2, 10.8, 9.7, 12.3, 13.1, 10.0, 11.5, 10.2];
        feed(&mut b, "power.system_w", bat_idle(), &normal, 20); // 200 samples
        let v = b.evaluate("power.system_w", bat_idle(), 28.4);
        match v {
            Verdict::Deviating { z, delta, median, p05, p95, n } => {
                assert!(z > 3.5, "z was {z}");
                assert!(delta > 15.0);
                assert!((9.0..13.0).contains(&median), "median {median}");
                assert!(p05 < p95);
                assert_eq!(n, 200);
            }
            other => panic!("expected a deviation, got {other:?}"),
        }
    }

    #[test]
    fn a_small_but_statistically_significant_change_is_ignored() {
        // This is the guard that matters most: idle power 9.0 -> 9.9 W is many sigma
        // on a tight distribution and completely irrelevant to a user.
        let mut b = Baselines::new(512, 120, 3.5);
        feed(&mut b, "power.system_w", bat_idle(), &[9.0, 9.05, 8.95, 9.02, 8.98], 40);
        let v = b.evaluate("power.system_w", bat_idle(), 9.9);
        assert!(!v.is_deviating(), "0.9 W above a 9 W baseline must not alert: {v:?}");
        // But a real jump still fires.
        assert!(b.evaluate("power.system_w", bat_idle(), 30.0).is_deviating());
    }

    #[test]
    fn a_constant_metric_does_not_produce_infinite_z() {
        let mut b = Baselines::new(512, 50, 3.5);
        feed(&mut b, "thermal.cpu_package_c", ac_active(), &[45.0], 200);
        // MAD is exactly 0 here.
        let s = b.get("thermal.cpu_package_c", ac_active()).unwrap();
        assert_eq!(s.mad, 0.0);
        match b.evaluate("thermal.cpu_package_c", ac_active(), 46.0) {
            Verdict::Normal { z, .. } => assert!(z.is_finite(), "z must be finite, got {z}"),
            Verdict::Deviating { z, .. } => panic!("1 C above constant must not alert (z={z})"),
            other => panic!("unexpected {other:?}"),
        }
        // A genuinely large excursion still fires despite the floor.
        assert!(b.evaluate("thermal.cpu_package_c", ac_active(), 95.0).is_deviating());
    }

    #[test]
    fn contexts_do_not_contaminate_each_other() {
        let mut b = Baselines::new(512, 50, 3.5);
        // Idle on battery: ~10 W. Active on AC: ~45 W.
        feed(&mut b, "power.system_w", bat_idle(), &[10.0, 11.0, 9.5], 40);
        feed(&mut b, "power.system_w", ac_active(), &[44.0, 46.0, 45.0], 40);
        // 45 W is alarming while idle on battery...
        assert!(b.evaluate("power.system_w", bat_idle(), 45.0).is_deviating());
        // ...and entirely normal while active on AC.
        assert!(!b.evaluate("power.system_w", ac_active(), 45.0).is_deviating());
    }

    #[test]
    fn an_unseen_context_reports_no_baseline_rather_than_guessing() {
        let mut b = Baselines::new(512, 10, 3.5);
        feed(&mut b, "power.system_w", bat_idle(), &[10.0], 50);
        assert_eq!(b.evaluate("power.system_w", ac_active(), 99.0), Verdict::NoBaseline);
    }

    #[test]
    fn one_huge_outlier_does_not_move_the_baseline() {
        let mut b = Baselines::new(512, 50, 3.5);
        feed(&mut b, "power.system_w", bat_idle(), &[10.0, 11.0, 10.5], 40);
        let before = b.get("power.system_w", bat_idle()).unwrap().median;
        // A single 400 W reading (a bad sensor sample).
        b.observe("power.system_w", bat_idle(), 400.0);
        let mut s = b.get("power.system_w", bat_idle()).unwrap().clone();
        s.recompute();
        assert!((s.median - before).abs() < 1.0, "median moved from {before} to {}", s.median);
    }

    #[test]
    fn freezing_stops_an_ongoing_fault_from_normalising_itself() {
        let mut b = Baselines::new(512, 50, 3.5);
        feed(&mut b, "power.system_w", bat_idle(), &[10.0, 11.0], 60);
        b.freeze("power.system_w");
        for _ in 0..500 {
            b.observe("power.system_w", bat_idle(), 30.0);
        }
        assert!(
            b.evaluate("power.system_w", bat_idle(), 30.0).is_deviating(),
            "a frozen metric must not learn its own fault as normal"
        );
        b.thaw("power.system_w");
        for _ in 0..2000 {
            b.observe("power.system_w", bat_idle(), 30.0);
        }
        assert!(!b.evaluate("power.system_w", bat_idle(), 30.0).is_deviating(),
                "after thawing, a genuine new normal should eventually be learned");
    }

    #[test]
    fn the_reservoir_stays_bounded() {
        let mut b = Baselines::new(64, 10, 3.5);
        for i in 0..100_000 {
            b.observe("x", bat_idle(), (i % 100) as f64);
        }
        let s = b.get("x", bat_idle()).unwrap();
        assert_eq!(s.samples.len(), 64, "reservoir must not grow");
        assert!(s.n >= 100_000, "but the total count keeps rising");
    }

    #[test]
    fn reservoir_stays_representative_of_the_whole_history() {
        // With a plain ring buffer the last 64 values would be 0..63 and the median
        // would collapse; reservoir sampling should keep it near the true 50.
        let mut b = Baselines::new(64, 10, 3.5);
        for i in 0..20_000 {
            b.observe("x", bat_idle(), (i % 101) as f64);
        }
        let mut s = b.get("x", bat_idle()).unwrap().clone();
        s.recompute();
        assert!((s.median - 50.0).abs() < 20.0, "median drifted to {}", s.median);
    }

    #[test]
    fn tuning_falls_back_from_instance_to_base_metric() {
        let b = Baselines::new(512, 120, 3.5);
        let t = b.tuning_for("power.system_w[BAT0]");
        assert_eq!(t.min_abs_delta, 5.0, "instanced key should inherit base tuning");
        assert_eq!(b.tuning_for("something.unknown").min_abs_delta, 0.0);
    }

    #[test]
    fn non_finite_observations_are_ignored() {
        let mut b = Baselines::new(512, 5, 3.5);
        b.observe("x", bat_idle(), f64::NAN);
        b.observe("x", bat_idle(), f64::INFINITY);
        assert_eq!(b.get("x", bat_idle()).map(|s| s.n), Some(0));
    }

    #[test]
    fn range_text_is_readable_by_a_non_expert() {
        let mut s = Series::default();
        for v in [9.1, 10.4, 11.2, 10.8, 9.7, 12.3, 13.1, 10.0] {
            s.push(v, 512);
        }
        s.recompute();
        let t = s.range_text("W");
        assert!(t.contains('–') && t.contains("median") && t.contains("samples"), "got {t}");
    }
}
