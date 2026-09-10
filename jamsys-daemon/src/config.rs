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

impl Default for Config {
    fn default() -> Self {
        Config {
            sampling: Sampling::default(),
            retention: Retention::default(),
            baseline: Baseline::default(),
            alerts: Alerts::default(),
            network: Network::default(),
            storage: Storage::default(),
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

impl Config {
    pub fn enabled(&self, collector: &str) -> bool {
        self.collectors.get(collector).copied().unwrap_or(true)
    }

    /// Threshold with a user override applied, falling back to the built-in default.
    pub fn threshold(&self, rule: &str, key: &str, default: f64) -> f64 {
        self.alerts.thresholds.get(&format!("{rule}.{key}")).copied().unwrap_or(default)
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
                Ok(c) => (c, None),
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
    fn snap_mounts_are_ignored_by_default() {
        let c = Config::default();
        assert!(c.storage.ignore_fstypes.contains(&"squashfs".to_string()));
        assert!(c.storage.ignore_mounts.iter().any(|g| crate::util::glob_match(g, "/snap/firefox/8863")));
    }
}
