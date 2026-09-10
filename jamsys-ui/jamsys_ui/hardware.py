"""Hardware controls — currently the ASUS keyboard backlight and RGB.

Two rules shape this file:

* **The UI never gains privilege.** Writes go to `jamsys-kbd` through `pkexec`. The
  monitoring daemon is not involved at all, so a lighting failure cannot disturb
  monitoring — which is the reason they are separate processes.
* **`kbd_rgb_mode` and `kbd_rgb_state` are write-only in the kernel.** The current
  colour genuinely cannot be read back. This page therefore remembers what it last
  set and says so, rather than displaying a colour it is only guessing at.
"""

from __future__ import annotations

import shutil
import subprocess
from typing import Optional

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, GLib, Gtk  # noqa: E402

HELPER_PATHS = ["/usr/libexec/jamsys-kbd", "/usr/bin/jamsys-kbd"]

# Order matches jamsys-kbd's own validation, which matches the kernel's
# kbd_rgb_mode_index: "cmd mode red green blue speed".
MODES = [("Static", 0), ("Breathing", 1), ("Colour cycle", 2), ("Strobing", 3)]
SPEEDS = [("Slow", 0), ("Medium", 1), ("Fast", 2)]
PRESETS = [
    ("White", (255, 255, 255)),
    ("Red", (255, 0, 0)),
    ("Blue", (0, 0, 255)),
    ("Purple", (160, 0, 255)),
    ("Green", (0, 255, 0)),
]


def helper_path() -> Optional[str]:
    for p in HELPER_PATHS:
        if shutil.which(p) or _is_file(p):
            return p
    return None


def _is_file(p: str) -> bool:
    import os
    return os.path.isfile(p)


class KeyboardControl:
    """Invokes the privileged helper. Nothing here builds a shell command."""

    def __init__(self):
        self.last_error: Optional[str] = None

    def available(self, control_path: str) -> bool:
        return control_path in ("helper", "direct")

    def _run(self, args: list[str], control_path: str) -> bool:
        """Run one validated operation. `args` are already plain integers as strings."""
        self.last_error = None
        if control_path == "direct":
            # A udev rule has made the attributes group-writable, so no privileged
            # code is involved at all. Write them ourselves.
            return self._write_direct(args)
        h = helper_path()
        if not h:
            self.last_error = "jamsys-kbd is not installed"
            return False
        # argv, never a shell string. pkexec execve()s the helper directly.
        argv = ["pkexec", h, *args]
        try:
            r = subprocess.run(argv, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired) as e:
            self.last_error = str(e)
            return False
        if r.returncode != 0:
            # 126/127 are pkexec's "dismissed" and "not authorised".
            if r.returncode in (126, 127):
                self.last_error = "Authorisation was declined"
            else:
                self.last_error = (r.stderr or "").strip() or f"exit {r.returncode}"
            return False
        return True

    def _write_direct(self, args: list[str]) -> bool:
        base = "/sys/class/leds/asus::kbd_backlight"
        op = args[0]
        try:
            if op == "brightness":
                target, value = f"{base}/brightness", args[1]
            elif op == "rgb":
                target, value = f"{base}/kbd_rgb_mode", "1 " + " ".join(args[1:])
            elif op == "state":
                target, value = f"{base}/kbd_rgb_state", "1 " + " ".join(args[1:])
            else:
                self.last_error = f"unknown operation {op}"
                return False
            with open(target, "w") as f:
                f.write(value + "\n")
            return True
        except OSError as e:
            self.last_error = str(e)
            return False

    def set_brightness(self, v: int, control_path: str) -> bool:
        return self._run(["brightness", str(int(v))], control_path)

    def set_rgb(self, mode: int, r: int, g: int, b: int, speed: int, control_path: str) -> bool:
        return self._run(
            ["rgb", str(int(mode)), str(int(r)), str(int(g)), str(int(b)), str(int(speed))],
            control_path,
        )

    def set_states(self, boot: bool, awake: bool, sleep: bool, keyboard: bool,
                   control_path: str) -> bool:
        return self._run(
            ["state", *(("1" if x else "0") for x in (boot, awake, sleep, keyboard))],
            control_path,
        )


def rgba_to_ints(c: Gdk.RGBA) -> tuple[int, int, int]:
    return (round(c.red * 255), round(c.green * 255), round(c.blue * 255))


def ints_to_rgba(r: int, g: int, b: int) -> Gdk.RGBA:
    c = Gdk.RGBA()
    c.red, c.green, c.blue, c.alpha = r / 255, g / 255, b / 255, 1.0
    return c
