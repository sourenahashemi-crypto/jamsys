#!/usr/bin/env python3
"""Contract tests for the battery charge-limit control.

The UI half never gains privilege: it builds an argv and hands it to pkexec. What
is worth testing here is that it refuses bad values locally instead of shipping
them to a privileged process, and that the offered choices are sane.

    python3 jamsys-ui/tests/charge-limit-test.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from jamsys_ui.hardware import (CHARGE_LIMITS, POWER_HELPER_PATHS,  # noqa: E402
                                ChargeLimitControl, power_helper_path)

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


print("offered limits")
values = [v for _, v in CHARGE_LIMITS]
check("every offered limit is in the helper's accepted range",
      all(20 <= v <= 100 for v in values), str(values))
check("100 is offered, so the limit can be turned off", 100 in values)
check("80 is offered, being the usual recommendation", 80 in values)
check("the list is ordered low to high", values == sorted(values), str(values))
check("labels say what the numbers mean",
      all(isinstance(lbl, str) and "%" in lbl for lbl, _ in CHARGE_LIMITS))

print("\nhelper discovery")
check("only absolute paths are ever searched",
      all(p.startswith("/") for p in POWER_HELPER_PATHS))
check("the helper lives outside PATH-writable locations",
      all(p.startswith(("/usr/libexec/", "/usr/bin/")) for p in POWER_HELPER_PATHS))
found = power_helper_path()
check("discovery returns a real path or None",
      found is None or pathlib.Path(found).is_file(), repr(found))

print("\nlocal validation happens before pkexec is involved")
c = ChargeLimitControl()
for bad in [0, 5, 19, 101, 255, -1, -80]:
    ok = c.set_limit(bad)
    check(f"refuses {bad}", ok is False and "20-100" in (c.last_error or ""),
          repr(c.last_error))
for bad in ["80", 80.0, None, True]:
    ok = c.set_limit(bad)
    # bool is an int subclass, so True would otherwise slip through as 1.
    check(f"refuses {bad!r} ({type(bad).__name__})", ok is False, repr(c.last_error))

print("\nwhen the helper is absent the reason is actionable")
if found is None:
    c2 = ChargeLimitControl()
    check("reports it is not available", c2.available() is False)
    ok = c2.set_limit(80)
    check("a valid value still fails cleanly", ok is False)
    check("and names the installer",
          "install-privileged" in (c2.last_error or ""), repr(c2.last_error))
else:
    print("  (helper installed; skipping the not-installed path)")

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
