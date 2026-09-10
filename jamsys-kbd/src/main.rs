//! jamsys-kbd — the only component that *writes* anything as root.
//!
//! # Threat model
//!
//! This runs with privilege and, unlike the read-only metric helper, it necessarily
//! takes input. Everything below exists to keep that input from being interesting to
//! an attacker.
//!
//! * **No path is ever a parameter.** The four files it can write are compile-time
//!   constants. There is no traversal because there is nothing to traverse.
//! * **No shell, ever.** It is `execve`'d directly by pkexec with an argument vector.
//!   It spawns nothing, reads no environment variable, and opens no socket or stdin.
//! * **Every argument is parsed into a typed, range-checked value** before anything is
//!   opened. An out-of-range value is a hard exit, not a clamp — silently accepting
//!   nonsense is how validation rots.
//! * **Write-only surface.** It cannot be used to read anything back, so it is not an
//!   information-disclosure primitive either.
//! * It is roughly 200 lines so that it can actually be audited by reading it.
//!
//! # Usage
//!
//! ```text
//! jamsys-kbd brightness <0-3>
//! jamsys-kbd rgb <mode> <r> <g> <b> <speed>      # mode 0-3, rgb 0-255, speed 0-2
//! jamsys-kbd state <boot> <awake> <sleep> <keyboard>   # each 0 or 1
//! ```

use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;

/// The complete, fixed set of files this program may write. Not a pattern, not a
/// prefix, not a parameter — an exhaustive constant list.
const LED_DIR: &str = "/sys/class/leds/asus::kbd_backlight";
const P_BRIGHTNESS: &str = "/sys/class/leds/asus::kbd_backlight/brightness";
const P_RGB_MODE: &str = "/sys/class/leds/asus::kbd_backlight/kbd_rgb_mode";
const P_RGB_STATE: &str = "/sys/class/leds/asus::kbd_backlight/kbd_rgb_state";

/// `kbd_rgb_mode_index` on this kernel reads: `cmd mode red green blue speed`.
/// `cmd` is always 1 for "set lighting".
const RGB_CMD: u8 = 1;
/// `kbd_rgb_state_index` reads: `cmd boot awake sleep keyboard`. `cmd` is always 1.
const STATE_CMD: u8 = 1;

const MAX_BRIGHTNESS: u8 = 3;
/// 0 static, 1 breathing, 2 colour cycle, 3 strobing. Higher values exist on some
/// firmware but are not exposed until they can be tested on hardware.
const MAX_MODE: u8 = 3;
/// 0 slow, 1 medium, 2 fast.
const MAX_SPEED: u8 = 2;

fn usage() -> ExitCode {
    eprintln!(
        "jamsys-kbd — validated ASUS keyboard lighting writer\n\n\
         USAGE:\n  \
           jamsys-kbd brightness <0-{MAX_BRIGHTNESS}>\n  \
           jamsys-kbd rgb <mode 0-{MAX_MODE}> <r 0-255> <g 0-255> <b 0-255> <speed 0-{MAX_SPEED}>\n  \
           jamsys-kbd state <boot 0|1> <awake 0|1> <sleep 0|1> <keyboard 0|1>\n\n\
         Writes only to {LED_DIR}. Takes no other input."
    );
    ExitCode::from(2)
}

/// Parse a decimal integer within an inclusive range. Rejects anything else outright:
/// no clamping, no leading `+`, no whitespace, no hex, no unary minus.
fn arg_u8(s: &str, lo: u8, hi: u8, name: &str) -> Result<u8, String> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name}: expected a plain decimal number, got {s:?}"));
    }
    let v: u32 = s.parse().map_err(|_| format!("{name}: not a number: {s:?}"))?;
    if v > hi as u32 || v < lo as u32 {
        return Err(format!("{name}: {v} is outside {lo}..={hi}"));
    }
    Ok(v as u8)
}

/// Open one of the constant paths and write a line. `O_WRONLY` only — this program
/// has no code path that reads a sysfs attribute.
/// The exact bytes handed to sysfs, as one buffer.
///
/// Separated out so the "one write, one line" rule is testable without a device.
fn attr_line(value: &str) -> String {
    let mut line = String::with_capacity(value.len() + 1);
    line.push_str(value);
    line.push('\n');
    line
}

fn write_attr(path: &'static str, value: &str) -> Result<(), String> {
    // One write() call, and only one.
    //
    // A sysfs attribute parses each write independently: it is not a stream. This
    // used to write the value and then a bare "\n" as a second call, and the
    // kernel duly handed that lone newline to its own sscanf, matched none of the
    // six fields it wanted, and returned EINVAL. The colour had already been set
    // by the first write, so the helper reported failure for something that had
    // in fact worked -- which is the worst kind of wrong.
    let line = attr_line(value);

    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    f.write_all(line.as_bytes()).map_err(|e| format!("{path}: {e}"))?;
    Ok(())
}

/// The three operations, already validated into typed values.
#[derive(Debug, PartialEq)]
enum Op {
    Brightness(u8),
    Rgb { mode: u8, r: u8, g: u8, b: u8, speed: u8 },
    State { boot: u8, awake: u8, sleep: u8, keyboard: u8 },
}

/// Pure parsing, separated from any I/O so it can be exhaustively tested.
fn parse(args: &[String]) -> Result<Op, String> {
    match args.first().map(|s| s.as_str()) {
        Some("brightness") if args.len() == 2 => {
            Ok(Op::Brightness(arg_u8(&args[1], 0, MAX_BRIGHTNESS, "brightness")?))
        }
        Some("rgb") if args.len() == 6 => Ok(Op::Rgb {
            mode: arg_u8(&args[1], 0, MAX_MODE, "mode")?,
            r: arg_u8(&args[2], 0, 255, "red")?,
            g: arg_u8(&args[3], 0, 255, "green")?,
            b: arg_u8(&args[4], 0, 255, "blue")?,
            speed: arg_u8(&args[5], 0, MAX_SPEED, "speed")?,
        }),
        Some("state") if args.len() == 5 => Ok(Op::State {
            boot: arg_u8(&args[1], 0, 1, "boot")?,
            awake: arg_u8(&args[2], 0, 1, "awake")?,
            sleep: arg_u8(&args[3], 0, 1, "sleep")?,
            keyboard: arg_u8(&args[4], 0, 1, "keyboard")?,
        }),
        Some("brightness") | Some("rgb") | Some("state") => {
            Err("wrong number of arguments".into())
        }
        Some(other) => Err(format!("unknown operation {other:?}")),
        None => Err("no operation given".into()),
    }
}

/// Render an operation as the exact bytes the kernel attribute expects.
fn render(op: &Op) -> (&'static str, String) {
    match *op {
        Op::Brightness(v) => (P_BRIGHTNESS, v.to_string()),
        Op::Rgb { mode, r, g, b, speed } => (
            P_RGB_MODE,
            format!("{RGB_CMD} {mode} {r} {g} {b} {speed}"),
        ),
        Op::State { boot, awake, sleep, keyboard } => (
            P_RGB_STATE,
            format!("{STATE_CMD} {boot} {awake} {sleep} {keyboard}"),
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return usage();
    }

    let op = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("jamsys-kbd: {e}");
            return usage();
        }
    };

    // Refuse rather than fail obscurely later. This also means an unprivileged caller
    // gets a clear message instead of a permission error from deep inside a write.
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "jamsys-kbd must run as root; it is invoked through pkexec by the \
             JamSys application. See privilege-model.md."
        );
        return ExitCode::from(1);
    }

    if !std::path::Path::new(LED_DIR).is_dir() {
        eprintln!("jamsys-kbd: {LED_DIR} not present; this is not an ASUS laptop with a lit keyboard");
        return ExitCode::from(3);
    }

    let (path, value) = render(&op);
    match write_attr(path, &value) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jamsys-kbd: {e}");
            ExitCode::from(4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn valid_brightness_parses() {
        for v in 0..=3u8 {
            assert_eq!(parse(&a(&["brightness", &v.to_string()])).unwrap(), Op::Brightness(v));
        }
    }

    #[test]
    fn brightness_above_the_hardware_maximum_is_refused() {
        assert!(parse(&a(&["brightness", "4"])).is_err());
        assert!(parse(&a(&["brightness", "255"])).is_err());
    }

    #[test]
    fn valid_rgb_parses_and_renders_in_the_kernels_field_order() {
        let op = parse(&a(&["rgb", "0", "255", "128", "0", "1"])).unwrap();
        let (path, s) = render(&op);
        assert_eq!(path, P_RGB_MODE);
        // kbd_rgb_mode_index: cmd mode red green blue speed
        assert_eq!(s, "1 0 255 128 0 1");
    }

    #[test]
    fn rgb_components_are_range_checked() {
        assert!(parse(&a(&["rgb", "0", "256", "0", "0", "0"])).is_err(), "red 256");
        assert!(parse(&a(&["rgb", "0", "0", "999", "0", "0"])).is_err(), "green 999");
        assert!(parse(&a(&["rgb", "9", "0", "0", "0", "0"])).is_err(), "mode 9");
        assert!(parse(&a(&["rgb", "0", "0", "0", "0", "5"])).is_err(), "speed 5");
        assert!(parse(&a(&["rgb", "0", "255", "255", "255", "2"])).is_ok(), "the maxima are valid");
    }

    #[test]
    fn state_renders_in_the_kernels_field_order() {
        let op = parse(&a(&["state", "1", "1", "0", "1"])).unwrap();
        let (path, s) = render(&op);
        assert_eq!(path, P_RGB_STATE);
        // kbd_rgb_state_index: cmd boot awake sleep keyboard
        assert_eq!(s, "1 1 1 0 1");
    }

    #[test]
    fn state_flags_are_boolean_only() {
        assert!(parse(&a(&["state", "2", "0", "0", "0"])).is_err());
        assert!(parse(&a(&["state", "0", "0", "0", "0"])).is_ok());
    }

    #[test]
    fn nothing_resembling_a_path_or_a_command_is_accepted() {
        for bad in [
            vec!["../../etc/shadow"],
            vec!["brightness", "/etc/passwd"],
            vec!["brightness", "3; rm -rf /"],
            vec!["brightness", "$(id)"],
            vec!["rgb", "0", "0", "0", "0", "0", "extra"],
            vec!["write", "/sys/class/leds/x/brightness", "1"],
            vec!["brightness"],
            vec![""],
        ] {
            assert!(parse(&a(&bad)).is_err(), "should have refused {bad:?}");
        }
    }

    #[test]
    fn numeric_parsing_is_strict() {
        // No sign, no whitespace, no hex, no unicode digits, no overlong input.
        for bad in ["+1", " 1", "1 ", "0x2", "١", "01234", "-1", "1.0", "3\n"] {
            assert!(arg_u8(bad, 0, 3, "x").is_err(), "should have refused {bad:?}");
        }
        assert_eq!(arg_u8("3", 0, 3, "x").unwrap(), 3);
        assert_eq!(arg_u8("0", 0, 3, "x").unwrap(), 0);
    }

    #[test]
    fn the_writable_paths_are_a_closed_set() {
        // Every op must render to one of exactly three constant paths.
        let ops = [
            Op::Brightness(1),
            Op::Rgb { mode: 0, r: 1, g: 2, b: 3, speed: 0 },
            Op::State { boot: 1, awake: 1, sleep: 0, keyboard: 1 },
        ];
        for op in &ops {
            let (p, _) = render(op);
            assert!(
                p == P_BRIGHTNESS || p == P_RGB_MODE || p == P_RGB_STATE,
                "unexpected write target {p}"
            );
            assert!(p.starts_with(LED_DIR), "write target escaped the LED directory");
            assert!(!p.contains(".."), "path contains a traversal component");
        }
    }

    #[test]
    fn an_attribute_write_is_one_line_and_one_newline() {
        // Regression. This was written as two write() calls -- the value, then a
        // bare "\n" -- and because a sysfs attribute parses each write on its own,
        // the kernel got a lone newline, matched none of its six fields and
        // returned EINVAL. The colour had already been applied by the first write,
        // so the helper reported failure for an operation that had succeeded.
        let line = attr_line("1 0 255 255 255 1");
        assert_eq!(line, "1 0 255 255 255 1\n");
        assert_eq!(line.matches('\n').count(), 1, "exactly one newline");
        assert!(line.ends_with('\n'), "and it is at the end");
        assert!(!line.trim_end().contains('\n'), "nothing splits the value");
    }

    #[test]
    fn every_operation_renders_to_a_single_writable_line() {
        for op in [
            Op::Brightness(2),
            Op::Rgb { mode: 1, r: 10, g: 20, b: 30, speed: 2 },
            Op::State { boot: 1, awake: 1, sleep: 0, keyboard: 1 },
        ] {
            let (_, v) = render(&op);
            let line = attr_line(&v);
            assert_eq!(line.matches('\n').count(), 1, "{op:?} produced {line:?}");
        }
    }

    #[test]
    fn rendered_values_contain_only_digits_and_spaces() {
        // Nothing that could be interpreted as anything but a value reaches sysfs.
        let (_, s) = render(&Op::Rgb { mode: 3, r: 255, g: 0, b: 127, speed: 2 });
        assert!(s.bytes().all(|b| b.is_ascii_digit() || b == b' '), "got {s:?}");
    }
}
