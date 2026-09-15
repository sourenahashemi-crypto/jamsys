#!/usr/bin/env python3
"""The per-core matrix: layout, ramp, and the promise that colour is never alone.

The matrix answers a shape question — 'is one core pinned while the rest idle?' —
so the things that can break it are geometric: a column count that drops a core,
a ramp that inverts between light and dark, a cell whose text nobody can read
against its own fill. Those are what is checked here, against real GTK widgets.

    python3 jamsys-ui/tests/core-matrix-test.py
"""

import math
import os
import pathlib
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

from jamsys_ui.widgets import (SEQ_BLUE, CoreMatrix, _INK_FLIP, _INK_ON_DARK,  # noqa: E402
                               _INK_ON_LIGHT, _rgba, core_usage_legend, ramp_step)

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


def matrix(values):
    m = CoreMatrix()
    m.set_values(values)
    return m


print("the ramp reads the same way in both themes")
last = len(SEQ_BLUE) - 1
check("idle takes the palest step on a light surface", ramp_step(0, False) == 0)
check("busy takes the darkest", ramp_step(100, False) == last)
check("the ramp inverts on a dark surface", ramp_step(0, True) == last)
check("so busy is still the step furthest from the surface", ramp_step(100, True) == 0)
check("it rises without a step backwards",
      all(ramp_step(v, False) <= ramp_step(v + 1, False) for v in range(100)))
check("out-of-range values clamp rather than index off the end",
      ramp_step(-40, False) == 0 and ramp_step(180, False) == last)
check("a missing reading is not an exception", ramp_step(float("nan"), False) == 0)

print("\nevery cell's own text is readable on its own fill")


def contrast(a, b):
    def lum(c):
        def ch(v):
            return v / 12.92 if v <= 0.03928 else ((v + 0.055) / 1.055) ** 2.4
        return 0.2126 * ch(c.red) + 0.7152 * ch(c.green) + 0.0722 * ch(c.blue)
    la, lb = sorted((lum(a), lum(b)))
    return (lb + 0.05) / (la + 0.05)


worst = min(
    contrast(_rgba(step), _rgba(_INK_ON_DARK if i >= _INK_FLIP else _INK_ON_LIGHT))
    for i, step in enumerate(SEQ_BLUE))
check("the ink flips before the fill gets too dark for it", worst >= 4.4, f"worst {worst:.2f}:1")

print("\nthe grid keeps every core, at every width")
for n in (1, 2, 4, 6, 7, 8, 12, 16, 24, 32, 64, 128):
    m = matrix([50.0] * n)
    for width in (180.0, 340.0, 700.0, 904.0, 1600.0):
        cells = m._cells(width)
        if len(cells) != n:
            check(f"{n} cores at {width:.0f}px", False, f"drew {len(cells)}")
            break
        cols = m._columns(width)
        rows = -(-n // cols)
        overlap = any(
            cells[i][0] < cells[i - 1][0] + cells[i - 1][2] - 0.01
            for i in range(1, n) if i % cols)
        ragged = n % cols and cols in [c for c in range(1, cols + 1) if n % c == 0]
        if overlap or ragged or cols > m.MAX_COLS or rows * cols < n:
            check(f"{n} cores at {width:.0f}px", False,
                  f"cols={cols} overlap={overlap} ragged={ragged}")
            break
    else:
        check(f"{n} cores lay out at every width", True)

print("\nthe shape is a grid, not a stretched row")
m = matrix([10.0] * 24)
check("a wide window adds columns, not width",
      all(c[2] <= m.CELL_W_MAX + 0.01 for c in m._cells(2400.0)))
check("and centres what it does not fill", m._cells(2400.0)[0][0] > 0)
check("a full row divides the core count evenly", 24 % m._columns(904.0) == 0,
      f"cols={m._columns(904.0)}")
check("a narrow window gets taller, not clipped",
      m.do_measure(Gtk.Orientation.VERTICAL, 260)[0]
      > m.do_measure(Gtk.Orientation.VERTICAL, 904)[0])
check("the minimum width is still a grid",
      m.do_measure(Gtk.Orientation.HORIZONTAL, -1)[0]
      >= m.MIN_COLS * m.CELL_W)
# A one-column minimum would make GTK demand the height of a 24-row stack as the
# window's minimum height, which is how this went wrong the first time.
check("so the minimum height stays a window-sized number",
      m.do_measure(Gtk.Orientation.VERTICAL,
                   m.do_measure(Gtk.Orientation.HORIZONTAL, -1)[0])[0] < 500)

print("\nno cores, or absurd ones, are survivable")
empty = matrix([])
check("an empty matrix draws nothing and asks for nothing",
      empty._cells(400.0) == []
      and empty.do_measure(Gtk.Orientation.VERTICAL, 400) == (0, 0, -1, -1))
odd = matrix([float("nan"), float("inf"), -5.0, 1e9])
check("junk readings become numbers", all(math.isfinite(v) for v in odd._values))
check("and still get four cells", len(odd._cells(400.0)) == 4)


class Tip:
    def __init__(self):
        self.text = None

    def set_text(self, t):
        self.text = t

    def set_tip_area(self, r):
        self.area = r


print("\nthe number is reachable without the colour")
# Hit-testing reads the real allocation, so the widget has to be in a window that
# has actually been laid out — a bare widget is zero pixels wide and hits nothing.
m = matrix([3.0, 50.0, 97.0] * 8)
win = Gtk.Window()
win.set_child(m)
win.set_default_size(904, 420)
win.present()
for _ in range(80):
    GLib.MainContext.default().iteration(False)
check("the matrix was allocated for the test", m.get_width() > 0, f"width={m.get_width()}")
cx, cy, cw, ch = m._cells(float(m.get_width()))[7]
tip = Tip()
hit = m._on_tooltip(m, cx + cw / 2, cy + ch / 2, False, tip)
check("hovering a cell names that core",
      hit and (tip.text or "").startswith("cpu7 "), str(tip.text))
check("and quotes its reading", "50" in (tip.text or ""), str(tip.text))
check("outside the grid there is no tooltip", not m._on_tooltip(m, 9e5, 9e5, False, Tip()))
check("a screen reader gets the summary the shape gives",
      "24 logical CPUs" in m.summary() and "busiest cpu2 " in m.summary(), m.summary())
check("even with nothing to report", "no cores" in matrix([]).summary())
win.close()

print("\nthe legend names both ends of the scale")
legend = core_usage_legend()
texts = []
child = legend.get_first_child()
while child is not None:
    if isinstance(child, Gtk.Label):
        texts.append(child.get_text())
    child = child.get_next_sibling()
check("0% and 100% are both spelled out",
      any("0%" in t for t in texts) and any("100%" in t for t in texts), str(texts))

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
