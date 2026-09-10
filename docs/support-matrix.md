# Support Matrix

What JamSys actually does on the target machine, what it half-does, what it does not
do, and where the sharp edges are.

The rule applied throughout: **a capability is only claimed as working if it is wired to
a real data source and that path has been executed on this hardware.** Where code exists
but the path could not be exercised, it is listed as unverified — not as working.

Audited 2026-09-09 on ASUS TUF F16 FX608JMR, Ubuntu 26.04.1, kernel 7.0.0-31,
GNOME Shell 50.1, Wayland.

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
| Corner-readout rendering | 23 tests; exact single-line format from the specification |
| `jamsys --json <op>` CLI | reference client |
| `jamsysd --discover` | hardware and coverage report |
| .deb packaging | 37 files, valid control, maintainer scripts syntax-checked |

## 2. Partially working

| Capability | What works | What does not, and why |
|---|---|---|
| **GNOME Shell readout** | Everything: rendering (23 tests), D-Bus proxy, live `StateChanged`, JSON parse, label generation, placement code, preferences — all verified against the running daemon by a GJS harness using the identical interface and code path. | **It has never been displayed in a panel.** GNOME Shell on Wayland will not load a *newly installed* extension without a session restart, and this session cannot be restarted. Everything except the final `Main.panel.addToStatusArea` call is exercised. |
| **Bottom placement** | Implemented with `Main.layoutManager.addChrome()`, the same mechanism as OSD popups. | Genuinely less robust than the panel: floats above the desktop, can overlap a dock, hidden by fullscreen. Documented in the preferences dialog rather than hidden. |
| **Keyboard RGB control** | Capability detection, field-order readback, the full UI, argument validation (10 tests including injection attempts), correct exit codes, refusal when unprivileged. | **No colour has been written to hardware.** Every path is `root:root` and this environment has no usable `sudo`. See §3. |
| **Bluetooth** | Adapter, address, rfkill, **count** of connections. | No device *names* or battery levels — needs BlueZ `ObjectManager` enumeration, which the minimal D-Bus client does not implement. |
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
| **Keyboard RGB writes** | `brightness` is `root:root 0644`, `kbd_rgb_mode`/`kbd_rgb_state` are `--w------- root:root`. Writing as the user returns `Permission denied`, verified. No usable `sudo` here to test through pkexec. | Rendering is unit-tested against the kernel's advertised field order (`cmd mode red green blue speed`), which was read from `kbd_rgb_mode_index` on this machine. Medium-high — **treat as unproven**. |
| **NVMe SMART ioctl** (log page 0x02) | `/dev/nvme0` is `0600 root`. The 512-byte parser is tested against synthetic buffers including a failing-drive case. | Follows NVMe 1.4. Medium-high, unproven. |
| **RAPL energy reading** | `energy_uj` is `0400 root`. Wrap arithmetic is unit-tested. | High — a file read and a delta. |
| **Polkit action** | Cannot install to `/usr/share/polkit-1/actions` without root. | The policy is standard and small; `allow_active=yes` matches how desktop brightness is handled. |
| **udev alternative** | Cannot install to `/etc/udev/rules.d`. | The daemon's `direct` detection path is code-complete and the UI honours it. |
| **A real suspend/resume** | Suspending ends the session running the tests. `BOOTTIME − MONOTONIC` was 0 throughout, as expected for a machine that never slept. | Detection is arithmetic over two clocks, unit-tested with an 8-hour synthetic sleep. High. |
| **Suspend battery drain** | Needs a real suspend. | Rate arithmetic tested. |
| **A real NVIDIA Xid** | None occurred. | 12 code classes unit-tested against real message formats. |
| **A real OOM kill** | None occurred, and provoking one would kill the user's applications. | Counter-delta and journal-pattern paths unit-tested. |
| **Disk-full and SMART-failure alerts** | Would require damaging the machine. | Rules unit-tested with synthetic snapshots. |
| **Memory-leak rule end to end** | Nothing leaked during the session. | Theil–Sen estimator tested against a synthetic 1 GB/h leak with an injected outlier. |
| **Wi-Fi / Bluetooth disconnect** | Would interrupt a live network and disconnect paired devices while the machine is unattended. | `./scripts/fault-injection.sh wifi` and `bluetooth` run them on request. |
| **Ethernet under load** | No cable attached. | Correctly detected and reported as down. |

## 4. Unsupported

| Item | Reason |
|---|---|
| **Thunderbolt / USB4** | No controller (`/sys/bus/thunderbolt` empty). Not implemented rather than implemented-and-untestable. |
| **AMD GPUs** | Only `i915`, `xe` and `nvidia` are detected. An AMD machine reports the GPU collector unsupported rather than showing wrong numbers. |
| **SATA/SCSI SMART** | NVMe only. SATA would mean spawning `smartctl`; the helper is ioctl-only by design. |
| **Fan control, platform profile, charge limit, GPU MUX** | The interfaces exist here (`platform_profile`, `throttle_thermal_policy`, `charge_control_end_threshold`, `asus-armoury`) and are deliberately **not** wired. Monitoring has priority over tweaking, and each needs its own validation and hardware testing before going near a root binary. |
| **Per-process network bandwidth** | Needs cgroup accounting or eBPF. |
| **Packet inspection of any kind** | Explicitly excluded. No `AF_PACKET` socket exists in the codebase. |
| **Remote or multi-machine monitoring** | Local only, by design. |
| **Windows partition health** | The dual-boot NTFS/BitLocker partitions are visible as block devices but not mounted. |

## 5. Known limitations

1. **The Shell readout has not been seen on screen.** Every layer beneath the final
   panel insertion is verified, but that is not the same as having looked at it. It is
   the first thing to check after a logout.
2. **Keyboard writes are unproven.** The most likely failure is a firmware that accepts
   mode 0 but not 1–3; try static first.
3. **The window is heavy.** 110 MB PSS, dominated by the Python interpreter, PyGObject
   and Mesa. A `gtk4-rs` front-end against the same socket would fix it; the protocol
   exists partly to make that substitution cheap. The daemon stays at 26.8 MB.
4. **`process` costs 55.8 ms per run.** On the slow tier and disableable, but on a
   machine with thousands of processes it will be felt.
5. **Baselines take time.** 120 samples per context and there are six contexts. A laptop
   rarely on battery will take days to learn a battery-idle baseline. Correct, but a
   user expecting instant anomaly detection will be disappointed for the first hour.
6. **No AMD support**, which is a large share of Linux laptops.
7. **On AC there is no whole-system power figure** without the RAPL helper, so idle-power
   anomaly detection effectively only works on battery.
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
| Everything in §1 except SMART and RAPL | **Nothing.** No root, no capabilities, no setuid. |
| Read the system journal | `adm` or `systemd-journal` membership. Already satisfied here. |
| Per-process I/O and GPU for *other users'* processes | Root. Not requested. |
| NVMe SMART, CPU package power | The optional read-only helper (`CAP_SYS_ADMIN` for the NVMe ioctl). |
| Keyboard lighting | Either the Polkit-gated write helper, or the udev rule and no privileged code at all. |
| Install the .deb, enable the Shell extension | Root for the package; a logout for the extension. |
| Run the window or the readout | Never root. |

## 8. Tests

**285 assertions, all passing.**

```
jamsys-daemon   231 unit + 16 integration
jamsys-helper     5 unit
jamsys-kbd       10 unit   (every injection attempt is a named test)
extension          23 rendering  +  live end-to-end against the running daemon
fault injection     5 safe scenarios executed on real hardware
```

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
