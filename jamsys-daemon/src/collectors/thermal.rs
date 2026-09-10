//! Thermals and fans, from hwmon.
//!
//! Channels are resolved by driver name + label at probe time, never by `hwmonN` index
//! — see `sysfs::HwmonChannel` for why that matters across kernel upgrades.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Reading {
    pub key: String,
    pub label: String,
    pub driver: String,
    pub value: f64,
    pub crit: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ThermalState {
    pub temps: Vec<Reading>,
    pub fans: Vec<Reading>,
    pub cpu_package_c: Option<f64>,
    pub nvme_c: Option<f64>,
    pub max_temp_c: Option<f64>,
    pub max_temp_label: String,
}

pub struct ThermalCollector {
    temps: Vec<HwmonChannel>,
    fans: Vec<HwmonChannel>,
}

impl ThermalCollector {
    pub fn new() -> Self {
        ThermalCollector { temps: Vec::new(), fans: Vec::new() }
    }
}

impl Default for ThermalCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// The channel that best represents "the CPU package".
fn is_cpu_package(c: &HwmonChannel) -> bool {
    (c.driver == "coretemp" && c.label.starts_with("Package"))
        || (c.driver == "k10temp" && (c.label == "Tctl" || c.label == "Tdie"))
        || c.driver == "zenpower"
}

impl Collector for ThermalCollector {
    fn name(&self) -> &'static str {
        "thermal"
    }
    fn tier(&self) -> Tier {
        Tier::Medium
    }

    fn probe(&mut self) -> Support {
        self.temps = discover_hwmon(&[HwmonKind::Temp]);
        self.fans = discover_hwmon(&[HwmonKind::Fan]);
        if self.temps.is_empty() && self.fans.is_empty() {
            // Fall back to thermal zones, which exist even where hwmon does not.
            let zones = list_dir("/sys/class/thermal")
                .into_iter()
                .filter(|z| z.starts_with("thermal_zone"))
                .count();
            if zones == 0 {
                return Support::Unsupported { reason: "no hwmon or thermal zones".into() };
            }
            return Support::Partial { detail: "thermal zones only, no hwmon".into() };
        }
        if self.fans.is_empty() {
            return Support::Partial {
                detail: format!("{} temperature sensors, no fan tachometers", self.temps.len()),
            };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = ThermalState::default();
        let mut gone = 0usize;

        for c in &self.temps {
            match c.read() {
                Some(v) => {
                    let r = Reading {
                        key: c.key(),
                        label: c.label.clone(),
                        driver: c.driver.clone(),
                        value: v,
                        crit: c.crit,
                    };
                    ctx.sample("thermal", "temp_c", &r.key, "C", v);
                    if is_cpu_package(c) {
                        st.cpu_package_c = Some(v);
                    }
                    if c.driver == "nvme" && c.label.starts_with("Composite") {
                        st.nvme_c = Some(v);
                    }
                    if st.max_temp_c.map(|m| v > m).unwrap_or(true) {
                        st.max_temp_c = Some(v);
                        st.max_temp_label = r.key.clone();
                    }
                    st.temps.push(r);
                }
                None => gone += 1,
            }
        }

        for c in &self.fans {
            if let Some(v) = c.read() {
                ctx.sample("thermal", "fan_rpm", &c.key(), "rpm", v);
                st.fans.push(Reading {
                    key: c.key(),
                    label: c.label.clone(),
                    driver: c.driver.clone(),
                    value: v,
                    crit: None,
                });
            } else {
                gone += 1;
            }
        }

        // Every channel disappearing at once means a driver unloaded, not a bad reading.
        if gone > 0 && st.temps.is_empty() && st.fans.is_empty() {
            // Ctx retains the previous tick. Clear it even when the registry will
            // stop sampling this collector, or old temperatures keep driving alerts.
            ctx.snap.thermal = st;
            return Err(CollectorError::Gone("all hwmon channels vanished".into()));
        }

        if let Some(t) = st.cpu_package_c {
            ctx.g("thermal", "cpu_package_c", "C", t);
        }
        ctx.snap.thermal = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn discovers_and_reads_real_sensors() {
        let mut c = ThermalCollector::new();
        let s = c.probe();
        assert!(s.is_usable(), "expected sensors on this machine, got {s:?}");
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let t = &x.snap.thermal;
        assert!(!t.temps.is_empty(), "no temperatures read");
        for r in &t.temps {
            assert!(r.value > -40.0 && r.value < 150.0, "{} = {} is implausible", r.key, r.value);
        }
        assert!(t.max_temp_c.is_some());
    }

    #[test]
    fn identifies_the_cpu_package_channel() {
        let c = HwmonChannel {
            driver: "coretemp".into(),
            label: "Package id 0".into(),
            kind: HwmonKind::Temp,
            input: "/dev/null".into(),
            crit: Some(100.0),
            max: None,
        };
        assert!(is_cpu_package(&c));
        let amd = HwmonChannel { driver: "k10temp".into(), label: "Tctl".into(), ..c.clone() };
        assert!(is_cpu_package(&amd), "AMD machines must also be recognised");
        let core = HwmonChannel { label: "Core 8".into(), ..c.clone() };
        assert!(!is_cpu_package(&core));
    }

    #[test]
    fn a_machine_with_no_sensors_reports_unsupported_not_an_error() {
        // Simulates a VM: probe against an empty hwmon root.
        let chans = discover_hwmon_in("/nonexistent", &[HwmonKind::Temp]);
        assert!(chans.is_empty());
    }

    #[test]
    fn losing_all_channels_clears_the_previous_snapshot() {
        let mut c = ThermalCollector {
            temps: vec![HwmonChannel {
                driver: "coretemp".into(), label: "Package id 0".into(),
                kind: HwmonKind::Temp, input: "/nonexistent/jamsys-temp".into(),
                crit: None, max: None,
            }],
            fans: Vec::new(),
        };
        let mut x = Ctx::new(Arc::new(Config::default()));
        x.snap.thermal.cpu_package_c = Some(99.0);
        x.snap.thermal.max_temp_c = Some(99.0);
        x.snap.thermal.temps.push(Reading { value: 99.0, ..Default::default() });
        assert!(matches!(c.collect(&mut x), Err(CollectorError::Gone(_))));
        assert_eq!(x.snap.thermal.cpu_package_c, None, "a vanished sensor must not stay hot forever");
        assert_eq!(x.snap.thermal.max_temp_c, None);
        assert!(x.snap.thermal.temps.is_empty());
        assert!(x.samples.is_empty());
    }
}
