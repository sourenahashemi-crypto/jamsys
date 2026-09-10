//! systemd unit health, over D-Bus.

use super::{Collector, Ctx};
use crate::dbus::{list_units_filtered, Connection, Unit};
use crate::types::*;
use crate::util::glob_match;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ServiceState {
    pub failed: Vec<Unit>,
    pub failed_user: Vec<Unit>,
    /// Failed units the user has chosen to ignore — shown, but not alerted on.
    pub ignored: Vec<String>,
    pub restart_counts: HashMap<String, u32>,
    pub bus_available: bool,
}

pub struct ServiceCollector {
    system: Option<Connection>,
    user: Option<Connection>,
    /// Units seen failed on the previous pass, for edge detection.
    prev_failed: std::collections::HashSet<String>,
    /// Recent failure timestamps per unit, for flap detection.
    failures: HashMap<String, Vec<i64>>,
}

impl ServiceCollector {
    pub fn new() -> Self {
        ServiceCollector {
            system: None,
            user: None,
            prev_failed: Default::default(),
            failures: Default::default(),
        }
    }

    /// Failures for `unit` inside `window_ms`. The age must be non-negative so a
    /// backwards wall-clock step cannot make old failures look current.
    pub fn flap_count(&self, unit: &str, now: i64, window_ms: i64) -> usize {
        self.failures.get(unit).map(|v| v.iter().filter(|t| (0..=window_ms).contains(&(now - **t))).count()).unwrap_or(0)
    }
}

impl Default for ServiceCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ServiceCollector {
    fn name(&self) -> &'static str {
        "services"
    }
    fn tier(&self) -> Tier {
        Tier::Slow
    }

    fn probe(&mut self) -> Support {
        self.system = Connection::system().ok();
        self.user = Connection::session().ok();
        match (&self.system, &self.user) {
            (Some(_), Some(_)) => Support::Full,
            (Some(_), None) => Support::Partial { detail: "system units only, no session bus".into() },
            (None, Some(_)) => Support::Partial { detail: "user units only, no system bus".into() },
            (None, None) => Support::Unsupported { reason: "no D-Bus connection".into() },
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = ServiceState { bus_available: true, ..Default::default() };
        let cfg = ctx.config.clone();
        let mut now_failed = std::collections::HashSet::new();

        for (conn, is_user) in [(self.system.as_mut(), false), (self.user.as_mut(), true)] {
            let Some(c) = conn else { continue };
            match list_units_filtered(c, &["failed"]) {
                Ok(units) => {
                    for u in units {
                        now_failed.insert(u.name.clone());
                        if cfg.service_ignore.iter().any(|g| glob_match(g, &u.name)) {
                            st.ignored.push(u.name.clone());
                            continue;
                        }
                        if is_user {
                            st.failed_user.push(u);
                        } else {
                            st.failed.push(u);
                        }
                    }
                }
                Err(e) => {
                    // The bus can drop when systemd reloads; reconnect next probe
                    // rather than accumulating strikes toward quarantine.
                    crate::log_warn!("systemd ListUnitsFiltered failed: {e}");
                    st.bus_available = false;
                    return Err(CollectorError::Gone(format!("D-Bus: {e}")));
                }
            }
        }

        // Only newly-failed units produce an event; a unit that has been failed since
        // boot must not re-announce itself every sixty seconds.
        for name in now_failed.difference(&self.prev_failed) {
            self.failures.entry(name.clone()).or_default().push(ctx.ts_ms);
            if let Some(v) = self.failures.get_mut(name) {
                v.retain(|t| ctx.ts_ms - *t < 3_600_000);
            }
            ctx.event(Event::new("services", "unit_failed", format!("Unit {name} entered a failed state"), Severity::Warning)
                .with_detail(serde_json::json!({"unit": name})));
        }
        for name in self.prev_failed.difference(&now_failed) {
            ctx.event(Event::new("services", "unit_recovered", format!("Unit {name} recovered"), Severity::Info)
                .with_detail(serde_json::json!({"unit": name})));
        }
        self.prev_failed = now_failed;

        for (u, v) in &self.failures {
            st.restart_counts.insert(u.clone(), v.len() as u32);
        }
        ctx.g("services", "failed_count", "", (st.failed.len() + st.failed_user.len()) as f64);
        ctx.snap.services = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn lists_this_machines_failed_units() {
        let mut c = ServiceCollector::new();
        let s = c.probe();
        if !s.is_usable() {
            eprintln!("no D-Bus in this environment, skipping");
            return;
        }
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        // The target machine really does have snap.openshell.gateway.service failed.
        let all: Vec<&str> = x.snap.services.failed.iter().map(|u| u.name.as_str()).collect();
        eprintln!("failed system units: {all:?}");
        for u in &x.snap.services.failed {
            assert_eq!(u.active_state, "failed");
        }
    }

    #[test]
    fn ignored_units_are_separated_not_alerted() {
        let mut cfg = Config::default();
        cfg.service_ignore.push("snap.*.service".into());
        let mut c = ServiceCollector::new();
        if !c.probe().is_usable() {
            return;
        }
        let mut x = Ctx::new(Arc::new(cfg));
        c.collect(&mut x).unwrap();
        assert!(
            !x.snap.services.failed.iter().any(|u| u.name.starts_with("snap.")),
            "ignored units must not appear in the alertable list"
        );
    }

    #[test]
    fn only_newly_failed_units_generate_events() {
        let mut c = ServiceCollector::new();
        if !c.probe().is_usable() {
            return;
        }
        let cfg = Arc::new(Config::default());
        let mut first = Ctx::new(cfg.clone());
        c.collect(&mut first).unwrap();
        let mut second = Ctx::new(cfg);
        c.collect(&mut second).unwrap();
        assert!(
            second.events.is_empty(),
            "a unit failed since boot must not re-announce every tick, got {:?}",
            second.events
        );
    }

    #[test]
    fn flap_counting_uses_a_sliding_window() {
        let mut c = ServiceCollector::new();
        c.failures.insert("x.service".into(), vec![0, 100, 200, 900_000]);
        assert_eq!(c.flap_count("x.service", 1_000, 600_000), 3);
        assert_eq!(c.flap_count("y.service", 1_000, 600_000), 0);
    }
}
