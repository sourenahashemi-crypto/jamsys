"""Custom widgets: sparkline, per-core matrix, status pill, metric card.

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
gi.require_version("Pango", "1.0")
from gi.repository import Adw, Gdk, GLib, Graphene, Gsk, Gtk, Pango  # noqa: E402


def _rgba(css_class_color: str) -> Gdk.RGBA:
    c = Gdk.RGBA()
    c.parse(css_class_color)
    return c


def _rounded_path(x: float, y: float, w: float, h: float, r: float,
                  left: bool = True, right: bool = True) -> Gsk.Path:
    """A rounded rectangle as a path, with either end optionally left square.

    Built from a path rather than from ``Gsk.RoundedRect``: the rounded-rect
    nodes (``push_rounded_clip``, ``append_border``) are silently dropped by the
    renderer this application runs on — they paint nothing at all, which shows up
    as cells that are invisible until they are dark enough to see their text.
    Path fills and strokes render everywhere.
    """
    rl = min(r, w / 2.0, h / 2.0) if left else 0.0
    rr = min(r, w / 2.0, h / 2.0) if right else 0.0
    b = Gsk.PathBuilder()
    b.move_to(x + rl, y)
    b.line_to(x + w - rr, y)
    if rr:
        b.arc_to(x + w, y, x + w, y + rr)
    b.line_to(x + w, y + h - rr)
    if rr:
        b.arc_to(x + w, y + h, x + w - rr, y + h)
    b.line_to(x + rl, y + h)
    if rl:
        b.arc_to(x, y + h, x, y + h - rl)
    b.line_to(x, y + rl)
    if rl:
        b.arc_to(x, y, x + rl, y)
    b.close()
    return b.to_path()


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


# Per-core usage is a magnitude, not a status: a core pinned at 100% is a machine
# doing its job, not a machine in trouble. So the matrix is coloured from a single
# sequential blue ramp (light → dark) rather than from the green/amber/red status
# palette, which stays reserved for things that are actually wrong.
SEQ_BLUE = ("#cde2fb", "#b7d3f6", "#9ec5f4", "#86b6ef", "#6da7ec", "#5598e7",
            "#3987e5", "#2a78d6", "#256abf", "#1c5cab", "#184f95", "#104281",
            "#0d366b")

# The step at which the fill turns dark enough to need light text on it. Every
# cell keeps at least 4.4:1 against its own fill on either side of this line.
_INK_FLIP = 7
_INK_ON_LIGHT = "#0b0b0b"
_INK_ON_DARK = "#ffffff"


def _is_dark() -> bool:
    try:
        return bool(Adw.StyleManager.get_default().get_dark())
    except Exception:
        return False


def ramp_step(pct: float, dark: bool) -> int:
    """Index into SEQ_BLUE for a 0–100 value.

    On a light surface near-zero takes the palest step and recedes into the card;
    on a dark surface that reverses, so 'more ink' still means 'busier' in both
    themes rather than the ramp inverting its meaning.
    """
    v = min(100.0, max(0.0, pct if pct is not None and math.isfinite(pct) else 0.0))
    i = int(round(v / 100.0 * (len(SEQ_BLUE) - 1)))
    return (len(SEQ_BLUE) - 1 - i) if dark else i


def _cell_colors(step: int) -> tuple[Gdk.RGBA, Gdk.RGBA]:
    fill = _rgba(SEQ_BLUE[step])
    ink = _rgba(_INK_ON_DARK if step >= _INK_FLIP else _INK_ON_LIGHT)
    return fill, ink


class CoreMatrix(Gtk.Widget):
    """One box per logical CPU, laid out as a grid and shaded by how busy it is.

    The shape is the point: a whole-machine glance answers 'is one core pinned
    while the rest idle?' — the question a row of separate bars makes you read
    one bar at a time. Each box still prints its own percentage, so the colour
    is a second reading of the number and never the only carrier of it.
    """

    __gtype_name__ = "JamSysCoreMatrix"

    CELL_W = 58          # narrowest a cell may be squeezed
    CELL_W_MAX = 96      # widest, so a wide window gives a grid and not a stretched row
    CELL_H = 46
    GAP = 5
    RADIUS = 6.0
    MIN_COLS = 4         # the floor that keeps the minimum height sane
    MAX_COLS = 8

    def __init__(self):
        super().__init__()
        self._values: list[float] = []
        self.set_hexpand(True)
        self.set_has_tooltip(True)
        self.connect("query-tooltip", self._on_tooltip)

    def set_values(self, values: Sequence[float]) -> None:
        self._values = [float(v) if v is not None and math.isfinite(v) else 0.0
                        for v in values]
        self._describe()
        self.queue_resize()

    def summary(self) -> str:
        """In words, what the shape of the grid says."""
        if not self._values:
            return "Per-core usage: no cores reported"
        hot = max(range(len(self._values)), key=lambda i: self._values[i])
        return (f"Per-core usage for {len(self._values)} logical CPUs, "
                f"busiest cpu{hot} at {self._values[hot]:.0f} percent")

    def _describe(self) -> None:
        """A screen reader gets the summary a sighted user gets from the shape."""
        self.update_property([Gtk.AccessibleProperty.LABEL], [self.summary()])

    # -- geometry, shared by drawing and by hit-testing -------------------

    def _columns(self, width: float) -> int:
        n = len(self._values)
        if n <= 1:
            return max(1, n)
        fits = int((width + self.GAP) // (self.CELL_W + self.GAP))
        fits = max(1, min(n, self.MAX_COLS, fits))
        # Prefer a column count that divides the core count, so the last row is
        # full rather than ragged — a matrix with a stub row reads as an error.
        divisors = [c for c in range(1, fits + 1) if n % c == 0]
        return max(divisors) if divisors else fits

    def _cells(self, width: float):
        n = len(self._values)
        if n == 0:
            return []
        cols = self._columns(width)
        rows = -(-n // cols)
        cw = min(float(self.CELL_W_MAX), (width - self.GAP * (cols - 1)) / cols)
        # Centre the grid rather than letting eight cells stretch across a wide
        # window: a box that is twice as wide as it is tall stops reading as a cell.
        left = max(0.0, (width - (cols * cw + self.GAP * (cols - 1))) / 2.0)
        out = []
        for i in range(n):
            r, c = divmod(i, cols)
            out.append((left + c * (cw + self.GAP), r * (self.CELL_H + self.GAP),
                        cw, float(self.CELL_H)))
        return out

    def do_get_request_mode(self):
        return Gtk.SizeRequestMode.HEIGHT_FOR_WIDTH

    def do_measure(self, orientation, for_size):
        n = len(self._values)
        if orientation == Gtk.Orientation.HORIZONTAL:
            if n == 0:
                return (0, 0, -1, -1)
            def span(cols, w):
                return cols * (w + self.GAP) - self.GAP
            # The minimum is a real multi-column grid. Allowing a single column
            # would let GTK ask for the height of a 24-row stack and impose that
            # as the window's minimum height.
            lo = span(min(n, self.MIN_COLS), self.CELL_W)
            return (lo, span(min(n, self.MAX_COLS), self.CELL_W_MAX), -1, -1)
        if n == 0:
            return (0, 0, -1, -1)
        width = float(for_size) if for_size > 0 else float(
            min(n, self.MIN_COLS) * (self.CELL_W + self.GAP) - self.GAP)
        rows = -(-n // self._columns(width))
        h = rows * self.CELL_H + (rows - 1) * self.GAP
        return (h, h, -1, -1)

    def do_css_changed(self, change):
        # Following the system light/dark switch: the ramp is chosen per theme,
        # and nothing else would tell this widget the surface moved under it.
        Gtk.Widget.do_css_changed(self, change)
        self.queue_draw()

    def do_snapshot(self, snapshot):
        width = float(self.get_width())
        if width <= 1 or not self._values:
            return
        dark = _is_dark()
        border = _rgba(_INK_ON_DARK if dark else _INK_ON_LIGHT)
        border.alpha = 0.10

        for i, (x, y, w, h) in enumerate(self._cells(width)):
            v = self._values[i]
            fill, ink = _cell_colors(ramp_step(v, dark))
            path = _rounded_path(x, y, w, h, self.RADIUS)
            snapshot.append_fill(path, Gsk.FillRule.WINDING, fill)
            # A hairline keeps a near-idle cell from dissolving into the card.
            snapshot.append_stroke(path, Gsk.Stroke.new(1.0), border)

            name = self._layout(f"cpu{i}", 7.5, Pango.Weight.NORMAL)
            value = self._layout(f"{v:.0f}%", 11.0, Pango.Weight.SEMIBOLD)
            nw, nh = name.get_pixel_size()
            vw, vh = value.get_pixel_size()
            block = nh + vh
            top = y + (h - block) / 2.0
            faint = Gdk.RGBA()
            faint.red, faint.green, faint.blue, faint.alpha = ink.red, ink.green, ink.blue, 0.75
            self._text(snapshot, name, x + (w - nw) / 2.0, top, faint)
            self._text(snapshot, value, x + (w - vw) / 2.0, top + nh, ink)

    def _layout(self, text: str, size_pt: float, weight) -> Pango.Layout:
        layout = self.create_pango_layout(text)
        desc = layout.get_context().get_font_description()
        desc = desc.copy() if desc is not None else Pango.FontDescription()
        desc.set_size(int(size_pt * Pango.SCALE))
        desc.set_weight(weight)
        layout.set_font_description(desc)
        return layout

    @staticmethod
    def _text(snapshot, layout, x: float, y: float, color: Gdk.RGBA) -> None:
        snapshot.save()
        snapshot.translate(Graphene.Point().init(x, y))
        snapshot.append_layout(layout, color)
        snapshot.restore()

    def _on_tooltip(self, _w, x, y, _keyboard, tooltip) -> bool:
        for i, (cx, cy, cw, ch) in enumerate(self._cells(float(self.get_width()))):
            if cx <= x < cx + cw and cy <= y < cy + ch:
                tooltip.set_text(f"cpu{i} — {self._values[i]:.1f}% busy")
                r = Gdk.Rectangle()
                r.x, r.y, r.width, r.height = int(cx), int(cy), int(cw), int(ch)
                tooltip.set_tip_area(r)
                return True
        return False


class RampStrip(Gtk.Widget):
    """The matrix's scale: the same steps, in order, as a continuous bar."""

    __gtype_name__ = "JamSysRampStrip"

    def __init__(self, width: int = 132, height: int = 10):
        super().__init__()
        self._w, self._h = width, height
        self.set_valign(Gtk.Align.CENTER)

    def do_measure(self, orientation, for_size):
        if orientation == Gtk.Orientation.VERTICAL:
            return (self._h, self._h, -1, -1)
        return (self._w, self._w, -1, -1)

    def do_css_changed(self, change):
        Gtk.Widget.do_css_changed(self, change)
        self.queue_draw()

    def do_snapshot(self, snapshot):
        width, height = float(self.get_width()), float(self.get_height())
        if width <= 1 or height <= 1:
            return
        dark = _is_dark()
        n = len(SEQ_BLUE)
        step = width / n
        for i in range(n):
            fill = _rgba(SEQ_BLUE[(n - 1 - i) if dark else i])
            # Segments butt up against each other: a sequential scale is
            # continuous, so only the two ends are rounded.
            x = i * step
            w = (width - x) if i == n - 1 else (step + 1.0)
            snapshot.append_fill(
                _rounded_path(x, 0, w, height, height / 2,
                              left=(i == 0), right=(i == n - 1)),
                Gsk.FillRule.WINDING, fill)


def core_usage_legend() -> Gtk.Box:
    """'idle 0% ▁▂▃ 100% busy' — a sequential scale needs its ends named."""
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    box.set_halign(Gtk.Align.END)
    for text, widget in (("idle 0%", None), (None, RampStrip()), ("100% busy", None)):
        if widget is None:
            lab = Gtk.Label(label=text)
            lab.add_css_class("caption")
            lab.add_css_class("dim-label")
            box.append(lab)
        else:
            box.append(widget)
    return box


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
