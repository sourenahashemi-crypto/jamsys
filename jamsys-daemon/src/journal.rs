//! Journal ingestion — event-driven, not polled.
//!
//! `libsystemd` development headers are not installed on the target and pulling in a
//! C dependency for this would make the build fragile, so the daemon instead spawns
//! **one** long-lived `journalctl --follow` and reads its stdout as an epoll source.
//! That satisfies the real requirement — never repeatedly scan the log — while keeping
//! the build dependency-free.
//!
//! Server-side filtering (`-p 4`) means info-level chatter never even crosses the pipe.

use crate::types::Severity;
use crate::util::glob_match;
use serde::Deserialize;
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};

#[derive(Clone, Debug, Default)]
pub struct JournalEntry {
    pub ts_ms: i64,
    pub priority: u8,
    pub message: String,
    pub unit: String,
    pub syslog_id: String,
    pub is_kernel: bool,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime: Option<String>,
    #[serde(rename = "PRIORITY")]
    priority: Option<serde_json::Value>,
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    unit: Option<String>,
    #[serde(rename = "UNIT")]
    unit2: Option<String>,
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_id: Option<String>,
    #[serde(rename = "_TRANSPORT")]
    transport: Option<String>,
}

/// `MESSAGE` is a string normally, but journald emits an array of byte values for
/// entries containing non-UTF-8 data. Handle both rather than dropping the line.
fn message_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let bytes: Vec<u8> = a.iter().filter_map(|x| x.as_u64()).map(|x| x as u8).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => String::new(),
    }
}

pub fn parse_line(line: &str) -> Option<JournalEntry> {
    let raw: RawEntry = serde_json::from_str(line).ok()?;
    let msg = raw.message.as_ref().map(message_to_string).unwrap_or_default();
    if msg.is_empty() {
        return None;
    }
    let priority = raw
        .priority
        .as_ref()
        .and_then(|p| match p {
            serde_json::Value::String(s) => s.parse::<u8>().ok(),
            serde_json::Value::Number(n) => n.as_u64().map(|x| x as u8),
            _ => None,
        })
        .unwrap_or(6);
    // journald's __REALTIME_TIMESTAMP is microseconds since the epoch.
    let ts_ms = raw
        .realtime
        .as_ref()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|us| us / 1000)
        .unwrap_or_else(crate::clock::now_ms);
    Some(JournalEntry {
        ts_ms,
        priority,
        message: msg,
        unit: raw.unit.or(raw.unit2).unwrap_or_default(),
        syslog_id: raw.syslog_id.unwrap_or_default(),
        is_kernel: raw.transport.as_deref() == Some("kernel"),
    })
}

/// What a journal line means, if anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    pub kind: &'static str,
    pub severity: Severity,
    /// True when this should raise an alert rather than only record an event.
    pub alertable: bool,
}

/// Pattern table. Ordered: the first match wins, so specific patterns precede general.
///
/// Deliberately a plain glob table rather than a regex engine — it keeps the binary
/// small and every entry stays readable by someone who is not a regex author.
const PATTERNS: &[(&str, &str, Severity, bool)] = &[
    // --- GPU -------------------------------------------------------------
    ("*NVRM: Xid*", "gpu_xid", Severity::Warning, true),
    ("*GPU has fallen off the bus*", "gpu_off_bus", Severity::Critical, true),
    ("*nvidia*: GPU at PCI*has fallen*", "gpu_off_bus", Severity::Critical, true),
    ("*GPU reset*", "gpu_reset", Severity::Warning, true),
    ("*[drm]*ERROR*reset*", "gpu_reset", Severity::Warning, true),
    ("*i915*GPU HANG*", "gpu_hang", Severity::Warning, true),
    ("*amdgpu*ring*timeout*", "gpu_hang", Severity::Warning, true),
    // --- memory ----------------------------------------------------------
    ("*Out of memory: Killed process*", "oom", Severity::Critical, true),
    ("*oom-kill:*", "oom", Severity::Critical, true),
    ("*invoked oom-killer*", "oom", Severity::Critical, true),
    // --- storage ---------------------------------------------------------
    ("*nvme*: I/O*QID*timeout*", "nvme_timeout", Severity::Critical, true),
    ("*nvme*: Removing after probe failure*", "nvme_fail", Severity::Critical, true),
    ("*Buffer I/O error on*", "io_error", Severity::Critical, true),
    ("*critical medium error*", "media_error", Severity::Critical, true),
    ("*EXT4-fs error*", "fs_error", Severity::Critical, true),
    ("*EXT4-fs*: Remounting filesystem read-only*", "fs_readonly", Severity::Critical, true),
    ("*BTRFS error*", "fs_error", Severity::Critical, true),
    ("*XFS*Corruption*", "fs_error", Severity::Critical, true),
    ("*I/O error, dev *", "io_error", Severity::Warning, true),
    // --- thermal ---------------------------------------------------------
    ("*CPU*above threshold, cpu clock throttled*", "thermal_throttle", Severity::Warning, true),
    ("*Package temperature above threshold*", "thermal_throttle", Severity::Warning, true),
    ("*critical temperature reached*", "thermal_critical", Severity::Critical, true),
    ("*thermal zone*: critical temperature*", "thermal_critical", Severity::Critical, true),
    // --- hardware --------------------------------------------------------
    ("*Machine check events logged*", "mce", Severity::Critical, true),
    ("*Hardware Error*", "hw_error", Severity::Critical, true),
    ("*PCIe Bus Error*", "pcie_error", Severity::Warning, true),
    ("*AER:*severity=Uncorrected*", "pcie_error", Severity::Critical, true),
    // --- network ---------------------------------------------------------
    ("*rtw89*: firmware*fail*", "wifi_fw_error", Severity::Warning, true),
    ("*iwlwifi*: Microcode SW error*", "wifi_fw_error", Severity::Warning, true),
    ("*: Firmware error*", "wifi_fw_error", Severity::Warning, true),
    ("*deauthenticating from*", "wifi_deauth", Severity::Info, false),
    ("*: Link is Down*", "link_down", Severity::Info, false),
    ("*r8169*: rtl_*_cond == 1*", "net_driver_error", Severity::Warning, true),
    // --- bluetooth -------------------------------------------------------
    ("*Bluetooth: hci*: hardware error*", "bt_hw_error", Severity::Warning, true),
    ("*Bluetooth: hci*: command*tx timeout*", "bt_timeout", Severity::Notice, false),
    // --- power / suspend --------------------------------------------------
    ("*PM: suspend entry*", "suspend_entry", Severity::Info, false),
    ("*PM: suspend exit*", "suspend_exit", Severity::Info, false),
    ("*PM: Some devices failed to suspend*", "suspend_failed", Severity::Warning, true),
    ("*PM: dpm_run_callback(): *returns -*", "resume_device_error", Severity::Warning, true),
    ("*Freezing of tasks failed*", "suspend_failed", Severity::Warning, true),
    // --- audio -----------------------------------------------------------
    ("*snd_hda_intel*: azx_get_response timeout*", "audio_error", Severity::Warning, true),
    ("*pipewire*: *: error*", "audio_error", Severity::Notice, false),
    // --- usb -------------------------------------------------------------
    ("*usb*: device descriptor read/64, error*", "usb_error", Severity::Notice, false),
    ("*usb*: device not accepting address*", "usb_error", Severity::Warning, true),
];

/// Classify a message. Returns `None` for anything unrecognised — unrecognised lines
/// become events, never alerts, which is what stops the journal from being a firehose.
pub fn classify(msg: &str) -> Option<Classification> {
    for (pat, kind, sev, alertable) in PATTERNS {
        if glob_match(pat, msg) {
            return Some(Classification { kind, severity: *sev, alertable: *alertable });
        }
    }
    None
}

/// NVIDIA Xid codes are not equally serious. Treating them all as CRITICAL is how a
/// monitor teaches its user to ignore it.
pub fn classify_xid(msg: &str) -> (Option<u32>, Severity) {
    // Format: "NVRM: Xid (PCI:0000:01:00): 13, pid=1234, ..."
    let Some(idx) = msg.find("Xid") else { return (None, Severity::Warning) };
    let tail = &msg[idx..];
    let Some(colon) = tail.find("): ") else { return (None, Severity::Warning) };
    let num: String = tail[colon + 3..].chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(code) = num.parse::<u32>() else { return (None, Severity::Warning) };
    let sev = match code {
        45 => Severity::Info,               // preemptive channel removal, often Ctrl-C
        43 => Severity::Notice,             // channel reset by a user app
        13 | 31 => Severity::Warning,       // graphics exception / MMU fault: app bug
        119 | 120 => Severity::Warning,     // GSP RPC timeout
        92 => Severity::Warning,            // contained ECC
        48 | 63 | 64 | 94 | 95 => Severity::Critical, // ECC / uncontained
        79 => Severity::Critical,           // GPU has fallen off the bus
        _ => Severity::Warning,
    };
    (Some(code), sev)
}

/// The long-lived `journalctl --follow` child.
pub struct JournalStream {
    child: Child,
    fd: RawFd,
    buf: Vec<u8>,
}

impl JournalStream {
    /// `priority` is the syslog level ceiling (4 = warning and above).
    pub fn spawn(priority: u8) -> std::io::Result<JournalStream> {
        // Fixed argument vector, no shell. `-n 0` means "start from now": the daemon
        // must not replay the whole boot's warnings and notify about all of them.
        let mut child = Command::new("journalctl")
            .args(["--follow", "--output=json", "--no-pager", "--lines=0", "--priority"])
            .arg(priority.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "journalctl produced no stdout")
        })?;
        let fd = stdout.as_raw_fd();
        // Leak the handle: the fd is owned by epoll for the process lifetime and is
        // reclaimed when the child is reaped in `Drop`.
        std::mem::forget(stdout);
        crate::eventloop::set_nonblocking(fd)?;
        Ok(JournalStream { child, fd, buf: Vec::with_capacity(8192) })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Drain whatever is readable and return the complete entries found.
    pub fn read_entries(&mut self) -> Vec<JournalEntry> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: reading into a stack buffer of the declared length from an fd we own.
            let n = unsafe { libc::read(self.fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
            if n <= 0 {
                break;
            }
            self.buf.extend_from_slice(&chunk[..n as usize]);
            // A pathological producer must not let the buffer grow without bound.
            if self.buf.len() > 1 << 20 {
                crate::log_warn!("journal buffer exceeded 1 MiB, discarding partial line");
                self.buf.clear();
            }
        }
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Ok(s) = std::str::from_utf8(&line[..line.len() - 1]) {
                if let Some(e) = parse_line(s) {
                    out.push(e);
                }
            }
        }
        out
    }

    /// Has the child exited? If journald restarts, the follow dies and must be respawned.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for JournalStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SAFETY: fd was leaked from the child's stdout and is not otherwise owned.
        unsafe { libc::close(self.fd) };
    }
}

/// Collapses identical messages so one flapping driver cannot produce a hundred alerts.
pub struct Deduper {
    seen: std::collections::HashMap<String, (i64, u32)>,
    window_ms: i64,
}

impl Deduper {
    pub fn new(window_ms: i64) -> Self {
        Deduper { seen: std::collections::HashMap::new(), window_ms }
    }

    /// Returns `Some(count_since_last_report)` if this key should be reported now.
    pub fn admit(&mut self, key: &str, now_ms: i64) -> Option<u32> {
        match self.seen.get_mut(key) {
            Some((last, n)) if now_ms - *last < self.window_ms => {
                *n += 1;
                None
            }
            Some((last, n)) => {
                let suppressed = *n;
                *last = now_ms;
                *n = 0;
                Some(suppressed)
            }
            None => {
                self.seen.insert(key.to_string(), (now_ms, 0));
                Some(0)
            }
        }
    }

    /// Drop keys not seen for ten windows so the map cannot grow unbounded on a
    /// long-running system.
    pub fn gc(&mut self, now_ms: i64) {
        let w = self.window_ms * 10;
        self.seen.retain(|_, (last, _)| now_ms - *last < w);
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_journald_json_line() {
        let line = r#"{"__REALTIME_TIMESTAMP":"1757422415123456","PRIORITY":"3","MESSAGE":"NVRM: Xid (PCI:0000:01:00): 79, GPU has fallen off the bus.","_TRANSPORT":"kernel","SYSLOG_IDENTIFIER":"kernel"}"#;
        let e = parse_line(line).unwrap();
        assert_eq!(e.priority, 3);
        assert_eq!(e.ts_ms, 1_757_422_415_123);
        assert!(e.is_kernel);
        assert!(e.message.contains("Xid"));
    }

    #[test]
    fn handles_a_binary_message_array() {
        // journald emits MESSAGE as a byte array when the text is not valid UTF-8.
        let line = r#"{"MESSAGE":[104,105,255],"PRIORITY":4}"#;
        let e = parse_line(line).unwrap();
        assert!(e.message.starts_with("hi"));
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("{}").is_none());
        assert!(parse_line(r#"{"MESSAGE":""}"#).is_none());
    }

    #[test]
    fn classifies_the_events_that_matter() {
        assert_eq!(classify("NVRM: Xid (PCI:0000:01:00): 13, pid=1").unwrap().kind, "gpu_xid");
        assert_eq!(classify("Out of memory: Killed process 1234 (firefox)").unwrap().kind, "oom");
        assert_eq!(classify("EXT4-fs error (device nvme0n1p7): ext4_find_entry").unwrap().kind, "fs_error");
        assert_eq!(classify("nvme nvme0: I/O 12 QID 3 timeout, aborting").unwrap().kind, "nvme_timeout");
        assert!(classify("Package temperature above threshold, cpu clock throttled").unwrap().alertable);
    }

    #[test]
    fn ordinary_chatter_is_not_classified() {
        assert!(classify("Started Daily apt download activities.").is_none());
        assert!(classify("gnome-shell: Window manager warning: last_focus_time").is_none());
    }

    #[test]
    fn informational_matches_are_recorded_but_not_alertable() {
        let c = classify("wlp108s0: deauthenticating from aa:bb:cc:dd:ee:ff by local choice").unwrap();
        assert_eq!(c.kind, "wifi_deauth");
        assert!(!c.alertable, "a normal roam must not raise an alert");
    }

    #[test]
    fn xid_severity_is_graded_not_uniform() {
        assert_eq!(classify_xid("NVRM: Xid (PCI:0000:01:00): 79, GPU has fallen off the bus"),
                   (Some(79), Severity::Critical));
        assert_eq!(classify_xid("NVRM: Xid (PCI:0000:01:00): 45, Ch 00000008"),
                   (Some(45), Severity::Info));
        assert_eq!(classify_xid("NVRM: Xid (PCI:0000:01:00): 13, Graphics Exception"),
                   (Some(13), Severity::Warning));
        assert_eq!(classify_xid("NVRM: Xid (PCI:0000:01:00): 48, Double Bit ECC"),
                   (Some(48), Severity::Critical));
    }

    #[test]
    fn unparseable_xid_defaults_to_warning_not_a_panic() {
        assert_eq!(classify_xid("NVRM: Xid weird format"), (None, Severity::Warning));
        assert_eq!(classify_xid("no xid here"), (None, Severity::Warning));
    }

    #[test]
    fn deduper_collapses_a_flapping_source() {
        let mut d = Deduper::new(300_000);
        assert_eq!(d.admit("k", 0), Some(0), "first is always reported");
        for i in 1..50 {
            assert_eq!(d.admit("k", i * 1000), None, "inside the window");
        }
        // After the window, report once and say how many were suppressed.
        assert_eq!(d.admit("k", 400_000), Some(49));
        assert_eq!(d.admit("k", 400_001), None);
    }

    #[test]
    fn deduper_keys_are_independent() {
        let mut d = Deduper::new(1000);
        assert!(d.admit("a", 0).is_some());
        assert!(d.admit("b", 0).is_some(), "a different key is not suppressed");
    }

    #[test]
    fn deduper_gc_bounds_memory() {
        let mut d = Deduper::new(1000);
        for i in 0..500 {
            d.admit(&format!("k{i}"), 0);
        }
        assert_eq!(d.len(), 500);
        d.gc(100_000);
        assert_eq!(d.len(), 0, "stale keys must be reclaimed");
    }
}
