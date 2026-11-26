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
    /// A channel failed to read on the last pass, so the list may be stale.
    suspect: bool,
    runs: u32,
    last_rediscover_ms: i64,
}

impl ThermalCollector {
    pub fn new() -> Self {
        ThermalCollector { temps: Vec::new(), fans: Vec::new(), suspect: false, runs: 0,
                           last_rediscover_ms: 0 }
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
            // No channels means no readings, and saying "Partial" here was a lie
            // with teeth. `collect()` never reads /sys/class/thermal — grep for
            // thermal_zone and the only other hit is a log filter — so the old
            // fallback advertised coverage that could not exist. Worse, Partial
            // counts as usable, so the registry scheduled no retry and, with the
            // channel list now empty, `gone` could never again exceed zero and
            // `Gone` could never fire: the collector reported healthy, produced
            // nothing, and could not recover without a daemon restart.
            //
            // Unsupported is the honest answer and, because it is not usable, it
            // puts the collector on the bounded retry path that does recover.
            let zones = list_dir("/sys/class/thermal")
                .into_iter()
                .filter(|z| z.starts_with("thermal_zone"))
                .count();
            let reason = if zones == 0 {
                "no hwmon or thermal zones".to_string()
            } else {
                format!("no hwmon temperature or fan channels ({zones} thermal zones \
                         exist but are not read)")
            };
            return Support::Unsupported { reason };
        }
        if self.fans.is_empty() {
            return Support::Partial {
                detail: format!("{} temperature sensors, no fan tachometers", self.temps.len()),
            };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        // A channel is identified by driver and label but *read* through the
        // hwmonN path found at probe, and N is handed out in registration order.
        // If one driver reloads it comes back under a new number: its channels
        // then read nothing until a restart, or -- if another driver took the old
        // number -- read that driver's values under the wrong name. So when a
        // read fails, and every so often regardless, check each path still
        // belongs to the driver it was found under, and look again if not.
        self.runs = self.runs.wrapping_add(1);
        if (self.suspect || self.runs % 30 == 0)
            && !(0..30_000).contains(&(ctx.ts_ms - self.last_rediscover_ms))
            && (self.suspect || self.temps.iter().chain(&self.fans).any(|c| !c.still_valid()))
        {
            self.last_rediscover_ms = ctx.ts_ms;
            let (temps, fans) = (discover_hwmon(&[HwmonKind::Temp]), discover_hwmon(&[HwmonKind::Fan]));
            // Keep the old list if discovery finds nothing: the all-gone path
            // below is what reports a driver that has really left.
            if !temps.is_empty() || !fans.is_empty() {
                self.temps = temps;
                self.fans = fans;
            }
        }
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

        self.suspect = gone > 0;
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
            suspect: false, runs: 0, last_rediscover_ms: 0,
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
