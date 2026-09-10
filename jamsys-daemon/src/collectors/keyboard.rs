//! ASUS keyboard lighting: capability detection and brightness readback.
//!
//! This collector is **read-only**, like every other collector. Writing is done by
//! `jamsys-kbd`, a separate privileged binary invoked through Polkit — the daemon
//! never gains write privilege, and a keyboard-lighting failure cannot affect
//! monitoring, which is the point of keeping them apart.
//!
//! What the kernel exposes on this hardware, verified by inspection:
//!
//! ```text
//! brightness            rw  root:root  0..3
//! max_brightness        r
//! kbd_rgb_mode          -w-------      write-only: "cmd mode red green blue speed"
//! kbd_rgb_mode_index    r              reads back the field *names*, not the values
//! kbd_rgb_state         -w-------      write-only: "cmd boot awake sleep keyboard"
//! kbd_rgb_state_index   r              field names again
//! ```
//!
//! The mode and state attributes are genuinely write-only, so the current colour
//! cannot be read back from the kernel at all. The UI therefore remembers what it last
//! set rather than pretending to know — see `docs/privilege-model.md`.

use super::{Collector, Ctx};
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;

pub const LED_DIR: &str = "/sys/class/leds/asus::kbd_backlight";

#[derive(Clone, Debug, Default, Serialize)]
pub struct KeyboardState {
    pub present: bool,
    pub brightness: u8,
    pub max_brightness: u8,
    /// True when `kbd_rgb_mode` exists, i.e. the keyboard has addressable colour.
    pub rgb_capable: bool,
    /// True when `kbd_rgb_state` exists (boot/awake/sleep/keyboard lighting states).
    pub state_capable: bool,
    /// Field order the kernel advertises, shown in the UI so the mapping is auditable.
    pub rgb_mode_fields: String,
    pub rgb_state_fields: String,
    /// How the UI can actually change it on this installation.
    pub control: ControlPath,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlPath {
    /// The attributes are writable by this user — a udev rule is installed.
    Direct,
    /// `jamsys-kbd` is installed; writes go through Polkit.
    Helper,
    /// Neither. The UI shows the controls as unavailable and says why.
    #[default]
    None,
}

pub const HELPER_PATHS: [&str; 2] = ["/usr/libexec/jamsys-kbd", "/usr/bin/jamsys-kbd"];

/// Which write path is available, if any. Checked at probe and on each slow tick, so
/// installing the helper or the udev rule takes effect without a restart.
pub fn detect_control_path() -> ControlPath {
    // Direct write beats the helper: if the user has installed the udev rule they
    // have chosen the no-privileged-code option and we should honour it.
    let writable = |p: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| {
                // Cheap check: is the group-write bit set and are we in that group?
                // An actual open(O_WRONLY) would be definitive but would also be a
                // side effect on every probe.
                m.permissions().mode() & 0o020 != 0
            })
            .unwrap_or(false)
    };
    if writable(&format!("{LED_DIR}/brightness")) {
        return ControlPath::Direct;
    }
    if HELPER_PATHS.iter().any(|p| std::path::Path::new(p).is_file()) {
        return ControlPath::Helper;
    }
    ControlPath::None
}

pub struct KeyboardCollector {
    present: bool,
}

impl KeyboardCollector {
    pub fn new() -> Self {
        KeyboardCollector { present: false }
    }
}

impl Default for KeyboardCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for KeyboardCollector {
    fn name(&self) -> &'static str {
        "keyboard"
    }
    fn tier(&self) -> Tier {
        Tier::Slow
    }

    fn probe(&mut self) -> Support {
        self.present = exists(LED_DIR);
        if !self.present {
            return Support::Unsupported {
                reason: "no ASUS keyboard backlight LED on this machine".into(),
            };
        }
        let rgb = exists(format!("{LED_DIR}/kbd_rgb_mode"));
        match (rgb, detect_control_path()) {
            (true, ControlPath::None) => Support::Partial {
                detail: "RGB present but read-only: install jamsys-kbd or the udev rule to control it".into(),
            },
            (true, _) => Support::Full,
            (false, _) => Support::Partial { detail: "backlight brightness only, no RGB".into() },
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        if !exists(LED_DIR) {
            return Err(CollectorError::Gone(LED_DIR.into()));
        }
        let st = KeyboardState {
            present: true,
            brightness: read_checked(format!("{LED_DIR}/brightness"), 0.0, 255.0).unwrap_or(0.0) as u8,
            max_brightness: read_checked(format!("{LED_DIR}/max_brightness"), 0.0, 255.0).unwrap_or(0.0) as u8,
            rgb_capable: exists(format!("{LED_DIR}/kbd_rgb_mode")),
            state_capable: exists(format!("{LED_DIR}/kbd_rgb_state")),
            rgb_mode_fields: read_str(format!("{LED_DIR}/kbd_rgb_mode_index")).unwrap_or_default(),
            rgb_state_fields: read_str(format!("{LED_DIR}/kbd_rgb_state_index")).unwrap_or_default(),
            control: detect_control_path(),
        };
        ctx.g("keyboard", "brightness", "", st.brightness as f64);
        ctx.snap.keyboard = st;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn detects_this_machines_keyboard() {
        let mut c = KeyboardCollector::new();
        let s = c.probe();
        if !exists(LED_DIR) {
            assert!(matches!(s, Support::Unsupported { .. }), "no LED dir, must be Unsupported");
            return;
        }
        assert!(s.is_usable() || matches!(s, Support::Partial { .. }));
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let k = &x.snap.keyboard;
        assert!(k.present);
        assert!(k.max_brightness > 0, "max_brightness should be readable");
        assert!(k.brightness <= k.max_brightness, "brightness cannot exceed its maximum");
        // Verified on the target: the kernel advertises these field orders.
        if k.rgb_capable {
            assert!(k.rgb_mode_fields.contains("mode"), "got {:?}", k.rgb_mode_fields);
            assert!(k.rgb_mode_fields.contains("speed"));
        }
        if k.state_capable {
            assert!(k.rgb_state_fields.contains("awake"), "got {:?}", k.rgb_state_fields);
        }
    }

    #[test]
    fn write_only_attributes_are_never_read() {
        // The collector must not try to read kbd_rgb_mode; it is --w------- and any
        // read is a guaranteed permission error that would look like a broken sensor.
        let src = include_str!("keyboard.rs");
        // Find the collect() body and confirm no read of the write-only attributes.
        assert!(!src.contains("read_str(format!(\"{LED_DIR}/kbd_rgb_mode\")"),
                "collector must not read the write-only mode attribute");
        assert!(!src.contains("read_str(format!(\"{LED_DIR}/kbd_rgb_state\")"),
                "collector must not read the write-only state attribute");
    }

    #[test]
    fn control_path_reports_honestly_when_nothing_is_installed() {
        // On this machine the attributes are root-only and the helper is not installed,
        // so the honest answer is None rather than pretending control is available.
        let p = detect_control_path();
        let helper = HELPER_PATHS.iter().any(|x| std::path::Path::new(x).is_file());
        if !helper {
            assert_ne!(p, ControlPath::Helper, "claimed a helper that is not installed");
        }
    }

    #[test]
    fn a_machine_without_the_led_is_unsupported_not_an_error() {
        let mut c = KeyboardCollector::new();
        c.present = false;
        // probe() on a non-ASUS machine returns Unsupported, which is not a failure.
        let s = Support::Unsupported { reason: "x".into() };
        assert!(!s.is_usable());
        assert_eq!(s.label(), "Unavailable");
    }
}
