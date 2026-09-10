//! Validated readers for /proc and /sys.
//!
//! Two rules hold everywhere in this module:
//!
//! 1. **Nothing here panics or propagates an unexpected error for a missing file.**
//!    Files under /sys appear and disappear as drivers load, devices are unplugged and
//!    kernels are upgraded. Absence is `None`, not a failure.
//! 2. **Every parsed number is range-checked.** Drivers really do return `-274000`,
//!    `65535`, empty strings and `2^63-1` for "I don't know". An unchecked value becomes
//!    a false alert, so a value outside physical plausibility is treated as absent.

use crate::util::sane;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Read a file to a trimmed `String`. `None` if it does not exist or is not readable.
pub fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Read a file expected to contain a single integer.
pub fn read_i64(p: impl AsRef<Path>) -> Option<i64> {
    read_str(p)?.parse::<i64>().ok()
}

pub fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    read_str(p)?.parse::<u64>().ok()
}

pub fn read_f64(p: impl AsRef<Path>) -> Option<f64> {
    read_str(p)?.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Read an integer and range-check it. This is the function collectors should use.
pub fn read_checked(p: impl AsRef<Path>, lo: f64, hi: f64) -> Option<f64> {
    sane(read_i64(p)? as f64, lo, hi)
}

/// Read a millidegree/microvolt-style sysfs integer and scale it.
/// `scale` is the divisor: 1000 for m-units, 1_000_000 for µ-units.
pub fn read_scaled(p: impl AsRef<Path>, scale: f64, lo: f64, hi: f64) -> Option<f64> {
    sane(read_i64(p)? as f64 / scale, lo, hi)
}

pub fn exists(p: impl AsRef<Path>) -> bool {
    p.as_ref().exists()
}

/// List directory entry names, sorted for deterministic ordering.
/// Sorting matters: unsorted `read_dir` gives inode order, so per-core metrics would
/// shuffle between boots and history would be attributed to the wrong instance.
pub fn list_dir(p: impl AsRef<Path>) -> Vec<String> {
    let mut v: Vec<String> = match fs::read_dir(p) {
        Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| natural_cmp(a, b));
    v
}

/// Compare with embedded numbers ordered numerically, so `cpu2` sorts before `cpu10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut nx = 0u64;
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nx = nx.saturating_mul(10).saturating_add(c as u64 - '0' as u64);
                    ai.next();
                }
                let mut ny = 0u64;
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    ny = ny.saturating_mul(10).saturating_add(c as u64 - '0' as u64);
                    bi.next();
                }
                if nx != ny {
                    return nx.cmp(&ny);
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hwmon
// ---------------------------------------------------------------------------

/// One resolved hwmon channel.
///
/// Identified by **driver name + channel label**, never by the `hwmonN` index. On this
/// machine the NVMe controller is `hwmon4` today; after a kernel upgrade or a driver
/// load-order change it can be `hwmon6`. Keying history on the index would silently
/// attribute NVMe temperatures to the CPU. Keying on `("nvme", "Composite")` is stable.
#[derive(Clone, Debug, PartialEq)]
pub struct HwmonChannel {
    pub driver: String,
    pub label: String,
    pub kind: HwmonKind,
    pub input: PathBuf,
    pub crit: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HwmonKind {
    Temp,
    Fan,
    Power,
    Voltage,
    Current,
}

impl HwmonKind {
    fn prefix(self) -> &'static str {
        match self {
            HwmonKind::Temp => "temp",
            HwmonKind::Fan => "fan",
            HwmonKind::Power => "power",
            HwmonKind::Voltage => "in",
            HwmonKind::Current => "curr",
        }
    }
    /// Divisor from the raw sysfs integer to the display unit.
    fn scale(self) -> f64 {
        match self {
            HwmonKind::Temp => 1000.0,     // millidegrees C
            HwmonKind::Fan => 1.0,         // RPM
            HwmonKind::Power => 1_000_000.0, // microwatts
            HwmonKind::Voltage => 1000.0,  // millivolts
            HwmonKind::Current => 1000.0,  // milliamps
        }
    }
    /// Physically plausible range, used to reject driver sentinel values.
    fn range(self) -> (f64, f64) {
        match self {
            HwmonKind::Temp => (-40.0, 150.0),
            HwmonKind::Fan => (0.0, 30_000.0),
            HwmonKind::Power => (0.0, 1000.0),
            HwmonKind::Voltage => (0.0, 100.0),
            HwmonKind::Current => (0.0, 100.0),
        }
    }
}

/// Walk `/sys/class/hwmon` once and resolve every channel of the requested kinds.
pub fn discover_hwmon(kinds: &[HwmonKind]) -> Vec<HwmonChannel> {
    discover_hwmon_in("/sys/class/hwmon", kinds)
}

pub fn discover_hwmon_in(root: &str, kinds: &[HwmonKind]) -> Vec<HwmonChannel> {
    let mut out = Vec::new();
    for dev in list_dir(root) {
        if !dev.starts_with("hwmon") {
            continue;
        }
        let dir = Path::new(root).join(&dev);
        let driver = read_str(dir.join("name")).unwrap_or_else(|| dev.clone());
        for &kind in kinds {
            let pfx = kind.prefix();
            for entry in list_dir(&dir) {
                // Match `<prefix><N>_input` and nothing else.
                let Some(rest) = entry.strip_prefix(pfx) else { continue };
                let Some(idx) = rest.strip_suffix("_input") else { continue };
                if idx.is_empty() || !idx.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let label = read_str(dir.join(format!("{pfx}{idx}_label")))
                    .unwrap_or_else(|| format!("{pfx}{idx}"));
                let (lo, hi) = kind.range();
                let s = kind.scale();
                out.push(HwmonChannel {
                    driver: driver.clone(),
                    label,
                    kind,
                    input: dir.join(&entry),
                    crit: read_scaled(dir.join(format!("{pfx}{idx}_crit")), s, lo, hi),
                    max: read_scaled(dir.join(format!("{pfx}{idx}_max")), s, lo, hi),
                });
            }
        }
    }
    out
}

impl HwmonChannel {
    /// Current reading in display units, or `None` if the sensor vanished or lied.
    pub fn read(&self) -> Option<f64> {
        let (lo, hi) = self.kind.range();
        read_scaled(&self.input, self.kind.scale(), lo, hi)
    }
    /// Stable instance key for the metric table.
    pub fn key(&self) -> String {
        format!("{}/{}", self.driver, self.label)
    }
}

// ---------------------------------------------------------------------------
// /proc parsers
// ---------------------------------------------------------------------------

/// Parse a `key: value` file such as /proc/meminfo into kB-valued entries.
/// Trailing units are stripped; anything unparseable is skipped rather than fatal.
pub fn parse_kv_kb(text: &str) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let num = v.split_whitespace().next().unwrap_or("");
        if let Ok(n) = num.parse::<u64>() {
            m.insert(k.trim().to_string(), n);
        }
    }
    m
}

/// Parse a whitespace-separated `key value` file such as /proc/vmstat.
pub fn parse_kv_space(text: &str) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            if let Ok(n) = v.parse::<u64>() {
                m.insert(k.to_string(), n);
            }
        }
    }
    m
}

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Psi {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub some_total_us: u64,
    pub full_total_us: u64,
}

/// Parse a /proc/pressure/* file.
///
/// PSI is the best cheap signal for "is the machine actually struggling", because it
/// measures stalled time rather than utilisation: a machine can be 100 % busy and fine,
/// or 30 % busy and thrashing.
pub fn parse_psi(text: &str) -> Option<Psi> {
    let mut p = Psi::default();
    let mut saw = false;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let which = it.next()?;
        let mut vals = HashMap::new();
        for tok in it {
            if let Some((k, v)) = tok.split_once('=') {
                if let Ok(f) = v.parse::<f64>() {
                    vals.insert(k, f);
                }
            }
        }
        let g = |k: &str| vals.get(k).copied().unwrap_or(0.0);
        match which {
            "some" => {
                saw = true;
                p.some_avg10 = g("avg10");
                p.some_avg60 = g("avg60");
                p.some_avg300 = g("avg300");
                p.some_total_us = g("total") as u64;
            }
            "full" => {
                p.full_avg10 = g("avg10");
                p.full_avg60 = g("avg60");
                p.full_total_us = g("total") as u64;
            }
            _ => {}
        }
    }
    saw.then_some(p)
}

/// Aggregate jiffy counters from a /proc/stat `cpu` line.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
    /// Idle includes iowait: a CPU waiting on disk is not executing work.
    pub fn idle_all(&self) -> u64 {
        self.idle + self.iowait
    }
    /// Busy fraction between two snapshots, 0..=100. `None` if the counters did not
    /// advance or went backwards (which happens across a CPU hotplug).
    pub fn busy_pct(&self, prev: &CpuTimes) -> Option<f64> {
        let dt = self.total().checked_sub(prev.total())?;
        let di = self.idle_all().checked_sub(prev.idle_all())?;
        if dt == 0 {
            return None;
        }
        Some((100.0 * (dt.saturating_sub(di)) as f64 / dt as f64).clamp(0.0, 100.0))
    }
}

/// Parse /proc/stat. Returns (aggregate, per-cpu in index order, extra counters).
pub fn parse_proc_stat(text: &str) -> (CpuTimes, Vec<CpuTimes>, HashMap<String, u64>) {
    let mut agg = CpuTimes::default();
    let mut per = Vec::new();
    let mut extra = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(tag) = it.next() else { continue };
        if tag.starts_with("cpu") {
            let n: Vec<u64> = it.take(8).filter_map(|x| x.parse().ok()).collect();
            if n.len() < 4 {
                continue;
            }
            let g = |i: usize| n.get(i).copied().unwrap_or(0);
            let t = CpuTimes {
                user: g(0), nice: g(1), system: g(2), idle: g(3),
                iowait: g(4), irq: g(5), softirq: g(6), steal: g(7),
            };
            if tag == "cpu" { agg = t } else { per.push(t) }
        } else if let Some(v) = it.next().and_then(|x| x.parse::<u64>().ok()) {
            extra.insert(tag.to_string(), v);
        }
    }
    (agg, per, extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jamsys-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(p: &Path, s: &str) {
        if let Some(par) = p.parent() {
            fs::create_dir_all(par).unwrap();
        }
        let mut f = fs::File::create(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn missing_files_are_none_not_errors() {
        assert_eq!(read_str("/definitely/not/here"), None);
        assert_eq!(read_i64("/definitely/not/here"), None);
        assert_eq!(read_checked("/definitely/not/here", 0.0, 1.0), None);
        assert!(list_dir("/definitely/not/here").is_empty());
    }

    #[test]
    fn empty_and_garbage_files_are_none() {
        let d = tmpdir("garbage");
        write(&d.join("empty"), "");
        write(&d.join("blank"), "   \n");
        write(&d.join("text"), "not-a-number");
        write(&d.join("huge"), "99999999999999999999999999");
        assert_eq!(read_str(d.join("empty")), None);
        assert_eq!(read_str(d.join("blank")), None);
        assert_eq!(read_i64(d.join("text")), None);
        assert_eq!(read_i64(d.join("huge")), None, "overflow must not panic");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn out_of_range_sensor_values_are_rejected() {
        let d = tmpdir("range");
        // A real i915/coretemp sentinel for "no reading".
        write(&d.join("temp1_input"), "-274000");
        write(&d.join("temp2_input"), "58000");
        assert_eq!(read_scaled(d.join("temp1_input"), 1000.0, -40.0, 150.0), None);
        assert_eq!(read_scaled(d.join("temp2_input"), 1000.0, -40.0, 150.0), Some(58.0));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hwmon_is_keyed_by_name_and_label_not_index() {
        let d = tmpdir("hwmon");
        // Same sensor set, but the NVMe controller has moved from hwmon4 to hwmon6,
        // exactly as happens across a kernel upgrade.
        for (idx, name) in [("hwmon6", "coretemp"), ("hwmon9", "nvme")] {
            let h = d.join(idx);
            write(&h.join("name"), name);
        }
        write(&d.join("hwmon6/temp1_input"), "56000");
        write(&d.join("hwmon6/temp1_label"), "Package id 0");
        write(&d.join("hwmon6/temp1_crit"), "100000");
        write(&d.join("hwmon9/temp1_input"), "33850");
        write(&d.join("hwmon9/temp1_label"), "Composite");

        let chans = discover_hwmon_in(d.to_str().unwrap(), &[HwmonKind::Temp]);
        let nvme = chans.iter().find(|c| c.key() == "nvme/Composite").expect("nvme channel");
        assert_eq!(nvme.read(), Some(33.85));
        let pkg = chans.iter().find(|c| c.key() == "coretemp/Package id 0").expect("pkg channel");
        assert_eq!(pkg.crit, Some(100.0));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hwmon_ignores_non_input_attributes() {
        let d = tmpdir("hwmon2");
        write(&d.join("hwmon0/name"), "asus");
        write(&d.join("hwmon0/fan1_input"), "2300");
        write(&d.join("hwmon0/fan1_label"), "cpu_fan");
        write(&d.join("hwmon0/pwm1_enable"), "2");
        write(&d.join("hwmon0/fan1_min"), "0");
        let chans = discover_hwmon_in(d.to_str().unwrap(), &[HwmonKind::Fan]);
        assert_eq!(chans.len(), 1, "only fan1_input is a channel");
        assert_eq!(chans[0].read(), Some(2300.0));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn natural_order_puts_cpu2_before_cpu10() {
        let mut v = vec!["cpu10", "cpu2", "cpu1", "hwmon9", "hwmon10"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["cpu1", "cpu2", "cpu10", "hwmon9", "hwmon10"]);
    }

    #[test]
    fn psi_parses_real_kernel_output() {
        let t = "some avg10=0.00 avg60=0.01 avg300=0.00 total=12266162\n\
                 full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let p = parse_psi(t).unwrap();
        assert_eq!(p.some_avg60, 0.01);
        assert_eq!(p.some_total_us, 12_266_162);
        assert_eq!(p.full_total_us, 0);
    }

    #[test]
    fn psi_absent_is_none_not_a_panic() {
        assert!(parse_psi("").is_none());
        assert!(parse_psi("garbage\n").is_none());
    }

    #[test]
    fn proc_stat_parses_and_computes_busy() {
        let t = "cpu  100 0 50 850 0 0 0 0\ncpu0 50 0 25 425 0 0 0 0\ncpu1 50 0 25 425 0 0 0 0\n\
                 ctxt 12345\nprocesses 678\nprocs_running 2\n";
        let (agg, per, extra) = parse_proc_stat(t);
        assert_eq!(per.len(), 2);
        assert_eq!(extra["ctxt"], 12345);
        assert_eq!(extra["procs_running"], 2);
        let t2 = "cpu  200 0 100 1700 0 0 0 0\n";
        let (agg2, _, _) = parse_proc_stat(t2);
        // 150 busy jiffies of 1000 elapsed.
        assert_eq!(agg2.busy_pct(&agg), Some(15.0));
    }

    #[test]
    fn counters_going_backwards_do_not_panic() {
        let hi = CpuTimes { user: 1000, idle: 1000, ..Default::default() };
        let lo = CpuTimes { user: 10, idle: 10, ..Default::default() };
        // CPU hotplug can reset counters; must be None, not a subtract overflow.
        assert_eq!(lo.busy_pct(&hi), None);
    }

    #[test]
    fn meminfo_parses_with_units() {
        let m = parse_kv_kb("MemTotal:       31926304 kB\nMemFree: 22964184 kB\nHugePages_Total: 0\n");
        assert_eq!(m["MemTotal"], 31_926_304);
        assert_eq!(m["HugePages_Total"], 0);
    }
}
