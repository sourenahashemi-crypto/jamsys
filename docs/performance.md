# Measured Performance of the Monitoring Application Itself

A monitor that costs battery life is self-defeating, so this is measured rather than
asserted. Every number below was produced on the target machine (ASUS TUF F16, i7-14650HX,
Ubuntu 26.04.1, kernel 7.0.0-31) against the release build, on **2026-09-09**.

The machine was not idle during measurement — a normal desktop session was running with
several Electron applications, a browser and an IDE. That makes these figures pessimistic
rather than flattering.

---

## Method

Two independent measurements, because each has a blind spot.

**Internal.** The daemon reports its own `/proc/self/stat` CPU time, `VmRSS`, epoll
wakeup count and per-collector timings through the `stats` IPC call.

**External.** A separate script samples `/proc/<pid>/stat` and
`/proc/<pid>/status` before and after a fixed window, with **no IPC client attached**.
This matters: connecting a client is itself one of the conditions that disables the
reduced sampling regime, so the internal measurement cannot observe idle behaviour
without destroying it.

```bash
python3 scripts/measure-daemon.py <pid> 180
```

Memory is reported as both RSS and **PSS** (proportional set size). PSS is the honest
figure for a desktop application, because RSS counts shared library pages that are paid
for once by the whole system.

---

## Daemon CPU

| Configuration | Window | % of one core | % of the 24-thread CPU |
|---|---|---|---|
| Default tiers (2 s / 10 s / 60 s / 900 s) | 180 s | **0.244 %** | 0.010 % |
| Default tiers, long window | 662 s | **0.298 %** | 0.012 % |
| Reduced tiers (6 s / 30 s / 180 s / 45 min) | 180 s | **0.089 %** | 0.004 % |

Measured by `scripts/acceptance-test.sh`, which samples `/proc/<pid>/stat` externally.

The reduced row is exactly what the adaptive multiplier produces when the machine is idle
on battery with no window open. It was measured by configuring those intervals directly,
because the laptop was on AC during the measurement session and the adaptive path
correctly refuses to engage on AC — there is no battery to save.

**The ×3 multiplier delivers a 3× CPU reduction**, which is the whole point of it.

Startup costs a one-off **0.12 s**: probing 11 collectors, opening the database, taking
the first inventory.

### Where the time goes

Per-collector figures from a 1 729-second run. These are **wall-clock durations including
blocking I/O**, not CPU time — which is why they sum to more than the process' CPU time.
`thermal` looks expensive mostly because ACPI sensor reads block in firmware.

| Collector | Tier | Mean per run | Runs in 29 min |
|---|---|---|---|
| `process` | slow | 55.8 ms | 27 |
| `thermal` | medium | 8.6 ms | 173 |
| `network` | fast | 5.2 ms | 863 |
| `devices` | slow | 3.4 ms | 27 |
| `storage` | slow | 3.0 ms | 27 |
| `inventory` | glacial | 2.3 ms | 2 |
| `services` | slow | 1.2 ms | 27 |
| `gpu` | medium | 1.1 ms | 173 |
| `power` | medium | 0.8 ms | 173 |
| `cpu` | fast | 0.48 ms | 863 |
| `memory` | fast | 0.32 ms | 863 |

`process` is the single most expensive thing the daemon does — it walks `/proc` and reads
four small files per process across roughly 550 processes. It is on the slow tier for
that reason, and it can be turned off entirely (`[collectors] process = false`).

Two optimisations came directly out of these measurements during development:

* `network` was **15.3 ms** per run. It called `getifaddrs()` once per interface (walking
  the whole list each time) and counted TCP connections from `/proc/net/tcp{,6}` on every
  2-second tick. One `getifaddrs()` walk bucketed by name, plus a 30-second cache for
  addresses and connection counts, brought it to 5.2 ms — a third of the original, on the
  hottest tier.
* `storage` re-walked all of `/sys/class/hwmon` on every run to find one NVMe temperature.
  Resolving the channel once at probe time removed that.

---

## Wakeups

| Configuration | Voluntary context switches | Rate |
|---|---|---|
| Default tiers | 506 in 180 s | **2.81 / s** |
| Reduced tiers | 178 in 180 s | **0.99 / s** |

Voluntary context switches are an upper bound on wakeups: they count every blocking
syscall that sleeps, including slow ACPI sensor reads, not only `epoll_wait` returns. The
daemon's own epoll counter reports **0.85 / s** under default tiers with a client
attached, 0.68 / s without.

The single-timer design is what keeps this low. Eleven collectors across four tiers
produce one timer wakeup per tier deadline, not one per collector — the scheduler computes
a single next deadline as the minimum across all tiers and arms one `timerfd`.

Event-driven sources (journal, rtnetlink, uevent) contribute **zero** wakeups when nothing
is happening; they only wake the loop when the kernel has something to say.

---

## Discrete GPU: the number that mattered most

Measured on this hardware: calling into NVML wakes an RTX 5060 out of D3cold and holds it
awake for about **nine seconds**. At the medium tier that would keep the card permanently
powered.

```
Before the fix (NVML initialised at probe, queried on a timer):
    dGPU wake events every 20–40 s, several watts continuously

After (sysfs runtime_status gate + deferred nvmlInit):
    daemon running 90 s, runtime_active_time delta = 0 ms
```

A controlled A/B over 180-second windows, from `scripts/acceptance-test.sh`:

| | dGPU awake |
|---|---|
| Monitor **running** | **0 ms** of 180 000 ms |
| Monitor **stopped** | 919 ms of 180 000 ms |

The monitor-off window is *higher* because ordinary desktop activity woke the card
during it. The attributable figure is therefore negative, which is the strongest form
the claim can take: **JamSys is not merely a small contributor to discrete-GPU wake
time, it is not a contributor at all.**

Verified independently by reading
`/sys/class/drm/card2/device/power/runtime_status` in a loop from outside the daemon,
and by the kernel's own `runtime_active_time` accounting:

```bash
A=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
systemctl --user start jamsysd && sleep 90
B=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
echo $((B-A))   # 0
```

This was not correct on the first attempt. An earlier build initialised NVML during
`probe()`, which woke the GPU once at every daemon start. The application's own event log
is what surfaced it, and the fix was to `dlopen` the library at probe (free) and defer
`nvmlInit()` until a tick finds the GPU already awake.

---

## Memory

| Component | RSS | PSS | Threads |
|---|---|---|---|
| `jamsysd` (always running) | 29.2 MB | **26.8 MB** | 2 |
| `jamsys` window (transient) | 201–232 MB | **110 MB** | 16 |
| GNOME Shell readout | not separately resident — see below | | |
| `jamsys-helper` | not resident (oneshot on a timer) | | |
| `jamsys-kbd` | not resident (runs for milliseconds on a click) | | |

The Shell extension has no process of its own; it lives inside `gnome-shell`. Its cost
is a handful of `St` actors, one D-Bus proxy and no timer, and it cannot be isolated
from the compositor's own footprint by measurement. What *can* be stated precisely is
its work rate: it does nothing at all until the daemon pushes, and the daemon pushed 22
updates over several minutes on a live desktop.

The always-running component is **23.6 MB PSS**, comfortably inside the brief's
"below 100 MB total, preferably much lower".

The UI is not that small, and it would be dishonest to bury it. 141 MB of its RSS is
shared clean pages — the Python interpreter, PyGObject, GTK4, libadwaita and the Mesa GL
stack, most of which a GNOME session has already mapped. Its 110 MB PSS is comparable to
the main process of an Electron application on the same machine (102 MB PSS). If that
matters, the fix is a `gtk4-rs` front-end against the same socket, which removes the
Python interpreter entirely; the IPC protocol exists partly to make that substitution
cheap. The UI is also only resident while the window is open — closing it leaves the
daemon's 23.6 MB.

The daemon's second thread is the one reading `journalctl --follow`. There is no thread
pool and no async runtime.

Binary sizes: `jamsysd` **2.32 MB**, `jamsys-helper` **0.32 MB**, UI source 2 000
lines of Python (280 kB).

---

## Storage

Full resolution is held **in memory only**; what reaches the disk is a 10-second mean.
Measured over a clean 120-second window with 111 active series:

| | Measured |
|---|---|
| Rows written | 829 000 / day |
| Bytes per row | **14.2** |
| Disk growth | **11.8 MB / day** |
| In-memory full-resolution buffer | 72 kB for 4 418 points |

An earlier design wrote every 2-second sample and grew at ~96 MB/day. Aggregating to
10 seconds before the disk cut that by a factor of eight, and the narrow `WITHOUT ROWID`
layout keeps a row to 14 bytes.

Steady state at the default retention is roughly **75 MB** after 90 days, most of it
rollups rather than samples.

> A caution about the figure the daemon reports itself: `db_bytes ÷ uptime` overstates
> growth badly, because the database already contains history from previous runs. The
> acceptance script measures a delta over a fixed window instead. An earlier version of
> this document quoted 149 MB/day from the naive calculation; the measured figure is
> 11.8 MB/day.

To cut it further, shorten the raw window:

```toml
[retention]
raw_hours = 6        # ≈ 3 MB instead of 12 MB for the sample table
```

Writes are batched: the fast tier buffers in memory and flushes in **one transaction every
15 seconds**, so a 2-second sampling interval does not mean a disk write every 2 seconds.
WAL mode with `synchronous=NORMAL` avoids an fsync per commit while staying crash-safe.

Retention and rollups run at the glacial tier and delete in 5 000-row chunks, so a machine
that has been asleep for a week cannot stall the event loop catching up.

---

## Battery impact

The honest framing: JamSys's measurable cost is 0.089–0.30 % of one core and no discrete
GPU wake time.

On this laptop, idle system draw sits around 10–13 W and a fully-loaded CPU core adds
roughly 4–6 W. At 0.089 % of a core in the idle regime, the arithmetic gives on the order
of **5 milliwatts** — about 0.05 % of idle draw, which is below what `power_now` can
resolve (its quantisation is ~10 mW) and far below the tick-to-tick noise of the battery
gauge.

**So the correct statement is: the daemon's battery cost is below the measurement floor of
the machine's own power sensor.** I am not going to claim a number the hardware cannot
distinguish from zero. What can be stated positively is the thing that *would* have
mattered: had the GPU gate not been implemented, the 6.2 W the discrete GPU draws while
awake would have been a real and easily measurable ~50 % increase in idle power.

---

## Reproducing these measurements

```bash
# Live self-report
jamsys --json stats

# External, no client attached
python3 scripts/measure-daemon.py "$(pgrep -x jamsysd)" 180

# Discrete GPU wake time attributable to JamSys
A=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
sleep 90
B=$(cat /sys/class/drm/card2/device/power/runtime_active_time)
echo "$((B-A)) ms awake in 90 000 ms"
```

`scripts/measure-daemon.py` is shipped in the repository.


---

## Controlled fault injection

`scripts/fault-injection.sh` drives real faults and checks what the application does
about them. Results from the target machine.

### CPU load — 24 threads, 340 s

```
before:  cpu=3.4%   activity=interactive  temp=53 C
t+60s:   cpu=99.8%  activity=load         temp=65 C  freq=2258 MHz
t+150s:  cpu=99.6%  activity=load         temp=67 C  freq=2278 MHz
t+240s:  cpu=99.7%  activity=load         temp=68 C  freq=2217 MHz
t+320s:  cpu=99.3%  activity=load         temp=71 C  freq=2133 MHz
alert:   [NOTICE] CPU has been busy for a while
after:   cpu=5.9%   activity=interactive
```

The activity classifier moved into `load` and back, so nothing sampled during the run
contaminated the idle baselines. The `cpu.sustained_load` rule fired only after its
300-second dwell, and no temperature alert fired at 71 °C — correctly, since the
threshold is 90 °C.

An earlier version of this test ran for 90 s and sampled *after* the load stopped, and
therefore concluded nothing had been detected. The test was wrong, not the daemon.

### Memory pressure — 8 GiB allocated and freed

```
before:  76.4% available   PSI some=0.00
during:  50.2% available   PSI some=0.00
after:   76.6% available   (recovered)
no alert raised
```

Correct on both counts: the allocation was tracked, and **no alert fired**, because
50 % free memory is not a problem. PSI stayed at zero because nothing ever stalled.

### Failed systemd unit

```
daemon sees: ['snap.openshell.gateway.service', 'jamsys-faulttest.service']
alert:  [WARNING] jamsys-faulttest.service has failed
event:  unit_failed — Unit jamsys-faulttest.service entered a failed state
event:  kernel_error — Failed to start jamsys-faulttest.service (from the journal)
```

Detected by both routes independently: the systemd D-Bus query and the journal stream.
The pre-existing failed unit did **not** re-alert, because only newly-failed units emit.

### NVIDIA workload — offloaded render

```
before:  suspended
during:  active   util=6%   power=6.8 W   temp=47 C
after:   returned to suspended
events:  dgpu_wake, dgpu_sleep
dGPU awake during the whole test: 62 s of 110 s — all of it the render, none the monitor
```

NVML metrics are real when the card is up, and the wake/sleep transitions are recorded.

### Heavy disk write — 3 GB

```
before:  write = 146 kB/s
after:   write = 53.9 MB/s    NVMe temp 41.9 C
no alert raised
```

Throughput and NVMe temperature both tracked. No alert, correctly: a 3 GB write is
ordinary, and the disk-write rule needs a sustained excursion past a learned baseline.

### Not run automatically

| Test | Why |
|---|---|
| Wi-Fi disconnect | Interrupts a real network for ~10 s. `./fault-injection.sh wifi` |
| Bluetooth disconnect | Would disconnect paired devices. `./fault-injection.sh bluetooth` |
| Battery unplug | Physical |
| Suspend / resume | Ends the session running the tests |
| USB camera disconnect | Physical |

The suspend path is nonetheless covered by unit tests against synthetic clock values,
including an eight-hour sleep, and the drain rule by its own arithmetic test.

---

## Battery draw with the monitor on versus off

**Not measured: the machine was on AC for the whole session**, and `power_now` reports
charging current there, not system draw. `scripts/acceptance-test.sh` performs this
comparison automatically when run on battery — 20 medians either side of a daemon
restart — and skips it with an explanation otherwise.

What can be said without it:

* the daemon's own CPU cost in the reduced regime is 0.089 % of one core, which at
  roughly 4–6 W per fully-loaded core works out to about **5 mW**;
* `power_now` on this battery quantises at about 10 mW;
* so the daemon's draw is **below the resolution of the machine's own power sensor**,
  and no honest number can be quoted for it.

The figure that *is* measurable is the one that would have mattered: the discrete GPU
draws 6.8 W while awake, and a monitor that polled NVML on a timer would hold it there.
Against a 10–13 W idle draw that is a ~50 % increase. The gate is worth more than
everything else in this document combined.
