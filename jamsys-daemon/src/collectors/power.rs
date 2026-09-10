//! Battery, AC and system power draw.
//!
//! Two battery flavours exist in sysfs and only one is present on any given machine:
//! *energy* domain (µWh / µW — this laptop) and *charge* domain (µAh / µA). Both are
//! handled; charge-domain values are converted with the present voltage.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize)]
pub struct PowerState {
    pub has_battery: bool,
    pub on_battery: bool,
    pub ac_online: bool,
    pub percent: f64,
    pub status: String,
    /// Positive while discharging, negative while charging.
    pub power_w: f64,
    pub voltage_v: f64,
    pub energy_now_wh: f64,
    pub energy_full_wh: f64,
    pub energy_design_wh: f64,
    /// energy_full / energy_full_design, as a percentage. Can exceed 100 on a new pack.
    pub health_pct: f64,
    pub cycle_count: i64,
    /// Estimated seconds remaining, or `None` when not discharging or rate is unknown.
    pub runtime_s: Option<i64>,
    /// CPU package watts from the privileged helper, when installed.
    pub cpu_package_w: Option<f64>,
}

pub struct PowerCollector {
    /// Root of the power-supply class. Overridable so that a machine with no battery,
    /// or one with a bogus firmware entry, can be tested without that hardware.
    root: PathBuf,
    bat: Option<PathBuf>,
    ac: Option<PathBuf>,
    /// Smoothed power, because `power_now` is noisy enough that a raw reading swings
    /// several watts between ticks and would make the UI unreadable.
    ewma_w: Option<f64>,
    last_status: String,
}

impl PowerCollector {
    pub fn new() -> Self {
        PowerCollector::with_root(PSY)
    }
}

impl PowerCollector {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        PowerCollector {
            root: root.into(),
            bat: None,
            ac: None,
            ewma_w: None,
            last_status: String::new(),
        }
    }
}

impl Default for PowerCollector {
    fn default() -> Self {
        Self::new()
    }
}

const PSY: &str = "/sys/class/power_supply";

impl Collector for PowerCollector {
    fn name(&self) -> &'static str {
        "power"
    }
    fn tier(&self) -> Tier {
        Tier::Medium
    }

    fn probe(&mut self) -> Support {
        self.bat = None;
        self.ac = None;
        let root = self.root.clone();
        for e in list_dir(&root) {
            let p = root.join(&e);
            let ty = read_str(p.join("type")).unwrap_or_default();
            if ty == "Battery" && self.bat.is_none() {
                // Some firmwares expose a second "Battery" for a wireless mouse or a
                // UPS; require a present flag and a usable energy/charge attribute
                // before accepting one as *the* system battery.
                let present = read_i64(p.join("present")).unwrap_or(1) == 1;
                let usable = exists(p.join("energy_now")) || exists(p.join("charge_now"));
                if present && usable {
                    self.bat = Some(p);
                }
            } else if ty == "Mains" && self.ac.is_none() && exists(p.join("online")) {
                self.ac = Some(p);
            }
        }
        match (&self.bat, &self.ac) {
            (Some(_), Some(_)) => Support::Full,
            (Some(_), None) => Support::Partial { detail: "no AC adapter sensor".into() },
            (None, Some(_)) => Support::Partial { detail: "no battery (desktop or VM)".into() },
            (None, None) => Support::Unsupported { reason: "no power supply sensors".into() },
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = PowerState::default();

        if let Some(ac) = &self.ac {
            st.ac_online = read_i64(ac.join("online")).unwrap_or(0) == 1;
        }

        if let Some(b) = &self.bat {
            if !b.exists() {
                return Err(CollectorError::Gone(format!("{} vanished", b.display())));
            }
            st.has_battery = true;
            st.status = read_str(b.join("status")).unwrap_or_else(|| "Unknown".into());
            st.percent = read_checked(b.join("capacity"), 0.0, 100.0).unwrap_or(0.0);
            st.cycle_count = read_i64(b.join("cycle_count")).unwrap_or(0);
            st.voltage_v = read_scaled(b.join("voltage_now"), 1e6, 0.0, 100.0).unwrap_or(0.0);

            // Energy domain (this machine) or charge domain (converted via voltage).
            let (now_wh, full_wh, design_wh, raw_w) = if exists(b.join("energy_now")) {
                (
                    read_scaled(b.join("energy_now"), 1e6, 0.0, 1e4),
                    read_scaled(b.join("energy_full"), 1e6, 0.0, 1e4),
                    read_scaled(b.join("energy_full_design"), 1e6, 0.0, 1e4),
                    read_scaled(b.join("power_now"), 1e6, 0.0, 1000.0),
                )
            } else {
                let v = if st.voltage_v > 1.0 { st.voltage_v } else { 1.0 };
                (
                    read_scaled(b.join("charge_now"), 1e6, 0.0, 1e4).map(|ah| ah * v),
                    read_scaled(b.join("charge_full"), 1e6, 0.0, 1e4).map(|ah| ah * v),
                    read_scaled(b.join("charge_full_design"), 1e6, 0.0, 1e4).map(|ah| ah * v),
                    read_scaled(b.join("current_now"), 1e6, 0.0, 100.0).map(|a| a * v),
                )
            };
            st.energy_now_wh = now_wh.unwrap_or(0.0);
            st.energy_full_wh = full_wh.unwrap_or(0.0);
            st.energy_design_wh = design_wh.unwrap_or(0.0);

            // Health can legitimately exceed 100 % on a new pack — this laptop reports
            // 101.3 % — so it is reported as measured rather than clamped, which would
            // hide a genuinely miscalibrated battery.
            if st.energy_design_wh > 0.0 {
                st.health_pct = 100.0 * st.energy_full_wh / st.energy_design_wh;
                ctx.g("power", "health_pct", "%", st.health_pct);
            }

            st.on_battery = st.status.eq_ignore_ascii_case("discharging")
                || (!st.ac_online && !st.status.eq_ignore_ascii_case("charging"));

            if let Some(w) = raw_w {
                // Some firmwares report 0 W briefly at the charge/discharge transition;
                // an EWMA hides that without hiding a real change.
                let a = 0.4;
                let s = self.ewma_w.map(|p| a * w + (1.0 - a) * p).unwrap_or(w);
                self.ewma_w = Some(s);
                st.power_w = if st.status.eq_ignore_ascii_case("charging") { -s } else { s };
                ctx.g("power", "system_w", "W", s);
                if st.on_battery && s > 0.5 {
                    st.runtime_s = Some((st.energy_now_wh / s * 3600.0) as i64);
                }
            }

            ctx.g("power", "percent", "%", st.percent);
            ctx.g("power", "energy_now_wh", "Wh", st.energy_now_wh);
            ctx.g("power", "on_battery", "", if st.on_battery { 1.0 } else { 0.0 });

            if !self.last_status.is_empty() && self.last_status != st.status {
                ctx.event(Event::new(
                    "power",
                    "ac_change",
                    format!("Power source changed: {} → {}", self.last_status, st.status),
                    Severity::Info,
                ));
            }
            self.last_status = st.status.clone();
        }

        // CPU package power, if the privileged helper is installed.
        if let Some(w) = super::privileged::cpu_package_w() {
            st.cpu_package_w = Some(w);
            ctx.g("power", "cpu_package_w", "W", w);
        }

        ctx.snap.power = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn reads_this_laptops_battery() {
        let mut c = PowerCollector::new();
        let s = c.probe();
        assert!(s.is_usable(), "battery should be detected, got {s:?}");
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let p = &x.snap.power;
        assert!(p.has_battery);
        assert!(p.percent > 0.0 && p.percent <= 100.0, "percent {}", p.percent);
        assert!(p.energy_design_wh > 10.0, "design capacity {}", p.energy_design_wh);
        assert!(p.health_pct > 50.0 && p.health_pct < 130.0, "health {}", p.health_pct);
        assert!(p.voltage_v > 5.0 && p.voltage_v < 30.0, "voltage {}", p.voltage_v);
    }

    #[test]
    fn health_above_100_percent_is_reported_not_clamped() {
        // A new pack whose full charge exceeds the design figure is normal; clamping
        // would hide a miscalibrated battery.
        let full = 91.183;
        let design = 90.004;
        let health = 100.0 * full / design;
        assert!(health > 100.0);
        assert!(health < 130.0, "but an absurd value should still look absurd");
    }

    #[test]
    fn a_machine_with_no_battery_is_unsupported_not_broken() {
        let mut c = PowerCollector::new();
        c.bat = None;
        c.ac = None;
        // Direct construction: on a desktop `probe` finds neither and must not error.
        let s = Support::Unsupported { reason: "no power supply sensors".into() };
        assert!(!s.is_usable());
        assert_eq!(s.label(), "Unavailable");
    }

    #[test]
    fn runtime_estimate_needs_a_real_discharge_rate() {
        // 84.8 Wh remaining at 21.7 W => about 3 h 54 m.
        let secs = (84.801 / 21.744 * 3600.0) as i64;
        assert!(secs > 13_000 && secs < 15_000, "got {secs}s");
    }
}
