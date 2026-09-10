//! The D-Bus service the GNOME Shell extension talks to.
//!
//! # Why a separate surface from the Unix socket
//!
//! The GTK application wants everything — history, process tables, inventory — over a
//! rich request/response protocol. The Shell extension wants almost nothing: eight
//! numbers and a health word, pushed to it when they change. Giving the extension the
//! full IPC protocol would mean it reimplements framing, polling and diffing inside
//! `gjs`, in the compositor process, where a mistake janks the whole desktop.
//!
//! So the extension gets a deliberately tiny D-Bus surface: one method to read the
//! current state, one signal emitted **only when a displayed value has changed enough to
//! be worth a repaint**. All the diffing happens here, in Rust, once.
//!
//! # Why JSON in a string rather than typed properties
//!
//! A typed `a{sv}` property interface would need bidirectional variant marshalling for a
//! payload that is read by exactly one consumer which has `JSON.parse` built in. The
//! wire cost is a few hundred bytes at most once a second. `Introspect` is implemented
//! properly, because every D-Bus client calls it before anything else.

use crate::collectors::Snapshot;
use crate::dbus::{Connection, Marshal, Message};
use crate::types::Severity;
use serde::Serialize;

/// Distinct from the GTK application's `application_id`, which is
/// `org.jamsys.Monitor` and must stay that way because GNOME matches an
/// application id to its `.desktop` filename. When both claimed the same name,
/// GApplication found the daemon already owning it, assumed a running instance of
/// itself, and tried to call `org.gtk.Actions.DescribeAll` on it — so the window
/// refused to start at all.
pub const BUS_NAME: &str = "org.jamsys.Daemon";
pub const OBJECT_PATH: &str = "/org/jamsys/Daemon";
pub const INTERFACE: &str = "org.jamsys.Daemon";

const INTROSPECT_XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg name="xml" type="s" direction="out"/></method>
  </interface>
  <interface name="org.freedesktop.DBus.Peer">
    <method name="Ping"/>
    <method name="GetMachineId"><arg name="id" type="s" direction="out"/></method>
  </interface>
  <interface name="org.jamsys.Daemon">
    <!-- Current widget state as a JSON object. -->
    <method name="GetState"><arg name="state" type="s" direction="out"/></method>
    <!-- Emitted only when a displayed value changes enough to matter. -->
    <signal name="StateChanged"><arg name="state" type="s"/></signal>
    <!-- Acknowledge the currently highlighted alert. -->
    <method name="AckTopAlert"><arg name="ok" type="b" direction="out"/></method>
    <!-- Daemon version and protocol revision. -->
    <method name="GetVersion"><arg name="version" type="s" direction="out"/></method>
  </interface>
</node>"#;

/// Exactly what the corner widget renders. Nothing else.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct WidgetState {
    /// "healthy" | "attention" | "critical" | "unknown"
    pub health: String,
    pub cpu_pct: f64,
    pub cpu_temp_c: f64,
    pub mem_pct: f64,
    pub gpu_pct: f64,
    /// Positive while discharging, negative while charging, 0 with no battery.
    pub power_w: f64,
    pub battery_pct: f64,
    pub on_battery: bool,
    pub has_battery: bool,
    pub net_ok: bool,
    pub net_label: String,
    /// dGPU runtime power state, for the "awake but idle" case.
    pub dgpu: String,
    /// The single most important abnormal thing, or empty when healthy.
    pub alert_title: String,
    pub alert_detail: String,
    pub alert_expected: String,
    pub alert_subsystem: String,
    pub alert_severity: u8,
    pub open_alerts: u32,
}

impl WidgetState {
    /// Would a user notice the difference between these two states?
    ///
    /// This is the whole reason the widget is calm. Percentages are quantised to whole
    /// numbers, temperature to whole degrees, power to 0.1 W; anything finer is noise
    /// that would repaint the panel several times a second for no informational gain.
    pub fn differs_materially(&self, other: &WidgetState) -> bool {
        fn q(v: f64, step: f64) -> i64 {
            if !v.is_finite() { return i64::MIN; }
            (v / step).round() as i64
        }
        self.health != other.health
            || self.alert_title != other.alert_title
            || self.alert_detail != other.alert_detail
            || self.alert_severity != other.alert_severity
            || self.open_alerts != other.open_alerts
            || self.net_ok != other.net_ok
            || self.net_label != other.net_label
            || self.dgpu != other.dgpu
            || self.on_battery != other.on_battery
            || q(self.cpu_pct, 1.0) != q(other.cpu_pct, 1.0)
            || q(self.cpu_temp_c, 1.0) != q(other.cpu_temp_c, 1.0)
            || q(self.mem_pct, 1.0) != q(other.mem_pct, 1.0)
            || q(self.gpu_pct, 1.0) != q(other.gpu_pct, 1.0)
            || q(self.power_w, 0.1) != q(other.power_w, 0.1)
            || q(self.battery_pct, 1.0) != q(other.battery_pct, 1.0)
    }

    /// Build from a snapshot plus the current health verdict.
    pub fn from_snapshot(s: &Snapshot, health: &str, open_alerts: u32) -> WidgetState {
        let active = s
            .network
            .ifaces
            .iter()
            .find(|i| i.kind != "loopback" && i.carrier);
        let reach = &s.network.reach;
        // "Working" means an interface has carrier and nothing has positively told us
        // the internet is unreachable. `None` is unknown, not broken.
        let net_ok = active.is_some() && reach.internet_ok.unwrap_or(true);
        let net_label = match active {
            Some(i) if i.kind == "wifi" => i
                .signal_dbm
                .map(|d| format!("{d:.0} dBm"))
                .unwrap_or_else(|| "Wi-Fi".into()),
            Some(i) => i.name.clone(),
            None => "offline".into(),
        };
        WidgetState {
            health: health.to_string(),
            cpu_pct: s.cpu.usage_pct,
            cpu_temp_c: s.thermal.cpu_package_c.unwrap_or(0.0),
            mem_pct: 100.0 - s.memory.available_pct,
            gpu_pct: s.gpu.nvidia.util_pct.unwrap_or(0.0),
            power_w: s.power.power_w,
            battery_pct: s.power.percent,
            on_battery: s.power.on_battery,
            has_battery: s.power.has_battery,
            net_ok,
            net_label,
            dgpu: s.gpu.nvidia.runtime_status.clone(),
            open_alerts,
            ..Default::default()
        }
    }

    /// Attach the one alert the widget should surface. The spec asks for *only the
    /// important abnormal metric*, so this is the single highest-severity open alert,
    /// not a list.
    pub fn with_alert(
        mut self,
        title: &str,
        detail: &str,
        expected: &str,
        subsystem: &str,
        sev: Severity,
    ) -> Self {
        self.alert_title = title.to_string();
        self.alert_detail = detail.to_string();
        self.alert_expected = expected.to_string();
        self.alert_subsystem = subsystem.to_string();
        self.alert_severity = sev as u8;
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Owns the bus connection and answers the extension.
pub struct DbusService {
    conn: Connection,
    last_sent: Option<WidgetState>,
    pub name_acquired: bool,
    pub calls_served: u64,
    pub signals_emitted: u64,
}

/// What the caller should do after `handle`.
pub enum ServiceAction {
    None,
    AckTopAlert,
}

impl DbusService {
    /// Connect and claim the bus name. Returns `None` if there is no session bus —
    /// which is normal for a daemon started outside a graphical session, and must not
    /// prevent the rest of the daemon from running.
    pub fn new() -> Option<DbusService> {
        let mut conn = match Connection::session() {
            Ok(c) => c,
            Err(e) => {
                crate::log_warn!("no session bus, GNOME widget unavailable: {e}");
                return None;
            }
        };
        let code = conn.request_name(BUS_NAME).unwrap_or(0);
        if code != 1 {
            // 3 means another instance already owns it.
            crate::log_warn!("could not own {BUS_NAME} (reply {code}); widget unavailable");
            return None;
        }
        if conn.set_nonblocking().is_err() {
            crate::log_warn!("could not set the bus socket non-blocking");
            return None;
        }
        crate::log_info!("D-Bus service listening on {BUS_NAME} {OBJECT_PATH}");
        Some(DbusService { conn, last_sent: None, name_acquired: true, calls_served: 0, signals_emitted: 0 })
    }

    pub fn fd(&self) -> std::os::unix::io::RawFd {
        self.conn.as_raw_fd()
    }

    /// Push a new state, but only if a user would notice. Returns true if emitted.
    pub fn publish(&mut self, state: &WidgetState) -> bool {
        let changed = match &self.last_sent {
            Some(prev) => state.differs_materially(prev),
            None => true,
        };
        if !changed {
            return false;
        }
        let mut b = Marshal::new();
        b.string(&state.to_json());
        match self.conn.send_signal(OBJECT_PATH, INTERFACE, "StateChanged", "s", &b.buf) {
            Ok(()) => {
                self.last_sent = Some(state.clone());
                self.signals_emitted += 1;
                true
            }
            Err(e) => {
                crate::log_warn!("failed to emit StateChanged: {e}");
                false
            }
        }
    }

    /// Drain and answer incoming calls. `current` supplies the state for `GetState`.
    pub fn handle(&mut self, current: &WidgetState) -> Vec<ServiceAction> {
        let msgs = match self.conn.try_read_messages() {
            Ok(m) => m,
            Err(e) => {
                crate::log_warn!("bus read failed: {e}");
                return Vec::new();
            }
        };
        let mut actions = Vec::new();
        for m in msgs {
            if m.msg_type != 1 {
                continue; // only method calls concern us
            }
            self.calls_served += 1;
            if let Some(a) = self.dispatch(&m, current) {
                actions.push(a);
            }
        }
        actions
    }

    fn dispatch(&mut self, m: &Message, current: &WidgetState) -> Option<ServiceAction> {
        let iface = m.interface.as_deref().unwrap_or("");
        let member = m.member.as_deref().unwrap_or("");
        let dest = m.sender.clone();
        let mut reply_str = |s: &str| {
            let mut b = Marshal::new();
            b.string(s);
            if let Err(e) = self.conn.send_reply(dest.as_deref(), m.serial, "s", &b.buf) {
                crate::log_warn!("D-Bus reply to serial {} failed: {e}", m.serial);
            }
        };
        match (iface, member) {
            // Every client calls this before anything else. Answering it is not
            // optional: gdbus and Gio.DBusProxy both hang without it.
            ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                reply_str(INTROSPECT_XML);
                None
            }
            ("org.freedesktop.DBus.Peer", "Ping") => {
                let _ = self.conn.send_reply(m.sender.as_deref(), m.serial, "", &[]);
                None
            }
            ("org.freedesktop.DBus.Peer", "GetMachineId") => {
                let id = std::fs::read_to_string("/etc/machine-id")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                reply_str(&id);
                None
            }
            (INTERFACE, "GetState") => {
                reply_str(&current.to_json());
                None
            }
            (INTERFACE, "GetVersion") => {
                reply_str(env!("CARGO_PKG_VERSION"));
                None
            }
            (INTERFACE, "AckTopAlert") => {
                let mut b = Marshal::new();
                b.u32(1); // boolean true
                let _ = self.conn.send_reply(m.sender.as_deref(), m.serial, "b", &b.buf);
                Some(ServiceAction::AckTopAlert)
            }
            _ => {
                let _ = self.conn.send_error(
                    m.sender.as_deref(),
                    m.serial,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    &format!("no such method: {iface}.{member}"),
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WidgetState {
        WidgetState {
            health: "healthy".into(),
            cpu_pct: 7.4,
            cpu_temp_c: 52.3,
            mem_pct: 18.2,
            gpu_pct: 0.0,
            power_w: 11.24,
            battery_pct: 87.0,
            on_battery: true,
            has_battery: true,
            net_ok: true,
            net_label: "-56 dBm".into(),
            dgpu: "suspended".into(),
            ..Default::default()
        }
    }

    #[test]
    fn tiny_fluctuations_do_not_trigger_a_repaint() {
        let a = base();
        let mut b = base();
        // Sub-quantum jitter of exactly the kind /proc produces every tick.
        b.cpu_pct = 7.44;
        b.cpu_temp_c = 52.31;
        b.mem_pct = 18.24;
        b.power_w = 11.239;
        assert!(!a.differs_materially(&b), "the widget would have repainted for noise");
    }

    #[test]
    fn a_change_a_user_would_see_does_trigger_a_repaint() {
        let a = base();
        for mutate in [
            (|s: &mut WidgetState| s.cpu_pct = 9.0) as fn(&mut WidgetState),
            |s| s.cpu_temp_c = 54.0,
            |s| s.mem_pct = 19.5,
            |s| s.power_w = 11.5,
            |s| s.battery_pct = 86.0,
            |s| s.health = "attention".into(),
            |s| s.net_ok = false,
            |s| s.dgpu = "active".into(),
            |s| s.on_battery = false,
            |s| s.open_alerts = 1,
        ] {
            let mut b = base();
            mutate(&mut b);
            assert!(a.differs_materially(&b), "missed a visible change");
        }
    }

    #[test]
    fn power_is_quantised_finer_than_percentages() {
        // The widget renders power to one decimal, so the quantum is 0.1 W: a change
        // that would not alter "11.2 W" must not repaint, and one that would must.
        let a = base(); // 11.24 W  ->  "11.2 W"
        let mut b = base();
        b.power_w = 11.23; // still "11.2 W"
        assert!(!a.differs_materially(&b), "11.24 and 11.23 both render as 11.2 W");
        b.power_w = 11.35; // "11.4 W"
        assert!(a.differs_materially(&b), "a visible tenth of a watt must repaint");

        // A percentage is quantised a decade coarser, because it renders as an integer.
        let mut c = base();
        c.cpu_pct = 7.44; // still "7%"
        assert!(!a.differs_materially(&c));
    }

    #[test]
    fn a_non_finite_value_is_treated_as_a_change_not_a_panic() {
        let a = base();
        let mut b = base();
        b.cpu_pct = f64::NAN;
        assert!(a.differs_materially(&b));
    }

    #[test]
    fn state_serialises_to_the_json_the_extension_expects() {
        let s = base().with_alert(
            "Unusually high power draw while idle",
            "Battery discharge is 28.4 W.",
            "8.7-13.5 W",
            "Power",
            Severity::Notice,
        );
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
        assert_eq!(v["health"], "healthy");
        assert_eq!(v["alert_subsystem"], "Power");
        assert_eq!(v["alert_severity"], 1);
        assert!(v["power_w"].as_f64().unwrap() > 11.0);
        assert!(v["net_ok"].as_bool().unwrap());
    }

    #[test]
    fn introspection_xml_is_well_formed_and_declares_the_interface() {
        assert!(INTROSPECT_XML.starts_with("<!DOCTYPE node"));
        assert!(INTROSPECT_XML.contains("org.jamsys.Daemon"));
        assert!(INTROSPECT_XML.contains("StateChanged"));
        assert!(INTROSPECT_XML.contains("GetState"));
        assert!(INTROSPECT_XML.contains("Introspectable"));
        assert_eq!(INTROSPECT_XML.matches("<node>").count(), 1);
        assert_eq!(INTROSPECT_XML.matches("</node>").count(), 1);
        // Balanced interface tags.
        assert_eq!(
            INTROSPECT_XML.matches("<interface").count(),
            INTROSPECT_XML.matches("</interface>").count()
        );
    }

    #[test]
    fn unknown_network_reachability_is_not_reported_as_broken() {
        let mut s = Snapshot::default();
        s.network.ifaces = vec![crate::collectors::network::Iface {
            name: "wlp108s0".into(),
            kind: "wifi".into(),
            carrier: true,
            up: true,
            signal_dbm: Some(-56.0),
            ..Default::default()
        }];
        // reach.internet_ok stays None: not yet probed.
        let w = WidgetState::from_snapshot(&s, "healthy", 0);
        assert!(w.net_ok, "an unprobed network must not show as offline");
        assert_eq!(w.net_label, "-56 dBm");
    }

    #[test]
    fn no_carrier_reports_offline() {
        let mut s = Snapshot::default();
        s.network.ifaces = vec![crate::collectors::network::Iface {
            name: "wlp108s0".into(),
            kind: "wifi".into(),
            carrier: false,
            ..Default::default()
        }];
        let w = WidgetState::from_snapshot(&s, "attention", 1);
        assert!(!w.net_ok);
        assert_eq!(w.net_label, "offline");
    }
}
