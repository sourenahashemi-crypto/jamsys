# UI Architecture

Three surfaces, in descending order of how often you look at them.

| Surface | Technology | Purpose | Runs where |
|---|---|---|---|
| **Corner readout** | GNOME Shell extension (GJS) | The glance. Nearly invisible while healthy. | inside `gnome-shell` |
| **Full window** | GTK4 + libadwaita (PyGObject) | The investigation. Opened when something needs looking at. | its own process |
| **CLI** | `jamsys --json <op>` | Scripting and support. | its own process |

All three are clients. **None contains monitoring logic**; every number they show came
from the daemon. That is what allows the daemon to keep running with all of them closed,
and what would let any of them be replaced without touching the backend.

---

## 1. The corner readout

The most important surface, because it is the one that is on screen all the time.

### Calm by construction

* **It does not poll.** It subscribes to `StateChanged` on the session bus and repaints
  when told to. There is no timer in the extension at all.
* **The daemon decides what "changed" means.** Percentages are quantised to whole
  numbers, temperature to whole degrees, power to 0.1 W. A reading that would not alter
  a single rendered character produces no signal, so the panel does not repaint several
  times a second for noise. Measured on a live machine: 22 repaints over several minutes.
* **No animation, no graphs, no colour cycling.** A panel element that moves is a panel
  element you learn to tune out.
* While healthy the text is drawn at 55 % opacity so it reads as part of the panel.

### Two layouts

Compact — one line, the default:

```
CPU 7% · 52° | RAM 18% | GPU — | 11.2W | NET ✓
```

Minimal — stacked, for people who want to read it vertically:

```
CPU 7% · 52°
RAM 18%
GPU —
11.2W
NET ✓
```

`GPU —` rather than `GPU 0%` when the discrete GPU is suspended: zero would imply it was
measured, when in fact the card is powered down and was deliberately not queried.

### When something is wrong

The readings are **replaced**, not appended to. The spec's requirement is to show only
the important abnormal metric, so that is literally what happens:

```
⚠ POWER 29.8W
```

Clicking it opens a menu carrying the full explanation — measurement, expected range,
and an **Open JamSys** item that launches the window straight to the relevant page
via `jamsys --page Power`.

### Placement, and an honest limitation

GNOME Shell has **no bottom panel**. Top-left, top-centre and top-right insert into
`Main.panel`'s boxes and are completely robust.

Bottom-left and bottom-right are implemented with `Main.layoutManager.addChrome()` — the
same mechanism OSD popups use, not a fake always-on-top window, which cannot work under
Wayland at all. They work, but they are genuinely less robust than the panel: the readout
floats above the desktop, can overlap a dock, and is hidden by fullscreen windows. The
preferences dialog says so in the placement description rather than letting you discover
it. If you want it truly out of the way, use a top position.

### What the extension deliberately does not do

No sampling, no thresholds, no history, no D-Bus calls on a timer, no file reads, no
subprocess except launching the window on an explicit click. It is about 300 lines, and
the only part with real logic — turning a state object into a label — lives in
`format.js` so it can be unit-tested outside `gnome-shell`, where none of the Shell
imports resolve. 23 tests cover it.

---

## 2. The full window

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
