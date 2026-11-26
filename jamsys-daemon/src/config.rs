//! Configuration. TOML at `~/.config/jamsys/config.toml`, created with defaults on
//! first run. Every field has a default so an old or partial config keeps working after
//! an upgrade instead of refusing to start.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sampling: Sampling,
    pub retention: Retention,
    pub baseline: Baseline,
    pub alerts: Alerts,
    pub network: Network,
    pub storage: Storage,
    pub security: Security,
    /// Per-collector on/off. Missing key means enabled.
    pub collectors: BTreeMap<String, bool>,
    /// Journal message globs that must never produce an alert.
    pub journal_ignore: Vec<String>,
    /// systemd units that must never produce an alert.
    pub service_ignore: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sampling {
    pub fast_ms: u64,
    pub medium_ms: u64,
    pub slow_ms: u64,
    pub glacial_ms: u64,
    /// Multiply every interval by this when on battery and idle with no UI attached.
    pub idle_multiplier: f64,
    /// CPU % below which the machine counts as idle for context and throttling.
    pub idle_cpu_pct: f64,
    /// Flush buffered aggregates to SQLite at most this often.
    pub flush_ms: u64,
    /// How much full-resolution history to keep **in memory**. Never written to disk.
    pub live_window_ms: u64,
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling {
            fast_ms: 2_000,
            medium_ms: 10_000,
            slow_ms: 60_000,
            glacial_ms: 900_000,
            idle_multiplier: 3.0,
            idle_cpu_pct: 8.0,
            flush_ms: 15_000,
            live_window_ms: 30 * 60 * 1000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    pub raw_hours: i64,
    pub minute_days: i64,
    pub five_min_days: i64,
    pub fifteen_min_days: i64,
    pub event_days: i64,
    pub alert_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            raw_hours: 24,
            minute_days: 7,
            five_min_days: 30,
            fifteen_min_days: 90,
            event_days: 90,
            alert_days: 90,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Baseline {
    /// Samples required in a context before any learned rule may fire.
    pub warmup_samples: u32,
    /// Reservoir size per metric per context.
    pub window: usize,
    /// Robust z-score above which a learned rule is a candidate.
    pub z_threshold: f64,
    /// Split baselines into four time-of-day buckets as well as ac/bat × idle/active.
    /// Off by default: it quadruples warm-up for little gain on a personal laptop.
    pub time_of_day: bool,
}

impl Default for Baseline {
    fn default() -> Self {
        Baseline { warmup_samples: 120, window: 512, z_threshold: 3.5, time_of_day: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Alerts {
    pub desktop_notifications: bool,
    /// Do not notify below this severity (events are still recorded).
    pub min_notify_severity: String,
    /// Token bucket per severity.
    pub burst: u32,
    pub refill_s: i64,
    /// User-adjusted L1 thresholds, `rule_id.key = value`.
    pub thresholds: BTreeMap<String, f64>,
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts {
            desktop_notifications: true,
            min_notify_severity: "notice".into(),
            burst: 10,
            refill_s: 60,
            thresholds: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    /// TCP connect probes for the reachability trio. No payload is ever sent.
    pub reachability: bool,
    /// Host:port used for the "is the internet up" leg.
    pub internet_probe: String,
    pub probe_timeout_ms: u64,
    /// Interfaces to skip entirely (globs).
    pub ignore_interfaces: Vec<String>,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            reachability: true,
            // Cloudflare DNS over TCP/53: a bare connect, chosen because it is
            // anycast, has no logging value for a 0-byte connect, and is not an
            // HTTP endpoint that could be mistaken for telemetry.
            internet_probe: "1.1.1.1:53".into(),
            probe_timeout_ms: 2_000,
            ignore_interfaces: vec!["veth*".into(), "docker*".into(), "br-*".into(), "virbr*".into()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    /// Mount points to ignore (globs). Snap/flatpak loop mounts are read-only squashfs
    /// pinned at 100 % use; without this the "disk nearly full" rule fires 37 times on
    /// a normal Ubuntu desktop.
    pub ignore_mounts: Vec<String>,
    pub ignore_fstypes: Vec<String>,
}

impl Default for Storage {
    fn default() -> Self {
        Storage {
            ignore_mounts: vec!["/snap/*".into(), "/var/snap/*".into(), "/var/lib/snapd/snap/*".into()],
            ignore_fstypes: vec![
                "squashfs".into(), "tmpfs".into(), "devtmpfs".into(), "overlay".into(),
                "proc".into(), "sysfs".into(), "cgroup".into(), "cgroup2".into(),
                "devpts".into(), "debugfs".into(), "tracefs".into(), "configfs".into(),
                "securityfs".into(), "pstore".into(), "efivarfs".into(), "bpf".into(),
                "autofs".into(), "mqueue".into(), "hugetlbfs".into(), "fusectl".into(),
                "binfmt_misc".into(), "ramfs".into(), "nsfs".into(), "fuse.portal".into(),
                "fuse.gvfsd-fuse".into(), "iso9660".into(),
            ],
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Security {
    /// Network-facing listeners the user has said are meant to be there, as
    /// `name:proto/port` (the key shown on the Security page). Anything else that
    /// starts listening after the first run raises an alert.
    pub accepted_listeners: Vec<String>,
    /// USB keyboards accepted the same way, as `usb-keyboard:vendor:product`.
    pub accepted_devices: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sampling: Sampling::default(),
            retention: Retention::default(),
            baseline: Baseline::default(),
            alerts: Alerts::default(),
            network: Network::default(),
            storage: Storage::default(),
            security: Security::default(),
            collectors: BTreeMap::new(),
            journal_ignore: default_journal_ignore(),
            service_ignore: Vec::new(),
        }
    }
}

/// Recurring firmware and driver chatter that is harmless on this class of hardware.
/// These become `event` rows but never alerts. Users can edit the list.
fn default_journal_ignore() -> Vec<String> {
    [
        "*ACPI BIOS Error*",
        "*ACPI Error: Needed type [Reference]*",
        "*ACPI Warning: SystemIO range*",
        "*ACPI: \\_SB_.PC00*",
        "*acpi PNP0C14*duplicate WMI*",
        "*Bluetooth: hci0: Opcode 0x*failed: -110*",
        "*psmouse serio*: Failed to*",
        "*i915*Failed to send flush*",
        "*thermal thermal_zone*failed to read out thermal zone*",
        "*Spurious APIC interrupt*",
        "*ucsi_acpi*: unknown error 256*",
        "*nvidia-gpu*: i2c timeout error*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// One user-adjustable threshold: its identity in `[alerts.thresholds]`, the
/// alert it governs, and the range a value may sensibly take.
///
/// This table exists so that a client can *offer* the thresholds instead of
/// asking the user to type a key from memory. Before it, `set_threshold`
/// accepted any `rule.key` and any finite number: a mistyped key was written to
/// the config file, changed nothing, and was reported as a success.
pub struct ThresholdSpec {
    /// Config rule name; the override is stored as `rule.key`.
    pub rule: &'static str,
    pub key: &'static str,
    /// `rule_id` of the alert this value decides. Usually `rule` itself.
    pub alert: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub unit: &'static str,
    pub label: &'static str,
}

const fn spec(rule: &'static str, key: &'static str, alert: &'static str, default: f64,
              min: f64, max: f64, unit: &'static str, label: &'static str) -> ThresholdSpec {
    ThresholdSpec { rule, key, alert, default, min, max, unit, label }
}

/// Every threshold the rule engine reads. A test holds this in step with the
/// `cfg.threshold(...)` calls in `anomaly/rules.rs`, defaults included.
pub const THRESHOLDS: &[ThresholdSpec] = &[
    spec("cpu.temp.critical", "celsius", "cpu.temp.critical", 95.0, 60.0, 110.0, "°C", "CPU temperature, critical"),
    spec("cpu.temp.high", "celsius", "cpu.temp.high", 90.0, 50.0, 110.0, "°C", "CPU temperature, high"),
    spec("cpu.sustained_load", "pct", "cpu.sustained_load", 90.0, 10.0, 100.0, "%", "Sustained CPU load"),
    spec("cpu.runaway_process", "pct", "cpu.runaway_process", 85.0, 10.0, 6400.0, "% of one core", "Runaway process CPU"),
    spec("mem.critical", "avail_pct", "mem.critical", 3.0, 0.5, 50.0, "% available", "Memory critically low"),
    spec("mem.low", "avail_pct", "mem.low", 8.0, 1.0, 60.0, "% available", "Memory low"),
    spec("mem.pressure", "psi", "mem.pressure", 20.0, 1.0, 100.0, "% stalled", "Memory pressure"),
    spec("mem.swap_sustained", "pct", "mem.swap_sustained", 25.0, 1.0, 100.0, "%", "Sustained swap use"),
    spec("mem.leak", "bytes_per_hour", "mem.leak", 100e6, 1e6, 100e9, "bytes/hour", "Memory leak growth rate"),
    spec("thermal.fan_stalled", "celsius", "thermal.fan_stalled", 70.0, 40.0, 100.0, "°C", "Fan stopped while hotter than"),
    spec("disk.full", "pct", "disk.full", 95.0, 50.0, 100.0, "%", "Filesystem full"),
    spec("disk.filling", "pct", "disk.filling", 90.0, 50.0, 100.0, "%", "Filesystem filling up"),
    spec("disk.inodes", "pct", "disk.inodes", 90.0, 50.0, 100.0, "%", "Inodes used"),
    spec("nvme.wear", "pct", "nvme.wear", 90.0, 10.0, 255.0, "%", "NVMe endurance used"),
    spec("nvme.temp", "celsius", "nvme.temp", 70.0, 40.0, 100.0, "°C", "NVMe temperature"),
    spec("gpu.nvidia.temp", "celsius", "gpu.nvidia.temp", 87.0, 50.0, 110.0, "°C", "NVIDIA GPU temperature"),
    spec("gpu.nvidia.vram", "pct", "gpu.nvidia.vram", 95.0, 50.0, 100.0, "%", "NVIDIA video memory used"),
    spec("battery.health", "pct", "battery.health", 70.0, 10.0, 100.0, "% of design", "Battery health below"),
    spec("battery.critical", "pct", "battery.critical", 5.0, 1.0, 50.0, "%", "Battery critically low"),
    spec("net.errors", "pct", "net.errors", 1.0, 0.01, 100.0, "%", "Packet error rate"),
    spec("bluetooth.disconnect.hold", "seconds", "bluetooth.disconnected", 120.0, 0.0, 3600.0, "s", "Bluetooth disconnect notice stays open for"),
    spec("suspend.drain", "pct_per_hour", "suspend.drain", 2.0, 0.1, 50.0, "% per hour", "Battery drain while asleep"),
    spec("security.login_failures", "count", "security.login_failures", 5.0, 2.0, 1000.0, "in 10 min", "Failed sign-in attempts"),
    spec("bluetooth.flap.count", "disconnects", "bluetooth.flapping", 3.0, 2.0, 50.0, "in 15 min", "Bluetooth disconnects that count as flapping"),
];

pub fn threshold_spec(rule: &str, key: &str) -> Option<&'static ThresholdSpec> {
    THRESHOLDS.iter().find(|t| t.rule == rule && t.key == key)
}

impl Config {
    pub fn enabled(&self, collector: &str) -> bool {
        self.collectors.get(collector).copied().unwrap_or(true)
    }

    /// Threshold with a user override applied, falling back to the built-in default.
    pub fn threshold(&self, rule: &str, key: &str, default: f64) -> f64 {
        // TOML can spell `nan` and `inf`. Either would make every comparison
        // against it false, which silently turns the rule off.
        self.alerts.thresholds.get(&format!("{rule}.{key}")).copied()
            .filter(|v| v.is_finite())
            .unwrap_or(default)
    }

    pub fn min_notify(&self) -> crate::types::Severity {
        use crate::types::Severity::*;
        match self.alerts.min_notify_severity.to_ascii_lowercase().as_str() {
            "info" => Info,
            "warning" => Warning,
            "critical" => Critical,
            _ => Notice,
        }
    }

    pub fn tier_ms(&self, t: crate::types::Tier) -> u64 {
        use crate::types::Tier::*;
        match t {
            Fast => self.sampling.fast_ms,
            Medium => self.sampling.medium_ms,
            Slow => self.sampling.slow_ms,
            Glacial => self.sampling.glacial_ms,
            Event => u64::MAX,
        }
    }

    /// Load, or create with defaults. A malformed config is reported and the defaults
    /// are used — a monitoring daemon that refuses to start because of a typo in a
    /// comment is worse than one running on defaults.
    pub fn load_or_create(path: &Path) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut c) => {
                    for fix in c.sanitize() {
                        crate::log_warn!("config: {fix}");
                    }
                    (c, None)
                }
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => {
                let c = Config::default();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(path, c.to_toml());
                (c, None)
            }
        }
    }

    /// Replace values the daemon cannot run with, returning one line per change.
    ///
    /// The file parses as TOML long before it makes sense: `fast_ms = 0` made a
    /// tier due on every loop iteration (a busy loop), a negative retention put
    /// the cutoff in the future so each pass deleted everything, and
    /// `idle_multiplier = inf` overflowed the deadline arithmetic. `--check`
    /// called all of these "config OK".
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let d = Config::default();
        let mut ms = |name: &str, v: &mut u64, min: u64, max: u64| {
            if *v < min || *v > max {
                let fixed = (*v).clamp(min, max);
                out.push(format!("sampling.{name} = {v} is outside {min}..={max}; using {fixed}"));
                *v = fixed;
            }
        };
        const DAY: u64 = 86_400_000;
        ms("fast_ms", &mut self.sampling.fast_ms, 250, DAY);
        ms("medium_ms", &mut self.sampling.medium_ms, 250, DAY);
        ms("slow_ms", &mut self.sampling.slow_ms, 1_000, DAY);
        ms("glacial_ms", &mut self.sampling.glacial_ms, 10_000, DAY);
        ms("flush_ms", &mut self.sampling.flush_ms, 1_000, DAY);
        ms("live_window_ms", &mut self.sampling.live_window_ms, 60_000, DAY);
        ms("probe_timeout_ms (network)", &mut self.network.probe_timeout_ms, 50, 5_000);

        let m = self.sampling.idle_multiplier;
        if !m.is_finite() || !(1.0..=20.0).contains(&m) {
            let fixed = if m.is_finite() { m.clamp(1.0, 20.0) } else { d.sampling.idle_multiplier };
            out.push(format!("sampling.idle_multiplier = {m} is outside 1..=20; using {fixed}"));
            self.sampling.idle_multiplier = fixed;
        }
        let c = self.sampling.idle_cpu_pct;
        if !c.is_finite() || !(0.0..=100.0).contains(&c) {
            out.push(format!("sampling.idle_cpu_pct = {c} is not a percentage; using {}",
                             d.sampling.idle_cpu_pct));
            self.sampling.idle_cpu_pct = d.sampling.idle_cpu_pct;
        }
        for (name, v) in [
            ("raw_hours", &mut self.retention.raw_hours),
            ("minute_days", &mut self.retention.minute_days),
            ("five_min_days", &mut self.retention.five_min_days),
            ("fifteen_min_days", &mut self.retention.fifteen_min_days),
            ("event_days", &mut self.retention.event_days),
            ("alert_days", &mut self.retention.alert_days),
        ] {
            if *v < 1 || *v > 36_500 {
                let fixed = (*v).clamp(1, 36_500);
                out.push(format!("retention.{name} = {v} is outside 1..=36500; using {fixed}"));
                *v = fixed;
            }
        }
        if self.alerts.burst == 0 {
            out.push("alerts.burst = 0 would silence every notification; using 1 \
                      (set desktop_notifications = false to turn them off)".into());
            self.alerts.burst = 1;
        }
        if self.alerts.refill_s < 1 {
            out.push(format!("alerts.refill_s = {} must be at least 1; using {}",
                             self.alerts.refill_s, d.alerts.refill_s));
            self.alerts.refill_s = d.alerts.refill_s;
        }
        out
    }

    /// Write the file atomically: a crash or a full disk half-way through must
    /// leave the previous file, not an empty one that loads as all-defaults.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml())?;
        std::fs::rename(&tmp, path)
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!(
            "# JamSys configuration.\n\
             # Delete this file to regenerate defaults. All keys are optional.\n\
             # Reload without restarting:  systemctl --user reload jamsysd\n\n{body}"
        )
    }

    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("jamsys")
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("jamsys")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/jamsys-{}", unsafe { libc::getuid() })))
        .join("jamsys")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.sampling.fast_ms, c.sampling.fast_ms);
        assert_eq!(back.storage.ignore_fstypes.len(), c.storage.ignore_fstypes.len());
    }

    #[test]
    fn a_partial_config_keeps_every_other_default() {
        // Simulates an old config file after an upgrade added new keys.
        let c: Config = toml::from_str("[sampling]\nfast_ms = 5000\n").unwrap();
        assert_eq!(c.sampling.fast_ms, 5000);
        assert_eq!(c.sampling.medium_ms, 10_000, "unset keys must fall back");
        assert_eq!(c.retention.raw_hours, 24);
        assert!(!c.journal_ignore.is_empty());
    }

    #[test]
    fn a_broken_config_falls_back_instead_of_refusing_to_start() {
        let p = std::env::temp_dir().join(format!("sv-badcfg-{}.toml", std::process::id()));
        std::fs::write(&p, "this is not = = toml").unwrap();
        let (c, err) = Config::load_or_create(&p);
        assert!(err.is_some(), "the problem must be reported");
        assert_eq!(c.sampling.fast_ms, 2000, "but defaults must still load");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn collectors_default_to_enabled_and_can_be_turned_off() {
        let c: Config = toml::from_str("[collectors]\nnvidia = false\n").unwrap();
        assert!(!c.enabled("nvidia"));
        assert!(c.enabled("cpu"), "unlisted collectors are enabled");
    }

    #[test]
    fn threshold_overrides_win_over_defaults() {
        let c: Config = toml::from_str("[alerts.thresholds]\n\"cpu.temp.critical.celsius\" = 99.0\n").unwrap();
        assert_eq!(c.threshold("cpu.temp.critical", "celsius", 95.0), 99.0);
        assert_eq!(c.threshold("cpu.temp.high", "celsius", 90.0), 90.0);
    }

    #[test]
    fn values_the_daemon_cannot_run_with_are_replaced_and_reported() {
        let mut c: Config = toml::from_str(
            "[sampling]\nfast_ms = 0\nglacial_ms = 0\nidle_multiplier = inf\n\
             [retention]\nraw_hours = -5\n[alerts]\nburst = 0\n").unwrap();
        let fixes = c.sanitize();
        assert_eq!(c.sampling.fast_ms, 250, "a zero interval is a busy loop");
        assert_eq!(c.sampling.glacial_ms, 10_000);
        assert_eq!(c.sampling.idle_multiplier, 3.0);
        assert_eq!(c.retention.raw_hours, 1, "a negative retention deletes everything");
        assert_eq!(c.alerts.burst, 1);
        assert_eq!(fixes.len(), 5, "each change is reported: {fixes:?}");
        assert!(Config::default().sanitize().is_empty(), "the defaults need no fixing");
    }

    #[test]
    fn saving_replaces_the_file_in_one_step() {
        let p = std::env::temp_dir().join(format!("jamsys-save-{}.toml", std::process::id()));
        std::fs::write(&p, "previous").unwrap();
        let mut c = Config::default();
        c.alerts.thresholds.insert("disk.full.pct".into(), 97.0);
        c.save(&p).unwrap();
        let back: Config = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back.threshold("disk.full", "pct", 95.0), 97.0);
        assert!(!p.with_extension("toml.tmp").exists(), "no temporary file is left behind");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_non_finite_override_falls_back_to_the_default() {
        let c: Config = toml::from_str(
            "[alerts.thresholds]\n\"disk.full.pct\" = nan\n\"mem.low.avail_pct\" = inf\n").unwrap();
        assert_eq!(c.threshold("disk.full", "pct", 95.0), 95.0);
        assert_eq!(c.threshold("mem.low", "avail_pct", 8.0), 8.0);
    }

    /// The table clients are shown and the values the engine actually reads
    /// must be the same list, or the UI offers a knob that is connected to
    /// nothing (or hides one that exists).
    #[test]
    fn the_threshold_table_matches_what_the_rules_read() {
        let src = concat!(include_str!("anomaly/rules.rs"), include_str!("daemon.rs"));
        let mut read = Vec::new();
        for part in src.split("cfg.threshold(").skip(1) {
            let args: Vec<&str> = part.split(')').next().unwrap().split(',').map(str::trim).collect();
            // Only literal call sites; the IPC listing passes the table's own fields.
            if !args[0].starts_with('"') {
                continue;
            }
            let default: f64 = args[2].parse().unwrap_or_else(|_| panic!("default {:?}", args[2]));
            read.push((args[0].trim_matches('"').to_string(), args[1].trim_matches('"').to_string(), default));
        }
        assert!(read.len() >= 20, "the scan found only {} call sites", read.len());
        for (rule, key, default) in &read {
            let t = threshold_spec(rule, key)
                .unwrap_or_else(|| panic!("{rule}.{key} is read by a rule but not listed"));
            assert_eq!(t.default, *default, "{rule}.{key} default");
        }
        for t in THRESHOLDS {
            assert!(read.iter().any(|(r, k, _)| r == t.rule && k == t.key),
                    "{}.{} is listed but no rule reads it", t.rule, t.key);
            assert!(t.min <= t.default && t.default <= t.max, "{}.{} default out of range", t.rule, t.key);
            assert!(src.contains(&format!("Alert::new(\"{}\"", t.alert)),
                    "{}.{} names an alert no rule raises", t.rule, t.key);
        }
    }

    #[test]
    fn snap_mounts_are_ignored_by_default() {
        let c = Config::default();
        assert!(c.storage.ignore_fstypes.contains(&"squashfs".to_string()));
        assert!(c.storage.ignore_mounts.iter().any(|g| crate::util::glob_match(g, "/snap/firefox/8863")));
    }
}
