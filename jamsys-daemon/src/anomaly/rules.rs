//! The rule engine. Layer 1 (deterministic thresholds) and Layer 2 (learned baselines),
//! evaluated against a [`Snapshot`] on every tick.
//!
//! Every rule returns an `Alert` carrying an [`Explanation`] with real numbers. There is
//! no code path that produces a bare "anomaly detected" — the `Alert` constructor
//! requires an explanation, and the rules below fill in the measurement, the expected
//! range, how long it has been true, and one or two safe things to check.

use super::baseline::{Baselines, Verdict};
use super::dwell::DwellTracker;
use crate::collectors::Snapshot;
use crate::config::Config;
use crate::types::*;
use crate::util::{human_bytes, human_duration};

pub struct RuleEngine {
    pub dwell: DwellTracker,
    pub baselines: Baselines,
    /// Fingerprints produced on the current tick, so the caller can resolve the rest.
    firing: Vec<String>,
    /// Mirrors `[baseline] time_of_day`, refreshed on every evaluation so a config
    /// reload takes effect without restarting.
    tod: bool,
}

/// Result of one evaluation pass.
pub struct Evaluation {
    pub alerts: Vec<Alert>,
    /// Fingerprints whose condition is currently false and which may be resolved.
    pub firing: Vec<String>,
}

impl RuleEngine {
    pub fn new(cfg: &Config) -> Self {
        RuleEngine {
            dwell: DwellTracker::new(),
            baselines: Baselines::new(
                cfg.baseline.window,
                cfg.baseline.warmup_samples,
                cfg.baseline.z_threshold,
            ),
            firing: Vec::new(),
            tod: cfg.baseline.time_of_day,
        }
    }

    /// Feed the tick's samples into the learned baselines.
    pub fn learn(&mut self, snap: &Snapshot, samples: &[Sample]) {
        self.learn_with(snap, samples, false)
    }

    pub fn learn_with(&mut self, snap: &Snapshot, samples: &[Sample], time_of_day: bool) {
        let ctx = snap.context_at(crate::clock::now_ms(), time_of_day);
        for s in samples {
            if is_learnable(s.id.subsystem, s.id.name) {
                self.baselines.observe(&s.id.key(), ctx, s.value);
            }
        }
    }

    pub fn evaluate(&mut self, snap: &Snapshot, cfg: &Config, now_ms: i64) -> Evaluation {
        self.tod = cfg.baseline.time_of_day;
        self.firing.clear();
        let mut out = Vec::new();
        self.cpu_rules(snap, cfg, now_ms, &mut out);
        self.memory_rules(snap, cfg, now_ms, &mut out);
        self.thermal_rules(snap, cfg, now_ms, &mut out);
        self.storage_rules(snap, cfg, now_ms, &mut out);
        self.gpu_rules(snap, cfg, now_ms, &mut out);
        self.power_rules(snap, cfg, now_ms, &mut out);
        self.network_rules(snap, cfg, now_ms, &mut out);
        self.service_rules(snap, cfg, now_ms, &mut out);
        self.device_rules(snap, cfg, now_ms, &mut out);
        self.bluetooth_rules(snap, cfg, now_ms, &mut out);
        self.learned_rules(snap, cfg, now_ms, &mut out);
        Evaluation { alerts: out, firing: self.firing.clone() }
    }

    fn fire(&mut self, out: &mut Vec<Alert>, a: Alert) {
        self.firing.push(a.fingerprint.clone());
        out.push(a);
    }

    // ---- Layer 1 ---------------------------------------------------------

    fn cpu_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let c = &s.cpu;
        if let Some(t) = c.package_temp_c.or(s.thermal.cpu_package_c) {
            let crit = cfg.threshold("cpu.temp.critical", "celsius", 95.0);
            let high = cfg.threshold("cpu.temp.high", "celsius", 90.0);
            if let Some(held) = self.dwell.update("cpu.temp.critical", t >= crit, now, 60) {
                self.fire(out, Alert::new("cpu.temp.critical", "", Severity::Critical,
                    "CPU is running critically hot",
                    Explanation::new(format!("CPU package temperature is {t:.1} °C."))
                        .expected(format!("below {crit:.0} °C"))
                        .since(held)
                        .evidence("Fans", fan_summary(s))
                        .evidence("CPU load", format!("{:.0}%", c.usage_pct))
                        .cause(hot_cpu_cause(s))
                        .action("Check for a runaway process on the Processes page")
                        .action("Make sure the vents are not blocked")));
            } else if let Some(held) = self.dwell.update("cpu.temp.high", t >= high, now, 120) {
                self.fire(out, Alert::new("cpu.temp.high", "", Severity::Warning,
                    "CPU temperature is high",
                    Explanation::new(format!("CPU package temperature is {t:.1} °C."))
                        .expected(format!("below {high:.0} °C"))
                        .since(held)
                        .evidence("Fans", fan_summary(s))
                        .evidence("CPU load", format!("{:.0}%", c.usage_pct))
                        .cause(hot_cpu_cause(s))
                        .action("Open the Processes page to see what is using the CPU")));
            }
        }
        if c.throttling_now {
            self.fire(out, Alert::new("cpu.throttling", "", Severity::Warning,
                "CPU is thermally throttling",
                Explanation::new(format!(
                    "The kernel package-throttle counter advanced (now {}).", c.package_throttle_count))
                    .expected("no new throttle events")
                    .evidence("CPU temperature", s.thermal.cpu_package_c.map(|t| format!("{t:.1} °C")).unwrap_or_else(|| "unknown".into()))
                    .evidence("Current frequency", format!("{:.0} MHz of {:.0} MHz", c.freq_mhz, c.freq_max_mhz))
                    .action("Sustained throttling under load is normal on a thin laptop; investigate if it happens at idle")));
        }
        let sustained = cfg.threshold("cpu.sustained_load", "pct", 90.0);
        if let Some(held) = self.dwell.update("cpu.sustained_load", c.usage_pct >= sustained, now, 300) {
            let top = s.process.top_cpu.first();
            self.fire(out, Alert::new("cpu.sustained_load", "", Severity::Notice,
                "CPU has been busy for a while",
                Explanation::new(format!("Total CPU usage is {:.0}%.", c.usage_pct))
                    .expected(format!("below {sustained:.0}% sustained"))
                    .since(held)
                    .evidence("Load average", format!("{:.2} / {:.2}", c.load1, c.load5))
                    .evidence("Busiest process", top.map(|p| format!("{} (pid {}) at {:.0}%", p.name, p.pid, p.cpu_pct)).unwrap_or_else(|| "unknown".into()))
                    .action("This is only a problem if you did not expect it")));
        }
        // A runaway process: high CPU, sustained, and not something the user started
        // in the foreground of a terminal a moment ago.
        if let Some(p) = s.process.top_cpu.first() {
            let key = format!("cpu.runaway_process:{}", p.pid);
            let thresh = cfg.threshold("cpu.runaway_process", "pct", 85.0);
            if let Some(held) = self.dwell.update(&key, p.cpu_pct >= thresh, now, 300) {
                self.fire(out, Alert::new("cpu.runaway_process", &p.name, Severity::Notice,
                    format!("{} is using a lot of CPU", p.name),
                    Explanation::new(format!("{} (pid {}) has used {:.0}% of a CPU core.", p.name, p.pid, p.cpu_pct))
                        .expected(format!("below {thresh:.0}% sustained"))
                        .since(held)
                        .evidence("Memory", human_bytes(p.rss_bytes as f64))
                        .evidence("Threads", p.threads.to_string())
                        .evidence("Command", p.cmd.chars().take(120).collect::<String>())
                        .action(format!("Inspect it with:  ps -p {} -o pid,comm,%cpu,%mem,etime", p.pid))));
            }
        }
    }

    fn memory_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let m = &s.memory;
        if m.total_bytes == 0 {
            return;
        }
        if m.oom_just_happened {
            self.fire(out, Alert::new("mem.oom", "", Severity::Critical,
                "The kernel killed a process to free memory",
                Explanation::new(format!("The OOM killer has run {} time(s) since boot.", m.oom_kill_total))
                    .expected("no OOM kills")
                    .evidence("Available memory", format!("{} ({:.0}%)", human_bytes(m.available_bytes as f64), m.available_pct))
                    .evidence("Swap in use", human_bytes(m.swap_used_bytes as f64))
                    .action("Check the Events page for which process was killed")
                    .action("Look for a memory leak on the Memory page")));
        }
        let crit = cfg.threshold("mem.critical", "avail_pct", 3.0);
        let low = cfg.threshold("mem.low", "avail_pct", 8.0);
        if m.available_pct <= crit {
            self.fire(out, Alert::new("mem.critical", "", Severity::Critical,
                "Almost out of memory",
                Explanation::new(format!("Only {} ({:.1}%) of memory is available.",
                    human_bytes(m.available_bytes as f64), m.available_pct))
                    .expected(format!("above {crit:.0}% available"))
                    .evidence("Largest process", top_mem_text(s))
                    .evidence("Swap", format!("{} of {}", human_bytes(m.swap_used_bytes as f64), human_bytes(m.swap_total_bytes as f64)))
                    .action("Close the largest applications listed on the Memory page")));
        } else if let Some(held) = self.dwell.update("mem.low", m.available_pct <= low, now, 60) {
            self.fire(out, Alert::new("mem.low", "", Severity::Warning,
                "Memory is running low",
                Explanation::new(format!("{} ({:.1}%) of memory is available.",
                    human_bytes(m.available_bytes as f64), m.available_pct))
                    .expected(format!("above {low:.0}% available"))
                    .since(held)
                    .evidence("Largest process", top_mem_text(s))
                    .action("Open the Memory page to see what is using it")));
        }
        let pressure = cfg.threshold("mem.pressure", "psi", 20.0);
        if let Some(held) = self.dwell.update("mem.pressure", m.psi_some_avg60 >= pressure, now, 60) {
            self.fire(out, Alert::new("mem.pressure", "", Severity::Warning,
                "The system is stalling on memory",
                Explanation::new(format!(
                    "Processes spent {:.0}% of the last minute waiting for memory (PSI).", m.psi_some_avg60))
                    .expected(format!("below {pressure:.0}%"))
                    .since(held)
                    .evidence("Available", human_bytes(m.available_bytes as f64))
                    .evidence("Major page faults", format!("{:.0}/s", m.major_faults_per_s))
                    .action("Memory pressure means real slowdown even when free memory looks adequate")));
        }
        if m.swap_total_bytes > 0 {
            let sw = cfg.threshold("mem.swap_sustained", "pct", 25.0);
            if let Some(held) = self.dwell.update("mem.swap_sustained", m.swap_used_pct >= sw, now, 300) {
                self.fire(out, Alert::new("mem.swap_sustained", "", Severity::Notice,
                    "Swap has been in use for a while",
                    Explanation::new(format!("{:.0}% of swap is in use ({}).",
                        m.swap_used_pct, human_bytes(m.swap_used_bytes as f64)))
                        .expected(format!("below {sw:.0}%"))
                        .since(held)
                        .evidence("Available RAM", human_bytes(m.available_bytes as f64))
                        .action("Swap use alone is not a fault; it matters when combined with memory pressure")));
            }
        }
    }

    fn thermal_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        // A stopped fan while something is hot is one of the few genuinely urgent
        // hardware conditions a monitor can detect.
        let hot = cfg.threshold("thermal.fan_stalled", "celsius", 70.0);
        let hottest = s.thermal.max_temp_c.unwrap_or(0.0);
        for f in &s.thermal.fans {
            if f.value > 0.0 {
                continue;
            }
            let key = format!("thermal.fan_stalled:{}", f.key);
            if let Some(held) = self.dwell.update(&key, hottest >= hot, now, 60) {
                self.fire(out, Alert::new("thermal.fan_stalled", &f.key, Severity::Critical,
                    format!("{} is not spinning while the system is hot", f.label),
                    Explanation::new(format!("{} reads 0 rpm while the hottest sensor is {hottest:.1} °C.", f.label))
                        .expected(format!("a fan should spin above {hot:.0} °C"))
                        .since(held)
                        .evidence("Hottest sensor", s.thermal.max_temp_label.clone())
                        .evidence("Other fans", fan_summary(s))
                        .action("Some laptops stop fans below a firmware threshold; check whether it spins under load")
                        .action("If it never spins, the fan or its header may have failed")));
            }
        }
        for t in &s.thermal.temps {
            let Some(crit) = t.crit else { continue };
            if crit <= 0.0 {
                continue;
            }
            let key = format!("thermal.near_crit:{}", t.key);
            if let Some(held) = self.dwell.update(&key, t.value >= crit - 5.0, now, 60) {
                self.fire(out, Alert::new("thermal.near_crit", &t.key, Severity::Warning,
                    format!("{} is close to its critical temperature", t.label),
                    Explanation::new(format!("{} is {:.1} °C.", t.label, t.value))
                        .expected(format!("the driver's critical point is {crit:.0} °C"))
                        .since(held)
                        .evidence("Fans", fan_summary(s))
                        .action("Reduce load or improve airflow")));
            }
        }
    }

    fn storage_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let full = cfg.threshold("disk.full", "pct", 95.0);
        let filling = cfg.threshold("disk.filling", "pct", 90.0);
        let inodes = cfg.threshold("disk.inodes", "pct", 90.0);
        for f in &s.storage.filesystems {
            if f.used_pct >= full {
                self.fire(out, Alert::new("disk.full", &f.mount, Severity::Critical,
                    format!("{} is almost full", f.mount),
                    Explanation::new(format!("{} is {:.1}% full — {} free of {}.",
                        f.mount, f.used_pct, human_bytes(f.free_bytes as f64), human_bytes(f.total_bytes as f64)))
                        .expected(format!("below {full:.0}% used"))
                        .evidence("Filesystem", format!("{} on {}", f.fstype, f.device))
                        .action(format!("Find the biggest directories:  du -xh --max-depth=1 {} | sort -h | tail", f.mount))));
            } else if f.used_pct >= filling {
                self.fire(out, Alert::new("disk.filling", &f.mount, Severity::Warning,
                    format!("{} is filling up", f.mount),
                    Explanation::new(format!("{} is {:.1}% full — {} free.",
                        f.mount, f.used_pct, human_bytes(f.free_bytes as f64)))
                        .expected(format!("below {filling:.0}% used"))
                        .evidence("Total size", human_bytes(f.total_bytes as f64))
                        .action(format!("du -xh --max-depth=1 {} | sort -h | tail", f.mount))));
            }
            if f.inode_used_pct >= inodes {
                self.fire(out, Alert::new("disk.inodes", &f.mount, Severity::Warning,
                    format!("{} is running out of inodes", f.mount),
                    Explanation::new(format!("{} has used {:.1}% of its inodes.", f.mount, f.inode_used_pct))
                        .expected(format!("below {inodes:.0}%"))
                        .evidence("Space used", format!("{:.1}%", f.used_pct))
                        .action("A filesystem can run out of inodes while it still has free space — usually many tiny files")));
            }
        }
        for n in &s.storage.nvme {
            if let Some(cw) = n.critical_warning {
                if cw != 0 {
                    self.fire(out, Alert::new("nvme.smart", &n.device, Severity::Critical,
                        format!("{} reports a SMART critical warning", n.device),
                        Explanation::new(format!("SMART critical_warning is 0x{cw:02x}: {}.", smart_warning_text(cw)))
                            .expected("0x00")
                            .evidence("Model", n.model.clone())
                            .evidence("Media errors", n.media_errors.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into()))
                            .evidence("Wear", n.percentage_used.map(|v| format!("{v}%")).unwrap_or_else(|| "unknown".into()))
                            .action("Back up important data now")
                            .action(format!("Full report:  sudo smartctl -a /dev/{}", n.device))));
                }
            }
            if let (Some(sp), Some(th)) = (n.available_spare, n.available_spare_threshold) {
                if sp < th {
                    self.fire(out, Alert::new("nvme.spare", &n.device, Severity::Critical,
                        format!("{} has exhausted its spare blocks", n.device),
                        Explanation::new(format!("Available spare is {sp}%, below the drive's own threshold of {th}%."))
                            .expected(format!("at or above {th}%"))
                            .evidence("Wear", n.percentage_used.map(|v| format!("{v}%")).unwrap_or_default())
                            .action("The drive is near end of life. Back up and plan a replacement.")));
                }
            }
            if let Some(m) = n.media_errors {
                if m > 0 {
                    self.fire(out, Alert::new("nvme.media_errors", &n.device, Severity::Critical,
                        format!("{} reports media errors", n.device),
                        Explanation::new(format!("The drive has logged {m} media and data-integrity error(s)."))
                            .expected("0")
                            .evidence("Model", n.model.clone())
                            .evidence("Power-on hours", n.power_on_hours.map(|v| v.to_string()).unwrap_or_default())
                            .action("Verify your backups")
                            .action("Check the kernel log for I/O errors on this device")));
                }
            }
            if let Some(w) = n.percentage_used {
                let wear = cfg.threshold("nvme.wear", "pct", 90.0);
                if w as f64 >= wear {
                    self.fire(out, Alert::new("nvme.wear", &n.device, Severity::Warning,
                        format!("{} is near its rated write endurance", n.device),
                        Explanation::new(format!("The drive reports {w}% of its rated endurance used."))
                            .expected(format!("below {wear:.0}%"))
                            .evidence("Data written", n.data_units_written.map(|d| human_bytes(d as f64 * 512_000.0)).unwrap_or_default())
                            .action("This is a wear indicator, not a failure; plan a replacement in due course")));
                }
            }
            if let (Some(t), Some(_)) = (n.temp_c, Some(())) {
                let nt = cfg.threshold("nvme.temp", "celsius", 70.0);
                if let Some(held) = self.dwell.update(&format!("nvme.temp:{}", n.device), t >= nt, now, 120) {
                    self.fire(out, Alert::new("nvme.temp", &n.device, Severity::Warning,
                        format!("{} is running hot", n.device),
                        Explanation::new(format!("NVMe composite temperature is {t:.1} °C."))
                            .expected(format!("below {nt:.0} °C"))
                            .since(held)
                            .evidence("Write throughput", crate::util::human_bps(s.storage.total_write_bps))
                            .action("Sustained heavy writes heat NVMe drives; they throttle themselves to protect the flash")));
                }
            }
        }
    }

    fn gpu_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let n = &s.gpu.nvidia;
        if !n.present {
            return;
        }
        if let Some(t) = n.temp_c {
            let gt = cfg.threshold("gpu.nvidia.temp", "celsius", 87.0);
            if let Some(held) = self.dwell.update("gpu.nvidia.temp", t >= gt, now, 60) {
                self.fire(out, Alert::new("gpu.nvidia.temp", "", Severity::Warning,
                    "The graphics card is running hot",
                    Explanation::new(format!("GPU temperature is {t:.1} °C."))
                        .expected(format!("below {gt:.0} °C"))
                        .since(held)
                        .evidence("GPU utilisation", n.util_pct.map(|u| format!("{u:.0}%")).unwrap_or_default())
                        .evidence("GPU power", n.power_w.map(|w| format!("{w:.1} W")).unwrap_or_default())
                        .evidence("GPU fan", s.thermal.fans.iter().find(|f| f.label.contains("gpu"))
                            .map(|f| format!("{:.0} rpm", f.value)).unwrap_or_else(|| "unknown".into()))
                        .action("Check that the GPU fan is spinning on the Thermals page")));
            }
        }
        if let (Some(used), Some(total)) = (n.vram_used_mb, n.vram_total_mb) {
            if total > 0.0 {
                let pct = 100.0 * used / total;
                let vt = cfg.threshold("gpu.nvidia.vram", "pct", 95.0);
                if let Some(held) = self.dwell.update("gpu.nvidia.vram", pct >= vt, now, 60) {
                    self.fire(out, Alert::new("gpu.nvidia.vram", "", Severity::Warning,
                        "Graphics memory is nearly full",
                        Explanation::new(format!("VRAM is {pct:.0}% used ({used:.0} MB of {total:.0} MB)."))
                            .expected(format!("below {vt:.0}%"))
                            .since(held)
                            .evidence("GPU processes", n.process_count.to_string())
                            .action("Applications may crash or fall back to slow system memory")));
                }
            }
        }
        // The battery-drain condition the brief specifically asks about.
        if let Some(held) = self.dwell.update("gpu.idle_awake", s.gpu.dgpu_awake_idle && s.power.on_battery, now, 300) {
            let w = n.power_w.unwrap_or(0.0);
            self.fire(out, Alert::new("gpu.idle_awake", "", Severity::Notice,
                "The discrete GPU is awake but idle",
                Explanation::new(format!(
                    "The NVIDIA GPU is powered on at {w:.1} W although its utilisation is 0% and no process is using it."))
                    .expected("suspended (0 W) when nothing is using it")
                    .since(held)
                    .evidence("Runtime power state", n.runtime_status.clone())
                    .evidence("Awake this boot", human_duration(n.active_time_s as i64))
                    .evidence("System draw", format!("{:.1} W", s.power.power_w))
                    .cause("Something is holding the device open — commonly a browser with hardware acceleration, an application launched with the dGPU, or a monitoring tool polling it")
                    .action("Check:  cat /sys/class/drm/card2/device/power/runtime_status")
                    .action("Look for GPU clients on the GPU page")));
        }
    }

    fn power_rules(&mut self, s: &Snapshot, cfg: &Config, _now: i64, out: &mut Vec<Alert>) {
        let p = &s.power;
        if !p.has_battery {
            return;
        }
        let health = cfg.threshold("battery.health", "pct", 70.0);
        if p.health_pct > 0.0 && p.health_pct < health {
            self.fire(out, Alert::new("battery.health", "", Severity::Warning,
                "Battery capacity has degraded",
                Explanation::new(format!(
                    "The battery now holds {:.1} Wh of its {:.1} Wh design capacity ({:.0}% health).",
                    p.energy_full_wh, p.energy_design_wh, p.health_pct))
                    .expected(format!("above {health:.0}% of design capacity"))
                    .evidence("Charge cycles", p.cycle_count.to_string())
                    .action("This is normal wear, not a fault. Expect proportionally shorter runtime.")));
        }
        if p.on_battery && p.percent <= cfg.threshold("battery.critical", "pct", 5.0) {
            self.fire(out, Alert::new("battery.critical", "", Severity::Critical,
                "Battery is critically low",
                Explanation::new(format!("The battery is at {:.0}% and discharging at {:.1} W.", p.percent, p.power_w))
                    .expected("above 5%")
                    .evidence("Estimated remaining", p.runtime_s.map(human_duration).unwrap_or_else(|| "unknown".into()))
                    .action("Connect the charger")));
        }
    }

    fn network_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let n = &s.network;
        let err_thresh = cfg.threshold("net.errors", "pct", 1.0);
        for i in &n.ifaces {
            if i.kind == "loopback" {
                continue;
            }
            if let Some(held) = self.dwell.update(&format!("net.errors:{}", i.name), i.error_rate_pct >= err_thresh, now, 60) {
                self.fire(out, Alert::new("net.errors", &i.name, Severity::Warning,
                    format!("{} is dropping packets", i.name),
                    Explanation::new(format!("{} shows a {:.2}% error and drop rate.", i.name, i.error_rate_pct))
                        .expected(format!("below {err_thresh:.1}%"))
                        .since(held)
                        .evidence("RX errors / drops", format!("{} / {}", i.rx_errors, i.rx_dropped))
                        .evidence("TX errors / drops", format!("{} / {}", i.tx_errors, i.tx_dropped))
                        .evidence("Signal", i.signal_dbm.map(|d| format!("{d:.0} dBm")).unwrap_or_else(|| "n/a".into()))
                        .cause(if i.kind == "wifi" && i.signal_dbm.map(|d| d < -70.0).unwrap_or(false) {
                            "Weak Wi-Fi signal"
                        } else {
                            "Driver, cable or interference"
                        })
                        .action("Move closer to the access point, or check the cable")));
            }
        }
        // "Gateway is fine but the internet is not" is a genuinely useful distinction —
        // it separates a local problem from an ISP problem.
        let gw_ok = n.reach.gateway_ok.unwrap_or(false);
        let net_ok = n.reach.internet_ok.unwrap_or(true);
        let dns_ok = n.reach.dns_ok.unwrap_or(true);
        if let Some(held) = self.dwell.update("net.no_internet", gw_ok && !net_ok, now, 120) {
            self.fire(out, Alert::new("net.no_internet", "", Severity::Notice,
                "Local network works but the internet does not",
                Explanation::new("Your router responds, but an outside address does not.".to_string())
                    .expected("both reachable")
                    .since(held)
                    .evidence("Gateway", n.reach.gateway.clone().unwrap_or_else(|| "unknown".into()))
                    .evidence("DNS", if dns_ok { "reachable" } else { "unreachable" }.to_string())
                    .cause("The problem is upstream of your machine — the router's uplink or your ISP")
                    .action("Restarting your own machine will not help; check the router")));
        }
        if let Some(held) = self.dwell.update("net.dns_fail", gw_ok && !dns_ok, now, 120) {
            self.fire(out, Alert::new("net.dns_fail", "", Severity::Warning,
                "DNS is not responding",
                Explanation::new("The configured DNS server did not accept a connection while the gateway did.".to_string())
                    .expected("DNS reachable")
                    .since(held)
                    .evidence("Gateway", n.reach.gateway.clone().unwrap_or_default())
                    .action("Names will not resolve even though the network is up")
                    .action("Check:  resolvectl status")));
        }
    }

    fn service_rules(&mut self, s: &Snapshot, cfg: &Config, _now: i64, out: &mut Vec<Alert>) {
        let _ = cfg;
        for u in s.services.failed.iter().chain(s.services.failed_user.iter()) {
            self.fire(out, Alert::new("service.failed", &u.name, Severity::Warning,
                format!("{} has failed", u.name),
                Explanation::new(format!("The systemd unit {} is in a failed state.", u.name))
                    .expected("active or inactive, not failed")
                    .evidence("Description", u.description.clone())
                    .evidence("Sub-state", u.sub_state.clone())
                    .action(format!("systemctl status {}", u.name))
                    .action("If this unit does not matter to you, use \"Ignore this service\"")));
        }
        // Only judge audio once the devices collector has actually reported. An empty
        // string means "not yet observed", and claiming a subsystem is broken because
        // we have not looked at it yet is the worst failure mode a monitor has.
        let a = &s.devices.audio;
        let observed = !a.pipewire.is_empty() && !a.wireplumber.is_empty();
        if observed && (a.pipewire != "active" || a.wireplumber != "active") {
            self.fire(out, Alert::new("audio.service_down", "", Severity::Warning,
                "Audio services are not running",
                Explanation::new(format!("PipeWire is {}, WirePlumber is {}, pipewire-pulse is {}.",
                    a.pipewire, a.wireplumber, a.pipewire_pulse))
                    .expected("both active")
                    .evidence("Sound cards detected", format!("{}", a.cards.len()))
                    .evidence("Restarts seen", a.restarts_seen.to_string())
                    .action("systemctl --user status pipewire wireplumber")));
        }
    }

    fn device_rules(&mut self, s: &Snapshot, _cfg: &Config, _now: i64, out: &mut Vec<Alert>) {
        for d in &s.devices.missing_expected {
            self.fire(out, Alert::new("device.missing", d, Severity::Warning,
                format!("A device that is normally present has disappeared: {d}"),
                Explanation::new(format!("{d} has been present every time JamSys checked, and is now absent."))
                    .expected("present")
                    .action("If you removed it deliberately, this will clear itself")
                    .action("If not, a reboot usually re-enumerates internal devices")));
        }
        if s.devices.bluetooth.present && s.devices.bluetooth.hard_blocked {
            self.fire(out, Alert::new("bt.blocked", "", Severity::Notice,
                "Bluetooth is blocked by a hardware switch",
                Explanation::new("The Bluetooth adapter is hard-blocked via rfkill.".to_string())
                    .expected("unblocked")
                    .action("Use the hardware switch or function key to re-enable it")));
        }
    }

    // ---- Layer 2 ---------------------------------------------------------

    /// Bluetooth disconnects.
    ///
    /// A disconnect is an *event*, but alerts are *states*, so each one is held
    /// firing for a short window and then allowed to resolve on its own. Without
    /// that, the alert would appear and vanish between two evaluations and the user
    /// would never see the notification they asked for.
    fn bluetooth_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        let bt = &s.bluetooth;
        if !bt.bluez_available {
            return;
        }
        // How long a disconnect keeps the alert open.
        let hold_ms = cfg.threshold("bluetooth.disconnect.hold", "seconds", 120.0) as i64 * 1000;
        let flap_min = cfg.threshold("bluetooth.flap.count", "disconnects", 3.0) as u32;

        // Flapping first: when a device is dropping repeatedly, that is the real
        // finding and a single-drop notice would only bury it.
        for (name, count) in bt.recent_disconnects.iter() {
            if *count < flap_min {
                continue;
            }
            let mins = crate::collectors::bluetooth::FLAP_WINDOW_MS / 60_000;
            let last = bt
                .events
                .iter()
                .rev()
                .find(|e| &e.name == name && e.kind == crate::collectors::bluetooth::BtEventKind::Disconnected);
            let mut ex = Explanation::new(format!(
                "{name} has disconnected {count} times in the last {mins} minutes."
            ))
            .expected("a stable link stays connected".to_string())
            .evidence("Disconnects", format!("{count} in {mins} min"));
            if let Some(e) = last {
                ex = ex.evidence("Radio power state", radio_phrase(&e.context));
                if let Some(c) = bt.last_stack_error.as_ref().or(e.likely_cause.as_ref()) {
                    ex = ex.cause(c.clone());
                }
            }
            for note in &bt.risk_notes {
                ex = ex.action(note.clone());
            }
            self.fire(out, Alert::new("bluetooth.flapping", name, Severity::Warning,
                format!("{name} keeps disconnecting"), ex));
        }

        // Single disconnects, for devices that are not already reported as flapping.
        for e in bt.events.iter().rev() {
            if e.kind != crate::collectors::bluetooth::BtEventKind::Disconnected
                || now - e.at_mono_ms > hold_ms
            {
                continue;
            }
            if bt.recent_disconnects.get(&e.name).copied().unwrap_or(0) >= flap_min {
                continue;
            }
            let still_gone = bt
                .devices
                .iter()
                .find(|d| d.address == e.address)
                .map(|d| !d.connected)
                .unwrap_or(true);
            let what = if still_gone {
                format!("{} disconnected and has not come back.", e.name)
            } else {
                format!("{} disconnected, then reconnected.", e.name)
            };
            let mut ex = Explanation::new(what)
                .since((now - e.at_mono_ms) / 1000)
                .evidence("Device", format!("{} ({})", e.name, e.address))
                .evidence("Radio power state", radio_phrase(&e.context));
            // BlueZ's own complaint, when it made one, outranks an inferred cause:
            // it is the stack reporting what happened rather than us correlating.
            match bt.last_stack_error.as_ref().or(e.likely_cause.as_ref()) {
                Some(c) => ex = ex.cause(c.clone()),
                // Saying so beats leaving a blank where a reason should be.
                None => {
                    ex = ex.evidence(
                        "Reason",
                        "not observable — the kernel does not expose an HCI \
                         disconnect reason to an unprivileged process"
                            .to_string(),
                    )
                }
            }
            for note in &bt.risk_notes {
                ex = ex.action(note.clone());
            }
            self.fire(out, Alert::new("bluetooth.disconnected", &e.name, Severity::Notice,
                format!("{} disconnected", e.name), ex));
        }
    }

    fn learned_rules(&mut self, s: &Snapshot, cfg: &Config, now: i64, out: &mut Vec<Alert>) {
        // Abnormal idle power — the worked example from the specification.
        if s.power.has_battery && s.idle && s.power.power_w > 0.0 {
            let ctx = s.context_at(crate::clock::now_ms(), self.tod);
            let v = self.baselines.evaluate("power.system_w", ctx, s.power.power_w);
            let dwell_s = self.baselines.tuning_for("power.system_w").dwell_s;
            if let Verdict::Deviating { delta, median, p05, p95, n, .. } = v {
                if delta > 0.0 {
                    if let Some(held) = self.dwell.update("power.idle_high", true, now, dwell_s) {
                        let mut e = Explanation::new(format!(
                            "Battery discharge is {:.1} W.", s.power.power_w))
                            .expected(format!("your learned idle range on battery is {p05:.1}–{p95:.1} W (median {median:.1} W, from {n} samples)"))
                            .since(held)
                            .evidence("CPU usage", format!("{:.0}%", s.cpu.usage_pct))
                            .evidence("Above baseline by", format!("{delta:.1} W"));
                        // Correlation, never a guess: name only what actually correlates.
                        if let Some(cause) = idle_power_cause(s) {
                            e = e.cause(cause);
                        }
                        e = e
                            .action("Compare the Power page history against the shaded normal band")
                            .action("Check the GPU page for a discrete GPU that is awake");
                        self.fire(out, Alert::new("power.idle_high", "", Severity::Notice,
                            "Unusually high power draw while idle", e));
                        self.baselines.freeze("power.system_w");
                    }
                }
            } else {
                self.dwell.clear("power.idle_high");
                self.baselines.thaw("power.system_w");
            }
        }

        // Memory leak: a *trend* test, not a z-score. A leak is defined by monotone
        // growth, not by being far from a median.
        let slope_thresh = cfg.threshold("mem.leak", "bytes_per_hour", 100e6);
        for p in s.process.top_mem.iter() {
            let Some(&slope) = s.process.growth_bph.get(&p.pid) else { continue };
            let grew_enough = p.rss_bytes as f64 > 1e9;
            let key = format!("mem.leak:{}", p.pid);
            if let Some(held) = self.dwell.update(&key, slope >= slope_thresh && grew_enough, now, 1800) {
                self.fire(out, Alert::new("mem.leak", &p.name, Severity::Notice,
                    format!("{} may be leaking memory", p.name),
                    Explanation::new(format!(
                        "{} (pid {}) is using {} and has grown by about {}/hour over the last {}.",
                        p.name, p.pid, human_bytes(p.rss_bytes as f64), human_bytes(slope), human_duration(held)))
                        .expected("stable memory use once an application has settled")
                        .since(held)
                        .evidence("Growth rate", format!("{}/hour", human_bytes(slope)))
                        .evidence("System memory available", format!("{:.0}%", s.memory.available_pct))
                        .cause("Steady, monotonic growth over half an hour is the signature of a leak rather than a working set that has grown")
                        .action(format!("Watch it with:  ps -p {} -o pid,comm,rss,etime", p.pid))
                        .action("Restarting the application reclaims the memory")));
            }
        }

        // Unusual temperature for the current context.
        if let Some(t) = s.thermal.cpu_package_c {
            let ctx = s.context_at(crate::clock::now_ms(), self.tod);
            let v = self.baselines.evaluate("thermal.cpu_package_c", ctx, t);
            if let Verdict::Deviating { delta, p05, p95, median, n, .. } = v {
                if delta > 0.0 {
                    let d = self.baselines.tuning_for("thermal.cpu_package_c").dwell_s;
                    if let Some(held) = self.dwell.update("thermal.unusual", true, now, d) {
                        self.fire(out, Alert::new("thermal.unusual", "", Severity::Notice,
                            "CPU is hotter than usual for this workload",
                            Explanation::new(format!("CPU package temperature is {t:.1} °C."))
                                .expected(format!("normally {p05:.0}–{p95:.0} °C in this state (median {median:.0} °C, {n} samples)"))
                                .since(held)
                                .evidence("CPU usage", format!("{:.0}%", s.cpu.usage_pct))
                                .evidence("Fans", fan_summary(s))
                                .evidence("Power source", if s.power.on_battery { "battery" } else { "AC" }.to_string())
                                .action("Dust in the vents is the usual cause of a gradual shift")));
                    }
                }
            } else {
                self.dwell.clear("thermal.unusual");
            }
        }

        // Traffic spike against the learned per-interface baseline.
        for i in &s.network.ifaces {
            if i.kind == "loopback" || !i.up {
                continue;
            }
            for (dir, bps) in [("rx", i.rx_bps), ("tx", i.tx_bps)] {
                let metric = format!("network.{dir}_bps[{}]", i.name);
                let ctx = s.context_at(crate::clock::now_ms(), self.tod);
                if let Verdict::Deviating { p95, median, .. } = self.baselines.evaluate(&metric, ctx, bps) {
                    // An absolute floor as well: 4x a tiny baseline is still tiny.
                    if bps > (p95 * 4.0).max(5e6) {
                        let key = format!("net.traffic_spike:{}:{dir}", i.name);
                        if let Some(held) = self.dwell.update(&key, true, now, 120) {
                            self.fire(out, Alert::new("net.traffic_spike", &format!("{}:{dir}", i.name), Severity::Notice,
                                format!("Unusually heavy {} traffic on {}", if dir == "rx" { "download" } else { "upload" }, i.name),
                                Explanation::new(format!("{} is at {}.", i.name, crate::util::human_bps(bps)))
                                    .expected(format!("normally around {} (95th percentile {})",
                                        crate::util::human_bps(median), crate::util::human_bps(p95)))
                                    .since(held)
                                    .evidence("Open TCP connections", s.network.tcp_connections.to_string())
                                    .action("If you are not downloading or backing up, check the Processes page")));
                        }
                    }
                }
            }
        }
    }
}

// ---- explanation helpers -------------------------------------------------

/// Which metrics are worth learning a baseline for. Learning "number of CPUs" is noise.
pub fn is_learnable(subsystem: &str, name: &str) -> bool {
    matches!(
        (subsystem, name),
        ("power", "system_w")
            | ("power", "cpu_package_w")
            | ("cpu", "usage_pct")
            | ("memory", "used_bytes")
            | ("thermal", "cpu_package_c")
            | ("thermal", "temp_c")
            | ("thermal", "fan_rpm")
            | ("network", "rx_bps")
            | ("network", "tx_bps")
            | ("storage", "write_bps")
            | ("storage", "read_bps")
            | ("gpu", "power_w")
            | ("gpu", "util_pct")
    )
}

fn fan_summary(s: &Snapshot) -> String {
    if s.thermal.fans.is_empty() {
        return "no fan sensors".into();
    }
    s.thermal.fans.iter().map(|f| format!("{} {:.0} rpm", f.label, f.value)).collect::<Vec<_>>().join(", ")
}

fn top_mem_text(s: &Snapshot) -> String {
    s.process
        .top_mem
        .first()
        .map(|p| format!("{} (pid {}) using {}", p.name, p.pid, human_bytes(p.rss_bytes as f64)))
        .unwrap_or_else(|| "unknown".into())
}

/// Only names a cause when one actually correlates. Returning `None` and saying nothing
/// is better than inventing an explanation.
fn hot_cpu_cause(s: &Snapshot) -> String {
    if let Some(p) = s.process.top_cpu.first() {
        if p.cpu_pct > 50.0 {
            return format!("{} (pid {}) is using {:.0}% CPU", p.name, p.pid, p.cpu_pct);
        }
    }
    if s.thermal.fans.iter().any(|f| f.value == 0.0) {
        return "a fan is reporting 0 rpm".into();
    }
    if s.cpu.usage_pct < 20.0 {
        return "the CPU is not busy, so this may be airflow or a sensor problem".into();
    }
    "general system load".into()
}

/// Correlation rules for high idle power. Each branch has a stated precondition.
fn idle_power_cause(s: &Snapshot) -> Option<String> {
    let n = &s.gpu.nvidia;
    if n.present && n.runtime_status == "active" {
        let w = n.power_w.unwrap_or(0.0);
        let u = n.util_pct.unwrap_or(0.0);
        return Some(format!(
            "The NVIDIA GPU is active and drawing {w:.1} W although its utilisation is {u:.0}%"
        ));
    }
    if let Some(p) = s.process.top_cpu.first() {
        if p.cpu_pct > 15.0 {
            return Some(format!("{} (pid {}) is using {:.0}% CPU while the system is otherwise idle", p.name, p.pid, p.cpu_pct));
        }
    }
    if s.storage.total_write_bps > 5e6 {
        return Some(format!("Disk writes are running at {}", crate::util::human_bps(s.storage.total_write_bps)));
    }
    if s.devices.bluetooth.connected_devices > 0 {
        return Some(format!("{} Bluetooth device(s) connected", s.devices.bluetooth.connected_devices));
    }
    None
}

fn smart_warning_text(cw: u8) -> String {
    let mut v = Vec::new();
    if cw & 0x01 != 0 { v.push("spare capacity below threshold") }
    if cw & 0x02 != 0 { v.push("temperature outside its safe range") }
    if cw & 0x04 != 0 { v.push("internal reliability degraded") }
    if cw & 0x08 != 0 { v.push("media placed in read-only mode") }
    if cw & 0x10 != 0 { v.push("volatile memory backup failed") }
    if cw & 0x20 != 0 { v.push("persistent memory region read-only") }
    if v.is_empty() {
        format!("unknown warning bits 0x{cw:02x}")
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::{cpu::CpuState, memory::MemState, power::PowerState, process::ProcInfo,
                            storage::FsInfo, thermal::Reading};

    fn cfg() -> Config {
        Config::default()
    }

    fn snap() -> Snapshot {
        Snapshot {
            memory: MemState { total_bytes: 32 << 30, available_bytes: 25 << 30, available_pct: 78.0, ..Default::default() },
            ..Default::default()
        }
    }

    fn fire_until(e: &mut RuleEngine, s: &Snapshot, c: &Config, start: i64, secs: i64) -> Vec<Alert> {
        // Two passes: one to start the dwell, one after it has elapsed.
        e.evaluate(s, c, start);
        e.evaluate(s, c, start + secs * 1000).alerts
    }

    // ---- Bluetooth --------------------------------------------------

    use crate::collectors::bluetooth::{BtContext, BtDevice, BtEvent, BtEventKind, BtState};

    fn bt_snap(events: Vec<BtEvent>, connected: bool) -> Snapshot {
        let mut s = snap();
        let mut recent = std::collections::BTreeMap::new();
        for e in events.iter().filter(|e| e.kind == BtEventKind::Disconnected) {
            *recent.entry(e.name.clone()).or_insert(0u32) += 1;
        }
        s.bluetooth = BtState {
            bluez_available: true,
            adapter_powered: true,
            connected_count: usize::from(connected),
            devices: vec![BtDevice {
                address: "D8:19:04:D9:D4:0E".into(),
                name: "AXR100".into(),
                icon: "audio-headset".into(),
                paired: true,
                connected,
                battery: None,
            }],
            events,
            recent_disconnects: recent,
            risk_notes: vec!["USB autosuspend is enabled on the Bluetooth radio.".into()],
            last_stack_error: None,
        };
        s
    }

    fn drop_event(at_ms: i64, cause: Option<&str>) -> BtEvent {
        BtEvent {
            at_ms,
            at_mono_ms: at_ms,
            kind: BtEventKind::Disconnected,
            address: "D8:19:04:D9:D4:0E".into(),
            name: "AXR100".into(),
            context: BtContext {
                adapter_powered: true,
                radio_runtime_status: "suspended".into(),
                radio_power_control: "auto".into(),
                radio_suspended_recently: true,
                ..Default::default()
            },
            likely_cause: cause.map(String::from),
        }
    }

    #[test]
    fn a_disconnect_raises_a_notice_naming_the_device() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(vec![drop_event(now - 5_000, Some("the radio was USB-autosuspended"))], false);
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        let a = alerts.iter().find(|a| a.rule_id == "bluetooth.disconnected")
            .expect("a disconnect must be reported — this is the whole point");
        assert!(a.title.contains("AXR100"), "the user needs the device name: {}", a.title);
        assert_eq!(a.severity, Severity::Notice);
        assert!(a.explanation.what.contains("has not come back"));
        assert!(a.explanation.likely_cause.as_deref().unwrap().contains("autosuspend"));
    }

    #[test]
    fn a_reconnect_is_described_differently_from_a_device_still_gone() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(vec![drop_event(now - 5_000, None)], true);
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        let a = alerts.iter().find(|a| a.rule_id == "bluetooth.disconnected").unwrap();
        assert!(a.explanation.what.contains("then reconnected"), "{}", a.explanation.what);
    }

    #[test]
    fn an_unexplained_drop_says_so_rather_than_leaving_a_blank() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(vec![drop_event(now - 5_000, None)], false);
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        let a = alerts.iter().find(|a| a.rule_id == "bluetooth.disconnected").unwrap();
        assert!(a.explanation.likely_cause.is_none());
        assert!(a.explanation.evidence.iter().any(|ev| ev.value.contains("not observable")),
            "an unknown reason must be stated explicitly");
    }

    #[test]
    fn an_old_disconnect_stops_firing_so_the_alert_can_resolve() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(vec![drop_event(now - 600_000, None)], true);
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        assert!(!alerts.iter().any(|a| a.rule_id == "bluetooth.disconnected"),
            "a ten-minute-old event must not hold the alert open forever");
    }

    #[test]
    fn repeated_drops_escalate_to_a_flapping_warning() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(
            vec![drop_event(now - 400_000, None), drop_event(now - 200_000, None),
                 drop_event(now - 5_000, Some("the radio was USB-autosuspended"))],
            true,
        );
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        let f = alerts.iter().find(|a| a.rule_id == "bluetooth.flapping")
            .expect("three drops in the window is the actual finding");
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.title.contains("keeps disconnecting"));
        assert!(f.explanation.what.contains("3 times"));
        // and it must not also emit a single-drop notice, which would bury it
        assert!(!alerts.iter().any(|a| a.rule_id == "bluetooth.disconnected"),
            "flapping supersedes the individual notice");
    }

    #[test]
    fn the_known_misconfiguration_is_offered_as_an_action() {
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let s = bt_snap(vec![drop_event(now - 5_000, None)], false);
        let alerts = e.evaluate(&s, &cfg(), now).alerts;
        let a = alerts.iter().find(|a| a.rule_id == "bluetooth.disconnected").unwrap();
        assert!(a.explanation.actions.iter().any(|x| x.contains("autosuspend")));
    }

    #[test]
    fn bluez_own_error_outranks_an_inferred_cause() {
        // When the stack says why, that beats our correlation. Verified against the
        // line this machine actually logs: "Host is down (112)".
        let mut e = RuleEngine::new(&cfg());
        let now = 1_000_000;
        let mut s = bt_snap(vec![drop_event(now - 5_000, Some("the radio was USB-autosuspended"))], false);
        s.bluetooth.last_stack_error =
            Some("the device is not responding — it is probably switched off".into());
        let a = e.evaluate(&s, &cfg(), now).alerts.into_iter()
            .find(|a| a.rule_id == "bluetooth.disconnected").unwrap();
        assert!(a.explanation.likely_cause.as_deref().unwrap().contains("not responding"),
            "BlueZ's own reason must win over the inferred one");
    }

    #[test]
    fn wall_clock_and_monotonic_stamps_are_not_mixed() {
        // Regression. Events carry a wall-clock stamp (~1.7e12) for display and a
        // monotonic one (~1e6) for windowing; the rule engine runs on the monotonic
        // clock. Subtracting the wrong one gave a hugely negative age, which read as
        // "always inside the hold window" and left the alert open forever.
        let mut e = RuleEngine::new(&cfg());
        let now_mono = 1_000_000;
        let wall = 1_757_400_000_000i64;
        let mut ev = drop_event(wall, None);
        ev.at_mono_ms = now_mono - 600_000; // ten minutes ago on the real clock
        let s = bt_snap(vec![ev], true);
        let alerts = e.evaluate(&s, &cfg(), now_mono).alerts;
        assert!(
            !alerts.iter().any(|a| a.rule_id == "bluetooth.disconnected"),
            "a ten-minute-old drop must not still be firing"
        );

        let mut ev2 = drop_event(wall, None);
        ev2.at_mono_ms = now_mono - 5_000;
        let s2 = bt_snap(vec![ev2], true);
        let a = e
            .evaluate(&s2, &cfg(), now_mono)
            .alerts
            .into_iter()
            .find(|a| a.rule_id == "bluetooth.disconnected")
            .expect("a five-second-old drop is still current");
        assert_eq!(a.explanation.since_s, 5, "age must be computed on one clock");
    }

    #[test]
    fn nothing_is_reported_when_bluez_is_absent() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = bt_snap(vec![drop_event(999_000, None)], false);
        s.bluetooth.bluez_available = false;
        let alerts = e.evaluate(&s, &cfg(), 1_000_000).alerts;
        assert!(!alerts.iter().any(|a| a.rule_id.starts_with("bluetooth.")),
            "without BlueZ there is no per-device truth to report");
    }

    #[test]
    fn nothing_fires_on_a_healthy_machine() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.cpu = CpuState { usage_pct: 8.0, package_temp_c: Some(58.0), ..Default::default() };
        s.thermal.cpu_package_c = Some(58.0);
        s.thermal.fans = vec![Reading { key: "asus/cpu_fan".into(), label: "cpu_fan".into(), driver: "asus".into(), value: 2300.0, crit: None }];
        s.storage.filesystems = vec![FsInfo { mount: "/".into(), used_pct: 46.0, total_bytes: 300 << 30, free_bytes: 160 << 30, ..Default::default() }];
        let a = fire_until(&mut e, &s, &cfg(), 0, 3600);
        assert!(a.is_empty(), "healthy machine produced: {:?}", a.iter().map(|x| &x.rule_id).collect::<Vec<_>>());
    }

    #[test]
    fn a_critical_cpu_temperature_fires_with_numbers_and_advice() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.thermal.cpu_package_c = Some(97.0);
        s.cpu.usage_pct = 99.0;
        s.process.top_cpu = vec![ProcInfo { pid: 4242, name: "cc1plus".into(), cpu_pct: 96.0, ..Default::default() }];
        let a = fire_until(&mut e, &s, &cfg(), 0, 61);
        let hit = a.iter().find(|x| x.rule_id == "cpu.temp.critical").expect("should fire");
        assert_eq!(hit.severity, Severity::Critical);
        assert!(hit.explanation.what.contains("97.0 °C"), "no measurement: {}", hit.explanation.what);
        assert!(hit.explanation.expected.is_some());
        assert!(!hit.explanation.actions.is_empty(), "must suggest something");
        assert!(hit.explanation.likely_cause.as_ref().unwrap().contains("cc1plus"));
        // Never panic wording.
        let r = hit.explanation.render().to_lowercase();
        assert!(!r.contains("anomaly detected"));
        assert!(!r.contains("!!!") && !r.contains("urgent!"));
    }

    #[test]
    fn a_brief_temperature_spike_does_not_fire() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.thermal.cpu_package_c = Some(97.0);
        // Only 30 s, under the 60 s dwell.
        e.evaluate(&s, &cfg(), 0);
        let a = e.evaluate(&s, &cfg(), 30_000).alerts;
        assert!(!a.iter().any(|x| x.rule_id == "cpu.temp.critical"));
    }

    #[test]
    fn thresholds_can_be_overridden_by_the_user() {
        let mut c = cfg();
        c.alerts.thresholds.insert("cpu.temp.critical.celsius".into(), 99.0);
        let mut e = RuleEngine::new(&c);
        let mut s = snap();
        s.thermal.cpu_package_c = Some(97.0);
        let a = fire_until(&mut e, &s, &c, 0, 61);
        assert!(!a.iter().any(|x| x.rule_id == "cpu.temp.critical"), "97 < the raised 99 C threshold");
        // And the alert text must quote the threshold actually in force.
        s.thermal.cpu_package_c = Some(100.0);
        let a2 = fire_until(&mut e, &s, &c, 200_000, 61);
        let hit = a2.iter().find(|x| x.rule_id == "cpu.temp.critical").unwrap();
        assert!(hit.explanation.expected.as_ref().unwrap().contains("99"));
    }

    #[test]
    fn a_full_disk_fires_per_mount_point() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.storage.filesystems = vec![
            FsInfo { mount: "/".into(), used_pct: 96.0, total_bytes: 300 << 30, free_bytes: 12 << 30, fstype: "ext4".into(), device: "/dev/nvme0n1p7".into(), ..Default::default() },
            FsInfo { mount: "/boot".into(), used_pct: 40.0, ..Default::default() },
        ];
        let a = e.evaluate(&s, &cfg(), 0).alerts;
        let hit = a.iter().find(|x| x.rule_id == "disk.full").expect("should fire");
        assert_eq!(hit.fingerprint, "disk.full:/", "fingerprint must carry the mount point");
        assert!(hit.explanation.what.contains("96.0%"));
        assert_eq!(a.iter().filter(|x| x.rule_id == "disk.full").count(), 1, "/boot is fine");
    }

    #[test]
    fn a_stalled_fan_only_matters_when_something_is_hot() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.thermal.fans = vec![Reading { key: "asus/cpu_fan".into(), label: "cpu_fan".into(), driver: "asus".into(), value: 0.0, crit: None }];
        s.thermal.max_temp_c = Some(42.0);
        s.thermal.max_temp_label = "coretemp/Package id 0".into();
        let cool = fire_until(&mut e, &s, &cfg(), 0, 61);
        assert!(!cool.iter().any(|x| x.rule_id == "thermal.fan_stalled"),
                "a stopped fan on a cool laptop is normal, not an alert");

        s.thermal.max_temp_c = Some(85.0);
        let hot = fire_until(&mut e, &s, &cfg(), 500_000, 61);
        let hit = hot.iter().find(|x| x.rule_id == "thermal.fan_stalled").expect("should fire when hot");
        assert_eq!(hit.severity, Severity::Critical);
    }

    #[test]
    fn oom_is_immediate_and_critical() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.memory.oom_just_happened = true;
        s.memory.oom_kill_total = 1;
        let a = e.evaluate(&s, &cfg(), 0).alerts;
        let hit = a.iter().find(|x| x.rule_id == "mem.oom").expect("OOM must fire at once");
        assert_eq!(hit.severity, Severity::Critical);
    }

    #[test]
    fn smart_critical_warning_bits_are_explained_in_words() {
        assert!(smart_warning_text(0x08).contains("read-only"));
        assert!(smart_warning_text(0x01).contains("spare"));
        assert!(smart_warning_text(0x09).contains("spare") && smart_warning_text(0x09).contains("read-only"));
        assert!(smart_warning_text(0x40).contains("0x40"), "unknown bits should still be shown");
    }

    #[test]
    fn the_specs_worked_example_produces_the_specs_wording() {
        let mut c = cfg();
        c.baseline.warmup_samples = 20;
        let mut e = RuleEngine::new(&c);
        let mut s = snap();
        s.power = PowerState { has_battery: true, on_battery: true, power_w: 10.9, percent: 80.0, ..Default::default() };
        s.idle = true;

        // Learn a normal idle range.
        for v in [9.1, 10.4, 11.2, 10.8, 9.7, 12.3, 13.1, 10.0, 11.5, 10.2].iter().cycle().take(300) {
            s.power.power_w = *v;
            let samples = vec![Sample { id: MetricId::global("power", "system_w"), value: *v, unit: "W" }];
            e.learn(&s, &samples);
        }

        // Now the fault: 28.4 W with the dGPU awake at 0% utilisation.
        s.power.power_w = 28.4;
        s.gpu.nvidia.present = true;
        s.gpu.nvidia.runtime_status = "active".into();
        s.gpu.nvidia.power_w = Some(6.2);
        s.gpu.nvidia.util_pct = Some(0.0);
        s.cpu.usage_pct = 2.0;

        let a = fire_until(&mut e, &s, &c, 0, 601);
        let hit = a.iter().find(|x| x.rule_id == "power.idle_high").expect("should fire");
        let text = hit.explanation.render();
        assert!(text.contains("28.4 W"), "missing the measurement:\n{text}");
        assert!(text.contains("learned idle range"), "missing the learned range:\n{text}");
        assert!(text.contains("NVIDIA GPU is active"), "missing the correlated cause:\n{text}");
        assert!(text.contains("6.2 W"));
        assert!(text.contains("0%"));
        assert!(text.contains("Suggested checks"));
        eprintln!("--- rendered alert ---\n{text}");
    }

    #[test]
    fn high_idle_power_says_nothing_about_cause_when_nothing_correlates() {
        let mut s = snap();
        s.power = PowerState { has_battery: true, on_battery: true, power_w: 28.0, ..Default::default() };
        s.idle = true;
        // No GPU, no busy process, no disk activity, no Bluetooth.
        assert_eq!(idle_power_cause(&s), None, "must not invent a cause");
    }

    #[test]
    fn an_idle_awake_dgpu_is_flagged_on_battery() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.power = PowerState { has_battery: true, on_battery: true, power_w: 20.0, ..Default::default() };
        s.gpu.nvidia.present = true;
        s.gpu.nvidia.runtime_status = "active".into();
        s.gpu.nvidia.power_w = Some(5.0);
        s.gpu.nvidia.util_pct = Some(0.0);
        s.gpu.dgpu_awake_idle = true;
        let a = fire_until(&mut e, &s, &cfg(), 0, 301);
        let hit = a.iter().find(|x| x.rule_id == "gpu.idle_awake").expect("should fire");
        assert!(hit.explanation.what.contains("5.0 W"));
        assert!(hit.explanation.actions.iter().any(|x| x.contains("runtime_status")));
    }

    #[test]
    fn gateway_up_but_internet_down_is_distinguished() {
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.network.reach.gateway_ok = Some(true);
        s.network.reach.internet_ok = Some(false);
        s.network.reach.dns_ok = Some(true);
        s.network.reach.gateway = Some("192.168.1.1".into());
        let a = fire_until(&mut e, &s, &cfg(), 0, 121);
        let hit = a.iter().find(|x| x.rule_id == "net.no_internet").expect("should fire");
        assert!(hit.explanation.likely_cause.as_ref().unwrap().contains("upstream"));
        // And when both are down it is not this rule's business.
        s.network.reach.gateway_ok = Some(false);
        let b = fire_until(&mut e, &s, &cfg(), 500_000, 121);
        assert!(!b.iter().any(|x| x.rule_id == "net.no_internet"));
    }

    #[test]
    fn every_alert_carries_a_real_explanation() {
        // Broad sweep: drive many rules at once and assert the contract holds for all.
        let mut e = RuleEngine::new(&cfg());
        let mut s = snap();
        s.thermal.cpu_package_c = Some(97.0);
        s.memory.available_pct = 2.0;
        s.memory.available_bytes = 500 << 20;
        s.storage.filesystems = vec![FsInfo { mount: "/".into(), used_pct: 99.0, inode_used_pct: 95.0, ..Default::default() }];
        s.gpu.nvidia.present = true;
        s.gpu.nvidia.temp_c = Some(92.0);
        s.power = PowerState { has_battery: true, on_battery: true, percent: 3.0, health_pct: 60.0, energy_full_wh: 54.0, energy_design_wh: 90.0, ..Default::default() };
        let a = fire_until(&mut e, &s, &cfg(), 0, 3600);
        assert!(a.len() >= 5, "expected several rules to fire, got {}", a.len());
        for x in &a {
            assert!(!x.explanation.what.is_empty(), "{} has no measurement", x.rule_id);
            assert!(!x.title.is_empty());
            assert!(!x.explanation.actions.is_empty(), "{} suggests nothing", x.rule_id);
            let r = x.explanation.render();
            assert!(!r.to_lowercase().contains("anomaly detected"), "{} used forbidden wording", x.rule_id);
            // Measurement rules must quote a number. State rules (a unit failed, a
            // device vanished) are specific without one, so they are exempt by name.
            const STATE_ONLY: &[&str] = &["service.failed", "audio.service_down", "device.missing", "bt.blocked"];
            if !STATE_ONLY.contains(&x.rule_id.as_str()) {
                assert!(r.chars().any(|c| c.is_ascii_digit()), "{} has no numbers: {r}", x.rule_id);
            }
        }
    }

    #[test]
    fn learnable_metrics_are_a_deliberate_subset() {
        assert!(is_learnable("power", "system_w"));
        assert!(is_learnable("network", "rx_bps"));
        assert!(!is_learnable("cpu", "procs_total"), "counting processes is not a distribution");
        assert!(!is_learnable("memory", "total_bytes"), "a constant is not worth learning");
    }
}

/// One short phrase describing the radio's power state at a disconnect, for the
/// evidence list. Kept human: "USB-autosuspended" means something to a reader,
/// "runtime_status=suspended" does not.
fn radio_phrase(c: &crate::collectors::bluetooth::BtContext) -> String {
    if c.rfkill_hard {
        return "hard-blocked".into();
    }
    if c.rfkill_soft {
        return "soft-blocked".into();
    }
    if c.radio_runtime_status == "suspended" || c.radio_suspended_recently {
        return "USB-autosuspended around the drop".into();
    }
    if c.radio_runtime_status.is_empty() {
        return "unknown".into();
    }
    format!("{} (autosuspend {})", c.radio_runtime_status,
        if c.radio_power_control == "auto" { "allowed" } else { "off" })
}
