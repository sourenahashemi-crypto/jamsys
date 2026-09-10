//! Reader for the optional privileged helper's output.
//!
//! The helper has **no request channel**: it is a `Type=oneshot` unit on a timer that
//! reads two fixed paths and writes `/run/jamsys/privileged.json`. The daemon simply
//! reads that file. There is no root-privileged parser reachable from unprivileged
//! code, because there is nothing for unprivileged code to send.
//!
//! Everything here degrades to `None` when the helper is not installed.

use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const HELPER_OUTPUT: &str = "/run/jamsys/privileged.json";
/// Output older than this is treated as absent: a stopped helper must downgrade
/// coverage rather than pin the UI to a stale reading forever.
const STALE_AFTER: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SmartLog {
    pub critical_warning: Option<u8>,
    pub temperature_c: Option<f64>,
    pub available_spare: Option<u8>,
    pub available_spare_threshold: Option<u8>,
    pub percentage_used: Option<u8>,
    pub data_units_read: Option<u64>,
    pub data_units_written: Option<u64>,
    pub power_cycles: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub media_errors: Option<u64>,
    pub error_log_entries: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PrivilegedData {
    pub ts_ms: i64,
    pub version: u32,
    /// Watts, averaged over the helper's own sampling interval.
    pub cpu_package_w: Option<f64>,
    pub dram_w: Option<f64>,
    /// Keyed by controller name, e.g. "nvme0".
    #[serde(default)]
    pub nvme: std::collections::HashMap<String, SmartLog>,
}

struct Cache {
    data: Option<PrivilegedData>,
    read_at: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { data: None, read_at: None });

/// Re-read at most once every 10 s; the file itself only changes once a minute.
fn load() -> Option<PrivilegedData> {
    let mut c = CACHE.lock().ok()?;
    let fresh = c.read_at.map(|t| t.elapsed() < Duration::from_secs(10)).unwrap_or(false);
    if !fresh {
        c.read_at = Some(Instant::now());
        c.data = std::fs::read_to_string(HELPER_OUTPUT)
            .ok()
            .and_then(|t| serde_json::from_str::<PrivilegedData>(&t).ok())
            .filter(|d| {
                let age = crate::clock::now_ms() - d.ts_ms;
                age >= 0 && age < STALE_AFTER.as_millis() as i64
            });
    }
    c.data.clone()
}

pub fn available() -> bool {
    load().is_some()
}

pub fn cpu_package_w() -> Option<f64> {
    load()?.cpu_package_w
}

pub fn nvme_smart_available() -> bool {
    load().map(|d| !d.nvme.is_empty()).unwrap_or(false)
}

pub fn nvme_smart(dev: &str) -> Option<SmartLog> {
    load()?.nvme.get(dev).cloned()
}

/// Decode an NVMe SMART / Health Information log page (log id 0x02, 512 bytes).
///
/// Shared with the helper so the byte layout is defined and tested in exactly one
/// place. Multi-byte fields are little-endian; several are 128-bit, of which only the
/// low 64 bits are kept (the high half would need 10^20 writes to become non-zero).
pub fn parse_smart_log(b: &[u8]) -> Option<SmartLog> {
    if b.len() < 512 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u64le = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let raw_temp = u16le(1) as f64;
    Some(SmartLog {
        critical_warning: Some(b[0]),
        // Kelvin in the log page; 0 means "not reported".
        temperature_c: if raw_temp > 0.0 { crate::util::sane(raw_temp - 273.15, -40.0, 150.0) } else { None },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_log() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0x00; // critical_warning: none
        // 313.15 K = 40 C
        b[1..3].copy_from_slice(&313u16.to_le_bytes());
        b[3] = 100; // available spare %
        b[4] = 10; // spare threshold %
        b[5] = 3; // percentage used
        b[48..56].copy_from_slice(&123_456u64.to_le_bytes()); // data units written
        b[128..136].copy_from_slice(&1_234u64.to_le_bytes()); // power on hours
        b[160..168].copy_from_slice(&0u64.to_le_bytes()); // media errors
        b
    }

    #[test]
    fn smart_log_decodes_the_fields_that_matter() {
        let s = parse_smart_log(&synthetic_log()).unwrap();
        assert_eq!(s.critical_warning, Some(0));
        assert!((s.temperature_c.unwrap() - 39.85).abs() < 0.01, "got {:?}", s.temperature_c);
        assert_eq!(s.available_spare, Some(100));
        assert_eq!(s.available_spare_threshold, Some(10));
        assert_eq!(s.percentage_used, Some(3));
        assert_eq!(s.data_units_written, Some(123_456));
        assert_eq!(s.power_on_hours, Some(1_234));
        assert_eq!(s.media_errors, Some(0));
    }

    #[test]
    fn a_failing_drive_is_visible_in_the_decoded_log() {
        let mut b = synthetic_log();
        b[0] = 0x08; // bit 3: media has entered read-only mode
        b[3] = 5; // spare below threshold
        b[160..168].copy_from_slice(&42u64.to_le_bytes());
        let s = parse_smart_log(&b).unwrap();
        assert_eq!(s.critical_warning, Some(8));
        assert!(s.available_spare.unwrap() < s.available_spare_threshold.unwrap());
        assert_eq!(s.media_errors, Some(42));
    }

    #[test]
    fn a_short_or_empty_buffer_is_rejected() {
        assert!(parse_smart_log(&[]).is_none());
        assert!(parse_smart_log(&vec![0u8; 511]).is_none());
    }

    #[test]
    fn a_zero_temperature_reads_as_unknown_not_minus_273() {
        let mut b = synthetic_log();
        b[1..3].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(parse_smart_log(&b).unwrap().temperature_c, None);
    }

    #[test]
    fn missing_helper_degrades_to_none_everywhere() {
        // The helper is not installed in the test environment.
        if !std::path::Path::new(HELPER_OUTPUT).exists() {
            assert!(!available());
            assert_eq!(cpu_package_w(), None);
            assert!(!nvme_smart_available());
            assert_eq!(nvme_smart("nvme0").map(|_| ()), None);
        }
    }
}
