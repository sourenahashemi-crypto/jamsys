# Sampling Strategy

The design goal is stated as a constraint rather than an aspiration: **the monitor must
never become a meaningful battery consumer.** Everything below follows from that.

---

## 1. Events first, polling only where no event exists

Polling is the fallback, not the default. A source is polled only when the kernel offers
no way to be told.

| Source | Interface | Wakes the daemon when |
|---|---|---|
| Kernel and service errors | `journalctl --follow -o json -p 4`, stdout in epoll | a warning-or-worse is logged |
| Link, address, route changes | `NETLINK_ROUTE` multicast groups | an interface or route actually changes |
| Device hotplug | `NETLINK_KOBJECT_UEVENT` group 1 | a device is added or removed |
| Shutdown, config reload | `signalfd` | a signal arrives |
| UI attach and requests | Unix socket listener | a client connects or asks |
| Suspend and resume | derived from `CLOCK_BOOTTIME − CLOCK_MONOTONIC` | *no wakeup at all* — it is noticed on the next ordinary tick |

None of these cost anything while the system is quiet. A laptop sitting on a desk with no
network changes and a clean log wakes the daemon only for its timer.

### Why suspend/resume is derived rather than subscribed

`logind`'s `PrepareForSleep` signal is the conventional route. Two clock reads are better
here: they cannot be missed if the daemon was not scheduled at the moment the signal
fired, they work with no D-Bus connection at all, and they yield the sleep duration to the
millisecond as a side effect. The trade-off is that resume is noticed on the next tick
rather than instantly, which is irrelevant for the questions being asked of it.

## 2. One timer, not one per collector

Every scheduled collector shares a **single `timerfd`** armed as a one-shot. The scheduler
computes the next deadline as the minimum across all tiers and arms once.

Eleven collectors across four tiers therefore produce **one wakeup per tier deadline**,
not eleven. Measured: 0.68 epoll wakeups/s at default intervals.

One-shot rather than periodic is deliberate — intervals change at runtime (idle
throttling, config reload) and a periodic timer would drift or fire spuriously.

After a resume every deadline is in the past. The scheduler **rebases** rather than
catching up, so waking from an overnight sleep does not fire hours of missed ticks at once.

## 3. Tiers

Assignment is by *cost and rate of change*, not by perceived importance.

| Tier | Default | Reduced (idle on battery) | Collectors | Why |
|---|---|---|---|---|
| **Fast** | 2 s | 6 s | `cpu`, `memory`, `network` | Single small `/proc` reads, sub-millisecond, and genuinely fast-moving |
| **Medium** | 10 s | 30 s | `thermal`, `power`, `gpu` | hwmon and ACPI reads block in firmware; temperature and battery do not move meaningfully in 2 s |
| **Slow** | 60 s | 180 s | `storage`, `services`, `devices`, `process` | `statvfs`, a D-Bus round trip, a `/proc` walk — tens of milliseconds |
| **Glacial** | 900 s | 2700 s | `inventory`, SMART, rollups, retention, GC | Expensive and near-static |
| **Event** | — | — | journal, netlink, uevent | Never scheduled |

Measured cost per run on the target machine:

```
process    55.8 ms   ← by far the most expensive; slow tier, and disableable
thermal     8.6 ms   ← ACPI blocking, not CPU
network     5.2 ms
devices     3.4 ms
storage     3.0 ms
gpu         1.1 ms
power       0.8 ms
cpu         0.48 ms
memory      0.32 ms
```

## 4. Adaptive throttling

Every interval is multiplied by 3 when **all** of:

* the machine is **on battery** — on AC there is no battery to save, so full rate is correct;
* the machine is **idle** — 5-minute CPU average below the threshold, GPU utilisation
  under 5 %, disk I/O under 2 MB/s;
* **no UI client is attached** — if a window is open, the user is looking at it.

Because the multiplier is applied in the scheduler rather than inside collectors, it
reduces *wakeups* and not merely work per wakeup.

Measured effect: **0.267 % → 0.089 % of one core, and 2.63 → 0.99 context switches/s.**

Attaching a client necessarily ends the reduced regime, so the current multiplier cannot
be observed over IPC without destroying it. The daemon therefore also reports
`idle_ticks`, `idle_tick_fraction` and `idle_blocked_by` — a plain reason string such as
`"on AC power"` — so the behaviour is inspectable after the fact.

## 5. Conditional sampling: the NVIDIA rule

Some metrics cost more than their acquisition time.

**Measured on this hardware: reading NVML wakes a runtime-suspended RTX 5060 and holds it
awake for about nine seconds, drawing ~6 W against a 10–13 W idle system draw.** Polling
it every 10 s keeps the card permanently powered — the monitor becomes the drain it exists
to detect.

So the GPU collector:

1. reads `runtime_status` from sysfs (free, verified not to wake the device);
2. calls NVML **only** when it already reads `active`;
3. defers `nvmlInit()` itself until that first awake tick — `dlopen` at probe is free, but
   initialisation is not.

A suspended GPU is reported as suspended at 0 W, which is both true and costless. The
failure users care about — the dGPU awake with nothing using it — is still detected,
because `runtime_status` *is* that signal.

**Verified: 0 ms of dGPU wake time attributable to JamSys over 90 seconds**, by the
kernel's own `runtime_active_time` accounting.

The same principle governs elsewhere: reachability probes run at most once a minute;
interface addresses and TCP connection counts are cached for 30 s; hwmon channels are
resolved once at probe rather than rediscovered per run.

## 6. Write batching

The fast tier samples every 2 s but **does not write every 2 s**. Samples accumulate in a
bounded in-memory ring and flush in **one transaction every 15 s**. WAL mode with
`synchronous=NORMAL` avoids an fsync per commit while remaining crash-safe.

Full-resolution data never reaches the disk at all — see `database-schema.md`. What is
persisted is a 10-second aggregate, which cuts write volume roughly fivefold against
storing raw fast-tier samples.

Retention and rollups run at the glacial tier and delete in 5 000-row chunks, so a machine
that has been asleep for a week cannot stall the event loop catching up.

## 7. What is deliberately not sampled

| Not collected | Why |
|---|---|
| Packet contents | Excluded by design. No `AF_PACKET` socket exists in the codebase. |
| Per-process network bandwidth | Needs socket-inode matching across every `/proc/<pid>/fd`, roughly doubling the cost of the most expensive collector. |
| Other users' process I/O and GPU | Would need root. Own-user processes work unprivileged and that is what is shown. |
| Intel GPU busy % | `i915` exposes none without `i915_perf` and `CAP_PERFMON`. RC6 residency is reported instead, labelled as what it is. |
| SATA/SCSI SMART | Would mean spawning `smartctl`. The helper is ioctl-only and NVMe-only. |
| Anything "because it is available" | Each metric has to answer a question a user would actually ask. |

## 8. Configuration

```toml
[sampling]
fast_ms          = 2000
medium_ms        = 10000
slow_ms          = 60000
glacial_ms       = 900000
idle_multiplier  = 3.0     # applied when on battery + idle + no client
idle_cpu_pct     = 8.0     # below this 5-minute average counts as idle
flush_ms         = 15000   # one SQLite transaction per this interval
```

Every collector can also be disabled outright, from the file or the Coverage page:

```toml
[collectors]
process = false            # removes the 55.8 ms/run scan entirely
```

`systemctl --user reload jamsysd` applies changes without a restart.
