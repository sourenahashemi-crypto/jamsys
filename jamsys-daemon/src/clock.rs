//! Time. Three clocks, used for three different jobs, because mixing them up is how
//! monitoring daemons mis-report suspends and NTP steps.
//!
//! * `CLOCK_REALTIME`  — wall clock. Storage timestamps only.
//! * `CLOCK_MONOTONIC` — does **not** advance while suspended. All interval/dwell logic.
//! * `CLOCK_BOOTTIME`  — **does** advance while suspended.
//!
//! `BOOTTIME − MONOTONIC` is exactly the cumulative time this boot has spent asleep,
//! which is how suspend/resume is detected without a DBus subscription.

use std::time::Duration;

fn clock_gettime(id: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, correctly-sized output buffer for a supported clock id.
    unsafe { libc::clock_gettime(id, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Wall-clock milliseconds since the Unix epoch. For storage and display only.
pub fn now_ms() -> i64 {
    let d = clock_gettime(libc::CLOCK_REALTIME);
    d.as_millis() as i64
}

/// Monotonic milliseconds. Immune to NTP steps; does not count suspended time.
pub fn mono_ms() -> i64 {
    clock_gettime(libc::CLOCK_MONOTONIC).as_millis() as i64
}

/// Boot-time milliseconds. Counts suspended time.
pub fn boot_ms() -> i64 {
    clock_gettime(libc::CLOCK_BOOTTIME).as_millis() as i64
}

/// Cumulative milliseconds this boot has spent suspended.
pub fn suspended_total_ms() -> i64 {
    // Read boottime first: if a suspend begins between the two reads the result is
    // slightly low rather than negative, and the caller's saturating_sub keeps it sane.
    let b = boot_ms();
    let m = mono_ms();
    (b - m).max(0)
}

/// Detects suspend/resume by watching the boottime-minus-monotonic gap grow.
///
/// A tick that "took" far longer in boottime than in monotonic time means the machine
/// was asleep in between; the difference is the sleep duration, to the millisecond.
pub struct SuspendWatch {
    last_mono: i64,
    last_boot: i64,
    /// A gap below this is scheduling jitter, not a suspend.
    threshold_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SuspendEvent {
    pub slept_ms: i64,
    pub resumed_at_ms: i64,
}

impl SuspendWatch {
    pub fn new() -> Self {
        SuspendWatch { last_mono: mono_ms(), last_boot: boot_ms(), threshold_ms: 5_000 }
    }

    /// Construct with explicit clock readings. Exposed so suspend detection can be
    /// exercised without actually suspending the machine.
    pub fn with_state(mono: i64, boot: i64, threshold_ms: i64) -> Self {
        SuspendWatch { last_mono: mono, last_boot: boot, threshold_ms }
    }

    /// Call once per loop iteration. Returns `Some` on the first tick after a resume.
    pub fn poll(&mut self) -> Option<SuspendEvent> {
        self.check(mono_ms(), boot_ms(), now_ms())
    }

    /// Pure core, so the detection logic is testable without actually suspending.
    pub fn check(&mut self, mono: i64, boot: i64, wall: i64) -> Option<SuspendEvent> {
        let d_mono = mono - self.last_mono;
        let d_boot = boot - self.last_boot;
        self.last_mono = mono;
        self.last_boot = boot;
        let slept = d_boot - d_mono;
        if slept >= self.threshold_ms {
            Some(SuspendEvent { slept_ms: slept, resumed_at_ms: wall })
        } else {
            None
        }
    }
}

impl Default for SuspendWatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_are_sane() {
        assert!(now_ms() > 1_600_000_000_000, "wall clock before 2020");
        assert!(mono_ms() >= 0);
        assert!(boot_ms() >= mono_ms(), "boottime must be >= monotonic");
    }

    #[test]
    fn no_suspend_on_normal_tick() {
        let mut w = SuspendWatch::with_state(1000, 1000, 5000);
        // 2 s of ordinary wall time passes; both clocks advance together.
        assert_eq!(w.check(3000, 3000, 0), None);
    }

    #[test]
    fn scheduling_jitter_is_not_a_suspend() {
        let mut w = SuspendWatch::with_state(1000, 1000, 5000);
        // boottime ran 900 ms ahead: jitter, well under the 5 s threshold.
        assert_eq!(w.check(3000, 3900, 0), None);
    }

    #[test]
    fn detects_a_real_suspend() {
        let mut w = SuspendWatch::with_state(1000, 1000, 5000);
        // Monotonic advanced 2 s, boottime advanced 1 h 0 m 2 s => slept 1 h.
        let ev = w.check(3000, 3_600_000 + 3000, 42).expect("suspend not detected");
        assert_eq!(ev.slept_ms, 3_600_000);
        assert_eq!(ev.resumed_at_ms, 42);
    }

    #[test]
    fn suspend_is_reported_once_not_repeatedly() {
        let mut w = SuspendWatch::with_state(1000, 1000, 5000);
        assert!(w.check(3000, 3_603_000, 0).is_some());
        // Next ordinary tick must be quiet: the gap is absorbed, not re-reported.
        assert_eq!(w.check(5000, 3_605_000, 0), None);
    }
}
