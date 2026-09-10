//! Bluetooth, audio, USB and cameras.
//!
//! All presence-and-health, no device interrogation: the point is to notice when
//! internal hardware disappears or a service dies, not to enumerate capabilities.

use super::{Collector, Ctx, ExternalEvent};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Serialize)]
pub struct BluetoothState {
    pub present: bool,
    pub adapter: String,
    pub address: String,
    pub powered: bool,
    pub soft_blocked: bool,
    pub hard_blocked: bool,
    pub connected_devices: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioState {
    pub pipewire: String,
    pub wireplumber: String,
    pub pipewire_pulse: String,
    pub healthy: bool,
    pub cards: Vec<String>,
    pub restarts_seen: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsbDevice {
    pub id: String,
    pub vendor: String,
    pub product: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceState {
    pub bluetooth: BluetoothState,
    pub audio: AudioState,
    pub usb: Vec<UsbDevice>,
    pub cameras: Vec<String>,
    pub webcam_present: bool,
    pub ir_camera_present: bool,
    /// Devices learned as "normally present" that are currently missing.
    pub missing_expected: Vec<String>,
}

pub struct DeviceCollector {
    /// Learned inventory of internal devices, built during the first minutes of
    /// running. Only devices seen consistently become "expected", so plugging in a
    /// USB stick once does not create a permanent false alarm.
    expected: BTreeSet<String>,
    sightings: std::collections::HashMap<String, u32>,
    learn_ticks: u32,
    prev_pipewire_pid: Option<String>,
    restarts: u32,
}

impl DeviceCollector {
    pub fn new() -> Self {
        DeviceCollector {
            expected: BTreeSet::new(),
            sightings: Default::default(),
            learn_ticks: 0,
            prev_pipewire_pid: None,
            restarts: 0,
        }
    }

    /// Seen on at least this many consecutive slow ticks (~10 min) to count as
    /// permanently attached.
    const LEARN_THRESHOLD: u32 = 10;

    pub fn expected_devices(&self) -> &BTreeSet<String> {
        &self.expected
    }
}

impl Default for DeviceCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Read /dev/rfkill state without an ioctl: sysfs exposes the same two flags.
fn rfkill_state(kind: &str) -> Option<(bool, bool)> {
    for e in list_dir("/sys/class/rfkill") {
        let base = format!("/sys/class/rfkill/{e}");
        if read_str(format!("{base}/type")).as_deref() == Some(kind) {
            let soft = read_i64(format!("{base}/soft")).unwrap_or(0) == 1;
            let hard = read_i64(format!("{base}/hard")).unwrap_or(0) == 1;
            return Some((soft, hard));
        }
    }
    None
}

fn systemd_user_active(unit: &str) -> String {
    // `systemctl --user is-active` would be a process spawn per unit per tick. The
    // same answer is in the unit's cgroup presence, which is a single stat.
    let uid = unsafe { libc::getuid() };
    let p = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/session.slice/{unit}");
    let p2 = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{unit}");
    if exists(&p) || exists(&p2) {
        "active".into()
    } else {
        // Fall back to looking for the process, which covers non-systemd sessions.
        if process_exists(unit.trim_end_matches(".service")) { "active".into() } else { "inactive".into() }
    }
}

fn process_exists(name: &str) -> bool {
    for e in list_dir("/proc") {
        if !e.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(comm) = read_str(format!("/proc/{e}/comm")) {
            if comm == name {
                return true;
            }
        }
    }
    false
}

fn pipewire_pid() -> Option<String> {
    for e in list_dir("/proc") {
        if !e.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if read_str(format!("/proc/{e}/comm")).as_deref() == Some("pipewire") {
            return Some(e);
        }
    }
    None
}

impl Collector for DeviceCollector {
    fn name(&self) -> &'static str {
        "devices"
    }
    fn tier(&self) -> Tier {
        Tier::Slow
    }

    fn probe(&mut self) -> Support {
        let bt = !list_dir("/sys/class/bluetooth").is_empty();
        let audio = exists("/sys/class/sound");
        let usb = exists("/sys/bus/usb/devices");
        let cam = exists("/sys/class/video4linux");
        let mut missing = Vec::new();
        if !bt {
            missing.push("Bluetooth");
        }
        if !cam {
            missing.push("camera");
        }
        if !audio && !usb {
            return Support::Unsupported { reason: "no device subsystems present".into() };
        }
        if missing.is_empty() {
            Support::Full
        } else {
            Support::Partial { detail: format!("absent: {}", missing.join(", ")) }
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = DeviceState::default();
        let mut present_now: BTreeSet<String> = BTreeSet::new();

        // ---- Bluetooth ----------------------------------------------------
        let adapters: Vec<String> =
            list_dir("/sys/class/bluetooth").into_iter().filter(|e| !e.contains(':')).collect();
        if let Some(a) = adapters.first() {
            let base = format!("/sys/class/bluetooth/{a}");
            let (soft, hard) = rfkill_state("bluetooth").unwrap_or((false, false));
            // A connection appears as a sibling node named `hciN:M`.
            let conns = list_dir("/sys/class/bluetooth")
                .into_iter()
                .filter(|e| e.starts_with(&format!("{a}:")))
                .count();
            st.bluetooth = BluetoothState {
                present: true,
                adapter: a.clone(),
                address: read_str(format!("{base}/address")).unwrap_or_default(),
                powered: !soft && !hard,
                soft_blocked: soft,
                hard_blocked: hard,
                connected_devices: conns,
            };
            ctx.g("devices", "bt_connected", "", conns as f64);
            present_now.insert(format!("bluetooth:{a}"));
        }

        // ---- audio ---------------------------------------------------------
        let mut a = AudioState {
            pipewire: systemd_user_active("pipewire.service"),
            wireplumber: systemd_user_active("wireplumber.service"),
            pipewire_pulse: systemd_user_active("pipewire-pulse.service"),
            ..Default::default()
        };
        a.healthy = a.pipewire == "active" && a.wireplumber == "active";
        for c in list_dir("/sys/class/sound") {
            if c.starts_with("card") && !c.contains('D') && !c.contains('C') {
                let id = read_str(format!("/sys/class/sound/{c}/id")).unwrap_or_else(|| c.clone());
                a.cards.push(format!("{c} ({id})"));
                present_now.insert(format!("sound:{id}"));
            }
        }
        // A changed PID means PipeWire restarted, which users experience as audio
        // cutting out; the service state alone would look healthy either side of it.
        let pid = pipewire_pid();
        if let (Some(prev), Some(cur)) = (&self.prev_pipewire_pid, &pid) {
            if prev != cur {
                self.restarts += 1;
                ctx.event(Event::new("devices", "pipewire_restart", "PipeWire restarted", Severity::Notice));
            }
        }
        if pid.is_some() {
            self.prev_pipewire_pid = pid;
        }
        a.restarts_seen = self.restarts;
        st.audio = a;

        // ---- USB ------------------------------------------------------------
        for e in list_dir("/sys/bus/usb/devices") {
            // Skip interfaces (`1-14:1.0`) and root hubs (`usb1`).
            if e.contains(':') || e.starts_with("usb") {
                continue;
            }
            let base = format!("/sys/bus/usb/devices/{e}");
            let (Some(v), Some(p)) = (read_str(format!("{base}/idVendor")), read_str(format!("{base}/idProduct")))
            else {
                continue;
            };
            let name = read_str(format!("{base}/product")).unwrap_or_default();
            let id = format!("{v}:{p}");
            present_now.insert(format!("usb:{id}"));
            st.usb.push(UsbDevice {
                id: id.clone(),
                vendor: read_str(format!("{base}/manufacturer")).unwrap_or_default(),
                product: p,
                name,
            });
        }

        // ---- cameras ---------------------------------------------------------
        for v in list_dir("/sys/class/video4linux") {
            if let Some(n) = read_str(format!("/sys/class/video4linux/{v}/name")) {
                let lower = n.to_ascii_lowercase();
                if lower.contains("ir ") || lower.contains("ir camera") {
                    st.ir_camera_present = true;
                } else {
                    st.webcam_present = true;
                }
                st.cameras.push(format!("{v}: {n}"));
                present_now.insert(format!("video:{n}"));
            }
        }

        // ---- learned expectations ---------------------------------------------
        self.learn_ticks += 1;
        for d in &present_now {
            *self.sightings.entry(d.clone()).or_insert(0) += 1;
        }
        for (d, n) in &self.sightings {
            if *n >= Self::LEARN_THRESHOLD {
                self.expected.insert(d.clone());
            }
        }
        for d in &self.expected {
            if !present_now.contains(d) {
                st.missing_expected.push(d.clone());
            }
        }

        ctx.snap.devices = st;
        Ok(())
    }

    fn on_event(&mut self, ev: &ExternalEvent, ctx: &mut Ctx) -> CResult<()> {
        if let ExternalEvent::Uevent { action, subsystem, devpath } = ev {
            if subsystem == "usb" || subsystem == "video4linux" || subsystem == "bluetooth" {
                let sev = if action == "remove" { Severity::Notice } else { Severity::Info };
                ctx.event(
                    Event::new("devices", &format!("{subsystem}_{action}"),
                               format!("{subsystem} device {action}: {devpath}"), sev)
                        .with_detail(serde_json::json!({"action": action, "subsystem": subsystem})),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn finds_this_machines_devices() {
        let mut c = DeviceCollector::new();
        assert!(c.probe().is_usable());
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let d = &x.snap.devices;
        // Verified present during discovery.
        assert!(d.bluetooth.present, "hci0 should be detected");
        assert!(d.webcam_present, "ASUS FHD webcam should be detected");
        assert!(d.ir_camera_present, "ASUS IR camera should be detected");
        assert!(!d.usb.is_empty());
        assert!(!d.audio.cards.is_empty());
        assert!(d.audio.healthy, "PipeWire and WirePlumber are active on this machine");
    }

    #[test]
    fn expectations_need_repeated_sightings() {
        let mut c = DeviceCollector::new();
        c.probe();
        let cfg = Arc::new(Config::default());
        let mut x = Ctx::new(cfg.clone());
        c.collect(&mut x).unwrap();
        assert!(
            c.expected_devices().is_empty(),
            "a single sighting must not create a permanent expectation"
        );
        for _ in 0..DeviceCollector::LEARN_THRESHOLD {
            let mut y = Ctx::new(cfg.clone());
            c.collect(&mut y).unwrap();
        }
        assert!(!c.expected_devices().is_empty(), "steady devices should be learned");
        // Nothing was unplugged during the test, so nothing may be reported missing.
        let mut z = Ctx::new(cfg);
        c.collect(&mut z).unwrap();
        assert!(z.snap.devices.missing_expected.is_empty());
    }

    #[test]
    fn bluetooth_connections_are_counted_from_child_nodes() {
        // `hci0:1` is a connection; `hci0` is the adapter.
        let entries = vec!["hci0".to_string(), "hci0:1".to_string(), "hci0:2".to_string()];
        let adapters: Vec<&String> = entries.iter().filter(|e| !e.contains(':')).collect();
        assert_eq!(adapters.len(), 1);
        let conns = entries.iter().filter(|e| e.starts_with("hci0:")).count();
        assert_eq!(conns, 2);
    }
}
