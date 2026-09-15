#!/usr/bin/env python3
"""Render the JamSys window to PNG files, one per page.

The cluster has render-cluster.js for this; the window had nothing, so every
screenshot of it was a manual grab that nobody could reproduce and that went
stale the moment a page changed. This drives the real application against the
real daemon and renders through GTK's own renderer, so what lands in
docs/screenshots is what the code actually draws today.

    ./scripts/render-window.py docs/screenshots                 # light
    ./scripts/render-window.py /tmp --dark                      # dark
    ./scripts/render-window.py /tmp --pages CPU,Report          # a subset
    ./scripts/render-window.py /tmp --size 1180x760             # a given size

Screenshots of a system monitor carry whatever the monitor is showing --
hostname, process names, interface names. Read them before publishing them.
"""

import argparse
import os
import pathlib
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "jamsys-ui"))

import gi  # noqa: E402

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, GLib, Gtk  # noqa: E402

if not (os.environ.get("WAYLAND_DISPLAY") or os.environ.get("DISPLAY")):
    print("render-window: needs a display; GTK has to lay the window out to draw it",
          file=sys.stderr)
    raise SystemExit(1)

from jamsys_ui.app import JamSysApp  # noqa: E402

DEFAULT_PAGES = ("Overview", "CPU", "Report")


def pump(until, seconds=20.0):
    """Run the main loop until `until()` holds, or give up.

    Non-blocking iteration only drains what has already arrived, so waiting by
    counting iterations does not wait at all — the same trap page-refresh-test
    used to fall into. Block for the next event, with a throwaway timeout so the
    block is guaranteed to end.
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
        GLib.timeout_add(20, lambda: GLib.SOURCE_REMOVE)
        ctx.iteration(True)


def snapshot_widget(widget, pump_fn):
    """A render node for what the widget shows *now*.

    Not Gtk.WidgetPaintable: a paintable mirrors the last frame its widget
    actually painted, and a window the compositor considers occluded -- which is
    any window with a terminal in front of it -- stops painting altogether. The
    screenshots then lag a page behind the navigation, or catch the stack
    frozen mid-crossfade, and nothing in the output says so. Asking the parent
    to snapshot the child draws the widget tree as it stands instead.

    A widget that has not been allocated cannot be drawn ("without a current
    allocation"), and allocation happens on a frame tick, so this retries while
    the loop runs rather than giving up on the first attempt.
    """
    parent = widget.get_parent()
    if parent is None:
        return None
    clock = widget.get_frame_clock()
    if clock is not None:
        # Run the clock briefly before the first attempt. Navigation changes CSS
        # state -- which sidebar row is selected -- and that settles on a style
        # pass, not on the property change, so a snapshot taken too eagerly
        # shows the new page beside the old row highlighted.
        clock.begin_updating()
        pump_fn(lambda: False, 0.4)
        clock.end_updating()
    for attempt in range(20):
        snapshot = Gtk.Snapshot()
        parent.snapshot_child(widget, snapshot)
        node = snapshot.to_node()
        if node is not None:
            return node
        if clock is not None:
            # Ask for the layout phase explicitly and let the loop run it. On a
            # window the compositor is not sending frame callbacks to, this is
            # the only thing that moves the frame clock along.
            clock.begin_updating()
            pump_fn(lambda: False, 0.25)
            clock.end_updating()
        if attempt >= 4:
            # Still nothing: allocate it ourselves. Stepping over the parent's
            # layout is not how an application should behave, but this is a
            # screenshot harness and the alternative is no screenshot.
            width, height = widget.get_width(), widget.get_height()
            if width < 1 or height < 1:
                parent_alloc = parent.get_allocation()
                width, height = parent_alloc.width, parent_alloc.height
            if width > 0 and height > 0:
                widget.allocate(width, height, -1, None)
        pump_fn(lambda: False, 0.2)
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("outdir", type=pathlib.Path)
    ap.add_argument("--pages", default=",".join(DEFAULT_PAGES),
                    help=f"comma-separated page titles (default: {','.join(DEFAULT_PAGES)})")
    ap.add_argument("--dark", action="store_true", help="render in the dark theme")
    ap.add_argument("--size", default="1180x800", help="window size, WxH")
    ap.add_argument("--prefix", default="window", help="output file name prefix")
    ap.add_argument("--settle", type=float, default=3.0,
                    help="seconds to let a page finish loading before drawing it")
    args = ap.parse_args()

    width, _, height = args.size.partition("x")
    settle = args.settle
    args.outdir.mkdir(parents=True, exist_ok=True)

    Adw.init()
    Adw.StyleManager.get_default().set_color_scheme(
        Adw.ColorScheme.FORCE_DARK if args.dark else Adw.ColorScheme.FORCE_LIGHT)

    app = JamSysApp(None)
    # A private application id: the real one would hand this off to a window the
    # user already has open, and render nothing.
    app.set_application_id("org.jamsys.RenderWindow")
    app.register(None)
    app.activate()
    win = app.win
    if win is None:
        print("render-window: the window did not open", file=sys.stderr)
        return 1
    win.set_default_size(int(width), int(height))
    # No crossfade. The stack animates between pages on frame ticks, and an
    # occluded window gets no frame ticks -- the animation then never finishes
    # and every screenshot is two pages blended together. Switching instantly
    # makes what is on screen a function of the navigation and nothing else.
    win.stack.set_transition_type(Gtk.StackTransitionType.NONE)

    if not pump(lambda: bool(win.last_snapshot)):
        print("render-window: no snapshot arrived; is jamsysd running?"
              "  systemctl --user status jamsysd", file=sys.stderr)
        return 1

    renderer = win.get_renderer()
    content = win.get_content()
    written = []
    for title in [p.strip() for p in args.pages.split(",") if p.strip()]:
        if title not in win.pages:
            print(f"render-window: no page called {title!r}; have: "
                  + ", ".join(win.pages), file=sys.stderr)
            return 2
        win.goto(title)
        pump(lambda: win.stack.get_visible_child_name() == title)
        # Settle: pages fetch their history charts asynchronously, and a
        # screenshot taken too early shows "collecting…" where a chart belongs.
        pump(lambda: False, seconds=settle)
        node = snapshot_widget(content, lambda u, s: pump(u, seconds=s))
        if node is None:
            print(f"render-window: {title} produced no drawing", file=sys.stderr)
            return 1
        suffix = "-dark" if args.dark else ""
        out = args.outdir / f"{args.prefix}-{title.lower()}{suffix}.png"
        renderer.render_texture(node, None).save_to_png(str(out))
        written.append(out)
        print(f"  {out}  ({content.get_width()}x{content.get_height()})")

    win.close()
    app.quit()
    print(f"{len(written)} written. Read them before publishing: they show this "
          "machine's hostname, process names and interfaces.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
