//! Filesystems, block I/O, NVMe health.
//!
//! The filtering here is not cosmetic. This machine has **37 snap `loop` mounts**, every
//! one a read-only squashfs pinned at 100 % usage. A "disk nearly full" rule without
//! this filter fires 37 false criticals on a stock Ubuntu desktop.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use crate::util::glob_match;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::CString;

#[derive(Clone, Debug, Default, Serialize)]
pub struct FsInfo {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_pct: f64,
    pub inode_used_pct: f64,
    pub read_only: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiskIo {
    pub device: String,
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    /// Fraction of wall time the device had I/O in flight.
    pub util_pct: f64,
    /// Mean service time in ms, derived from the weighted I/O time.
    pub avg_latency_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NvmeHealth {
    pub device: String,
    pub model: String,
    pub firmware: String,
    pub temp_c: Option<f64>,
    pub critical_warning: Option<u8>,
    pub percentage_used: Option<u8>,
    pub available_spare: Option<u8>,
    pub available_spare_threshold: Option<u8>,
    pub media_errors: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub data_units_written: Option<u64>,
    /// True when the figures came from the privileged helper rather than sysfs.
    pub smart_available: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StorageState {
    pub filesystems: Vec<FsInfo>,
    pub disks: Vec<DiskIo>,
    pub nvme: Vec<NvmeHealth>,
    /// ext4 error counters, keyed by device.
    pub fs_error_counts: HashMap<String, u64>,
    pub total_read_bps: f64,
    pub total_write_bps: f64,
    /// Number of pseudo/loop mounts skipped, shown in the UI so the filtering is visible.
    pub hidden_mounts: usize,
}

pub struct StorageCollector {
    prev_diskstats: HashMap<String, (DiskStat, i64)>,
    prev_fs_errors: HashMap<String, u64>,
    /// Resolved once at probe. Rediscovering hwmon on every collect meant walking
    /// all of /sys/class/hwmon each time for a single temperature.
    nvme_temp: Vec<(String, HwmonChannel)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiskStat {
    pub reads: u64,
    pub read_sectors: u64,
    pub writes: u64,
    pub write_sectors: u64,
    pub io_ticks_ms: u64,
    pub weighted_ms: u64,
}

/// Parse /proc/diskstats, keeping only whole devices (not partitions, not loop/ram).
pub fn parse_diskstats(text: &str) -> HashMap<String, DiskStat> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let name = f[2];
        if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") || name.starts_with("dm-") {
            continue;
        }
        // Skip partitions: `nvme0n1p7` and `sda1` are covered by their parent device.
        let is_partition = if name.starts_with("nvme") {
            name.contains('p') && name.rsplit('p').next().is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty())
        } else {
            name.chars().last().is_some_and(|c| c.is_ascii_digit())
                && name.chars().any(|c| c.is_ascii_alphabetic())
                && !name.starts_with("mmcblk")
        };
        if is_partition {
            continue;
        }
        let g = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        m.insert(
            name.to_string(),
            DiskStat {
                reads: g(3),
                read_sectors: g(5),
                writes: g(7),
                write_sectors: g(9),
                io_ticks_ms: g(12),
                weighted_ms: g(13),
            },
        );
    }
    m
}

/// One line of /proc/self/mountinfo, reduced to what the collector needs.
#[derive(Clone, Debug, PartialEq)]
pub struct MountEntry {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub read_only: bool,
}

pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        // ... mountpoint opts - fstype source superopts
        let Some(sep) = line.find(" - ") else { continue };
        let (left, right) = line.split_at(sep);
        let lf: Vec<&str> = left.split_whitespace().collect();
        let rf: Vec<&str> = right[3..].split_whitespace().collect();
        if lf.len() < 6 || rf.len() < 2 {
            continue;
        }
        out.push(MountEntry {
            // mountinfo escapes spaces and tabs as octal.
            mount: unescape_octal(lf[4]),
            device: rf[1].to_string(),
            fstype: rf[0].to_string(),
            read_only: lf[5].split(',').any(|o| o == "ro"),
        });
    }
    out
}

fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Should this mount be monitored, or is it kernel/snap plumbing?
pub fn is_real_filesystem(m: &MountEntry, cfg: &crate::config::Storage) -> bool {
    if cfg.ignore_fstypes.iter().any(|t| t == &m.fstype) {
        return false;
    }
    if cfg.ignore_mounts.iter().any(|g| glob_match(g, &m.mount)) {
        return false;
    }
    // A read-only mount cannot fill up, so it can never be actionable.
    if m.read_only {
        return false;
    }
    m.device.starts_with('/') || m.fstype == "zfs" || m.fstype == "btrfs" || m.fstype == "nfs4"
}

fn statvfs(path: &str) -> Option<(u64, u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    // SAFETY: zeroed statvfs is a valid out-parameter; path is NUL-terminated.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize } as u64;
    Some((
        s.f_blocks as u64 * bs,
        // f_bavail, not f_bfree: the reserved-for-root blocks are not usable space.
        s.f_bavail as u64 * bs,
        s.f_files as u64,
        s.f_ffree as u64,
    ))
}

impl StorageCollector {
    pub fn new() -> Self {
        StorageCollector {
            prev_diskstats: HashMap::new(),
            prev_fs_errors: HashMap::new(),
            nvme_temp: Vec::new(),
        }
    }
}

impl Default for StorageCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for StorageCollector {
    fn name(&self) -> &'static str {
        "storage"
    }
    fn tier(&self) -> Tier {
        Tier::Slow
    }

    fn probe(&mut self) -> Support {
        if !exists("/proc/diskstats") || !exists("/proc/self/mountinfo") {
            return Support::Unsupported { reason: "/proc/diskstats unavailable".into() };
        }
        // Resolve NVMe temperature channels once. There is one hwmon device per
        // controller, so pairing them in discovery order is correct and stable.
        self.nvme_temp.clear();
        let controllers = list_dir("/sys/class/nvme");
        let chans: Vec<HwmonChannel> = discover_hwmon(&[HwmonKind::Temp])
            .into_iter()
            .filter(|c| c.driver == "nvme" && c.label.starts_with("Composite"))
            .collect();
        for (i, dev) in controllers.iter().enumerate() {
            if let Some(c) = chans.get(i) {
                self.nvme_temp.push((dev.clone(), c.clone()));
            }
        }
        let has_nvme = !controllers.is_empty();
        let smart = super::privileged::nvme_smart_available();
        if has_nvme && !smart {
            return Support::Partial {
                detail: "NVMe SMART needs the privileged helper; temperature and I/O are available".into(),
            };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = StorageState::default();
        let cfg = ctx.config.clone();

        // ---- filesystems --------------------------------------------------
        let mi = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| CollectorError::Gone(format!("mountinfo: {e}")))?;
        let mounts = parse_mountinfo(&mi);
        let mut seen = std::collections::HashSet::new();
        for m in &mounts {
            if !is_real_filesystem(m, &cfg.storage) {
                st.hidden_mounts += 1;
                continue;
            }
            // Bind mounts share a device; report each device once.
            if !seen.insert(m.device.clone()) {
                continue;
            }
            let Some((total, avail, inodes, ifree)) = statvfs(&m.mount) else { continue };
            if total == 0 {
                continue;
            }
            let used_pct = 100.0 * (total - avail) as f64 / total as f64;
            let inode_used_pct =
                if inodes > 0 { 100.0 * (inodes - ifree) as f64 / inodes as f64 } else { 0.0 };
            ctx.sample("storage", "fs_used_pct", &m.mount, "%", used_pct);
            ctx.sample("storage", "fs_free_bytes", &m.mount, "B", avail as f64);
            st.filesystems.push(FsInfo {
                mount: m.mount.clone(),
                device: m.device.clone(),
                fstype: m.fstype.clone(),
                total_bytes: total,
                free_bytes: avail,
                used_pct,
                inode_used_pct,
                read_only: m.read_only,
            });
        }

        // ---- block I/O ----------------------------------------------------
        if let Some(text) = read_str("/proc/diskstats") {
            let now = parse_diskstats(&text);
            for (dev, cur) in &now {
                if let Some((prev, t0)) = self.prev_diskstats.get(dev) {
                    let dt_ms = (ctx.ts_ms - t0) as f64;
                    if dt_ms <= 0.0 {
                        continue;
                    }
                    let dt = dt_ms / 1000.0;
                    // Counters reset on device re-enumeration; saturating_sub keeps
                    // that from producing an absurd spike.
                    let rd = cur.read_sectors.saturating_sub(prev.read_sectors) as f64 * 512.0 / dt;
                    let wr = cur.write_sectors.saturating_sub(prev.write_sectors) as f64 * 512.0 / dt;
                    let rio = cur.reads.saturating_sub(prev.reads) as f64 / dt;
                    let wio = cur.writes.saturating_sub(prev.writes) as f64 / dt;
                    let util = (100.0 * cur.io_ticks_ms.saturating_sub(prev.io_ticks_ms) as f64 / dt_ms)
                        .clamp(0.0, 100.0);
                    let ops = rio + wio;
                    let lat = if ops > 0.0 {
                        cur.weighted_ms.saturating_sub(prev.weighted_ms) as f64 / (ops * dt)
                    } else {
                        0.0
                    };
                    ctx.sample("storage", "read_bps", dev, "B/s", rd);
                    ctx.sample("storage", "write_bps", dev, "B/s", wr);
                    ctx.sample("storage", "util_pct", dev, "%", util);
                    st.total_read_bps += rd;
                    st.total_write_bps += wr;
                    st.disks.push(DiskIo {
                        device: dev.clone(),
                        read_bps: rd,
                        write_bps: wr,
                        read_iops: rio,
                        write_iops: wio,
                        util_pct: util,
                        avg_latency_ms: lat,
                    });
                }
                self.prev_diskstats.insert(dev.clone(), (*cur, ctx.ts_ms));
            }
        }

        // ---- ext4 error counters (free, unprivileged, and a real signal) ----
        for dev in list_dir("/sys/fs/ext4") {
            if let Some(n) = read_u64(format!("/sys/fs/ext4/{dev}/errors_count")) {
                if let Some(&prev) = self.prev_fs_errors.get(&dev) {
                    if n > prev {
                        ctx.event(Event::new(
                            "storage",
                            "fs_error",
                            format!("ext4 recorded {} new error(s) on {dev}", n - prev),
                            Severity::Critical,
                        ));
                    }
                }
                self.prev_fs_errors.insert(dev.clone(), n);
                st.fs_error_counts.insert(dev, n);
            }
        }

        // ---- NVMe ----------------------------------------------------------
        for dev in list_dir("/sys/class/nvme") {
            let base = format!("/sys/class/nvme/{dev}");
            let mut h = NvmeHealth {
                device: dev.clone(),
                model: read_str(format!("{base}/model")).unwrap_or_default().trim().to_string(),
                firmware: read_str(format!("{base}/firmware_rev")).unwrap_or_default(),
                ..Default::default()
            };
            // Temperature is available unprivileged through hwmon, from the channel
            // resolved at probe time.
            if let Some((_, c)) = self.nvme_temp.iter().find(|(d, _)| *d == dev) {
                h.temp_c = c.read();
            }
            if let Some(t) = h.temp_c {
                ctx.sample("storage", "nvme_temp_c", &dev, "C", t);
            }
            // SMART only if the privileged helper is installed.
            if let Some(s) = super::privileged::nvme_smart(&dev) {
                h.smart_available = true;
                h.critical_warning = s.critical_warning;
                h.percentage_used = s.percentage_used;
                h.available_spare = s.available_spare;
                h.available_spare_threshold = s.available_spare_threshold;
                h.media_errors = s.media_errors;
                h.unsafe_shutdowns = s.unsafe_shutdowns;
                h.power_on_hours = s.power_on_hours;
                h.data_units_written = s.data_units_written;
                if let Some(p) = s.percentage_used {
                    ctx.sample("storage", "nvme_wear_pct", &dev, "%", p as f64);
                }
            }
            st.nvme.push(h);
        }

        ctx.snap.storage = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Storage as StorageCfg};
    use std::sync::Arc;

    #[test]
    fn snap_loop_mounts_are_filtered_out() {
        let cfg = StorageCfg::default();
        let snap = MountEntry {
            mount: "/snap/firefox/8863".into(),
            device: "/dev/loop14".into(),
            fstype: "squashfs".into(),
            read_only: true,
        };
        assert!(!is_real_filesystem(&snap, &cfg), "squashfs snap mount must be hidden");

        let root = MountEntry {
            mount: "/".into(),
            device: "/dev/nvme0n1p7".into(),
            fstype: "ext4".into(),
            read_only: false,
        };
        assert!(is_real_filesystem(&root, &cfg), "the root filesystem must be monitored");
    }

    #[test]
    fn pseudo_filesystems_are_filtered_out() {
        let cfg = StorageCfg::default();
        for (fs, dev, mnt) in [
            ("tmpfs", "tmpfs", "/run"),
            ("proc", "proc", "/proc"),
            ("cgroup2", "cgroup2", "/sys/fs/cgroup"),
            ("overlay", "overlay", "/var/lib/docker/overlay2/x/merged"),
        ] {
            let m = MountEntry { mount: mnt.into(), device: dev.into(), fstype: fs.into(), read_only: false };
            assert!(!is_real_filesystem(&m, &cfg), "{fs} should be filtered");
        }
    }

    #[test]
    fn mountinfo_parses_real_output_including_escapes() {
        let t = "36 25 259:7 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p7 rw,errors=remount-ro\n\
                 41 26 7:14 / /snap/firefox/8863 ro,nodev,relatime shared:5 - squashfs /dev/loop14 ro\n\
                 55 25 0:52 / /mnt/my\\040disk rw,relatime shared:9 - ext4 /dev/sdb1 rw\n";
        let m = parse_mountinfo(t);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].mount, "/");
        assert_eq!(m[0].fstype, "ext4");
        assert!(!m[0].read_only);
        assert!(m[1].read_only);
        assert_eq!(m[2].mount, "/mnt/my disk", "octal escape must be decoded");
    }

    #[test]
    fn diskstats_keeps_whole_devices_and_drops_partitions_and_loops() {
        let t = " 259  0 nvme0n1 53472 16145 5900162 13861 61512 71546 5769610 2953143 0 13543 2968843 0 0 0 0\n\
                  259  7 nvme0n1p7 52763 15731 5831530 13730 61509 71546 5769608 2953142 0 14002 2966872 0 0\n\
                    7  0 loop0 100 0 200 5 0 0 0 0 0 5 5 0 0 0 0\n\
                    8  0 sda 10 0 20 1 2 0 4 1 0 1 1 0 0 0 0\n\
                    8  1 sda1 5 0 10 1 1 0 2 1 0 1 1 0 0 0 0\n";
        let d = parse_diskstats(t);
        assert!(d.contains_key("nvme0n1"), "whole NVMe device missing");
        assert!(!d.contains_key("nvme0n1p7"), "partition should be excluded");
        assert!(!d.contains_key("loop0"), "loop device should be excluded");
        assert!(d.contains_key("sda"));
        assert!(!d.contains_key("sda1"));
        assert_eq!(d["nvme0n1"].read_sectors, 5_900_162);
        assert_eq!(d["nvme0n1"].io_ticks_ms, 13_543);
    }

    #[test]
    fn diskstats_short_or_garbage_lines_are_skipped() {
        assert!(parse_diskstats("garbage\n1 2 3\n").is_empty());
    }

    #[test]
    fn reads_this_machines_real_storage() {
        let mut c = StorageCollector::new();
        assert!(c.probe().is_usable());
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let s = &x.snap.storage;
        assert!(!s.filesystems.is_empty(), "root filesystem not found");
        assert!(s.filesystems.iter().any(|f| f.mount == "/"));
        // The 37 snap mounts on this machine must have been filtered.
        assert!(s.hidden_mounts > 10, "expected many hidden pseudo mounts, got {}", s.hidden_mounts);
        for f in &s.filesystems {
            assert!(f.used_pct >= 0.0 && f.used_pct <= 100.0, "{} at {}%", f.mount, f.used_pct);
            assert!(!f.fstype.is_empty());
        }
        assert!(!s.nvme.is_empty(), "NVMe controller not found");
        assert!(s.nvme[0].temp_c.is_some(), "NVMe temperature should be readable unprivileged");
    }

    #[test]
    fn statvfs_of_a_missing_path_is_none() {
        assert!(statvfs("/definitely/not/a/mount/point").is_none());
    }
}
