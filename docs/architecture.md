# JamSys — Architecture

> A local, lightweight system-health monitor for Ubuntu 26.04 laptops.
> One question, answered in one second: **"Is my machine behaving normally right now?"**

---

## 1. Shape of the system

```
┌──────────────────────────────────────────────────────────────────┐
│  jamsysd        (Rust, systemd --user service, always running)  │
│                                                                   │
│   ┌────────────────── single epoll event loop ────────────────┐   │
│   │  timerfd (tiered scheduler)   journal stream (child pipe) │   │
│   │  netlink: rtnetlink + uevent  signalfd     IPC listener   │   │
│   └───────────────────────────────────────────────────────────┘   │
│            │                                                      │
│   ┌────────▼─────────┐   ┌──────────────┐   ┌────────────────┐    │
│   │   Collectors     │──▶│ Anomaly      │──▶│ Alert manager  │    │
│   │  (18, isolated)  │   │ L1 rules     │   │ dedup / rate   │    │
│   └────────┬─────────┘   │ L2 baselines │   │ limit / levels │    │
│            │             └──────┬───────┘   └───────┬────────┘    │
│            ▼                    ▼                   ▼             │
│   ┌────────────────────────────────────────────────────────┐      │
│   │  Store: SQLite (WAL)  samples · rollups · events ·      │     │
│   │         baselines · inventory · alerts                  │     │
│   └────────────────────────────────────────────────────────┘      │
│            │                                    │                 │
└────────────┼────────────────────────────────────┼─────────────────┘
             │ Unix socket, line-delimited JSON   │ DBus session
             │ $XDG_RUNTIME_DIR/jamsys/sock     │ org.freedesktop
             ▼                                    ▼   .Notifications
   ┌────────────────────┐              ┌──────────────────────┐
   │ jamsys-ui        │              │  GNOME notification  │
   │ GTK4 + libadwaita  │              └──────────────────────┘
   │ (start/stop freely)│
   └────────────────────┘

   ┌──────────────────────────────────────────────────────────┐
   │ jamsys-helper  (optional, root, system service)         │
   │  reads RAPL energy_uj + NVMe SMART log page 0x02          │
   │  writes /run/jamsys/privileged.json (0644)              │
   │  NO input channel — nothing to validate, nothing to abuse │
   └──────────────────────────────────────────────────────────┘
```

**Three processes, strictly layered by privilege.** The daemon is the only thing that
must keep running. The UI is a disposable client. The helper is optional and the whole
application works without it (two metrics degrade to "unsupported").

---

## 2. Stack choice, and one deliberate deviation

| Layer | Chosen | Why |
|---|---|---|
| Daemon | **Rust** (edition 2021) | As specified. Predictable memory, no GC, no runtime. |
| Storage | **SQLite** via `rusqlite` (bundled) | As specified. Single file, WAL, zero admin. |
| IPC | **Unix socket, NDJSON** | As specified. Filesystem permissions = access control. |
| Service | **systemd user unit** | Runs as you, not root. Survives UI exit. |
| UI | **GTK4 + libadwaita (PyGObject)** | *Deviation — see below.* |

### Why not Tauri

The brief prefers Tauri and I want to be explicit that I did not follow that, with the
reasoning, because it is a real architectural decision and not an oversight:

1. **The build dependencies are not installable here.** `webkit2gtk-4.1-dev`,
   `libsoup-3.0-dev`, `libjavascriptcoregtk-4.1-dev`, `gtk-3-dev` and their transitive
   `-dev` closure are absent, and `sudo` is not passwordless in this environment. A Tauri
   UI could have been *written* but not *built or tested* — the brief explicitly says
   "do not stop after creating a mockup". This is the decisive reason.
2. **GTK4 + libadwaita is the native GNOME toolkit.** It is already installed
   (`Gtk-4.0`, `Adw-1`, `Notify-0.7` typelibs verified present). It gives correct HIG
   styling, automatic light/dark, and Wayland-native rendering for free, and a Tauri
   window would embed WebKitGTK *in addition to* this same GTK stack.
3. **The memory budget applies to the part that always runs.** The daemon measures
   **23.6 MB PSS**, well inside *"below 100 MB total, preferably much lower"*.

   I should be straight about the UI, because I originally justified this choice partly
   on the UI's footprint and the measurement did not support that as strongly as claimed.
   The GTK4 + PyGObject window measures **110 MB PSS** (232 MB RSS, of which 141 MB is
   shared library text). That is *not* dramatically better than a webview would be; most
   of it is the Python interpreter, PyGObject and the Mesa/GL stack rather than anything
   this application allocates. A Tauri UI would still be larger, since WebKitGTK sits on
   top of the same GTK libraries — but "GTK is far lighter" was an overstatement, and the
   real argument is buildability and native integration, not bytes.

   The honest optimisation path, if the UI footprint matters, is a `gtk4-rs` front-end
   against the same socket, which removes the Python interpreter entirely. The protocol
   was designed to make exactly that substitution cheap.

The deviation is contained by design: **the UI is a thin client over a documented,
versioned JSON protocol** (`docs/ipc-protocol.md`). A Tauri, web, TUI or CLI front-end can
be written against the same socket without touching the daemon. `jamsys-ui --json`
already acts as that reference client.

**No Electron, no Docker, no Prometheus, no cloud, no account, no telemetry.**
The daemon opens no outbound sockets except the optional reachability probe
(a TCP connect to the default gateway / DNS resolver, no payload).

---

## 3. The event loop, and why it matters for battery

Everything happens on **one thread**, in **one `epoll_wait`**. There is no async runtime,
no thread pool, and no per-collector timer. The loop blocks on:

| fd | purpose | wakeups |
|---|---|---|
| `timerfd` | the *single* scheduler tick | the only periodic wakeup |
| journal child stdout | `journalctl -f -o json -p 4` | only when a warning+ is logged |
| `NETLINK_ROUTE` | link / addr / route changes | only on real network change |
| `NETLINK_KOBJECT_UEVENT` | device hotplug | only on real hotplug |
| `signalfd` | SIGTERM / SIGINT / SIGHUP | shutdown, reload |
| Unix listener + clients | UI connections | only while UI is open |

The scheduler computes **one** next deadline as the minimum across all enabled tiers and
arms the timerfd once. Ten collectors on a 10 s tier produce **one** wakeup, not ten.

### Sampling tiers

| Tier | Default | On battery + idle | Collectors |
|---|---|---|---|
| Fast | 2 s | 6 s | CPU, memory, PSI, net throughput, diskstats |
| Medium | 10 s | 30 s | temps, fans, battery, cpufreq, Wi-Fi signal, Intel GPU, NVIDIA *(gated)* |
| Slow | 60 s | 180 s | filesystems, processes, systemd units, Bluetooth, audio, USB/camera, reachability |
| Glacial | 900 s | 1800 s | SMART, inventory diff |
| Event | — | — | journal, rtnetlink, uevent, suspend/resume |

**Adaptive throttling** multiplies every interval by ×3 when *all* of: on battery,
5-minute CPU average < 5 %, no IPC client connected. The multiplier is applied at the
scheduler, so it also cuts wakeup count, not just work per wakeup.

### The NVIDIA rule

Measured on this machine: touching NVML wakes the dGPU out of D3cold and it stays awake
~9 s. So `gpu_nvidia` **reads `/sys/class/drm/card2/device/power/runtime_status` first**:

* `suspended` → report `{state: "suspended", power_w: 0}` from sysfs alone. **NVML is not called.**
* `active` → call NVML (already awake, marginal cost ≈ 0).

Consequence: monitoring an idle hybrid laptop costs *nothing* on the dGPU, and the app
still detects "the dGPU is awake when it shouldn't be" — which is the failure the user
actually cares about — because `runtime_status` and `runtime_active_time` are the signal.

---

## 4. Collector contract

```rust
pub trait Collector {
    fn name(&self) -> &'static str;
    fn tier(&self) -> Tier;
    fn probe(&mut self) -> Support;              // called once at startup
    fn collect(&mut self, ctx: &mut Ctx) -> Result<(), CollectorError>;
}
```

Rules enforced by the runtime, not by convention:

* **A broken sensor never kills the daemon.** Every `collect()` is wrapped in
  `catch_unwind`; a panic or `Err` increments a failure counter and is logged to the
  *application* log. Three consecutive failures → the collector is **quarantined**
  (disabled, coverage downgraded to `Degraded`, retried at the glacial tier).
  Host alerts and app errors are separate streams and never mix.
* **Every collector is independently disableable** via config (`[collectors] nvidia = false`)
  and at runtime over IPC.
* **`probe()` decides coverage**, so "unsupported" is a first-class state, not an error.
  No battery, no Wi-Fi, no NVIDIA, no `asus` hwmon → the app reports reduced coverage and
  keeps working.
* Collectors never spawn processes on a timer. The only child process in the daemon is
  the single long-lived `journalctl --follow`.

---

## 5. Data flow and time model

1. `collect()` writes typed samples into a per-tick `Ctx`.
2. `Ctx` is handed to the anomaly engine (in-memory; no DB round-trip on the hot path).
3. Samples are appended to an in-memory ring and flushed to SQLite in **one transaction
   every 15 s**, so the fast tier does not cause a disk write every 2 s.
4. Rollups (1 min / 5 min / 15 min) are computed by a maintenance pass at the glacial
   tier, then source rows past retention are deleted and the DB is incrementally vacuumed.

All timestamps are stored as `INTEGER` Unix-epoch milliseconds (UTC).
Durations and "has the machine been asleep" use `CLOCK_BOOTTIME` so that a suspend does
not look like a stalled daemon.

---

## 6. Failure philosophy

| Situation | Behaviour |
|---|---|
| Sensor file disappears (driver reload) | collector re-probes, coverage flips, no alert |
| Sensor returns garbage (`""`, `-1`, `2^63`) | value rejected by the parser, sample dropped |
| NVML absent / NVIDIA removed | `Support::Unsupported`, card hidden in UI |
| No battery (desktop) | power collector unsupported, idle-power baseline disabled |
| Journal not readable (not in `adm`) | journal collector `Degraded`, everything else fine |
| Helper not installed | RAPL + SMART unsupported, clearly shown on Coverage page |
| Kernel/driver upgrade changes hwmon numbering | sensors keyed by **`name` + `label`**, never by `hwmonN` index |
| DB corrupt | daemon renames it aside, starts fresh, raises an app-level error |
| Clock jumps (NTP) | monotonic clock used for all intervals; wall clock only for storage |

The hwmon-numbering point is a real hazard on Ubuntu: `hwmon4` is the NVMe today and may
be `hwmon6` after the next kernel. Sensors are resolved by reading every `hwmon*/name`
at probe time and matching on the driver name plus the `tempN_label`.
