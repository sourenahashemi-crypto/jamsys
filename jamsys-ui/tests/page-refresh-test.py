#!/usr/bin/env python3
"""The refresh loop must not fight the person using the window.

Every page rebuilds its whole body on each two-second refresh. That is fine
for text and hostile for anything interactive: an open dropdown is destroyed
under the pointer and the scroll position snaps to the top, so the page jumps
and the list cannot be scrolled or picked from.

These drive the real Page.update() against real GTK widgets.

    python3 jamsys-ui/tests/page-refresh-test.py
"""

import os
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import gi  # noqa: E402
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gtk  # noqa: E402

if not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("DISPLAY")):
    print("no display; skipping (this suite needs a real GTK stack)")
    raise SystemExit(0)

Adw.init()

from jamsys_ui.app import Page, _has_open_popover  # noqa: E402

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


class Counting(Page):
    """A page that records how often it was actually rebuilt."""

    title, icon = "Counting", "x"

    def __init__(self, win):
        super().__init__(win)
        self.renders = 0

    def render(self, snap):
        self.renders += 1
        self.clear()
        for i in range(40):
            self.body.append(Gtk.Label(label=f"row {i}"))


win = Gtk.Window()
page = Counting(None)
win.set_child(page)
win.set_default_size(400, 300)
win.present()

# Let GTK allocate, so the scroller has a real upper bound.
ctx = win.get_display().get_default_seat() and None
for _ in range(80):
    Gtk.main_iteration_do(False) if hasattr(Gtk, "main_iteration_do") else None
    from gi.repository import GLib
    GLib.MainContext.default().iteration(False)

print("ordinary refresh")
page.update({})
check("a refresh with nothing open re-renders", page.renders >= 1,
      f"renders={page.renders}")

print("\nan open popover suspends the rebuild")
pop = Gtk.Popover()
btn = Gtk.MenuButton(popover=pop)
page.body.append(btn)
pop.popup()
for _ in range(40):
    from gi.repository import GLib
    GLib.MainContext.default().iteration(False)

check("the popover is detected as open", _has_open_popover(page))
check("the page reports it is being interacted with", page.is_interacting())
before = page.renders
page.update({})
check("update() does not rebuild while a popup is open",
      page.renders == before, f"renders went {before} -> {page.renders}")

pop.popdown()
for _ in range(40):
    from gi.repository import GLib
    GLib.MainContext.default().iteration(False)
check("the popover is no longer detected once closed", not _has_open_popover(page))
before = page.renders
page.update({})
check("and the rebuild resumes after it closes", page.renders == before + 1)

print("\nscroll position survives a rebuild")
adj = page._scroller.get_vadjustment()
from gi.repository import GLib
for _ in range(60):
    GLib.MainContext.default().iteration(False)
# Mid-scroll, not the very bottom: at the bottom the legitimate clamp to
# (upper - page_size) is indistinguishable from a failure to restore, and the
# clamp is the behaviour we want when content shrinks.
target = max(1.0, (adj.get_upper() - adj.get_page_size()) / 2.0)
adj.set_value(target)
moved = adj.get_value()
page.update({})
for _ in range(80):
    GLib.MainContext.default().iteration(False)
check("the scroll offset was actually set for the test", moved > 0.0, f"value={moved}")
if moved > 0.0:
    check("scroll position is restored after a refresh",
          abs(adj.get_value() - moved) < 2.0,
          f"was {moved:.1f}, now {adj.get_value():.1f}")

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
