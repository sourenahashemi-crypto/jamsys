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
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import gi  # noqa: E402
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, GLib, Gtk  # noqa: E402

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


def pump(until, seconds=5.0):
    """Run the main loop until `until()` holds, or give up after `seconds`.

    Not a fixed number of iterations. GLib's non-blocking iteration returns at
    once when the queue is empty, so `for _ in range(80): iteration(False)` only
    drains what has already arrived -- measured at 11-17 ms, all of it spent
    spinning on an empty queue. It does not wait for anything.

    That is what made this suite fail about one run in twelve, here on an idle
    machine: with the forty rows built and the page allocated 400x264, the
    scroller's adjustment had not yet been re-measured for the new content, so
    upper still equalled page_size. An empty range clamps the test's own
    set_value(1.0) back to zero, and the suite then reported a scroll offset of
    0.0 as a product failure when nothing was wrong with the product.

    Blocking for the next event is what actually waits; the throwaway timeout
    guarantees the block ends, so a condition that never comes true costs the
    deadline rather than hanging the suite.
    """
    ctx = GLib.MainContext.default()
    deadline = time.monotonic() + seconds
    while True:
        # Bounded: a page stuck in a render loop keeps the queue permanently
        # non-empty, and an unbounded drain would spin here forever instead of
        # timing out and reporting it.
        drained = 0
        while ctx.pending() and drained < 200:
            ctx.iteration(False)
            drained += 1
        if until():
            return True
        if time.monotonic() >= deadline:
            return False
        GLib.timeout_add(10, lambda: GLib.SOURCE_REMOVE)
        ctx.iteration(True)


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

# Wait for a real allocation: everything below reads geometry the compositor
# has not produced yet at this point.
allocated = pump(lambda: page.get_width() > 0 and page.get_height() > 0)

print("ordinary refresh")
check("the window was allocated for the test", allocated,
      f"{page.get_width()}x{page.get_height()} after the deadline")
page.update({})
check("a refresh with nothing open re-renders", page.renders >= 1,
      f"renders={page.renders}")

print("\nan open popover suspends the rebuild")
pop = Gtk.Popover()
btn = Gtk.MenuButton(popover=pop)
page.body.append(btn)
pop.popup()
pump(lambda: _has_open_popover(page))

check("the popover is detected as open", _has_open_popover(page))
check("the page reports it is being interacted with", page.is_interacting())
before = page.renders
page.update({})
check("update() does not rebuild while a popup is open",
      page.renders == before, f"renders went {before} -> {page.renders}")

pop.popdown()
pump(lambda: not _has_open_popover(page))
check("the popover is no longer detected once closed", not _has_open_popover(page))
before = page.renders
page.update({})
check("and the rebuild resumes after it closes", page.renders == before + 1)

print("\nscroll position survives a rebuild")
adj = page._scroller.get_vadjustment()
# A scroller with no range cannot hold a position, so there is nothing to test
# until the content is taller than the viewport.
scrollable = pump(lambda: adj.get_upper() > adj.get_page_size() > 0)
# Mid-scroll, not the very bottom: at the bottom the legitimate clamp to
# (upper - page_size) is indistinguishable from a failure to restore, and the
# clamp is the behaviour we want when content shrinks.
target = max(1.0, (adj.get_upper() - adj.get_page_size()) / 2.0)
adj.set_value(target)
moved = adj.get_value()
page.update({})
# The restore is queued at idle/low priority, so it lands some way after
# update() returns; wait for it rather than assuming it already happened.
pump(lambda: abs(adj.get_value() - moved) < 2.0)
check("the scroll offset was actually set for the test", scrollable and moved > 0.0,
      f"value={moved} upper={adj.get_upper():.0f} page={adj.get_page_size():.0f}")
if moved > 0.0:
    check("scroll position is restored after a refresh",
          abs(adj.get_value() - moved) < 2.0,
          f"was {moved:.1f}, now {adj.get_value():.1f}")

print("\nthe Report page fetches on a schedule, not in a loop")
# render() calls _fetch(); _fetch's completion calls render(). _pending is
# already cleared by then, so before the age check this recursed as fast as the
# daemon could answer -- 1101 renders in five seconds, a core held busy for as
# long as the page was open, and a page GTK could never finish allocating.
from jamsys_ui.app import REFRESH_MS, ReportPage  # noqa: E402


class StubClient:
    connected = True

    def __init__(self):
        self.calls = 0

    def call(self, op, **kw):
        self.calls += 1
        return {"alerts": [], "collectors": [], "events": [], "items": []}


class StubWin:
    def __init__(self):
        self.client = StubClient()
        self.last_snapshot = {"snapshot": {}}
        self.page = None

    def current_page(self):
        return self.page


stub = StubWin()
report_page = ReportPage(stub)
stub.page = report_page
report_page.render(stub.last_snapshot)
pump(lambda: stub.client.calls > 0, seconds=5.0)
first = stub.client.calls
check("the first render fetches", first > 0, f"calls={first}")

# Two seconds is one refresh interval: a page that re-fetches per render would
# be into the hundreds by now.
start = time.monotonic()
pump(lambda: False, seconds=2.0)
elapsed = time.monotonic() - start
per_refresh = len(("alerts", "coverage", "events", "stats", "inventory"))
budget = first + per_refresh * (elapsed / (REFRESH_MS / 1000.0) + 2)
check("and it does not re-fetch on every re-render",
      stub.client.calls <= budget,
      f"{stub.client.calls} IPC calls in {elapsed:.1f}s, budget {budget:.0f}")

# The throttle must not become a freeze: after the interval, a render fetches again.
before = stub.client.calls
pump(lambda: False, seconds=REFRESH_MS / 1000.0 + 0.2)
report_page.render(stub.last_snapshot)
pump(lambda: stub.client.calls > before, seconds=5.0)
check("a render after the interval fetches fresh data",
      stub.client.calls > before, f"stuck at {before}")

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
