"""Custom widgets: sparkline, status pill, metric card.

Charts are drawn with GTK 4's own scene graph (``Gsk.PathBuilder`` inside
``do_snapshot``) rather than with Cairo or a charting library. Three reasons:

* it is GPU-composited rather than rasterised on the CPU, which matters for an
  application whose entire premise is that it costs nothing to run;
* it removes the ``python3-gi-cairo`` runtime dependency, which is not installed by
  default on Ubuntu and whose absence otherwise breaks every chart at runtime;
* no charting library means no dependency to keep current.
"""

from __future__ import annotations

import math
from typing import Optional, Sequence

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
gi.require_version("Gsk", "4.0")
gi.require_version("Graphene", "1.0")
from gi.repository import Adw, Gdk, GLib, Graphene, Gsk, Gtk  # noqa: E402


def _rgba(css_class_color: str) -> Gdk.RGBA:
    c = Gdk.RGBA()
    c.parse(css_class_color)
    return c


# Status colours are also carried by an icon and a word, never by colour alone.
STATUS_COLORS = {
    "healthy": "#2ec27e",
    "attention": "#e5a50a",
    "critical": "#e01b24",
    "unknown": "#9a9996",
    "idle": "#3584e4",
}


class Sparkline(Gtk.Widget):
    """A tiny history strip. Answers 'is this new?', which a bare number cannot."""

    __gtype_name__ = "JamSysSparkline"

    def __init__(self, height: int = 34):
        super().__init__()
        self._points: list[float] = []
        self._band: Optional[tuple[float, float]] = None
        self._color = STATUS_COLORS["healthy"]
        self._height = height
        self.set_size_request(-1, height)
        self.set_hexpand(True)

    def set_points(self, pts: Sequence[float]) -> None:
        self._points = [p for p in pts if p is not None and math.isfinite(p)]
        self.queue_draw()

    def set_band(self, lo: Optional[float], hi: Optional[float]) -> None:
        """Shade the learned normal range behind the line."""
        self._band = (lo, hi) if lo is not None and hi is not None and hi > lo else None
        self.queue_draw()

    def set_color(self, status: str) -> None:
        self._color = STATUS_COLORS.get(status, STATUS_COLORS["healthy"])
        self.queue_draw()

    def do_measure(self, orientation, for_size):
        if orientation == Gtk.Orientation.VERTICAL:
            return (self._height, self._height, -1, -1)
        return (40, 120, -1, -1)

    def do_snapshot(self, snapshot):
        width = float(self.get_width())
        height = float(self.get_height())
        if width <= 1 or height <= 1:
            return
        pts = self._points

        if len(pts) < 2:
            # Say so rather than drawing a misleading flat line at zero.
            layout = self.create_pango_layout("collecting\u2026")
            grey = Gdk.RGBA()
            grey.parse("#77767b")
            snapshot.save()
            snapshot.translate(Graphene.Point().init(2, height / 2 - 9))
            snapshot.append_layout(layout, grey)
            snapshot.restore()
            return

        lo, hi = min(pts), max(pts)
        if self._band:
            lo = min(lo, self._band[0])
            hi = max(hi, self._band[1])
        if hi - lo < 1e-9:
            hi = lo + 1.0
        pad = (hi - lo) * 0.1
        lo -= pad
        hi += pad

        def y(v: float) -> float:
            return height - (v - lo) / (hi - lo) * height

        if self._band:
            b0, b1 = self._band
            band = Gdk.RGBA()
            band.parse(STATUS_COLORS["idle"])
            band.alpha = 0.16
            snapshot.append_color(
                band, Graphene.Rect().init(0, y(b1), width, max(1.0, y(b0) - y(b1))))

        rgba = _rgba(self._color)
        step = width / (len(pts) - 1)

        # Filled area under the line.
        fill = Gsk.PathBuilder()
        fill.move_to(0, height)
        for i, v in enumerate(pts):
            fill.line_to(i * step, y(v))
        fill.line_to(width, height)
        fill.close()
        shade = Gdk.RGBA()
        shade.red, shade.green, shade.blue, shade.alpha = rgba.red, rgba.green, rgba.blue, 0.15
        snapshot.append_fill(fill.to_path(), Gsk.FillRule.WINDING, shade)

        # The line itself.
        line = Gsk.PathBuilder()
        line.move_to(0, y(pts[0]))
        for i, v in enumerate(pts[1:], start=1):
            line.line_to(i * step, y(v))
        stroke = Gsk.Stroke.new(1.6)
        stroke.set_line_join(Gsk.LineJoin.ROUND)
        stroke.set_line_cap(Gsk.LineCap.ROUND)
        snapshot.append_stroke(line.to_path(), stroke, rgba)

        # Mark the current value.
        dot = Gsk.PathBuilder()
        dot.add_circle(Graphene.Point().init(width - 2.0, y(pts[-1])), 2.4)
        snapshot.append_fill(dot.to_path(), Gsk.FillRule.WINDING, rgba)


class StatusPill(Gtk.Box):
    """Icon + word. Colour is decoration, never the only carrier of meaning."""

    ICONS = {
        "healthy": "emblem-ok-symbolic",
        "attention": "dialog-warning-symbolic",
        "critical": "dialog-error-symbolic",
        "unknown": "dialog-question-symbolic",
        "idle": "weather-clear-night-symbolic",
    }

    def __init__(self, status: str = "unknown", text: str = ""):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        self._icon = Gtk.Image()
        self._label = Gtk.Label(xalign=0)
        self._label.add_css_class("caption")
        self.append(self._icon)
        self.append(self._label)
        self.set(status, text)

    def set(self, status: str, text: str = "") -> None:
        self._icon.set_from_icon_name(self.ICONS.get(status, self.ICONS["unknown"]))
        for c in ("sv-healthy", "sv-attention", "sv-critical", "sv-unknown", "sv-idle"):
            self._icon.remove_css_class(c)
            self._label.remove_css_class(c)
        self._icon.add_css_class(f"sv-{status}")
        self._label.add_css_class(f"sv-{status}")
        self._label.set_text(text or status.capitalize())


class MetricCard(Gtk.Button):
    """One Overview tile: title, big value, secondary line, normal range, sparkline.

    A Button so the whole tile is clickable and keyboard-reachable — clicking it opens
    the matching detail page.
    """

    def __init__(self, title: str, icon: str, on_click=None):
        super().__init__()
        self.add_css_class("card")
        self.add_css_class("sv-card")
        self.set_hexpand(True)
        if on_click:
            self.connect("clicked", lambda *_: on_click())

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        box.set_margin_top(12)
        box.set_margin_bottom(12)
        box.set_margin_start(14)
        box.set_margin_end(14)

        head = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        img = Gtk.Image.new_from_icon_name(icon)
        img.add_css_class("dim-label")
        t = Gtk.Label(label=title, xalign=0)
        t.add_css_class("heading")
        t.add_css_class("dim-label")
        head.append(img)
        head.append(t)
        box.append(head)

        self._value = Gtk.Label(xalign=0, label="—")
        self._value.add_css_class("sv-value")
        box.append(self._value)

        self._sub = Gtk.Label(xalign=0, label="")
        self._sub.add_css_class("caption")
        self._sub.set_wrap(True)
        box.append(self._sub)

        self._range = Gtk.Label(xalign=0, label="")
        self._range.add_css_class("caption")
        self._range.add_css_class("dim-label")
        box.append(self._range)

        self.spark = Sparkline()
        self.spark.set_margin_top(6)
        box.append(self.spark)

        self._status = StatusPill("unknown")
        self._status.set_margin_top(4)
        box.append(self._status)

        self.set_child(box)

    def update(self, value: str, sub: str = "", normal: str = "",
               status: str = "healthy", status_text: str = "") -> None:
        self._value.set_text(value)
        self._sub.set_text(sub)
        self._range.set_text(normal)
        self._range.set_visible(bool(normal))
        self._status.set(status, status_text)
        self.spark.set_color(status)

    def set_unavailable(self, why: str) -> None:
        """Dim rather than hide. A silently missing card reads as 'fine'."""
        self._value.set_text("—")
        self._sub.set_text(why)
        self._range.set_visible(False)
        self._status.set("unknown", "Not available")
        self.add_css_class("dim-label")
        self.spark.set_visible(False)


def severity_row(sev: int, title: str, subtitle: str) -> Adw.ActionRow:
    row = Adw.ActionRow(title=GLib.markup_escape_text(title),
                        subtitle=GLib.markup_escape_text(subtitle))
    names = ["emblem-ok-symbolic", "dialog-information-symbolic",
             "dialog-warning-symbolic", "dialog-error-symbolic"]
    classes = ["sv-healthy", "sv-idle", "sv-attention", "sv-critical"]
    i = Gtk.Image.new_from_icon_name(names[min(sev, 3)])
    i.add_css_class(classes[min(sev, 3)])
    row.add_prefix(i)
    return row


CSS = b"""
.sv-value { font-size: 26px; font-weight: 300; }
.sv-card { padding: 0; }
.sv-healthy   { color: #2ec27e; }
.sv-attention { color: #e5a50a; }
.sv-critical  { color: #e01b24; }
.sv-unknown   { color: #9a9996; }
.sv-idle      { color: #3584e4; }
.sv-banner-healthy   { background: alpha(#2ec27e, 0.14); border-radius: 12px; }
.sv-banner-attention { background: alpha(#e5a50a, 0.16); border-radius: 12px; }
.sv-banner-critical  { background: alpha(#e01b24, 0.16); border-radius: 12px; }
.sv-banner-unknown   { background: alpha(#9a9996, 0.14); border-radius: 12px; }
.sv-headline { font-size: 19px; font-weight: 600; }
.sv-mono { font-family: monospace; font-size: 12px; }
"""
