//! Collector framework.
//!
//! Every data source implements [`Collector`]. The [`Registry`] owns them, runs them
//! at their tier, and — critically — **isolates them**: a collector that panics or
//! errors repeatedly is quarantined rather than being allowed to take the daemon down.
//! A monitoring tool that dies because one sensor returned nonsense is worse than no
//! monitoring tool, because the user believes they are covered.

pub mod bluetooth;
pub mod cpu;
pub mod devices;
pub mod gpu;
pub mod inventory;
pub mod keyboard;
pub mod memory;
pub mod network;
pub mod power;
pub mod privileged;
pub mod process;
pub mod services;
pub mod storage;
pub mod thermal;

use crate::config::Config;
use crate::types::*;
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

/// Live state of the machine as the daemon currently understands it.
///
/// Collectors write into this; the anomaly engine and the `snapshot` IPC call read it.
/// Everything is `Option` because on some machine, somewhere, each of these is absent.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Snapshot {
    pub cpu: cpu::CpuState,
    pub memory: memory::MemState,
    pub thermal: thermal::ThermalState,
    pub power: power::PowerState,
    pub gpu: gpu::GpuState,
    pub storage: storage::StorageState,
    pub network: network::NetState,
    pub devices: devices::DeviceState,
    pub bluetooth: bluetooth::BtState,
    pub services: services::ServiceState,
    pub process: process::ProcState,
    pub keyboard: keyboard::KeyboardState,
    /// True when the machine is quiet enough for idle baselines to apply.
    pub idle: bool,
    /// Idle / interactive / high load. Baselines are partitioned by this so a compile
    /// is never compared against a machine sitting on a desk.
    pub activity: Activity,
    pub boot_time_ms: i64,
    pub suspended_total_ms: i64,
}

impl Snapshot {
    /// The context a sample belongs to. Baselines are partitioned by this.
    ///
    /// `time_of_day` comes from the config; when off (the default) the context is just
    /// AC/battery x idle/active.
    pub fn context_at(&self, ts_ms: i64, time_of_day: bool) -> Context {
        Context::new(self.power.on_battery, self.activity).with_time_of_day(ts_ms, time_of_day)
    }

    /// Convenience for call sites with no config to hand; never partitions by time.
    pub fn context(&self) -> Context {
        Context::new(self.power.on_battery, self.activity)
    }
}

/// Per-tick scratch space handed to each collector.
pub struct Ctx {
    pub ts_ms: i64,
    pub samples: Vec<Sample>,
    pub events: Vec<Event>,
    pub snap: Snapshot,
    pub config: Arc<Config>,
    /// Set when a resume was detected on this tick, so collectors can suppress the
    /// spurious "everything changed" burst that follows a suspend.
    pub resumed_ms: Option<i64>,
}

impl Ctx {
    pub fn new(config: Arc<Config>) -> Ctx {
        Ctx {
            ts_ms: crate::clock::now_ms(),
            samples: Vec::with_capacity(160),
            events: Vec::new(),
            snap: Snapshot::default(),
            config,
            resumed_ms: None,
        }
    }

    pub fn sample(&mut self, subsystem: &'static str, name: &'static str, instance: &str, unit: &'static str, value: f64) {
        if value.is_finite() {
            self.samples.push(Sample { id: MetricId::new(subsystem, name, instance), value, unit });
        }
    }

    pub fn g(&mut self, subsystem: &'static str, name: &'static str, unit: &'static str, value: f64) {
        self.sample(subsystem, name, "", unit, value);
    }

    pub fn event(&mut self, e: Event) {
        self.events.push(e);
    }
}

pub trait Collector: Send {
    fn name(&self) -> &'static str;
    fn tier(&self) -> Tier;
    /// Decide what this collector can actually do on this machine. Called at startup
    /// and again after a `Gone` error, so a driver reload restores coverage.
    fn probe(&mut self) -> Support;
    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()>;
    /// Handle an out-of-band event (journal line, netlink message, resume).
    fn on_event(&mut self, _ev: &ExternalEvent, _ctx: &mut Ctx) -> CResult<()> {
        Ok(())
    }
    /// A file descriptor the daemon should watch on the collector's behalf, so it can
    /// react the moment something happens instead of waiting for its tier. The
    /// daemon dispatches `ExternalEvent::CollectorReadable` when it becomes readable.
    fn event_fd(&self) -> Option<std::os::unix::io::RawFd> {
        None
    }
}

/// Something that happened outside the sampling schedule.
#[derive(Clone, Debug)]
pub enum ExternalEvent {
    /// A parsed journal entry above the configured priority.
    Journal(crate::journal::JournalEntry),
    /// rtnetlink reported a link/address/route change.
    NetlinkLink,
    /// A device was added or removed.
    Uevent { action: String, subsystem: String, devpath: String },
    /// A collector's own event fd became readable. Carries the collector name so
    /// only the owner reacts; every other collector ignores it.
    CollectorReadable { name: &'static str },
    /// The machine woke up; payload is how long it slept.
    Resumed { slept_ms: i64 },
}

struct Entry {
    c: Box<dyn Collector>,
    support: Support,
    consecutive_failures: u32,
    /// Monotonic ms of the next attempt while quarantined.
    retry_after_ms: i64,
    last_error: Option<String>,
    total_runs: u64,
    total_us: u64,
}

/// Three strikes. One transient read failure during a driver reload should not disable
/// a sensor; three in a row means it is genuinely broken.
const QUARANTINE_AFTER: u32 = 3;
/// How long a quarantined collector waits before being retried.
const QUARANTINE_RETRY_MS: i64 = 900_000;

pub struct Registry {
    entries: Vec<Entry>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry { entries: Vec::new() }
    }

    /// Register and probe. A collector disabled in config is registered anyway so it
    /// still appears on the Coverage page as `Disabled` rather than silently missing.
    pub fn add(&mut self, c: Box<dyn Collector>, cfg: &Config) {
        let mut c = c;
        let support = if !cfg.enabled(c.name()) {
            Support::Disabled
        } else {
            match catch_unwind(AssertUnwindSafe(|| c.probe())) {
                Ok(s) => s,
                Err(_) => Support::Quarantined { reason: "panicked during probe".into(), failures: 1 },
            }
        };
        crate::log_info!("collector {:<12} {:?}", c.name(), support);
        self.entries.push(Entry {
            c,
            support,
            consecutive_failures: 0,
            retry_after_ms: 0,
            last_error: None,
            total_runs: 0,
            total_us: 0,
        });
    }

    /// Run every usable collector in `tier`.
    ///
    /// The `catch_unwind` here is the load-bearing part of "a broken sensor must never
    /// crash the daemon". `panic = "unwind"` is pinned in Cargo.toml for this reason.
    pub fn run_tier(&mut self, tier: Tier, ctx: &mut Ctx, now_mono: i64) {
        for e in self.entries.iter_mut() {
            if e.c.tier() != tier {
                continue;
            }
            if matches!(e.support, Support::Disabled | Support::Unsupported { .. }) {
                continue;
            }
            if let Support::Quarantined { .. } = e.support {
                if now_mono < e.retry_after_ms {
                    continue;
                }
                // Retry window reached: re-probe rather than blindly re-running.
                let name = e.c.name();
                let re = catch_unwind(AssertUnwindSafe(|| e.c.probe()));
                match re {
                    Ok(s) if s.is_usable() => {
                        crate::log_info!("collector {name} recovered: {:?}", s);
                        e.support = s;
                        e.consecutive_failures = 0;
                    }
                    _ => {
                        e.retry_after_ms = now_mono + QUARANTINE_RETRY_MS;
                        continue;
                    }
                }
            }

            let t0 = std::time::Instant::now();
            let r = catch_unwind(AssertUnwindSafe(|| e.c.collect(ctx)));
            e.total_us += t0.elapsed().as_micros() as u64;
            e.total_runs += 1;

            match r {
                Ok(Ok(())) => {
                    e.consecutive_failures = 0;
                    e.last_error = None;
                }
                Ok(Err(err)) => {
                    let name = e.c.name();
                    e.last_error = Some(err.to_string());
                    // `Gone` means the interface vanished (driver reload, unplug).
                    // That is a coverage change, not a fault, so re-probe immediately.
                    if let CollectorError::Gone(_) = err {
                        crate::log_info!("collector {name} interface gone, re-probing: {err}");
                        let s = catch_unwind(AssertUnwindSafe(|| e.c.probe()))
                            .unwrap_or(Support::Unsupported { reason: "probe panicked".into() });
                        e.support = s;
                        e.consecutive_failures = 0;
                    } else {
                        e.consecutive_failures += 1;
                        crate::log_warn!("collector {name} failed ({}/{QUARANTINE_AFTER}): {err}", e.consecutive_failures);
                        Self::maybe_quarantine(e, now_mono);
                    }
                }
                Err(_) => {
                    let name = e.c.name();
                    e.consecutive_failures += 1;
                    e.last_error = Some("panicked".into());
                    crate::log_error!("collector {name} PANICKED ({}/{QUARANTINE_AFTER}) — isolated", e.consecutive_failures);
                    Self::maybe_quarantine(e, now_mono);
                }
            }
        }
    }

    fn maybe_quarantine(e: &mut Entry, now_mono: i64) {
        if e.consecutive_failures >= QUARANTINE_AFTER {
            let reason = e.last_error.clone().unwrap_or_else(|| "repeated failures".into());
            crate::log_error!("collector {} quarantined: {reason}", e.c.name());
            e.support = Support::Quarantined { reason, failures: e.consecutive_failures };
            e.retry_after_ms = now_mono + QUARANTINE_RETRY_MS;
        }
    }

    pub fn dispatch_event(&mut self, ev: &ExternalEvent, ctx: &mut Ctx) {
        for e in self.entries.iter_mut() {
            if !e.support.is_usable() {
                continue;
            }
            if catch_unwind(AssertUnwindSafe(|| e.c.on_event(ev, ctx))).is_err() {
                crate::log_error!("collector {} panicked handling an event — isolated", e.c.name());
            }
        }
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for e in self.entries.iter_mut() {
            if e.c.name() == name {
                e.support = if enabled {
                    e.consecutive_failures = 0;
                    catch_unwind(AssertUnwindSafe(|| e.c.probe()))
                        .unwrap_or(Support::Unsupported { reason: "probe panicked".into() })
                } else {
                    Support::Disabled
                };
                return true;
            }
        }
        false
    }

    /// Coverage report: what works, what does not, and why.
    /// Event fds collectors want watched, with the owning collector's name.
    ///
    /// Called once after registration. A collector that opens its fd later simply
    /// does not get one, and falls back to its tier.
    pub fn event_fds(&self) -> Vec<(&'static str, std::os::unix::io::RawFd)> {
        self.entries
            .iter()
            .filter(|e| !matches!(e.support, Support::Disabled | Support::Unsupported { .. }))
            .filter_map(|e| e.c.event_fd().map(|fd| (e.c.name(), fd)))
            .collect()
    }

    pub fn coverage(&self) -> Vec<serde_json::Value> {
        self.entries
            .iter()
            .map(|e| {
                let avg_us = if e.total_runs > 0 { e.total_us / e.total_runs } else { 0 };
                serde_json::json!({
                    "name": e.c.name(),
                    "tier": e.c.tier().as_str(),
                    "support": e.support,
                    "label": e.support.label(),
                    "last_error": e.last_error,
                    "runs": e.total_runs,
                    "avg_us": avg_us,
                })
            })
            .collect()
    }

    /// Per-collector cost, used by the self-measurement in `docs/PERFORMANCE.md`.
    pub fn timings(&self) -> BTreeMap<&'static str, (u64, u64)> {
        self.entries.iter().map(|e| (e.c.name(), (e.total_runs, e.total_us))).collect()
    }

    pub fn usable_count(&self) -> usize {
        self.entries.iter().filter(|e| e.support.is_usable()).count()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flaky {
        mode: &'static str,
        calls: u32,
    }

    impl Collector for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn tier(&self) -> Tier {
            Tier::Fast
        }
        fn probe(&mut self) -> Support {
            if self.mode == "unprobeable" {
                Support::Unsupported { reason: "no hardware".into() }
            } else {
                Support::Full
            }
        }
        fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
            self.calls += 1;
            match self.mode {
                "panic" => panic!("sensor exploded"),
                "err" => Err(CollectorError::BadData("nonsense".into())),
                "gone" => Err(CollectorError::Gone("/sys/class/x".into())),
                _ => {
                    ctx.g("test", "ok", "", 1.0);
                    Ok(())
                }
            }
        }
    }

    struct Panicker;
    impl Collector for Panicker {
        fn name(&self) -> &'static str { "panicker" }
        fn tier(&self) -> Tier { Tier::Fast }
        fn probe(&mut self) -> Support { panic!("probe blew up") }
        fn collect(&mut self, _c: &mut Ctx) -> CResult<()> { Ok(()) }
    }

    fn ctx() -> Ctx {
        Ctx::new(Arc::new(Config::default()))
    }

    #[test]
    fn a_panicking_collector_does_not_kill_the_run() {
        let mut r = Registry::new();
        let cfg = Config::default();
        r.add(Box::new(Flaky { mode: "panic", calls: 0 }), &cfg);
        r.add(Box::new(Flaky { mode: "ok", calls: 0 }), &cfg);
        let mut c = ctx();
        r.run_tier(Tier::Fast, &mut c, 0);
        // The healthy collector still produced its sample.
        assert_eq!(c.samples.len(), 1);
    }

    #[test]
    fn a_panicking_probe_is_survivable() {
        let mut r = Registry::new();
        r.add(Box::new(Panicker), &Config::default());
        let cov = r.coverage();
        assert_eq!(cov[0]["label"], "Quarantined");
    }

    #[test]
    fn three_strikes_quarantines_and_then_it_stops_running() {
        let mut r = Registry::new();
        r.add(Box::new(Flaky { mode: "err", calls: 0 }), &Config::default());
        let mut c = ctx();
        for _ in 0..3 {
            r.run_tier(Tier::Fast, &mut c, 0);
        }
        assert_eq!(r.coverage()[0]["label"], "Quarantined");
        assert_eq!(r.usable_count(), 0);
        let runs_before = r.coverage()[0]["runs"].as_u64().unwrap();
        // Still inside the retry window: must not run again.
        r.run_tier(Tier::Fast, &mut c, 1000);
        assert_eq!(r.coverage()[0]["runs"].as_u64().unwrap(), runs_before);
    }

    #[test]
    fn two_failures_then_success_clears_the_counter() {
        struct Twice(u32);
        impl Collector for Twice {
            fn name(&self) -> &'static str { "twice" }
            fn tier(&self) -> Tier { Tier::Fast }
            fn probe(&mut self) -> Support { Support::Full }
            fn collect(&mut self, _c: &mut Ctx) -> CResult<()> {
                self.0 += 1;
                if self.0 <= 2 { Err(CollectorError::Other("blip".into())) } else { Ok(()) }
            }
        }
        let mut r = Registry::new();
        r.add(Box::new(Twice(0)), &Config::default());
        let mut c = ctx();
        for _ in 0..5 {
            r.run_tier(Tier::Fast, &mut c, 0);
        }
        assert_eq!(r.coverage()[0]["label"], "Full", "transient blips must not quarantine");
    }

    #[test]
    fn a_vanished_interface_triggers_a_reprobe_not_a_quarantine() {
        let mut r = Registry::new();
        r.add(Box::new(Flaky { mode: "gone", calls: 0 }), &Config::default());
        let mut c = ctx();
        for _ in 0..5 {
            r.run_tier(Tier::Fast, &mut c, 0);
        }
        // probe() still returns Full for this fixture, so it stays covered and never
        // accumulates strikes: an unplug/replug cycle should restore monitoring.
        assert_eq!(r.coverage()[0]["label"], "Full");
    }

    #[test]
    fn unsupported_collectors_are_listed_but_never_run() {
        let mut r = Registry::new();
        r.add(Box::new(Flaky { mode: "unprobeable", calls: 0 }), &Config::default());
        let mut c = ctx();
        r.run_tier(Tier::Fast, &mut c, 0);
        assert_eq!(r.coverage()[0]["label"], "Unavailable");
        assert_eq!(r.coverage()[0]["runs"], 0);
        assert_eq!(r.len(), 1, "still visible on the coverage page");
    }

    #[test]
    fn disabling_in_config_is_honoured_and_reversible_at_runtime() {
        let mut cfg = Config::default();
        cfg.collectors.insert("flaky".into(), false);
        let mut r = Registry::new();
        r.add(Box::new(Flaky { mode: "ok", calls: 0 }), &cfg);
        let mut c = ctx();
        r.run_tier(Tier::Fast, &mut c, 0);
        assert!(c.samples.is_empty());
        assert_eq!(r.coverage()[0]["label"], "Disabled");

        assert!(r.set_enabled("flaky", true));
        r.run_tier(Tier::Fast, &mut c, 0);
        assert_eq!(c.samples.len(), 1);
    }

    #[test]
    fn tiers_are_isolated() {
        struct Slow;
        impl Collector for Slow {
            fn name(&self) -> &'static str { "slow" }
            fn tier(&self) -> Tier { Tier::Slow }
            fn probe(&mut self) -> Support { Support::Full }
            fn collect(&mut self, c: &mut Ctx) -> CResult<()> { c.g("test", "slow", "", 1.0); Ok(()) }
        }
        let mut r = Registry::new();
        let cfg = Config::default();
        r.add(Box::new(Flaky { mode: "ok", calls: 0 }), &cfg);
        r.add(Box::new(Slow), &cfg);
        let mut c = ctx();
        r.run_tier(Tier::Fast, &mut c, 0);
        assert_eq!(c.samples.len(), 1, "only the fast collector ran");
    }
}
