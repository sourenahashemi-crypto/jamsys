//! Bluetooth, from BlueZ over the system bus.
//!
//! Why this is separate from `devices`
//! -----------------------------------
//! `devices` answers "is there a radio, is it blocked, how many links are up" from
//! sysfs alone, which works with BlueZ absent or stopped. That is the right fallback
//! but it cannot say *which* device dropped, and a count that goes 1 -> 0 -> 1
//! between two samples is invisible.
//!
//! This collector answers the question the user actually has when headphones cut out:
//! **what disconnected, when, and what else was true at that moment.** It is
//! event-driven — BlueZ emits `PropertiesChanged` the instant a link drops, so a flap
//! shorter than the sampling interval is still caught. The periodic `collect()` is
//! only a reconciliation pass in case a signal was missed.
//!
//! Cause attribution is deliberately conservative. The kernel does not expose an HCI
//! disconnect reason to an unprivileged process, so this records what was
//! *observable* at the moment of the drop and names a likely cause only when a
//! specific precondition held. Anything else is reported as "reason not observable",
//! which is more useful than a confident guess that is wrong.

use super::{Collector, Ctx, Support, Tier};
use crate::types::{CResult, CollectorError};
use crate::dbus::{Connection, Value};
use crate::sysfs::{list_dir, read_str, read_u64};
use serde::Serialize;
use std::collections::BTreeMap;
use std::os::unix::io::RawFd;

/// How many disconnect records to keep. Enough to see a pattern, small enough that the
/// snapshot stays cheap to serialise on every push.
const MAX_EVENTS: usize = 32;

/// Disconnects within this window count towards "flapping".
pub const FLAP_WINDOW_MS: i64 = 15 * 60 * 1000;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct BtDevice {
    pub address: String,
    pub name: String,
    /// BlueZ's own icon name: audio-headset, input-mouse, phone, ...
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    /// From `org.bluez.Battery1`, when the device publishes it.
    pub battery: Option<u8>,
}

impl BtDevice {
    /// What to call this device in a notification. Never the bare address if a name
    /// is known, because "AXR100 disconnected" is actionable and "D8:19:… " is not.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.address
        } else {
            &self.name
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BtEventKind {
    Connected,
    Disconnected,
}

/// What was observably true at the instant of a disconnect.
///
/// Every field is something read at that moment, not inferred afterwards.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct BtContext {
    /// The radio's USB runtime power state, e.g. "active" or "suspended".
    pub radio_runtime_status: String,
    /// Whether USB autosuspend is permitted for the radio ("auto" vs "on").
    pub radio_power_control: String,
    /// Total time the radio has spent runtime-suspended, in ms.
    pub radio_suspended_ms: u64,
    /// True when that counter moved since the previous observation.
    pub radio_suspended_recently: bool,
    pub rfkill_soft: bool,
    pub rfkill_hard: bool,
    pub adapter_powered: bool,
    /// Whether the wireless device is allowed to runtime-suspend
    /// (`/sys/class/net/*/device/power/control` == "auto").
    ///
    /// This is *not* 802.11 power save, which lives behind nl80211 and is not read
    /// here. Named for what it measures, so nothing downstream over-claims.
    pub wifi_runtime_pm: Option<bool>,
    /// True when the machine resumed from suspend within the last 30 s.
    pub just_resumed: bool,
    /// bluetoothd's PID changed since the last observation.
    pub bluetoothd_restarted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BtEvent {
    /// Wall clock, for display and for the stored history.
    pub at_ms: i64,
    /// Monotonic, for every window and age comparison.
    ///
    /// These are two different clocks and mixing them produces nonsense: the rule
    /// engine is driven by the monotonic clock, so an age computed against a
    /// wall-clock stamp is meaningless and a hold window built from it never expires.
    pub at_mono_ms: i64,
    pub kind: BtEventKind,
    pub address: String,
    pub name: String,
    /// Only meaningful for a disconnect.
    pub context: BtContext,
    /// A cause only when a precondition actually held, else None.
    pub likely_cause: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BtState {
    /// True when BlueZ answered. False means the sysfs view in `devices` is all there
    /// is, and no per-device reporting is possible.
    pub bluez_available: bool,
    pub adapter_powered: bool,
    pub devices: Vec<BtDevice>,
    pub connected_count: usize,
    /// Newest last.
    pub events: Vec<BtEvent>,
    /// address -> disconnects inside FLAP_WINDOW_MS.
    pub recent_disconnects: BTreeMap<String, u32>,
    /// Standing advice derived from configuration that is known to cause drops on
    /// this hardware. Empty when nothing is misconfigured.
    pub risk_notes: Vec<String>,
    /// The last thing bluetoothd complained about, in plain words. BlueZ logs
    /// connection failures even though it logs nothing for a clean disconnect, and
    /// "the headset is not responding" is the answer the user actually wants.
    pub last_stack_error: Option<String>,
}

pub struct BluetoothCollector {
    /// Blocking, with a read timeout: used for GetManagedObjects and GetNameOwner.
    conn: Option<Connection>,
    /// Non-blocking, watched by the daemon's epoll: used only as a wake-up. Its
    /// messages are drained and discarded, because the authoritative state always
    /// comes from a fresh enumeration on `conn`.
    ///
    /// Two sockets rather than one because `call()` reads the socket directly while
    /// `try_read_messages()` reads through an internal buffer; mixing them on one
    /// connection corrupts message framing the moment a signal interleaves a reply.
    sig: Option<Connection>,
    /// address -> last known device record, for transition detection.
    last: BTreeMap<String, BtDevice>,
    events: Vec<BtEvent>,
    /// The radio's suspended-time counter at the previous observation.
    prev_radio_suspended_ms: u64,
    /// BlueZ's unique bus name. It changes if and only if bluetoothd restarted, so
    /// this detects a service restart exactly, with no process scanning.
    prev_bluez_owner: Option<String>,
    /// Monotonic ms of the last resume, for the `just_resumed` precondition.
    last_resume_ms: i64,
    /// Set once so the first enumeration does not report every paired device as a
    /// brand-new connection.
    primed: bool,
    /// Most recent bluetoothd complaint, already translated.
    stack_error: Option<String>,
}

impl BluetoothCollector {
    pub fn new() -> Self {
        BluetoothCollector {
            conn: None,
            sig: None,
            last: BTreeMap::new(),
            events: Vec::new(),
            prev_radio_suspended_ms: 0,
            prev_bluez_owner: None,
            last_resume_ms: i64::MIN / 4,
            primed: false,
            stack_error: None,
        }
    }

    /// The signal socket's fd, so the daemon can wake on BlueZ signals rather than
    /// discovering a drop up to one sampling interval late.
    pub fn fd(&self) -> Option<RawFd> {
        self.sig.as_ref().map(|c| c.as_raw_fd())
    }

    fn open(&mut self) -> bool {
        if self.conn.is_none() {
            match Connection::system() {
                Ok(c) => {
                    // Bounded, so a wedged bus cannot stall the single-threaded loop.
                    c.set_timeout(Some(std::time::Duration::from_secs(2)));
                    self.conn = Some(c);
                }
                Err(_) => return false,
            }
        }
        if self.sig.is_none() {
            if let Ok(mut c) = Connection::system() {
                // Everything BlueZ says about devices appearing, changing, going away.
                for rule in [
                    "type='signal',sender='org.bluez',\
                     interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'",
                    "type='signal',sender='org.bluez',\
                     interface='org.freedesktop.DBus.ObjectManager',member='InterfacesAdded'",
                    "type='signal',sender='org.bluez',\
                     interface='org.freedesktop.DBus.ObjectManager',member='InterfacesRemoved'",
                ] {
                    let _ = c.add_match(rule);
                }
                if c.set_nonblocking().is_ok() {
                    self.sig = Some(c);
                }
            }
            // A missing signal socket is not fatal: the Medium tier still reconciles.
        }
        self.conn.is_some()
    }
}

/// Pull the device list out of a `GetManagedObjects` reply.
///
/// The reply is `a{oa{sa{sv}}}`: object path -> interface -> property -> value. Only
/// paths carrying `org.bluez.Device1` are devices; `org.bluez.Battery1` on the same
/// path carries the charge.
pub fn parse_managed_objects(v: &Value) -> (Vec<BtDevice>, bool) {
    let mut out = Vec::new();
    let mut adapter_powered = false;
    let Some(objects) = v.as_array() else {
        return (out, adapter_powered);
    };
    for obj in objects {
        let Value::DictEntry(_path, ifaces) = obj else { continue };
        let Some(iface_list) = ifaces.as_array() else { continue };

        let mut dev: Option<BtDevice> = None;
        let mut battery: Option<u8> = None;
        for ie in iface_list {
            let Value::DictEntry(iname, props) = ie else { continue };
            let Some(iname) = iname.as_str() else { continue };
            match iname {
                "org.bluez.Device1" => {
                    let mut d = BtDevice::default();
                    for p in props.as_array().unwrap_or(&[]) {
                        let Value::DictEntry(k, val) = p else { continue };
                        match k.as_str().unwrap_or("") {
                            "Address" => d.address = val.as_str().unwrap_or("").to_string(),
                            // Alias is what the user renamed it to; prefer it.
                            "Alias" => d.name = val.as_str().unwrap_or("").to_string(),
                            "Name" if d.name.is_empty() => {
                                d.name = val.as_str().unwrap_or("").to_string()
                            }
                            "Icon" => d.icon = val.as_str().unwrap_or("").to_string(),
                            "Paired" => d.paired = as_bool(val).unwrap_or(false),
                            "Connected" => d.connected = as_bool(val).unwrap_or(false),
                            _ => {}
                        }
                    }
                    if !d.address.is_empty() {
                        dev = Some(d);
                    }
                }
                "org.bluez.Battery1" => {
                    for p in props.as_array().unwrap_or(&[]) {
                        let Value::DictEntry(k, val) = p else { continue };
                        if k.as_str() == Some("Percentage") {
                            battery = as_u8(val);
                        }
                    }
                }
                "org.bluez.Adapter1" => {
                    for p in props.as_array().unwrap_or(&[]) {
                        let Value::DictEntry(k, val) = p else { continue };
                        if k.as_str() == Some("Powered") && as_bool(val) == Some(true) {
                            adapter_powered = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(mut d) = dev {
            d.battery = battery;
            out.push(d);
        }
    }
    out.sort_by(|a, b| a.address.cmp(&b.address));
    (out, adapter_powered)
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Variant(inner) => as_bool(inner),
        _ => None,
    }
}

fn as_u8(v: &Value) -> Option<u8> {
    match v {
        Value::Byte(b) => Some(*b),
        Value::U32(n) => u8::try_from(*n).ok(),
        Value::Variant(inner) => as_u8(inner),
        _ => None,
    }
}

/// Locate the Bluetooth radio's USB node, so its power management can be read.
fn radio_usb_path() -> Option<String> {
    for e in list_dir("/sys/bus/usb/devices") {
        let base = format!("/sys/bus/usb/devices/{e}");
        let product = read_str(&format!("{base}/product")).unwrap_or_default();
        if product.to_lowercase().contains("bluetooth") {
            return Some(base);
        }
    }
    None
}

impl BluetoothCollector {
    fn observe_context(&mut self) -> BtContext {
        let mut c = BtContext::default();
        if let Some(base) = radio_usb_path() {
            c.radio_runtime_status =
                read_str(&format!("{base}/power/runtime_status")).unwrap_or_default();
            c.radio_power_control =
                read_str(&format!("{base}/power/control")).unwrap_or_default();
            c.radio_suspended_ms =
                read_u64(&format!("{base}/power/runtime_suspended_time")).unwrap_or(0);
            c.radio_suspended_recently = c.radio_suspended_ms > self.prev_radio_suspended_ms;
            self.prev_radio_suspended_ms = c.radio_suspended_ms;
        }
        let (soft, hard) = rfkill_bluetooth();
        c.rfkill_soft = soft;
        c.rfkill_hard = hard;
        c.wifi_runtime_pm = wifi_runtime_pm();
        c.just_resumed = crate::clock::mono_ms() - self.last_resume_ms < 30_000;
        c
    }
}

/// Soft/hard block for the Bluetooth rfkill switch.
fn rfkill_bluetooth() -> (bool, bool) {
    for e in list_dir("/sys/class/rfkill") {
        let base = format!("/sys/class/rfkill/{e}");
        if read_str(&format!("{base}/type")).as_deref() == Some("bluetooth") {
            let soft = read_u64(&format!("{base}/soft")).unwrap_or(0) == 1;
            let hard = read_u64(&format!("{base}/hard")).unwrap_or(0) == 1;
            return (soft, hard);
        }
    }
    (false, false)
}

/// Whether the wireless device may runtime-suspend.
///
/// 802.11 power save proper is an nl80211 attribute and is deliberately not guessed
/// at from sysfs; `iw dev X get power_save` is the only way to read it and this
/// daemon does not spawn processes on a timer.
fn wifi_runtime_pm() -> Option<bool> {
    for e in list_dir("/sys/class/net") {
        let base = format!("/sys/class/net/{e}");
        if !std::path::Path::new(&format!("{base}/wireless")).exists() {
            continue;
        }
        if let Some(v) = read_str(&format!("{base}/device/power/control")) {
            return Some(v == "auto");
        }
    }
    None
}

/// Name the cause only when a specific precondition held.
///
/// The ordering is by confidence: a blocked radio or a resume is a fact, autosuspend
/// is a strong correlation, coexistence is a hypothesis and is worded as one.
pub fn attribute(c: &BtContext) -> Option<String> {
    if c.rfkill_hard {
        return Some("the Bluetooth radio is hard-blocked (airplane mode or the radio switch)".into());
    }
    if c.rfkill_soft {
        return Some("the Bluetooth radio is soft-blocked (turned off in software)".into());
    }
    if !c.adapter_powered {
        return Some("the adapter was powered down".into());
    }
    if c.just_resumed {
        return Some("the machine had just resumed from suspend".into());
    }
    if c.bluetoothd_restarted {
        return Some("the Bluetooth service restarted".into());
    }
    if c.radio_suspended_recently || c.radio_runtime_status == "suspended" {
        return Some(
            "the radio was USB-autosuspended around the drop — \
             a known cause of dropouts on Realtek combo adapters"
                .into(),
        );
    }
    if c.radio_power_control == "auto" && c.wifi_runtime_pm == Some(true) {
        return Some(
            "possibly Wi-Fi/Bluetooth coexistence: both radios are allowed to \
             power-manage themselves. Not confirmed — the kernel does not expose \
             a disconnect reason to an unprivileged process"
                .into(),
        );
    }
    None
}

/// Configuration on this machine that is known to cause dropouts.
pub fn risk_notes(c: &BtContext) -> Vec<String> {
    let mut v = Vec::new();
    if c.radio_power_control == "auto" {
        v.push(
            "USB autosuspend is enabled on the Bluetooth radio \
             (power/control = auto). This is a common cause of audio dropouts."
                .into(),
        );
    }
    if c.wifi_runtime_pm == Some(true) {
        v.push(
            "The wireless device is allowed to runtime-suspend. On a combo \
             Wi-Fi/Bluetooth chip this adds coexistence pressure and can interrupt \
             audio. Check 802.11 power save too: iw dev <iface> get power_save"
                .into(),
        );
    }
    v
}

/// Translate a bluetoothd log line into something a person can act on.
///
/// BlueZ logs nothing at all for an ordinary disconnect — verified on this machine,
/// where a real drop left no journal entry — but it does log failures to (re)connect,
/// and those carry the useful part.
pub fn translate_stack_error(msg: &str) -> Option<String> {
    let m = msg.to_ascii_lowercase();
    if !m.contains("avdtp") && !m.contains("a2dp") && !m.contains("connect") && !m.contains("bluetooth") {
        return None;
    }
    if m.contains("host is down") {
        return Some("the device is not responding — it is probably switched off, \
                     asleep or out of range"
            .into());
    }
    if m.contains("page-timeout") || m.contains("page timeout") {
        return Some("the device did not answer the connection attempt (page timeout) \
                     — switch it off and on again to wake its radio"
            .into());
    }
    if m.contains("connection refused") {
        return Some("the device refused the connection — it may be connected to \
                     something else"
            .into());
    }
    if m.contains("connection timeout") || m.contains("timed out") {
        return Some("the connection timed out".into());
    }
    if m.contains("protocol not available") || m.contains("unable to get") {
        return Some("the device did not offer the audio profile BlueZ expected".into());
    }
    None
}

impl Collector for BluetoothCollector {
    fn name(&self) -> &'static str {
        "bluetooth"
    }

    fn tier(&self) -> Tier {
        // Reconciliation only. Disconnects arrive as signals, not on this schedule.
        Tier::Medium
    }

    fn probe(&mut self) -> Support {
        if list_dir("/sys/class/bluetooth").is_empty() {
            return Support::Unsupported { reason: "no Bluetooth adapter".into() };
        }
        if !self.open() {
            return Support::Partial {
                detail: "BlueZ is not reachable on the system bus; \
                         per-device reporting is unavailable"
                    .into(),
            };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        if !self.open() {
            return Err(CollectorError::Gone("system bus unavailable".into()));
        }
        // Drain any signals that arrived; they are the wake-up, not the data. The
        // authoritative state always comes from a fresh enumeration, so a missed or
        // malformed signal cannot desynchronise anything.
        if let Some(c) = self.sig.as_mut() {
            if c.try_read_messages().is_err() {
                // The signal socket died; drop it so open() rebuilds it. Losing it
                // costs latency, not correctness.
                self.sig = None;
            }
        }

        let reply = {
            let c = self.conn.as_mut().unwrap();
            c.call(
                "org.bluez",
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "GetManagedObjects",
                "",
                &[],
            )
        };
        let msg = match reply {
            Ok(m) => m,
            Err(e) => {
                // BlueZ stopped or was never running. Drop the connection so the next
                // pass reopens it rather than reusing a dead socket.
                self.conn = None;
                return Err(CollectorError::Gone(format!("GetManagedObjects: {e}")));
            }
        };
        let args = msg.args();
        let Some(root) = args.first() else {
            return Err(CollectorError::BadData("GetManagedObjects returned nothing".into()));
        };
        let (devices, adapter_powered) = parse_managed_objects(root);

        let mut context = self.observe_context();
        context.adapter_powered = adapter_powered;

        // A restart shows up as a new unique name for org.bluez.
        let owner = {
            let c = self.conn.as_mut().unwrap();
            let mut b = crate::dbus::Marshal::new();
            b.string("org.bluez");
            c.call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                "s",
                &b.buf,
            )
            .ok()
            .and_then(|m| m.args().first().and_then(|v| v.as_str().map(String::from)))
        };
        if let Some(now_owner) = owner {
            if let Some(prev) = &self.prev_bluez_owner {
                context.bluetoothd_restarted = prev != &now_owner;
            }
            self.prev_bluez_owner = Some(now_owner);
        }

        // Transitions. Skipped on the first pass, otherwise starting the daemon while
        // headphones are connected would report a connect that did not happen.
        if self.primed {
            for d in &devices {
                let was = self.last.get(&d.address).map(|p| p.connected).unwrap_or(false);
                if was == d.connected {
                    continue;
                }
                let kind = if d.connected {
                    BtEventKind::Connected
                } else {
                    BtEventKind::Disconnected
                };
                let likely_cause = if d.connected { None } else { attribute(&context) };
                self.events.push(BtEvent {
                    at_ms: ctx.ts_ms,
                    at_mono_ms: crate::clock::mono_ms(),
                    kind,
                    address: d.address.clone(),
                    name: d.label().to_string(),
                    context: context.clone(),
                    likely_cause,
                });
            }
            // A device BlueZ forgot entirely counts as a disconnect if it had been up.
            for (addr, prev) in self.last.iter() {
                if prev.connected && !devices.iter().any(|d| &d.address == addr) {
                    self.events.push(BtEvent {
                        at_ms: ctx.ts_ms,
                        at_mono_ms: crate::clock::mono_ms(),
                        kind: BtEventKind::Disconnected,
                        address: addr.clone(),
                        name: prev.label().to_string(),
                        context: context.clone(),
                        likely_cause: Some("the device was removed from BlueZ".into()),
                    });
                }
            }
        }
        self.primed = true;
        self.last = devices.iter().map(|d| (d.address.clone(), d.clone())).collect();

        if self.events.len() > MAX_EVENTS {
            let drop_n = self.events.len() - MAX_EVENTS;
            self.events.drain(0..drop_n);
        }

        let cutoff = crate::clock::mono_ms() - FLAP_WINDOW_MS;
        let mut recent: BTreeMap<String, u32> = BTreeMap::new();
        for e in self.events.iter().filter(|e| e.at_mono_ms >= cutoff) {
            if e.kind == BtEventKind::Disconnected {
                *recent.entry(e.name.clone()).or_insert(0) += 1;
            }
        }

        let connected_count = devices.iter().filter(|d| d.connected).count();
        ctx.snap.bluetooth = BtState {
            bluez_available: true,
            adapter_powered,
            connected_count,
            risk_notes: risk_notes(&context),
            last_stack_error: self.stack_error.clone(),
            devices,
            events: self.events.clone(),
            recent_disconnects: recent,
        };

        ctx.g("bluetooth", "connected", "devices", connected_count as f64);
        Ok(())
    }

    fn event_fd(&self) -> Option<RawFd> {
        self.fd()
    }

    fn on_event(&mut self, ev: &super::ExternalEvent, ctx: &mut Ctx) -> CResult<()> {
        match ev {
            super::ExternalEvent::Journal(entry) => {
                // Only bluetoothd's own lines; everything else on the bus is noise.
                let from_bluez = entry.syslog_id.contains("bluetooth")
                    || entry.unit.contains("bluetooth")
                    || entry.message.contains("avdtp");
                if from_bluez {
                    if let Some(t) = translate_stack_error(&entry.message) {
                        self.stack_error = Some(t);
                    }
                }
                Ok(())
            }
            super::ExternalEvent::Resumed { .. } => {
                self.last_resume_ms = crate::clock::mono_ms();
                Ok(())
            }
            // A BlueZ signal means something changed *now*; re-enumerate immediately
            // so a flap shorter than the sampling interval is still recorded.
            super::ExternalEvent::CollectorReadable { name } if *name == "bluetooth" => {
                self.collect(ctx)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Value;

    /// Build the `a{oa{sa{sv}}}` shape GetManagedObjects returns, without a bus.
    fn dict(k: &str, v: Value) -> Value {
        Value::DictEntry(Box::new(Value::Str(k.into())), Box::new(v))
    }
    fn odict(k: &str, v: Value) -> Value {
        Value::DictEntry(Box::new(Value::ObjectPath(k.into())), Box::new(v))
    }
    fn var(v: Value) -> Value {
        Value::Variant(Box::new(v))
    }

    fn headset(connected: bool) -> Value {
        odict(
            "/org/bluez/hci0/dev_D8_19_04_D9_D4_0E",
            Value::Array(vec![dict(
                "org.bluez.Device1",
                Value::Array(vec![
                    dict("Address", var(Value::Str("D8:19:04:D9:D4:0E".into()))),
                    dict("Name", var(Value::Str("AXR100".into()))),
                    dict("Alias", var(Value::Str("AXR100".into()))),
                    dict("Icon", var(Value::Str("audio-headset".into()))),
                    dict("Paired", var(Value::Bool(true))),
                    dict("Connected", var(Value::Bool(connected))),
                ]),
            )]),
        )
    }

    #[test]
    fn parses_a_device_out_of_managed_objects() {
        let (devs, _) = parse_managed_objects(&Value::Array(vec![headset(true)]));
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].address, "D8:19:04:D9:D4:0E");
        assert_eq!(devs[0].name, "AXR100");
        assert_eq!(devs[0].icon, "audio-headset");
        assert!(devs[0].connected && devs[0].paired);
    }

    #[test]
    fn alias_wins_over_name_because_it_is_what_the_user_renamed_it_to() {
        let obj = odict(
            "/org/bluez/hci0/dev_AA",
            Value::Array(vec![dict(
                "org.bluez.Device1",
                Value::Array(vec![
                    dict("Address", var(Value::Str("AA:BB:CC:DD:EE:FF".into()))),
                    dict("Name", var(Value::Str("WH-1000XM4".into()))),
                    dict("Alias", var(Value::Str("Sony headphones".into()))),
                ]),
            )]),
        );
        let (devs, _) = parse_managed_objects(&Value::Array(vec![obj]));
        assert_eq!(devs[0].name, "Sony headphones");
    }

    #[test]
    fn battery_is_taken_from_the_battery_interface_on_the_same_path() {
        let obj = odict(
            "/org/bluez/hci0/dev_AA",
            Value::Array(vec![
                dict(
                    "org.bluez.Device1",
                    Value::Array(vec![dict("Address", var(Value::Str("AA:BB".into())))]),
                ),
                dict(
                    "org.bluez.Battery1",
                    Value::Array(vec![dict("Percentage", var(Value::Byte(72)))]),
                ),
            ]),
        );
        let (devs, _) = parse_managed_objects(&Value::Array(vec![obj]));
        assert_eq!(devs[0].battery, Some(72));
    }

    #[test]
    fn adapter_power_is_read_from_the_adapter_interface() {
        let adapter = odict(
            "/org/bluez/hci0",
            Value::Array(vec![dict(
                "org.bluez.Adapter1",
                Value::Array(vec![dict("Powered", var(Value::Bool(true)))]),
            )]),
        );
        let (devs, powered) = parse_managed_objects(&Value::Array(vec![adapter]));
        assert!(powered, "adapter reports powered");
        assert!(devs.is_empty(), "an adapter is not a device");
    }

    #[test]
    fn objects_without_a_device_interface_are_ignored() {
        let junk = odict(
            "/org/bluez",
            Value::Array(vec![dict("org.bluez.AgentManager1", Value::Array(vec![]))]),
        );
        let (devs, _) = parse_managed_objects(&Value::Array(vec![junk]));
        assert!(devs.is_empty());
    }

    #[test]
    fn a_garbage_reply_yields_nothing_rather_than_panicking() {
        let (devs, powered) = parse_managed_objects(&Value::Str("not an array".into()));
        assert!(devs.is_empty() && !powered);
    }

    #[test]
    fn label_falls_back_to_the_address_when_there_is_no_name() {
        let d = BtDevice { address: "AA:BB".into(), ..Default::default() };
        assert_eq!(d.label(), "AA:BB");
    }

    // --- cause attribution ------------------------------------------------

    #[test]
    fn a_hard_block_outranks_everything_else() {
        let c = BtContext {
            rfkill_hard: true,
            rfkill_soft: true,
            radio_runtime_status: "suspended".into(),
            ..Default::default()
        };
        assert!(attribute(&c).unwrap().contains("hard-blocked"));
    }

    #[test]
    fn a_resume_is_named_before_autosuspend_is_blamed() {
        let c = BtContext {
            adapter_powered: true,
            just_resumed: true,
            radio_suspended_recently: true,
            ..Default::default()
        };
        assert!(attribute(&c).unwrap().contains("resumed from suspend"));
    }

    #[test]
    fn autosuspend_is_named_when_the_radio_actually_suspended() {
        let c = BtContext {
            adapter_powered: true,
            radio_suspended_recently: true,
            radio_power_control: "auto".into(),
            ..Default::default()
        };
        assert!(attribute(&c).unwrap().contains("autosuspend"));
    }

    #[test]
    fn coexistence_is_offered_only_as_a_hypothesis_and_says_so() {
        let c = BtContext {
            adapter_powered: true,
            radio_power_control: "auto".into(),
            wifi_runtime_pm: Some(true),
            ..Default::default()
        };
        let s = attribute(&c).unwrap();
        assert!(s.contains("possibly"), "must not assert an unproven cause: {s}");
        assert!(s.contains("Not confirmed"));
    }

    #[test]
    fn no_cause_is_invented_when_nothing_was_observable() {
        let c = BtContext {
            adapter_powered: true,
            radio_power_control: "on".into(),
            radio_runtime_status: "active".into(),
            wifi_runtime_pm: Some(false),
            ..Default::default()
        };
        assert_eq!(attribute(&c), None, "silence beats a confident wrong answer");
    }

    // --- bluetoothd log translation --------------------------------------

    #[test]
    fn host_is_down_becomes_something_a_person_can_act_on() {
        let line = "profiles/audio/avdtp.c:avdtp_connect_cb() connect to \
                    D8:19:04:D9:D4:0E: Host is down (112)";
        let t = translate_stack_error(line).expect("this is the line BlueZ actually logs");
        assert!(t.contains("not responding"));
        assert!(t.contains("switched off") || t.contains("out of range"));
        assert!(!t.contains("112"), "an errno is not an explanation");
    }

    #[test]
    fn a_page_timeout_suggests_waking_the_device() {
        let t = translate_stack_error("Failed to connect: org.bluez.Error.Failed \
                                       br-connection-page-timeout")
            .unwrap();
        assert!(t.contains("page timeout"));
        assert!(t.contains("off and on"));
    }

    #[test]
    fn unrelated_log_lines_are_ignored() {
        assert_eq!(translate_stack_error("Endpoint registered: sender=:1.103"), None);
        assert_eq!(translate_stack_error("kernel: usb 1-14: Product: Bluetooth Radio"), None);
        assert_eq!(translate_stack_error(""), None);
    }

    #[test]
    fn an_unrecognised_bluetooth_error_is_not_paraphrased_into_a_guess() {
        assert_eq!(
            translate_stack_error("bluetoothd: connect: some brand new failure mode"),
            None,
            "an unknown error must stay unknown rather than be mistranslated"
        );
    }

    #[test]
    fn a_bluetoothd_journal_line_reaches_the_collector_and_is_stored() {
        // The daemon hands collectors the real entry before classification. This is
        // the exact line bluetoothd wrote on this machine.
        let mut c = BluetoothCollector::new();
        let mut ctx = Ctx::new(std::sync::Arc::new(crate::config::Config::default()));
        let entry = crate::journal::JournalEntry {
            ts_ms: 1,
            priority: 3,
            message: "profiles/audio/avdtp.c:avdtp_connect_cb() connect to \
                      D8:19:04:D9:D4:0E: Host is down (112)"
                .into(),
            unit: "bluetooth.service".into(),
            syslog_id: "bluetoothd".into(),
            is_kernel: false,
        };
        c.on_event(&super::super::ExternalEvent::Journal(entry), &mut ctx).unwrap();
        assert!(
            c.stack_error.as_deref().unwrap().contains("not responding"),
            "the collector must keep BlueZ's own explanation"
        );
    }

    #[test]
    fn journal_lines_from_other_services_are_ignored() {
        let mut c = BluetoothCollector::new();
        let mut ctx = Ctx::new(std::sync::Arc::new(crate::config::Config::default()));
        let entry = crate::journal::JournalEntry {
            ts_ms: 1,
            priority: 3,
            message: "connect to the database: Host is down".into(),
            unit: "postgresql.service".into(),
            syslog_id: "postgres".into(),
            is_kernel: false,
        };
        c.on_event(&super::super::ExternalEvent::Journal(entry), &mut ctx).unwrap();
        assert!(c.stack_error.is_none(), "another service's errors are not ours");
    }

    #[test]
    fn risk_notes_flag_both_known_misconfigurations() {
        let c = BtContext {
            radio_power_control: "auto".into(),
            wifi_runtime_pm: Some(true),
            ..Default::default()
        };
        let n = risk_notes(&c);
        assert_eq!(n.len(), 2);
        assert!(n[0].contains("autosuspend"));
        assert!(n[1].contains("runtime-suspend"));
    }

    #[test]
    fn risk_notes_are_empty_on_a_correctly_configured_machine() {
        let c = BtContext {
            radio_power_control: "on".into(),
            wifi_runtime_pm: Some(false),
            ..Default::default()
        };
        assert!(risk_notes(&c).is_empty());
    }
}
