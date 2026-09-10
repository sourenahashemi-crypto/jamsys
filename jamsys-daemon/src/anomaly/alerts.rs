//! Alert lifecycle: suppression, rate limiting, deduplication, notification, resolution.
//!
//! Deduplication happens twice by design — once here in memory, to avoid pointless
//! work and repeat notifications, and once in SQLite via a partial unique index, so a
//! daemon restart cannot resurrect a duplicate open alert.

use crate::config::Config;
use crate::store::{Store, Suppression};
use crate::types::*;
use std::collections::HashMap;

/// Per-severity token bucket. A flapping sensor must not be able to produce hundreds
/// of desktop notifications, and the suppression must be *visible* rather than silent.
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_s: f64,
    last_ms: i64,
    suppressed: u64,
}

impl Bucket {
    fn new(capacity: u32, refill_s: i64, now: i64) -> Self {
        Bucket {
            tokens: capacity as f64,
            capacity: capacity as f64,
            refill_per_s: if refill_s > 0 { 1.0 / refill_s as f64 } else { 1.0 },
            last_ms: now,
            suppressed: 0,
        }
    }
    fn take(&mut self, now: i64) -> bool {
        let dt = ((now - self.last_ms).max(0)) as f64 / 1000.0;
        self.tokens = (self.tokens + dt * self.refill_per_s).min(self.capacity);
        self.last_ms = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.suppressed += 1;
            false
        }
    }
}

pub struct AlertManager {
    buckets: HashMap<Severity, Bucket>,
    /// fingerprint -> (last notify monotonic ms, notification id, severity notified at)
    notified: HashMap<String, (i64, u32, Severity)>,
    /// Fingerprints currently believed to be firing.
    open: HashMap<String, Severity>,
    /// Fingerprint -> monotonic ms when the condition first went false, for hysteresis.
    clearing: HashMap<String, i64>,
    suppressions: Vec<Suppression>,
    notifier: Option<crate::dbus::Connection>,
    pub notify_failures: u32,
    pub total_raised: u64,
    pub total_suppressed: u64,
}

/// Resolution hysteresis. Without it, a metric oscillating around a threshold produces
/// an endless fire/resolve/fire stream in the event log.
const MIN_RESOLVE_HOLD_S: i64 = 120;

impl AlertManager {
    pub fn new(cfg: &Config, store: &Store) -> Self {
        let now = crate::clock::mono_ms();
        let mut buckets = HashMap::new();
        for s in [Severity::Info, Severity::Notice, Severity::Warning, Severity::Critical] {
            buckets.insert(s, Bucket::new(cfg.alerts.burst, cfg.alerts.refill_s, now));
        }
        let mut m = AlertManager {
            buckets,
            notified: HashMap::new(),
            open: HashMap::new(),
            clearing: HashMap::new(),
            suppressions: store.load_suppressions().unwrap_or_default(),
            notifier: None,
            notify_failures: 0,
            total_raised: 0,
            total_suppressed: 0,
        };
        // Reload open alerts so a restart neither re-notifies nor loses state.
        if let Ok(rows) = store.alerts(true, 500) {
            for r in rows {
                if let (Some(fp), Some(sev)) = (r["fingerprint"].as_str(), r["severity"].as_i64()) {
                    m.open.insert(fp.to_string(), Severity::from_i64(sev));
                    // Treat as already notified so a restart is quiet -- but carry
                    // the real notification id across, or the toast still on screen
                    // can never be replaced or withdrawn. Critical notifications are
                    // sent with timeout 0, so "cannot withdraw" means "on the
                    // desktop for ever".
                    let nid = r["notify_id"].as_i64().unwrap_or(0).clamp(0, u32::MAX as i64) as u32;
                    m.notified.insert(fp.to_string(), (now, nid, Severity::from_i64(sev)));
                }
            }
        }
        m
    }

    pub fn reload_suppressions(&mut self, store: &Store) {
        self.suppressions = store.load_suppressions().unwrap_or_default();
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    pub fn open_fingerprints(&self) -> Vec<String> {
        self.open.keys().cloned().collect()
    }

    /// Is this alert muted, snoozed, or permanently ignored?
    pub fn is_suppressed(&self, a: &Alert, now: i64) -> bool {
        let instance = a.fingerprint.strip_prefix(&format!("{}:", a.rule_id)).unwrap_or("");
        self.suppressions
            .iter()
            .any(|s| s.active(now) && s.covers(&a.rule_id, instance, &a.fingerprint))
    }

    /// Raise an alert. Returns whether a desktop notification was actually sent.
    pub fn raise(&mut self, a: Alert, store: &Store, cfg: &Config) -> bool {
        let now = crate::clock::mono_ms();
        self.clearing.remove(&a.fingerprint);

        // Suppressions persist as Unix timestamps; cooldown and refill intervals
        // must remain independent of NTP or manual wall-clock changes.
        if self.is_suppressed(&a, crate::clock::now_ms()) {
            self.total_suppressed += 1;
            // Still recorded, so the UI can show "muted" rather than hiding the fact.
            let _ = store.upsert_alert(&a);
            self.open.insert(a.fingerprint.clone(), a.severity);
            return false;
        }

        let (_, is_new) = match store.upsert_alert(&a) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("failed to persist alert {}: {e}", a.fingerprint);
                (0, false)
            }
        };
        self.open.insert(a.fingerprint.clone(), a.severity);
        if is_new {
            self.total_raised += 1;
        }

        // Cooldown: a re-fire inside the window never re-notifies.
        if let Some((last, _, sev)) = self.notified.get(&a.fingerprint) {
            let escalated = a.severity > *sev;
            if !escalated && now - last < a.severity.cooldown_s() * 1000 {
                return false;
            }
            if !escalated && !is_new {
                return false;
            }
        }

        if !cfg.alerts.desktop_notifications || a.severity < cfg.min_notify() {
            self.notified.insert(a.fingerprint.clone(), (now, 0, a.severity));
            return false;
        }

        // Rate limit last, so suppression counts reflect what the user would have seen.
        let allowed = self.buckets.get_mut(&a.severity).map(|b| b.take(now)).unwrap_or(true);
        if !allowed {
            self.total_suppressed += 1;
            crate::log_warn!("notification rate limit hit for {}", a.severity);
            self.notified.insert(a.fingerprint.clone(), (now, 0, a.severity));
            return false;
        }

        let replaces = self.notified.get(&a.fingerprint).map(|(_, id, _)| *id).unwrap_or(0);
        let id = self.send_notification(&a, replaces);
        self.notified.insert(a.fingerprint.clone(), (now, id, a.severity));
        // Persist it so a restart can still take this notification down.
        let _ = store.set_notify_id(&a.fingerprint, id);
        true
    }

    /// Tell the manager a condition is no longer true. Resolution is delayed by
    /// hysteresis so an oscillating metric does not spam the timeline.
    pub fn clear(&mut self, fingerprint: &str, store: &Store) {
        if !self.open.contains_key(fingerprint) {
            return;
        }
        let now = crate::clock::mono_ms();
        let since = *self.clearing.entry(fingerprint.to_string()).or_insert(now);
        if (now - since) / 1000 < MIN_RESOLVE_HOLD_S {
            return;
        }
        self.open.remove(fingerprint);
        self.clearing.remove(fingerprint);
        self.withdraw(fingerprint);
        let _ = store.resolve_alert(fingerprint);
    }

    /// Immediate resolution, for conditions that are genuinely edge-triggered
    /// (an interface coming back up, a unit recovering).
    pub fn clear_now(&mut self, fingerprint: &str, store: &Store) {
        if self.open.remove(fingerprint).is_some() {
            self.clearing.remove(fingerprint);
            self.withdraw(fingerprint);
            let _ = store.resolve_alert(fingerprint);
        }
    }

    pub fn is_open(&self, fingerprint: &str) -> bool {
        self.open.contains_key(fingerprint)
    }

    pub fn suppressed_counts(&self) -> u64 {
        self.buckets.values().map(|b| b.suppressed).sum::<u64>() + self.total_suppressed
    }

    /// Forget an alert's notification *and* take it off the screen.
    ///
    /// Dropping the id on its own was the bug: the daemon stopped tracking the
    /// notification while the shell went on showing it, so a resolved problem left
    /// a warning behind that nothing would ever clear.
    fn withdraw(&mut self, fingerprint: &str) {
        let Some((_, id, _)) = self.notified.remove(fingerprint) else { return };
        if id == 0 {
            return;
        }
        if self.notifier.is_none() {
            self.notifier = crate::dbus::Connection::session().ok();
        }
        if let Some(conn) = self.notifier.as_mut() {
            if let Err(e) = crate::dbus::close_notification(conn, id) {
                crate::log_warn!("could not withdraw notification {id}: {e}");
                self.notifier = None;
            }
        }
    }

    fn send_notification(&mut self, a: &Alert, replaces: u32) -> u32 {
        if self.notifier.is_none() {
            self.notifier = crate::dbus::Connection::session().ok();
        }
        let Some(conn) = self.notifier.as_mut() else {
            self.notify_failures += 1;
            return 0;
        };
        let (urgency, timeout, icon) = match a.severity {
            Severity::Critical => (2u8, 0i32, "dialog-error"),   // 0 = never expires
            Severity::Warning => (1, 20_000, "dialog-warning"),
            _ => (0, 10_000, "dialog-information"),
        };
        // Two lines of body, always with real numbers — never "anomaly detected".
        let body = {
            let e = &a.explanation;
            let mut s = e.what.clone();
            if let Some(x) = &e.expected {
                s.push_str(&format!("\nExpected: {x}"));
            } else if let Some(c) = &e.likely_cause {
                s.push_str(&format!("\n{c}"));
            }
            s
        };
        match crate::dbus::notify(conn, "JamSys", replaces, icon, &a.title, &body, urgency, timeout) {
            Ok(id) => id,
            Err(e) => {
                crate::log_warn!("desktop notification failed: {e}");
                self.notify_failures += 1;
                // Drop the connection so the next attempt reconnects; the session bus
                // goes away when the desktop session restarts.
                self.notifier = None;
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(rule: &str, inst: &str, sev: Severity) -> Alert {
        Alert::new(rule, inst, sev, "T", Explanation::new("something measurable"))
    }

    fn setup() -> (Store, Config) {
        let s = Store::open_memory().unwrap();
        let mut c = Config::default();
        c.alerts.desktop_notifications = false; // no bus in tests
        (s, c)
    }

    #[test]
    fn notification_intervals_use_monotonic_time() {
        let (store, cfg) = setup();
        let before = crate::clock::mono_ms();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("cpu.hot", "", Severity::Warning), &store, &cfg);
        let after = crate::clock::mono_ms();
        let notified = m.notified["cpu.hot"].0;
        assert!((before..=after).contains(&notified), "notification timestamp {notified} is not monotonic");
        for bucket in m.buckets.values() {
            assert!((before..=after).contains(&bucket.last_ms), "refill clock must be monotonic");
        }
    }

    #[test]
    fn resolution_finishes_after_monotonic_hold() {
        let (store, cfg) = setup();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("cpu.hot", "", Severity::Warning), &store, &cfg);
        m.clear("cpu.hot", &store);
        let since = m.clearing["cpu.hot"];
        assert!((crate::clock::mono_ms() - since).abs() < 1_000,
                "resolution hold started on the wall clock");
        m.clearing.insert("cpu.hot".into(), crate::clock::mono_ms() - 119_000);
        m.clear("cpu.hot", &store);
        assert!(m.is_open("cpu.hot"), "must hold for the full two minutes");
        m.clearing.insert("cpu.hot".into(), crate::clock::mono_ms() - 120_000);
        m.clear("cpu.hot", &store);
        assert!(!m.is_open("cpu.hot"));
    }

    #[test]
    fn an_active_snooze_uses_wall_time_even_when_intervals_are_monotonic() {
        let (store, cfg) = setup();
        store.add_suppression("rule", "cpu.hot", Some(crate::clock::now_ms() + 60_000), None).unwrap();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("cpu.hot", "", Severity::Warning), &store, &cfg);
        assert_eq!(m.total_suppressed, 1);
        assert!(!m.notified.contains_key("cpu.hot"));
    }

    #[test]
    fn a_notification_survives_a_restart_and_can_still_be_withdrawn() {
        // Critical notifications are sent with timeout 0, meaning never expires.
        // A restart used to reload open alerts with notification id 0, so the
        // first re-notify created a second toast instead of replacing the first,
        // and withdraw() bailed on id == 0 and left the original on the desktop
        // for ever -- defeating the withdrawal it exists to perform.
        let (store, cfg) = setup();
        let a = alert("cpu.temp.critical", "", Severity::Critical);
        {
            let mut m = AlertManager::new(&cfg, &store);
            m.raise(a.clone(), &store, &cfg);
            // Simulate the shell having handed back a real id.
            m.notified.insert(a.fingerprint.clone(), (crate::clock::mono_ms(), 4242,
                                                      Severity::Critical));
            store.set_notify_id(&a.fingerprint, 4242).unwrap();
        }

        let m2 = AlertManager::new(&cfg, &store);
        let (_, id, _) = m2.notified[&a.fingerprint];
        assert_eq!(id, 4242, "the id must survive the restart, or nothing can be withdrawn");
    }

    #[test]
    fn a_snooze_that_has_expired_lets_the_alert_through() {
        // The mirror of the active-snooze test, and the case that actually
        // breaks if the suppression check is moved onto the monotonic clock: a
        // past wall-clock deadline compared against monotonic time looks like it
        // is still in the future, so the alert would stay muted for ever.
        let (store, cfg) = setup();
        store.add_suppression("rule", "cpu.hot", Some(crate::clock::now_ms() - 60_000), None)
            .unwrap();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("cpu.hot", "", Severity::Warning), &store, &cfg);
        assert_eq!(m.total_suppressed, 0, "an expired snooze must not still suppress");
        assert!(m.notified.contains_key("cpu.hot"), "and the alert must be notified");
    }

    #[test]
    fn an_alert_is_persisted_and_deduplicated() {
        let (store, cfg) = setup();
        let mut m = AlertManager::new(&cfg, &store);
        let a = alert("disk.full", "/home", Severity::Critical);
        m.raise(a.clone(), &store, &cfg);
        m.raise(a.clone(), &store, &cfg);
        m.raise(a, &store, &cfg);
        assert_eq!(store.row_count("alert"), 1, "one row per open fingerprint");
        assert_eq!(m.open_count(), 1);
        assert_eq!(m.total_raised, 1, "re-fires are not new alerts");
    }

    #[test]
    fn different_instances_are_different_alerts() {
        let (store, cfg) = setup();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("disk.full", "/home", Severity::Warning), &store, &cfg);
        m.raise(alert("disk.full", "/boot", Severity::Warning), &store, &cfg);
        assert_eq!(store.row_count("alert"), 2);
        assert_eq!(m.open_count(), 2);
    }

    #[test]
    fn muting_a_rule_stops_notification_but_still_records() {
        let (store, cfg) = setup();
        store.add_suppression("rule", "service.failed", None, Some("noisy")).unwrap();
        let mut m = AlertManager::new(&cfg, &store);
        let a = alert("service.failed", "snap.openshell.gateway.service", Severity::Warning);
        assert!(m.is_suppressed(&a, crate::clock::now_ms()));
        assert!(!m.raise(a, &store, &cfg), "must not notify");
        assert_eq!(store.row_count("alert"), 1, "but the fact is still recorded");
    }

    #[test]
    fn ignoring_a_specific_service_leaves_others_alone() {
        let (store, cfg) = setup();
        store.add_suppression("instance", "snap.*.service", None, None).unwrap();
        let m = AlertManager::new(&cfg, &store);
        let now = crate::clock::now_ms();
        assert!(m.is_suppressed(&alert("service.failed", "snap.openshell.gateway.service", Severity::Warning), now));
        assert!(!m.is_suppressed(&alert("service.failed", "bluetooth.service", Severity::Warning), now));
    }

    #[test]
    fn a_snooze_expires() {
        let (store, cfg) = setup();
        let now = crate::clock::now_ms();
        store.add_suppression("rule", "cpu.temp.high", Some(now - 1000), None).unwrap();
        let m = AlertManager::new(&cfg, &store);
        assert!(!m.is_suppressed(&alert("cpu.temp.high", "", Severity::Warning), now),
                "an expired snooze must not keep suppressing");
    }

    #[test]
    fn resolution_requires_hysteresis() {
        let (store, cfg) = setup();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("net.iface_down", "wlp108s0", Severity::Warning), &store, &cfg);
        m.clear("net.iface_down:wlp108s0", &store);
        assert!(m.is_open("net.iface_down:wlp108s0"), "must not resolve instantly");
        // Force the hold to have elapsed.
        m.clearing.insert("net.iface_down:wlp108s0".into(), crate::clock::mono_ms() - 130_000);
        m.clear("net.iface_down:wlp108s0", &store);
        assert!(!m.is_open("net.iface_down:wlp108s0"));
    }

    #[test]
    fn edge_triggered_conditions_can_resolve_immediately() {
        let (store, cfg) = setup();
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("service.failed", "x.service", Severity::Warning), &store, &cfg);
        m.clear_now("service.failed:x.service", &store);
        assert!(!m.is_open("service.failed:x.service"));
        let rows = store.alerts(false, 10).unwrap();
        assert!(rows[0]["resolved_ts"].is_i64());
    }

    #[test]
    fn a_restart_does_not_re_notify_open_alerts() {
        let (store, cfg) = setup();
        {
            let mut m = AlertManager::new(&cfg, &store);
            m.raise(alert("cpu.temp.critical", "", Severity::Critical), &store, &cfg);
        }
        // Fresh manager over the same store, as after a daemon restart.
        let m2 = AlertManager::new(&cfg, &store);
        assert_eq!(m2.open_count(), 1, "open alerts must be reloaded");
        assert!(m2.notified.contains_key("cpu.temp.critical"), "and considered already notified");
    }

    #[test]
    fn the_token_bucket_limits_a_flapping_source() {
        let now = crate::clock::now_ms();
        let mut b = Bucket::new(3, 60, now);
        assert!(b.take(now));
        assert!(b.take(now));
        assert!(b.take(now));
        assert!(!b.take(now), "bucket should be empty");
        assert_eq!(b.suppressed, 1);
        // One token refills after 60 s.
        assert!(b.take(now + 60_000));
        assert!(!b.take(now + 60_000));
    }

    #[test]
    fn the_bucket_cannot_over_refill() {
        let now = crate::clock::now_ms();
        let mut b = Bucket::new(2, 1, now);
        // A very long idle period must not create unlimited credit.
        for _ in 0..2 {
            assert!(b.take(now + 3_600_000));
        }
        assert!(!b.take(now + 3_600_000));
    }

    #[test]
    fn escalation_notifies_again_even_inside_the_cooldown() {
        let (store, mut cfg) = setup();
        cfg.alerts.desktop_notifications = true; // exercise the path; no bus => no send
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("cpu.temp", "", Severity::Warning), &store, &cfg);
        let before = m.notified.get("cpu.temp").map(|(_, _, s)| *s);
        assert_eq!(before, Some(Severity::Warning));
        m.raise(alert("cpu.temp", "", Severity::Critical), &store, &cfg);
        assert_eq!(m.notified.get("cpu.temp").map(|(_, _, s)| *s), Some(Severity::Critical));
    }

    #[test]
    fn a_missing_session_bus_does_not_break_alerting() {
        let (store, mut cfg) = setup();
        cfg.alerts.desktop_notifications = true;
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus");
        let mut m = AlertManager::new(&cfg, &store);
        m.raise(alert("x", "", Severity::Critical), &store, &cfg);
        // The alert is still recorded even though it could not be shown.
        assert_eq!(store.row_count("alert"), 1);
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    }
}
