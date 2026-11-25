//! Dwell tracking: "has this condition been true continuously for N seconds?"
//!
//! Nearly every rule needs this, and getting it wrong in either direction is what makes
//! monitoring tools annoying — fire instantly and every transient spike pages you; use a
//! simple counter and a condition that flickers never accumulates.

use std::collections::HashMap;

#[derive(Default)]
pub struct DwellTracker {
    /// key -> monotonic ms at which the condition first became true.
    since: HashMap<String, i64>,
    /// key -> monotonic ms at which the condition was last reported true.
    seen: HashMap<String, i64>,
}

impl DwellTracker {
    /// The longest silence that still counts as "the same episode". Longer than
    /// the slowest tier under the idle multiplier, far shorter than anything a
    /// person would call continuous.
    pub const STALE_MS: i64 = 5 * 60_000;

    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current truth of a condition.
    ///
    /// Returns `Some(seconds_held)` once it has been continuously true for at least
    /// `required_s`. Returns `None` while it is false or has not held long enough.
    pub fn update(&mut self, key: &str, condition: bool, now_ms: i64, required_s: i64) -> Option<i64> {
        if !condition {
            self.since.remove(key);
            self.seen.remove(key);
            return None;
        }
        // "Continuously true" needs the rule to have kept saying so. Several
        // rules only call this while their precondition holds (the machine is
        // idle, the process is still top of the list, the fan reads zero) and
        // simply stop calling when it does not. Without this the timer stayed
        // armed: one burst of traffic, six quiet hours, a second burst, and the
        // alert fired at once claiming the condition had held for six hours.
        if let Some(last) = self.seen.insert(key.to_string(), now_ms) {
            if now_ms - last > Self::STALE_MS {
                self.since.insert(key.to_string(), now_ms);
            }
        }
        let start = *self.since.entry(key.to_string()).or_insert(now_ms);
        // A backwards clock step must not make a condition look eternally held.
        if now_ms < start {
            self.since.insert(key.to_string(), now_ms);
            return None;
        }
        let held = (now_ms - start) / 1000;
        (held >= required_s).then_some(held)
    }

    /// How long the condition has held, whether or not it has met its threshold.
    pub fn held_s(&self, key: &str, now_ms: i64) -> i64 {
        self.since.get(key).map(|s| ((now_ms - s) / 1000).max(0)).unwrap_or(0)
    }

    pub fn is_active(&self, key: &str) -> bool {
        self.since.contains_key(key)
    }

    pub fn clear(&mut self, key: &str) {
        self.since.remove(key);
        self.seen.remove(key);
    }

    /// Drop keys that have been held implausibly long, bounding memory on a machine
    /// that runs for months.
    pub fn gc(&mut self, now_ms: i64) {
        self.since.retain(|_, s| now_ms - *s < 30 * 86_400_000);
        // A key nobody has reported for a while is finished, whatever it was;
        // per-pid keys would otherwise accumulate for a month each.
        let seen = &self.seen;
        self.since.retain(|k, _| seen.get(k).is_none_or(|l| now_ms - *l <= Self::STALE_MS));
        self.seen.retain(|_, l| now_ms - *l <= Self::STALE_MS);
    }

    pub fn len(&self) -> usize {
        self.since.len()
    }
    pub fn is_empty(&self) -> bool {
        self.since.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_that_goes_quiet_does_not_keep_its_timer_running() {
        let mut d = DwellTracker::new();
        // One true sample, then the rule stops evaluating the key altogether.
        assert_eq!(d.update("spike", true, 0, 120), None);
        // Six hours later it is true again for a single sample.
        let later = 6 * 3_600_000;
        assert_eq!(d.update("spike", true, later, 120), None,
                   "two isolated samples six hours apart are not a sustained condition");
        // From there it has to earn its dwell like anything else.
        assert_eq!(d.update("spike", true, later + 60_000, 120), None);
        assert_eq!(d.update("spike", true, later + 120_000, 120), Some(120));
    }

    #[test]
    fn slow_but_regular_reports_still_accumulate() {
        let mut d = DwellTracker::new();
        // A slow tier under the idle multiplier reports every three minutes.
        for i in 0..4 {
            d.update("slow", true, i * 180_000, 600);
        }
        assert_eq!(d.update("slow", true, 4 * 180_000, 600), Some(720));
    }

    #[test]
    fn gc_forgets_keys_nobody_reports_any_more() {
        let mut d = DwellTracker::new();
        d.update("cpu.runaway_process:4242", true, 0, 300);
        d.gc(DwellTracker::STALE_MS + 1);
        assert!(d.is_empty());
    }

    #[test]
    fn a_transient_spike_never_fires() {
        let mut d = DwellTracker::new();
        assert_eq!(d.update("cpu.hot", true, 0, 60), None);
        assert_eq!(d.update("cpu.hot", true, 30_000, 60), None);
        // Condition clears at 40 s, well before the 60 s requirement.
        assert_eq!(d.update("cpu.hot", false, 40_000, 60), None);
        assert!(!d.is_active("cpu.hot"));
    }

    #[test]
    fn a_sustained_condition_fires_once_the_dwell_elapses() {
        let mut d = DwellTracker::new();
        assert_eq!(d.update("cpu.hot", true, 0, 60), None);
        assert_eq!(d.update("cpu.hot", true, 59_000, 60), None);
        assert_eq!(d.update("cpu.hot", true, 60_000, 60), Some(60));
        assert_eq!(d.update("cpu.hot", true, 125_000, 60), Some(125), "keeps reporting while true");
    }

    #[test]
    fn a_flicker_resets_the_timer() {
        let mut d = DwellTracker::new();
        d.update("x", true, 0, 60);
        d.update("x", true, 55_000, 60);
        d.update("x", false, 56_000, 60);
        d.update("x", true, 57_000, 60);
        // The clock restarted at 57 s, so 60 s later is 117 s, not 60 s.
        assert_eq!(d.update("x", true, 100_000, 60), None);
        assert_eq!(d.update("x", true, 117_000, 60), Some(60));
    }

    #[test]
    fn keys_are_independent() {
        let mut d = DwellTracker::new();
        d.update("a", true, 0, 10);
        d.update("b", true, 5_000, 10);
        assert_eq!(d.update("a", true, 10_000, 10), Some(10));
        assert_eq!(d.update("b", true, 10_000, 10), None);
    }

    #[test]
    fn zero_dwell_fires_immediately() {
        let mut d = DwellTracker::new();
        assert_eq!(d.update("now", true, 1000, 0), Some(0));
    }

    #[test]
    fn a_backwards_clock_step_does_not_fabricate_a_long_dwell() {
        let mut d = DwellTracker::new();
        d.update("x", true, 1_000_000, 60);
        // NTP steps the clock back an hour.
        assert_eq!(d.update("x", true, 0, 60), None, "must restart, not report an hour");
        assert_eq!(d.update("x", true, 60_000, 60), Some(60));
    }

    #[test]
    fn gc_bounds_memory() {
        let mut d = DwellTracker::new();
        for i in 0..1000 {
            d.update(&format!("k{i}"), true, 0, 60);
        }
        assert_eq!(d.len(), 1000);
        d.gc(31 * 86_400_000);
        assert!(d.is_empty());
    }
}
