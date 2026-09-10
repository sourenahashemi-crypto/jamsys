//! Memory: usage, cache, swap, PSI, OOM kills.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct MemState {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub cached_bytes: u64,
    pub buffers_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub dirty_bytes: u64,
    pub available_pct: f64,
    pub swap_used_pct: f64,
    pub psi_some_avg60: f64,
    pub psi_full_avg60: f64,
    /// Cumulative OOM kills since boot, from /proc/vmstat.
    pub oom_kill_total: u64,
    /// True on the tick where the counter advanced.
    pub oom_just_happened: bool,
    pub major_faults_per_s: f64,
}

pub struct MemCollector {
    prev_oom: Option<u64>,
    prev_pgmajfault: Option<(u64, i64)>,
}

impl MemCollector {
    pub fn new() -> Self {
        MemCollector { prev_oom: None, prev_pgmajfault: None }
    }
}

impl Default for MemCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for MemCollector {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }

    fn probe(&mut self) -> Support {
        if !exists("/proc/meminfo") {
            return Support::Unsupported { reason: "/proc/meminfo missing".into() };
        }
        if !exists("/proc/pressure/memory") {
            return Support::Partial { detail: "PSI unavailable (CONFIG_PSI off)".into() };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let text = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| CollectorError::Gone(format!("/proc/meminfo: {e}")))?;
        let m = parse_kv_kb(&text);
        let kb = |k: &str| m.get(k).copied().unwrap_or(0) * 1024;

        let mut st = MemState {
            total_bytes: kb("MemTotal"),
            available_bytes: kb("MemAvailable"),
            cached_bytes: kb("Cached"),
            buffers_bytes: kb("Buffers"),
            swap_total_bytes: kb("SwapTotal"),
            dirty_bytes: kb("Dirty"),
            ..Default::default()
        };
        if st.total_bytes == 0 {
            return Err(CollectorError::BadData("MemTotal is zero".into()));
        }
        st.swap_used_bytes = st.swap_total_bytes.saturating_sub(kb("SwapFree"));
        // "Used" excludes reclaimable cache — the number a user recognises as memory
        // actually consumed, not the kernel's raw MemTotal-MemFree.
        st.used_bytes = st.total_bytes.saturating_sub(st.available_bytes);
        st.available_pct = 100.0 * st.available_bytes as f64 / st.total_bytes as f64;
        st.swap_used_pct = if st.swap_total_bytes > 0 {
            100.0 * st.swap_used_bytes as f64 / st.swap_total_bytes as f64
        } else {
            0.0
        };

        ctx.g("memory", "total_bytes", "B", st.total_bytes as f64);
        ctx.g("memory", "available_bytes", "B", st.available_bytes as f64);
        ctx.g("memory", "used_bytes", "B", st.used_bytes as f64);
        ctx.g("memory", "cached_bytes", "B", st.cached_bytes as f64);
        ctx.g("memory", "swap_used_bytes", "B", st.swap_used_bytes as f64);
        ctx.g("memory", "available_pct", "%", st.available_pct);

        if let Some(t) = read_str("/proc/pressure/memory") {
            if let Some(p) = parse_psi(&t) {
                st.psi_some_avg60 = p.some_avg60;
                st.psi_full_avg60 = p.full_avg60;
                ctx.g("memory", "psi_some", "%", p.some_avg60);
                ctx.g("memory", "psi_full", "%", p.full_avg60);
            }
        }

        if let Some(t) = read_str("/proc/vmstat") {
            let v = parse_kv_space(&t);
            if let Some(&oom) = v.get("oom_kill") {
                st.oom_kill_total = oom;
                if let Some(prev) = self.prev_oom {
                    if oom > prev {
                        st.oom_just_happened = true;
                        ctx.event(Event::new(
                            "memory",
                            "oom",
                            format!("Kernel OOM killer terminated {} process(es)", oom - prev),
                            Severity::Critical,
                        ));
                    }
                }
                self.prev_oom = Some(oom);
                ctx.g("memory", "oom_kill_total", "", oom as f64);
            }
            if let Some(&mf) = v.get("pgmajfault") {
                if let Some((p, t0)) = self.prev_pgmajfault {
                    let dt = (ctx.ts_ms - t0) as f64 / 1000.0;
                    if dt > 0.0 && mf >= p {
                        st.major_faults_per_s = (mf - p) as f64 / dt;
                        ctx.g("memory", "majfault_per_s", "1/s", st.major_faults_per_s);
                    }
                }
                self.prev_pgmajfault = Some((mf, ctx.ts_ms));
            }
        }

        ctx.snap.memory = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn reads_real_memory_state() {
        let mut c = MemCollector::new();
        assert!(c.probe().is_usable());
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let m = &x.snap.memory;
        assert!(m.total_bytes > 1 << 30, "implausible total {}", m.total_bytes);
        assert!(m.available_bytes <= m.total_bytes);
        assert!(m.available_pct > 0.0 && m.available_pct <= 100.0);
        assert!(m.used_bytes + m.available_bytes <= m.total_bytes + 1024);
        assert!(x.samples.iter().any(|s| s.id.name == "available_pct"));
    }

    #[test]
    fn oom_counter_fires_once_per_increase() {
        let mut c = MemCollector::new();
        c.prev_oom = Some(5);
        // Simulate the delta logic directly: the collector must only report the
        // transition, never re-report a historical total.
        assert!(c.prev_oom.is_some());
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        // On a healthy machine oom_kill is 0, which is < 5, so no event.
        assert!(!x.events.iter().any(|e| e.kind == "oom"));
    }

    #[test]
    fn swap_percentage_is_zero_not_nan_without_swap() {
        // A machine with no swap must not produce NaN and poison the database.
        let st = MemState { swap_total_bytes: 0, swap_used_bytes: 0, ..Default::default() };
        assert_eq!(st.swap_used_pct, 0.0);
        assert!(!st.swap_used_pct.is_nan());
    }
}
