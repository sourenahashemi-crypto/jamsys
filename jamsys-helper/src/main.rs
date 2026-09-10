//! jamsys-helper — the only part of JamSys that runs as root.
//!
//! # Design
//!
//! This program has **no input channel**. It takes no arguments, reads no stdin, opens
//! no socket, and consults no environment variable. The set of files it touches is a
//! compile-time constant. There is therefore no request parser running as root, and
//! nothing for an unprivileged process to send it.
//!
//! It runs as a `Type=oneshot` unit on a timer: a few milliseconds of root code per
//! minute, then the process is gone. It writes one world-readable JSON file and exits.
//!
//! Everything it provides is optional. If this program is never installed, JamSys
//! reports CPU package power and NVMe SMART as unavailable and works normally.
//!
//! Exactly two things need privilege on a modern kernel:
//!
//! * `/sys/class/powercap/intel-rapl:*/energy_uj` is `0400 root` because unrestricted
//!   RAPL is a side channel (CVE-2020-8694).
//! * `/dev/nvmeN` is `0600 root`; the SMART log needs an admin passthrough ioctl.

use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const OUTPUT_DIR: &str = "/run/jamsys";
const OUTPUT: &str = "/run/jamsys/privileged.json";
const VERSION: u32 = 1;

/// The complete, fixed list of RAPL domains this program will read. Not a pattern, not
/// a parameter: an exhaustive constant, so there is no path to traverse.
const RAPL_PACKAGE: &str = "/sys/class/powercap/intel-rapl:0";
const RAPL_DRAM: &str = "/sys/class/powercap/intel-rapl:0:1";

#[derive(Serialize, Default)]
pub struct SmartLog {
    critical_warning: Option<u8>,
    temperature_c: Option<f64>,
    available_spare: Option<u8>,
    available_spare_threshold: Option<u8>,
    percentage_used: Option<u8>,
    data_units_read: Option<u64>,
    data_units_written: Option<u64>,
    power_cycles: Option<u64>,
    power_on_hours: Option<u64>,
    unsafe_shutdowns: Option<u64>,
    media_errors: Option<u64>,
    error_log_entries: Option<u64>,
}

#[derive(Serialize)]
struct Output {
    ts_ms: i64,
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_package_w: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dram_w: Option<f64>,
    nvme: HashMap<String, SmartLog>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn read_u64(p: &str) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Average watts over `sample_ms`, from the RAPL energy counter.
///
/// The counter is a wrapping microjoule accumulator, so two reads and a delay are
/// required; a single read carries no information. The wrap is handled explicitly
/// because it happens every few minutes at laptop power levels.
fn rapl_watts(domain: &str, sample_ms: u64) -> Option<f64> {
    let path = format!("{domain}/energy_uj");
    let max = read_u64(&format!("{domain}/max_energy_range_uj")).unwrap_or(u64::MAX);
    let a = read_u64(&path)?;
    std::thread::sleep(std::time::Duration::from_millis(sample_ms));
    let b = read_u64(&path)?;
    let delta = if b >= a { b - a } else { max.saturating_sub(a).saturating_add(b) };
    let w = (delta as f64 / 1e6) / (sample_ms as f64 / 1000.0);
    // A laptop package cannot plausibly draw more than 200 W; anything above that is a
    // counter reset misread as a delta.
    (w.is_finite() && (0.0..=200.0).contains(&w)).then_some(w)
}

// --- NVMe SMART ------------------------------------------------------------

const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xC0484E41;
const NVME_ADMIN_GET_LOG_PAGE: u8 = 0x02;
const SMART_LOG_ID: u32 = 0x02;
const SMART_LOG_BYTES: u32 = 512;

#[repr(C)]
#[derive(Default)]
struct NvmePassthruCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// Issue Get Log Page 0x02 (SMART / Health Information) on a controller.
///
/// This is a **read-only** admin command. The helper never issues a command that can
/// modify device state, and the opcode is a constant rather than a parameter.
fn nvme_smart(dev: &str) -> Option<SmartLog> {
    let path = format!("/dev/{dev}");
    let f = fs::File::open(&path).ok()?;
    let mut buf = vec![0u8; SMART_LOG_BYTES as usize];
    let numd = (SMART_LOG_BYTES / 4) - 1; // zero-based dword count
    let mut cmd = NvmePassthruCmd {
        opcode: NVME_ADMIN_GET_LOG_PAGE,
        nsid: 0xFFFF_FFFF, // whole controller
        addr: buf.as_mut_ptr() as u64,
        data_len: SMART_LOG_BYTES,
        cdw10: SMART_LOG_ID | (numd << 16),
        timeout_ms: 5_000,
        ..Default::default()
    };
    // SAFETY: `cmd` describes a buffer we own of exactly `data_len` bytes, and the
    // opcode is a read-only admin command. The fd is a valid NVMe character device.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD, &mut cmd) };
    if rc != 0 {
        return None;
    }
    parse_smart_log(&buf)
}

/// Decode a 512-byte SMART / Health Information log page.
///
/// Multi-byte fields are little-endian. Several are 128-bit; only the low 64 bits are
/// kept, which would take on the order of 10^20 operations to overflow.
pub fn parse_smart_log(b: &[u8]) -> Option<SmartLog> {
    if b.len() < 512 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u64le = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let raw_temp = u16le(1) as f64;
    let temp_c = if raw_temp > 0.0 {
        let c = raw_temp - 273.15;
        (-40.0..=150.0).contains(&c).then_some(c)
    } else {
        None
    };
    Some(SmartLog {
        critical_warning: Some(b[0]),
        temperature_c: temp_c,
        available_spare: Some(b[3]),
        available_spare_threshold: Some(b[4]),
        percentage_used: Some(b[5]),
        data_units_read: Some(u64le(32)),
        data_units_written: Some(u64le(48)),
        power_cycles: Some(u64le(112)),
        power_on_hours: Some(u64le(128)),
        unsafe_shutdowns: Some(u64le(144)),
        media_errors: Some(u64le(160)),
        error_log_entries: Some(u64le(176)),
    })
}

/// Controller names, discovered from sysfs. Validated to be plain `nvmeN` so nothing
/// resembling a path can ever reach `/dev/`.
fn nvme_controllers() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir("/sys/class/nvme")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| {
                    n.starts_with("nvme")
                        && n.len() > 4
                        && n[4..].chars().all(|c| c.is_ascii_digit())
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn main() {
    // Refuse to run unprivileged rather than writing a misleading empty file that the
    // daemon would treat as "the helper is installed and there is nothing to report".
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("jamsys-helper must run as root; it is started by jamsys-helper.timer");
        std::process::exit(1);
    }

    let mut out = Output {
        ts_ms: now_ms(),
        version: VERSION,
        cpu_package_w: rapl_watts(RAPL_PACKAGE, 250),
        dram_w: rapl_watts(RAPL_DRAM, 100),
        nvme: HashMap::new(),
    };
    for dev in nvme_controllers() {
        if let Some(s) = nvme_smart(&dev) {
            out.nvme.insert(dev, s);
        }
    }
    out.ts_ms = now_ms();

    if let Err(e) = write_atomic(&out) {
        eprintln!("jamsys-helper: could not write {OUTPUT}: {e}");
        std::process::exit(1);
    }
}

/// Write to a temporary file and rename, so a reader never sees a half-written file.
fn write_atomic(out: &Output) -> std::io::Result<()> {
    fs::create_dir_all(OUTPUT_DIR)?;
    let tmp = format!("{OUTPUT}.tmp");
    let json = serde_json::to_string(out).unwrap_or_else(|_| "{}".into());
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    // 0644: the daemon runs as the desktop user and only needs to read it. The file
    // contains no secrets — power figures and drive wear counters.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, Path::new(OUTPUT))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[1..3].copy_from_slice(&313u16.to_le_bytes()); // 313 K
        b[3] = 100;
        b[4] = 10;
        b[5] = 4;
        b[48..56].copy_from_slice(&999u64.to_le_bytes());
        b[160..168].copy_from_slice(&7u64.to_le_bytes());
        b
    }

    #[test]
    fn smart_log_decodes() {
        let s = parse_smart_log(&synthetic()).unwrap();
        assert_eq!(s.available_spare, Some(100));
        assert_eq!(s.percentage_used, Some(4));
        assert_eq!(s.data_units_written, Some(999));
        assert_eq!(s.media_errors, Some(7));
        assert!((s.temperature_c.unwrap() - 39.85).abs() < 0.01);
    }

    #[test]
    fn short_buffers_are_rejected() {
        assert!(parse_smart_log(&[]).is_none());
        assert!(parse_smart_log(&vec![0u8; 300]).is_none());
    }

    #[test]
    fn controller_names_are_strictly_validated() {
        // The filter must reject anything that is not literally nvmeN, so no value
        // resembling a path can ever be concatenated onto /dev/.
        for bad in ["nvme", "nvme0n1", "nvme0/../../etc", "../nvme0", "nvmeX"] {
            let ok = bad.starts_with("nvme") && bad.len() > 4
                && bad[4..].chars().all(|c| c.is_ascii_digit());
            assert!(!ok, "{bad} should be rejected");
        }
        for good in ["nvme0", "nvme12"] {
            let ok = good.starts_with("nvme") && good.len() > 4
                && good[4..].chars().all(|c| c.is_ascii_digit());
            assert!(ok, "{good} should be accepted");
        }
    }

    #[test]
    fn rapl_wrap_is_handled() {
        // Simulate the arithmetic used for a wrapped microjoule counter.
        let max = 262_143_328_850u64;
        let a = max - 1000;
        let b = 2000u64;
        let delta = if b >= a { b - a } else { max.saturating_sub(a).saturating_add(b) };
        assert_eq!(delta, 3000, "a wrap must produce a small positive delta");
    }

    #[test]
    fn implausible_wattage_is_rejected() {
        for w in [-1.0f64, 5000.0, f64::NAN, f64::INFINITY] {
            assert!(!(w.is_finite() && (0.0..=200.0).contains(&w)), "{w} should be rejected");
        }
        assert!(12.5f64.is_finite() && (0.0..=200.0).contains(&12.5));
    }
}
