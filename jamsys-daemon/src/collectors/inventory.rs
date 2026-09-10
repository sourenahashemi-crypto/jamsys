//! Hardware and software inventory, and the diff after an update.
//!
//! Runs on the glacial tier. The value is not the list itself — it is being able to
//! answer "what changed?" after a kernel or driver upgrade breaks something.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use std::collections::BTreeMap;

pub struct InventoryCollector {
    /// Last snapshot, so only differences are emitted as events.
    last: BTreeMap<String, String>,
    first_run: bool,
}

impl InventoryCollector {
    pub fn new() -> Self {
        InventoryCollector { last: BTreeMap::new(), first_run: true }
    }
}

impl Default for InventoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert only non-empty, trimmed values, so a driver returning "" never creates a
/// phantom inventory entry that then "disappears" on the next pass.
fn put(m: &mut BTreeMap<String, String>, k: &str, v: Option<String>) {
    if let Some(v) = v {
        let v = v.trim().to_string();
        if !v.is_empty() {
            m.insert(k.to_string(), v);
        }
    }
}

/// Collect the full inventory. Public so the first-run report and tests can use it.
pub fn gather() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();

    // --- system ---
    put(&mut m, "kernel.release", read_str("/proc/sys/kernel/osrelease"));
    put(&mut m, "kernel.version", read_str("/proc/sys/kernel/version"));
    put(&mut m, "hostname", read_str("/proc/sys/kernel/hostname"));
    if let Some(os) = read_str("/etc/os-release") {
        for line in os.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                m.insert("distro".into(), v.trim_matches('"').to_string());
            }
        }
    }
    // --- firmware / board ---
    for (k, f) in [
        ("bios.version", "bios_version"),
        ("bios.date", "bios_date"),
        ("board.name", "board_name"),
        ("board.vendor", "sys_vendor"),
        ("product.name", "product_name"),
        ("chassis.type", "chassis_type"),
    ] {
        put(&mut m, k, read_str(format!("/sys/class/dmi/id/{f}")));
    }
    // --- CPU ---
    if let Some(ci) = read_str("/proc/cpuinfo") {
        for line in ci.lines() {
            if let Some((k, v)) = line.split_once(':') {
                match k.trim() {
                    "model name" if !m.contains_key("cpu.model") => {
                        m.insert("cpu.model".into(), v.trim().to_string());
                    }
                    "microcode" if !m.contains_key("cpu.microcode") => {
                        m.insert("cpu.microcode".into(), v.trim().to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    let ncpu = list_dir("/sys/devices/system/cpu")
        .iter()
        .filter(|d| d.len() > 3 && d.starts_with("cpu") && d[3..].chars().all(|c| c.is_ascii_digit()))
        .count();
    m.insert("cpu.logical".into(), ncpu.to_string());
    // --- memory ---
    if let Some(mi) = read_str("/proc/meminfo") {
        if let Some(t) = parse_kv_kb(&mi).get("MemTotal") {
            m.insert("memory.total_gb".into(), format!("{:.1}", *t as f64 / 1048576.0));
        }
    }
    // --- GPUs ---
    for e in list_dir("/sys/class/drm") {
        if !e.starts_with("card") || e.contains('-') {
            continue;
        }
        let base = format!("/sys/class/drm/{e}/device");
        let drv = std::fs::read_link(format!("{base}/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let vend = read_str(format!("{base}/vendor")).unwrap_or_default();
        let dev = read_str(format!("{base}/device")).unwrap_or_default();
        if let Some(d) = drv {
            m.insert(format!("gpu.{e}.driver"), d);
            m.insert(format!("gpu.{e}.pci_id"), format!("{vend}:{dev}"));
        }
    }
    put(&mut m, "gpu.nvidia.driver_version", read_str("/proc/driver/nvidia/version").map(|v| {
        v.split_whitespace()
            .find(|t| t.contains('.') && t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .to_string()
    }));
    // --- disks ---
    for e in list_dir("/sys/class/nvme") {
        put(&mut m, &format!("disk.{e}.model"), read_str(format!("/sys/class/nvme/{e}/model")));
        put(&mut m, &format!("disk.{e}.firmware"), read_str(format!("/sys/class/nvme/{e}/firmware_rev")));
    }
    for e in list_dir("/sys/block") {
        if e.starts_with("loop") || e.starts_with("ram") || e.starts_with("zram") {
            continue;
        }
        if let Some(sz) = read_u64(format!("/sys/block/{e}/size")) {
            m.insert(format!("disk.{e}.size_gb"), format!("{:.1}", sz as f64 * 512.0 / 1e9));
        }
    }
    // --- network adapters ---
    for e in list_dir("/sys/class/net") {
        if e == "lo" {
            continue;
        }
        if let Some(d) = std::fs::read_link(format!("/sys/class/net/{e}/device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        {
            m.insert(format!("net.{e}.driver"), d);
        }
    }
    // --- audio / camera / battery ---
    for c in list_dir("/sys/class/sound") {
        if c.starts_with("card") && !c.contains('D') && !c.contains('C') {
            put(&mut m, &format!("audio.{c}.id"), read_str(format!("/sys/class/sound/{c}/id")));
        }
    }
    for v in list_dir("/sys/class/video4linux") {
        put(&mut m, &format!("camera.{v}"), read_str(format!("/sys/class/video4linux/{v}/name")));
    }
    for b in list_dir("/sys/class/power_supply") {
        let base = format!("/sys/class/power_supply/{b}");
        if read_str(format!("{base}/type")).as_deref() == Some("Battery") {
            put(&mut m, &format!("battery.{b}.model"), read_str(format!("{base}/model_name")));
            put(&mut m, &format!("battery.{b}.manufacturer"), read_str(format!("{base}/manufacturer")));
            put(&mut m, &format!("battery.{b}.technology"), read_str(format!("{base}/technology")));
            if let Some(d) = read_scaled(format!("{base}/energy_full_design"), 1e6, 0.0, 1e4) {
                m.insert(format!("battery.{b}.design_wh"), format!("{d:.1}"));
            }
        }
    }
    // --- key kernel modules (drivers whose version changes break things) ---
    if let Some(mods) = read_str("/proc/modules") {
        let interesting = ["nvidia", "i915", "xe", "rtw89_8852ce", "r8169", "btusb", "snd_hda_intel", "nvme"];
        for line in mods.lines() {
            if let Some(name) = line.split_whitespace().next() {
                if interesting.contains(&name) {
                    let ver = read_str(format!("/sys/module/{name}/version")).unwrap_or_else(|| "loaded".into());
                    m.insert(format!("module.{name}"), ver);
                }
            }
        }
    }
    m
}

impl Collector for InventoryCollector {
    fn name(&self) -> &'static str {
        "inventory"
    }
    fn tier(&self) -> Tier {
        Tier::Glacial
    }

    fn probe(&mut self) -> Support {
        if exists("/proc/cpuinfo") {
            Support::Full
        } else {
            Support::Unsupported { reason: "/proc unavailable".into() }
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let now = gather();
        if !self.first_run {
            for (k, v) in &now {
                match self.last.get(k) {
                    Some(old) if old != v => {
                        ctx.event(
                            Event::new("inventory", "changed", format!("{k}: {old} → {v}"), Severity::Notice)
                                .with_detail(serde_json::json!({"key": k, "old": old, "new": v})),
                        );
                    }
                    None => {
                        ctx.event(
                            Event::new("inventory", "added", format!("{k} appeared: {v}"), Severity::Info)
                                .with_detail(serde_json::json!({"key": k, "new": v})),
                        );
                    }
                    _ => {}
                }
            }
            for k in self.last.keys() {
                if !now.contains_key(k) {
                    ctx.event(
                        Event::new("inventory", "removed", format!("{k} disappeared"), Severity::Notice)
                            .with_detail(serde_json::json!({"key": k})),
                    );
                }
            }
        }
        self.first_run = false;
        self.last = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn gathers_this_machines_identity() {
        let inv = gather();
        assert!(inv.contains_key("kernel.release"));
        assert!(inv.contains_key("cpu.model"));
        assert!(inv.contains_key("distro"));
        assert!(inv["cpu.model"].contains("Intel") || inv["cpu.model"].contains("AMD"));
        assert!(inv.contains_key("memory.total_gb"));
        // Verified during discovery on this machine.
        assert_eq!(inv.get("bios.version").map(|s| s.as_str()), Some("FX608JMR.312"));
        assert!(inv.keys().any(|k| k.starts_with("gpu.")), "no GPU recorded");
        assert!(inv.keys().any(|k| k.starts_with("battery.")), "no battery recorded");
    }

    #[test]
    fn first_run_is_silent_and_later_changes_are_reported() {
        let mut c = InventoryCollector::new();
        c.probe();
        let cfg = Arc::new(Config::default());
        let mut x = Ctx::new(cfg.clone());
        c.collect(&mut x).unwrap();
        assert!(x.events.is_empty(), "the first inventory must not report everything as new");

        // Simulate a kernel upgrade.
        c.last.insert("kernel.release".into(), "6.0.0-1-generic".into());
        let mut y = Ctx::new(cfg);
        c.collect(&mut y).unwrap();
        assert!(
            y.events.iter().any(|e| e.summary.contains("kernel.release")),
            "a kernel change must be reported, got {:?}",
            y.events.iter().map(|e| &e.summary).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unchanged_system_produces_no_events() {
        let mut c = InventoryCollector::new();
        c.probe();
        let cfg = Arc::new(Config::default());
        c.collect(&mut Ctx::new(cfg.clone())).unwrap();
        let mut y = Ctx::new(cfg);
        c.collect(&mut y).unwrap();
        assert!(y.events.is_empty(), "stable inventory should be quiet, got {:?}", y.events);
    }
}
