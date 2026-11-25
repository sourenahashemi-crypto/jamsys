//! What this machine exposes to the network, whether anything guards it, and who
//! has been failing to sign in.
//!
//! This is detection, not prevention. Nothing here blocks a packet: the daemon is
//! unprivileged and stays that way, and a firewall is a job the system's own
//! (`ufw`, `nftables`) already does. What an unprivileged process *can* see is
//! enough to answer three questions an owner cannot answer at a glance:
//!
//! * **What is listening?** `/proc/net/{tcp,tcp6,udp,udp6}` list every socket with
//!   its local address, owner and inode. No packet is read and no socket is opened.
//! * **Is a firewall on?** Only as far as can honestly be told without root: `ufw`
//!   records whether it is enabled in a world-readable file, `firewalld` leaves a
//!   pid file. The *rules* are root-only, and this never pretends to have read them.
//! * **Is someone guessing a password?** PAM, sshd, su and polkit log each failure
//!   to the journal, which the `adm` group can read.
//!
//! A new listener is judged against what was listening the first time this ran
//! (`known-listeners.json`), plus whatever the user has accepted since. The
//! baseline is learned once and never grows on its own: a monitor that silently
//! re-learns after every restart would accept an intruder's listener the moment the
//! daemon was restarted.

use super::{Collector, Ctx, ExternalEvent};
use crate::journal::JournalEntry;
use crate::sysfs::{list_dir, read_str};
use crate::types::*;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

/// Ports from here up are handed out by the kernel for one-off use. An application
/// that listens on a different one each time it starts is one thing to accept, not
/// a new thing every launch.
const EPHEMERAL_FROM: u16 = 32768;
const WINDOW_SHORT_MS: i64 = 10 * 60_000;
const WINDOW_LONG_MS: i64 = 60 * 60_000;
/// Individual failures are recorded as events up to this many per ten minutes.
/// A real guessing attack is thousands of lines; the alert carries the count.
const MAX_FAILURE_EVENTS: usize = 20;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Listener {
    /// "tcp" or "udp".
    pub proto: String,
    pub port: u16,
    /// Every local address it is bound to, e.g. `0.0.0.0` and `::`.
    pub addresses: Vec<String>,
    /// "loopback" (this machine only), "all" (every interface) or "address"
    /// (one specific non-loopback address).
    pub exposure: String,
    pub uid: u32,
    /// Account that owns the socket.
    pub owner: String,
    /// Known only for the user's own processes: another user's file descriptors
    /// are not readable without root.
    pub process: Option<String>,
    pub pid: Option<i32>,
    /// Stable identity used for the baseline and for alerts: `name:port`.
    pub key: String,
    /// False for a TCP listener reachable from the network that was neither
    /// present at first run nor accepted since.
    pub expected: bool,
}

impl Listener {
    pub fn exposed(&self) -> bool {
        self.exposure != "loopback"
    }
    /// The process if it could be identified, otherwise the owning account.
    pub fn name(&self) -> &str {
        self.process.as_deref().unwrap_or(&self.owner)
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct FirewallState {
    /// "enabled", "disabled", or "unknown".
    pub state: String,
    /// Which tool the verdict is about; empty when none was found.
    pub tool: String,
    /// One sentence saying what was actually checked.
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AuthFailure {
    pub ts_ms: i64,
    /// "sudo", "sshd", "su", "polkit", "gdm-password", ...
    pub service: String,
    pub user: String,
    /// Remote host for a network login; empty for a local one.
    pub source: String,
}

/// One line of the security checkup: a setting that is either as it should be
/// or not, with the one command that changes it.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Check {
    pub id: String,
    /// "ok", "warn" (worth fixing, and `fix` says how), "note" (worth knowing,
    /// not simply fixable) or "unknown" (could not be determined here).
    pub status: String,
    pub title: String,
    pub detail: String,
    /// A command to run, or empty.
    pub fix: String,
}

fn check(id: &str, status: &str, title: impl Into<String>, detail: impl Into<String>,
         fix: &str) -> Check {
    Check { id: id.into(), status: status.into(), title: title.into(),
            detail: detail.into(), fix: fix.into() }
}

/// `APT::Periodic::Unattended-Upgrade "1";` -> Some(true). None when not stated.
pub fn parse_auto_upgrades(text: &str) -> Option<bool> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
        .find_map(|l| l.strip_prefix("APT::Periodic::Unattended-Upgrade"))
        .and_then(|v| v.split('"').nth(1))
        .map(|v| v.trim() != "0" && !v.trim().is_empty())
}

/// From update-notifier's summary: (updates installable now, of which security).
/// The second is None when the file does not say.
pub fn parse_updates_available(text: &str) -> Option<(u32, Option<u32>)> {
    let first_number = |l: &str| l.split_whitespace().next().and_then(|n| n.parse::<u32>().ok());
    let all = text.lines().find(|l| l.contains("can be applied immediately")
                                    || l.contains("can be installed immediately"))
        .and_then(first_number)?;
    let sec = text.lines().find(|l| l.contains("standard security update")
                                    || l.contains("are security updates"))
        .and_then(first_number);
    Some((all, sec.or(if all == 0 { Some(0) } else { None })))
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SecurityState {
    pub listeners: Vec<Listener>,
    /// TCP listeners reachable from outside this machine.
    pub exposed_tcp: usize,
    /// Exposed TCP listeners that are neither in the baseline nor accepted.
    pub unexpected: Vec<Listener>,
    pub firewall: FirewallState,
    pub auth_failures_10m: u32,
    pub auth_failures_1h: u32,
    /// Most recent first, at most ten.
    pub recent_failures: Vec<AuthFailure>,
    /// Whether the auth log is being followed at all (see the daemon).
    pub auth_log_watched: bool,
    /// False until the baseline of expected listeners exists.
    pub baseline_ready: bool,
    /// The checkup, in the order worth reading.
    pub checks: Vec<Check>,
    /// How many of `checks` are "warn".
    pub checks_failing: usize,
}

/// One row of `/proc/net/{tcp,udp}{,6}`.
#[derive(Clone, Debug, PartialEq)]
pub struct RawSocket {
    pub addr: IpAddr,
    pub port: u16,
    pub uid: u32,
    pub inode: u64,
}

/// Decode the kernel's hex address: 8 digits for IPv4, 32 for IPv6, each 32-bit
/// word printed in host byte order (so `0100007F` is 127.0.0.1).
fn parse_addr(hex: &str) -> Option<IpAddr> {
    let word = |s: &str| u32::from_str_radix(s, 16).ok().map(u32::to_le_bytes);
    match hex.len() {
        8 => word(hex).map(|b| IpAddr::V4(Ipv4Addr::from(b))),
        32 => {
            let mut out = [0u8; 16];
            for i in 0..4 {
                out[i * 4..i * 4 + 4].copy_from_slice(&word(&hex[i * 8..i * 8 + 8])?);
            }
            Some(IpAddr::V6(Ipv6Addr::from(out)))
        }
        _ => None,
    }
}

fn parse_endpoint(s: &str) -> Option<(IpAddr, u16)> {
    let (a, p) = s.rsplit_once(':')?;
    Some((parse_addr(a)?, u16::from_str_radix(p, 16).ok()?))
}

/// Sockets waiting for someone to talk to them.
///
/// TCP: state `0A` (LISTEN). UDP has no listen state, so the equivalent is a
/// socket with no remote end; a connected UDP socket is a conversation this
/// machine started, not a door left open.
pub fn parse_proc_net(text: &str, tcp: bool) -> Vec<RawSocket> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let (Some((addr, port)), Some((raddr, rport))) = (parse_endpoint(f[1]), parse_endpoint(f[2]))
        else { continue };
        let waiting = if tcp { f[3] == "0A" } else { raddr.is_unspecified() && rport == 0 };
        if !waiting || port == 0 {
            continue;
        }
        let (Ok(uid), Ok(inode)) = (f[7].parse::<u32>(), f[9].parse::<u64>()) else { continue };
        out.push(RawSocket { addr, port, uid, inode });
    }
    out
}

/// Who can reach a socket bound to `addr`.
pub fn exposure_of(addr: &IpAddr) -> &'static str {
    let v4 = match addr {
        IpAddr::V4(a) => Some(*a),
        // `::ffff:127.0.0.1` is the IPv4 loopback wearing an IPv6 coat.
        IpAddr::V6(a) => a.to_ipv4_mapped(),
    };
    if addr.is_loopback() || v4.is_some_and(|a| a.is_loopback()) {
        "loopback"
    } else if addr.is_unspecified() || v4.is_some_and(|a| a.is_unspecified()) {
        "all"
    } else {
        "address"
    }
}

/// The identity a listener is remembered by.
pub fn listener_key(name: &str, proto: &str, port: u16) -> String {
    if port >= EPHEMERAL_FROM {
        format!("{name}:{proto}-high-port")
    } else {
        format!("{name}:{proto}/{port}")
    }
}

/// Recognise one failed sign-in in a journal entry.
///
/// Each attempt must be counted once, and sshd logs a failed password twice (its
/// own "Failed password" line and PAM's). So sshd is counted from its own lines,
/// which also name the remote host, and every other service from PAM's.
pub fn classify_auth(e: &JournalEntry) -> Option<AuthFailure> {
    let m = e.message.as_str();
    let field = |name: &str| -> String {
        m.split_whitespace()
            .find_map(|t| t.strip_prefix(name))
            .unwrap_or("")
            .trim_end_matches(';')
            .to_string()
    };
    let word_after = |marker: &str| -> String {
        m.split_once(marker)
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("")
            .to_string()
    };
    let id = e.syslog_id.as_str();
    let mut f = AuthFailure::default();

    if id == "sshd" || id == "sshd-session" {
        if m.starts_with("Failed password for ") || m.starts_with("Failed keyboard-interactive") {
            f.user = if m.contains(" for invalid user ") {
                word_after(" for invalid user ")
            } else {
                word_after(" for ")
            };
        } else if m.starts_with("Invalid user ") {
            f.user = word_after("Invalid user ");
        } else {
            return None;
        }
        f.service = "sshd".into();
        f.source = word_after(" from ");
        return Some(f);
    }
    if let Some(rest) = m.strip_prefix("pam_unix(") {
        // pam_unix(sudo:auth): authentication failure; logname=x uid=0 ... rhost=  user=x
        let (svc, tail) = rest.split_once(':')?;
        if svc == "sshd" || !tail.starts_with("auth): authentication failure") {
            return None;
        }
        f.service = svc.to_string();
        f.user = field("user=");
        if f.user.is_empty() {
            f.user = field("logname=");
        }
        f.source = field("rhost=");
        return Some(f);
    }
    if id == "polkitd" && m.contains("FAILED to authenticate") {
        f.service = "polkit".into();
        f.user = word_after("Operator of ");
        return Some(f);
    }
    if id == "su" && m.starts_with("FAILED SU") {
        f.service = "su".into();
        f.user = word_after(") ");
        return Some(f);
    }
    None
}

/// Arguments that make `journalctl` follow only the processes that report failed
/// sign-ins. These are informational-priority lines, which the main follower
/// (warnings and above) never sees.
pub const AUTH_JOURNAL_MATCHES: &[&str] = &[
    "SYSLOG_IDENTIFIER=sshd", "SYSLOG_IDENTIFIER=sshd-session", "SYSLOG_IDENTIFIER=sudo",
    "SYSLOG_IDENTIFIER=su", "SYSLOG_IDENTIFIER=login", "SYSLOG_IDENTIFIER=polkitd",
    "SYSLOG_IDENTIFIER=gdm-password", "SYSLOG_IDENTIFIER=gdm-fingerprint",
    "SYSLOG_IDENTIFIER=unix_chkpwd",
];

pub struct SecurityCollector {
    proc_net: PathBuf,
    proc_root: PathBuf,
    etc: PathBuf,
    run: PathBuf,
    sys_module: PathBuf,
    sys_root: PathBuf,
    var: PathBuf,
    baseline_file: PathBuf,
    /// Keys present at first run. `None` until loaded or learned.
    baseline: Option<BTreeSet<String>>,
    /// inode -> (pid, name), for the user's own sockets.
    owners: HashMap<u64, (i32, String)>,
    users: HashMap<u32, String>,
    failures: Vec<AuthFailure>,
    /// Keys of TCP listeners on the previous pass, for open/close events.
    prev: Option<BTreeMap<String, String>>,
    auth_seen: bool,
}

impl SecurityCollector {
    pub fn new() -> Self {
        SecurityCollector {
            proc_net: "/proc/net".into(),
            proc_root: "/proc".into(),
            etc: "/etc".into(),
            run: "/run".into(),
            sys_module: "/sys/module".into(),
            sys_root: "/sys".into(),
            var: "/var".into(),
            baseline_file: crate::config::data_dir().join("known-listeners.json"),
            baseline: None,
            owners: HashMap::new(),
            users: HashMap::new(),
            failures: Vec::new(),
            prev: None,
            auth_seen: false,
        }
    }

    /// Point every path at a directory tree of the test's making.
    pub fn with_roots(root: &std::path::Path) -> Self {
        SecurityCollector {
            proc_net: root.join("proc/net"),
            proc_root: root.join("proc"),
            etc: root.join("etc"),
            run: root.join("run"),
            sys_module: root.join("sys/module"),
            sys_root: root.join("sys"),
            var: root.join("var"),
            baseline_file: root.join("known-listeners.json"),
            ..Self::new()
        }
    }

    fn user_name(&mut self, uid: u32) -> String {
        if !self.users.contains_key(&uid) {
            // Re-read on a miss only: accounts are created rarely.
            if let Some(text) = read_str(self.etc.join("passwd")) {
                for line in text.lines() {
                    let f: Vec<&str> = line.split(':').collect();
                    if let (Some(name), Some(Ok(id))) = (f.first(), f.get(2).map(|x| x.parse::<u32>())) {
                        self.users.entry(id).or_insert_with(|| name.to_string());
                    }
                }
            }
        }
        self.users.get(&uid).cloned().unwrap_or_else(|| format!("uid {uid}"))
    }

    /// Find which of this user's processes hold the given socket inodes.
    ///
    /// Walking every `/proc/<pid>/fd` is the expensive way to learn this, so it is
    /// done only for inodes not seen before -- in practice, when a listener appears.
    fn resolve_owners(&mut self, wanted: &BTreeSet<u64>) {
        self.owners.retain(|ino, _| wanted.contains(ino));
        let missing: BTreeSet<u64> =
            wanted.iter().filter(|i| !self.owners.contains_key(i)).copied().collect();
        if missing.is_empty() {
            return;
        }
        for entry in list_dir(&self.proc_root) {
            let Ok(pid) = entry.parse::<i32>() else { continue };
            let fd_dir = self.proc_root.join(&entry).join("fd");
            let Ok(rd) = std::fs::read_dir(&fd_dir) else { continue }; // not ours
            for fd in rd.filter_map(|e| e.ok()) {
                let Ok(target) = std::fs::read_link(fd.path()) else { continue };
                let Some(ino) = target.to_str()
                    .and_then(|t| t.strip_prefix("socket:["))
                    .and_then(|t| t.strip_suffix(']'))
                    .and_then(|t| t.parse::<u64>().ok())
                else { continue };
                if missing.contains(&ino) && !self.owners.contains_key(&ino) {
                    let name = read_str(self.proc_root.join(&entry).join("comm"))
                        .unwrap_or_else(|| format!("pid {pid}"));
                    self.owners.insert(ino, (pid, name));
                }
            }
        }
    }

    fn listeners(&mut self) -> Option<Vec<Listener>> {
        let mut raw: Vec<(&'static str, RawSocket)> = Vec::new();
        let mut any = false;
        for (file, proto, tcp) in [("tcp", "tcp", true), ("tcp6", "tcp", true),
                                   ("udp", "udp", false), ("udp6", "udp", false)] {
            if let Ok(text) = std::fs::read_to_string(self.proc_net.join(file)) {
                any = true;
                raw.extend(parse_proc_net(&text, tcp).into_iter().map(|s| (proto, s)));
            }
        }
        if !any {
            return None;
        }
        let my_uid = unsafe { libc::getuid() };
        let mine: BTreeSet<u64> =
            raw.iter().filter(|(_, s)| s.uid == my_uid).map(|(_, s)| s.inode).collect();
        self.resolve_owners(&mine);

        // One row per (protocol, port, exposure, owner): a service bound to both
        // 0.0.0.0 and :: is one service.
        let mut merged: BTreeMap<(String, u16, String, String), Listener> = BTreeMap::new();
        for (proto, s) in raw {
            let owner = self.user_name(s.uid);
            let who = self.owners.get(&s.inode).cloned();
            let name = who.as_ref().map(|w| w.1.clone()).unwrap_or_else(|| owner.clone());
            let exposure = exposure_of(&s.addr).to_string();
            let l = merged
                .entry((proto.to_string(), s.port, exposure.clone(), name.clone()))
                .or_insert_with(|| Listener {
                    proto: proto.to_string(),
                    port: s.port,
                    exposure,
                    uid: s.uid,
                    owner,
                    process: who.as_ref().map(|w| w.1.clone()),
                    pid: who.as_ref().map(|w| w.0),
                    key: listener_key(&name, proto, s.port),
                    expected: true,
                    ..Default::default()
                });
            let a = s.addr.to_string();
            if !l.addresses.contains(&a) {
                l.addresses.push(a);
            }
        }
        Some(merged.into_values().collect())
    }

    fn firewall(&self) -> FirewallState {
        // ufw writes ENABLED=yes|no to a world-readable file when it is switched.
        if let Some(conf) = read_str(self.etc.join("ufw/ufw.conf")) {
            let on = conf.lines().map(str::trim)
                .find_map(|l| l.strip_prefix("ENABLED="))
                .map(|v| v.trim_matches('"').eq_ignore_ascii_case("yes"));
            if let Some(on) = on {
                return FirewallState {
                    state: if on { "enabled" } else { "disabled" }.into(),
                    tool: "ufw".into(),
                    detail: format!(
                        "ufw is {} in /etc/ufw/ufw.conf. Its rules, and any loaded by \
                         other tools, are readable only by root and were not inspected.",
                        if on { "enabled" } else { "switched off" }),
                };
            }
        }
        if self.run.join("firewalld.pid").exists() || self.run.join("firewalld").is_dir() {
            return FirewallState {
                state: "enabled".into(),
                tool: "firewalld".into(),
                detail: "firewalld is running. Its zones and rules were not inspected.".into(),
            };
        }
        let nft = self.sys_module.join("nf_tables").exists();
        FirewallState {
            state: "unknown".into(),
            tool: String::new(),
            detail: if nft {
                "Neither ufw nor firewalld was found. The kernel's nftables is loaded, but \
                 whether it holds any rules can only be read as root (sudo nft list ruleset)."
                    .into()
            } else {
                "No firewall tool was found, and the kernel's packet filter is not loaded.".into()
            },
        }
    }

    /// Is the block device `name` (as in /sys/class/block) encrypted, or built
    /// on something that is? Follows device-mapper `slaves` so LVM-on-LUKS counts.
    fn encrypted(&self, name: &str, depth: u8) -> bool {
        let dev = self.sys_root.join("class/block").join(name);
        if read_str(dev.join("dm/uuid")).is_some_and(|u| u.starts_with("CRYPT-")) {
            return true;
        }
        depth < 4 && list_dir(dev.join("slaves")).iter().any(|s| self.encrypted(s, depth + 1))
    }

    /// The device mounted at `/`, as a /sys/class/block name.
    fn root_device(&self) -> Option<String> {
        let text = read_str(self.proc_root.join("self/mountinfo"))?;
        let line = text.lines().find(|l| l.split_whitespace().nth(4) == Some("/"))?;
        let src = line.split(" - ").nth(1)?.split_whitespace().nth(1)?;
        let path = std::fs::canonicalize(src).unwrap_or_else(|_| PathBuf::from(src));
        path.file_name()?.to_str().map(String::from)
    }

    /// The checkup. Every line is something read from a file anyone may read;
    /// where the file is missing the answer is "unknown", never a guess.
    fn checks(&self, fw: &FirewallState, exposed: &[&Listener], unexpected: usize) -> Vec<Check> {
        let mut out = Vec::new();

        out.push(match fw.state.as_str() {
            "enabled" => check("firewall", "ok", format!("Firewall is on ({})", fw.tool), &fw.detail, ""),
            "disabled" => check("firewall", "warn", "Firewall is off",
                "Nothing refuses incoming connections. Turning it on blocks what other devices \
                 start and leaves everything you start untouched.", "sudo ufw enable"),
            _ => check("firewall", "unknown", "Firewall state unknown", &fw.detail, ""),
        });

        out.push(if exposed.is_empty() {
            check("exposure", "ok", "Nothing is reachable from the network",
                  "No program is accepting connections from other devices.", "")
        } else {
            let names: Vec<String> = exposed.iter().map(|l| format!("{} ({})", l.port, l.name())).collect();
            check("exposure", if unexpected > 0 { "warn" } else { "note" },
                  format!("{} program(s) reachable from the network", exposed.len()),
                  format!("Ports {}. Fine if you mean to share them; each one is a door.{}",
                          names.join(", "),
                          if unexpected > 0 { " At least one is new and has not been accepted." } else { "" }),
                  "")
        });

        let apt = self.etc.join("apt/apt.conf.d");
        let auto = list_dir(&apt).iter()
            .filter_map(|f| read_str(apt.join(f)))
            .filter_map(|t| parse_auto_upgrades(&t))
            .last();
        out.push(match auto {
            Some(true) => check("auto_updates", "ok", "Security updates install automatically",
                                "unattended-upgrades is enabled.", ""),
            Some(false) => check("auto_updates", "warn", "Automatic security updates are off",
                "Fixes for known holes wait until you install them by hand.",
                "sudo dpkg-reconfigure -plow unattended-upgrades"),
            None => check("auto_updates", "unknown", "Automatic updates: could not tell",
                          "No APT::Periodic setting was found; this may not be an apt system.", ""),
        });

        if let Some(t) = read_str(self.var.join("lib/update-notifier/updates-available")) {
            if let Some((all, sec)) = parse_updates_available(&t) {
                out.push(match (all, sec) {
                    (_, Some(s)) if s > 0 => check("updates", "warn",
                        format!("{s} security update(s) waiting"),
                        format!("{all} update(s) can be installed now, {s} of them security fixes."),
                        "sudo apt update && sudo apt upgrade"),
                    (0, _) => check("updates", "ok", "No updates waiting",
                                    "As of the system's last check.", ""),
                    (n, _) => check("updates", "note", format!("{n} update(s) waiting"),
                                    "None is marked as a security fix.",
                                    "sudo apt update && sudo apt upgrade"),
                });
            }
        }

        if self.run.join("reboot-required").exists() {
            let pkgs = read_str(self.run.join("reboot-required.pkgs"))
                .map(|p| p.lines().take(4).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            out.push(check("restart", "warn", "A restart is needed to finish updating",
                if pkgs.is_empty() { "An installed update only takes effect after a restart.".to_string() }
                else { format!("Installed but not yet running: {pkgs}.") }, ""));
        }

        if let Some(dev) = self.root_device() {
            out.push(if self.encrypted(&dev, 0) {
                check("disk_encryption", "ok", "The system disk is encrypted",
                      "If this computer is lost or stolen, its files cannot be read.", "")
            } else {
                check("disk_encryption", "note", "The system disk is not encrypted",
                    format!("Anyone holding this computer can read its files without your \
                             password ({dev} is a plain partition). Full-disk encryption can only \
                             be chosen when installing; for now, keep sensitive files in an \
                             encrypted folder or volume."), "")
            });
        }

        let efi = self.sys_root.join("firmware/efi/efivars");
        if let Some(var) = list_dir(&efi).into_iter().find(|n| n.starts_with("SecureBoot-")) {
            // Four attribute bytes, then the value.
            match std::fs::read(efi.join(var)).ok().and_then(|b| b.get(4).copied()) {
                Some(1) => out.push(check("secure_boot", "ok", "Secure Boot is on",
                    "The firmware only starts a signed bootloader and kernel.", "")),
                Some(_) => out.push(check("secure_boot", "note", "Secure Boot is off",
                    "The firmware will start any bootloader. It is switched on in the firmware \
                     setup screen, not from here.", "")),
                None => {}
            }
        }
        out
    }

    fn load_baseline(&mut self, current: &[Listener]) {
        if self.baseline.is_some() {
            return;
        }
        if let Some(text) = read_str(&self.baseline_file) {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&text) {
                self.baseline = Some(v.into_iter().collect());
                return;
            }
            crate::log_warn!("{} is unreadable; learning the listeners again",
                             self.baseline_file.display());
        }
        // First run: what is here now is what "normal" means.
        let learned: BTreeSet<String> = current.iter()
            .filter(|l| l.proto == "tcp" && l.exposed())
            .map(|l| l.key.clone())
            .collect();
        if let Some(dir) = self.baseline_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = serde_json::to_string_pretty(&learned.iter().collect::<Vec<_>>()).unwrap_or_default();
        let tmp = self.baseline_file.with_extension("json.tmp");
        if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &self.baseline_file)).is_err() {
            crate::log_warn!("could not save {}; new listeners will be judged against \
                              this session only", self.baseline_file.display());
        }
        crate::log_info!("learned {} network-facing listener(s) as the baseline", learned.len());
        self.baseline = Some(learned);
    }
}

impl Default for SecurityCollector {
    fn default() -> Self {
        Self::new()
    }
}

fn describe(l: &Listener) -> String {
    format!("{} on {} port {} ({})", l.name(), l.proto.to_uppercase(), l.port, match l.exposure.as_str() {
        "loopback" => "this machine only",
        "all" => "all network interfaces",
        _ => "one network address",
    })
}

impl Collector for SecurityCollector {
    fn name(&self) -> &'static str {
        "security"
    }
    fn tier(&self) -> Tier {
        // Medium, not slow: a listener that lives for less than the sampling
        // interval is never seen, so the interval is the size of the blind spot.
        // Every source here is a small text file; nothing is spawned.
        Tier::Medium
    }

    fn probe(&mut self) -> Support {
        if !self.proc_net.join("tcp").exists() && !self.proc_net.join("tcp6").exists() {
            return Support::Unsupported { reason: "/proc/net/tcp is not available".into() };
        }
        Support::Partial {
            detail: "listening sockets, firewall on/off and failed sign-ins. Firewall rules \
                     and other users' process names need root and are not read; connections \
                     shorter than the sampling interval are not seen".into(),
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let Some(mut listeners) = self.listeners() else {
            return Err(CollectorError::Gone("/proc/net/tcp".into()));
        };
        self.load_baseline(&listeners);
        let accepted = &ctx.config.security.accepted_listeners;
        let baseline = self.baseline.as_ref();
        for l in listeners.iter_mut() {
            if l.proto == "tcp" && l.exposed() {
                l.expected = baseline.is_some_and(|b| b.contains(&l.key))
                    || accepted.iter().any(|a| a == &l.key);
            }
        }
        // Network-facing first, then by port: the order a person wants to read.
        listeners.sort_by(|a, b| (a.proto != "tcp", !a.exposed(), a.port, a.name().to_string())
            .cmp(&(b.proto != "tcp", !b.exposed(), b.port, b.name().to_string())));

        // Open/close events for TCP, after the first pass has established "before".
        let now: BTreeMap<String, String> = listeners.iter()
            .filter(|l| l.proto == "tcp")
            .map(|l| (format!("{}|{}", l.key, l.exposure), describe(l)))
            .collect();
        if let Some(prev) = &self.prev {
            for (k, d) in now.iter().filter(|(k, _)| !prev.contains_key(*k)) {
                let exposed = !k.ends_with("|loopback");
                ctx.event(Event::new("security", "listener_opened",
                    format!("Started listening: {d}"),
                    if exposed { Severity::Notice } else { Severity::Info }));
            }
            for (_, d) in prev.iter().filter(|(k, _)| !now.contains_key(*k)) {
                ctx.event(Event::new("security", "listener_closed",
                    format!("Stopped listening: {d}"), Severity::Info));
            }
        }
        self.prev = Some(now);

        self.failures.retain(|f| (0..=WINDOW_LONG_MS).contains(&(ctx.ts_ms - f.ts_ms)));
        let short = self.failures.iter()
            .filter(|f| ctx.ts_ms - f.ts_ms <= WINDOW_SHORT_MS).count() as u32;

        let firewall = self.firewall();
        let unexpected: Vec<Listener> = listeners.iter().filter(|l| !l.expected).cloned().collect();
        let checks = {
            let exposed: Vec<&Listener> =
                listeners.iter().filter(|l| l.proto == "tcp" && l.exposed()).collect();
            self.checks(&firewall, &exposed, unexpected.len())
        };
        let st = SecurityState {
            exposed_tcp: listeners.iter().filter(|l| l.proto == "tcp" && l.exposed()).count(),
            unexpected,
            checks_failing: checks.iter().filter(|c| c.status == "warn").count(),
            checks,
            firewall,
            auth_failures_10m: short,
            auth_failures_1h: self.failures.len() as u32,
            recent_failures: self.failures.iter().rev().take(10).cloned().collect(),
            auth_log_watched: ctx.snap.security.auth_log_watched,
            baseline_ready: self.baseline.is_some(),
            listeners,
        };
        ctx.g("security", "exposed_tcp_listeners", "", st.exposed_tcp as f64);
        ctx.g("security", "auth_failures_10m", "", short as f64);
        ctx.snap.security = st;
        Ok(())
    }

    fn on_event(&mut self, ev: &ExternalEvent, ctx: &mut Ctx) -> CResult<()> {
        let ExternalEvent::AuthLog(entry) = ev else { return Ok(()) };
        self.auth_seen = true;
        let Some(mut f) = classify_auth(entry) else { return Ok(()) };
        f.ts_ms = ctx.ts_ms;
        let recent = self.failures.iter()
            .filter(|x| ctx.ts_ms - x.ts_ms <= WINDOW_SHORT_MS).count();
        if recent < MAX_FAILURE_EVENTS {
            let mut text = format!("Failed sign-in: {}", f.service);
            if !f.user.is_empty() {
                text.push_str(&format!(" as {}", f.user));
            }
            if !f.source.is_empty() {
                text.push_str(&format!(" from {}", f.source));
            }
            ctx.event(Event::new("security", "auth_failure", text, Severity::Notice));
        }
        self.failures.push(f);
        // Reflect it at once rather than a slow tick later.
        let s = &mut ctx.snap.security;
        s.auth_failures_10m += 1;
        s.auth_failures_1h += 1;
        s.recent_failures.insert(0, self.failures.last().cloned().unwrap_or_default());
        s.recent_failures.truncate(10);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:8D15 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 63086 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 50187 1 0000000000000000 100 0 0 10 0
   2: 760CA8C0:C350 5DB8D822:01BB 01 00000000:00000000 02:00000A3C 00000000  1000        0 91234 2 0000000000000000 24 4 30 10 -1
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0277 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 50186 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 50188 1 0000000000000000 100 0 0 10 0
";
    const UDP: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  100: 760CA8C0:0044 010CA8C0:0043 01 00000000:00000000 00:00000000 00000000     0        0 4001 2 0000000000000000 0
  200: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   103        0 4002 2 0000000000000000 0
";

    fn entry(id: &str, msg: &str) -> JournalEntry {
        JournalEntry { priority: 5, message: msg.into(), unit: String::new(),
                       syslog_id: id.into(), is_kernel: false, ..Default::default() }
    }

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jamsys-sec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("proc/net")).unwrap();
        std::fs::create_dir_all(d.join("etc/ufw")).unwrap();
        std::fs::write(d.join("etc/passwd"), "root:x:0:0::/root:/bin/sh\navahi:x:103:110::/:/bin/false\n").unwrap();
        std::fs::write(d.join("proc/net/tcp"), TCP).unwrap();
        std::fs::write(d.join("proc/net/tcp6"), TCP6).unwrap();
        std::fs::write(d.join("proc/net/udp"), UDP).unwrap();
        d
    }

    fn ctx() -> Ctx {
        Ctx::new(Arc::new(crate::config::Config::default()))
    }

    #[test]
    fn addresses_are_decoded_in_the_kernels_byte_order() {
        assert_eq!(parse_addr("0100007F"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(parse_addr("760CA8C0"), Some("192.168.12.118".parse().unwrap()));
        assert_eq!(parse_addr("00000000000000000000000001000000"), Some("::1".parse().unwrap()));
        assert_eq!(parse_addr("0000000000000000FFFF00000100007F"),
                   Some("::ffff:127.0.0.1".parse().unwrap()));
        assert_eq!(parse_addr("nonsense"), None);
        assert_eq!(parse_addr(""), None);
    }

    #[test]
    fn only_sockets_waiting_for_a_peer_are_listeners() {
        let t = parse_proc_net(TCP, true);
        assert_eq!(t.iter().map(|s| s.port).collect::<Vec<_>>(), vec![0x8D15, 22],
                   "the established connection (state 01) is not a listener");
        assert_eq!(t[1].uid, 0);
        assert_eq!(t[1].inode, 50187);
        let u = parse_proc_net(UDP, false);
        assert_eq!(u.len(), 1, "a connected UDP socket (the DHCP client) is not a listener");
        assert_eq!(u[0].port, 5353);
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        assert!(parse_proc_net("", true).is_empty());
        assert!(parse_proc_net("header\ngarbage line\n 0: zz:zz 00:00 0A\n", true).is_empty());
        assert!(parse_proc_net("header\n 0: 0100007F:0016 00000000:0000 0A a b c x y z\n", true).is_empty());
    }

    #[test]
    fn exposure_tells_local_from_reachable() {
        let e = |s: &str| exposure_of(&s.parse().unwrap());
        assert_eq!(e("127.0.0.1"), "loopback");
        assert_eq!(e("127.0.0.53"), "loopback");
        assert_eq!(e("::1"), "loopback");
        assert_eq!(e("::ffff:127.0.0.1"), "loopback");
        assert_eq!(e("0.0.0.0"), "all");
        assert_eq!(e("::"), "all");
        assert_eq!(e("192.168.1.5"), "address");
        assert_eq!(e("fe80::1"), "address");
    }

    #[test]
    fn a_random_high_port_is_one_identity_not_a_new_one_each_launch() {
        assert_eq!(listener_key("spotify", "tcp", 40123), listener_key("spotify", "tcp", 51777));
        assert_ne!(listener_key("root", "tcp", 22), listener_key("root", "tcp", 80));
    }

    #[test]
    fn failed_sign_ins_are_recognised_once_each() {
        let f = classify_auth(&entry("sudo",
            "pam_unix(sudo:auth): authentication failure; logname=ronin uid=1000 euid=0 tty=/dev/pts/0 ruser=ronin rhost=  user=ronin")).unwrap();
        assert_eq!((f.service.as_str(), f.user.as_str(), f.source.as_str()), ("sudo", "ronin", ""));

        let f = classify_auth(&entry("sshd-session",
            "Failed password for invalid user admin from 203.0.113.9 port 51234 ssh2")).unwrap();
        assert_eq!((f.service.as_str(), f.user.as_str(), f.source.as_str()),
                   ("sshd", "admin", "203.0.113.9"));
        let f = classify_auth(&entry("sshd", "Failed password for root from 198.51.100.7 port 22 ssh2")).unwrap();
        assert_eq!((f.user.as_str(), f.source.as_str()), ("root", "198.51.100.7"));

        // PAM's own line for the same sshd attempt must not count a second time.
        assert!(classify_auth(&entry("sshd",
            "pam_unix(sshd:auth): authentication failure; logname= uid=0 euid=0 tty=ssh ruser= rhost=203.0.113.9  user=root")).is_none());

        let f = classify_auth(&entry("polkitd",
            "Operator of unix-session:2 FAILED to authenticate to gain authorization for action org.freedesktop.policykit.exec")).unwrap();
        assert_eq!(f.service, "polkit");

        // Things that merely mention failure are not sign-in failures.
        assert!(classify_auth(&entry("sudo", "pam_unix(sudo:auth): conversation failed")).is_none());
        assert!(classify_auth(&entry("sudo", "pam_unix(sudo:session): session opened for user root")).is_none());
        assert!(classify_auth(&entry("sshd", "Accepted publickey for ronin from 10.0.0.2 port 5 ssh2")).is_none());
        assert!(classify_auth(&entry("kernel", "Failed password for root from 1.2.3.4")).is_none(),
                "only the processes that authenticate are believed");
    }

    #[test]
    fn listeners_are_merged_named_and_ordered() {
        let d = tree("merge");
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        let s = &x.snap.security;
        let ssh = s.listeners.iter().find(|l| l.port == 22).unwrap();
        assert_eq!(ssh.addresses.len(), 2, "0.0.0.0 and :: are one service: {:?}", ssh.addresses);
        assert_eq!((ssh.owner.as_str(), ssh.exposure.as_str()), ("root", "all"));
        assert_eq!(s.exposed_tcp, 1);
        assert_eq!(s.listeners[0].port, 22, "network-facing TCP is listed first");
        let mdns = s.listeners.iter().find(|l| l.port == 5353).unwrap();
        assert_eq!((mdns.proto.as_str(), mdns.owner.as_str()), ("udp", "avahi"));
        assert!(mdns.expected, "UDP is listed but never judged");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_baseline_is_learned_once_and_survives_a_restart() {
        let d = tree("baseline");
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        assert!(x.snap.security.unexpected.is_empty(), "what was there at first run is expected");
        assert!(d.join("known-listeners.json").exists());

        // A new network-facing listener appears: 0.0.0.0:8080 owned by root.
        let more = format!("{TCP}   3: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 77777 1 0000000000000000 100 0 0 10 0\n");
        std::fs::write(d.join("proc/net/tcp"), &more).unwrap();
        c.collect(&mut x).unwrap();
        let s = &x.snap.security;
        assert_eq!(s.unexpected.len(), 1);
        assert_eq!(s.unexpected[0].key, "root:tcp/8080");
        assert!(x.events.iter().any(|e| e.kind == "listener_opened" && e.summary.contains("8080")));

        // A restarted daemon must not quietly adopt it.
        let mut again = SecurityCollector::with_roots(&d);
        let mut y = ctx();
        again.collect(&mut y).unwrap();
        assert_eq!(y.snap.security.unexpected.len(), 1,
                   "re-learning on restart would accept an intruder's listener");

        // The user accepting it is what makes it expected.
        let mut cfg = crate::config::Config::default();
        cfg.security.accepted_listeners.push("root:tcp/8080".into());
        let mut z = Ctx::new(Arc::new(cfg));
        again.collect(&mut z).unwrap();
        assert!(z.snap.security.unexpected.is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_new_loopback_listener_is_noted_but_not_unexpected() {
        let d = tree("loop");
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        let more = format!("{TCP}   3: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 77777 1 0000000000000000 100 0 0 10 0\n");
        std::fs::write(d.join("proc/net/tcp"), &more).unwrap();
        c.collect(&mut x).unwrap();
        assert!(x.snap.security.unexpected.is_empty());
        assert!(x.events.iter().any(|e| e.kind == "listener_opened" && e.severity == Severity::Info));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_firewall_verdict_says_only_what_was_checked() {
        let d = tree("fw");
        let c = SecurityCollector::with_roots(&d);
        assert_eq!(c.firewall().state, "unknown", "no tool found is unknown, not 'off'");
        std::fs::write(d.join("etc/ufw/ufw.conf"), "# comment\nENABLED=no\nLOGLEVEL=low\n").unwrap();
        let f = c.firewall();
        assert_eq!((f.state.as_str(), f.tool.as_str()), ("disabled", "ufw"));
        assert!(f.detail.contains("not inspected"));
        std::fs::write(d.join("etc/ufw/ufw.conf"), "ENABLED=yes\n").unwrap();
        assert_eq!(c.firewall().state, "enabled");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn failures_are_counted_in_their_windows_and_expire() {
        let d = tree("auth");
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        let t0 = x.ts_ms;
        let line = entry("sudo", "pam_unix(sudo:auth): authentication failure; logname=a uid=1 euid=0 tty=x ruser=a rhost=  user=a");
        for _ in 0..3 {
            c.on_event(&ExternalEvent::AuthLog(line.clone()), &mut x).unwrap();
        }
        assert_eq!(x.snap.security.auth_failures_10m, 3, "visible before the next slow tick");
        assert_eq!(x.events.iter().filter(|e| e.kind == "auth_failure").count(), 3);
        c.collect(&mut x).unwrap();
        assert_eq!(x.snap.security.auth_failures_10m, 3);
        x.ts_ms = t0 + 11 * 60_000;
        c.collect(&mut x).unwrap();
        assert_eq!((x.snap.security.auth_failures_10m, x.snap.security.auth_failures_1h), (0, 3));
        x.ts_ms = t0 + 61 * 60_000;
        c.collect(&mut x).unwrap();
        assert_eq!(x.snap.security.auth_failures_1h, 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_flood_of_failures_does_not_flood_the_event_log() {
        let d = tree("flood");
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        let line = entry("sshd", "Failed password for root from 203.0.113.9 port 1 ssh2");
        for _ in 0..500 {
            c.on_event(&ExternalEvent::AuthLog(line.clone()), &mut x).unwrap();
        }
        assert_eq!(x.snap.security.auth_failures_10m, 500, "every attempt is counted");
        assert_eq!(x.events.len(), MAX_FAILURE_EVENTS, "but only the first few are logged one by one");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn apt_and_update_notifier_files_are_read_for_what_they_say() {
        assert_eq!(parse_auto_upgrades("APT::Periodic::Update-Package-Lists \"1\";\nAPT::Periodic::Unattended-Upgrade \"1\";\n"), Some(true));
        assert_eq!(parse_auto_upgrades("APT::Periodic::Unattended-Upgrade \"0\";"), Some(false));
        assert_eq!(parse_auto_upgrades("// APT::Periodic::Unattended-Upgrade \"1\";"), None, "a comment is not a setting");
        assert_eq!(parse_auto_upgrades(""), None);

        // Verbatim from this machine.
        let here = "\nExpanded Security Maintenance for Applications is not enabled.\n\n0 updates can be applied immediately.\n\n22 additional security updates can be applied with ESM Apps.\n";
        assert_eq!(parse_updates_available(here), Some((0, Some(0))),
                   "the ESM advertisement is not a count of waiting updates");
        let some = "14 updates can be applied immediately.\n9 of these updates are standard security updates.\n";
        assert_eq!(parse_updates_available(some), Some((14, Some(9))));
        assert_eq!(parse_updates_available("3 updates can be applied immediately.\n"), Some((3, None)));
        assert_eq!(parse_updates_available("nothing useful"), None);
    }

    #[test]
    fn the_checkup_reports_each_setting_or_admits_it_cannot() {
        let d = tree("checks");
        let by = |x: &Ctx, id: &str| x.snap.security.checks.iter().find(|c| c.id == id).cloned();
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        assert_eq!(by(&x, "firewall").unwrap().status, "unknown");
        assert_eq!(by(&x, "auto_updates").unwrap().status, "unknown");
        assert!(by(&x, "updates").is_none() && by(&x, "restart").is_none()
                && by(&x, "disk_encryption").is_none() && by(&x, "secure_boot").is_none(),
                "a check with no source is left out, not guessed");
        // sshd on 0.0.0.0 was there at first run: reachable, but expected.
        assert_eq!(by(&x, "exposure").unwrap().status, "note");
        assert_eq!(x.snap.security.checks_failing, 0);

        std::fs::write(d.join("etc/ufw/ufw.conf"), "ENABLED=no\n").unwrap();
        std::fs::create_dir_all(d.join("etc/apt/apt.conf.d")).unwrap();
        std::fs::write(d.join("etc/apt/apt.conf.d/20auto-upgrades"),
                       "APT::Periodic::Unattended-Upgrade \"0\";\n").unwrap();
        std::fs::create_dir_all(d.join("var/lib/update-notifier")).unwrap();
        std::fs::write(d.join("var/lib/update-notifier/updates-available"),
                       "5 updates can be applied immediately.\n2 of these updates are standard security updates.\n").unwrap();
        std::fs::create_dir_all(d.join("run")).unwrap();
        std::fs::write(d.join("run/reboot-required"), "").unwrap();
        std::fs::write(d.join("run/reboot-required.pkgs"), "linux-image-7.0.0-44-generic\n").unwrap();
        c.collect(&mut x).unwrap();
        let fw = by(&x, "firewall").unwrap();
        assert_eq!((fw.status.as_str(), fw.fix.as_str()), ("warn", "sudo ufw enable"));
        assert_eq!(by(&x, "auto_updates").unwrap().status, "warn");
        assert!(by(&x, "updates").unwrap().title.starts_with("2 security"));
        assert!(by(&x, "restart").unwrap().detail.contains("linux-image"));
        assert_eq!(x.snap.security.checks_failing, 4);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn disk_encryption_is_followed_through_device_mapper() {
        let d = tree("crypt");
        let block = d.join("sys/class/block");
        // Plain partition.
        std::fs::create_dir_all(block.join("nvme0n1p7")).unwrap();
        // LVM volume (dm-1) on a LUKS container (dm-0).
        std::fs::create_dir_all(block.join("dm-0/dm")).unwrap();
        std::fs::write(block.join("dm-0/dm/uuid"), "CRYPT-LUKS2-abc-nvme0n1p3_crypt\n").unwrap();
        std::fs::create_dir_all(block.join("dm-1/dm")).unwrap();
        std::fs::write(block.join("dm-1/dm/uuid"), "LVM-xyz\n").unwrap();
        std::fs::create_dir_all(block.join("dm-1/slaves/dm-0")).unwrap();
        let c = SecurityCollector::with_roots(&d);
        assert!(!c.encrypted("nvme0n1p7", 0));
        assert!(c.encrypted("dm-0", 0));
        assert!(c.encrypted("dm-1", 0), "LVM on LUKS is encrypted");
        assert!(!c.encrypted("missing", 0));

        std::fs::create_dir_all(d.join("proc/self")).unwrap();
        std::fs::write(d.join("proc/self/mountinfo"),
            "25 1 259:7 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p7 rw\n30 25 0:26 / /sys rw - sysfs sysfs rw\n").unwrap();
        assert_eq!(c.root_device().as_deref(), Some("nvme0n1p7"));
        std::fs::create_dir_all(d.join("sys/firmware/efi/efivars")).unwrap();
        std::fs::write(d.join("sys/firmware/efi/efivars/SecureBoot-8be4df61"), [6u8, 0, 0, 0, 1]).unwrap();
        let mut c = SecurityCollector::with_roots(&d);
        let mut x = ctx();
        c.collect(&mut x).unwrap();
        let get = |id: &str| x.snap.security.checks.iter().find(|c| c.id == id).unwrap().status.clone();
        assert_eq!(get("disk_encryption"), "note", "not simply fixable, so not a failing check");
        assert_eq!(get("secure_boot"), "ok");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_machine_without_proc_net_is_unsupported_not_an_error() {
        let d = std::env::temp_dir().join(format!("jamsys-sec-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let mut c = SecurityCollector::with_roots(&d);
        assert!(matches!(c.probe(), Support::Unsupported { .. }));
        assert!(matches!(c.collect(&mut ctx()), Err(CollectorError::Gone(_))));
        std::fs::remove_dir_all(&d).ok();
    }
}
