"""JamSys desktop application — GTK4 + libadwaita.

The UI is a thin client over the daemon's documented socket. It holds no monitoring
logic of its own: everything it shows came from `snapshot`, `history`, `alerts`,
`events`, `coverage` or `processes`. That separation is what lets the daemon keep
running with the window closed, and what would let a different front-end replace this
one without touching the backend.
"""

from __future__ import annotations

import datetime
import sys
import time
from typing import Optional

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gio, GLib, Gtk  # noqa: E402

from . import report
from .processes import select_processes
from .client import Client, DaemonError, run_async
from .hardware import (CHARGE_LIMITS, MODES, PRESETS, SPEEDS, ChargeLimitControl,
                       FirewallControl,
                       KeyboardControl, ints_to_rgba, repo_script, rgba_to_ints)
from .format import (SEVERITY, clock_hm, human_ago, human_bps, human_bytes,
                     human_duration, pct, temp, watts)
from .widgets import (CSS, CoreMatrix, MetricCard, Sparkline, StatusPill,
                      core_usage_legend, severity_row)

APP_ID = "org.jamsys.Monitor"


def _self_rss() -> int:
    """This process's resident size, so the Diagnostics page can report the UI's own
    cost rather than only the daemon's."""
    try:
        with open("/proc/self/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        pass
    return 0
REFRESH_MS = 2000


def _selected(table, dropdown, fallback):
    """The value behind a GtkDropDown's selection, or `fallback`.

    get_selected() answers GTK_INVALID_LIST_POSITION (0xFFFFFFFF) when nothing is
    selected, which indexes nothing and raises.
    """
    i = dropdown.get_selected()
    if i == Gtk.INVALID_LIST_POSITION or not 0 <= i < len(table):
        return fallback
    return table[i][1]


def _has_open_popover(widget) -> bool:
    """Is any popover under this widget currently on screen?

    A GtkDropDown's list, a menu, and a colour picker are all popovers, and in
    GTK4 they are children of the widget that owns them, so an ordinary
    depth-first walk finds them.
    """
    child = widget.get_first_child()
    while child is not None:
        if isinstance(child, Gtk.Popover) and child.is_visible():
            return True
        if _has_open_popover(child):
            return True
        child = child.get_next_sibling()
    return False


def _walk(widget):
    """Every descendant of `widget`, depth first."""
    child = widget.get_first_child()
    while child is not None:
        yield child
        yield from _walk(child)
        child = child.get_next_sibling()


def _expanded_titles(widget) -> set:
    """Titles of the expander rows the user currently has open under `widget`."""
    return {w.get_title() for w in _walk(widget)
            if isinstance(w, Adw.ExpanderRow) and w.get_expanded()}


def _reexpand(widget, titles: set) -> None:
    """Open the rows that were open before a rebuild, without replaying the slide.

    A rebuilt row is a new widget, so setting it expanded would animate it open
    again on every refresh -- the explanation the user is reading would collapse
    and slide back every two seconds. Animations are switched off for the
    duration of the call, which makes the revealer jump straight to its end state.
    """
    if not titles:
        return
    settings = Gtk.Settings.get_default()
    animated = settings.get_property("gtk-enable-animations") if settings else False
    if animated:
        settings.set_property("gtk-enable-animations", False)
    try:
        for w in _walk(widget):
            if isinstance(w, Adw.ExpanderRow) and w.get_title() in titles:
                w.set_expanded(True)
    finally:
        if animated:
            settings.set_property("gtk-enable-animations", True)


class Page(Gtk.Box):
    """Base for every detail page. Subclasses implement `render`."""

    title = "Page"
    icon = "view-list-symbolic"
    subsystem: Optional[str] = None

    def __init__(self, win: "MainWindow"):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.win = win
        self.clamp = Adw.Clamp(maximum_size=980, tightening_threshold=700)
        self.clamp.set_margin_top(18)
        self.clamp.set_margin_bottom(24)
        self.clamp.set_margin_start(14)
        self.clamp.set_margin_end(14)
        self.body = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.clamp.set_child(self.body)
        scroller = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER, vexpand=True)
        scroller.set_child(self.clamp)
        self.append(scroller)
        self._scroller = scroller

    def render(self, snap: dict) -> None:
        raise NotImplementedError

    # -- refresh, without fighting the user ------------------------------

    def update(self, snap: dict) -> None:
        """Re-render for new data, unless that would interrupt the user.

        Every page rebuilds its whole body from scratch. That is fine for text
        and hostile for anything you can interact with: on each two-second
        refresh an open dropdown is destroyed underneath the pointer, and the
        scroll position snaps back to the top. The symptom is a page that
        jumps and a list you cannot scroll or pick from.

        So: never rebuild while a popup is open or a control has focus, and
        put the scroll position back afterwards. Data is at most one tick
        stale while a menu is open, which nobody can perceive and everybody
        prefers to a list that moves as they reach for it.
        """
        if self.is_interacting():
            return
        state = self.view_state()
        self.render(snap)
        self.restore_view_state(state)

    def view_state(self):
        """What a rebuild would otherwise throw away: scroll offset and open rows."""
        adj = self._scroller.get_vadjustment() if self._scroller else None
        return (adj.get_value() if adj else 0.0, _expanded_titles(self.body))

    def restore_view_state(self, state) -> None:
        pos, expanded = state
        _reexpand(self.body, expanded)
        if pos > 0.0:
            # After the rebuild the new children have not been allocated yet,
            # so upper/page_size are still stale; restoring on idle waits for
            # the layout that makes the clamp meaningful.
            GLib.idle_add(self._restore_scroll, pos, priority=GLib.PRIORITY_LOW)

    def _restore_scroll(self, pos: float) -> bool:
        adj = self._scroller.get_vadjustment() if self._scroller else None
        if adj:
            adj.set_value(min(pos, max(0.0, adj.get_upper() - adj.get_page_size())))
        return False

    def is_interacting(self) -> bool:
        """True when rebuilding now would yank something out from under the user."""
        if _has_open_popover(self):
            return True
        root = self.get_root()
        focus = root.get_focus() if root else None
        # A text entry keeps focus while being typed into; a rebuild would drop
        # the caret and the partially-typed value with it.
        return bool(focus and focus.is_ancestor(self)
                    and isinstance(focus, (Gtk.Entry, Gtk.SearchEntry, Gtk.Text)))

    def show_render_error(self, err: Exception) -> None:
        """Say that the page failed, on the page.

        A render that raises used to be printed to a terminal nobody was looking
        at, leaving a half-built page that read as "nothing to report here".
        """
        self.clear()
        g = self.group("This page could not be drawn",
                       "The readings arrived but JamSys failed to display them. "
                       "The other pages are unaffected. Running "
                       "`jamsys --json snapshot` shows the raw data, and the "
                       "error below is what to include in a bug report.")
        g.add(self.row(type(err).__name__, "", str(err)))
        self.body.append(g)

    # -- small helpers used by every page --------------------------------

    @staticmethod
    def group(title: str, description: str = "") -> Adw.PreferencesGroup:
        # Both are parsed as Pango markup, like row titles. A drive model or an
        # interface name with an ampersand in it otherwise blanks the heading.
        return Adw.PreferencesGroup(title=GLib.markup_escape_text(title),
                                    description=GLib.markup_escape_text(description))

    @staticmethod
    def row(title: str, value: str, subtitle: str = "") -> Adw.ActionRow:
        """One labelled reading.

        Note the escaping: AdwActionRow titles and subtitles are parsed as Pango
        markup. Anything from the daemon -- a process name, a unit description, a
        mount path -- must be escaped, or an ampersand makes the whole title render
        as nothing and a tag styles it.
        """
        r = Adw.ActionRow(title=GLib.markup_escape_text(title))
        if subtitle:
            r.set_subtitle(GLib.markup_escape_text(subtitle))
        lbl = Gtk.Label(label=value, xalign=1)
        lbl.add_css_class("dim-label")
        lbl.set_selectable(True)
        r.add_suffix(lbl)
        return r

    def clear(self) -> None:
        self._empty(self.body)

    @staticmethod
    def _empty(box) -> None:
        child = box.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            box.remove(child)
            child = nxt


class AsyncPage(Page):
    """A page whose content needs its own round trip to the daemon.

    These pages used to clear themselves and then fill in when the reply
    arrived, on every two-second refresh. Three things followed from that: the
    page blinked empty each tick; the scroll offset was restored while the page
    was still empty, so it snapped to the top and a long event list could not be
    read past the first screen; and any row the user had expanded was collapsed
    again before they finished reading it.

    So the order is reversed: fetch first, and only touch the widgets once the
    reply is in hand -- and not even then if nothing the page shows has changed.
    """

    #: Seconds a page may go without a rebuild when only volatile figures
    #: (run counters, uptime) moved. None means "rebuild only on real change".
    max_age: Optional[float] = None

    def __init__(self, win: "MainWindow"):
        super().__init__(win)
        self.area = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.body.append(self.area)
        self._pending = False
        self._shown = None
        self._built_at = 0.0
        self._wanted = None

    # -- for subclasses ----------------------------------------------------

    def request(self):
        """Hashable description of what to fetch (a filter, say). Main thread."""
        return None

    def fetch(self, request):
        """Worker thread. Returns the data `build` needs; may raise DaemonError."""
        raise NotImplementedError

    def build(self, data) -> None:
        """Main thread. `self.area` is empty; fill it."""
        raise NotImplementedError

    def identity(self, data):
        """The part of `data` whose change is worth a rebuild."""
        return data

    # -- machinery -----------------------------------------------------------

    def render(self, snap: dict) -> None:
        self.reload()

    def reload(self, force: bool = False) -> None:
        req = self.request()
        if self._pending:
            # Remember the newest request so a filter clicked mid-fetch is not lost.
            self._wanted = (req, force)
            return
        if not self.win.client.connected:
            if self._shown is None:
                self._message("Waiting for the monitoring service",
                              "This page fills in as soon as JamSys reconnects.")
            return
        if self._shown is None:
            self._message("Loading…", "")
        self._pending = True

        def done(res, err):
            self._pending = False
            if getattr(self.win, "_closed", False):
                return
            wanted, self._wanted = self._wanted, None
            if req == self.request():
                self._deliver(req, res, err, force)
            if wanted is not None and (wanted[0] != req or wanted[1]):
                self.reload(force=wanted[1])

        run_async(lambda: self.fetch(req), done)

    def _deliver(self, req, res, err, force: bool) -> None:
        if err is not None or res is None:
            # Keep what is on screen: stale readings beat an error box, and the
            # window-wide banner already says the service is unreachable.
            if self._shown is None or self._shown[0] != req:
                self._message("Could not load this page", str(err))
                self._shown = None
            return
        key = (req, self.identity(res))
        age = time.monotonic() - self._built_at
        fresh = self.max_age is None or age < self.max_age
        if not force and key == self._shown and fresh:
            return
        # A rebuild for a new request is the user's own doing; one for new data
        # must wait until they have let go of whatever they are holding.
        same_request = self._shown is not None and self._shown[0] == req
        if not force and same_request and self.is_interacting():
            return
        state = self.view_state()
        self._empty(self.area)
        try:
            self.build(res)
        except Exception as e:  # noqa: BLE001 -- a render bug must not kill the UI
            print(f"[jamsys-ui] render error on {self.title}: {e!r}", file=sys.stderr)
            self._message("This page could not be drawn",
                          f"{type(e).__name__}: {e}")
            self._shown = None
            return
        self._shown = key
        self._built_at = time.monotonic()
        if same_request:
            self.restore_view_state(state)
        else:
            self._scroller.get_vadjustment().set_value(0)

    def _message(self, title: str, detail: str) -> None:
        self._empty(self.area)
        self.area.append(self.group(title, detail))


# ---------------------------------------------------------------------------
# Overview
# ---------------------------------------------------------------------------

class OverviewPage(Page):
    title = "Overview"
    icon = "view-grid-symbolic"

    def __init__(self, win):
        super().__init__(win)
        # The one-second answer.
        self.banner = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=14)
        self.banner.add_css_class("sv-banner-unknown")
        self.banner.set_margin_bottom(4)
        for m in ("top", "bottom"):
            getattr(self.banner, f"set_margin_{m}")(16)
        self.banner.set_margin_start(18)
        self.banner.set_margin_end(18)
        self.banner_icon = Gtk.Image()
        self.banner_icon.set_pixel_size(34)
        self.banner_text = Gtk.Label(xalign=0, label="Connecting…")
        self.banner_text.add_css_class("sv-headline")
        self.banner_sub = Gtk.Label(xalign=0, label="")
        self.banner_sub.add_css_class("caption")
        self.banner_sub.set_wrap(True)
        col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2, hexpand=True)
        col.append(self.banner_text)
        col.append(self.banner_sub)
        self.banner.append(self.banner_icon)
        self.banner.append(col)
        self.body.append(self.banner)

        self.coverage_note = Gtk.Label(xalign=0, label="")
        self.coverage_note.add_css_class("caption")
        self.coverage_note.add_css_class("dim-label")
        self.coverage_note.set_wrap(True)
        self.body.append(self.coverage_note)

        grid = Gtk.FlowBox(
            selection_mode=Gtk.SelectionMode.NONE, homogeneous=True,
            min_children_per_line=2, max_children_per_line=3,
            row_spacing=12, column_spacing=12)
        self.cards: dict[str, MetricCard] = {}
        for key, title, icon, page in [
            ("cpu", "CPU", "computer-symbolic", "CPU"),
            ("memory", "Memory", "drive-harddisk-solidstate-symbolic", "Memory"),
            ("gpu", "Graphics", "video-display-symbolic", "GPU"),
            ("power", "Battery", "battery-symbolic", "Power"),
            ("thermal", "Temperature", "temperature-symbolic", "Thermals"),
            ("storage", "Disk", "drive-harddisk-symbolic", "Storage"),
            ("network", "Network", "network-wireless-symbolic", "Network"),
            ("services", "Services", "system-run-symbolic", "Services"),
            ("security", "Security", "security-high-symbolic", "Security"),
        ]:
            c = MetricCard(title, icon, on_click=lambda p=page: self.win.goto(p))
            self.cards[key] = c
            grid.append(c)
        self.body.append(grid)

        self.events_group = self.group("Recent events and alerts")
        self.body.append(self.events_group)
        self._event_rows: list = []
        self._events_shown = None
        # Sparklines are refreshed on a much slower cadence than the numbers. Eight
        # history queries every two seconds would be pointless work for a 60-pixel
        # strip whose shape barely changes between ticks.
        self._spark_tick = 0

    def render(self, snap: dict) -> None:
        d = snap
        s = d.get("snapshot", {})
        health = d.get("health", "unknown")
        warn = d.get("open_warnings", 0)
        crit = d.get("open_critical", 0)

        for c in ("sv-banner-healthy", "sv-banner-attention", "sv-banner-critical", "sv-banner-unknown"):
            self.banner.remove_css_class(c)
        self.banner.add_css_class(f"sv-banner-{health}")
        self.banner_icon.set_from_icon_name({
            "healthy": "emblem-ok-symbolic",
            "attention": "dialog-warning-symbolic",
            "critical": "dialog-error-symbolic",
        }.get(health, "dialog-question-symbolic"))
        for c in ("sv-healthy", "sv-attention", "sv-critical", "sv-unknown"):
            self.banner_icon.remove_css_class(c)
        self.banner_icon.add_css_class({"healthy": "sv-healthy", "attention": "sv-attention",
                                        "critical": "sv-critical"}.get(health, "sv-unknown"))
        self.banner_text.set_text({
            "healthy": "Everything is normal",
            "attention": "Attention needed",
            "critical": "Something is wrong",
        }.get(health, "Status unknown"))

        usable = d.get("collectors_usable", 0)
        total = d.get("collectors_total", 0)
        if health == "healthy":
            self.banner_sub.set_text(
                f"All {usable} monitored subsystems are behaving normally.")
        else:
            bits = []
            if crit:
                bits.append(f"{crit} critical")
            if warn:
                bits.append(f"{warn} needing attention")
            self.banner_sub.set_text(", ".join(bits) + " — see the list below.")

        # Monitoring gaps are stated separately: a gap is not a system fault, and
        # conflating the two is how users learn to distrust the status light.
        notes = []
        if usable < total:
            notes.append(f"{total - usable} of {total} monitors are unavailable on this machine")
        if not d.get("helper", False):
            notes.append("the privileged helper is not installed, so CPU package power and NVMe SMART are unavailable")
        self.coverage_note.set_text(("Coverage: " + "; ".join(notes) + ".") if notes else "")
        self.coverage_note.set_visible(bool(notes))

        self._maybe_refresh_sparklines(s)
        self._cpu_card(s)
        self._mem_card(s)
        self._gpu_card(s)
        self._power_card(s)
        self._thermal_card(s)
        self._storage_card(s)
        self._network_card(s)
        self._services_card(s)
        self._security_card(s)

    # -- sparklines ---------------------------------------------------------

    #: card key -> (subsystem, metric, instance-or-None-meaning-"active interface")
    SPARK_METRICS = {
        "cpu":     ("cpu", "usage_pct", ""),
        "memory":  ("memory", "used_bytes", ""),
        "gpu":     ("gpu", "power_w", "nvidia"),
        "power":   ("power", "system_w", ""),
        "thermal": ("thermal", "cpu_package_c", ""),
        "storage": ("storage", "write_bps", None),
        "network": ("network", "rx_bps", None),
        "services": ("services", "failed_count", ""),
    }

    def _maybe_refresh_sparklines(self, snap):
        # Every 8th refresh, so roughly every 16 s at the default 2 s cadence.
        self._spark_tick += 1
        if self._spark_tick % 8 != 1:
            return

        disks = snap.get("storage", {}).get("disks", [])
        disk = disks[0]["device"] if disks else "nvme0n1"
        active = next((i["name"] for i in snap.get("network", {}).get("ifaces", [])
                       if i["kind"] != "loopback" and i.get("carrier")), "")
        wanted = {}
        for key, (sub, name, inst) in self.SPARK_METRICS.items():
            if inst is None:
                inst = disk if key == "storage" else active
                if not inst:
                    continue
            wanted[key] = (sub, name, inst)

        def work():
            until = int(time.time() * 1000)
            since = until - 3_600_000
            out = {}
            for key, (sub, name, inst) in wanted.items():
                try:
                    r = self.win.client.call("history", subsystem=sub, name=name,
                                             instance=inst, since_ms=since,
                                             until_ms=until, max_points=60)
                    out[key] = [p[1] for p in r.get("points", [])]
                except DaemonError:
                    out[key] = []
            return out

        def done(res, err):
            if err or not res:
                return
            for key, pts in res.items():
                card = self.cards.get(key)
                if card is not None and pts:
                    card.spark.set_points(pts)
        run_async(work, done)

    # -- individual cards -------------------------------------------------

    def _cpu_card(self, s):
        c = s.get("cpu", {})
        t = (s.get("thermal") or {}).get("cpu_package_c")
        status = "healthy"
        word = "Normal"
        if c.get("throttling_now"):
            status, word = "attention", "Throttling"
        elif t and t >= 90:
            status, word = "critical", "Very hot"
        elif c.get("usage_pct", 0) >= 90:
            status, word = "attention", "Busy"
        self.cards["cpu"].update(
            f"{c.get('usage_pct', 0):.0f}%",
            f"{temp(t)} · {c.get('freq_mhz', 0):.0f} MHz · load {c.get('load1', 0):.2f}",
            "", status, word)

    def _mem_card(self, s):
        m = s.get("memory", {})
        total = m.get("total_bytes", 0)
        avail = m.get("available_pct", 100)
        status, word = "healthy", "Normal"
        if avail < 3:
            status, word = "critical", "Almost full"
        elif avail < 8:
            status, word = "attention", "Low"
        elif m.get("psi_some_avg60", 0) > 20:
            status, word = "attention", "Under pressure"
        swap = m.get("swap_used_bytes", 0)
        self.cards["memory"].update(
            f"{human_bytes(m.get('used_bytes', 0))}",
            f"of {human_bytes(total)} · {avail:.0f}% free · swap {human_bytes(swap)}",
            "", status, word)

    def _gpu_card(self, s):
        g = s.get("gpu", {})
        n = g.get("nvidia", {})
        i = g.get("intel", {})
        if not n.get("present") and not i.get("present"):
            self.cards["gpu"].set_unavailable("No supported GPU found")
            return
        if n.get("present"):
            state = n.get("runtime_status", "unknown")
            if state == "suspended":
                self.cards["gpu"].update(
                    "Suspended", f"NVIDIA asleep · {i.get('act_freq_mhz', 0):.0f} MHz Intel · display: {g.get('display_driver','?')}",
                    "", "idle", "Idle")
            else:
                status, word = "healthy", "Active"
                if g.get("dgpu_awake_idle"):
                    status, word = "attention", "Awake but idle"
                if (n.get("temp_c") or 0) >= 87:
                    status, word = "critical", "Hot"
                self.cards["gpu"].update(
                    pct(n.get("util_pct")),
                    f"{watts(n.get('power_w'))} · {temp(n.get('temp_c'))} · P{n.get('pstate') if n.get('pstate') is not None else '?'}",
                    "", status, word)
        else:
            self.cards["gpu"].update(
                f"{i.get('act_freq_mhz', 0):.0f} MHz",
                f"Intel · RC6 {pct(i.get('rc6_pct'))}", "", "healthy", "Normal")

    def _power_card(self, s):
        p = s.get("power", {})
        if not p.get("has_battery"):
            self.cards["power"].set_unavailable("No battery on this machine")
            return
        pctv = p.get("percent", 0)
        w = p.get("power_w", 0)
        status, word = "healthy", "Normal"
        if p.get("on_battery") and pctv <= 5:
            status, word = "critical", "Critically low"
        elif p.get("health_pct", 100) < 70:
            status, word = "attention", "Degraded"
        rt = p.get("runtime_s")
        sub = f"{abs(w):.1f} W {'discharge' if p.get('on_battery') else 'charge'}"
        if rt:
            sub += f" · {human_duration(rt)} left"
        elif not p.get("on_battery"):
            sub += " · on AC"
        self.cards["power"].update(f"{pctv:.0f}%", sub,
                                   f"health {p.get('health_pct', 0):.0f}% of design",
                                   status, word)

    def _thermal_card(self, s):
        t = s.get("thermal", {})
        mx = t.get("max_temp_c")
        fans = t.get("fans", [])
        status, word = "healthy", "Normal"
        if mx and mx >= 95:
            status, word = "critical", "Very hot"
        elif mx and mx >= 85:
            status, word = "attention", "Hot"
        fan_txt = " · ".join(f"{f['label']} {f['value']:.0f}" for f in fans[:2]) or "no fan sensors"
        self.cards["thermal"].update(
            temp(t.get("cpu_package_c")),
            f"NVMe {temp(t.get('nvme_c'))} · {fan_txt}",
            f"hottest: {t.get('max_temp_label','—')} {temp(mx)}",
            status, word)

    def _storage_card(self, s):
        st = s.get("storage", {})
        fs = st.get("filesystems", [])
        root = next((f for f in fs if f["mount"] == "/"), fs[0] if fs else None)
        if not root:
            self.cards["storage"].set_unavailable("No filesystems reported yet")
            return
        status, word = "healthy", "Normal"
        worst = max((f["used_pct"] for f in fs), default=0)
        if worst >= 95:
            status, word = "critical", "Almost full"
        elif worst >= 90:
            status, word = "attention", "Filling up"
        nvme = st.get("nvme", [])
        extra = f"NVMe {temp(nvme[0].get('temp_c'))}" if nvme else ""
        self.cards["storage"].update(
            f"{root['used_pct']:.0f}%",
            f"{human_bytes(root['free_bytes'])} free on / · {extra}",
            f"read {human_bps(st.get('total_read_bps', 0))} · write {human_bps(st.get('total_write_bps', 0))}",
            status, word)

    def _network_card(self, s):
        n = s.get("network", {})
        ifs = [i for i in n.get("ifaces", []) if i["kind"] != "loopback"]
        active = next((i for i in ifs if i.get("carrier")), None)
        reach = n.get("reach", {})
        if not active:
            self.cards["network"].update("Offline", "No interface has a link", "",
                                         "attention", "Disconnected")
            return
        status, word = "healthy", "Normal"
        if reach.get("internet_ok") is False and reach.get("gateway_ok"):
            status, word = "attention", "No internet"
        elif reach.get("gateway_ok") is False:
            status, word = "attention", "No gateway"
        elif active.get("error_rate_pct", 0) >= 1:
            status, word = "attention", "Packet loss"
        sig = f"{active['signal_dbm']:.0f} dBm · " if active.get("signal_dbm") else ""
        self.cards["network"].update(
            active["name"],
            f"{sig}{active.get('error_rate_pct', 0):.2f}% loss",
            f"↓ {human_bps(n.get('total_rx_bps', 0))}  ↑ {human_bps(n.get('total_tx_bps', 0))}",
            status, word)

    def _services_card(self, s):
        sv = s.get("services", {})
        failed = sv.get("failed", []) + sv.get("failed_user", [])
        ignored = sv.get("ignored", [])
        if failed:
            self.cards["services"].update(
                str(len(failed)),
                "failed unit" + ("s" if len(failed) != 1 else "") + ": " + ", ".join(u["name"] for u in failed[:2]),
                f"{len(ignored)} ignored" if ignored else "",
                "attention", "Needs attention")
        else:
            self.cards["services"].update(
                "0", "No failed units",
                f"{len(ignored)} ignored" if ignored else "",
                "healthy", "Normal")

    def _security_card(self, s):
        sec = s.get("security")
        card = self.cards["security"]
        card.spark.set_visible(False)       # a checklist has no curve
        if not sec:
            card.set_unavailable("Restart the daemon to enable: "
                                 "systemctl --user restart jamsysd")
            return
        checks = sec.get("checks") or []
        failing = [c for c in checks if c.get("status") == "warn"]
        new = len(sec.get("unexpected") or [])
        fails = sec.get("auth_failures_10m", 0)
        fw = (sec.get("firewall") or {}).get("state", "unknown")
        status, word = "healthy", "Normal"
        if new:
            status, word = "attention", "New listener"
        elif failing:
            status, word = "attention", "Worth fixing"
        card.update(
            "All clear" if not failing and not new else
            f"{len(failing) + new} to check",
            (failing[0]["title"] if failing else
             f"firewall {fw} · {sec.get('exposed_tcp', 0)} reachable from the network"),
            f"{fails} failed sign-in(s) in 10 min" if fails else "",
            status, word)
        card.spark.set_visible(False)

    def set_events(self, items: list[dict]) -> None:
        """Replace the event list -- unless that would only get in the way.

        This runs on every refresh, outside Page.update(), so it needs the same
        guards: rebuilding the rows closed an alert's open menu under the
        pointer and collapsed the explanation being read, every two seconds.
        Relative times ("3m ago") are the only thing that moves when the data
        does not, so the minute is part of what counts as a change.
        """
        key = (items[:12], int(time.time() // 60))
        if key == self._events_shown or self.is_interacting():
            return
        self._events_shown = key
        expanded = _expanded_titles(self.events_group)
        self._replace_events(items)
        _reexpand(self.events_group, expanded)

    def _replace_events(self, items: list[dict]) -> None:
        for r in self._event_rows:
            self.events_group.remove(r)
        self._event_rows.clear()
        now = time.time() * 1000
        if not items:
            r = Adw.ActionRow(title="Nothing to report",
                              subtitle="No events or alerts have been recorded yet.")
            self.events_group.add(r)
            self._event_rows.append(r)
            return
        for it in items[:12]:
            sev = it.get("severity", 0)
            when = f"{clock_hm(it['ts'])} · {human_ago(it['ts'], now)}"
            r = severity_row(sev, it.get("title") or it.get("summary", ""), when)
            if it.get("rendered"):
                exp = Adw.ExpanderRow(title=GLib.markup_escape_text(it.get("title") or it["summary"]),
                                      subtitle=when)
                names = ["emblem-ok-symbolic", "dialog-information-symbolic",
                         "dialog-warning-symbolic", "dialog-error-symbolic"]
                classes = ["sv-healthy", "sv-idle", "sv-attention", "sv-critical"]
                ic = Gtk.Image.new_from_icon_name(names[min(sev, 3)])
                ic.add_css_class(classes[min(sev, 3)])
                exp.add_prefix(ic)
                detail = Gtk.Label(xalign=0, label=it["rendered"], selectable=True)
                detail.set_wrap(True)
                detail.add_css_class("sv-mono")
                detail.set_margin_top(8)
                detail.set_margin_bottom(8)
                detail.set_margin_start(12)
                detail.set_margin_end(12)
                holder = Adw.ActionRow()
                holder.set_child(detail)
                exp.add_row(holder)
                if it.get("fingerprint"):
                    exp.add_suffix(self.win.alert_menu(it))
                r = exp
            self.events_group.add(r)
            self._event_rows.append(r)


# ---------------------------------------------------------------------------
# Detail pages
# ---------------------------------------------------------------------------

class CpuPage(Page):
    title, icon = "CPU", "computer-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        c = s.get("cpu", {})
        self.clear()
        g = self.group("Now")
        g.add(self.row("Total usage", f"{c.get('usage_pct', 0):.1f}%"))
        g.add(self.row("Load average", f"{c.get('load1', 0):.2f} · {c.get('load5', 0):.2f} · {c.get('load15', 0):.2f}",
                       f"{c.get('cores', 0)} logical CPUs"))
        g.add(self.row("Frequency", f"{c.get('freq_mhz', 0):.0f} MHz",
                       f"maximum {c.get('freq_max_mhz', 0):.0f} MHz · {c.get('scaling_driver','')} / {c.get('governor','')}"))
        g.add(self.row("Package temperature", temp((s.get("thermal") or {}).get("cpu_package_c"))))
        g.add(self.row("Stall time (PSI)", f"{c.get('psi_some_avg60', 0):.2f}%",
                       "share of the last minute some task spent waiting for CPU"))
        g.add(self.row("Throttle events", str(c.get("package_throttle_count", 0)),
                       "cumulative since boot" + (" · throttling right now" if c.get("throttling_now") else "")))
        g.add(self.row("Processes", f"{c.get('procs_total', 0)}",
                       f"{c.get('procs_running', 0)} runnable · {c.get('ctxt_per_s', 0):.0f} context switches/s"))
        self.body.append(g)

        cores = c.get("per_core_pct", [])
        if cores:
            hot = max(range(len(cores)), key=lambda i: cores[i])
            spread = max(cores) - min(cores)
            cg = self.group(
                "Per-core usage",
                f"One box per logical CPU · busiest cpu{hot} at {cores[hot]:.0f}% · "
                + ("work is spread evenly" if spread < 25
                   else f"{spread:.0f} points between the busiest and the quietest"))
            holder = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
            holder.set_margin_top(8)
            matrix = CoreMatrix()
            matrix.set_values(cores)
            holder.append(matrix)
            holder.append(core_usage_legend())
            cg.add(holder)
            self.body.append(cg)

        self.body.append(self.win.chart("cpu", "usage_pct", "", "CPU usage", "%"))
        top = self.group("Top processes by CPU")
        for p in s.get("process", {}).get("top_cpu", [])[:8]:
            top.add(self.row(f"{p['name']} ({p['pid']})", f"{p['cpu_pct']:.1f}%",
                             f"{human_bytes(p['rss_bytes'])} · {p['threads']} threads"))
        self.body.append(top)


class MemoryPage(Page):
    title, icon = "Memory", "drive-harddisk-solidstate-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        m = s.get("memory", {})
        self.clear()
        g = self.group("Now")
        g.add(self.row("Used", human_bytes(m.get("used_bytes", 0)),
                       f"of {human_bytes(m.get('total_bytes', 0))} total"))
        g.add(self.row("Available", f"{human_bytes(m.get('available_bytes', 0))} ({m.get('available_pct', 0):.0f}%)"))
        g.add(self.row("Cache and buffers", human_bytes(m.get("cached_bytes", 0) + m.get("buffers_bytes", 0)),
                       "reclaimable, counted as available"))
        g.add(self.row("Swap", f"{human_bytes(m.get('swap_used_bytes', 0))} of {human_bytes(m.get('swap_total_bytes', 0))}",
                       f"{m.get('swap_used_pct', 0):.1f}% used"))
        g.add(self.row("Memory pressure (PSI)", f"some {m.get('psi_some_avg60', 0):.2f}% · full {m.get('psi_full_avg60', 0):.2f}%",
                       "time tasks spent stalled waiting for memory over the last minute"))
        g.add(self.row("Major page faults", f"{m.get('major_faults_per_s', 0):.1f}/s"))
        g.add(self.row("OOM kills since boot", str(m.get("oom_kill_total", 0))))
        self.body.append(g)
        self.body.append(self.win.chart("memory", "used_bytes", "", "Memory in use", "B"))

        growth = s.get("process", {}).get("growth_bph", {}) or {}
        top = self.group("Top processes by memory",
                         "Growth is a Theil–Sen slope over the retained window; a steady climb is the signature of a leak.")
        for p in s.get("process", {}).get("top_mem", [])[:10]:
            gr = growth.get(str(p["pid"]), growth.get(p["pid"], 0)) or 0
            sub = f"{p['cpu_pct']:.1f}% CPU"
            if gr and abs(gr) > 1e6:
                sub += f" · growing {human_bytes(gr)}/hour"
            top.add(self.row(f"{p['name']} ({p['pid']})", human_bytes(p["rss_bytes"]), sub))
        self.body.append(top)


class GpuPage(Page):
    title, icon = "GPU", "video-display-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        g = s.get("gpu", {})
        self.clear()
        hdr = self.group("Hybrid graphics",
                         "On a hybrid laptop the question that matters is which GPU is doing the work, "
                         "and whether the discrete one is awake when it need not be.")
        hdr.add(self.row("Driving the display", g.get("display_driver", "unknown")))
        self.body.append(hdr)

        i = g.get("intel", {})
        if i.get("present"):
            ig = self.group(f"Intel integrated ({i.get('card','')})")
            ig.add(self.row("Frequency", f"{i.get('act_freq_mhz', 0):.0f} MHz",
                            f"requested {i.get('cur_freq_mhz', 0):.0f} MHz · max {i.get('max_freq_mhz', 0):.0f} MHz"))
            ig.add(self.row("RC6 residency", pct(i.get("rc6_pct")),
                            "share of time the render engine was powered down"))
            tr = i.get("throttle_reasons") or []
            ig.add(self.row("Throttling", ", ".join(tr) if tr else "none"))
            ig.add(self.row("Connected output", "yes" if i.get("drives_display") else "no"))
            self.body.append(ig)

        n = g.get("nvidia", {})
        if n.get("present"):
            ng = self.group(f"NVIDIA discrete — {n.get('name') or 'discrete GPU'}",
                            "JamSys reads the runtime power state from sysfs first and only queries NVML "
                            "when the GPU is already awake. Polling NVML would itself wake the card and cost "
                            "several watts continuously.")
            ng.add(self.row("Runtime power state", n.get("runtime_status", "unknown"),
                            "queried live via NVML" if n.get("live") else "read from sysfs without waking the GPU"))
            ng.add(self.row("Driver", n.get("driver_version") or "unknown"))
            ng.add(self.row("Awake this boot", human_duration(n.get("active_time_s", 0)),
                            f"suspended {human_duration(n.get('suspended_time_s', 0))}"))
            if n.get("live"):
                ng.add(self.row("Utilisation", pct(n.get("util_pct"))))
                ng.add(self.row("Power draw", watts(n.get("power_w"))))
                ng.add(self.row("Temperature", temp(n.get("temp_c"))))
                ng.add(self.row("Performance state", f"P{n.get('pstate')}" if n.get("pstate") is not None else "—"))
                ng.add(self.row("SM clock", f"{n.get('sm_clock_mhz', 0):.0f} MHz" if n.get("sm_clock_mhz") else "—"))
                used, total = n.get("vram_used_mb"), n.get("vram_total_mb")
                if used is not None and total:
                    ng.add(self.row("Video memory", f"{used:.0f} MB of {total:.0f} MB ({100*used/total:.0f}%)"))
                ng.add(self.row("GPU processes", str(n.get("process_count", 0))))
            else:
                ng.add(self.row("Live metrics", "not collected",
                                "The GPU is asleep. Utilisation, temperature and power are reported as zero "
                                "because the card is powered down, not because they are unknown."))
            if g.get("dgpu_awake_idle"):
                w = Adw.ActionRow(title="The discrete GPU is awake with nothing using it",
                                  subtitle="This costs battery. Open Events to see when it woke up.")
                ic = Gtk.Image.new_from_icon_name("dialog-warning-symbolic")
                ic.add_css_class("sv-attention")
                w.add_prefix(ic)
                ng.add(w)
            self.body.append(ng)
            self.body.append(self.win.chart("gpu", "power_w", "nvidia", "NVIDIA power draw", "W"))
        else:
            self.body.append(self.group("NVIDIA discrete", "Not present on this machine."))


class PowerPage(Page):
    title, icon = "Power", "battery-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        p = s.get("power", {})
        self.clear()
        if not p.get("has_battery"):
            self.body.append(self.group("Battery", "No battery is present on this machine."))
            return
        g = self.group("Battery")
        g.add(self.row("Charge", f"{p.get('percent', 0):.0f}%", p.get("status", "")))
        g.add(self.row("Power flow", f"{abs(p.get('power_w', 0)):.1f} W",
                       "discharging" if p.get("on_battery") else ("charging" if p.get("power_w", 0) < 0 else "on AC")))
        rt = p.get("runtime_s")
        g.add(self.row("Estimated remaining", human_duration(rt) if rt else "—",
                       "at the current rate"))
        g.add(self.row("Voltage", f"{p.get('voltage_v', 0):.2f} V"))
        g.add(self.row("Energy", f"{p.get('energy_now_wh', 0):.1f} Wh of {p.get('energy_full_wh', 0):.1f} Wh"))
        g.add(self.row("Health", f"{p.get('health_pct', 0):.1f}%",
                       f"full charge {p.get('energy_full_wh', 0):.1f} Wh against a design capacity of "
                       f"{p.get('energy_design_wh', 0):.1f} Wh · {p.get('cycle_count', 0)} cycles"))
        g.add(self.row("AC adapter", "connected" if p.get("ac_online") else "not connected"))
        cw = p.get("cpu_package_w")
        g.add(self.row("CPU package power", watts(cw) if cw is not None else "unavailable",
                       "" if cw is not None else "needs the privileged helper (RAPL is root-only)"))
        self.body.append(g)
        self.body.append(self.win.chart("power", "system_w", "", "System power draw", "W", band_metric="power.system_w"))


class ThermalPage(Page):
    title, icon = "Thermals", "temperature-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        t = s.get("thermal", {})
        self.clear()
        g = self.group("Temperatures", "Sensors are identified by driver and label, so they stay "
                                       "correctly attributed across kernel upgrades.")
        for r in sorted(t.get("temps", []), key=lambda x: -x["value"]):
            crit = f"critical at {r['crit']:.0f} °C" if r.get("crit") else ""
            g.add(self.row(r["key"], f"{r['value']:.1f} °C", crit))
        self.body.append(g)
        fg = self.group("Fans")
        fans = t.get("fans", [])
        if fans:
            for f in fans:
                fg.add(self.row(f["key"], f"{f['value']:.0f} rpm",
                                "stopped" if f["value"] == 0 else ""))
        else:
            fg.add(self.row("Fan tachometers", "none exposed by this hardware"))
        self.body.append(fg)
        self.body.append(self.win.chart("thermal", "cpu_package_c", "", "CPU package temperature", "°C"))


class StoragePage(Page):
    title, icon = "Storage", "drive-harddisk-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        st = s.get("storage", {})
        self.clear()
        g = self.group("Filesystems",
                       f"{st.get('hidden_mounts', 0)} pseudo, read-only and snap mounts are hidden — "
                       "they are always full by design and would drown out real warnings.")
        for f in st.get("filesystems", []):
            row = Adw.ActionRow(title=GLib.markup_escape_text(f["mount"]),
                                subtitle=f"{f['fstype']} on {f['device']} · {human_bytes(f['free_bytes'])} free of {human_bytes(f['total_bytes'])}")
            bar = Gtk.LevelBar(min_value=0, max_value=100, value=f["used_pct"])
            bar.set_size_request(140, 8)
            bar.set_valign(Gtk.Align.CENTER)
            lbl = Gtk.Label(label=f"{f['used_pct']:.0f}%")
            lbl.add_css_class("dim-label")
            row.add_suffix(bar)
            row.add_suffix(lbl)
            g.add(row)
        self.body.append(g)

        ig = self.group("Activity")
        for d in st.get("disks", []):
            ig.add(self.row(d["device"],
                            f"↓ {human_bps(d['read_bps'])}  ↑ {human_bps(d['write_bps'])}",
                            f"{d['util_pct']:.0f}% utilised · {d['read_iops']+d['write_iops']:.0f} IOPS · "
                            f"{d['avg_latency_ms']:.1f} ms mean service time"))
        self.body.append(ig)

        for n in st.get("nvme", []):
            ng = self.group(f"{n['device']} — {n['model'].strip()}")
            ng.add(self.row("Firmware", n.get("firmware", "—")))
            ng.add(self.row("Temperature", temp(n.get("temp_c"))))
            if n.get("smart_available"):
                ng.add(self.row("SMART critical warning", f"0x{n.get('critical_warning', 0):02x}"))
                ng.add(self.row("Endurance used", f"{n.get('percentage_used', 0)}%"))
                ng.add(self.row("Available spare", f"{n.get('available_spare', 0)}%",
                                f"threshold {n.get('available_spare_threshold', 0)}%"))
                ng.add(self.row("Media errors", str(n.get("media_errors", 0))))
                ng.add(self.row("Unsafe shutdowns", str(n.get("unsafe_shutdowns", 0))))
                ng.add(self.row("Power-on hours", str(n.get("power_on_hours", 0))))
                duw = n.get("data_units_written")
                if duw:
                    ng.add(self.row("Data written", human_bytes(duw * 512_000)))
            else:
                r = Adw.ActionRow(title="SMART health data unavailable",
                                  subtitle="The NVMe admin interface is root-only. Enable the optional "
                                           "read-only helper to see wear, spare blocks and media errors:  "
                                           "sudo systemctl enable --now jamsys-helper.timer   "
                                           "Temperature above is read unprivileged through hwmon "
                                           "and is unaffected.")
                ic = Gtk.Image.new_from_icon_name("dialog-information-symbolic")
                r.add_prefix(ic)
                ng.add(r)
            self.body.append(ng)

        errs = st.get("fs_error_counts") or {}
        if errs:
            eg = self.group("Filesystem error counters")
            for dev, n in errs.items():
                eg.add(self.row(dev, str(n), "ext4 recorded errors since the last fsck"))
            self.body.append(eg)


class NetworkPage(Page):
    title, icon = "Network", "network-wireless-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        n = s.get("network", {})
        self.clear()
        r = n.get("reach", {})

        def verdict(v):
            return "reachable" if v is True else ("unreachable" if v is False else "not checked")

        g = self.group("Connectivity", "Probes are bare TCP connects with no payload sent. "
                                       "They can be turned off entirely in Settings.")
        g.add(self.row("Default route", n.get("default_route") or "none",
                       f"via {n.get('default_iface') or '—'}"))
        g.add(self.row("Gateway", verdict(r.get("gateway_ok")), r.get("gateway") or ""))
        g.add(self.row("DNS", verdict(r.get("dns_ok"))))
        g.add(self.row("Internet", verdict(r.get("internet_ok"))))
        g.add(self.row("Established TCP connections", str(n.get("tcp_connections", 0))))
        self.body.append(g)

        for i in n.get("ifaces", []):
            title = f"{i['name']} — {i['kind']}"
            ig = self.group(title, i.get("driver", ""))
            ig.add(self.row("State", i.get("operstate", "?"),
                            "carrier present" if i.get("carrier") else "no carrier"))
            if i.get("ipv4") or i.get("ipv6"):
                ig.add(self.row("Addresses", ", ".join(i.get("ipv4", []) + i.get("ipv6", []))))
            ig.add(self.row("Throughput", f"↓ {human_bps(i.get('rx_bps', 0))}  ↑ {human_bps(i.get('tx_bps', 0))}"))
            ig.add(self.row("Errors and drops", f"{i.get('error_rate_pct', 0):.3f}%",
                            f"rx {i.get('rx_errors',0)}/{i.get('rx_dropped',0)} · "
                            f"tx {i.get('tx_errors',0)}/{i.get('tx_dropped',0)} (errors/drops, cumulative)"))
            if i.get("signal_dbm") is not None:
                q = i.get("link_quality")
                ig.add(self.row("Signal", f"{i['signal_dbm']:.0f} dBm",
                                f"link quality {q:.0f}" if q else ""))
            if i.get("speed_mbps"):
                ig.add(self.row("Link speed", f"{i['speed_mbps']} Mbit/s"))
            self.body.append(ig)
            if i["kind"] != "loopback":
                self.body.append(self.win.chart("network", "rx_bps", i["name"], f"{i['name']} download", "B/s"))

        self.body.append(self.win.event_timeline("network", "Network event timeline"))


class DevicesPage(Page):
    title, icon = "Devices", "drive-removable-media-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        d = s.get("devices", {})
        self.clear()
        bt = d.get("bluetooth", {})
        g = self.group("Bluetooth")
        if bt.get("present"):
            g.add(self.row("Adapter", bt.get("adapter", "—"), bt.get("address", "")))
            g.add(self.row("State", "blocked" if (bt.get("soft_blocked") or bt.get("hard_blocked")) else "available",
                           "hardware switch" if bt.get("hard_blocked") else ("software block" if bt.get("soft_blocked") else "")))
            g.add(self.row("Connected devices", str(bt.get("connected_devices", 0))))
        else:
            g.add(self.row("Bluetooth", "no adapter found"))
        self.body.append(g)

        a = d.get("audio", {})
        ag = self.group("Audio")
        ag.add(self.row("PipeWire", a.get("pipewire", "unknown")))
        ag.add(self.row("WirePlumber", a.get("wireplumber", "unknown")))
        ag.add(self.row("pipewire-pulse", a.get("pipewire_pulse", "unknown")))
        ag.add(self.row("Restarts observed", str(a.get("restarts_seen", 0)),
                        "a restart is what a user experiences as audio cutting out"))
        for c in a.get("cards", []):
            ag.add(self.row("Sound card", c))
        self.body.append(ag)

        cg = self.group("Cameras")
        cg.add(self.row("Webcam", "present" if d.get("webcam_present") else "not found"))
        cg.add(self.row("Infrared camera", "present" if d.get("ir_camera_present") else "not found"))
        for c in d.get("cameras", []):
            cg.add(self.row("Device", c))
        self.body.append(cg)

        ug = self.group("USB devices")
        for u in d.get("usb", []):
            ug.add(self.row(u.get("name") or u["id"], u["id"], u.get("vendor", "")))
        self.body.append(ug)

        missing = d.get("missing_expected") or []
        mg = self.group("Expected hardware",
                        "Devices seen consistently become 'expected'. A single plug-in does not qualify, "
                        "so a USB stick you used once will not be reported as missing forever.")
        if missing:
            for m in missing:
                mg.add(severity_row(2, f"{m} is missing", "This device is normally present"))
        else:
            mg.add(self.row("Status", "all expected devices present"))
        self.body.append(mg)


class ServicesPage(Page):
    title, icon = "Services", "system-run-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        sv = s.get("services", {})
        self.clear()
        failed = sv.get("failed", []) + sv.get("failed_user", [])
        g = self.group("Failed units")
        if not failed:
            g.add(self.row("Status", "no failed units"))
        for u in failed:
            row = Adw.ActionRow(title=GLib.markup_escape_text(u["name"]),
                                subtitle=GLib.markup_escape_text(u.get("description", "")))
            ic = Gtk.Image.new_from_icon_name("dialog-warning-symbolic")
            ic.add_css_class("sv-attention")
            row.add_prefix(ic)
            btn = Gtk.Button(label="Ignore this service", valign=Gtk.Align.CENTER)
            btn.add_css_class("flat")
            btn.connect("clicked", lambda _b, name=u["name"]: self.win.ignore_service(name))
            row.add_suffix(btn)
            g.add(row)
        self.body.append(g)

        ign = sv.get("ignored") or []
        if ign:
            ig = self.group("Ignored units", "Still tracked, never alerted on. Remove an entry in Settings.")
            for name in ign:
                ig.add(self.row(name, "ignored"))
            self.body.append(ig)

        rc = sv.get("restart_counts") or {}
        if rc:
            rg = self.group("Recent failures", "Counted over the last hour.")
            for k, v in sorted(rc.items(), key=lambda x: -x[1]):
                rg.add(self.row(k, f"{v}×"))
            self.body.append(rg)


class SecurityPage(Page):
    """What this machine exposes to the network, and who has been failing to get in.

    Detection only. JamSys cannot block anything and does not try to: the page says
    what is listening, whether a firewall is on, and how many sign-ins have failed,
    and for each of those what was actually checked and what could not be.
    """

    title, icon = "Security", "security-high-symbolic"

    EXPOSURE = {"all": "all network interfaces", "address": "one network address",
                "loopback": "this machine only"}

    def __init__(self, win):
        super().__init__(win)
        self._shown = None
        self.firewall = FirewallControl()
        self._switching = False

    def update(self, snap):
        # Rebuilt only when something on it changed: it has buttons, and a button
        # replaced between press and release loses the click.
        sec = (snap.get("snapshot") or {}).get("security")
        if self._shown is not None and sec == self._shown[0] \
                and int(time.time() // 60) == self._shown[1]:
            return
        super().update(snap)

    def render(self, snap):
        sec = (snap.get("snapshot") or {}).get("security")
        self._shown = (sec, int(time.time() // 60))
        self.clear()
        if sec is None:
            self.body.append(self.group(
                "Security",
                "The running daemon predates this page. Restart it to pick up the "
                "installed version:  systemctl --user restart jamsysd"))
            return

        self._checkup(sec)
        self._firewall(sec.get("firewall") or {}, sec.get("exposed_tcp", 0))
        self._sign_ins(sec)
        listeners = sec.get("listeners") or []
        tcp = [l for l in listeners if l.get("proto") == "tcp"]
        self._listeners(
            "Reachable from the network",
            "Programs accepting TCP connections from other devices. Anything that "
            "appears here after JamSys first ran raises an alert until you accept it.",
            [l for l in tcp if l.get("exposure") != "loopback"],
            "Nothing is accepting connections from the network.", judge=True)
        self._listeners(
            "This machine only",
            "Listening on the loopback address: other programs on this computer can "
            "connect, other devices cannot.",
            [l for l in tcp if l.get("exposure") == "loopback"],
            "Nothing is listening locally.")
        self._listeners(
            "UDP",
            "Sockets waiting for datagrams (name lookup, discovery, calls). Listed for "
            "reference and not judged: applications open and close these constantly.",
            [l for l in listeners if l.get("proto") == "udp"],
            "No UDP sockets are waiting.")

        g = self.group(
            "What this page cannot see",
            "It is built from what an unprivileged program may read, and no more.")
        g.add(self.row("Firewall rules", "", "Only whether a firewall is switched on. "
                       "The rules need root:  sudo ufw status verbose"))
        g.add(self.row("Other users' programs", "", "A service running as root is shown "
                       "by its account, not its name. To name it:  sudo ss -ltnp"))
        g.add(self.row("Short-lived connections", "", "Sockets are sampled about once a "
                       "minute; something that listens for a few seconds can be missed."))
        g.add(self.row("Traffic contents", "", "Never inspected, by design."))
        self.body.append(g)

    # -- sections -----------------------------------------------------------

    CHECK_ICONS = {"ok": ("emblem-ok-symbolic", "sv-healthy"),
                   "warn": ("dialog-warning-symbolic", "sv-attention"),
                   "note": ("dialog-information-symbolic", "sv-idle"),
                   "unknown": ("dialog-question-symbolic", "sv-unknown")}

    def _checkup(self, sec):
        """The short answer first: a handful of settings, each right or not."""
        checks = sec.get("checks") or []
        if not checks:
            return
        failing = [c for c in checks if c.get("status") == "warn"]
        g = self.group(
            "Checkup",
            ("Everything that can be checked from here is as it should be."
             if not failing else
             f"{len(failing)} thing(s) worth fixing, most important first. Each says "
             "what to do; nothing here is changed without you."))
        order = {"warn": 0, "note": 1, "unknown": 2, "ok": 3}
        for c in sorted(checks, key=lambda c: order.get(c.get("status"), 2)):
            row = Adw.ActionRow(title=GLib.markup_escape_text(c.get("title", "")),
                                subtitle=GLib.markup_escape_text(c.get("detail", "")))
            icon, css = self.CHECK_ICONS.get(c.get("status"), self.CHECK_ICONS["unknown"])
            ic = Gtk.Image.new_from_icon_name(icon)
            ic.add_css_class(css)
            row.add_prefix(ic)
            fix = c.get("fix")
            if fix and c.get("id") == "firewall" and self.firewall.available():
                row.add_suffix(self._firewall_button(True))
            elif fix:
                cmd = Gtk.Label(label=fix, selectable=True)
                cmd.add_css_class("sv-mono")
                row.add_suffix(cmd)
                copy = Gtk.Button(icon_name="edit-copy-symbolic", valign=Gtk.Align.CENTER,
                                  tooltip_text="Copy this command")
                copy.add_css_class("flat")
                copy.connect("clicked", lambda _b, text=fix: (
                    self.get_clipboard().set(text),
                    self.win.toasts.add_toast(Adw.Toast(
                        title="Copied — paste it into a terminal", timeout=3))))
                row.add_suffix(copy)
            g.add(row)
        self.body.append(g)

    def _firewall_button(self, on: bool) -> Gtk.Button:
        b = Gtk.Button(label="Turn on" if on else "Turn off", valign=Gtk.Align.CENTER,
                       sensitive=not self._switching)
        b.set_tooltip_text(
            "Runs `ufw enable` after asking for an administrator password. Incoming "
            "connections are refused unless allowed; outgoing ones are unaffected."
            if on else
            "Runs `ufw disable` after asking for an administrator password.")
        if on:
            b.add_css_class("suggested-action")
        else:
            b.add_css_class("flat")
        b.connect("clicked", lambda _b: self._switch_firewall(on))
        return b

    def _switch_firewall(self, on: bool):
        if self._switching:
            return
        self._switching = True
        self._shown = None
        self.update(self.win.last_snapshot or {})

        def done(ok, err):
            self._switching = False
            if getattr(self.win, "_closed", False):
                return
            if ok and not err:
                self.win.toasts.add_toast(Adw.Toast(
                    title=("Firewall turned on. Incoming connections are now refused "
                           "unless you allow them." if on else "Firewall turned off."),
                    timeout=5))
            else:
                self.win.toasts.add_toast(Adw.Toast(
                    title=f"Firewall not changed: {err or self.firewall.last_error}",
                    timeout=7))
            # The daemon re-reads the setting on its next pass; redraw then, and
            # now, so the button is usable again either way.
            self._shown = None
            if self.win.current_page() is self:
                self.update(self.win.last_snapshot or {})
            self.win.refresh()
        run_async(lambda: self.firewall.set_enabled(on), done)

    def _firewall(self, fw, exposed):
        state = fw.get("state", "unknown")
        g = self.group("Firewall")
        title = {"enabled": f"{fw.get('tool', 'A firewall')} is on",
                 "disabled": f"{fw.get('tool', 'The firewall')} is switched off",
                 }.get(state, "Could not tell whether a firewall is on")
        row = Adw.ActionRow(title=GLib.markup_escape_text(title),
                            subtitle=GLib.markup_escape_text(fw.get("detail", "")))
        icon, css = {"enabled": ("emblem-ok-symbolic", "sv-healthy"),
                     "disabled": ("dialog-warning-symbolic", "sv-attention"),
                     }.get(state, ("dialog-question-symbolic", "sv-unknown"))
        ic = Gtk.Image.new_from_icon_name(icon)
        ic.add_css_class(css)
        row.add_prefix(ic)
        # Reversible from the same place: what can be switched on here can be
        # switched off here.
        if fw.get("tool") == "ufw" and state in ("enabled", "disabled") \
                and self.firewall.available():
            row.add_suffix(self._firewall_button(state == "disabled"))
        g.add(row)
        if state == "disabled":
            g.add(self.row(
                "Or from a terminal", "sudo ufw enable",
                (f"{exposed} program(s) below are reachable from the network right now. "
                 if exposed else "Nothing is reachable from the network right now. ")
                + "Outgoing connections are not affected."))
        self.body.append(g)

    def _sign_ins(self, sec):
        short, long_ = sec.get("auth_failures_10m", 0), sec.get("auth_failures_1h", 0)
        g = self.group(
            "Failed sign-ins",
            "Wrong passwords for sudo, the login and lock screens, su, polkit prompts "
            "and SSH, as the system logs them.")
        if not sec.get("auth_log_watched"):
            g.add(self.row("Not being counted", "",
                           "The sign-in log could not be followed. Reading it needs "
                           "membership of the adm or systemd-journal group."))
            self.body.append(g)
            return
        g.add(self.row("Last ten minutes", str(short),
                       "an alert is raised at the threshold set in Settings"))
        g.add(self.row("Last hour", str(long_)))
        now = time.time() * 1000
        for f in (sec.get("recent_failures") or [])[:10]:
            who = f.get("service", "?")
            if f.get("user"):
                who += f" as {f['user']}"
            g.add(severity_row(
                2 if f.get("source") else 1, who,
                f"{clock_hm(f.get('ts_ms'))} · {human_ago(f.get('ts_ms'), now)} · "
                + (f"from {f['source']}" if f.get("source") else "at this computer")))
        self.body.append(g)

    def _listeners(self, title, description, items, empty, judge=False):
        g = self.group(title, description)
        if not items:
            g.add(Adw.ActionRow(title=empty))
        for l in items:
            name = l.get("process") or l.get("owner") or "?"
            sub = (f"{str(l.get('proto', '')).upper()} · "
                   f"{self.EXPOSURE.get(l.get('exposure'), l.get('exposure'))} "
                   f"({', '.join(l.get('addresses') or [])})")
            if l.get("process"):
                sub += f" · pid {l.get('pid')} · runs as {l.get('owner')}"
            else:
                sub += f" · runs as {l.get('owner')}; its name needs root to read"
            row = Adw.ActionRow(
                title=GLib.markup_escape_text(f"Port {l.get('port')} — {name}"),
                subtitle=GLib.markup_escape_text(sub))
            new = judge and l.get("expected") is False
            ic = Gtk.Image.new_from_icon_name(
                "dialog-warning-symbolic" if new else "network-server-symbolic")
            ic.add_css_class("sv-attention" if new else "dim-label")
            row.add_prefix(ic)
            if new:
                tag = Gtk.Label(label="New")
                tag.add_css_class("sv-attention")
                row.add_suffix(tag)
                b = Gtk.Button(label="Accept", valign=Gtk.Align.CENTER,
                               tooltip_text="This is meant to be here: stop alerting about it")
                b.connect("clicked", lambda _b, k=l.get("key"): self.win.accept_listener(k))
                row.add_suffix(b)
            g.add(row)
        self.body.append(g)


class ProcessPage(Page):
    title, icon = "Processes", "utilities-system-monitor-symbolic"

    def __init__(self, win):
        super().__init__(win)
        self._sample = {}
        self._vram = {}
        self._gpu_time_note = ""
        # Keep controls outside the rebuilt results so typing, selection and
        # keyboard focus survive incoming snapshots.
        tools = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self.search = Gtk.SearchEntry(placeholder_text="Filter top processes by name or PID",
                                      hexpand=True)
        self.search.set_tooltip_text("Search the top CPU and memory samples reported by the daemon")
        self.search.connect("search-changed", self._filter_changed)
        tools.append(self.search)
        self.sort = Gtk.DropDown.new_from_strings(["CPU usage", "Memory usage", "Name"])
        self.sort.set_tooltip_text("Sort processes")
        self.sort.connect("notify::selected", self._filter_changed)
        tools.append(self.sort)
        self.body.append(tools)
        self.results = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.body.append(self.results)

    def is_interacting(self):
        # The search entry is persistent. It can keep focus while readings update.
        return _has_open_popover(self)

    def _filter_changed(self, *_):
        self._scroller.get_vadjustment().set_value(0)
        self._render_results()

    def render(self, snap):
        s = snap.get("snapshot") or {}
        self._sample = s.get("process") or {}
        gpu = s.get("gpu") or {}
        nv = gpu.get("nvidia") or {}
        # NVML reports video memory per process, as [pid, bytes] pairs.
        self._vram = {}
        for row in nv.get("processes") or []:
            if isinstance(row, (list, tuple)) and len(row) == 2:
                self._vram[row[0]] = row[1]
        # GPU *time* per process comes from DRM fdinfo, which the Intel driver
        # fills in and NVIDIA's does not. When the NVIDIA card is the one drawing
        # the desktop there is simply no such figure -- and every row used to
        # show nothing, which read as "nothing is using the GPU".
        self._gpu_time_note = (
            " GPU time per process is not shown: the NVIDIA card is drawing the "
            "desktop and its driver does not publish it. NVIDIA video memory is "
            "shown where the driver reports it."
            if gpu.get("display_driver") == "NVIDIA" or nv.get("drives_display") else "")
        self._render_results()

    def _render_results(self):
        child = self.results.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self.results.remove(child)
            child = nxt
        sort = ("cpu", "memory", "name")[min(self.sort.get_selected(), 2)]
        rows = select_processes(self._sample, self.search.get_text(), sort)
        total = self._sample.get("total")
        g = self.group("Top processes",
                       f"{len(rows)} shown · {total if total is not None else '?'} total. "
                       "Combined top CPU and memory samples; this is not the full process list. "
                       "CPU is per core and can exceed 100%." + self._gpu_time_note)
        self.results.append(g)
        if not rows:
            g.add(self.row("No matching processes" if self.search.get_text().strip()
                           else "No process samples available", "",
                           "Try another name or PID." if self.search.get_text().strip()
                           else "Check that the process collector is enabled on Coverage."))
        for q in rows:
            sub = (f"PID {q['pid']} · {q.get('threads', '?')} threads · "
                   f"CPU {q.get('cpu_pct', 0):.1f}% · {human_bytes(q.get('rss_bytes', 0))}")
            if q.get("read_bps") or q.get("write_bps"):
                sub += f" · I/O ↓{human_bps(q.get('read_bps', 0))} ↑{human_bps(q.get('write_bps', 0))}"
            if q.get("gpu_ns_per_s"):
                sub += f" · GPU {q['gpu_ns_per_s']/1e9:.2f} engine-s/s"
            if self._vram.get(q.get("pid")):
                sub += f" · NVIDIA memory {human_bytes(self._vram[q['pid']])}"
            row = self.row(str(q.get("name", "?")),
                           human_bytes(q.get("rss_bytes", 0)) if sort == "memory"
                           else f"{q.get('cpu_pct', 0):.1f}%", sub)
            if q.get("flag"):
                ic = Gtk.Image.new_from_icon_name("dialog-warning-symbolic")
                ic.add_css_class("sv-attention")
                ic.set_tooltip_text(q["flag"])
                row.add_prefix(ic)
            g.add(row)


class HardwarePage(Page):
    """Hardware controls. Read-only monitoring is elsewhere; this page *changes* things,
    so every action here is explicit and reversible."""

    title, icon = "Hardware", "input-keyboard-symbolic"

    def __init__(self, win):
        super().__init__(win)
        self.kbd = KeyboardControl()
        self.charge = ChargeLimitControl()
        # kbd_rgb_mode is write-only in the kernel, so there is nothing to read back.
        # These remember what we last sent; the page says as much rather than
        # presenting a remembered value as if it were measured.
        self._colour = (255, 255, 255)
        self._mode = 0
        self._speed = 1
        self._sent_anything = False
        # kbd_rgb_state is write-only too. These start at the firmware's own
        # default and then follow what was sent, so a switch the user turned on
        # is not shown as off again by the next refresh.
        self._states = {"boot": False, "awake": True, "sleep": False, "keyboard": True}
        self._rendered_for = None
        self._scale = self._brightness_row = None
        self._scale_handler = 0
        # One helper invocation per kind at a time, newest request wins.
        self._sending: dict[str, bool] = {}
        self._queued: dict[str, tuple] = {}

    def _inputs(self, snap):
        """Everything this page is built from, except the brightness reading.

        The rest of the page is controls, and controls must not be rebuilt
        while nothing they depend on has changed: the two-second refresh was
        replacing the slider in the middle of a drag, discarding the colour
        picker's button while its dialog was open, and resetting the
        power-state switches to their defaults. Brightness is the one live
        reading here and is updated in place instead.
        """
        s = snap.get("snapshot", {})
        k = dict(s.get("keyboard", {}))
        k.pop("brightness", None)
        p = s.get("power", {})
        return (sorted(k.items()), p.get("has_battery"),
                p.get("charge_limit_supported"), p.get("charge_limit_pct"),
                self.charge.available())

    def update(self, snap):
        if self._rendered_for is None or self._inputs(snap) != self._rendered_for:
            super().update(snap)
            return
        self._show_brightness((snap.get("snapshot", {}).get("keyboard", {})
                               .get("brightness", 0)))

    def _show_brightness(self, level):
        if self._brightness_row is None:
            return
        self._brightness_row.set_subtitle(f"currently {level}")
        busy = self._sending.get("brightness") or "brightness" in self._queued
        # Follow a change made elsewhere (the Fn key), but never pull the
        # slider out from under a hand that is on it or a write still in flight.
        if (not busy and not self._scale.has_focus()
                and int(round(self._scale.get_value())) != int(level)):
            self._scale.handler_block(self._scale_handler)
            self._scale.set_value(float(level))
            self._scale.handler_unblock(self._scale_handler)

    def render(self, snap):
        s = snap.get("snapshot", {})
        k = s.get("keyboard", {})
        self._rendered_for = self._inputs(snap)
        self._scale = self._brightness_row = None
        self.clear()

        # Battery care comes first: it is the control with a lasting effect, and it
        # is useful even on a machine with no lit keyboard at all.
        self._render_charge_limit(s.get("power", {}))

        if not k.get("present"):
            g = self.group("Keyboard lighting",
                           "No ASUS keyboard backlight was found on this machine.")
            self.body.append(g)
            return

        control = k.get("control", "none")
        maxb = int(k.get("max_brightness") or 0)

        # -- availability -------------------------------------------------
        avail = self.group("Keyboard lighting", self._control_description(control))
        row = Adw.ActionRow(title="Control path",
                            subtitle=self._control_subtitle(control))
        ic = Gtk.Image.new_from_icon_name(
            "emblem-ok-symbolic" if control != "none" else "action-unavailable-symbolic")
        ic.add_css_class("sv-healthy" if control != "none" else "sv-unknown")
        row.add_prefix(ic)
        avail.add(row)
        avail.add(self.row("Kernel field order (mode)", k.get("rgb_mode_fields") or "—",
                           "read from kbd_rgb_mode_index; the helper writes exactly this order"))
        if k.get("state_capable"):
            avail.add(self.row("Kernel field order (state)", k.get("rgb_state_fields") or "—"))
        self.body.append(avail)

        enabled = control != "none"

        # -- brightness ---------------------------------------------------
        bg = self.group("Brightness", f"0 to {maxb} on this keyboard.")
        brow = Adw.ActionRow(title="Backlight level",
                             subtitle=f"currently {k.get('brightness', 0)}")
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, max(maxb, 1), 1)
        scale.set_value(float(k.get("brightness", 0)))
        scale.set_draw_value(True)
        scale.set_size_request(220, -1)
        scale.set_valign(Gtk.Align.CENTER)
        scale.set_sensitive(enabled)
        for i in range(maxb + 1):
            scale.add_mark(i, Gtk.PositionType.BOTTOM, str(i))
        scale.update_property([Gtk.AccessibleProperty.LABEL],
                              ["Keyboard backlight level"])
        self._scale_handler = scale.connect("value-changed", self._on_brightness, control)
        self._scale, self._brightness_row = scale, brow
        brow.add_suffix(scale)
        bg.add(brow)
        self.body.append(bg)

        if not k.get("rgb_capable"):
            self.body.append(self.group(
                "Colour", "This keyboard has a backlight but no addressable RGB."))
            if not enabled:
                self.body.append(self._how_to_enable())
            return

        # -- colour -------------------------------------------------------
        cg = self.group("Colour", self._readback_note())
        crow = Adw.ActionRow(title="Custom colour")
        try:
            dlg = Gtk.ColorDialog(with_alpha=False, title="Keyboard colour")
            btn = Gtk.ColorDialogButton(dialog=dlg, valign=Gtk.Align.CENTER)
            btn.set_rgba(ints_to_rgba(*self._colour))
            btn.connect("notify::rgba", self._on_colour, control)
        except TypeError:
            # Older GTK without ColorDialogButton; fall back rather than crash.
            btn = Gtk.Button(label="Colour picker unavailable", sensitive=False)
        btn.set_sensitive(enabled)
        crow.add_suffix(btn)
        cg.add(crow)

        prow = Adw.ActionRow(title="Presets")
        pbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6,
                       valign=Gtk.Align.CENTER)
        for name, rgb in PRESETS:
            b = Gtk.Button(label=name)
            b.add_css_class("pill")
            b.set_sensitive(enabled)
            b.connect("clicked", self._on_preset, rgb, control)
            pbox.append(b)
        prow.add_suffix(pbox)
        cg.add(prow)
        self.body.append(cg)

        # -- effect -------------------------------------------------------
        eg = self.group("Effect")
        mrow = Adw.ComboRow(title="Mode",
                            model=Gtk.StringList.new([m[0] for m in MODES]))
        mrow.set_selected(self._mode)
        mrow.set_sensitive(enabled)
        mrow.connect("notify::selected", self._on_mode, control)
        eg.add(mrow)

        srow = Adw.ComboRow(title="Speed",
                            subtitle="Applies to breathing, cycle and strobe",
                            model=Gtk.StringList.new([x[0] for x in SPEEDS]))
        srow.set_selected(self._speed)
        srow.set_sensitive(enabled)
        srow.connect("notify::selected", self._on_speed, control)
        eg.add(srow)
        self.body.append(eg)

        # -- when the lighting is on --------------------------------------
        if k.get("state_capable"):
            sg = self.group("When the keyboard is lit",
                            "Which power states keep the lighting on. Written together, "
                            "in the kernel's field order: boot, awake, sleep, keyboard. "
                            "The kernel cannot report these back, so the switches show "
                            "what JamSys last set, or the firmware default.")
            self._state_switches = {}
            for key, title in [("boot", "During boot"), ("awake", "While awake"),
                               ("sleep", "While asleep"), ("keyboard", "Keyboard lighting")]:
                r = Adw.SwitchRow(title=title)
                r.set_active(self._states[key])
                r.set_sensitive(enabled)
                r.connect("notify::active", self._on_state, control)
                self._state_switches[key] = r
                sg.add(r)
            self.body.append(sg)

        if not enabled:
            self.body.append(self._how_to_enable())

    # -- descriptions -----------------------------------------------------

    def _render_charge_limit(self, power):
        """Battery charge limit: stop charging early so the pack ages more slowly."""
        if not power.get("charge_limit_supported"):
            if power.get("has_battery"):
                g = self.group(
                    "Battery care",
                    "This battery does not expose charge_control_end_threshold, so "
                    "the charge limit cannot be set from software.")
                self.body.append(g)
            return

        current = power.get("charge_limit_pct")
        installed = self.charge.available()
        desc = ("Stop charging at a set percentage and run from the charger beyond "
                "it. Keeping a lithium cell at 100% is what ages it fastest; 80% is "
                "the usual compromise between longevity and usable runtime.")
        if not installed:
            desc += ("\n\nThe privileged helper is not installed, so this is "
                     "read-only. Install it with:\n"
                     f"    sudo {repo_script('install-privileged.sh')}")
        g = self.group("Battery care", desc)

        row = Adw.ActionRow(
            title="Charge limit",
            subtitle=("charging stops at this level"
                      if (current or 100) < 100 else "charging normally, to 100%"))
        choices = list(CHARGE_LIMITS)
        values = [v for _, v in choices]
        # A limit set by another tool need not be one of ours. A dropdown always
        # shows *something* selected, so without its own entry the firmware's 75
        # would be displayed as the first item in the list, 60.
        if isinstance(current, int) and current not in values:
            choices.append((f"{current}%  (set elsewhere)", current))
            values.append(current)
        combo = Gtk.DropDown.new_from_strings([label for label, _ in choices])
        if current in values:
            combo.set_selected(values.index(current))
        combo.set_valign(Gtk.Align.CENTER)
        combo.set_sensitive(installed)
        combo.update_property([Gtk.AccessibleProperty.LABEL], ["Battery charge limit"])
        combo.connect("notify::selected", self._on_charge_limit, values, current)
        row.add_suffix(combo)
        g.add(row)

        g.add(self.row(
            "Reported by the firmware",
            f"{current}%" if current is not None else "—",
            "read from charge_control_end_threshold; reading needs no privilege"))
        if current is not None and current < 100:
            g.add(self.row(
                "While plugged in above this level", "runs from the charger",
                "the battery neither charges nor discharges"))
        self.body.append(g)

    def _on_charge_limit(self, combo, _param, values, current):
        idx = combo.get_selected()
        if idx == Gtk.INVALID_LIST_POSITION or idx >= len(values):
            return
        want = values[idx]
        if want == current:
            return
        combo.set_sensitive(False)

        # pkexec blocks for as long as the password dialog is open. Run on the
        # main thread, that froze the whole window behind the prompt.
        def done(ok, err):
            if getattr(self.win, "_closed", False):
                return
            if ok and not err:
                # The firmware is the source of truth: say what was asked for and
                # let the next snapshot report what actually took effect.
                self.win.toasts.add_toast(Adw.Toast(
                    title=f"Charge limit set to {want}%", timeout=3))
            else:
                self.win.toasts.add_toast(Adw.Toast(
                    title=f"Charge limit: {err or self.charge.last_error or 'could not set it'}",
                    timeout=6))
            # Rebuild so the list shows the firmware's value again rather than a
            # choice that was refused.
            self._rendered_for = None
            if self.win.current_page() is self:
                self.update(self.win.last_snapshot or {})
        run_async(lambda: self.charge.set_limit(want), done)

    def _control_description(self, control):
        if control == "helper":
            return ("Changes go through jamsys-kbd, a ~200-line privileged helper with "
                    "no input except three range-checked integers. The interface itself "
                    "never runs as root.")
        if control == "direct":
            return ("A udev rule has made the lighting attributes writable by your user, "
                    "so no privileged code is involved at all.")
        # Saying "no control path is installed" told the user nothing they could
        # act on. Give the exact command, with the absolute path, because the
        # relative one only works from inside the source tree.
        return ("The lighting attributes are root-only (kbd_rgb_mode is mode 0200) "
                "and no write path is installed, so everything below is read-only.\n\n"
                "Install the helper once — this is the whole fix:\n"
                f"    sudo {repo_script('install-privileged.sh')}\n"
                "    systemctl --user restart jamsysd\n\n"
                "It installs two small validated binaries and their Polkit actions. "
                "Nothing runs as root afterwards.")

    def _control_subtitle(self, control):
        return {"helper": "jamsys-kbd via Polkit",
                "direct": "direct sysfs write (udev rule installed)",
                "none": "not available"}.get(control, control)

    def _readback_note(self):
        base = ("The kernel exposes kbd_rgb_mode as write-only, so the current colour "
                "cannot be read back from the hardware. ")
        return base + ("This shows what JamSys last set."
                       if self._sent_anything
                       else "Nothing has been set yet this session, so this is a default.")

    def _how_to_enable(self):
        g = self.group("Enabling control",
                       "Pick one. The helper is the default; the udev rule involves no "
                       "privileged code at all but grants broader access.")
        g.add(self.row("Privileged helper",
                       "sudo apt install ./jamsys_1.0.0_amd64.deb",
                       "installs /usr/libexec/jamsys-kbd and its Polkit action"))
        g.add(self.row("No privileged code",
                       "sudo install -m0644 99-jamsys-keyboard.rules /etc/udev/rules.d/",
                       "then: sudo udevadm control --reload && sudo udevadm trigger -s leds"))
        return g

    # -- actions ----------------------------------------------------------

    def _apply(self, ok: bool, what: str):
        if getattr(self.win, "_closed", False):
            return
        if ok:
            first = not self._sent_anything
            self._sent_anything = True
            self.win.toasts.add_toast(Adw.Toast(title=f"{what} applied", timeout=2))
            if first:
                # The colour note changes from "a default" to "what JamSys set".
                self._rendered_for = None
        else:
            self.win.toasts.add_toast(
                Adw.Toast(title=f"{what} failed: {self.kbd.last_error}", timeout=6))

    def _send(self, kind: str, fn, what: str):
        """Run one helper call per kind at a time; the newest request wins.

        Dragging the slider from 0 to 3 emits 1, 2 and 3. Each used to start its
        own pkexec, and whichever finished last decided the brightness -- not
        necessarily the one the slider stopped on.
        """
        if self._sending.get(kind):
            self._queued[kind] = (fn, what)
            return
        self._sending[kind] = True

        def done(ok, err):
            self._sending[kind] = False
            nxt = self._queued.pop(kind, None)
            if nxt is not None:
                # Superseded: report only the request that is still wanted.
                self._send(kind, *nxt)
                return
            self._apply(bool(ok) and not err, what)
        run_async(fn, done)

    def _on_brightness(self, scale, control):
        v = int(round(scale.get_value()))
        self._send("brightness", lambda: self.kbd.set_brightness(v, control),
                   f"Brightness {v}")

    def _push_rgb(self, control):
        r, g, b = self._colour
        mode, speed = self._mode, self._speed
        self._send("rgb", lambda: self.kbd.set_rgb(mode, r, g, b, speed, control),
                   "Lighting")

    def _on_colour(self, btn, _p, control):
        self._colour = rgba_to_ints(btn.get_rgba())
        self._push_rgb(control)

    def _on_preset(self, _b, rgb, control):
        self._colour = rgb
        self._push_rgb(control)

    def _on_mode(self, row, _p, control):
        self._mode = _selected(MODES, row, self._mode)
        self._push_rgb(control)

    def _on_speed(self, row, _p, control):
        self._speed = _selected(SPEEDS, row, self._speed)
        self._push_rgb(control)

    def _on_state(self, _row, _p, control):
        for key, sw in self._state_switches.items():
            self._states[key] = sw.get_active()
        st = dict(self._states)
        self._send("state", lambda: self.kbd.set_states(
                       st["boot"], st["awake"], st["sleep"], st["keyboard"], control),
                   "Lighting states")


class ReportPage(Page):
    """Everything worth acting on, in one place, in the order to act on it.

    The other pages answer "what is this subsystem doing?". This answers the
    question you have when something is actually wrong, and produces the same
    thing as text you can hand to someone else.
    """

    title, icon = "Report", "text-x-generic-symbolic"

    def __init__(self, win):
        super().__init__(win)
        self._summary = None
        self._pending = False
        self._fetched_at = 0.0

    def render(self, snap):
        self.clear()
        self._fetch()

        s = self._summary
        if s is None:
            g = self.group("Report", "Collecting from the daemon…")
            self.body.append(g)
            return

        headline = {
            "healthy": ("No open alerts in available monitoring data", "sv-healthy"),
            "attention": (f"{len(s['alerts'])} thing(s) need attention", "sv-warn"),
            "critical": (f"{len(s['alerts'])} thing(s) need attention, "
                         "including a critical one", "sv-critical"),
            "unknown": ("Report incomplete — health could not be confirmed", "dim-label"),
        }[s["verdict"]]

        top = self.group("Verdict")
        row = Adw.ActionRow(title=GLib.markup_escape_text(headline[0]))
        icon = Gtk.Image.new_from_icon_name(
            "emblem-ok-symbolic" if s["verdict"] == "healthy"
            else "dialog-warning-symbolic")
        icon.add_css_class(headline[1])
        row.add_prefix(icon)
        copy = Gtk.Button(label="Copy report", valign=Gtk.Align.CENTER)
        copy.connect("clicked", self._copy)
        row.add_suffix(copy)
        save = Gtk.Button(label="Save…", valign=Gtk.Align.CENTER)
        save.connect("clicked", self._save)
        row.add_suffix(save)
        top.add(row)
        m = s["machine"]
        top.add(self.row("Machine", f"{m['model']}", f"kernel {m['kernel']}"))
        top.add(self.row("Collectors usable", m["collectors"]))
        self.body.append(top)

        if s.get("missing"):
            g = self.group("Unavailable data", "This report is incomplete. Retry when monitoring is available.")
            for section, reason in s["missing"].items():
                g.add(self.row(section, "", reason))
            self.body.append(g)

        if s["alerts"]:
            g = self.group("Open problems",
                           "Most serious first. Each one says what was measured, "
                           "what was expected, and what to try.")
            for a in s["alerts"]:
                d = a.get("detail") or {}
                r = Adw.ExpanderRow(
                    title=GLib.markup_escape_text(a.get("title", "Untitled")),
                    subtitle=GLib.markup_escape_text(
                        report.SEVERITY.get(a.get("severity", 0), "?")
                        + " · " + report._age(a.get("first_ts"))))
                if d.get("what"):
                    r.add_row(self.row("What", "", d["what"]))
                if d.get("expected"):
                    r.add_row(self.row("Expected", str(d["expected"])))
                for ev in d.get("evidence") or []:
                    r.add_row(self.row(str(ev.get("label", "")), str(ev.get("value", ""))))
                if d.get("likely_cause"):
                    r.add_row(self.row("Likely cause", "", str(d["likely_cause"])))
                for act in d.get("actions") or []:
                    r.add_row(self.row("Try", "", str(act)))
                g.add(r)
            self.body.append(g)

        if s["risks"]:
            g = self.group("Configuration worth changing",
                           "Not faults, but known causes of problems you would "
                           "otherwise spend a long time chasing.")
            for note in s["risks"]:
                g.add(self.row("Risk", "", note))
            self.body.append(g)

        if s["gaps"]:
            g = self.group("Not being watched",
                           "An absent sensor explains a missing alert as much as "
                           "a present one explains a firing alert.")
            for c in s["gaps"]:
                g.add(self.row(str(c.get("name", "?")), str(c.get("label", "?")),
                               str(c.get("detail") or c.get("reason") or "")))
            self.body.append(g)

        if s["events"]:
            g = self.group("Recent events")
            for e in s["events"][:10]:
                g.add(self.row(report._ts(e.get("ts")),
                               str(e.get("kind", "")),
                               (e.get("summary") or "")[:160]))
            self.body.append(g)

    # -- data ---------------------------------------------------------

    def _fetch(self):
        """Pull the extra views the report needs, then re-render once.

        "Once" needs enforcing. The re-render at the end of a fetch calls
        straight back into here, and `_pending` is already False by then, so
        without an age check this is an unbounded loop: render, fetch, render,
        fetch, as fast as the daemon can answer. Measured at 1101 renders in
        five seconds -- a busy core, held for as long as the page is open, on
        the one page whose job is to tell you what is using your machine. It
        also never lets the page finish being laid out.

        So: no more often than the window refreshes anyway. A failed fetch
        leaves `_summary` None and is retried immediately.
        """
        if self._pending or not self.win.client.connected:
            return
        if (self._summary is not None
                and time.monotonic() - self._fetched_at < REFRESH_MS / 1000.0):
            return
        self._pending = True
        self._fetched_at = time.monotonic()

        def work():
            return report.collect(self.win.client.call)

        def done(res, err):
            self._pending = False
            if getattr(self.win, "_closed", False) or err or not res:
                return
            self._summary = res
            # Throttle from completion, otherwise a slow fetch immediately
            # starts another fetch when its result is rendered.
            self._fetched_at = time.monotonic()
            if self.win.current_page() is self:
                self.update(self.win.last_snapshot or {})

        run_async(work, done)

    # -- actions ------------------------------------------------------

    def _markdown(self) -> str:
        if self._summary is None:
            return "JamSys report: still collecting."
        return report.to_markdown(self._summary)

    def _copy(self, _btn):
        text = self._markdown()
        self.get_clipboard().set(text)
        self.win.toasts.add_toast(Adw.Toast(
            title=f"Report copied ({len(text.splitlines())} lines)", timeout=3))

    def _save(self, _btn):
        name = datetime.datetime.now().strftime("jamsys-report-%Y%m%d-%H%M.md")
        dlg = Gtk.FileDialog(initial_name=name)

        def finish(d, res):
            try:
                f = d.save_finish(res)
            except GLib.Error:
                return              # the user cancelled; not an error
            try:
                path = f.get_path()
                with open(path, "w", encoding="utf-8") as fh:
                    fh.write(self._markdown())
                self.win.toasts.add_toast(Adw.Toast(title=f"Saved to {path}", timeout=5))
            except OSError as e:
                self.win.toasts.add_toast(Adw.Toast(title=f"Could not save: {e}", timeout=6))

        dlg.save(self.win, None, finish)


class DiagnosticsPage(AsyncPage):
    """What the monitoring application costs. A monitor that will not report its own
    overhead is asking to be taken on trust."""

    title, icon = "Diagnostics", "speedometer-symbolic"

    def fetch(self, _request):
        call = self.win.client.call
        return call("stats"), call("ping")

    def build(self, data):
        d, ping = data
        daemon_g = self.group(
            "Monitoring daemon",
            "Measured live from /proc/self. If these numbers look wrong, they are wrong "
            "about the thing you should trust least: this application.")
        store_g = self.group("History store")
        widget_g = self.group("GNOME Shell readout")
        ui_g = self.group("This window")
        for g in (daemon_g, store_g, widget_g, ui_g):
            self.area.append(g)

        up = max(d.get("uptime_s", 1), 1)
        cpu = d.get("cpu_seconds", 0)
        daemon_g.add(self.row("Version", ping.get("version", "?"),
                              f"protocol {ping.get('schema')} · pid {ping.get('pid')}"))
        daemon_g.add(self.row("Uptime", human_duration(up)))
        daemon_g.add(self.row("CPU used", f"{cpu:.2f} s",
                              f"{100*cpu/up:.3f}% of one core since start"))
        daemon_g.add(self.row("Memory", human_bytes(d.get("rss_bytes", 0)),
                              f"{d.get('threads', 0)} threads"))
        daemon_g.add(self.row("Wakeups", f"{d.get('wakeups', 0):,}",
                              f"{d.get('wakeups_per_s', 0):.2f} per second"))
        daemon_g.add(self.row("Scheduler ticks", f"{d.get('ticks', 0):,}"))
        frac = d.get("idle_tick_fraction", 0) or 0
        blocked = d.get("idle_blocked_by") or ""
        daemon_g.add(self.row("Reduced sampling",
                              f"{100*frac:.0f}% of ticks",
                              (f"currently off: {blocked}" if blocked
                               else "currently active")))
        daemon_g.add(self.row("Sampling rate",
                              f"×{d.get('sampling_multiplier', 1):.0f} interval multiplier",
                              "3× while idle on battery with no window open"))
        daemon_g.add(self.row("Activity state", d.get("activity", "?"),
                              f"baseline context: {d.get('context','?')}"))
        daemon_g.add(self.row("Events processed", f"{d.get('events_processed', 0):,}",
                              f"{d.get('events_dropped', 0):,} dropped"))
        daemon_g.add(self.row("Notifications rate-limited",
                              str(d.get("suppressed_notifications", 0))))

        store_g.add(self.row("Database on disk", human_bytes(d.get("db_bytes", 0)),
                             f"{d.get('sample_rows',0):,} ten-second samples · "
                             f"{d.get('rollup_rows',0):,} rollups · "
                             f"{d.get('event_rows',0):,} events"))
        store_g.add(self.row("Full-resolution buffer",
                             human_bytes(d.get("live_bytes", 0)),
                             f"{d.get('live_points', 0):,} points held in memory only"))
        store_g.add(self.row("Pending writes", str(d.get("samples_pending", 0)),
                             "flushed in one transaction every 15 s"))
        store_g.add(self.row("Learned baselines", f"{d.get('baselines', 0)} series"))

        has = d.get("dbus_widget", False)
        widget_g.add(self.row("D-Bus service",
                              "org.jamsys.Daemon" if has else "unavailable",
                              "" if has else "no session bus when the daemon started"))
        widget_g.add(self.row("Repaints pushed", f"{d.get('dbus_signals', 0):,}",
                              "only when a displayed value changed enough to matter"))
        widget_g.add(self.row("Method calls served", f"{d.get('dbus_calls', 0):,}"))

        ui_g.add(self.row("Memory", human_bytes(_self_rss()),
                          "this window; the daemon keeps running when it closes"))
        ui_g.add(self.row("IPC clients attached", str(d.get("ipc_clients", 0))))


class EventsPage(AsyncPage):
    title, icon = "Events", "document-open-recent-symbolic"

    FILTERS = [("All", "all"), ("Alerts", "alerts"), ("Warnings and above", "warn")]

    def __init__(self, win):
        super().__init__(win)
        self.filter = "all"
        # Built once and kept: recreating the buttons on every refresh lost
        # keyboard focus, and as ungrouped toggles they could all be switched
        # off, leaving a list with no visible filter selected.
        bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=0)
        bar.add_css_class("linked")
        bar.set_halign(Gtk.Align.START)
        first = None
        for label, key in self.FILTERS:
            b = Gtk.ToggleButton(label=label, active=(self.filter == key))
            if first is None:
                first = b
            else:
                b.set_group(first)
            b.connect("toggled", self._set_filter, key)
            bar.append(b)
        self.body.prepend(bar)

    def _set_filter(self, btn, key):
        if btn.get_active() and self.filter != key:
            self.filter = key
            self.reload()

    def request(self):
        return self.filter

    def fetch(self, flt):
        call = self.win.client.call
        if flt == "alerts":
            return [("alert", a) for a in
                    call("alerts", open_only=False, limit=200).get("alerts", [])]
        evs = call("events", limit=300).get("events", [])
        if flt == "warn":
            evs = [e for e in evs if e["severity"] >= 2]
        return [("event", e) for e in evs]

    def identity(self, data):
        # Alert rows print a relative time, which goes stale without new data.
        return (data, int(time.time() // 60))

    def build(self, res):
        group = self.group("History")
        self.area.append(group)
        now = time.time() * 1000
        if not res:
            group.add(Adw.ActionRow(title={
                "alerts": "No alerts have been raised",
                "warn": "No warnings or critical events recorded",
            }.get(self.filter, "Nothing recorded yet")))
            return
        for kind, it in res[:300]:
            if kind == "alert":
                sub = (f"{clock_hm(it['last_ts'])} · {human_ago(it['last_ts'], now)}"
                       f" · fired {it['count']}×")
                if it.get("resolved_ts"):
                    sub += " · resolved"
                row = Adw.ExpanderRow(title=GLib.markup_escape_text(it["title"]), subtitle=sub)
                lbl = Gtk.Label(xalign=0, label=it.get("explanation", ""), selectable=True, wrap=True)
                lbl.add_css_class("sv-mono")
                for m in ("top", "bottom", "start", "end"):
                    getattr(lbl, f"set_margin_{m}")(10)
                holder = Adw.ActionRow()
                holder.set_child(lbl)
                row.add_row(holder)
                row.add_suffix(self.win.alert_menu(it))
            else:
                row = severity_row(it["severity"], it["summary"],
                                   f"{clock_hm(it['ts'])} · {it['subsystem']} · {it['kind']}")
            group.add(row)


class CoveragePage(AsyncPage):
    title, icon = "Coverage", "checkbox-checked-symbolic"

    # Run counters and timings move on every tick. They are worth showing but
    # not worth rebuilding a page of switches under the pointer for.
    max_age = 15.0

    LABELS = {"Full": ("emblem-ok-symbolic", "sv-healthy"),
              "Partial": ("dialog-information-symbolic", "sv-idle"),
              "Unavailable": ("action-unavailable-symbolic", "sv-unknown"),
              "Quarantined": ("dialog-error-symbolic", "sv-critical"),
              "Disabled": ("action-unavailable-symbolic", "sv-unknown")}

    def fetch(self, _request):
        call = self.win.client.call
        return call("coverage"), call("inventory"), call("inventory", changes=True)

    def identity(self, data):
        cov, inv, chg = data
        stable = [(c.get("name"), c.get("label"), c.get("tier"), str(c.get("support")))
                  for c in cov.get("collectors", [])]
        flags = [cov.get(k) for k in ("journal", "netlink_route", "netlink_uevent",
                                      "helper_installed")]
        return (stable, flags, inv, chg)

    def build(self, data):
        cov, inv, chg = data
        self.area.append(self.group(
            "Monitoring coverage",
            "A monitor that quietly fails to monitor something is worse than no monitor, "
            "because you believe you are covered. Everything JamSys can and cannot see "
            "on this machine is listed here."))
        group = self.group("Collectors",
                           "Switching a collector off stops it being sampled and is "
                           "remembered across restarts.")
        inv_group = self.group("Hardware inventory")
        chg_group = self.group("Changes since first run",
                               "Useful after a kernel or driver update: what actually changed.")
        for g in (group, inv_group, chg_group):
            self.area.append(g)

        for c in cov.get("collectors", []):
            lab = c["label"]
            sup = c.get("support", {})
            detail = sup.get("detail") or sup.get("reason") or ""
            sub = f"{c['tier']} tier · {c['runs']} runs · {c['avg_us']} µs average"
            if detail:
                sub = detail + " · " + sub
            row = Adw.ActionRow(title=GLib.markup_escape_text(c["name"]),
                                subtitle=GLib.markup_escape_text(sub))
            icon, css = self.LABELS.get(lab, self.LABELS["Unavailable"])
            ic = Gtk.Image.new_from_icon_name(icon)
            ic.add_css_class(css)
            row.add_prefix(ic)
            val = Gtk.Label(label=lab)
            val.add_css_class(css)
            row.add_suffix(val)
            sw = Gtk.Switch(active=(lab != "Disabled"), valign=Gtk.Align.CENTER)
            sw.set_tooltip_text(f"Sample the {c['name']} collector")
            sw.update_property([Gtk.AccessibleProperty.LABEL],
                               [f"Enable the {c['name']} collector"])
            sw.connect("state-set", self.win._toggle_collector, c["name"])
            row.add_suffix(sw)
            row.set_activatable_widget(sw)
            group.add(row)
        for name, ok, why in [
            ("Journal (kernel errors, Xid, OOM)", cov.get("journal"), "journalctl could not be started"),
            ("Network change events (rtnetlink)", cov.get("netlink_route"), "netlink socket unavailable"),
            ("Device hotplug events (uevent)", cov.get("netlink_uevent"), "netlink socket unavailable"),
            ("Privileged helper (RAPL, NVMe SMART)", cov.get("helper_installed"),
             "not installed — enable it with: sudo systemctl enable --now jamsys-helper.timer"),
        ]:
            row = Adw.ActionRow(title=name, subtitle="" if ok else why)
            ic = Gtk.Image.new_from_icon_name("emblem-ok-symbolic" if ok else "action-unavailable-symbolic")
            ic.add_css_class("sv-healthy" if ok else "sv-unknown")
            row.add_prefix(ic)
            row.add_suffix(Gtk.Label(label="Available" if ok else "Unavailable"))
            group.add(row)

        for it in inv.get("items", []):
            inv_group.add(self.row(it["key"], it["value"]))
        changes = chg.get("changes", [])
        if not changes:
            chg_group.add(Adw.ActionRow(title="Nothing has changed since JamSys was installed"))
        for c in changes[:80]:
            old = c.get("old")
            chg_group.add(self.row(
                c["key"], c["new"],
                (f"was {old} · " if old else "first seen · ") + clock_hm(c["ts"])))


class SettingsPage(AsyncPage):
    title, icon = "Settings", "preferences-system-symbolic"

    def fetch(self, _request):
        call = self.win.client.call
        try:
            thresholds = call("thresholds")
        except DaemonError:
            # A daemon from before this operation existed is still running.
            # That must cost one section, not the whole page.
            thresholds = None
        return call("suppressions"), thresholds, call("ping")

    def build(self, data):
        sup, thresholds, ping = data
        g = self.group("Notifications")
        row = Adw.ActionRow(
            title="Desktop notifications",
            subtitle="Sent by the daemon, so they work with this window closed. "
                     "The minimum severity is set in ~/.config/jamsys/config.toml.")
        g.add(row)
        self.area.append(g)

        sg = self.group("Active suppressions",
                        "Muted alerts, snoozes and ignored rules or devices. All reversible.")
        items = sup.get("suppressions", [])
        if not items:
            sg.add(Adw.ActionRow(title="Nothing is suppressed"))
        for s in items:
            until = s.get("until_ts")
            sub = f"{s['scope']} · " + ("permanent" if not until else f"until {clock_hm(until)}")
            if s.get("reason"):
                sub += f" · {s['reason']}"
            r = Adw.ActionRow(title=GLib.markup_escape_text(s["pattern"]),
                              subtitle=GLib.markup_escape_text(sub))
            b = Gtk.Button(label="Remove", valign=Gtk.Align.CENTER)
            b.add_css_class("flat")
            b.connect("clicked", lambda _w, i=s["id"]: self.win._unsuppress(i))
            r.add_suffix(b)
            sg.add(r)
        self.area.append(sg)

        tg = self.group("Alert thresholds",
                        "The level at which each rule fires. A changed value is shown "
                        "with its default, and takes effect immediately.")
        if thresholds is None:
            tg.add(self.row(
                "Not available from the running daemon", "",
                "It predates this feature. Restart it to pick up the installed "
                "version:  systemctl --user restart jamsysd"))
        else:
            for t in thresholds.get("thresholds", []):
                self._threshold_row(tg, t)
        self.area.append(tg)

        dg = self.group("Daemon")
        dg.add(self.row("Version", ping.get("version", "?"),
                        f"protocol {ping.get('schema')} · its running cost is on the "
                        "Diagnostics page"))
        self.area.append(dg)

    def _threshold_row(self, group, t):
        value, default = t["value"], t["default"]
        changed = value != default
        sub = f"{t['rule_id']} · {t['key']}"
        if changed:
            sub += f" · default {default:g}"
        row = Adw.ActionRow(title=GLib.markup_escape_text(t.get("label") or t["rule_id"]),
                            subtitle=GLib.markup_escape_text(sub))
        val = Gtk.Label(label=f"{value:g} {t.get('unit', '')}".strip())
        val.add_css_class("dim-label" if not changed else "sv-idle")
        row.add_suffix(val)
        edit = Gtk.Button(icon_name="document-edit-symbolic", valign=Gtk.Align.CENTER,
                          tooltip_text="Change this threshold")
        edit.add_css_class("flat")
        edit.connect("clicked", lambda *_: self.win._threshold_dialog(t["rule_id"], t["key"]))
        row.add_suffix(edit)
        if changed:
            reset = Gtk.Button(icon_name="edit-undo-symbolic", valign=Gtk.Align.CENTER,
                               tooltip_text=f"Back to the default ({default:g})")
            reset.add_css_class("flat")
            reset.connect("clicked", lambda *_: self.win._set_threshold(
                t["rule_id"], t["key"], default))
            row.add_suffix(reset)
        group.add(row)


# ---------------------------------------------------------------------------
# Window
# ---------------------------------------------------------------------------

PAGES = [OverviewPage, CpuPage, MemoryPage, GpuPage, PowerPage, ThermalPage,
         StoragePage, NetworkPage, SecurityPage, DevicesPage, ServicesPage, ProcessPage,
         HardwarePage, ReportPage, EventsPage, CoveragePage, DiagnosticsPage,
         SettingsPage]


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="JamSys", default_width=1080, default_height=760)
        self.client = Client(on_push=self._on_push, on_state=self._on_state)
        self.last_snapshot: Optional[dict] = None
        self._alert_cache: list[dict] = []
        self._charts: dict[tuple, list] = {}
        self._busy = False
        self._connecting = False
        self._events_busy = False
        self._closed = False
        self._last_updated = None

        self.split = Adw.NavigationSplitView()
        self.set_content(self.split)

        # Sidebar
        self.listbox = Gtk.ListBox()
        self.listbox.add_css_class("navigation-sidebar")
        self.listbox.connect("row-selected", self._on_nav)
        self.pages: dict[str, Page] = {}
        for cls in PAGES:
            p = cls(self)
            self.pages[cls.title] = p
            row = Gtk.ListBoxRow()
            b = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
            b.set_margin_top(7); b.set_margin_bottom(7)
            b.set_margin_start(6); b.set_margin_end(6)
            b.append(Gtk.Image.new_from_icon_name(cls.icon))
            b.append(Gtk.Label(label=cls.title, xalign=0))
            row.set_child(b)
            row.page_title = cls.title
            self.listbox.append(row)

        sb_scroll = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER, vexpand=True)
        sb_scroll.set_child(self.listbox)
        sb_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        sb_head = Adw.HeaderBar()
        sb_head.set_title_widget(Adw.WindowTitle(title="JamSys"))
        sb_box.append(sb_head)
        sb_box.append(sb_scroll)
        self.conn_pill = StatusPill("unknown", "Connecting…")
        self.conn_pill.set_margin_start(12)
        self.conn_pill.set_margin_end(12)
        self.conn_pill.set_margin_top(6)
        self.conn_pill.set_margin_bottom(10)
        sb_box.append(self.conn_pill)
        self.split.set_sidebar(Adw.NavigationPage(child=sb_box, title="JamSys"))

        # Content
        self.content_head = Adw.HeaderBar()
        self.content_title = Adw.WindowTitle(title="Overview")
        self.content_head.set_title_widget(self.content_title)
        self.refresh_button = Gtk.Button(icon_name="view-refresh-symbolic",
                                         tooltip_text="Refresh now (Ctrl+R)")
        self.refresh_button.connect("clicked", lambda *_: self.refresh())
        self.content_head.pack_end(self.refresh_button)
        action = Gio.SimpleAction.new("refresh", None)
        action.connect("activate", lambda *_: self.refresh())
        self.add_action(action)
        app.set_accels_for_action("win.refresh", ["<Control>r", "F5"])
        self.stack = Gtk.Stack(transition_type=Gtk.StackTransitionType.CROSSFADE,
                               transition_duration=120)
        for t, p in self.pages.items():
            self.stack.add_named(p, t)
        cbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        cbox.append(self.content_head)
        self.connection_banner = Adw.Banner(title="Connecting to the monitoring service…",
                                           button_label="Retry", revealed=True, use_markup=False)
        self.connection_banner.connect("button-clicked", lambda *_: self._connect_async())
        self.connection_banner.set_tooltip_text("Start the service: systemctl --user start jamsysd")
        cbox.append(self.connection_banner)
        self.toasts = Adw.ToastOverlay()
        self.toasts.set_child(self.stack)
        cbox.append(self.toasts)
        self.split.set_content(Adw.NavigationPage(child=cbox, title="Overview"))

        self.listbox.select_row(self.listbox.get_row_at_index(0))
        self._tick_source = GLib.timeout_add(REFRESH_MS, self._tick)
        self.connect("close-request", self._on_close)
        self._connect_async()

    # -- connection ------------------------------------------------------

    def _connect_async(self):
        if self._connecting or self._closed:
            return
        self._connecting = True

        def work():
            if not self.client.connect():
                return False
            # subscribe waits for IPC; it must not run in the GTK completion callback.
            self.client.subscribe(["alert", "event"])
            return True

        def done(ok, err):
            self._connecting = False
            if self._closed:
                return
            if ok:
                self.refresh()
            elif err:
                self._on_state(False, str(err))
        run_async(work, done)

    def _on_close(self, *_):
        self._closed = True
        if self._tick_source:
            GLib.source_remove(self._tick_source)
            self._tick_source = None
        self.client.shutdown()
        return False

    def _on_state(self, ok: bool, msg: str):
        if self._closed:
            return False
        self.conn_pill.set("healthy" if ok else "critical",
                           "Connected" if ok else "Service unavailable")
        self.refresh_button.set_sensitive(ok and not self._busy)
        self.connection_banner.set_revealed(True)
        self.connection_banner.set_title(
            "Connected — waiting for fresh readings…" if ok else
            "Monitoring is offline. Start it with: systemctl --user start jamsysd")
        self.connection_banner.set_tooltip_text(msg)
        if not ok:
            self.content_title.set_subtitle(
                f"Last update {self._last_updated} · offline" if self._last_updated
                else "No live readings")
            self.pages["Overview"].banner_text.set_text("Cannot reach the monitoring service")
            self.pages["Overview"].banner_sub.set_text(
                msg + "\nStart it with:  systemctl --user start jamsysd")
            for c in ("sv-banner-healthy", "sv-banner-attention", "sv-banner-critical", "sv-banner-unknown"):
                self.pages["Overview"].banner.remove_css_class(c)
            self.pages["Overview"].banner.add_css_class("sv-banner-unknown")
        return False

    def _on_push(self, topic: str, data: dict):
        if self._closed:
            return False
        if topic == "alert":
            # The daemon only pushes a frame for a genuinely new or escalated alert, so
            # this cannot repeat for a condition the user has already seen.
            sev = data.get("severity", 0)
            if sev >= 2:
                self.toasts.add_toast(Adw.Toast(title=data.get("title", "Alert"), timeout=6))
            self.refresh_events()
        elif topic == "event":
            self.refresh_events()
        return False

    def _tick(self):
        if self._closed:
            return False
        if self.client.connected:
            self.refresh()
        else:
            self._connect_async()
        return True

    # -- data ------------------------------------------------------------

    def refresh(self):
        if self._closed or self._busy or not self.client.connected:
            return
        self._busy = True
        self.refresh_button.set_sensitive(False)

        def work():
            return self.client.call("snapshot")

        def done(res, err):
            self._busy = False
            if self._closed:
                return
            self.refresh_button.set_sensitive(self.client.connected)
            if err or res is None:
                self.connection_banner.set_title(f"Could not refresh readings: {err}")
                self.connection_banner.set_revealed(True)
                return
            if not self.client.connected:
                return
            self.last_snapshot = res
            self._last_updated = datetime.datetime.now().strftime("%H:%M:%S")
            self.content_title.set_subtitle(f"Updated {self._last_updated}")
            self.connection_banner.set_revealed(False)
            page = self.current_page()
            if page:
                self._render(page, res, user=False)
            if isinstance(page, OverviewPage):
                self.refresh_events()
        run_async(work, done)

    def refresh_events(self):
        page = self.current_page()
        if (self._closed or self._events_busy or not isinstance(page, OverviewPage)
                or not self.client.connected):
            return
        self._events_busy = True

        def work():
            alerts = self.client.call("alerts", open_only=False, limit=25).get("alerts", [])
            events = self.client.call("events", limit=25).get("events", [])
            return alerts, events

        def done(res, err):
            self._events_busy = False
            if self._closed or err or not res:
                return
            alerts, events = res
            self._alert_cache = alerts
            merged = []
            for a in alerts:
                merged.append({
                    "ts": a["last_ts"], "severity": a["severity"],
                    "title": a["title"] + ("" if a.get("resolved_ts") is None else "  (resolved)"),
                    "summary": a["title"], "rendered": a.get("explanation", ""),
                    "fingerprint": a.get("fingerprint"), "rule_id": a.get("rule_id"),
                    "count": a.get("count", 1), "id": a.get("id"),
                })
            for e in events:
                merged.append({"ts": e["ts"], "severity": e["severity"],
                               "title": e["summary"], "summary": e["summary"]})
            merged.sort(key=lambda x: -x["ts"])
            page.set_events(merged)
        run_async(work, done)

    # -- charts and timelines ---------------------------------------------

    #: How long a fetched series or timeline is reused. Pages are rebuilt every
    #: REFRESH_MS, and each rebuild used to create an empty chart and query an
    #: hour of history for it: the plot blinked on every tick and the daemon
    #: answered five times more history requests than the picture could show.
    HISTORY_TTL_S = 10.0

    def _cached(self, key, fetch, apply):
        """Show the last reply for `key` at once; refresh it in the background.

        `apply(result)` runs on the main thread, immediately if a reply is
        cached and again when a fresh one arrives.
        """
        hit = self._charts.get(key)
        if hit is not None:
            apply(hit[1])
            if hit[2] or time.monotonic() - hit[0] < self.HISTORY_TTL_S:
                return
            # [fetched_at, result, refresh_in_flight]
            hit[2] = True

        def done(res, err):
            if self._closed:
                return
            if err or res is None:
                if hit is not None:
                    hit[2] = False
                return
            self._charts[key] = [time.monotonic(), res, False]
            if hit is None:
                apply(res)
        run_async(fetch, done)

    def chart(self, subsystem: str, name: str, instance: str, title: str, unit: str,
              band_metric: Optional[str] = None) -> Gtk.Widget:
        g = Adw.PreferencesGroup(title=GLib.markup_escape_text(title))
        spark = Sparkline(height=110)
        spark.set_margin_top(8)
        spark.set_margin_bottom(8)
        spark.set_margin_start(10)
        spark.set_margin_end(10)
        row = Adw.ActionRow()
        row.set_child(spark)
        g.add(row)
        caption = Gtk.Label(xalign=0, label="last hour")
        caption.add_css_class("caption")
        caption.add_css_class("dim-label")
        crow = Adw.ActionRow()
        crow.set_child(caption)
        g.add(crow)

        def work():
            until = int(time.time() * 1000)
            return self.client.call("history", subsystem=subsystem, name=name,
                                    instance=instance, since_ms=until - 3_600_000,
                                    until_ms=until, max_points=240)

        def show(res):
            pts = [p[1] for p in res.get("points", [])]
            spark.set_points(pts)
            if pts:
                text = (f"last hour · now {pts[-1]:,.1f} {unit} · min {min(pts):,.1f} · "
                        f"max {max(pts):,.1f} · {len(pts)} points at "
                        + ({0: "full resolution", 60000: "1-minute", 300000: "5-minute",
                            900000: "15-minute"}.get(res.get("bucket", 0), "aggregated")))
            else:
                text = "no history yet — the daemon has just started"
            caption.set_text(text)
            # The plot itself is a picture; this is what a screen reader gets.
            spark.update_property([Gtk.AccessibleProperty.LABEL], [f"{title}: {text}"])
        self._cached(("history", subsystem, name, instance), work, show)
        return g

    def event_timeline(self, subsystem: str, title: str) -> Gtk.Widget:
        g = Adw.PreferencesGroup(title=GLib.markup_escape_text(title))

        def work():
            return self.client.call("events", subsystem=subsystem, limit=40).get("events", [])

        def show(res):
            if not res:
                g.add(Adw.ActionRow(title="No events recorded for this subsystem"))
                return
            for e in res:
                g.add(severity_row(e["severity"], f"{clock_hm(e['ts'])}   {e['summary']}", e["kind"]))
        self._cached(("events", subsystem), work, show)
        return g

    # -- alert actions -----------------------------------------------------

    def alert_menu(self, alert: dict) -> Gtk.Widget:
        btn = Gtk.MenuButton(icon_name="view-more-symbolic", valign=Gtk.Align.CENTER)
        btn.add_css_class("flat")
        pop = Gtk.Popover()
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        for m in ("top", "bottom", "start", "end"):
            getattr(box, f"set_margin_{m}")(6)
        fp = alert.get("fingerprint", "")
        rule = alert.get("rule_id", "")
        instance = fp.split(":", 1)[1] if ":" in fp else ""

        def add(label, cb):
            b = Gtk.Button(label=label)
            b.add_css_class("flat")
            b.set_halign(Gtk.Align.FILL)
            b.get_child().set_xalign(0)
            b.connect("clicked", lambda *_: (pop.popdown(), cb()))
            box.append(b)

        add("Mute this alert", lambda: self._suppress("fingerprint", fp, None, "muted"))
        for label, hours in [("Snooze for 1 hour", 1), ("Snooze for 8 hours", 8), ("Snooze for 24 hours", 24)]:
            add(label, lambda h=hours: self._suppress(
                "fingerprint", fp, int((time.time() + h * 3600) * 1000), f"snoozed {h}h"))
        add("Ignore this rule permanently", lambda: self._suppress("rule", rule, None, "rule ignored"))
        if instance:
            add(f"Ignore “{instance}”", lambda: self._suppress("instance", instance, None, "device or service ignored"))
        add("Change threshold…", lambda: self._threshold_dialog(rule))
        # A plain Gtk.Popover full of buttons has no arrow-key navigation of its
        # own, so say what the button is and let Tab walk the choices.
        btn.set_tooltip_text("Mute, snooze, ignore or change the threshold")
        pop.set_child(box)
        btn.set_popover(pop)
        return btn

    def _suppress(self, scope, pattern, until, reason):
        if not pattern:
            return

        def work():
            return self.client.call("suppress", scope=scope, pattern=pattern,
                                    until_ms=until, reason=reason)

        def done(_r, err):
            if err:
                self.toasts.add_toast(Adw.Toast(title=f"Could not apply: {err}"))
            else:
                self.toasts.add_toast(Adw.Toast(title=f"{reason.capitalize()}: {pattern}"))
                self._reload_current()
        run_async(work, done)

    def _unsuppress(self, sid: int):
        def work():
            return self.client.call("unsuppress", id=sid)

        def done(_r, err):
            self.toasts.add_toast(Adw.Toast(title="Suppression removed" if not err else f"Failed: {err}"))
            self._reload_current()
        run_async(work, done)

    def _reload_current(self):
        """Redraw the visible page after an action that changed what it lists."""
        page = self.current_page()
        if isinstance(page, AsyncPage):
            page.reload(force=True)
        elif isinstance(page, OverviewPage):
            self.refresh_events()
        elif page:
            self._render(page, self.last_snapshot or {}, user=False)

    def accept_listener(self, key: str):
        def work():
            return self.client.call("accept_listener", key=key)

        def done(_r, err):
            if self._closed:
                return
            self.toasts.add_toast(Adw.Toast(
                title=f"Accepted {key}" if not err else f"Could not accept {key}: {err}",
                timeout=3 if not err else 6))
            # The daemon updates its snapshot at once; fetch it rather than
            # waiting out the tick with the button still showing.
            self.refresh()
        run_async(work, done)

    def ignore_service(self, name: str):
        self._suppress("instance", name, None, f"service {name} ignored")

    def _threshold_dialog(self, rule_id: str, key: Optional[str] = None):
        """Change a rule's threshold, choosing from the ones the rule really has.

        This used to be two blank text boxes: a key the user had to already know
        ("celsius"? "pct"? "avail_pct"?) and a number. A mistyped key was
        accepted and stored and changed nothing, with a toast saying it had
        worked. The daemon now lists each rule's thresholds with the value in
        force, so the dialog offers those and nothing else.
        """
        def work():
            return self.client.call("thresholds").get("thresholds", [])

        def done(res, err):
            if self._closed:
                return
            # From an alert we know the alert's rule; from Settings, the exact
            # threshold. One alert can be governed by a differently-named
            # setting (bluetooth.disconnected <- bluetooth.disconnect.hold).
            mine = [t for t in (res or [])
                    if (t.get("rule_id") == rule_id and t.get("key") == key) or
                    (key is None and rule_id in (t.get("applies_to"), t.get("rule_id")))]
            if err or not mine:
                self.toasts.add_toast(Adw.Toast(
                    title=(f"Could not read thresholds: {err}. If JamSys was just "
                           "updated, run: systemctl --user restart jamsysd" if err else
                           f"“{rule_id}” has no adjustable threshold — it fires on "
                           "a condition, not a level"), timeout=7))
                return
            self._present_threshold_dialog(mine)
        run_async(work, done)

    def _present_threshold_dialog(self, items: list):
        dlg = Adw.MessageDialog(transient_for=self, heading="Change threshold",
                                body="Takes effect immediately and is remembered. The "
                                     "alert text always quotes the value actually in force.")
        group = Adw.PreferencesGroup()
        spins = []
        for t in items:
            lo, hi = t.get("min", 0.0), t.get("max", 1e12)
            span = hi - lo
            step = 1.0 if span > 20 else 0.1
            adj = Gtk.Adjustment(value=t["value"], lower=lo, upper=hi,
                                 step_increment=step, page_increment=step * 5)
            unit = t.get("unit", "")
            row = Adw.SpinRow(title=GLib.markup_escape_text(t.get("label") or t["key"]),
                              subtitle=GLib.markup_escape_text(
                                  f"default {t['default']:g} {unit} · allowed "
                                  f"{lo:g} to {hi:g}".replace("  ", " ")),
                              adjustment=adj, digits=0 if step == 1.0 and
                              float(t["value"]).is_integer() else 2)
            group.add(row)
            spins.append((t, row))
        dlg.set_extra_child(group)
        dlg.add_response("cancel", "Cancel")
        dlg.add_response("reset", "Use defaults")
        dlg.add_response("save", "Save")
        dlg.set_response_appearance("save", Adw.ResponseAppearance.SUGGESTED)
        dlg.set_default_response("save")
        dlg.set_close_response("cancel")

        def on_resp(_d, resp):
            if resp not in ("save", "reset"):
                return
            for t, row in spins:
                want = t["default"] if resp == "reset" else row.get_value()
                if want != t["value"]:
                    self._set_threshold(t["rule_id"], t["key"], want)
        dlg.connect("response", on_resp)
        dlg.present()

    def _set_threshold(self, rule_id: str, key: str, value: float):
        def work():
            return self.client.call("set_threshold", rule_id=rule_id, key=key, value=value)

        def done(_r, err):
            if self._closed:
                return
            self.toasts.add_toast(Adw.Toast(
                title=f"Threshold {rule_id}.{key} set to {value:g}" if not err
                else f"Threshold not changed: {err}", timeout=3 if not err else 6))
            self._reload_current()
        run_async(work, done)

    def _toggle_collector(self, switch, state, name):
        def work():
            return self.client.call("set_collector", name=name, enabled=bool(state))

        def done(_r, err):
            if self._closed:
                return
            if err:
                self.toasts.add_toast(Adw.Toast(title=f"Could not change {name}: {err}"))
            # Redraw either way: on success the label changes, and on failure the
            # switch must go back to showing what the daemon is actually doing.
            self._reload_current()
        run_async(work, done)
        return False

    def _render(self, page, snap: dict, user: bool) -> None:
        """Draw `page`, and if that fails say so on the page.

        `user` is True for a page switch, which always draws; a background
        refresh goes through update() so it cannot interrupt an interaction.
        """
        try:
            if user:
                page.render(snap)
            else:
                page.update(snap)
        except Exception as e:  # noqa: BLE001 — a render bug must not kill the UI
            print(f"[jamsys-ui] render error on {page.title}: {e!r}", file=sys.stderr)
            try:
                page.show_render_error(e)
            except Exception:  # noqa: BLE001
                pass

    # -- navigation --------------------------------------------------------

    def current_page(self) -> Optional[Page]:
        n = self.stack.get_visible_child_name()
        return self.pages.get(n) if n else None

    def _on_nav(self, _lb, row):
        if row is None:
            return
        t = row.page_title
        self.stack.set_visible_child_name(t)
        self.content_title.set_title(t)
        page = self.pages[t]
        # Async pages fetch for themselves and need no snapshot to start.
        if self.last_snapshot or isinstance(page, AsyncPage):
            self._render(page, self.last_snapshot or {}, user=True)

    def goto(self, title: str):
        for i in range(len(PAGES)):
            r = self.listbox.get_row_at_index(i)
            if r and r.page_title == title:
                self.listbox.select_row(r)
                return


class JamSysApp(Adw.Application):
    def __init__(self, start_page: Optional[str] = None):
        super().__init__(application_id=APP_ID, flags=Gio.ApplicationFlags.DEFAULT_FLAGS)
        self.win: Optional[MainWindow] = None
        self.start_page = start_page

    def do_activate(self):
        provider = Gtk.CssProvider()
        provider.load_from_data(CSS)
        Gtk.StyleContext.add_provider_for_display(
            Gdk_display(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        if not self.win:
            self.win = MainWindow(self)
        self.win.present()
        # Jump straight to the page the caller asked for, if it exists.
        if self.start_page and self.start_page in self.win.pages:
            self.win.goto(self.start_page)
            self.start_page = None

    def do_shutdown(self):
        if self.win:
            self.win.client.shutdown()
        Adw.Application.do_shutdown(self)


def Gdk_display():
    from gi.repository import Gdk
    return Gdk.Display.get_default()


def main(start_page: Optional[str] = None) -> int:
    Adw.init()
    app = JamSysApp(start_page)

    # Registering the application id is a D-Bus round trip, and it fails if a previous
    # instance is still releasing the name — launching twice in quick succession, or
    # relaunching right after closing, is enough. GApplication treats that as fatal
    # ("Message recipient disconnected from message bus without replying"), which
    # presents to the user as the window simply not opening. Retry briefly instead.
    for attempt in range(4):
        try:
            app.register(None)
            break
        except GLib.Error as e:
            if attempt == 3:
                print(f"jamsys: could not register on the session bus: {e.message}",
                      file=sys.stderr)
                return 1
            time.sleep(0.35 * (attempt + 1))

    return app.run(None)
