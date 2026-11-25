#!/usr/bin/env python3
"""A background refresh must not undo what the user just did.

page-refresh-test.py covers Page.update() itself. These cover the pages that
slipped past it: the ones that fetch for themselves (they cleared first and
filled in later, so scroll and expanded rows were lost while the page was
empty), the Overview's event list (rebuilt outside update()), and the Hardware
page (controls rebuilt under the hand using them).

    python3 jamsys-ui/tests/refresh-state-test.py
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

from jamsys_ui import app as appmod  # noqa: E402
from jamsys_ui.app import (AsyncPage, EventsPage, HardwarePage, OverviewPage,  # noqa: E402
                           Page, SettingsPage, _expanded_titles)
from jamsys_ui.client import DaemonError  # noqa: E402
from jamsys_ui.widgets import MetricCard  # noqa: E402

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


def pump(until, seconds=3.0):
    ctx = GLib.MainContext.default()
    deadline = time.monotonic() + seconds
    while True:
        n = 0
        while ctx.pending() and n < 200:
            ctx.iteration(False)
            n += 1
        if until():
            return True
        if time.monotonic() >= deadline:
            return False
        GLib.timeout_add(10, lambda: GLib.SOURCE_REMOVE)
        ctx.iteration(True)


def settle():
    pump(lambda: False, seconds=0.15)


def labels(widget):
    return [w.get_text() for w in appmod._walk(widget) if isinstance(w, Gtk.Label)]


class StubClient:
    connected = True

    def __init__(self):
        self.replies = {}
        self.calls = []

    def call(self, op, **kw):
        self.calls.append(op)
        r = self.replies.get(op, {})
        if isinstance(r, Exception):
            raise r
        return r(kw) if callable(r) else r


class StubWin:
    _closed = False

    def __init__(self):
        self.client = StubClient()
        self.last_snapshot = {"snapshot": {}}
        self.page = None
        self.toasts = type("T", (), {"add_toast": lambda *_: None})()

    def current_page(self):
        return self.page

    def alert_menu(self, _alert):
        return Gtk.MenuButton(popover=Gtk.Popover())

    _threshold_dialog = _set_threshold = _unsuppress = _toggle_collector = \
        lambda *a, **k: None


def alert(title, ts=1_700_000_000_000):
    return {"title": title, "last_ts": ts, "count": 1, "severity": 2,
            "explanation": "why", "fingerprint": f"r:{title}", "rule_id": "r"}


print("group headings are markup, so they are escaped")
g = Page.group("WD & Sons <fast>", "a & b")
check("an ampersand in a heading survives", "WD & Sons <fast>" in labels(g), str(labels(g)))
check("and in the description", "a & b" in labels(g))

print("\na page that fetches for itself keeps what is on screen until the reply")
win = StubWin()
page = EventsPage(win)
win.page = page
host = Gtk.Window(child=page)
host.set_default_size(420, 300)
host.present()
pump(lambda: page.get_height() > 0)

page.filter = "alerts"
win.client.replies["alerts"] = {"alerts": [alert(f"alert {i}") for i in range(40)]}
page.render({})
pump(lambda: len(_expanded_titles(page.area)) == 0 and "alert 3" in labels(page.area))
check("the first reply builds the list", "alert 3" in labels(page.area))

rows = [w for w in appmod._walk(page.area) if isinstance(w, Adw.ExpanderRow)]
rows[2].set_expanded(True)
adj = page._scroller.get_vadjustment()
pump(lambda: adj.get_upper() > adj.get_page_size() > 0)
adj.set_value((adj.get_upper() - adj.get_page_size()) / 2)
where = adj.get_value()
first_row = rows[0]

page.update({})
check("nothing is cleared while the fetch is in flight",
      "alert 3" in labels(page.area))
settle()
check("identical data does not rebuild the rows", first_row.get_parent() is not None)
check("so the expanded row stays open", "alert 2" in _expanded_titles(page.area))
check("and the scroll offset is untouched", abs(adj.get_value() - where) < 2,
      f"{where} -> {adj.get_value()}")

win.client.replies["alerts"] = {"alerts": [alert("brand new")] +
                                [alert(f"alert {i}") for i in range(40)]}
page.update({})
pump(lambda: "brand new" in labels(page.area))
check("new data does rebuild", "brand new" in labels(page.area))
check("with the open row still open", "alert 2" in _expanded_titles(page.area))
pump(lambda: abs(adj.get_value() - where) < 2, seconds=1.0)
check("and the scroll offset restored", abs(adj.get_value() - where) < 2,
      f"{where} -> {adj.get_value()}")

win.client.replies["alerts"] = DaemonError("gone")
page.update({})
settle()
check("a failed refresh leaves the last good list in place",
      "brand new" in labels(page.area))

print("\nchanging the filter is the user's own doing")
win.client.replies["events"] = {"events": [
    {"ts": 1, "severity": 1, "summary": "an event", "subsystem": "net", "kind": "k"}]}
page.filter = "all"
page.reload()
pump(lambda: "an event" in labels(page.area))
check("the list is replaced", "an event" in labels(page.area)
      and "brand new" not in labels(page.area))
check("and starts from the top", adj.get_value() == 0)
toggles = [w for w in appmod._walk(page.body) if isinstance(w, Gtk.ToggleButton)]
# Clicking the selected filter again must not leave none selected, which
# ungrouped toggles allowed.
toggles[0].set_active(True)
toggles[0].emit("clicked")
check("one filter always stays selected", sum(t.get_active() for t in toggles) == 1)

print("\nan older daemon costs Settings one section, not the page")
swin = StubWin()
swin.client.replies["thresholds"] = DaemonError("unknown op")
swin.client.replies["suppressions"] = {"suppressions": []}
swin.client.replies["ping"] = {"version": "1.0.0", "schema": 1}
sp = SettingsPage(swin)
sp.render({})
pump(lambda: "Nothing is suppressed" in labels(sp.area))
check("suppressions still load", "Nothing is suppressed" in labels(sp.area))
check("and the gap says how to fix it",
      any("systemctl --user restart jamsysd" in t for t in labels(sp.area)))
swin.client.replies["thresholds"] = {"thresholds": [{
    "rule_id": "disk.full", "key": "pct", "applies_to": "disk.full", "default": 95.0,
    "min": 50.0, "max": 100.0, "value": 97.0, "unit": "%", "label": "Filesystem full"}]}
sp.reload(force=True)
pump(lambda: "Filesystem full" in labels(sp.area))
check("a threshold is listed with the value in force", "97 %" in labels(sp.area),
      str(labels(sp.area))[:300])
check("and a changed one names its default",
      any("default 95" in t for t in labels(sp.area)))

print("\nthe Overview's event list")
owin = StubWin()
ov = OverviewPage(owin)
items = [{"ts": 1_700_000_000_000, "severity": 2, "title": "disk filling",
          "summary": "disk filling", "rendered": "explanation", "fingerprint": "d:/"}]
ov.set_events(items)
row = ov._event_rows[0]
row.set_expanded(True)
ov.set_events([dict(i) for i in items])
check("is not rebuilt for identical data", ov._event_rows[0] is row)
ov.set_events(items + [{"ts": 5, "severity": 0, "title": "x", "summary": "x"}])
check("is rebuilt for new data", ov._event_rows[0] is not row)
check("keeping the expanded explanation open",
      "disk filling" in _expanded_titles(ov.events_group))

print("\na card that was unavailable once recovers")
card = MetricCard("Disk", "drive-harddisk-symbolic")
card.set_unavailable("No filesystems reported yet")
card.update("46%", "free", "", "healthy", "Normal")
check("it is no longer dimmed", not card.has_css_class("dim-label"))
check("and its sparkline is back", card.spark.get_visible())

print("\nthe Hardware page leaves its controls alone")
hwin = StubWin()
hw = HardwarePage(hwin)
hwin.page = hw
snap = {"snapshot": {"keyboard": {
    "present": True, "control": "helper", "max_brightness": 3, "brightness": 1,
    "rgb_capable": True, "state_capable": True, "rgb_mode_fields": "cmd mode r g b speed",
    "rgb_state_fields": "cmd boot awake sleep keyboard"}, "power": {"has_battery": False}}}
hw.update(snap)
scale = hw._scale
hw.update(snap)
check("an unchanged snapshot does not replace the slider", hw._scale is scale)
snap["snapshot"]["keyboard"]["brightness"] = 3
hw.update(snap)
check("a brightness change updates it in place",
      hw._scale is scale and scale.get_value() == 3.0)
check("and the reading beside it", hw._brightness_row.get_subtitle() == "currently 3")

sent = []
hw.kbd.set_states = lambda *a: sent.append(a) or True
hw._state_switches["boot"].set_active(True)
pump(lambda: bool(sent))
check("a power-state switch is sent", sent and sent[0][:4] == (True, True, False, True),
      str(sent))
hw.render(snap)
check("and still shows what was set after a rebuild",
      hw._state_switches["boot"].get_active())

print("\nhelper writes are serialised, newest wins")
order, gate = [], []
import threading  # noqa: E402
release = threading.Event()


def slow(v, _c):
    order.append(v)
    if v == 1:
        release.wait(2)
    return True


hw.kbd.set_brightness = slow
for v in (1, 2, 0):
    hw._scale.set_value(v)
pump(lambda: order == [1])
check("only one write is in flight", order == [1], str(order))
release.set()
pump(lambda: len(order) == 2)
check("the superseded value is skipped and the last one sent", order == [1, 0], str(order))

print("\nthe Security page")
from jamsys_ui.app import SecurityPage  # noqa: E402

accepted = []
secwin = StubWin()
secwin.accept_listener = accepted.append
sp2 = SecurityPage(secwin)
sp2.render({"snapshot": {}})
check("an older daemon is explained, not drawn as 'all clear'",
      any("systemctl --user restart jamsysd" in t for t in labels(sp2.body)))

SEC = {"firewall": {"state": "disabled", "tool": "ufw", "detail": "ufw is switched off & unread"},
       "exposed_tcp": 1, "auth_failures_10m": 2, "auth_failures_1h": 7, "auth_log_watched": True,
       "recent_failures": [{"ts_ms": 1_700_000_000_000, "service": "sshd", "user": "root",
                            "source": "203.0.113.9"}],
       "listeners": [
           {"proto": "tcp", "port": 8000, "exposure": "all", "addresses": ["0.0.0.0"],
            "owner": "ronin", "process": "python3 <b>", "pid": 4242,
            "key": "python3:tcp/8000", "expected": False},
           {"proto": "tcp", "port": 22, "exposure": "all", "addresses": ["0.0.0.0", "::"],
            "owner": "root", "process": None, "pid": None, "key": "root:tcp/22", "expected": True},
           {"proto": "tcp", "port": 631, "exposure": "loopback", "addresses": ["127.0.0.1"],
            "owner": "root", "process": None, "pid": None, "key": "root:tcp/631", "expected": True},
           {"proto": "udp", "port": 5353, "exposure": "all", "addresses": ["0.0.0.0"],
            "owner": "avahi", "process": None, "pid": None, "key": "avahi:udp/5353", "expected": True}]}
snap = {"snapshot": {"security": SEC}}
sp2.update(snap)
text = labels(sp2.body)
check("a disabled firewall is named with the command that enables it",
      "ufw is switched off" in text and "sudo ufw enable" in text, str(text)[:200])
check("its detail survives an ampersand", "ufw is switched off & unread" in text)
check("a listener is shown by port and program, markup and all",
      "Port 8000 — python3 <b>" in text, str([t for t in text if "8000" in t]))
check("a root-owned one admits its name is unknown",
      any("runs as root; its name needs root to read" in t for t in text))
check("failed sign-ins show the count and the remote source",
      "7" in text and any("from 203.0.113.9" in t for t in text))
buttons = [w for w in appmod._walk(sp2.body)
           if isinstance(w, Gtk.Button) and w.get_label() == "Accept"]
check("only the new, network-facing listener can be accepted", len(buttons) == 1,
      f"{len(buttons)} Accept buttons")
buttons[0].emit("clicked")
check("and accepting sends its key", accepted == ["python3:tcp/8000"], str(accepted))
first = buttons[0]
sp2.update({"snapshot": {"security": dict(SEC)}})
check("an unchanged reading leaves the button in place", first.get_root() is None
      and first.get_parent() is not None)
SEC2 = dict(SEC, auth_failures_10m=3)
sp2.update({"snapshot": {"security": SEC2}})
check("a changed one redraws", "3" in labels(sp2.body) and first.get_parent() is None)
SEC3 = dict(SEC, auth_log_watched=False)
sp2.update({"snapshot": {"security": SEC3}})
check("an unwatched sign-in log says so instead of showing zero",
      "Not being counted" in labels(sp2.body))

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
