//! CPU: utilisation, per-core, load, frequency, temperature, throttling, PSI.
//!
//! Everything here is a small read from /proc or /sys — no external process, no
//! per-core file opened more often than once per tick.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct CpuState {
    pub usage_pct: f64,
    pub per_core_pct: Vec<f64>,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub freq_mhz: f64,
    pub freq_max_mhz: f64,
    pub package_temp_c: Option<f64>,
    pub cores: usize,
    pub procs_running: u64,
    pub procs_total: u64,
    pub ctxt_per_s: f64,
    pub psi_some_avg60: f64,
    pub psi_full_avg60: f64,
    /// Cumulative package throttle events since boot.
    pub package_throttle_count: u64,
    /// True when the counter advanced during the last two ticks.
    pub throttling_now: bool,
    pub governor: String,
    pub scaling_driver: String,
}

pub struct CpuCollector {
    prev_agg: Option<CpuTimes>,
    prev_per: Vec<CpuTimes>,
    prev_ctxt: Option<(u64, i64)>,
    prev_throttle: Option<u64>,
    ncpu: usize,
    /// Rolling 5-minute mean, used for the idle/active context decision.
    hist: Vec<f64>,
}

impl CpuCollector {
    pub fn new() -> Self {
        CpuCollector {
            prev_agg: None,
            prev_per: Vec::new(),
            prev_ctxt: None,
            prev_throttle: None,
            ncpu: 0,
            hist: Vec::new(),
        }
    }

    /// Mean utilisation over the retained window, used for idle detection.
    pub fn recent_mean(&self) -> f64 {
        if self.hist.is_empty() {
            return 0.0;
        }
        self.hist.iter().sum::<f64>() / self.hist.len() as f64
    }
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        "cpu"
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }

    fn probe(&mut self) -> Support {
        if !exists("/proc/stat") {
            return Support::Unsupported { reason: "/proc/stat missing".into() };
        }
        self.ncpu = list_dir("/sys/devices/system/cpu")
            .iter()
            .filter(|d| d.starts_with("cpu") && d[3..].chars().all(|c| c.is_ascii_digit()) && d.len() > 3)
            .count();
        let mut missing = Vec::new();
        if !exists("/proc/pressure/cpu") {
            missing.push("PSI (kernel built without CONFIG_PSI)");
        }
        if !exists("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq") {
            missing.push("frequency (no cpufreq driver)");
        }
        if !exists("/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_count") {
            missing.push("throttle counters");
        }
        if missing.is_empty() {
            Support::Full
        } else {
            Support::Partial { detail: missing.join(", ") }
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let stat = std::fs::read_to_string("/proc/stat")
            .map_err(|e| CollectorError::Gone(format!("/proc/stat: {e}")))?;
        let (agg, per, extra) = parse_proc_stat(&stat);

        let mut st = CpuState { cores: per.len(), ..Default::default() };

        if let Some(prev) = &self.prev_agg {
            if let Some(p) = agg.busy_pct(prev) {
                st.usage_pct = p;
                ctx.g("cpu", "usage_pct", "%", p);
                self.hist.push(p);
                // 150 fast ticks ~= 5 minutes at the default 2 s interval.
                if self.hist.len() > 150 {
                    self.hist.remove(0);
                }
            }
        }
        for (i, t) in per.iter().enumerate() {
            if let Some(prev) = self.prev_per.get(i) {
                if let Some(p) = t.busy_pct(prev) {
                    st.per_core_pct.push(p);
                    ctx.sample("cpu", "core_pct", &format!("cpu{i}"), "%", p);
                }
            }
        }
        self.prev_agg = Some(agg);
        self.prev_per = per;

        // Context switches per second.
        if let Some(&c) = extra.get("ctxt") {
            if let Some((pc, pt)) = self.prev_ctxt {
                let dt = (ctx.ts_ms - pt) as f64 / 1000.0;
                if dt > 0.0 && c >= pc {
                    st.ctxt_per_s = (c - pc) as f64 / dt;
                    ctx.g("cpu", "ctxt_per_s", "1/s", st.ctxt_per_s);
                }
            }
            self.prev_ctxt = Some((c, ctx.ts_ms));
        }
        st.procs_running = extra.get("procs_running").copied().unwrap_or(0);
        ctx.g("cpu", "procs_running", "", st.procs_running as f64);

        if let Some(l) = read_str("/proc/loadavg") {
            let f: Vec<&str> = l.split_whitespace().collect();
            st.load1 = f.first().and_then(|x| x.parse().ok()).unwrap_or(0.0);
            st.load5 = f.get(1).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            st.load15 = f.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            // "3/842" -> running/total
            if let Some(procs) = f.get(3) {
                if let Some((_, total)) = procs.split_once('/') {
                    st.procs_total = total.parse().unwrap_or(0);
                }
            }
            ctx.g("cpu", "load1", "", st.load1);
            ctx.g("cpu", "load5", "", st.load5);
            ctx.g("cpu", "procs_total", "", st.procs_total as f64);
        }

        if let Some(text) = read_str("/proc/pressure/cpu") {
            if let Some(p) = parse_psi(&text) {
                st.psi_some_avg60 = p.some_avg60;
                st.psi_full_avg60 = p.full_avg60;
                ctx.g("cpu", "psi_some", "%", p.some_avg60);
            }
        }

        // Mean current frequency across cores. Reading every core each fast tick is
        // ~24 tiny reads; measured at well under 200 µs total on this machine.
        let mut fsum = 0.0;
        let mut fn_ = 0usize;
        for i in 0..self.ncpu {
            if let Some(f) = read_f64(format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")) {
                if f > 0.0 && f < 1e8 {
                    fsum += f / 1000.0;
                    fn_ += 1;
                }
            }
        }
        if fn_ > 0 {
            st.freq_mhz = fsum / fn_ as f64;
            ctx.g("cpu", "freq_mhz", "MHz", st.freq_mhz);
        }
        st.freq_max_mhz = read_f64("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
            .map(|f| f / 1000.0)
            .unwrap_or(0.0);
        st.governor = read_str("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").unwrap_or_default();
        st.scaling_driver = read_str("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver").unwrap_or_default();

        // Throttling: the counter is monotonic, so an increase means it happened *now*.
        // Reading the instantaneous frequency and guessing would give false positives
        // every time the governor idles a core down.
        if let Some(c) = read_u64("/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_count") {
            st.package_throttle_count = c;
            if let Some(prev) = self.prev_throttle {
                st.throttling_now = c > prev;
                if c > prev {
                    ctx.event(Event::new(
                        "cpu",
                        "throttle",
                        format!("CPU package throttled {} time(s)", c - prev),
                        Severity::Info,
                    ));
                }
            }
            self.prev_throttle = Some(c);
            ctx.g("cpu", "throttle_count", "", c as f64);
        }

        ctx.snap.cpu = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    fn ctx() -> Ctx {
        Ctx::new(Arc::new(Config::default()))
    }

    #[test]
    fn probes_and_collects_on_this_machine() {
        let mut c = CpuCollector::new();
        let s = c.probe();
        assert!(s.is_usable(), "CPU must always be supported on Linux, got {s:?}");
        let mut x = ctx();
        c.collect(&mut x).expect("first collect");
        // The first tick has no previous counter so usage is not yet known; the second
        // must produce a real reading.
        std::thread::sleep(std::time::Duration::from_millis(60));
        let mut x2 = ctx();
        c.collect(&mut x2).expect("second collect");
        assert!(x2.snap.cpu.cores > 0, "no cores detected");
        assert!(x2.snap.cpu.usage_pct >= 0.0 && x2.snap.cpu.usage_pct <= 100.0);
        assert!(x2.samples.iter().any(|s| s.id.name == "usage_pct"));
        assert!(x2.samples.iter().any(|s| s.id.name == "core_pct"), "per-core missing");
        assert!(x2.snap.cpu.load1 >= 0.0);
    }

    #[test]
    fn first_tick_emits_no_bogus_utilisation() {
        let mut c = CpuCollector::new();
        c.probe();
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        // Without a previous sample there is no delta, so no usage metric at all —
        // better than reporting a meaningless since-boot average as "now".
        assert!(!x.samples.iter().any(|s| s.id.name == "usage_pct"));
    }

    #[test]
    fn recent_mean_tracks_the_window() {
        let mut c = CpuCollector::new();
        c.hist = vec![10.0, 20.0, 30.0];
        assert!((c.recent_mean() - 20.0).abs() < 1e-9);
        c.hist.clear();
        assert_eq!(c.recent_mean(), 0.0);
    }
}
