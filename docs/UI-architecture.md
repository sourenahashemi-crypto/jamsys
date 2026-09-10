# UI Architecture

Three surfaces, in descending order of how often you look at them.

| Surface | Technology | Purpose | Runs where |
|---|---|---|---|
| **Corner cluster** | GNOME Shell extension, Cairo (GJS) | The glance. An instrument binnacle in a screen corner. | inside `gnome-shell` |
| **Cluster window** | GTK4 + Cairo (GJS) — *the same drawing* | The same cluster as an ordinary window. Works with no session restart, and on desktops that are not GNOME. | its own process |
| **Full window** | GTK4 + libadwaita (PyGObject) | The investigation. Opened when something needs looking at. | its own process |
| **CLI** | `jamsys --json <op>` | Scripting and support. | its own process |

All three are clients. **None contains monitoring logic**; every number they show came
from the daemon. That is what allows the daemon to keep running with all of them closed,
and what would let any of them be replaced without touching the backend.

---

## 1. The corner cluster

A desktop instrument binnacle that lives in a screen corner. It is the surface you
actually look at, so it gets the most design attention and the least CPU.

```
          ╭──────────────────────────────────────────────────────╮
          │                   J A M S Y S                        │
          │    ╭────╮        ╭──────────╮        ╭────╮          │
          │    │RAM │        │   CPU    │        │GPU │          │
          │    │ 18%│        │  7%  52° │        │ —  │          │
          │    ╰────╯        ╰──────────╯        ╰────╯          │
          │  PWR ▮▮▮▯▯▯▯▯  11.2W  BAT ▮▮▮▮▮▯ 87%  NET SVC SYS    │
          ╰──────────────────────────────────────────────────────╯
```

### The look

Automotive, deliberately: a swept 270° tachometer for CPU load with a green-amber-red
band and a redline, flanked by smaller auxiliary dials for memory and GPU. Machined
bezels, a domed dial face, precise tick cadence, and a tapered needle with a
counterweight tail and a lit bloom underneath it.

Two details that make it read as an instrument rather than a chart:

* **The value sits in a recessed digital window drawn over the needle.** Without it the
  needle sweeps across the number at exactly the loads you most want to read. Modern
  clusters solve this the same way, with an inset LCD.
* **Captions live in the 90° sector at the bottom that carries no ticks**, and numerals
  ride just inside the tick ring. Nothing is typeset where the needle or another label
  will land on it.

Auxiliary dials carry **no numerals**. A 46-pixel gauge with numbers on it is
unreadable, and real clusters leave them off too — the coloured band does that work.

### Calm, even so

An instrument cluster is inherently more visually present than a line of text, so the
restraint has to come from behaviour rather than from hiding:

* **It does not poll.** It subscribes to `StateChanged` and repaints when told to.
  There is no timer in the extension at all.
* **The daemon decides what "changed" means** — percentages to whole numbers,
  temperature to whole degrees, power to 0.1 W. A reading that would not alter a single
  drawn pixel produces no signal.
* **Nothing animates.** No sweeping needles on startup, no pulsing, no glow cycles.
* **Telltales are dark until they have something to say.** That is the entire point of
  a warning lamp, and they are the only elements that ever light up.
* Housing opacity is adjustable down to 0.35, so it can sink into the wallpaper.

### What each instrument says

| | |
|---|---|
| Centre dial | CPU load, redlined at 90 %. Needle turns red on a critical alert. |
| Below it | CPU package temperature, coloured: cyan under 75 °C, amber to 90 °C, red above |
| Left dial | Memory in use |
| Right dial | GPU utilisation — or a dash on a dimmed dial when the discrete GPU is asleep, because zero would imply it was measured |
| PWR bar | System draw, 0–60 W full scale, segmented rather than smooth. Cyan and labelled CHG while charging. |
| BAT | Charge, red below 15 %. Absent entirely on a machine with no battery, which says "no battery sensor" rather than drawing 0 W. |
| NET / SVC / SYS | Telltales |
| Header | The product name, or the single most important alert in full |

### Placement, and an honest limitation

The cluster is always a `Main.layoutManager` chrome actor — the same mechanism OSD
popups use, rather than an ordinary window pretending to be chrome. All four corners
plus top-centre are available, with an adjustable edge margin. (A *window* can be kept
above others, but only over XWayland and only by asking the window manager; that is
how `jamsys-cluster` does it, described below. An in-Shell actor needs none of that.)

It floats above the desktop. It can overlap a dock, and it is hidden by fullscreen
windows. That is inherent to being a desktop gadget on GNOME, and it is stated in the
preferences dialog rather than left to be discovered.

### The same cluster, as a window

`jamsys-cluster` puts the identical drawing in a plain GTK4 window. It exists because
of three real constraints:

1. **GNOME Shell caches a loaded extension module.** Installing or fixing an extension
   has no effect until the Shell restarts, which under Wayland means logging out.
   `disable`/`enable` reuses the cached module and `ReloadExtension` is a stub in
   GNOME 50. This window shows the cluster immediately.
2. **The Shell extension only runs on GNOME.** This runs anywhere GTK4 does.
3. It is the fastest way to iterate on the drawing, because it is live rather than a PNG.

It is undecorated and sized to its contents, with the whole face acting as a drag
handle (`Gtk.WindowHandle`), so it behaves like a gadget rather than an application.
Clicking it opens the full window on the relevant page; Escape closes it.

```bash
jamsys-cluster                          # remembered size, or 546x250 on first run
jamsys-cluster --scale 1.9 --opacity 0.8
jamsys-cluster --on-top --all-workspaces
jamsys-cluster --decorated              # keep a title bar
jamsys-cluster --reset                  # forget remembered settings
```

#### The cut-out cluster

The default face has no housing: four dials sit directly on the wallpaper with one
slim capsule for the readings that are not dials.

The four are RAM, CPU, **iGPU** and **dGPU**. The two graphics processors get their
own instruments rather than one averaged "GPU" number, because on a hybrid laptop
the interesting question is usually *which* of them is working -- and because they
have independent power states. The dGPU dial dims to a dash and reads "asleep" while
the card is suspended, and it is never woken to be read.

iGPU busy is `100 - RC6 residency`. That is a proxy, not a hardware busy counter --
i915 exposes none without `CAP_PERFMON` -- so the dial carries its clock underneath
rather than implying more precision than exists.

The capsule carries download and upload rates for the active interface, watts,
battery, and the telltales. The old power segment bar was dropped to make room: the
numeric watts already said the same thing. Rates are formatted by `rate()`, whose
precision follows magnitude (`1.5 kB/s`, `46 MB/s`) and which never returns more than
three digits, so the strip cannot grow and shove the telltales off the end. The rectangular panel had been doing
two jobs — grouping the instruments, and giving them a legible ground. Grouping three
objects in a row is unnecessary. A legible ground is not, so each dial gets a soft
halo instead: six concentric translucent rings, because Cairo has no blur. It was
reviewed over a light wallpaper as well as a dark one, which is where a cut-out design
actually fails.

There is no permanent header. A gadget that displays its own name at all times is
wasting the space; the name is replaced by an alert pill that appears only when
something is wrong.

`--housing` brings the old panel back, and the right-click menu has the same toggle.

**Click-through is available but off by default.** With no panel there is no
rectangle to click, so in principle the gaps should belong to whatever is behind the
gadget, and `bareHitRegions()` -> `Gdk.Surface.set_input_region()` does exactly that.

It is off by default because on this compositor it costs far more than it buys.
Measured on GNOME Shell 50.1 / Mutter with an XWayland surface: once the input region
is shaped, `_NET_ACTIVE_WINDOW` is ignored so the window can never be focused again,
and pointer clicks stop arriving even at coordinates inside the region Mutter itself
reports through `XShapeGetRectangles(..., ShapeInput, ...)`. The result was a gadget
that could not be focused, clicked, or closed. A gadget you cannot close is a worse
bug than a gadget that occupies a rectangle, so this is now a menu item that says
what it costs, and turning it on shows a dialog naming `jamsys-cluster --quit` as the
way back.

#### Closing it

Three ways, in order of how likely they are to work when something else has gone
wrong:

1. **The close button**, top right. It is a real `Gtk.Button` in a `Gtk.Overlay`
   above the drawing area -- *not* a shape painted into the canvas and hit-tested by
   hand. That was tried first and does not work: the whole face is a
   `Gtk.WindowHandle` so it can be dragged from anywhere, and a press inside a window
   handle is a candidate window drag, so the handle claims the gesture and the child
   never sees a click. A widget in an overlay sits outside the handle's subtree and
   gets the press directly.
2. **Escape or Ctrl+Q**, when the gadget has keyboard focus.
3. **`jamsys-cluster --quit`**, which needs neither focus nor a working pointer. The
   window writes its pid to `$XDG_RUNTIME_DIR/jamsys-cluster.pid` and `--quit` checks
   `/proc/<pid>/cmdline` still refers to the cluster before signalling, so a recycled
   pid is never hit.

#### Size

The default is scale 1.3 (546x229 px in cut-out mode), not 1.0. At 1.0 the main dials are readable but
the secondary numerals are not, on a 1920x1200 laptop panel at a normal viewing
distance — which is the whole point of a gadget you glance at.

It resizes by **scroll wheel**, by `+` / `-`, and by **Ctrl+drag** anywhere on the
face — free transform, rather than a ladder of fixed sizes. `0` returns to the
default and scale is clamped to 0.6-4.0. The drag gesture runs in the *capture* phase
because the whole face is a `Gtk.WindowHandle`, which would otherwise claim the drag
for moving the window before the gesture saw it; it denies the sequence immediately
unless Ctrl is held, so an ordinary drag still moves the gadget. The window is freely
resizable, so its aspect will rarely match the instruments exactly; `drawCluster`
scales to fit and **centres**, letterboxing inside the housing, because anchoring the
instruments to a corner of their own housing looks like a bug.

Size, opacity, decoration and stacking are remembered in
`~/.config/jamsys/cluster.json`, written debounced one second after the last change so
a scroll gesture does not write the file on every tick.

#### Two ways it used to break when resized

**It could be maximized by accident.** The whole face is a `Gtk.WindowHandle` so that
it can be dragged from anywhere, but a window handle also behaves like a title bar,
and GNOME's default `action-double-click-titlebar` is `toggle-maximize`. One
double-click pinned the gadget full-screen -- and with no title bar there was nothing
to double-click back. Worse, a maximized window ignores `set_default_size`, so every
subsequent resize silently did nothing. The window now refuses both maximize and
fullscreen (`notify::maximized` -> `unmaximize()`), and `applySize()` un-maximizes
before resizing.

**Shortcuts died under a non-Latin keyboard layout.** They were matched on keysym, and
with an `ir` layout active the "0" key delivers `Farsi_0` (0x10006f0) rather than 48,
so "reset the size" stopped working. `-` and `=` kept working purely by luck: those
two keysyms are identical in both groups. Shortcuts now match the *physical key* --
`Gdk.Display.map_keycode()` gives every keyval a keycode can produce in any group, and
the shortcut fires if the wanted one is among them.

#### Always on top, and how it actually works

Wayland has no protocol for a client to raise itself, and GTK4 dropped
`gtk_window_set_keep_above` with the rest of the X11-only API. The route that does work
on GNOME is EWMH applied to an **XWayland** window: Mutter honours
`_NET_WM_STATE_ABOVE` and `_NET_WM_STATE_STICKY` for X11 clients.

So `jamsys-cluster` runs on XWayland by default (`GDK_BACKEND=x11`; `--wayland` opts
out). The window is a 32-bit ARGB visual either way, so the rounded transparent housing
is unchanged. The right-click menu then offers two real toggles:

| Menu item | Atom | Effect |
|---|---|---|
| Always on top | `_NET_WM_STATE_ABOVE` | stays above ordinary windows |
| On all workspaces | `_NET_WM_STATE_STICKY` | visible on every workspace |

Changing that state requires a `ClientMessage` to the root window — writing the
property directly, as `xprop -set` does, is ignored by the window manager for mapped
windows. Nothing introspectable exposes `XSendEvent`, so GJS cannot do it, and the
call goes through `jamsys-xabove`, a ~40-line Python sidecar using `ctypes` and
`libX11`. It is **unprivileged**: no root, no Polkit, a closed vocabulary of two
states, a numeric window id, and `on`/`off` — validated before the display is opened.
See `jamsys-ui/tests/xabove-test.py`.

Under a native Wayland surface (`--wayland`) both toggles are greyed out and explain
why, rather than silently doing nothing.

Measured on GNOME Shell 50.1 / Mutter, Ubuntu 26.04: with `ABOVE` set, the cluster
remained topmost across three explicit raises of a competing window; with it cleared,
the competing window went back on top. The stacking order was read with `XQueryTree`,
which is bottom-to-top by protocol, rather than from `xwininfo`'s printed order.

The one limitation that remains: **Wayland gives an ordinary window no control over its
own position**, so the compositor decides where it first appears and you drag it to the
corner you want. The Shell extension does not have that problem, which is why it
remains the primary surface.

One copy of the rendering, three consumers: the Shell widget, this window, and the PNG
harness. That is why `standalone.js` lives beside the extension rather than in its own
directory — `gauges.js` is not duplicated.

### How it was designed without being able to see it

GNOME Shell will not load a newly installed extension on Wayland without a session
restart, so the cluster could not simply be looked at. The drawing therefore lives in
`gauges.js` with **no Shell imports at all**: inside the Shell the Cairo context comes
from an `St.DrawingArea` repaint, and `tests/render-cluster.js` feeds the identical code
an `ImageSurface` and writes a PNG. Every state — idle, load, warning, critical, on AC,
no battery, and live against the running daemon — was rendered and reviewed that way.

Three defects were found and fixed by looking at those renders: captions colliding with
numerals, the needle crossing the value, and the `100` numeral being clipped by the
digital inset. None would have been caught by a test.

## 2. The panel line

The two line styles put a text readout in the top panel instead of a floating cluster,
for people who would rather have nothing on the desktop:

```
CPU 7% · 52° | RAM 18% | GPU — | 11.2W | NET ✓        ← compact
⚠ POWER 29.8W                                          ← abnormal replaces it
```

Same rules: pushed, not polled; the abnormal state replaces the readings rather than
being appended to them.

## 3. The full window

GTK4 + libadwaita, following the GNOME HIG. One `AdwApplicationWindow` with an
`AdwNavigationSplitView`: a sidebar of subsystems, and a content pane.

## The one-second answer

The top of the Overview is a single `AdwBanner`-style status strip that is the whole point
of the application:

```
┌──────────────────────────────────────────────────────────────┐
│  ●  Everything is normal                                     │
│     All 14 monitored subsystems healthy · no alerts in 6 h   │
└──────────────────────────────────────────────────────────────┘
```

Three states, and the colour is never the only signal (icon + text carry it too, for
accessibility and for the ~8 % of men with colour-vision deficiency):

| State | Icon | Colour | Meaning |
|---|---|---|---|
| **Healthy** | ✓ circle | green | no open alerts above INFO |
| **Attention needed** | ! triangle | amber | ≥ 1 open NOTICE or WARNING |
| **Critical** | ✕ octagon | red | ≥ 1 open CRITICAL |

Degraded coverage (a quarantined collector, helper missing) is shown as a separate
neutral line — a monitoring gap is not a system fault, and conflating them is how users
learn to distrust the status light.

## Overview: the card grid

`AdwFlowBox` of eight cards, each `GtkDrawingArea` sparkline + three text rows:
**current value · learned normal range · status word**.

```
┌─ CPU ───────────┐ ┌─ Memory ────────┐ ┌─ NVIDIA ────────┐ ┌─ Battery ───────┐
│ 8%              │ │ 5.2 / 30.4 GB   │ │ Suspended       │ │ 87%             │
│ 58 °C           │ │ 17% · swap 0    │ │ 0 W · P8        │ │ 11.4 W · 7h18m  │
│ ▁▂▁▁▃▁▁▂  Normal│ │ ▂▂▂▃▃▃▃▃  Normal│ │ ▁▁▁▁▁▁▁▁   Idle │ │ ▇▆▆▅▅▄▄▃  Normal│
└─────────────────┘ └─────────────────┘ └─────────────────┘ └─────────────────┘
┌─ Temperature ───┐ ┌─ Disk ──────────┐ ┌─ Network ───────┐ ┌─ Services ──────┐
│ CPU 58 °C       │ │ / 46% used      │ │ Wi-Fi connected │ │ 1 failed unit   │
│ NVMe 34 °C      │ │ NVMe 34 °C      │ │ −41 dBm · 0.0%  │ │ (1 ignored)     │
│ fans 2300/2400  │ │ ▁▁▃▁▁▁▁▁  Normal│ │ ▁▂▁▁▅▂▁▁  Normal│ │ ✓ Normal        │
└─────────────────┘ └─────────────────┘ └─────────────────┘ └─────────────────┘
```

Sparklines are 60 points from the 1-minute rollup — cheap to fetch, and they answer
"is this new?" which a bare number cannot. A card whose subsystem is unsupported is
rendered dimmed with "Not available on this machine", never hidden — silently missing
cards make users think a subsystem is fine when it is simply unmonitored.

Below the grid: **Recent events & alerts**, a reverse-chronological `AdwPreferencesGroup`
of rows, each expandable to the full explanation.

## Navigation

| Page | Contents |
|---|---|
| **Overview** | the above |
| **CPU** | total + per-core heatmap, load, frequency, temp, throttle counters, top CPU processes |
| **Memory** | used/cache/swap stacked chart, PSI, OOM history, top RSS processes, leak candidates |
| **GPU** | one section per GPU. Intel: freq, RC6 residency, throttle reasons. NVIDIA: util, VRAM, temp, power, P-state, clocks, processes, **runtime PM state** and *which GPU drives the display* |
| **Power** | battery %, W now vs learned idle band, voltage, energy full vs design, health %, cycles, estimated runtime, AC state, discharge-rate history, suspend-drain history |
| **Thermals** | every discovered sensor with its label, fan RPM, throttle events, critical points |
| **Storage** | real filesystems only (loop/snap collapsed into one row), inodes, IO throughput, `io_ticks` utilisation, NVMe SMART when available, ext4 error counters |
| **Network** | per interface: state, IPs, throughput, errors, drops; Wi-Fi signal/bitrate; default route; reachability trio (gateway / DNS / internet); **event timeline** |
| **Devices** | Bluetooth adapter + connections, audio (PipeWire/WirePlumber/default sink+source), USB tree, webcam + IR camera presence, expected-device list |
| **Services** | failed units, restart counts, per-unit ignore toggle |
| **Processes** | sortable table: PID, name, CPU %, RSS, IO, GPU; abnormal rows flagged with the reason |
| **Events** | filterable log of events + alerts with severity chips |
| **Hardware** | keyboard backlight and RGB: brightness, colour picker, five presets, mode, speed, and which power states stay lit. The only page that *changes* anything. |
| **Coverage** | the honesty page — see below |
| **Diagnostics** | what the monitoring application itself costs: daemon CPU, memory, wakeups, events processed and dropped, database size, in-memory buffer, repaints pushed, UI memory |
| **Settings** | tiers, retention, thresholds, suppressions, notifications, per-collector enable |

## The network event timeline

Exactly as the brief asks, because "when did it break" is the actual question:

```
13:42:07  ⚠  Wi-Fi disconnected            wlp108s0 · carrier lost
13:42:11  ●  Wi-Fi reconnected             4 s outage · −47 dBm
13:43:02  ✓  Default route restored        via 192.168.1.1
13:43:04  ✓  Internet reachable            DNS 12 ms
```

## Coverage page

A monitor that quietly does not monitor something is worse than no monitor. Every metric
declares its state, resolved at probe time on *this* machine:

| | |
|---|---|
| CPU / load / frequency / throttling | **Full** |
| Memory / swap / PSI / OOM | **Full** |
| Intel GPU (freq, RC6, throttle reasons) | **Full** |
| NVIDIA GPU (NVML, gated on runtime PM) | **Full** |
| Battery / AC / power draw | **Full** |
| CPU package power (RAPL) | **Unavailable** — needs privileged helper |
| Thermals (coretemp, acpitz, NVMe, ASUS fans) | **Full** |
| NVMe temperature | **Full** |
| NVMe SMART / media errors | **Unavailable** — needs privileged helper |
| Filesystems / inodes / IO / ext4 errors | **Full** |
| Wi-Fi (state, signal, errors, events) | **Full** |
| Ethernet | **Full** (link down) |
| Reachability (gateway / DNS / internet) | **Full** |
| Bluetooth | **Basic** — adapter, rfkill, connection count |
| Audio | **Service/device health** — PipeWire, WirePlumber, sinks |
| Webcam / IR camera | **Presence** |
| USB devices | **Presence + hotplug events** |
| systemd units | **Full** |
| Journal (kernel, Xid, OOM, I/O, thermal) | **Full** (via `adm` group) |
| Suspend / resume | **Full** (BOOTTIME-derived) |
| Thunderbolt | **Not present on this machine** |

Each row expands to show the exact interface used and, if unavailable, the one command
that would enable it. Quarantined collectors appear here with their failure reason.

## Alert interaction

Every alert row has an overflow menu with precisely the four actions in the brief:

* **Mute** this alert (stop notifying, keep tracking)
* **Snooze** — 1 h / 8 h / 24 h
* **Ignore this rule** / **Ignore this device or service** (writes a `suppression` row)
* **Change threshold…** — a spin row pre-filled with the threshold currently in force

Suppressions are listed and reversible in Settings, with the reason and who set it. A
muted alert still shows in the UI with a muted badge — hiding it entirely is how people
forget they turned something off.

## The Hardware page

The only page with write access to anything, so it is built to be obvious about it.

* Controls are **insensitive** unless a write path actually exists, and the page states
  which one is in use — `jamsys-kbd via Polkit`, `direct sysfs write (udev rule
  installed)`, or `not available` with both ways to enable it.
* It shows the **kernel's own field order** (`cmd mode red green blue speed`, read from
  `kbd_rgb_mode_index`) next to the controls, so the mapping between what you click and
  what gets written is auditable rather than magic.
* `kbd_rgb_mode` is write-only in the kernel, so the current colour **cannot** be read
  back. The page says exactly that, and labels the swatch as "what JamSys last set"
  rather than presenting a remembered value as a measurement.
* Failures surface as a toast carrying the helper's own error text, including
  "Authorisation was declined" when a Polkit prompt is dismissed.

## Notifications

`org.freedesktop.Notifications` via the daemon (so notifications work with the UI closed).
Urgency maps INFO/NOTICE → low, WARNING → normal, CRITICAL → critical (which persists on
screen). Body text is the first two lines of the explanation, always with real numbers.
Notification actions map to Snooze 1 h and Open. No sound by default.
