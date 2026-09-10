//! Network interfaces, Wi-Fi, and reachability.
//!
//! Counters only — no packet capture, no `AF_PACKET` socket, no payload inspection.
//! Link changes arrive event-driven over rtnetlink; only throughput is sampled.

use super::{Collector, Ctx, ExternalEvent};
use crate::sysfs::*;
use crate::types::*;
use crate::util::glob_match;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Iface {
    pub name: String,
    pub kind: String, // wifi | ethernet | loopback | tunnel | other
    pub up: bool,
    pub carrier: bool,
    pub operstate: String,
    pub driver: String,
    pub mac: String,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    /// Errors + drops as a share of packets over the last interval.
    pub error_rate_pct: f64,
    pub speed_mbps: Option<i64>,
    // Wi-Fi only
    pub signal_dbm: Option<f64>,
    pub link_quality: Option<f64>,
    pub ssid: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Reachability {
    pub gateway: Option<String>,
    pub gateway_ok: Option<bool>,
    pub dns_ok: Option<bool>,
    pub internet_ok: Option<bool>,
    pub checked_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NetState {
    pub ifaces: Vec<Iface>,
    pub default_route: Option<String>,
    pub default_iface: Option<String>,
    pub reach: Reachability,
    pub tcp_connections: usize,
    pub total_rx_bps: f64,
    pub total_tx_bps: f64,
}

#[derive(Clone, Copy, Default)]
struct Counters {
    rx_bytes: u64,
    tx_bytes: u64,
    rx_packets: u64,
    tx_packets: u64,
    rx_errors: u64,
    tx_errors: u64,
    rx_dropped: u64,
    tx_dropped: u64,
}

pub struct NetCollector {
    /// Root of the net class. Overridable so a machine with no Wi-Fi interface, or no
    /// interfaces at all, can be tested for real.
    root: String,
    prev: HashMap<String, (Counters, i64)>,
    /// Last known carrier per interface, for flap detection.
    last_carrier: HashMap<String, bool>,
    /// Recent up/down transition timestamps per interface.
    transitions: HashMap<String, Vec<i64>>,
    last_reach_ms: i64,
    /// Addresses and connection count are refreshed on a slow cadence, or immediately
    /// when rtnetlink reports a change. Both are expensive relative to a counter read
    /// and neither changes between two-second ticks in normal operation.
    addr_cache: HashMap<String, (Vec<String>, Vec<String>)>,
    last_addr_ms: i64,
    conn_count: usize,
    last_conn_ms: i64,
}

impl NetCollector {
    pub fn new() -> Self {
        NetCollector {
            root: NET.to_string(),
            prev: HashMap::new(),
            last_carrier: HashMap::new(),
            transitions: HashMap::new(),
            last_reach_ms: 0,
            addr_cache: HashMap::new(),
            last_addr_ms: 0,
            conn_count: 0,
            last_conn_ms: 0,
        }
    }

    /// Number of up/down transitions inside `window_ms`.
    ///
    /// The age must be non-negative: a wall-clock step backwards (NTP correcting a
    /// drifted RTC after resume) leaves stored timestamps in the future, and a bare
    /// `<= window` test would then count every historical transition as "just now"
    /// and manufacture a flapping alert.
    pub fn flap_count(&self, iface: &str, now: i64, window_ms: i64) -> usize {
        self.transitions
            .get(iface)
            .map(|v| v.iter().filter(|t| (0..=window_ms).contains(&(now - **t))).count())
            .unwrap_or(0)
    }
}

impl NetCollector {
    pub fn with_root(root: &str) -> Self {
        NetCollector { root: root.to_string(), ..NetCollector::new() }
    }
}

impl Default for NetCollector {
    fn default() -> Self {
        Self::new()
    }
}

const NET: &str = "/sys/class/net";

fn iface_kind(name: &str) -> &'static str {
    iface_kind_in(NET, name)
}

fn iface_kind_in(root: &str, name: &str) -> &'static str {
    if name == "lo" {
        "loopback"
    } else if exists(format!("{root}/{name}/wireless")) || exists(format!("{root}/{name}/phy80211")) {
        "wifi"
    } else if name.starts_with("tun") || name.starts_with("tap") || name.starts_with("wg")
        || name.starts_with("ppp") || name.starts_with("proton") || name.starts_with("nordlynx")
    {
        "tunnel"
    } else if exists(format!("{root}/{name}/device")) {
        "ethernet"
    } else {
        "other"
    }
}

fn read_counters(name: &str) -> Counters {
    let s = |f: &str| read_u64(format!("{NET}/{name}/statistics/{f}")).unwrap_or(0);
    Counters {
        rx_bytes: s("rx_bytes"),
        tx_bytes: s("tx_bytes"),
        rx_packets: s("rx_packets"),
        tx_packets: s("tx_packets"),
        rx_errors: s("rx_errors"),
        tx_errors: s("tx_errors"),
        rx_dropped: s("rx_dropped"),
        tx_dropped: s("tx_dropped"),
    }
}

/// Parse /proc/net/wireless: `iface: status link level noise ...`
pub fn parse_proc_net_wireless(text: &str) -> HashMap<String, (f64, f64)> {
    let mut m = HashMap::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        // Values carry a trailing '.' in this file: "69." and "-41.".
        let q = f[1].trim_end_matches('.').parse::<f64>().unwrap_or(f64::NAN);
        let l = f[2].trim_end_matches('.').parse::<f64>().unwrap_or(f64::NAN);
        if q.is_finite() && l.is_finite() {
            m.insert(name.trim().to_string(), (q, l));
        }
    }
    m
}

/// Default gateway from /proc/net/route (IPv4). Little-endian hex, destination 0.
pub fn parse_default_route(text: &str) -> Option<(String, String)> {
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        if f[1] == "00000000" {
            let raw = u32::from_str_radix(f[2], 16).ok()?;
            let b = raw.to_le_bytes();
            return Some((f[0].to_string(), format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])));
        }
    }
    None
}

/// Count established TCP connections. State and counts only — never payloads.
///
/// Reads two potentially large proc files, so it is refreshed on a slow cadence rather
/// than every fast tick; a connection count does not need two-second resolution.
fn tcp_connection_count() -> usize {
    let mut n = 0;
    for p in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(t) = std::fs::read_to_string(p) {
            // Column 3 is the connection state; 01 is ESTABLISHED.
            n += t.lines().skip(1).filter(|l| l.split_whitespace().nth(3) == Some("01")).count();
        }
    }
    n
}

/// Every interface's addresses in one `getifaddrs` walk.
///
/// Calling this per interface meant walking the whole list once per interface on every
/// fast tick — measured at a large share of the daemon's total CPU time. One walk,
/// bucketed by name, costs the same as one of the old calls.
fn all_addresses() -> HashMap<String, (Vec<String>, Vec<String>)> {
    let mut map: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `ifap` is a valid out-parameter; the list is freed below.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return map;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: `cur` is a valid node while non-null.
        let ifa = unsafe { &*cur };
        if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
            // SAFETY: ifa_name is a NUL-terminated C string owned by the list.
            let nm = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
            // SAFETY: sa_family is always readable on a non-null sockaddr.
            let fam = unsafe { (*ifa.ifa_addr).sa_family } as i32;
            let e = map.entry(nm).or_default();
            if fam == libc::AF_INET {
                // SAFETY: family confirms this is a sockaddr_in.
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let b = sa.sin_addr.s_addr.to_ne_bytes();
                e.0.push(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]));
            } else if fam == libc::AF_INET6 {
                // SAFETY: family confirms this is a sockaddr_in6.
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                let a = sa.sin6_addr.s6_addr;
                let ip = std::net::Ipv6Addr::from(a);
                // Link-local addresses are noise in a UI.
                if !ip.is_loopback() && !(a[0] == 0xfe && (a[1] & 0xc0) == 0x80) {
                    e.1.push(ip.to_string());
                }
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: freeing the list returned by getifaddrs exactly once.
    unsafe { libc::freeifaddrs(ifap) };
    map
}

/// A bare TCP connect with **zero bytes sent**. Used only to answer "can this host be
/// reached"; nothing is transmitted and nothing is read.
fn tcp_reachable(addr: &str, timeout: Duration) -> Option<bool> {
    let sa: SocketAddr = addr.parse().ok()?;
    Some(TcpStream::connect_timeout(&sa, timeout).is_ok())
}

fn first_nameserver() -> Option<String> {
    let t = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    for line in t.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver ") {
            let ip = rest.trim();
            if ip.parse::<IpAddr>().is_ok() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

impl Collector for NetCollector {
    fn name(&self) -> &'static str {
        "network"
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }

    fn probe(&mut self) -> Support {
        if !exists(&self.root) {
            return Support::Unsupported { reason: "/sys/class/net missing".into() };
        }
        let ifaces = list_dir(&self.root);
        if ifaces.iter().all(|n| n == "lo") {
            return Support::Partial { detail: "only the loopback interface is present".into() };
        }
        let has_wifi = ifaces.iter().any(|n| iface_kind_in(&self.root, n) == "wifi");
        if !has_wifi {
            return Support::Partial { detail: "no Wi-Fi interface present".into() };
        }
        Support::Full
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = NetState::default();
        let cfg = ctx.config.clone();
        let wireless = read_str("/proc/net/wireless").map(|t| parse_proc_net_wireless(&t)).unwrap_or_default();
        if ctx.ts_ms - self.last_addr_ms > 30_000 {
            self.last_addr_ms = ctx.ts_ms;
            self.addr_cache = all_addresses();
        }

        if let Some(t) = read_str("/proc/net/route") {
            if let Some((iface, gw)) = parse_default_route(&t) {
                st.default_iface = Some(iface);
                st.default_route = Some(gw);
            }
        }

        for name in list_dir(NET) {
            if cfg.network.ignore_interfaces.iter().any(|g| glob_match(g, &name)) {
                continue;
            }
            let kind = iface_kind(&name);
            let operstate = read_str(format!("{NET}/{name}/operstate")).unwrap_or_else(|| "unknown".into());
            let carrier = read_i64(format!("{NET}/{name}/carrier")).unwrap_or(0) == 1;
            let mut i = Iface {
                name: name.clone(),
                kind: kind.to_string(),
                up: operstate == "up" || (kind == "loopback" && operstate == "unknown"),
                carrier,
                operstate,
                driver: std::fs::read_link(format!("{NET}/{name}/device/driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_default(),
                mac: read_str(format!("{NET}/{name}/address")).unwrap_or_default(),
                speed_mbps: read_i64(format!("{NET}/{name}/speed")).filter(|s| *s > 0),
                ..Default::default()
            };
            if let Some((v4, v6)) = self.addr_cache.get(&name) {
                i.ipv4 = v4.clone();
                i.ipv6 = v6.clone();
            }

            // Carrier transitions: the basis of flap detection.
            if let Some(&prev) = self.last_carrier.get(&name) {
                if prev != carrier && kind != "loopback" {
                    self.transitions.entry(name.clone()).or_default().push(ctx.ts_ms);
                    // Keep only the last hour.
                    if let Some(v) = self.transitions.get_mut(&name) {
                        v.retain(|t| ctx.ts_ms - *t < 3_600_000);
                    }
                    ctx.event(Event::new(
                        "network",
                        if carrier { "link_up" } else { "link_down" },
                        format!("{name} {}", if carrier { "connected" } else { "disconnected" }),
                        if carrier { Severity::Info } else { Severity::Notice },
                    ).with_detail(serde_json::json!({"iface": name, "kind": kind})));
                }
            }
            self.last_carrier.insert(name.clone(), carrier);

            let c = read_counters(&name);
            if let Some((p, t0)) = self.prev.get(&name) {
                let dt = (ctx.ts_ms - t0) as f64 / 1000.0;
                if dt > 0.0 {
                    i.rx_bps = c.rx_bytes.saturating_sub(p.rx_bytes) as f64 / dt;
                    i.tx_bps = c.tx_bytes.saturating_sub(p.tx_bytes) as f64 / dt;
                    let dpk = (c.rx_packets.saturating_sub(p.rx_packets)
                        + c.tx_packets.saturating_sub(p.tx_packets)) as f64;
                    let derr = (c.rx_errors.saturating_sub(p.rx_errors)
                        + c.tx_errors.saturating_sub(p.tx_errors)
                        + c.rx_dropped.saturating_sub(p.rx_dropped)
                        + c.tx_dropped.saturating_sub(p.tx_dropped)) as f64;
                    i.error_rate_pct = if dpk > 0.0 { 100.0 * derr / (dpk + derr) } else { 0.0 };
                }
            }
            self.prev.insert(name.clone(), (c, ctx.ts_ms));
            i.rx_errors = c.rx_errors;
            i.tx_errors = c.tx_errors;
            i.rx_dropped = c.rx_dropped;
            i.tx_dropped = c.tx_dropped;

            if kind == "wifi" {
                if let Some(&(q, l)) = wireless.get(&name) {
                    i.link_quality = Some(q);
                    i.signal_dbm = Some(l);
                    ctx.sample("network", "signal_dbm", &name, "dBm", l);
                }
            }

            if kind != "loopback" {
                ctx.sample("network", "rx_bps", &name, "B/s", i.rx_bps);
                ctx.sample("network", "tx_bps", &name, "B/s", i.tx_bps);
                ctx.sample("network", "error_rate_pct", &name, "%", i.error_rate_pct);
                st.total_rx_bps += i.rx_bps;
                st.total_tx_bps += i.tx_bps;
            }
            st.ifaces.push(i);
        }

        if ctx.ts_ms - self.last_conn_ms > 30_000 {
            self.last_conn_ms = ctx.ts_ms;
            self.conn_count = tcp_connection_count();
        }
        st.tcp_connections = self.conn_count;
        ctx.g("network", "tcp_connections", "", st.tcp_connections as f64);

        // Reachability is the only outbound activity in the daemon, runs at most once
        // a minute, sends nothing, and can be switched off entirely.
        if cfg.network.reachability && ctx.ts_ms - self.last_reach_ms > 60_000 {
            self.last_reach_ms = ctx.ts_ms;
            let to = Duration::from_millis(cfg.network.probe_timeout_ms);
            let mut r = Reachability { checked_ms: ctx.ts_ms, ..Default::default() };
            if let Some(gw) = &st.default_route {
                r.gateway = Some(gw.clone());
                // Port 53 first (most home routers answer), then 80.
                r.gateway_ok = Some(
                    tcp_reachable(&format!("{gw}:53"), to).unwrap_or(false)
                        || tcp_reachable(&format!("{gw}:80"), to).unwrap_or(false),
                );
            }
            if let Some(ns) = first_nameserver() {
                r.dns_ok = tcp_reachable(&format!("{ns}:53"), to);
            }
            r.internet_ok = tcp_reachable(&cfg.network.internet_probe, to);
            st.reach = r;
        } else {
            st.reach = ctx.snap.network.reach.clone();
        }

        ctx.snap.network = st;
        Ok(())
    }

    fn on_event(&mut self, ev: &ExternalEvent, ctx: &mut Ctx) -> CResult<()> {
        if let ExternalEvent::NetlinkLink = ev {
            // A link change invalidates the cached reachability verdict and the
            // address cache; refresh both on the next tick rather than reporting
            // stale values.
            self.last_reach_ms = 0;
            self.last_addr_ms = 0;
            ctx.event(Event::new("network", "route_change", "Network configuration changed", Severity::Info));
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
    fn parses_proc_net_wireless_from_this_machine() {
        let t = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n\
                  face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n\
                 wlp108s0: 0000   69.  -41.  -256        0      0      0      0   2014        0\n";
        let m = parse_proc_net_wireless(t);
        let (q, l) = m["wlp108s0"];
        assert_eq!(q, 69.0);
        assert_eq!(l, -41.0, "signal strength in dBm");
    }

    #[test]
    fn wireless_parse_tolerates_a_missing_file() {
        assert!(parse_proc_net_wireless("").is_empty());
        assert!(parse_proc_net_wireless("header\nheader\n").is_empty());
    }

    #[test]
    fn default_route_is_decoded_from_little_endian_hex() {
        let t = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                 wlp108s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n\
                 wlp108s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\n";
        let (iface, gw) = parse_default_route(t).unwrap();
        assert_eq!(iface, "wlp108s0");
        assert_eq!(gw, "192.168.1.1");
    }

    #[test]
    fn no_default_route_is_none_not_a_panic() {
        assert!(parse_default_route("Iface\tDestination\n").is_none());
        assert!(parse_default_route("").is_none());
    }

    #[test]
    fn classifies_this_machines_interfaces() {
        assert_eq!(iface_kind("lo"), "loopback");
        // These are the real interfaces on the target.
        if exists("/sys/class/net/wlp108s0") {
            assert_eq!(iface_kind("wlp108s0"), "wifi");
        }
        if exists("/sys/class/net/enp109s0") {
            assert_eq!(iface_kind("enp109s0"), "ethernet");
        }
    }

    #[test]
    fn collects_real_interfaces_without_reachability_probes() {
        let mut cfg = Config::default();
        cfg.network.reachability = false; // keep the test offline and fast
        let mut c = NetCollector::new();
        assert!(c.probe().is_usable());
        let mut x = Ctx::new(Arc::new(cfg));
        c.collect(&mut x).unwrap();
        let n = &x.snap.network;
        assert!(!n.ifaces.is_empty());
        assert!(n.ifaces.iter().any(|i| i.name == "lo"));
        let wifi = n.ifaces.iter().find(|i| i.kind == "wifi");
        if let Some(w) = wifi {
            assert!(w.signal_dbm.is_some_and(|d| d < 0.0 && d > -120.0), "signal {:?}", w.signal_dbm);
        }
    }

    #[test]
    fn flap_counting_uses_a_sliding_window() {
        let mut c = NetCollector::new();
        c.transitions.insert("wlp108s0".into(), vec![1_000, 2_000, 3_000, 500_000]);
        assert_eq!(c.flap_count("wlp108s0", 5_000, 10_000), 3, "only recent transitions count");
        assert_eq!(c.flap_count("wlp108s0", 600_000, 10_000), 0);
        assert_eq!(c.flap_count("nonexistent", 0, 10_000), 0);
    }
}
