//! Integration tests for hardware and tooling that is *absent*.
//!
//! The rule this file exists to enforce: **never assume every Linux machine has NVIDIA,
//! a battery, Wi-Fi, or any particular sensor.** Each test builds a fake sysfs tree, or
//! a synthetic snapshot, describing a machine unlike the development one, and asserts
//! that JamSys degrades to "unavailable" rather than erroring, panicking, or — worst
//! of all — silently reporting a subsystem as healthy that it cannot actually see.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jamsys::anomaly::alerts::AlertManager;
use jamsys::anomaly::rules::RuleEngine;
use jamsys::clock::SuspendWatch;
use jamsys::collectors::network::NetCollector;
use jamsys::collectors::power::PowerCollector;
use jamsys::collectors::{Collector, Ctx};
use jamsys::config::{Config, Retention};
use jamsys::store::Store;
use jamsys::sysfs::{discover_hwmon_in, HwmonKind};
use jamsys::types::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("jamsys-it-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn w(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::File::create(p).unwrap().write_all(s.as_bytes()).unwrap();
}

fn ctx() -> Ctx {
    Ctx::new(Arc::new(Config::default()))
}

// ---------------------------------------------------------------------------
// Absent hardware
// ---------------------------------------------------------------------------

#[test]
fn a_desktop_with_no_battery_reports_unavailable_not_broken() {
    let d = tmp("nobat");
    // A desktop: mains only, no battery of any kind.
    w(&d.join("AC/type"), "Mains");
    w(&d.join("AC/online"), "1");

    let mut c = PowerCollector::with_root(&d);
    let s = c.probe();
    assert!(matches!(s, Support::Partial { .. }), "expected Partial, got {s:?}");
    assert!(s.is_usable(), "AC state is still monitorable without a battery");

    let mut x = ctx();
    c.collect(&mut x).expect("collect must not fail on a battery-less machine");
    assert!(!x.snap.power.has_battery);
    assert!(x.snap.power.ac_online);
    // No battery means no runtime estimate and, critically, no NaN in the database.
    assert_eq!(x.snap.power.runtime_s, None);
    assert!(x.samples.iter().all(|s| s.value.is_finite()));
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_machine_with_no_power_supplies_at_all_is_unsupported() {
    let d = tmp("nopsy");
    let mut c = PowerCollector::with_root(&d);
    let s = c.probe();
    assert!(matches!(s, Support::Unsupported { .. }), "got {s:?}");
    assert!(!s.is_usable());
    assert_eq!(s.label(), "Unavailable");
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_phantom_firmware_battery_is_not_mistaken_for_the_system_battery() {
    let d = tmp("phantom");
    // Some firmwares expose a "Battery" with no energy or charge attributes at all —
    // typically a wireless peripheral. Treating it as the system battery would report
    // 0 % charge and fire a critical alert on a desktop.
    w(&d.join("BAT9/type"), "Battery");
    w(&d.join("BAT9/present"), "0");
    w(&d.join("AC/type"), "Mains");
    w(&d.join("AC/online"), "1");

    let mut c = PowerCollector::with_root(&d);
    c.probe();
    let mut x = ctx();
    c.collect(&mut x).unwrap();
    assert!(!x.snap.power.has_battery, "a battery with no present flag must be ignored");
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_charge_domain_battery_is_converted_correctly() {
    let d = tmp("chargedomain");
    // Many ThinkPads and most phones report µAh/µA instead of µWh/µW.
    w(&d.join("BAT0/type"), "Battery");
    w(&d.join("BAT0/present"), "1");
    w(&d.join("BAT0/status"), "Discharging");
    w(&d.join("BAT0/capacity"), "75");
    w(&d.join("BAT0/voltage_now"), "11400000");   // 11.4 V
    w(&d.join("BAT0/charge_now"), "3000000");     // 3.0 Ah
    w(&d.join("BAT0/charge_full"), "4000000");    // 4.0 Ah
    w(&d.join("BAT0/charge_full_design"), "5000000"); // 5.0 Ah
    w(&d.join("BAT0/current_now"), "1000000");    // 1.0 A

    let mut c = PowerCollector::with_root(&d);
    assert!(c.probe().is_usable());
    let mut x = ctx();
    c.collect(&mut x).unwrap();
    let p = &x.snap.power;
    assert!(p.has_battery);
    assert!((p.energy_now_wh - 34.2).abs() < 0.1, "3.0 Ah x 11.4 V = 34.2 Wh, got {}", p.energy_now_wh);
    assert!((p.power_w - 11.4).abs() < 0.1, "1.0 A x 11.4 V = 11.4 W, got {}", p.power_w);
    assert!((p.health_pct - 80.0).abs() < 0.5, "4.0/5.0 Ah = 80%, got {}", p.health_pct);
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_machine_with_no_wifi_is_partial_not_failed() {
    let d = tmp("nowifi");
    // Ethernet only, as on a desktop or a server.
    w(&d.join("eth0/operstate"), "up");
    w(&d.join("eth0/carrier"), "1");
    w(&d.join("eth0/address"), "aa:bb:cc:dd:ee:ff");
    w(&d.join("eth0/device/uevent"), "DRIVER=e1000e");
    w(&d.join("lo/operstate"), "unknown");
    w(&d.join("lo/carrier"), "1");

    let mut c = NetCollector::with_root(d.to_str().unwrap());
    let s = c.probe();
    assert!(matches!(s, Support::Partial { .. }), "expected Partial, got {s:?}");
    assert!(s.is_usable(), "ethernet is still fully monitorable");
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_machine_with_only_loopback_is_reported_honestly() {
    let d = tmp("looponly");
    w(&d.join("lo/operstate"), "unknown");
    let mut c = NetCollector::with_root(d.to_str().unwrap());
    let s = c.probe();
    assert!(matches!(s, Support::Partial { .. }), "got {s:?}");
    fs::remove_dir_all(&d).ok();
}

#[test]
fn no_hwmon_sensors_at_all_yields_no_channels_and_no_panic() {
    let d = tmp("nohwmon");
    assert!(discover_hwmon_in(d.to_str().unwrap(), &[HwmonKind::Temp]).is_empty());
    assert!(discover_hwmon_in("/nonexistent/path", &[HwmonKind::Fan]).is_empty());
    fs::remove_dir_all(&d).ok();
}

#[test]
fn nvidia_absent_is_a_normal_state_for_every_rule() {
    // A machine with no discrete GPU must produce no GPU alerts, ever.
    let cfg = Config::default();
    let mut e = RuleEngine::new(&cfg);
    let mut s = jamsys::collectors::Snapshot::default();
    s.memory.total_bytes = 16 << 30;
    s.memory.available_pct = 60.0;
    // gpu.nvidia.present stays false.
    e.evaluate(&s, &cfg, 0);
    let alerts = e.evaluate(&s, &cfg, 3_600_000).alerts;
    assert!(
        !alerts.iter().any(|a| a.rule_id.starts_with("gpu.")),
        "GPU rules fired without a GPU: {:?}",
        alerts.iter().map(|a| &a.rule_id).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Bad data
// ---------------------------------------------------------------------------

#[test]
fn invalid_sysfs_values_never_reach_the_database() {
    let d = tmp("badsysfs");
    w(&d.join("hwmon0/name"), "coretemp");
    w(&d.join("hwmon0/temp1_input"), "-274000");   // below absolute zero
    w(&d.join("hwmon0/temp1_label"), "Package id 0");
    w(&d.join("hwmon0/temp2_input"), "");          // empty
    w(&d.join("hwmon0/temp2_label"), "Core 0");
    w(&d.join("hwmon0/temp3_input"), "not-a-number");
    w(&d.join("hwmon0/temp3_label"), "Core 1");
    w(&d.join("hwmon0/temp4_input"), "9223372036854775807"); // i64::MAX
    w(&d.join("hwmon0/temp4_label"), "Core 2");
    w(&d.join("hwmon0/temp5_input"), "58000");     // the only good one
    w(&d.join("hwmon0/temp5_label"), "Core 3");

    let chans = discover_hwmon_in(d.to_str().unwrap(), &[HwmonKind::Temp]);
    assert_eq!(chans.len(), 5, "all five channels should be discovered");
    let readable: Vec<_> = chans.iter().filter_map(|c| c.read().map(|v| (c.label.clone(), v))).collect();
    assert_eq!(readable.len(), 1, "only the plausible reading may survive: {readable:?}");
    assert_eq!(readable[0].1, 58.0);
    fs::remove_dir_all(&d).ok();
}

#[test]
fn a_sensor_that_vanishes_mid_run_is_not_a_crash() {
    let d = tmp("vanish");
    w(&d.join("hwmon0/name"), "asus");
    w(&d.join("hwmon0/fan1_input"), "2300");
    w(&d.join("hwmon0/fan1_label"), "cpu_fan");
    let chans = discover_hwmon_in(d.to_str().unwrap(), &[HwmonKind::Fan]);
    assert_eq!(chans[0].read(), Some(2300.0));
    // Simulate a driver unload between discovery and the next read.
    fs::remove_dir_all(&d).unwrap();
    assert_eq!(chans[0].read(), None, "a vanished sensor reads as absent");
}

#[test]
fn a_disappeared_collector_is_retried_without_restarting_monitoring() {
    use jamsys::collectors::Registry;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Removable {
        present: Arc<AtomicBool>,
        probes: Arc<AtomicUsize>,
    }
    impl Collector for Removable {
        fn name(&self) -> &'static str { "removable" }
        fn tier(&self) -> Tier { Tier::Medium }
        fn probe(&mut self) -> Support {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.present.load(Ordering::SeqCst) {
                Support::Full
            } else {
                Support::Unsupported { reason: "driver unloaded".into() }
            }
        }
        fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
            if !self.present.load(Ordering::SeqCst) {
                return Err(CollectorError::Gone("driver unloaded".into()));
            }
            ctx.g("thermal", "cpu_package_c", "C", 55.0);
            Ok(())
        }
    }

    let present = Arc::new(AtomicBool::new(true));
    let probes = Arc::new(AtomicUsize::new(0));
    let cfg = Config::default();
    let mut registry = Registry::new();
    registry.add(Box::new(Removable { present: present.clone(), probes: probes.clone() }), &cfg);
    let mut x = ctx();
    registry.run_tier(Tier::Medium, &mut x, 0);
    assert_eq!(x.samples.len(), 1);

    present.store(false, Ordering::SeqCst);
    registry.run_tier(Tier::Medium, &mut x, 10_000);
    assert_eq!(registry.coverage()[0]["label"], "Unavailable");
    let after_loss = probes.load(Ordering::SeqCst);
    for t in [20_000, 30_000, 900_000] {
        registry.run_tier(Tier::Medium, &mut x, t);
    }
    assert_eq!(probes.load(Ordering::SeqCst), after_loss, "do not hot-loop on absent hardware");
    registry.run_tier(Tier::Medium, &mut x, 910_000);
    assert_eq!(probes.load(Ordering::SeqCst), after_loss + 1, "retry after fifteen minutes");
    present.store(true, Ordering::SeqCst);
    registry.run_tier(Tier::Medium, &mut x, 1_810_000);
    assert_eq!(registry.coverage()[0]["label"], "Full");
    assert_eq!(x.samples.len(), 2, "collection resumes after driver returns");
    registry.set_enabled("removable", false);
    registry.run_tier(Tier::Medium, &mut x, 2_710_000);
    assert_eq!(x.samples.len(), 2, "explicitly disabled collectors stay disabled");
}

#[test]
fn re_enabling_while_the_hardware_is_still_absent_keeps_the_retry() {
    // The obvious recovery gesture. A driver unloads, the Coverage page says
    // "Unavailable", and the user toggles the collector off and on to kick it.
    // That used to zero the retry deadline and drop the collector into the
    // "never usable, never retry" state, stranding it until a daemon restart --
    // the opposite of what the gesture was meant to achieve.
    use jamsys::collectors::Registry;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Absent { present: Arc<AtomicBool> }
    impl Collector for Absent {
        fn name(&self) -> &'static str { "absent" }
        fn tier(&self) -> Tier { Tier::Medium }
        fn probe(&mut self) -> Support {
            if self.present.load(Ordering::SeqCst) { Support::Full }
            else { Support::Unsupported { reason: "driver unloaded".into() } }
        }
        fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
            if !self.present.load(Ordering::SeqCst) {
                return Err(CollectorError::Gone("driver unloaded".into()));
            }
            ctx.g("test", "v", "", 1.0);
            Ok(())
        }
    }

    let present = Arc::new(AtomicBool::new(true));
    let cfg = Config::default();
    let mut r = Registry::new();
    r.add(Box::new(Absent { present: present.clone() }), &cfg);
    let mut x = ctx();
    r.run_tier(Tier::Medium, &mut x, 0);

    present.store(false, Ordering::SeqCst);
    r.run_tier(Tier::Medium, &mut x, 10_000);
    assert_eq!(r.coverage()[0]["label"], "Unavailable");

    // The user toggles it off and back on while the driver is still missing.
    r.set_enabled("absent", false);
    r.set_enabled("absent", true);

    // The driver returns. Without the fix this tick does nothing, for ever.
    present.store(true, Ordering::SeqCst);
    let before = x.samples.len();
    r.run_tier(Tier::Medium, &mut x, 10_000 + 900_001);
    assert_eq!(r.coverage()[0]["label"], "Full",
               "toggling a collector must not cancel its retry");
    assert_eq!(x.samples.len(), before + 1, "and monitoring must actually resume");
}

#[test]
fn a_collector_that_can_read_nothing_does_not_report_itself_as_covered() {
    // A probe that returns Partial while collect() can produce nothing is the
    // worst failure this crate has: the Coverage page says the subsystem is
    // watched, no data is ever stored, and because Partial counts as usable the
    // registry schedules no retry, so it can never recover either.
    use jamsys::collectors::Registry;

    struct EmptyPartial;
    impl Collector for EmptyPartial {
        fn name(&self) -> &'static str { "emptypartial" }
        fn tier(&self) -> Tier { Tier::Medium }
        fn probe(&mut self) -> Support {
            Support::Partial { detail: "advertises coverage it cannot deliver".into() }
        }
        fn collect(&mut self, _ctx: &mut Ctx) -> CResult<()> {
            Err(CollectorError::Gone("nothing readable".into()))
        }
    }

    let mut r = Registry::new();
    r.add(Box::new(EmptyPartial), &Config::default());
    let mut x = ctx();
    for t in 0..6 {
        r.run_tier(Tier::Medium, &mut x, t);
    }
    assert!(x.samples.is_empty(), "it produced nothing, as expected");
    assert_ne!(r.coverage()[0]["label"], "Full",
               "a source with no readings must never read as fully covered");
    let runs = r.coverage()[0]["runs"].as_u64().unwrap_or(0);
    for t in 6..60 {
        r.run_tier(Tier::Medium, &mut x, t);
    }
    assert_eq!(r.coverage()[0]["runs"].as_u64().unwrap_or(0), runs,
               "and it must back off rather than retry on every tick for ever");
}

// ---------------------------------------------------------------------------
// Suspend and resume
// ---------------------------------------------------------------------------

#[test]
fn suspend_and_resume_are_measured_from_the_clock_difference() {
    let mut w = SuspendWatch::with_state(0, 0, 5_000);
    // Ordinary ticks.
    assert!(w.check(2_000, 2_000, 0).is_none());
    assert!(w.check(4_000, 4_000, 0).is_none());
    // Eight hours asleep: monotonic advanced 2 s, boottime advanced 8 h 2 s.
    let ev = w.check(6_000, 4_000 + 8 * 3_600_000 + 2_000, 1_700_000_000_000).unwrap();
    assert_eq!(ev.slept_ms, 8 * 3_600_000);
    // The very next tick must be quiet.
    assert!(w.check(8_000, 4_000 + 8 * 3_600_000 + 4_000, 0).is_none());
}

#[test]
fn overnight_battery_drain_while_suspended_is_detectable() {
    // 8 hours, 95% -> 71% = 3%/h, above the 2%/h default.
    let hours = 8.0;
    let rate = (95.0 - 71.0) / hours;
    assert!(rate > 2.0);
    // A healthy machine: 95% -> 92% over the same period = 0.375%/h.
    assert!((95.0 - 92.0) / hours < 2.0);
}

// ---------------------------------------------------------------------------
// Whole-pipeline behaviour
// ---------------------------------------------------------------------------

#[test]
fn the_full_pipeline_fires_dedups_suppresses_and_resolves() {
    let store = Store::open_memory().unwrap();
    let mut cfg = Config::default();
    cfg.alerts.desktop_notifications = false;
    let mut engine = RuleEngine::new(&cfg);
    let mut mgr = AlertManager::new(&cfg, &store);

    let mut s = jamsys::collectors::Snapshot::default();
    s.memory.total_bytes = 32 << 30;
    s.memory.available_pct = 70.0;
    s.storage.filesystems = vec![jamsys::collectors::storage::FsInfo {
        mount: "/".into(), device: "/dev/nvme0n1p7".into(), fstype: "ext4".into(),
        total_bytes: 300 << 30, free_bytes: 3 << 30, used_pct: 99.0,
        inode_used_pct: 10.0, read_only: false,
    }];

    // Fire.
    let ev = engine.evaluate(&s, &cfg, 0);
    let full: Vec<_> = ev.alerts.into_iter().filter(|a| a.rule_id == "disk.full").collect();
    assert_eq!(full.len(), 1);
    assert!(mgr.raise(full[0].clone(), &store, &cfg) || !cfg.alerts.desktop_notifications);
    assert!(mgr.is_open("disk.full:/"));

    // Re-fire many times: one row, no duplicate.
    for t in 1..30 {
        let e2 = engine.evaluate(&s, &cfg, t * 2_000);
        for a in e2.alerts.into_iter().filter(|a| a.rule_id == "disk.full") {
            mgr.raise(a, &store, &cfg);
        }
    }
    assert_eq!(store.row_count("alert"), 1, "deduplication failed");

    // Suppress it, then confirm a later occurrence is still recorded but not notified.
    store.add_suppression("rule", "disk.full", None, Some("test")).unwrap();
    mgr.reload_suppressions(&store);
    let a = Alert::new("disk.full", "/", Severity::Critical, "x", Explanation::new("y"));
    assert!(!mgr.raise(a, &store, &cfg), "a suppressed alert must not notify");

    // Recover, and it resolves after hysteresis.
    s.storage.filesystems[0].used_pct = 40.0;
    let ev3 = engine.evaluate(&s, &cfg, 100_000);
    assert!(!ev3.alerts.iter().any(|a| a.rule_id == "disk.full"));
    mgr.clear_now("disk.full:/", &store);
    assert!(!mgr.is_open("disk.full:/"));
    let rows = store.alerts(false, 10).unwrap();
    assert!(rows.iter().any(|r| r["resolved_ts"].is_i64()));
}

#[test]
fn retention_and_rollups_keep_the_database_bounded() {
    let mut store = Store::open_memory().unwrap();
    let m = MetricId::global("power", "system_w");
    let now = jamsys::clock::now_ms();
    // Three days of one-minute samples.
    for i in 0..(3 * 24 * 60) {
        store.push(now - (3 * 24 * 60 - i) as i64 * 60_000, &m, "W", 10.0 + (i % 7) as f64);
    }
    store.flush().unwrap();
    let before = store.row_count("sample");
    assert_eq!(before, 3 * 24 * 60);

    store.build_rollups(60_000).unwrap();
    assert!(store.row_count("rollup") > 0, "no rollups were produced");

    store.enforce_retention(&Retention { raw_hours: 24, ..Default::default() }).unwrap();
    let after = store.row_count("sample");
    assert!(after < before, "retention deleted nothing");
    assert!(after <= 24 * 60 + 5, "kept {after} rows for a 24-hour window");

    // History still answers over the full span, from the rollups.
    let h = store.history(&m, now - 3 * 86_400_000, now, 100).unwrap();
    assert!(!h["points"].as_array().unwrap().is_empty(),
            "history must survive retention by falling back to rollups");
}

#[test]
fn an_alert_can_never_be_constructed_without_an_explanation() {
    // The type system is the enforcement mechanism for "never just say anomaly
    // detected". This test documents that intent and fails if the API is loosened.
    let a = Alert::new("x.y", "inst", Severity::Warning, "title",
                       Explanation::new("measured 42 units").expected("below 10"));
    let rendered = a.explanation.render();
    assert!(rendered.contains("42"));
    assert!(rendered.contains("Expected"));
    assert_eq!(a.fingerprint, "x.y:inst");
}

#[test]
fn a_daemon_restart_does_not_duplicate_or_re_notify_open_alerts() {
    let store = Store::open_memory().unwrap();
    let mut cfg = Config::default();
    cfg.alerts.desktop_notifications = false;
    {
        let mut mgr = AlertManager::new(&cfg, &store);
        mgr.raise(Alert::new("cpu.temp.critical", "", Severity::Critical, "hot",
                             Explanation::new("97 C")), &store, &cfg);
    }
    let mgr2 = AlertManager::new(&cfg, &store);
    assert_eq!(mgr2.open_count(), 1);
    assert!(mgr2.is_open("cpu.temp.critical"));
    assert_eq!(store.row_count("alert"), 1);
}
