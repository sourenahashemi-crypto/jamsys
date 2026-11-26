# Metrics Map

Every metric JamSys collects, with its source, interface, privilege, acquisition mode,
frequency, measured cost and failure behaviour.

**Cost** is the measured mean wall-clock time of the whole collector on the target
machine (see `performance.md`), divided across the metrics it produces. It includes
blocking I/O, so `thermal` looks expensive mainly because ACPI sensor reads block in
firmware rather than because they burn CPU.

**Privilege** column: `—` means no privilege of any kind; `adm` means membership of the
`adm` or `systemd-journal` group; `root` means the optional helper.

**Failure behaviour** is what happens when the source is missing or returns nonsense.
The universal rules: a missing file is `None`, not an error; an out-of-range value is
rejected by the parser and never stored; three consecutive collector failures quarantine
that collector and nothing else.

---

## CPU — collector `cpu`, fast tier, 0.48 ms/run measured

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Total utilisation | `/proc/stat` `cpu` line | file read, jiffy delta | — | poll | 2 s | First tick emits nothing (no delta); counter regression → `None`, not a spike |
| Per-core utilisation | `/proc/stat` `cpuN` lines | same | — | poll | 2 s | A core hot-unplugged simply disappears from the list |
| Load average 1/5/15 | `/proc/loadavg` | file read | — | poll | 2 s | Absent → 0.0, no alert |
| Runnable / total processes | `/proc/loadavg` field 4, `/proc/stat` `procs_running` | file read | — | poll | 2 s | Absent → 0 |
| Context switches/s | `/proc/stat` `ctxt` | counter delta | — | poll | 2 s | Regression → skipped |
| Clock frequency | `cpufreq/scaling_cur_freq` per CPU | file read, mean | — | poll | 2 s | No cpufreq driver → `Partial`, frequency hidden |
| Governor, scaling driver | `cpufreq/scaling_governor`, `scaling_driver` | file read | — | poll | 2 s | Absent → empty string |
| CPU stall (PSI) | `/proc/pressure/cpu` | file read | — | poll | 2 s | `CONFIG_PSI` off → `Partial`, PSI rules disabled |
| Package temperature | hwmon `coretemp` `Package id 0` | hwmon, resolved by driver+label | — | poll | 10 s | Sensor vanishes → that channel drops out; all channels gone → re-probe |
| Per-core temperature | hwmon `coretemp` `Core N` | same | — | poll | 10 s | As above |
| Throttle events | `cpu0/thermal_throttle/package_throttle_count` | monotonic counter delta | — | poll | 10 s | Absent → `Partial`; throttle rule disabled rather than guessed from frequency |

## Memory — collector `memory`, fast tier, 0.32 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Total / available / used | `/proc/meminfo` | key-value parse, kB→bytes | — | poll | 2 s | `MemTotal == 0` → `BadData`, tick skipped |
| Cache, buffers, dirty | `/proc/meminfo` | same | — | poll | 2 s | Missing key → 0 |
| Swap total / used | `/proc/meminfo` | same | — | poll | 2 s | No swap → percentage is 0, never NaN |
| Memory pressure (PSI) | `/proc/pressure/memory` | file read | — | poll | 2 s | Absent → `Partial` |
| OOM kills | `/proc/vmstat` `oom_kill` | monotonic counter delta | — | poll | 2 s | Only the *increase* fires; a historical total never re-alerts |
| Major page faults/s | `/proc/vmstat` `pgmajfault` | counter delta | — | poll | 2 s | Regression → skipped |
| Per-process RSS growth | `/proc/<pid>/statm` field 2 | Theil–Sen slope over 32 samples | — | poll | 60 s | Fewer than 8 samples → no slope, no leak verdict |

## Intel GPU — collector `gpu`, medium tier, 1.1 ms/run (both GPUs)

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Actual / requested frequency | `card1/gt_act_freq_mhz`, `gt_cur_freq_mhz`, or `gt/gt0/rps_*` | file read, both layouts tried | — | poll | 10 s | Neither layout → frequency omitted |
| Maximum frequency | `card1/gt_max_freq_mhz` | file read | — | poll | 10 s | Absent → 0 |
| RC6 residency | `gt/gt0/rc6_residency_ms` | monotonic ms counter → % of interval | — | poll | 10 s | Absent → omitted; this is the honest inverse proxy for "GPU busy" |
| Throttle reasons (8 flags) | `gt/gt0/throttle_reason_*` | file read per flag | — | poll | 10 s | Absent flags simply not listed |
| Drives the display | `card1-*/status` connector nodes | file read | — | poll | 10 s | Verified not to wake the dGPU |
| Per-process engine time | `/proc/<pid>/fdinfo/*` `drm-engine-*` | file scan, own processes only | — | poll | 60 s | Other users' processes → 0, no error |

> There is deliberately **no single "Intel GPU busy %"**. `i915` exposes none without
> `i915_perf`, which needs `CAP_PERFMON` or a relaxed `perf_event_paranoid`. Inventing one
> from frequency would be a guess. RC6 residency is reported instead and labelled as such.

## NVIDIA GPU — collector `gpu`, medium tier

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| **Runtime power state** | `card2/device/power/runtime_status` | file read | — | poll | 10 s | **Read first, always. Gates everything below.** |
| Awake / suspended time | `device/power/runtime_{active,suspended}_time` | file read | — | poll | 10 s | Absent → 0 |
| Utilisation | NVML `nvmlDeviceGetUtilizationRates` | `dlopen` FFI | — | poll **only when awake** | 10 s | Library absent → `Partial`, sysfs state still reported |
| VRAM used / total | NVML `nvmlDeviceGetMemoryInfo` | FFI | — | conditional | 10 s | Symbol missing → that metric omitted |
| Temperature | NVML `nvmlDeviceGetTemperature` | FFI | — | conditional | 10 s | Range-checked −40…150 °C |
| Power draw | NVML `nvmlDeviceGetPowerUsage` | FFI | — | conditional | 10 s | Range-checked 0…1000 W |
| P-state | NVML `nvmlDeviceGetPerformanceState` | FFI | — | conditional | 10 s | 32 means unknown → reported as unknown |
| SM clock | NVML `nvmlDeviceGetClockInfo` | FFI | — | conditional | 10 s | Omitted on failure |
| Fan % | NVML `nvmlDeviceGetFanSpeed` | FFI | — | conditional | 10 s | Many laptops report none → omitted |
| GPU processes | NVML `nvmlDeviceGetComputeRunningProcesses_v3` | FFI | — | conditional | 10 s | Falls back to `_v2`, then to empty |
| Driver version | `/proc/driver/nvidia/version` | file read | — | poll | 10 s | Read from `/proc`, never wakes the device |
| Xid errors | journal `NVRM: Xid` | stream match | adm | **event** | — | 12 code classes graded; unknown codes → WARNING |

> **When suspended, NVML is not called at all.** Utilisation and power are reported as 0
> because the card is powered down — which is true, and free. Measured: `nvmlInit()`
> itself wakes the GPU, so it is deferred until the first tick that finds it already awake.

## Battery and power — collector `power`, medium tier, 0.76 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Charge % | `BAT0/capacity` | file read | — | poll | 10 s | Range-checked 0…100 |
| Status | `BAT0/status` | file read | — | poll | 10 s | Absent → "Unknown" |
| **System power draw** | `BAT0/power_now` (energy domain) or `current_now × voltage_now` (charge domain) | file read + EWMA α=0.4 | — | poll | 10 s | Both domains handled; a 0 W transition blip is smoothed, not hidden |
| Voltage | `BAT0/voltage_now` | file read | — | poll | 10 s | Range-checked 0…100 V |
| Energy now / full / design | `energy_*` or `charge_* × voltage` | file read | — | poll | 10 s | Charge-domain conversion unit-tested |
| Health % | `energy_full / energy_full_design` | derived | — | poll | 10 s | Reported above 100 % when true (this pack reads 101.3 %), never clamped |
| Cycle count | `BAT0/cycle_count` | file read | — | poll | 10 s | Absent → 0 |
| Estimated runtime | `energy_now / power_now` | derived | — | poll | 10 s | Only while discharging above 0.5 W |
| AC present | `ADP0/online` | file read | — | poll | 10 s + uevent | Absent → `Partial` |
| **CPU package power** | `intel-rapl:0/energy_uj` | wrap-aware counter delta | **root** | poll | 60 s | Helper absent → `Unavailable`, clearly stated on Coverage |
| Suspend drain %/h | battery % before vs after sleep | derived from BOOTTIME gap | — | **event** | on resume | Needs ≥ 15 min sleep; otherwise not computed |

## Thermals and fans — collector `thermal`, medium tier, 8.6 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| All temperatures | `/sys/class/hwmon/*/temp*_input` | resolved by **driver name + label**, never `hwmonN` index | — | poll | 10 s | Index changes across kernel upgrades are survived by design |
| Critical points | `temp*_crit`, `temp*_max` | file read at probe | — | probe | once | Absent → near-critical rule disabled for that channel |
| Fan RPM | hwmon `asus` `fan1/fan2_input` | file read | — | poll | 10 s | No tachometer → `Partial`, stall rule disabled |
| Thermal zones | `/sys/class/thermal/thermal_zone*` | fallback when no hwmon | — | poll | 10 s | Neither → `Unsupported` |

## Storage — collector `storage`, slow tier, 3.0 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Filesystem used / free | `statvfs()` | syscall, `f_bavail` not `f_bfree` | — | poll | 60 s | Unmounted mid-scan → skipped |
| Inode usage | `statvfs()` `f_files`/`f_ffree` | syscall | — | poll | 60 s | 0 inodes → percentage 0, not NaN |
| Mount list | `/proc/self/mountinfo` | parse with octal unescaping | — | poll | 60 s | 73 pseudo/snap/read-only mounts filtered |
| Read / write throughput | `/proc/diskstats` fields 6, 10 | sector delta × 512 | — | poll | 60 s | Whole devices only; partitions and loops excluded |
| IOPS | `/proc/diskstats` fields 4, 8 | counter delta | — | poll | 60 s | Counter reset → saturating, no spike |
| Device utilisation | `/proc/diskstats` `io_ticks` | ms busy / ms elapsed | — | poll | 60 s | Clamped 0…100 |
| Mean service time | `/proc/diskstats` weighted ms | derived | — | poll | 60 s | Zero ops → 0 |
| NVMe temperature | hwmon `nvme` Composite | channel resolved at probe | — | poll | 60 s | Absent → omitted |
| ext4 error counter | `/sys/fs/ext4/*/errors_count` | monotonic counter delta | — | poll | 60 s | Non-ext4 → nothing to read |
| **NVMe SMART** | ioctl `NVME_IOCTL_ADMIN_CMD`, log page 0x02 | 512-byte log parse | **root** | poll | 60 s (helper) | Helper absent → `Partial`; temperature unaffected |

## Network — collector `network`, fast tier, 5.2 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Interface list and kind | `/sys/class/net/*` | directory scan | — | poll | 2 s | Empty → `Unsupported` |
| Operstate, carrier | `operstate`, `carrier` | file read | — | poll | 2 s | Absent → "unknown" |
| Driver | `device/driver` symlink | readlink | — | poll | 2 s | Virtual interfaces have none |
| MAC | `address` | file read | — | poll | 2 s | Absent → empty |
| IPv4 / IPv6 addresses | `getifaddrs()` | **one** walk, bucketed by name, cached 30 s | — | poll | 30 s | Link-local v6 filtered as noise |
| RX / TX throughput | `statistics/{rx,tx}_bytes` | counter delta | — | poll | 2 s | Saturating subtraction on reset |
| Errors and drops | `statistics/{rx,tx}_{errors,dropped}` | counter delta → % of packets | — | poll | 2 s | Zero packets → 0 %, not NaN |
| Wi-Fi signal, link quality | `/proc/net/wireless` | parse (values carry a trailing dot) | — | poll | 10 s | No Wi-Fi → `Partial` |
| Link speed | `speed` | file read | — | poll | 2 s | −1 on a down link → omitted |
| Default route and gateway | `/proc/net/route` | little-endian hex decode | — | poll | 2 s | No default route → `None` |
| Established TCP count | `/proc/net/tcp{,6}` state 01 | line count, cached 30 s | — | poll | 30 s | **Counts only. No payload is ever read.** |
| Gateway / DNS / internet reachable | `connect()` to gateway:53/80, resolver:53, configured probe | bare TCP, **zero bytes sent** | — | poll | 60 s | Disable with `[network] reachability = false` |
| Link / address / route changes | `NETLINK_ROUTE` `RTMGRP_LINK`/`_IFADDR`/`_ROUTE` | netlink socket | — | **event** | — | Socket fails → falls back to polling, logged |

## Devices — collector `devices`, slow tier, 3.4 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Bluetooth adapter, address | `/sys/class/bluetooth/hciN` | file read | — | poll | 60 s | Absent → `Partial` |
| Bluetooth rfkill state | `/sys/class/rfkill/*/{soft,hard}` | file read | — | poll | 60 s | Absent → assumed unblocked |
| Bluetooth connections | `hciN:M` child nodes | directory count | — | poll | 60 s | **Count only** — names need BlueZ ObjectManager, not implemented |
| PipeWire / WirePlumber state | user-slice cgroup presence, else process name | stat | — | poll | 60 s | Neither found → "inactive"; empty means *not yet observed* and never alerts |
| PipeWire restarts | main PID change | comparison | — | poll | 60 s | This is what a user experiences as audio cutting out |
| Sound cards | `/sys/class/sound/cardN/id` | file read | — | poll | 60 s | Absent → empty list |
| USB devices | `/sys/bus/usb/devices/*` | idVendor/idProduct/product | — | poll | 60 s | Interfaces and root hubs excluded |
| Webcam / IR camera | `/sys/class/video4linux/*/name` | name match | — | poll | 60 s | Presence only, never opened |
| Hotplug add/remove | `NETLINK_KOBJECT_UEVENT` group 1 | netlink socket | — | **event** | — | libudev-format messages skipped, not mis-parsed |
| Expected-device list | learned over 10 consecutive slow ticks | in-memory set | — | derived | — | One-off plug-ins never become "expected" |

## Security — collector `security`, medium tier, 0.7 ms/run measured

Detection only. Nothing is blocked and no packet is read; see
[privilege-model.md](privilege-model.md).

| Reading | Source | Privilege | Notes |
|---|---|---|---|
| Listening TCP sockets | `sock_diag` netlink with a LISTEN state filter; `/proc/net/tcp{,6}` (state `0A`) as the fallback | none | local address, port, owning uid, inode. Reading `/proc/net/tcp` makes the kernel walk every TCP socket and cost 5.5 ms per sample here; the filtered query costs 0.7 ms and a test holds the two sources equal |
| Waiting UDP sockets | `/proc/net/udp`, `/proc/net/udp6`, no remote end | none | listed, never alerted on |
| Exposure | the bound address | none | loopback / all interfaces / one address; `::ffff:127.0.0.1` counts as loopback |
| Owning account | uid → `/etc/passwd` | none | |
| Owning process | `/proc/<pid>/fd` → `socket:[inode]` | **own processes only** | another user's descriptors need root, so a root service is shown by account. Scanned only when a new inode appears |
| Firewall on/off | `ENABLED=` in `/etc/ufw/ufw.conf`; `/run/firewalld.pid` | none | **the rules are root-only and are not read.** With neither tool present the state is `unknown`, never "off" |
| Failed sign-ins | a second `journalctl --follow` restricted to sshd, sudo, su, login, polkitd, gdm | `adm` or `systemd-journal` | informational priority, which the main follower excludes. sshd is counted from its own lines and every other service from `pam_unix`, so one attempt is one count |

| Router hardware address | `/proc/net/arp` entry for the default gateway | none | compared only across samples where the interface's `carrier_changes` count is unchanged: a different router after reconnecting is a different network, the same link answering from a different address is not |
| DNS servers | `/run/systemd/resolve/resolv.conf`, else `/etc/resolv.conf` | none | a change on the same link is an event, not an alert: a VPN connecting does it legitimately |
| USB keyboards | `/sys/bus/usb/devices/*:*` with interface class `03`, protocol `01` | none | re-read at once on a USB `add` uevent, not at the next sample. Baseline in `known-keyboards.json`, learned once like the listeners; accepted ones in `[security] accepted_devices` |

**The checkup.** A short list of settings, each `ok`, `warn` (with the one command
that fixes it), `note` (worth knowing, not simply fixable) or `unknown`. A check
whose source file is absent is left out or marked unknown, never assumed.

| Check | Source | `warn` when |
|---|---|---|
| Firewall | as above | switched off |
| Exposure | the listener table | a network-facing listener is new and unaccepted (`note` if all are expected) |
| Automatic security updates | `APT::Periodic::Unattended-Upgrade` in `/etc/apt/apt.conf.d/*` | set to `0` |
| Updates waiting | `/var/lib/update-notifier/updates-available` | any is a security update. The "ESM Apps" advertisement in that file is not counted |
| Restart needed | `/run/reboot-required` | present |
| Automatic login | `AutomaticLoginEnable` / `TimedLoginEnable` in `/etc/gdm3/custom.conf` | enabled |
| Extra root accounts | `/etc/passwd` | any account other than `root` has uid 0 |
| SSH server | `sshd_config.d/*.conf` then `sshd_config`, first occurrence wins as in sshd. Only if sshd is installed | passwords accepted (the default), or root may sign in with one |
| SSH private keys | permission bits of `~/.ssh/id_*`. **The files are never opened** | readable by group or others |
| Disk encryption | device mounted at `/`, followed through device-mapper `slaves` for a `CRYPT-` uuid | never: unencrypted is a `note`, since it can only be chosen at install time |
| Secure Boot | byte 5 of the `SecureBoot-*` EFI variable | never: off is a `note`, changed in firmware setup |

The baseline of expected network-facing listeners is learned once, at first run, into
`~/.local/share/jamsys/known-listeners.json`, and extended only by the user accepting
a listener (`[security] accepted_listeners` in `config.toml`). It is deliberately not
re-learned on restart. Ports from 32768 up are treated as one identity per program,
since applications pick a different one each launch.

Not seen: firewall rules; which process owns another user's socket; a socket that
opens and closes between two samples (10 s, or 30 s while idle on battery);
outbound connections per process; anything about traffic contents.

## Services and journal

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Failed units | `org.freedesktop.systemd1.Manager.ListUnitsFiltered(["failed"])` | D-Bus, hand-written client | — | poll | 60 s | Bus drop → `Gone`, reconnect on re-probe, no quarantine |
| Restart / flap counts | failure timestamps, 1-hour window | in-memory | — | derived | — | Backwards clock step cannot fabricate flapping |
| Kernel and service errors | `journalctl --follow -o json -p 4` | **one** long-lived child, stdout in epoll | adm | **event** | — | Child exits → respawned at the glacial tier |
| Message classification | 45-pattern glob table | first match wins | adm | event | — | Unmatched → recorded as event, **never** an alert |
| Deduplication | 300 s window per message | in-memory map, GC'd | adm | event | — | Suppressed count is reported, not hidden |

> `libsystemd` headers are not installed on the target, so `sd_journal` cannot be linked.
> One long-lived `journalctl --follow` satisfies the real requirement — never rescan the
> log — without adding a C build dependency. Server-side `-p 4` filtering means
> info-level chatter never crosses the pipe.

## Processes — collector `process`, slow tier, 55.8 ms/run

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| PID, name, threads | `/proc/<pid>/stat` | parse from the **last** `)` | — | poll | 60 s | A command name containing `)` is handled |
| Command line | `/proc/<pid>/cmdline` | NUL-separated | — | poll | 60 s | Empty → `[comm]` |
| CPU % | `utime + stime` delta / `CLK_TCK` | counter delta | — | poll | 60 s | First tick has no delta → ties broken by RSS |
| RSS | `/proc/<pid>/statm` field 2 × page size | file read | — | poll | 60 s | One small read instead of parsing `status` |
| Disk read / write | `/proc/<pid>/io` | own processes only | — | poll | 60 s | Other users → 0, no wasted syscall |
| GPU engine time | `/proc/<pid>/fdinfo/*` | own processes only | — | poll | 60 s | Sum across engines, so it can exceed 1 s/s |
| Bookkeeping reaping | live PID set | map retain | — | derived | — | Prevents unbounded growth on process churn |

## Suspend / resume and inventory

| Metric | Source | Interface | Priv | Mode | Freq | Failure |
|---|---|---|---|---|---|---|
| Suspend detected, sleep duration | `CLOCK_BOOTTIME − CLOCK_MONOTONIC` | two `clock_gettime` calls | — | **derived, every tick** | 2 s | Cannot miss a suspend that happened while unscheduled; 5 s threshold rejects jitter |
| Resume timestamp | wall clock at detection | — | — | derived | — | — |
| Battery before / after sleep | retained snapshot vs first post-resume sample | derived | — | event | on resume | Needs both readings |
| Device recovery after resume | collectors re-run, coverage re-probed | — | — | event | on resume | NVML handles re-initialised |
| Hardware inventory (40+ keys) | DMI, `/proc/cpuinfo`, PCI ids, `/sys/module/*/version`, PSY, V4L2 | file reads | — | poll | 900 s | First run is silent; only differences become events |

## Self-monitoring

| Metric | Source | Interface | Priv | Mode | Freq |
|---|---|---|---|---|---|
| Daemon CPU seconds | `/proc/self/stat` | file read | — | on request |
| Daemon RSS, threads | `/proc/self/status` | file read | — | on request |
| Epoll wakeups, ticks, idle ticks | internal counters | — | — | on request |
| Events processed / dropped | internal counters | — | — | on request |
| Database size and row counts | `PRAGMA page_count × page_size` | SQLite | — | on request |
| Widget repaints pushed | internal counter | — | — | on request |
| Per-collector runs and mean cost | internal timing | — | — | on request |

---

## Summary of privilege

| Class | Count | Notes |
|---|---|---|
| No privilege at all | ~95 metrics | Everything above except the rows below |
| `adm` group | journal-derived | Already satisfied on the target; degrades to `Partial` without it |
| Root (read-only helper) | 2 | CPU package power, NVMe SMART |
| Root (write helper) | keyboard RGB and brightness | Separate binary, validated argv, Polkit-gated |


## Bluetooth

| What | Source | Notes |
|---|---|---|
| Adapter present, rfkill soft/hard | `/sys/class/bluetooth`, `/sys/class/rfkill/*` | Works with BlueZ stopped; this is the fallback view in `devices`. |
| Device name, address, icon, paired, connected | `org.bluez` `GetManagedObjects` on the system bus | `Alias` preferred over `Name`, because it is what the user renamed the device to. |
| Battery percentage | `org.bluez.Battery1` `Percentage` on the same object path | Absent on devices that do not publish it. |
| Connect / disconnect transitions | BlueZ `PropertiesChanged`, `InterfacesAdded`, `InterfacesRemoved` | Event-driven: the signal is only a wake-up, and the state is always re-read authoritatively, so a missed signal cannot desynchronise anything. |
| Radio USB power state | `/sys/bus/usb/devices/*/power/{control,runtime_status,runtime_suspended_time}` | The node whose `product` contains "bluetooth". Autosuspend is a common dropout cause and is reported as a standing risk note. |
| bluetoothd restart | `GetNameOwner("org.bluez")` | The unique bus name changes if and only if the service restarted — exact, and free. |
| Stack errors | journal lines from bluetoothd | Translated to plain language. BlueZ logs **nothing** for a clean disconnect — verified on this machine — but does log connect failures, which carry the useful part. |

### What is deliberately not claimed

The HCI disconnect reason is not readable without `CAP_NET_RAW` (a monitor socket), so
JamSys does not pretend to know it. It records what was observably true at the instant
of the drop — radio power state, rfkill, adapter power, whether the machine had just
resumed, whether bluetoothd restarted — and names a cause only when one of those
preconditions actually held. Otherwise it says the reason is not observable, which is
more useful than a confident guess that is wrong.
