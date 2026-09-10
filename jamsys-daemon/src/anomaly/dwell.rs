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
}

impl DwellTracker {
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
            return None;
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
    }

    /// Drop keys that have been held implausibly long, bounding memory on a machine
    /// that runs for months.
    pub fn gc(&mut self, now_ms: i64) {
        self.since.retain(|_, s| now_ms - *s < 30 * 86_400_000);
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
