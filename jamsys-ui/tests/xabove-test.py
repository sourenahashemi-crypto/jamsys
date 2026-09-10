#!/usr/bin/env python3
"""Contract tests for jamsys-xabove.

The helper is unprivileged, but it is still the one component that takes a value
from the UI and hands it to a system API, so it gets the same treatment as the
keyboard helper: everything is validated before anything is opened, and only a
fixed vocabulary is accepted.

    python3 jamsys-ui/tests/xabove-test.py
"""

import importlib.util
import pathlib
import subprocess
import sys
from importlib.machinery import SourceFileLoader

HELPER = pathlib.Path(__file__).resolve().parents[1] / "bin" / "jamsys-xabove"

# The helper is an installed program with no .py suffix, so the loader has to be
# named explicitly; importlib cannot infer one from the extension.
spec = importlib.util.spec_from_loader(
    "jamsys_xabove", SourceFileLoader("jamsys_xabove", str(HELPER)))
xa = importlib.util.module_from_spec(spec)
spec.loader.exec_module(xa)

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


def rejects(name, argv):
    """The helper must exit 2 (usage) and must not touch the display."""
    r = subprocess.run([sys.executable, str(HELPER), *argv],
                       capture_output=True, text=True, timeout=10)
    check(name, r.returncode == 2 and "Traceback" not in r.stderr,
          f"rc={r.returncode} err={r.stderr.strip()[:80]!r}")


print("jamsys-xabove — argument parsing")
check("hex xid", xa.parse_xid("0x2200004") == 0x2200004)
check("decimal xid", xa.parse_xid("35651588") == 35651588)
check("surrounding space tolerated", xa.parse_xid("  0x1f  ") == 0x1f)
for bad in ["", "0", "-1", "0x0", "0x100000000", "/etc/passwd", "1;id", "1 2",
            "0xdeadbeefcafe", "nan", "1e5"]:
    try:
        xa.parse_xid(bad)
        check(f"rejects xid {bad!r}", False, "accepted")
    except ValueError:
        check(f"rejects xid {bad!r}", True)

check("state on", xa.parse_state("on") is True)
check("state off", xa.parse_state("off") is False)
for bad in ["ON", "true", "1", "yes", "", "off ", "$(id)"]:
    try:
        xa.parse_state(bad)
        check(f"rejects state {bad!r}", False, "accepted")
    except ValueError:
        check(f"rejects state {bad!r}", True)

print("jamsys-xabove — the allowed vocabulary is closed")
check("exactly two states are settable", set(xa.ATOMS) == {"above", "sticky"})
check("above maps to the EWMH atom", xa.ATOMS["above"] == b"_NET_WM_STATE_ABOVE")
check("sticky maps to the EWMH atom", xa.ATOMS["sticky"] == b"_NET_WM_STATE_STICKY")

print("jamsys-xabove — refusals at the command line")
rejects("no arguments", [])
rejects("verb only", ["above"])
rejects("unknown verb", ["fullscreen", "0x1", "on"])
rejects("unknown verb (shell-ish)", ["above;id", "0x1", "on"])
rejects("path as window id", ["above", "/etc/shadow", "on"])
rejects("command substitution as state", ["above", "0x1", "$(id)"])
rejects("too many arguments", ["above", "0x1", "on", "extra"])
rejects("window id out of range", ["above", "0x100000000", "on"])

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
