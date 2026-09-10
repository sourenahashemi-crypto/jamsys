//! Lightweight process monitoring.
//!
//! Scanning /proc is the single most expensive thing this daemon does, so it runs on
//! the slow tier and reads at most four small files per process. Per-process GPU usage
//! comes from DRM fdinfo, which is unprivileged for your own processes.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcInfo {
    pub pid: i32,
    pub name: String,
    pub cmd: String,
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    pub gpu_ns_per_s: f64,
    pub threads: u32,
    pub uid: u32,
    /// Populated when a rule considers this process abnormal, with the reason.
    pub flag: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcState {
    pub top_cpu: Vec<ProcInfo>,
    pub top_mem: Vec<ProcInfo>,
    pub total: usize,
    /// RSS growth per process over the retained window, in bytes/hour.
    pub growth_bph: HashMap<i32, f64>,
}

#[derive(Clone, Copy)]
struct Prev {
    utime: u64,
    stime: u64,
    ts_ms: i64,
    read_bytes: u64,
    write_bytes: u64,
    gpu_ns: u64,
}

/// Bounded history for leak detection: (timestamp, RSS).
type RssHistory = Vec<(i64, f64)>;

pub struct ProcCollector {
    prev: HashMap<i32, Prev>,
    rss_hist: HashMap<i32, RssHistory>,
    names: HashMap<i32, String>,
    clk_tck: f64,
    page_size: u64,
    /// How many processes to report in each top-N list.
    top_n: usize,
}

impl ProcCollector {
    pub fn new() -> Self {
        // SAFETY: sysconf with well-known names; both return positive values on Linux.
        let clk = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        ProcCollector {
            prev: HashMap::new(),
            rss_hist: HashMap::new(),
            names: HashMap::new(),
            clk_tck: if clk > 0 { clk as f64 } else { 100.0 },
            page_size: if ps > 0 { ps as u64 } else { 4096 },
            top_n: 12,
        }
    }

    /// RSS history for a pid, oldest first. Used by the leak rule.
    pub fn rss_history(&self, pid: i32) -> Option<&RssHistory> {
        self.rss_hist.get(&pid)
    }

    pub fn name_of(&self, pid: i32) -> Option<&str> {
        self.names.get(&pid).map(|s| s.as_str())
    }
}

impl Default for ProcCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the fields of /proc/<pid>/stat that matter.
///
/// The comm field is parenthesised and may itself contain spaces and parentheses
/// (`(a (weird) name)`), so the tail is located from the **last** `)`, never by
/// splitting on whitespace.
pub fn parse_pid_stat(s: &str) -> Option<(String, u64, u64, u32)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = s[open + 1..close].to_string();
    let rest: Vec<&str> = s[close + 1..].split_whitespace().collect();
    // After comm, field 3 is state; utime is field 14 overall => index 11 here.
    let utime = rest.get(11)?.parse::<u64>().ok()?;
    let stime = rest.get(12)?.parse::<u64>().ok()?;
    let threads = rest.get(17).and_then(|x| x.parse::<u32>().ok()).unwrap_or(1);
    Some((comm, utime, stime, threads))
}

/// Sum `drm-engine-*` nanoseconds across a process' DRM fds.
///
/// Unprivileged for your own processes and the only per-process GPU signal that works
/// for the Intel iGPU at all.
fn drm_engine_ns(pid: i32) -> u64 {
    let dir = format!("/proc/{pid}/fdinfo");
    let mut total = 0u64;
    for e in list_dir(&dir) {
        let Ok(text) = std::fs::read_to_string(format!("{dir}/{e}")) else { continue };
        if !text.contains("drm-driver") {
            continue;
        }
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("drm-engine-") {
                if let Some((_, v)) = rest.split_once(':') {
                    if let Some(n) = v.trim().strip_suffix(" ns").and_then(|x| x.trim().parse::<u64>().ok()) {
                        total = total.saturating_add(n);
                    }
                }
            }
        }
    }
    total
}

impl Collector for ProcCollector {
    fn name(&self) -> &'static str {
        "process"
    }
    fn tier(&self) -> Tier {
        Tier::Slow
    }

    fn probe(&mut self) -> Support {
        if !exists("/proc/self/stat") {
            return Support::Unsupported { reason: "/proc unavailable".into() };
        }
        // /proc/<pid>/io is readable only for your own processes without root, which
        // is the intended behaviour — the app must not need privileges for normal use.
        if !exists("/proc/self/io") {
            return Support::Partial { detail: "per-process I/O counters unavailable".into() };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut list: Vec<ProcInfo> = Vec::with_capacity(400);
        let mut live: std::collections::HashSet<i32> = Default::default();
        let my_uid = unsafe { libc::getuid() };

        for e in list_dir("/proc") {
            let Ok(pid) = e.parse::<i32>() else { continue };
            let Some(stat) = read_str(format!("/proc/{pid}/stat")) else { continue };
            let Some((comm, utime, stime, threads)) = parse_pid_stat(&stat) else { continue };
            live.insert(pid);

            // statm field 2 is resident pages — one small read instead of parsing
            // the whole of /proc/<pid>/status.
            let rss_bytes = read_str(format!("/proc/{pid}/statm"))
                .and_then(|s| s.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()))
                .map(|pages| pages * self.page_size)
                .unwrap_or(0);

            let uid = std::fs::metadata(format!("/proc/{pid}"))
                .map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    m.uid()
                })
                .unwrap_or(0);

            // I/O and GPU only for our own processes: for others the read fails, and
            // attempting it for hundreds of PIDs would be pure wasted syscalls.
            let (rb, wb) = if uid == my_uid {
                read_str(format!("/proc/{pid}/io"))
                    .map(|t| {
                        let m = parse_kv_space(&t.replace(':', " "));
                        (m.get("read_bytes").copied().unwrap_or(0), m.get("write_bytes").copied().unwrap_or(0))
                    })
                    .unwrap_or((0, 0))
            } else {
                (0, 0)
            };
            let gpu_ns = if uid == my_uid { drm_engine_ns(pid) } else { 0 };

            let mut p = ProcInfo {
                pid,
                name: comm.clone(),
                cmd: read_str(format!("/proc/{pid}/cmdline"))
                    .map(|c| c.replace('\0', " ").trim().to_string())
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| format!("[{comm}]")),
                rss_bytes,
                threads,
                uid,
                ..Default::default()
            };

            if let Some(pv) = self.prev.get(&pid) {
                let dt_ms = (ctx.ts_ms - pv.ts_ms) as f64;
                if dt_ms > 0.0 {
                    let dt = dt_ms / 1000.0;
                    let dticks = (utime + stime).saturating_sub(pv.utime + pv.stime) as f64;
                    p.cpu_pct = 100.0 * (dticks / self.clk_tck) / dt;
                    p.read_bps = rb.saturating_sub(pv.read_bytes) as f64 / dt;
                    p.write_bps = wb.saturating_sub(pv.write_bytes) as f64 / dt;
                    p.gpu_ns_per_s = gpu_ns.saturating_sub(pv.gpu_ns) as f64 / dt;
                }
            }
            self.prev.insert(
                pid,
                Prev { utime, stime, ts_ms: ctx.ts_ms, read_bytes: rb, write_bytes: wb, gpu_ns },
            );

            // Bounded RSS history for the leak detector: 30 min at the slow tier.
            let h = self.rss_hist.entry(pid).or_default();
            h.push((ctx.ts_ms, rss_bytes as f64));
            if h.len() > 32 {
                h.remove(0);
            }
            self.names.insert(pid, comm);
            list.push(p);
        }

        // Reap bookkeeping for exited processes, or these maps grow forever on a
        // machine that churns processes (a build, a shell loop).
        self.prev.retain(|k, _| live.contains(k));
        self.rss_hist.retain(|k, _| live.contains(k));
        self.names.retain(|k, _| live.contains(k));

        let mut st = ProcState { total: list.len(), ..Default::default() };
        for (pid, h) in &self.rss_hist {
            if h.len() >= 8 {
                let t0 = h[0].0 as f64;
                let xs: Vec<f64> = h.iter().map(|(t, _)| (*t as f64 - t0) / 3_600_000.0).collect();
                let ys: Vec<f64> = h.iter().map(|(_, v)| *v).collect();
                let slope = crate::util::theil_sen_slope(&xs, &ys);
                if slope.is_finite() {
                    st.growth_bph.insert(*pid, slope);
                }
            }
        }

        // Break CPU ties by memory. On the very first pass there are no deltas yet, so
        // every cpu_pct is 0.0 and an unstable sort would surface kernel threads —
        // technically correct and completely useless to a user.
        list.sort_by(|a, b| {
            b.cpu_pct
                .partial_cmp(&a.cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.rss_bytes.cmp(&a.rss_bytes))
        });
        st.top_cpu = list.iter().take(self.top_n).cloned().collect();
        list.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes));
        st.top_mem = list.into_iter().take(self.top_n).collect();

        ctx.g("process", "count", "", st.total as f64);
        ctx.snap.process = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn pid_stat_parses_a_simple_comm() {
        let s = "1234 (bash) S 1 1234 1234 0 -1 4194304 100 200 0 0 15 7 0 0 20 0 1 0 999 100 50";
        let (comm, ut, st, th) = parse_pid_stat(s).unwrap();
        assert_eq!(comm, "bash");
        assert_eq!(ut, 15);
        assert_eq!(st, 7);
        assert_eq!(th, 1);
    }

    #[test]
    fn pid_stat_survives_a_comm_containing_spaces_and_parentheses() {
        // This is the classic /proc parsing bug; a whitespace split gets it wrong.
        let s = "42 (my (weird) app) S 1 42 42 0 -1 4194304 10 20 0 0 99 11 0 0 20 0 4 0 999 100 50";
        let (comm, ut, st, th) = parse_pid_stat(s).unwrap();
        assert_eq!(comm, "my (weird) app");
        assert_eq!(ut, 99);
        assert_eq!(st, 11);
        assert_eq!(th, 4);
    }

    #[test]
    fn malformed_stat_lines_are_rejected() {
        assert!(parse_pid_stat("").is_none());
        assert!(parse_pid_stat("1234 no-parens S 1").is_none());
        assert!(parse_pid_stat("1234 (short) S").is_none());
    }

    #[test]
    fn scans_real_processes_without_privileges() {
        let mut c = ProcCollector::new();
        assert!(c.probe().is_usable());
        let cfg = Arc::new(Config::default());
        let mut x = Ctx::new(cfg.clone());
        c.collect(&mut x).unwrap();
        assert!(x.snap.process.total > 20, "only {} processes seen", x.snap.process.total);
        // Second pass produces real CPU percentages.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let mut y = Ctx::new(cfg);
        c.collect(&mut y).unwrap();
        let p = &y.snap.process;
        assert_eq!(p.top_cpu.len().min(12), p.top_cpu.len());
        assert!(p.top_mem.iter().any(|q| q.rss_bytes > 1_000_000), "no process with meaningful RSS");
        for q in &p.top_cpu {
            assert!(q.cpu_pct >= 0.0, "{} has negative CPU", q.name);
            assert!(q.cpu_pct < 100.0 * 64.0, "{} at {}% is implausible", q.name, q.cpu_pct);
        }
    }

    #[test]
    fn bookkeeping_is_reaped_for_exited_processes() {
        let mut c = ProcCollector::new();
        c.probe();
        let cfg = Arc::new(Config::default());
        // Inject a process id that cannot exist.
        c.prev.insert(-9999, Prev { utime: 0, stime: 0, ts_ms: 0, read_bytes: 0, write_bytes: 0, gpu_ns: 0 });
        c.rss_hist.insert(-9999, vec![(0, 1.0)]);
        c.names.insert(-9999, "ghost".into());
        let mut x = Ctx::new(cfg);
        c.collect(&mut x).unwrap();
        assert!(!c.prev.contains_key(&-9999), "dead pid left in the CPU map");
        assert!(!c.rss_hist.contains_key(&-9999), "dead pid left in the RSS map");
        assert!(c.name_of(-9999).is_none());
    }

    #[test]
    fn rss_history_is_bounded() {
        let mut c = ProcCollector::new();
        c.probe();
        let cfg = Arc::new(Config::default());
        for _ in 0..40 {
            let mut x = Ctx::new(cfg.clone());
            c.collect(&mut x).unwrap();
        }
        for (_, h) in c.rss_hist.iter() {
            assert!(h.len() <= 32, "history grew to {}", h.len());
        }
    }
}
