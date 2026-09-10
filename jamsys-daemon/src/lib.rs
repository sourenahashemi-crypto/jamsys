//! JamSys daemon library.
//!
//! Everything lives here rather than in `main.rs` so that the threshold engine, the
//! baseline maths, the parsers and the retention logic can be exercised by integration
//! tests without starting a daemon.

pub mod anomaly;
pub mod clock;
pub mod collectors;
pub mod config;
pub mod dbus;
pub mod dbusservice;
pub mod eventloop;
pub mod ipc;
pub mod journal;
pub mod log;
pub mod netlink;
pub mod nvml;
pub mod store;
pub mod sysfs;
pub mod types;
pub mod util;
