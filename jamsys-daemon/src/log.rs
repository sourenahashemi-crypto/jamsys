//! Application logging.
//!
//! Deliberately separate from host alerts: "the daemon could not read a sensor" is an
//! *application* problem and belongs in the log, while "the CPU is at 97 °C" is a *host*
//! problem and belongs in the alert stream. Mixing the two trains users to ignore both.
//!
//! Output goes to stderr, which systemd routes into the journal under the unit name.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(2); // info

pub fn set_level(l: &str) {
    let v = match l.to_ascii_lowercase().as_str() {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        "trace" => 4,
        _ => 2,
    };
    LEVEL.store(v, Ordering::Relaxed);
}

pub fn enabled(v: u8) -> bool {
    v <= LEVEL.load(Ordering::Relaxed)
}

pub fn emit(tag: &str, msg: &str) {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "[{tag}] {msg}");
}

#[macro_export]
macro_rules! log_error { ($($a:tt)*) => { if $crate::log::enabled(0) { $crate::log::emit("error", &format!($($a)*)) } } }
#[macro_export]
macro_rules! log_warn  { ($($a:tt)*) => { if $crate::log::enabled(1) { $crate::log::emit("warn",  &format!($($a)*)) } } }
#[macro_export]
macro_rules! log_info  { ($($a:tt)*) => { if $crate::log::enabled(2) { $crate::log::emit("info",  &format!($($a)*)) } } }
#[macro_export]
macro_rules! log_debug { ($($a:tt)*) => { if $crate::log::enabled(3) { $crate::log::emit("debug", &format!($($a)*)) } } }
