#!/usr/bin/env python3
"""Text from the machine must survive being rendered.

AdwActionRow titles and subtitles are parsed as Pango markup. Everything the daemon
reports -- a process name from /proc/pid/stat, a systemd unit's free-text
Description=, a mount path -- is therefore markup unless it is escaped, and an
unescaped ampersand does not render as an ampersand: the title comes out *empty*.
A monitor that silently drops the row for a process called "a&b" is failing at its
one job, and a process that can style its own row can hide in the list.

    python3 jamsys-ui/tests/escaping-test.py
"""

import os
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import gi  # noqa: E402
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, GLib, Gtk  # noqa: E402

if not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("DISPLAY")):
    print("no display; skipping (this suite needs a real GTK stack)")
    raise SystemExit(0)

Adw.init()

from jamsys_ui.format import human_bytes, pct, temp, watts  # noqa: E402

passed = failed = 0

HOSTILE = ["a&b", "<b>bold</b>", '<span size="0">hidden</span>', "a<b", "100% & rising",
           "naïve—dash", "‮RTL", "'quoted'", 'say "what"']


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


def rendered(row):
    """The text GTK actually put on screen for a row."""
    out = []

    def walk(w):
        c = w.get_first_child()
        while c:
            if isinstance(c, Gtk.Label):
                out.append(c.get_text())
            walk(c)
            c = c.get_next_sibling()
    walk(row)
    return out


print("escaping is what makes a row render at all")
for s in HOSTILE:
    row = Adw.ActionRow(title=GLib.markup_escape_text(s))
    check(f"{s!r} survives escaping", s in rendered(row), str(rendered(row)))

print("\nand without it the text is lost or interpreted")
lost = Adw.ActionRow(title="a&b")
check("an unescaped ampersand renders as nothing (the bug this guards)",
      "a&b" not in rendered(lost), str(rendered(lost)))
styled = Adw.ActionRow(title="<b>bold</b>")
check("an unescaped tag is interpreted, not shown",
      "<b>bold</b>" not in rendered(styled), str(rendered(styled)))

print("\nevery row fed from daemon data escapes both fields")
src = (pathlib.Path(__file__).resolve().parents[1] / "jamsys_ui" / "app.py").read_text()
# Values that come from the daemon rather than from a literal in this file.
DAEMON_FIELDS = re.compile(
    r"Adw\.(?:Action|Expander)Row\(\s*title=(?!GLib\.markup_escape_text)"
    r"(?!\"|')(?P<expr>[^,)]+)")
offenders = []
for m in DAEMON_FIELDS.finditer(src):
    expr = m.group("expr").strip()
    if expr.startswith("f\"") or expr.startswith("f'") or "[" in expr or "get(" in expr:
        line = src[:m.start()].count("\n") + 1
        offenders.append(f"app.py:{line}: {expr[:50]}")
check("no unescaped daemon value reaches a row title", not offenders,
      "; ".join(offenders))

print("\nformatters answer for values they were not promised")
for bad in [None, "n/a", float("nan"), float("inf"), True, [], {}]:
    ok = all(isinstance(f(bad), str) for f in (temp, pct, watts, human_bytes))
    check(f"{bad!r} formats without raising", ok)
check("and a real number still formats", temp(55) == "55 °C" and pct(42) == "42%"
      and watts(11.24) == "11.2 W" and human_bytes(1536) == "1.5 kB")
check("nonsense reads as unknown, not as zero", temp(float("nan")) == "—"
      and pct("n/a") == "—", f"{temp(float('nan'))} {pct('n/a')}")

print("\na dropdown with no selection does not index off the end")
from jamsys_ui.app import _selected  # noqa: E402
from jamsys_ui.hardware import MODES  # noqa: E402


class FakeDropDown:
    def __init__(self, i):
        self._i = i

    def get_selected(self):
        return self._i


check("a real selection is read", _selected(MODES, FakeDropDown(2), 0) == MODES[2][1])
check("GTK_INVALID_LIST_POSITION falls back",
      _selected(MODES, FakeDropDown(Gtk.INVALID_LIST_POSITION), 3) == 3)
check("and so does an out-of-range index", _selected(MODES, FakeDropDown(99), 1) == 1)

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
