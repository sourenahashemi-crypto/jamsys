"""JamSys desktop application — GTK4 + libadwaita.

The UI is a thin client over the daemon's documented socket. It holds no monitoring
logic of its own: everything it shows came from `snapshot`, `history`, `alerts`,
`events`, `coverage` or `processes`. That separation is what lets the daemon keep
running with the window closed, and what would let a different front-end replace this
one without touching the backend.
"""

from __future__ import annotations

import sys
import time
from typing import Optional

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gio, GLib, Gtk  # noqa: E402

from .client import Client, DaemonError, run_async
from .hardware import (CHARGE_LIMITS, MODES, PRESETS, SPEEDS, ChargeLimitControl,
                       KeyboardControl, helper_path,
                       ints_to_rgba, rgba_to_ints)
from .format import (SEVERITY, clock_hm, human_ago, human_bps, human_bytes,
                     human_duration, pct, temp, watts)
from .widgets import CSS, MetricCard, Sparkline, StatusPill, severity_row

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
        adj = self._scroller.get_vadjustment() if self._scroller else None
        pos = adj.get_value() if adj else 0.0
        self.render(snap)
        if adj and pos > 0.0:
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

    # -- small helpers used by every page --------------------------------

    @staticmethod
    def group(title: str, description: str = "") -> Adw.PreferencesGroup:
        return Adw.PreferencesGroup(title=title, description=description)

    @staticmethod
    def row(title: str, value: str, subtitle: str = "") -> Adw.ActionRow:
        r = Adw.ActionRow(title=GLib.markup_escape_text(title))
        if subtitle:
            r.set_subtitle(GLib.markup_escape_text(subtitle))
        lbl = Gtk.Label(label=value, xalign=1)
        lbl.add_css_class("dim-label")
        lbl.set_selectable(True)
        r.add_suffix(lbl)
        return r

    def clear(self) -> None:
        child = self.body.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self.body.remove(child)
            child = nxt


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
            min_children_per_line=2, max_children_per_line=4,
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
        ]:
            c = MetricCard(title, icon, on_click=lambda p=page: self.win.goto(p))
            self.cards[key] = c
            grid.append(c)
        self.body.append(grid)

        self.events_group = self.group("Recent events and alerts")
        self.body.append(self.events_group)
        self._event_rows: list = []
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

    def set_events(self, items: list[dict]) -> None:
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
            cg = self.group("Per-core usage")
            box = Gtk.FlowBox(selection_mode=Gtk.SelectionMode.NONE, homogeneous=True,
                              min_children_per_line=4, max_children_per_line=8,
                              row_spacing=6, column_spacing=6)
            box.set_margin_top(8)
            for i, v in enumerate(cores):
                cell = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
                lab = Gtk.Label(label=f"cpu{i}", xalign=0.5)
                lab.add_css_class("caption")
                lab.add_css_class("dim-label")
                bar = Gtk.LevelBar(min_value=0, max_value=100, value=v)
                bar.set_size_request(-1, 8)
                val = Gtk.Label(label=f"{v:.0f}%", xalign=0.5)
                val.add_css_class("caption")
                cell.append(lab); cell.append(bar); cell.append(val)
                box.append(cell)
            cg.add(box)
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
            row = Adw.ActionRow(title=f["mount"],
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
                                  subtitle="/dev/nvme0 is root-only. Install the privileged helper to read "
                                           "wear, spare blocks and media errors. Temperature above is read "
                                           "unprivileged through hwmon and is unaffected.")
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
            row = Adw.ActionRow(title=u["name"], subtitle=u.get("description", ""))
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


class ProcessPage(Page):
    title, icon = "Processes", "utilities-system-monitor-symbolic"

    def render(self, snap):
        s = snap.get("snapshot", {})
        p = s.get("process", {})
        self.clear()
        hdr = self.group("Processes",
                         f"{p.get('total', 0)} running. I/O and GPU figures are available for your own "
                         "processes; JamSys does not need root to show this.")
        self.body.append(hdr)
        for label, key in [("By CPU", "top_cpu"), ("By memory", "top_mem")]:
            g = self.group(label)
            for q in p.get(key, []):
                sub = f"{human_bytes(q['rss_bytes'])} · {q['threads']} threads"
                if q.get("read_bps") or q.get("write_bps"):
                    sub += f" · io ↓{human_bps(q['read_bps'])} ↑{human_bps(q['write_bps'])}"
                if q.get("gpu_ns_per_s"):
                    # DRM fdinfo reports engine time per engine; this is their sum, so
                    # it can legitimately exceed one second per second on a GPU with
                    # several engines busy. Showing it as a percentage would look wrong.
                    sub += f" · gpu {q['gpu_ns_per_s']/1e9:.2f} engine-s/s"
                row = Adw.ActionRow(title=f"{q['name']}  ({q['pid']})", subtitle=GLib.markup_escape_text(sub))
                val = Gtk.Label(label=f"{q['cpu_pct']:.1f}%" if key == "top_cpu" else human_bytes(q["rss_bytes"]))
                val.add_css_class("dim-label")
                row.add_suffix(val)
                if q.get("flag"):
                    ic = Gtk.Image.new_from_icon_name("dialog-warning-symbolic")
                    ic.add_css_class("sv-attention")
                    ic.set_tooltip_text(q["flag"])
                    row.add_prefix(ic)
                g.add(row)
            self.body.append(g)


def _repo_script(name: str) -> str:
    """Absolute path to a script in the source checkout, if we can find one.

    A relative `./scripts/...` is useless in a message: it only works from inside
    the tree, and this text is read from a window that could have been launched
    from anywhere.
    """
    import os
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for base in (os.path.dirname(here), here):
        cand = os.path.join(base, "scripts", name)
        if os.path.isfile(cand):
            return cand
    return f"/path/to/jamsys/scripts/{name}"


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

    def render(self, snap):
        s = snap.get("snapshot", {})
        k = s.get("keyboard", {})
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
        scale.connect("value-changed", self._on_brightness, control)
        brow.add_suffix(scale)
        bg.add(brow)
        self.body.append(bg)

        if not k.get("rgb_capable"):
            self.body.append(self.group(
                "Colour", "This keyboard has a backlight but no addressable RGB."))
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
                            "in the kernel's field order: boot, awake, sleep, keyboard.")
            self._state_switches = {}
            for key, title in [("boot", "During boot"), ("awake", "While awake"),
                               ("sleep", "While asleep"), ("keyboard", "Keyboard lighting")]:
                r = Adw.SwitchRow(title=title)
                r.set_active(key in ("awake", "keyboard"))
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
                     "    sudo ./scripts/install-privileged.sh")
        g = self.group("Battery care", desc)

        row = Adw.ActionRow(
            title="Charge limit",
            subtitle=("charging stops at this level"
                      if (current or 100) < 100 else "charging normally, to 100%"))
        combo = Gtk.DropDown.new_from_strings([label for label, _ in CHARGE_LIMITS])
        values = [v for _, v in CHARGE_LIMITS]
        # Select the firmware's current value when it is one we offer; otherwise
        # leave the list alone rather than silently misreporting it.
        if current in values:
            combo.set_selected(values.index(current))
        combo.set_valign(Gtk.Align.CENTER)
        combo.set_sensitive(installed)
        combo.connect("notify::selected", self._on_charge_limit, values)
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

    def _on_charge_limit(self, combo, _param, values):
        idx = combo.get_selected()
        if idx < 0 or idx >= len(values):
            return
        want = values[idx]
        if not self.charge.set_limit(want):
            self.win.toasts.add_toast(Adw.Toast(
                title=f"Charge limit: {self.charge.last_error or 'could not set it'}",
                timeout=6))
            return
        # The firmware is the source of truth, so say what was asked for and let the
        # next snapshot report what actually took effect.
        self.win.toasts.add_toast(Adw.Toast(title=f"Charge limit set to {want}%",
                                            timeout=3))

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
                f"    sudo {_repo_script('install-privileged.sh')}\n"
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
        if ok:
            self._sent_anything = True
            self.win.toasts.add_toast(Adw.Toast(title=f"{what} applied", timeout=2))
        else:
            self.win.toasts.add_toast(
                Adw.Toast(title=f"{what} failed: {self.kbd.last_error}", timeout=6))

    def _on_brightness(self, scale, control):
        v = int(round(scale.get_value()))
        run_async(lambda: self.kbd.set_brightness(v, control),
                  lambda ok, err: self._apply(bool(ok) and not err, f"Brightness {v}"))

    def _push_rgb(self, control):
        r, g, b = self._colour
        run_async(lambda: self.kbd.set_rgb(self._mode, r, g, b, self._speed, control),
                  lambda ok, err: self._apply(bool(ok) and not err, "Lighting"))

    def _on_colour(self, btn, _p, control):
        self._colour = rgba_to_ints(btn.get_rgba())
        self._push_rgb(control)

    def _on_preset(self, _b, rgb, control):
        self._colour = rgb
        self._push_rgb(control)

    def _on_mode(self, row, _p, control):
        self._mode = MODES[row.get_selected()][1]
        self._push_rgb(control)

    def _on_speed(self, row, _p, control):
        self._speed = SPEEDS[row.get_selected()][1]
        self._push_rgb(control)

    def _on_state(self, _row, _p, control):
        sw = self._state_switches
        run_async(lambda: self.kbd.set_states(
                      sw["boot"].get_active(), sw["awake"].get_active(),
                      sw["sleep"].get_active(), sw["keyboard"].get_active(), control),
                  lambda ok, err: self._apply(bool(ok) and not err, "Lighting states"))


class DiagnosticsPage(Page):
    """What the monitoring application costs. A monitor that will not report its own
    overhead is asking to be taken on trust."""

    title, icon = "Diagnostics", "speedometer-symbolic"

    def render(self, snap):
        self.clear()
        self._daemon = self.group(
            "Monitoring daemon",
            "Measured live from /proc/self. If these numbers look wrong, they are wrong "
            "about the thing you should trust least: this application.")
        self.body.append(self._daemon)
        self._store = self.group("History store")
        self.body.append(self._store)
        self._widget = self.group("GNOME Shell readout")
        self.body.append(self._widget)
        self._ui = self.group("This window")
        self.body.append(self._ui)
        self.win.load_diagnostics(self._daemon, self._store, self._widget, self._ui)


class EventsPage(Page):
    title, icon = "Events", "document-open-recent-symbolic"

    def __init__(self, win):
        super().__init__(win)
        self.filter = "all"

    def render(self, snap):
        self.clear()
        bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        for label, key in [("All", "all"), ("Alerts", "alerts"), ("Warnings and above", "warn")]:
            b = Gtk.ToggleButton(label=label, active=(self.filter == key))
            b.connect("toggled", self._set_filter, key)
            bar.append(b)
        self.body.append(bar)
        self._group = self.group("History")
        self.body.append(self._group)
        self.win.load_events(self._group, self.filter)

    def _set_filter(self, btn, key):
        if btn.get_active() and self.filter != key:
            self.filter = key
            self.render(self.win.last_snapshot or {})


class CoveragePage(Page):
    title, icon = "Coverage", "checkbox-checked-symbolic"

    def render(self, snap):
        self.clear()
        intro = self.group(
            "Monitoring coverage",
            "A monitor that quietly fails to monitor something is worse than no monitor, "
            "because you believe you are covered. Everything JamSys can and cannot see "
            "on this machine is listed here.")
        self.body.append(intro)
        self._group = self.group("Collectors")
        self.body.append(self._group)
        self._inv = self.group("Hardware inventory")
        self.body.append(self._inv)
        self._chg = self.group("Changes since first run",
                               "Useful after a kernel or driver update: what actually changed.")
        self.body.append(self._chg)
        self.win.load_coverage(self._group, self._inv, self._chg)


class SettingsPage(Page):
    title, icon = "Settings", "preferences-system-symbolic"

    def render(self, snap):
        self.clear()
        g = self.group("Notifications")
        self._group = g
        self.body.append(g)
        s = self.group("Active suppressions",
                       "Muted alerts, snoozes and ignored rules or devices. All reversible.")
        self._sup = s
        self.body.append(s)
        d = self.group("Daemon", "Live self-measurement of the monitoring application itself.")
        self._daemon = d
        self.body.append(d)
        self.win.load_settings(self._group, self._sup, self._daemon)


# ---------------------------------------------------------------------------
# Window
# ---------------------------------------------------------------------------

PAGES = [OverviewPage, CpuPage, MemoryPage, GpuPage, PowerPage, ThermalPage,
         StoragePage, NetworkPage, DevicesPage, ServicesPage, ProcessPage,
         HardwarePage, EventsPage, CoveragePage, DiagnosticsPage, SettingsPage]


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="JamSys", default_width=1080, default_height=760)
        self.client = Client(on_push=self._on_push, on_state=self._on_state)
        self.last_snapshot: Optional[dict] = None
        self._alert_cache: list[dict] = []
        self._charts: dict[str, Sparkline] = {}
        self._busy = False

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
        refresh = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Refresh now")
        refresh.connect("clicked", lambda *_: self.refresh())
        self.content_head.pack_end(refresh)
        self.stack = Gtk.Stack(transition_type=Gtk.StackTransitionType.CROSSFADE,
                               transition_duration=120)
        for t, p in self.pages.items():
            self.stack.add_named(p, t)
        cbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        cbox.append(self.content_head)
        self.toasts = Adw.ToastOverlay()
        self.toasts.set_child(self.stack)
        cbox.append(self.toasts)
        self.split.set_content(Adw.NavigationPage(child=cbox, title="Overview"))

        self.listbox.select_row(self.listbox.get_row_at_index(0))
        GLib.timeout_add(REFRESH_MS, self._tick)
        self._connect_async()

    # -- connection ------------------------------------------------------

    def _connect_async(self):
        def done(ok, err):
            if ok:
                self.client.subscribe(["alert", "event"])
                self.refresh()
        run_async(self.client.connect, done)

    def _on_state(self, ok: bool, msg: str):
        self.conn_pill.set("healthy" if ok else "critical",
                           "Connected" if ok else "Service unavailable")
        if not ok:
            self.pages["Overview"].banner_text.set_text("Cannot reach the monitoring service")
            self.pages["Overview"].banner_sub.set_text(
                msg + "\nStart it with:  systemctl --user start jamsysd")
            for c in ("sv-banner-healthy", "sv-banner-attention", "sv-banner-critical", "sv-banner-unknown"):
                self.pages["Overview"].banner.remove_css_class(c)
            self.pages["Overview"].banner.add_css_class("sv-banner-unknown")
        return False

    def _on_push(self, topic: str, data: dict):
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
        if self.client.connected:
            self.refresh()
        else:
            self._connect_async()
        return True

    # -- data ------------------------------------------------------------

    def refresh(self):
        if self._busy or not self.client.connected:
            return
        self._busy = True

        def work():
            return self.client.call("snapshot")

        def done(res, err):
            self._busy = False
            if err or res is None:
                return
            self.last_snapshot = res
            page = self.current_page()
            if page:
                try:
                    page.update(res)
                except Exception as e:  # noqa: BLE001 — a render bug must not kill the UI
                    print(f"[jamsys-ui] render error on {page.title}: {e}")
            if isinstance(page, OverviewPage):
                self.refresh_events()
        run_async(work, done)

    def refresh_events(self):
        page = self.current_page()
        if not isinstance(page, OverviewPage) or not self.client.connected:
            return

        def work():
            alerts = self.client.call("alerts", open_only=False, limit=25).get("alerts", [])
            events = self.client.call("events", limit=25).get("events", [])
            return alerts, events

        def done(res, err):
            if err or not res:
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

    def load_events(self, group: Adw.PreferencesGroup, flt: str):
        def work():
            if flt == "alerts":
                return [("alert", a) for a in self.client.call("alerts", open_only=False, limit=200).get("alerts", [])]
            evs = self.client.call("events", limit=300).get("events", [])
            if flt == "warn":
                evs = [e for e in evs if e["severity"] >= 2]
            return [("event", e) for e in evs]

        def done(res, err):
            if err or res is None:
                group.add(Adw.ActionRow(title="Could not load events", subtitle=str(err)))
                return
            now = time.time() * 1000
            if not res:
                group.add(Adw.ActionRow(title="Nothing recorded yet"))
                return
            for kind, it in res[:300]:
                if kind == "alert":
                    sub = f"{clock_hm(it['last_ts'])} · {human_ago(it['last_ts'], now)} · fired {it['count']}×"
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
                    row.add_suffix(self.alert_menu(it))
                else:
                    row = severity_row(it["severity"], it["summary"],
                                       f"{clock_hm(it['ts'])} · {it['subsystem']} · {it['kind']}")
                group.add(row)
        run_async(work, done)

    def load_coverage(self, group, inv_group, chg_group):
        def work():
            cov = self.client.call("coverage")
            inv = self.client.call("inventory")
            chg = self.client.call("inventory", changes=True)
            return cov, inv, chg

        def done(res, err):
            if err or not res:
                group.add(Adw.ActionRow(title="Could not load coverage", subtitle=str(err)))
                return
            cov, inv, chg = res
            labels = {"Full": ("emblem-ok-symbolic", "sv-healthy"),
                      "Partial": ("dialog-information-symbolic", "sv-idle"),
                      "Unavailable": ("action-unavailable-symbolic", "sv-unknown"),
                      "Quarantined": ("dialog-error-symbolic", "sv-critical"),
                      "Disabled": ("action-unavailable-symbolic", "sv-unknown")}
            for c in cov.get("collectors", []):
                lab = c["label"]
                sup = c.get("support", {})
                detail = sup.get("detail") or sup.get("reason") or ""
                sub = f"{c['tier']} tier · {c['runs']} runs · {c['avg_us']} µs average"
                if detail:
                    sub = detail + " · " + sub
                row = Adw.ActionRow(title=c["name"], subtitle=GLib.markup_escape_text(sub))
                icon, css = labels.get(lab, labels["Unavailable"])
                ic = Gtk.Image.new_from_icon_name(icon)
                ic.add_css_class(css)
                row.add_prefix(ic)
                val = Gtk.Label(label=lab)
                val.add_css_class(css)
                row.add_suffix(val)
                sw = Gtk.Switch(active=(lab != "Disabled"), valign=Gtk.Align.CENTER)
                sw.connect("state-set", self._toggle_collector, c["name"])
                row.add_suffix(sw)
                group.add(row)
            for name, ok, why in [
                ("Journal (kernel errors, Xid, OOM)", cov.get("journal"), "journalctl could not be started"),
                ("Network change events (rtnetlink)", cov.get("netlink_route"), "netlink socket unavailable"),
                ("Device hotplug events (uevent)", cov.get("netlink_uevent"), "netlink socket unavailable"),
                ("Privileged helper (RAPL, NVMe SMART)", cov.get("helper_installed"),
                 "not installed — see the README for the optional helper"),
            ]:
                row = Adw.ActionRow(title=name, subtitle="" if ok else why)
                ic = Gtk.Image.new_from_icon_name("emblem-ok-symbolic" if ok else "action-unavailable-symbolic")
                ic.add_css_class("sv-healthy" if ok else "sv-unknown")
                row.add_prefix(ic)
                row.add_suffix(Gtk.Label(label="Available" if ok else "Unavailable"))
                group.add(row)

            for it in inv.get("items", []):
                inv_group.add(self.pages["Coverage"].row(it["key"], it["value"]))
            changes = chg.get("changes", [])
            if not changes:
                chg_group.add(Adw.ActionRow(title="Nothing has changed since JamSys was installed"))
            for c in changes[:80]:
                old = c.get("old")
                chg_group.add(self.pages["Coverage"].row(
                    c["key"], c["new"],
                    (f"was {old} · " if old else "first seen · ") + clock_hm(c["ts"])))
        run_async(work, done)

    def load_settings(self, group, sup_group, daemon_group):
        def work():
            return self.client.call("suppressions"), self.client.call("stats"), self.client.call("ping")

        def done(res, err):
            if err or not res:
                group.add(Adw.ActionRow(title="Could not load settings", subtitle=str(err)))
                return
            sup, stats, ping = res
            row = Adw.ActionRow(
                title="Desktop notifications",
                subtitle="Sent by the daemon, so they work with this window closed. "
                         "Edit config.toml to change the minimum severity.")
            row.add_suffix(Gtk.Label(label="enabled in config"))
            group.add(row)

            items = sup.get("suppressions", [])
            if not items:
                sup_group.add(Adw.ActionRow(title="Nothing is suppressed"))
            for s in items:
                until = s.get("until_ts")
                sub = f"{s['scope']} · " + ("permanent" if not until else f"until {clock_hm(until)}")
                if s.get("reason"):
                    sub += f" · {s['reason']}"
                r = Adw.ActionRow(title=s["pattern"], subtitle=sub)
                b = Gtk.Button(label="Remove", valign=Gtk.Align.CENTER)
                b.add_css_class("flat")
                b.connect("clicked", lambda _w, i=s["id"]: self._unsuppress(i))
                r.add_suffix(b)
                sup_group.add(r)

            pg = self.pages["Settings"]
            up = stats.get("uptime_s", 0)
            cpu = stats.get("cpu_seconds", 0)
            daemon_group.add(pg.row("Version", ping.get("version", "?"), f"protocol {ping.get('schema')}"))
            daemon_group.add(pg.row("Uptime", human_duration(up)))
            daemon_group.add(pg.row("Memory used by the daemon", human_bytes(stats.get("rss_bytes", 0)),
                                    f"{stats.get('threads', 0)} threads"))
            daemon_group.add(pg.row("CPU used by the daemon", f"{cpu:.2f} s",
                                    f"{100*cpu/max(up,1):.3f}% of one core since start"))
            daemon_group.add(pg.row("Wakeups", f"{stats.get('wakeups', 0)}",
                                    f"{stats.get('wakeups_per_s', 0):.2f} per second"))
            daemon_group.add(pg.row("Sampling rate", f"×{stats.get('sampling_multiplier', 1):.0f} interval multiplier",
                                    "3× while idle on battery with no window open"))
            daemon_group.add(pg.row("Database", human_bytes(stats.get("db_bytes", 0)),
                                    f"{stats.get('sample_rows',0):,} samples · {stats.get('rollup_rows',0):,} rollups · "
                                    f"{stats.get('event_rows',0):,} events"))
            daemon_group.add(pg.row("Learned baselines", f"{stats.get('baselines', 0)} series"))
            daemon_group.add(pg.row("Notifications suppressed by rate limiting",
                                    str(stats.get("suppressed_notifications", 0))))
        run_async(work, done)

    def load_diagnostics(self, daemon_g, store_g, widget_g, ui_g):
        def work():
            return self.client.call("stats"), self.client.call("ping")

        def done(res, err):
            pg = self.pages["Diagnostics"]
            if err or not res:
                daemon_g.add(Adw.ActionRow(title="Could not read daemon statistics",
                                           subtitle=str(err)))
                return
            d, ping = res
            up = max(d.get("uptime_s", 1), 1)
            cpu = d.get("cpu_seconds", 0)
            daemon_g.add(pg.row("Version", ping.get("version", "?"),
                                f"protocol {ping.get('schema')} · pid {ping.get('pid')}"))
            daemon_g.add(pg.row("Uptime", human_duration(up)))
            daemon_g.add(pg.row("CPU used", f"{cpu:.2f} s",
                                f"{100*cpu/up:.3f}% of one core since start"))
            daemon_g.add(pg.row("Memory", human_bytes(d.get("rss_bytes", 0)),
                                f"{d.get('threads', 0)} threads"))
            daemon_g.add(pg.row("Wakeups", f"{d.get('wakeups', 0):,}",
                                f"{d.get('wakeups_per_s', 0):.2f} per second"))
            daemon_g.add(pg.row("Scheduler ticks", f"{d.get('ticks', 0):,}"))
            frac = d.get("idle_tick_fraction", 0) or 0
            blocked = d.get("idle_blocked_by") or ""
            daemon_g.add(pg.row("Reduced sampling",
                                f"{100*frac:.0f}% of ticks",
                                (f"currently off: {blocked}" if blocked
                                 else "currently active")))
            daemon_g.add(pg.row("Activity state", d.get("activity", "?"),
                                f"baseline context: {d.get('context','?')}"))
            daemon_g.add(pg.row("Events processed", f"{d.get('events_processed', 0):,}",
                                f"{d.get('events_dropped', 0):,} dropped"))
            daemon_g.add(pg.row("Notifications rate-limited",
                                str(d.get("suppressed_notifications", 0))))

            store_g.add(pg.row("Database on disk", human_bytes(d.get("db_bytes", 0)),
                               f"{d.get('sample_rows',0):,} ten-second samples · "
                               f"{d.get('rollup_rows',0):,} rollups · "
                               f"{d.get('event_rows',0):,} events"))
            store_g.add(pg.row("Full-resolution buffer",
                               human_bytes(d.get("live_bytes", 0)),
                               f"{d.get('live_points', 0):,} points held in memory only"))
            store_g.add(pg.row("Pending writes", str(d.get("samples_pending", 0)),
                               "flushed in one transaction every 15 s"))
            store_g.add(pg.row("Learned baselines", f"{d.get('baselines', 0)} series"))

            has = d.get("dbus_widget", False)
            widget_g.add(pg.row("D-Bus service",
                                "org.jamsys.Monitor" if has else "unavailable",
                                "" if has else "no session bus when the daemon started"))
            widget_g.add(pg.row("Repaints pushed", f"{d.get('dbus_signals', 0):,}",
                                "only when a displayed value changed enough to matter"))
            widget_g.add(pg.row("Method calls served", f"{d.get('dbus_calls', 0):,}"))

            ui_g.add(pg.row("Memory", human_bytes(_self_rss()),
                            "this window; the daemon keeps running when it closes"))
            ui_g.add(pg.row("IPC clients attached", str(d.get("ipc_clients", 0))))
        run_async(work, done)

    # -- charts and timelines ---------------------------------------------

    def chart(self, subsystem: str, name: str, instance: str, title: str, unit: str,
              band_metric: Optional[str] = None) -> Gtk.Widget:
        g = Adw.PreferencesGroup(title=title)
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

        def done(res, err):
            if err or not res:
                return
            pts = [p[1] for p in res.get("points", [])]
            spark.set_points(pts)
            if pts:
                caption.set_text(
                    f"last hour · now {pts[-1]:,.1f} {unit} · min {min(pts):,.1f} · max {max(pts):,.1f} "
                    f"· {len(pts)} points at "
                    + ({0: "full resolution", 60000: "1-minute", 300000: "5-minute", 900000: "15-minute"}
                       .get(res.get("bucket", 0), "aggregated")))
            else:
                caption.set_text("no history yet — the daemon has just started")
        run_async(work, done)
        return g

    def event_timeline(self, subsystem: str, title: str) -> Gtk.Widget:
        g = Adw.PreferencesGroup(title=title)

        def work():
            return self.client.call("events", subsystem=subsystem, limit=40).get("events", [])

        def done(res, err):
            if err or res is None:
                return
            if not res:
                g.add(Adw.ActionRow(title="No events recorded for this subsystem"))
                return
            for e in res:
                g.add(severity_row(e["severity"], f"{clock_hm(e['ts'])}   {e['summary']}", e["kind"]))
        run_async(work, done)
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
                self.refresh_events()
        run_async(work, done)

    def _unsuppress(self, sid: int):
        def work():
            return self.client.call("unsuppress", id=sid)

        def done(_r, err):
            self.toasts.add_toast(Adw.Toast(title="Suppression removed" if not err else f"Failed: {err}"))
            page = self.current_page()
            if page:
                page.update(self.last_snapshot or {})
        run_async(work, done)

    def ignore_service(self, name: str):
        self._suppress("instance", name, None, f"service {name} ignored")

    def _threshold_dialog(self, rule_id: str):
        dlg = Adw.MessageDialog(transient_for=self, heading="Change threshold",
                                body=f"Set a new threshold for the rule “{rule_id}”.\n"
                                     "The alert text always quotes the value actually in force.")
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        key = Gtk.Entry(placeholder_text="key, for example: celsius, pct, avail_pct")
        val = Gtk.Entry(placeholder_text="numeric value")
        box.append(key)
        box.append(val)
        dlg.set_extra_child(box)
        dlg.add_response("cancel", "Cancel")
        dlg.add_response("save", "Save")
        dlg.set_response_appearance("save", Adw.ResponseAppearance.SUGGESTED)

        def on_resp(_d, resp):
            if resp != "save":
                return
            try:
                v = float(val.get_text())
            except ValueError:
                self.toasts.add_toast(Adw.Toast(title="That is not a number"))
                return
            k = key.get_text().strip()
            if not k:
                self.toasts.add_toast(Adw.Toast(title="A key is required"))
                return

            def work():
                return self.client.call("set_threshold", rule_id=rule_id, key=k, value=v)

            def done(_r, err):
                self.toasts.add_toast(Adw.Toast(
                    title=f"Threshold {rule_id}.{k} set to {v}" if not err else f"Failed: {err}"))
            run_async(work, done)
        dlg.connect("response", on_resp)
        dlg.present()

    def _toggle_collector(self, switch, state, name):
        def work():
            return self.client.call("set_collector", name=name, enabled=bool(state))

        def done(_r, err):
            if err:
                self.toasts.add_toast(Adw.Toast(title=f"Could not change {name}: {err}"))
        run_async(work, done)
        return False

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
        if self.last_snapshot:
            self.pages[t].render(self.last_snapshot)

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
