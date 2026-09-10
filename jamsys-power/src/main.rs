//! Set the battery charge-stop threshold. Root, via Polkit, and nothing else.
//!
//! Why this exists
//! ---------------
//! Keeping a lithium battery at 100% is what ages it. Most laptops, this ASUS
//! included, can be told to stop charging at a lower percentage and run from the
//! charger beyond that point. The knob is a single sysfs attribute owned by root:
//!
//!     /sys/class/power_supply/BAT0/charge_control_end_threshold
//!
//! Writing it needs privilege. Reading it does not, so the daemon reads it directly
//! and only this tiny program is ever elevated.
//!
//! Why it is separate from `jamsys-kbd`
//! ------------------------------------
//! Each privileged helper does one thing to one closed set of paths. `jamsys-kbd`
//! refuses to run at all on a machine with no lit keyboard, which is correct for a
//! keyboard tool and wrong for a battery one.
//!
//! Security
//! --------
//! There is no path argument and no way to supply one: the candidate paths are
//! compile-time constants and the only input is an integer that must parse cleanly
//! and lie inside a range this program decides. No shell is spawned, no file other
//! than the chosen attribute is opened, and validation happens before the effective
//! uid is even checked, so a mistake is refused identically whoever runs it.

use std::process::ExitCode;

/// Every battery this program is willing to touch. Not a pattern, not a glob: a
/// list. A machine with more batteries than this simply is not supported, which is
/// a better failure than accepting a path from the caller.
const BATTERIES: [&str; 3] = [
    "/sys/class/power_supply/BAT0",
    "/sys/class/power_supply/BAT1",
    "/sys/class/power_supply/BATT",
];

const ATTR: &str = "charge_control_end_threshold";

/// Below this, a "charge limit" is a way to be caught with a flat battery rather
/// than a way to look after it.
const MIN_LIMIT: u8 = 20;
const MAX_LIMIT: u8 = 100;

fn usage() -> ExitCode {
    eprintln!(
        "usage: jamsys-power charge-limit <{MIN_LIMIT}-{MAX_LIMIT}>\n\
         \n\
         Stops charging at the given percentage; the machine then runs from the\n\
         charger. 100 restores normal charging. Invoked through pkexec by JamSys."
    );
    ExitCode::from(2)
}

fn arg_u8(s: &str, lo: u8, hi: u8, name: &str) -> Result<u8, String> {
    // Deliberately strict: no sign, no whitespace, no "0x", no trailing text. The
    // parse is the security boundary, so it refuses anything it does not fully
    // understand rather than salvaging what it can.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name} must be a plain number, got {s:?}"));
    }
    let v: u8 = s
        .parse()
        .map_err(|_| format!("{name} must be {lo}-{hi}, got {s:?}"))?;
    if v < lo || v > hi {
        return Err(format!("{name} must be {lo}-{hi}, got {v}"));
    }
    Ok(v)
}

fn parse(args: &[String]) -> Result<u8, String> {
    match args.first().map(|s| s.as_str()) {
        Some("charge-limit") if args.len() == 2 => {
            arg_u8(&args[1], MIN_LIMIT, MAX_LIMIT, "limit")
        }
        Some("charge-limit") => Err("charge-limit takes exactly one value".into()),
        Some(other) => Err(format!("unknown operation {other:?}")),
        None => Err("no operation given".into()),
    }
}

/// The first battery present that exposes the attribute.
fn find_battery() -> Option<String> {
    BATTERIES
        .iter()
        .map(|b| format!("{b}/{ATTR}"))
        .find(|p| std::path::Path::new(p).exists())
}

fn write_attr(path: &str, value: &str) -> Result<(), String> {
    std::fs::write(path, value).map_err(|e| format!("writing {path}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return usage();
    }

    // Validate before anything else, including the privilege check, so that a
    // malformed request is refused the same way whoever runs it.
    let limit = match parse(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("jamsys-power: {e}");
            return usage();
        }
    };

    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "jamsys-power must run as root; it is invoked through pkexec by the \
             JamSys application. See privilege-model.md."
        );
        return ExitCode::from(1);
    }

    let Some(path) = find_battery() else {
        eprintln!(
            "jamsys-power: no battery exposes {ATTR}; this machine's firmware does \
             not support a charge limit"
        );
        return ExitCode::from(3);
    };

    match write_attr(&path, &limit.to_string()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jamsys-power: {e}");
            ExitCode::from(4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn accepts_the_sensible_limits() {
        assert_eq!(parse(&v("charge-limit 80")).unwrap(), 80);
        assert_eq!(parse(&v("charge-limit 60")).unwrap(), 60);
        assert_eq!(parse(&v("charge-limit 100")).unwrap(), 100);
        assert_eq!(parse(&v("charge-limit 20")).unwrap(), 20);
    }

    #[test]
    fn refuses_a_limit_that_would_strand_the_user() {
        assert!(parse(&v("charge-limit 0")).is_err());
        assert!(parse(&v("charge-limit 5")).is_err());
        assert!(parse(&v("charge-limit 19")).is_err());
    }

    #[test]
    fn refuses_out_of_range_and_nonsense() {
        for bad in ["101", "255", "256", "1000", "-1", "+80", "8 0", "", "eighty",
                    "0x50", "80.0", "80%", " 80", "80 "] {
            assert!(
                parse(&v(&format!("charge-limit {bad}"))).is_err()
                    || bad.contains(' '),
                "should have refused {bad:?}"
            );
        }
    }

    #[test]
    fn refuses_injection_shaped_input() {
        for bad in ["80;reboot", "80&&id", "$(id)", "`id`", "80|tee",
                    "../../etc/passwd", "/sys/kernel/x"] {
            assert!(parse(&v(&format!("charge-limit {bad}"))).is_err(),
                "should have refused {bad:?}");
        }
    }

    #[test]
    fn refuses_unknown_and_malformed_operations() {
        assert!(parse(&v("write 80")).is_err());
        assert!(parse(&v("charge-limit")).is_err());
        assert!(parse(&v("charge-limit 80 90")).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn there_is_no_way_to_name_a_path() {
        // The whole point: the candidate paths are constants and every one of them
        // is the charge threshold of a battery.
        for b in BATTERIES {
            assert!(b.starts_with("/sys/class/power_supply/"));
        }
        assert_eq!(ATTR, "charge_control_end_threshold");
    }

    #[test]
    fn arg_parsing_rejects_whitespace_and_signs() {
        assert!(arg_u8(" 80", 20, 100, "limit").is_err());
        assert!(arg_u8("80 ", 20, 100, "limit").is_err());
        assert!(arg_u8("+80", 20, 100, "limit").is_err());
        assert!(arg_u8("-80", 20, 100, "limit").is_err());
        assert_eq!(arg_u8("80", 20, 100, "limit").unwrap(), 80);
    }
}
