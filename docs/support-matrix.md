# Support Matrix

What JamSys actually does on the target machine, what it half-does, what it does not
do, and where the sharp edges are.

The rule applied throughout: **a capability is only claimed as working if it is wired to
a real data source and that path has been executed on this hardware.** Where code exists
but the path could not be exercised, it is listed as unverified — not as working.

Historical hardware evidence below was recorded by the 2026-09-09 audit on ASUS
TUF F16 FX608JMR, Ubuntu 26.04.1, kernel 7.0.0-31, GNOME Shell 50.1, Wayland.
It was not independently repeated in the 2026-09-10 repository review. That review
ran the automated suites, read the already-running daemon over D-Bus, rendered the
new offline face, and built the package. It did not install changes or exercise
hardware controls. See [the verification report](verification-2026-09-10.md).

---

## 1. Fully working

Wired, running, and observed producing correct values on this machine.

### Monitoring

| Capability | Evidence |
|---|---|
| CPU total, per-core, load, processes, context switches | 24 cores; 99.8 % observed under injected load |
| CPU frequency, governor, scaling driver | 2 258 → 2 133 MHz observed as the chip heated |
| CPU package and per-core temperature | 53 → 71 °C tracked through a 340 s load |
| CPU throttle counters | monotonic deltas, not guessed from frequency |
| CPU / IO / memory stall (PSI) | all three parsed; stayed 0.00 under a non-stalling 8 GiB allocation |
| Memory used, available, cache, swap, dirty | 76 % → 50 % → 76 % across an allocation test |
| OOM kill counter | delta-triggered; a historical total never re-alerts |
| Major page faults | live rate |
| Intel GPU frequency, RC6 residency, 8 throttle-reason flags | 700 MHz, RC6 15 % |
| Which GPU drives the display | correctly reports Intel |
| NVIDIA runtime power state and awake/suspended time | the gate everything else depends on |
| NVIDIA utilisation, VRAM, temperature, power, P-state, clocks | 6 %, 6.8 W, 47 °C observed under an offloaded render |
| NVIDIA wake/sleep transition events | `dgpu_wake` / `dgpu_sleep` recorded |
| Per-process GPU engine time | Xwayland 1.08, gnome-shell 0.27 engine-s/s |
| Battery charge, status, voltage, energy, cycles | 16.87 V, 55.6/91.2 Wh |
| Battery health against design capacity | 101.3 % — reported above 100 rather than clamped |
| System power draw | 72.1 W charging, 21.7 W discharging observed |
| AC transitions | `Discharging → Charging` event recorded live |
| All hwmon temperatures, keyed by driver + label | ~30 channels across 9 devices |
| Fan speeds | `cpu_fan` 2 400, `gpu_fan` 2 500 rpm |
| NVMe temperature | 39.9 → 41.9 °C after a 3 GB write |
| Filesystems, free space, inodes | 73 pseudo/snap mounts correctly hidden |
| Disk throughput, IOPS, utilisation, service time | 53.9 MB/s observed |
| ext4 error counters | read; 0 on this machine |
| Network interfaces, state, addresses, throughput | Realtek Wi-Fi and Ethernet |
| Packet errors and drops | per-interval rate |
| Wi-Fi signal and link quality | −43 to −56 dBm live |
| Default route | 192.168.12.1 via wlp108s0 |
| Reachability: gateway, DNS, internet | all three, TCP connect with no payload |
| Bluetooth adapter, rfkill state, connection count | hci0 |
| Audio service health and restart detection | PipeWire, WirePlumber, pipewire-pulse |
| Webcam and IR camera presence | both detected by name |
| USB device inventory | enumerated with vendor and product |
| systemd failed units | detected the injected unit within one slow tick |
| Journal ingestion and classification | caught the same failure independently |
| Hardware inventory and change log | 40+ keys |
| Process table: CPU, RSS, threads, I/O | 564 processes, no root |
| Keyboard backlight brightness readback | 3 of 3 |
| Keyboard RGB capability detection | reports the kernel's own field order |

### Detection and delivery

| Capability | Evidence |
|---|---|
| Deterministic threshold rules | `cpu.sustained_load` fired after its 300 s dwell, not before |
| Six-context learned baselines | activity moved `interactive → load → interactive` under test |
| Anti-false-positive guards | 8 GiB allocated, tracked, and correctly **not** alerted on |
| Alert dedup, escalation, cooldown, rate limiting | one row per fingerprint, enforced by SQLite |
| Suppression: mute, snooze, ignore rule, ignore instance | reversible from the UI |
| Threshold overrides | alert text quotes the value actually in force |
| Desktop notifications | delivered to GNOME, id 18 |
| Ten-second disk aggregation, in-memory full resolution | 11.8 MB/day, 14.2 bytes/row |
| Rollups and retention | chunked deletion at the glacial tier |

### Interfaces

| Capability | Evidence |
|---|---|
| Unix-socket protocol | 16 operations, peer-UID checked, 0700 directory |
| D-Bus service `org.jamsys.Daemon` | introspection, methods and signals verified with `gdbus` and GJS |
| Push-on-material-change | 22 signals over several minutes; sub-quantum noise emits nothing |
| GTK4 window, 16 pages | rendered against live data |
| Corner cluster rendering | Cairo instrument cluster; every state rendered to PNG and reviewed, including live against the daemon |
| Cluster value mapping | 22 tests: needle clamping, NaN parking, charge vs drain, unknown power |
| Panel-line rendering | 23 tests; exact single-line format from the specification |
| Cluster as a window (`jamsys-cluster`) | runs live against the daemon; resize by scroll / `+` / `-` / four presets; settings persist to `~/.config/jamsys/cluster.json` |
| Window always-on-top and all-workspaces | EWMH over XWayland. Verified with `XQueryTree`: with `_NET_WM_STATE_ABOVE` the cluster stayed topmost across three explicit raises of a competing window; cleared, the competing window went back on top. `_NET_WM_STATE_STICKY` toggles both ways. Window stays a 32-bit ARGB visual, so transparency is unaffected. |
| `jamsys-xabove` helper | unprivileged; 34 tests including closed verb vocabulary, path/shell-metacharacter/range refusals |
| **Keyboard RGB and brightness** | **Written to hardware.** Static red, green and white set through `jamsys-kbd` via Polkit; brightness 1 and 3 set and read back from sysfs. 12 helper tests. |
| **Battery charge limit** | **Written to hardware.** Set to 80 and back to 100 through `jamsys-power` via Polkit, each confirmed by reading `charge_control_end_threshold` back. |
| Bluetooth per-device monitoring | live against BlueZ on this machine; a forced disconnect/reconnect was detected and timestamped to the second (drop 00:07:34, return 00:07:45), 26 tests |
| Cut-out cluster (window **and** Shell extension) | four dials with no housing, rendered and reviewed over both a light and a dark ground; 20 geometry and formatting tests |
| Both GPUs, separately | Intel iGPU and NVIDIA dGPU are distinct dials. Measured live: iGPU 80% at 900 MHz while the dGPU sat suspended and was never woken to be read. |
| Network throughput | download and upload on the active interface, on the gadget. Measured live at 43 MB/s down / 731 kB/s up during a real transfer. |
| Battery charge limit | `charge_control_end_threshold` read unprivileged by the daemon and reported on the Hardware page; writes go through `jamsys-power` (7 helper tests + 22 UI-side tests) |
| Report page and `jamsys --report` | one summary of alerts, risk notes, coverage gaps and recent events, as widgets or Markdown; 37 tests covering reply-shape handling, ordering, and missing data |
| Interaction-safe refresh | the two-second rebuild no longer destroys an open dropdown or resets scroll; 8 GTK tests drive real popovers and a real scrolled window |
| `jamsys --json <op>` CLI | reference client |
| `jamsysd --discover` | hardware and coverage report |
| .deb packaging | 37 files, valid control, maintainer scripts syntax-checked |

## 2. Partially working

| Capability | What works | What does not, and why |
|---|---|---|
| **GNOME Shell cluster** | Everything short of being on screen: the full Cairo drawing (rendered to PNG in seven states and reviewed, including live against the running daemon), value mapping (22 tests), D-Bus proxy, live `StateChanged`, JSON parse, placement arithmetic, preferences, and `enable()`/`disable()` run for real against strict stubs of GNOME 50's own API contracts in every mode and corner. | **It has never been displayed on the desktop.** GNOME Shell was, however, made to *load* it, which surfaced a genuine defect — `addChrome({affectsInputRegion})`, a key GNOME 50 rejects outright. That is fixed, and `tests/shell-api-test.js` now enforces the real parameter sets. The fix itself cannot be confirmed live: GJS caches the module, `ReloadExtension` is a stub, so it needs a logout. |
| **Floating placement (Shell extension)** | `Main.layoutManager.addChrome()`, the same mechanism as OSD popups — an in-Shell actor, not a window pretending to be one. | Inherently a desktop gadget: floats above the wallpaper, can overlap a dock, hidden by fullscreen windows. Stated in the preferences dialog rather than left to be discovered. |
| **Window placement (`jamsys-cluster`)** | Stacking is solved: always-on-top and all-workspaces work over XWayland (see §1). Drag from anywhere on the face. | **Position cannot be set by the application.** Wayland gives a window no control over where it opens, and that is true on XWayland here too — the compositor places it and you drag it to the corner you want. The Shell extension has no such limit, which is why it stays the primary surface. |
| **Click-through gaps** | The earlier audit observed six input-region rectangles with `XShapeGetRectangles`. | The handover and UI architecture document lost focus and missing clicks on Mutter even inside those rectangles. Region geometry does **not** prove usable pointer behaviour. Opt-in and off by default; not retested in this review. |
| **Daemon loss/reconnect on both UI surfaces** | Callback regressions cover subscription cleanup, stale replies, restart and close/disable. The shared offline face has 24 real-Cairo checks and was inspected on light/dark grounds. | Live Shell and standalone restart behaviour with these changes is not yet manually verified. No service was stopped or restarted in this review. |
| **Bluetooth** | Per-device state from BlueZ over the system bus: name, address, type, paired, connected, and battery where the device publishes `org.bluez.Battery1`. Connect/disconnect transitions are **event-driven** — BlueZ signals wake the collector, so a flap shorter than the sampling interval is still caught. Disconnects raise a notice naming the device; three in fifteen minutes escalate to a flapping warning. | No device battery on hardware that does not publish it (this machine's headset does not). **Disconnect *reason* is not available**: the kernel exposes no HCI reason to an unprivileged process, so the app reports the observable context and names a cause only when a precondition held, otherwise says the reason is not observable. 802.11 power save is behind nl80211 and is not read; only the wireless device's runtime-PM setting is. |
| **Audio** | Service state, sound cards, restart detection via PID change. | No default sink/source names, no per-stream detail. `libpipewire-0.3` headers are not installed, so the native API cannot be linked; the alternative is spawning `pw-dump` on a timer, which the specification rules out. |
| **NVMe SMART** | Temperature, unprivileged, via hwmon. | Wear, spare blocks, media errors need the helper. See §3. |
| **CPU package power** | Whole-system draw on battery via `power_now`. | RAPL is `0400 root`. On AC there is currently no system-wide power figure at all. |
| **Intel GPU** | Frequency, RC6 residency, throttle reasons, per-client engine time. | No single "busy %". `i915` exposes none without `i915_perf` and `CAP_PERFMON`. RC6 is reported as the honest inverse proxy and labelled as such. |
| **Per-process network** | Aggregate established TCP count. | Not attributed per process: needs socket-inode matching across every `/proc/<pid>/fd`, roughly doubling the cost of the most expensive collector. |
| **Journal severity** | Warning and above (`-p 4`). | Info-level filtered at the source. Suspend markers are info-level, which is why suspend detection uses the clock difference instead. |
| **Time-of-day baselines** | Implemented, wired, local-timezone buckets, tested. | Off by default: four time buckets on top of six contexts means a great deal of history before every combination is usable. |

## 3. Implemented but unverified on hardware

Honest gaps. Code exists, parsers are tested against synthetic input, the hardware path
never executed.

| Item | Why not | Confidence |
|---|---|---|
| **NVMe SMART ioctl** (log page 0x02) | `/dev/nvme0` is `0600 root`. The 512-byte parser is tested against synthetic buffers including a failing-drive case. | Follows NVMe 1.4. Medium-high, unproven. |
| **RAPL energy reading** | `energy_uj` is `0400 root`. Wrap arithmetic is unit-tested. | High — a file read and a delta. |
| **Keyboard modes beyond the recorded static colours** | The historical audit records static red/green/white and brightness 1/3, not every firmware mode or power-state combination. | Parser tests pass; other modes remain hardware-unverified. Keyboard and battery Polkit writes themselves have historical evidence in §1. |
| **udev alternative** | Cannot install to `/etc/udev/rules.d`. | The daemon's `direct` detection path is code-complete and the UI honours it. |
| **A real suspend/resume** | Suspending ends the session running the tests. `BOOTTIME − MONOTONIC` was 0 throughout, as expected for a machine that never slept. | Detection is arithmetic over two clocks, unit-tested with an 8-hour synthetic sleep. High. |
| **Suspend battery drain** | Needs a real suspend. | Rate arithmetic tested. |
| **A real NVIDIA Xid** | None occurred. | 12 code classes unit-tested against real message formats. |
| **A real OOM kill** | None occurred, and provoking one would kill the user's applications. | Counter-delta and journal-pattern paths unit-tested. |
| **Disk-full and SMART-failure alerts** | Would require damaging the machine. | Rules unit-tested with synthetic snapshots. |
| **Memory-leak rule end to end** | Nothing leaked during the session. | Theil–Sen estimator tested against a synthetic 1 GB/h leak with an injected outlier. |
| **Wi-Fi disconnect; repeat Bluetooth fault injection** | Neither was triggered in this review. Bluetooth has historical disconnect/reconnect evidence in §1; Wi-Fi fault injection remains unverified here. | `./scripts/fault-injection.sh wifi` and `bluetooth` require explicit user approval before execution. |
| **Collector loss and recovery after a driver reload** | No real driver was unloaded. | Synthetic tests verify stale thermal removal and a bounded retry after `Gone`; actual unload/reload recovery remains unverified. |
| **Ethernet under load** | No cable attached. | Correctly detected and reported as down. |

## 4. Unsupported

| Item | Reason |
|---|---|
| **Thunderbolt / USB4** | No controller (`/sys/bus/thunderbolt` empty). Not implemented rather than implemented-and-untestable. |
| **AMD GPUs** | Only `i915`, `xe` and `nvidia` are detected. An AMD machine reports the GPU collector unsupported rather than showing wrong numbers. |
| **SATA/SCSI SMART** | NVMe only. SATA would mean spawning `smartctl`; the helper is ioctl-only by design. |
| **Fan control, platform profile, GPU MUX** | These controls remain unwired. Battery charge limit is separately implemented through `jamsys-power` and has historical write/readback evidence in §1. |
| **Per-process network bandwidth** | Needs cgroup accounting or eBPF. |
| **Packet inspection of any kind** | Explicitly excluded. No `AF_PACKET` socket exists in the codebase. |
| **Remote or multi-machine monitoring** | Local only, by design. |
| **Windows partition health** | The dual-boot NTFS/BitLocker partitions are visible as block devices but not mounted. |

## 5. Known limitations

1. **The Shell cluster has not been seen on the desktop.** The drawing *has* been
   looked at — every state rendered to PNG through the same code path, which is how
   three layout defects were found — and the Shell has been made to load the extension,
   which is how a fourth defect (`affectsInputRegion`) was found. But a PNG is not a
   live actor and a load is not a paint. It is the first thing to check after a logout.
2. **Not every keyboard mode is proven.** Static colours and brightness have historical
   evidence in §1; modes and power-state combinations beyond those remain unverified.
3. **The window is heavy.** 110 MB PSS, dominated by the Python interpreter, PyGObject
   and Mesa. A `gtk4-rs` front-end against the same socket would fix it; the protocol
   exists partly to make that substitution cheap. The daemon stays at 26.8 MB.
4. **`process` costs 55.8 ms per run.** On the slow tier and disableable, but on a
   machine with thousands of processes it will be felt.
5. **Baselines take time.** 120 samples per context and there are six contexts. A laptop
   rarely on battery will take days to learn a battery-idle baseline. Correct, but a
   user expecting instant anomaly detection will be disappointed for the first hour.
6. **No AMD support**, which is a large share of Linux laptops.
7. **On AC there is no whole-system power figure.** RAPL would provide CPU-package
   energy, not total laptop draw; whole-system idle-power comparisons rely on battery data.
8. **`db_bytes ÷ uptime` overstates growth**, because the database predates the current
   run. Measure a delta instead; the acceptance script does.

## 6. Resource usage

| | Measured |
|---|---|
| Daemon CPU, reduced regime | **0.089 % of one core** (0.004 % of 24 threads) |
| Daemon CPU, default tiers | **0.24 – 0.30 % of one core** |
| Daemon startup | 0.12 s, once |
| Daemon memory | **29.2 MB RSS / 26.8 MB PSS**, 2 threads |
| Window memory | 201–232 MB RSS / 110 MB PSS, while open |
| Shell readout | no process of its own; 22 pushed repaints over several minutes |
| Helpers | not resident |
| Wakeups | 0.99 /s reduced, 2.81 /s default (ctxsw upper bound); 0.85 /s epoll |
| **dGPU wake time caused by JamSys** | **0 ms in 180 s** (919 ms with the monitor *off*) |
| Database growth | **11.8 MB/day**, 14.2 bytes/row, ~75 MB steady state |
| Write frequency | one transaction per 15 s |
| Events processed / dropped | 12 / **0** |
| Binaries | `jamsysd` 2.3 MB, `jamsys-helper` 0.32 MB, `jamsys-kbd` 0.29 MB |

**Battery impact: below the measurement floor of the machine's own sensor.** ~5 mW
against a 10–13 W idle draw, where `power_now` quantises at ~10 mW. No number is quoted
because the hardware cannot substantiate one. The measurable figure is the one that
matters: the dGPU draws 6.8 W awake, and the gate keeps JamSys from causing that.

## 7. Permissions required

| To do this | You need |
|---|---|
| Read-only monitoring in §1 except SMART and RAPL | **Nothing.** No root, no capabilities, no setuid; journal and other-user process restrictions are listed below. Hardware control uses the separate helpers. |
| Read the system journal | `adm` or `systemd-journal` membership. Already satisfied here. |
| Per-process I/O and GPU for *other users'* processes | Root. Not requested. |
| NVMe SMART, CPU package power | The optional read-only helper (`CAP_SYS_ADMIN` for the NVMe ioctl). |
| Keyboard lighting | Either the Polkit-gated write helper, or the udev rule and no privileged code at all. |
| Battery charge-limit writes | `jamsys-power` via Polkit (`auth_admin_keep`); unprivileged reads remain available. |
| Install the .deb, enable the Shell extension | Root for the package; a logout for the extension. |
| Run the window or the readout | Never root. |

## 8. Tests

Repository review on 2026-09-10: all suites below passed. Rust counts are tests;
Python/GJS counts are the harnesses' checks, not equivalent unit-test totals.

```
jamsys-daemon   267 unit + 17 integration (baseline: 263 + 16)
jamsys-helper     5 unit
jamsys-kbd       12 unit
jamsys-power      7 unit
Python           34 xabove + 19 charge-limit checks
extension        23 panel-line + 60 gauge + 69 Shell-API checks
cluster          10 standalone lifecycle + 24 offline Cairo checks
live D-Bus       26 checks; received 2 StateChanged signals
```

Baseline Shell-API count was 53; the other pre-existing suites retained their counts.
The charge-limit harness explicitly skipped its absent-helper branch because a helper
is installed; this is not a new helper-write verification. Two pre-existing Rust
warnings in `collectors/keyboard.rs` remain. The first sandboxed daemon run had
17 local-socket failures; the unrestricted baseline and final runs passed unchanged
tests. The sandbox also blocked the initial live D-Bus connection; the permitted rerun
passed. No fault-injection or privileged hardware scenario was run in this review.
The live D-Bus check used the already-installed daemon, not the newly built binary.

Covering the threshold engine and every rule's explanation contract; baseline
mathematics including the degenerate-MAD, absolute-delta, warm-up, context-separation
and self-exclusion guards; the six-context partition and its distinctness; alert
deduplication, escalation, cooldown, token-bucket limiting, suppression scopes and
snooze expiry; retention, rollups, 10-second aggregation, the in-memory ring and
`flush_final`; every parser (`/proc/stat`, `meminfo`, `vmstat`, PSI, `diskstats`,
`mountinfo` with octal escapes, `/proc/net/wireless`, `/proc/net/route`,
`/proc/<pid>/stat` with parentheses in the command name, journald JSON including
byte-array messages, D-Bus wire format both directions, netlink, NVMe SMART log pages);
missing sensors; invalid sysfs data; a sensor vanishing mid-run; NVIDIA absent; no
battery; a phantom firmware battery; a charge-domain battery; no Wi-Fi; loopback only;
suspend/resume detection; collector isolation under panics; IPC framing, limits and
access control; and the widget's change-detection quantisation.
