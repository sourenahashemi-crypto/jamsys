# Anomaly Model

There is no machine learning here, and deliberately so. Every alert this application
raises can be explained in one sentence of arithmetic that the user can check by hand.

Two layers run on every tick, in order.

---

## Layer 1 — deterministic safety rules

Fixed, physically-meaningful thresholds. These fire regardless of what the machine
considers "normal", because some things are never normal.

| Rule id | Condition | Level |
|---|---|---|
| `cpu.temp.critical` | package temp ≥ 95 °C sustained 60 s | CRITICAL |
| `cpu.temp.high` | package temp ≥ 90 °C sustained 120 s | WARNING |
| `cpu.throttling` | `package_throttle_count` increased over 2 consecutive medium ticks | WARNING |
| `cpu.sustained_load` | total CPU ≥ 90 % for 300 s | NOTICE |
| `cpu.runaway_process` | one process ≥ 85 % of one core for 300 s and not user-foreground | NOTICE |
| `mem.low` | `MemAvailable` < 8 % of total for 60 s | WARNING |
| `mem.critical` | `MemAvailable` < 3 % of total | CRITICAL |
| `mem.swap_sustained` | swap used > 25 % and rising for 300 s | NOTICE |
| `mem.oom` | `/proc/vmstat:oom_kill` increased, or journal OOM match | CRITICAL |
| `mem.pressure` | `memory` PSI `some avg60` > 20 | WARNING |
| `disk.full` | filesystem ≥ 95 % (real filesystems only) | CRITICAL |
| `disk.filling` | filesystem ≥ 90 % | WARNING |
| `disk.inodes` | inode use ≥ 90 % | WARNING |
| `disk.fs_errors` | `ext4 errors_count` increased | CRITICAL |
| `nvme.temp` | composite ≥ `temp1_crit` − 10 °C | WARNING |
| `nvme.smart` | `critical_warning != 0`, or media errors increased | CRITICAL |
| `nvme.spare` | `available_spare` < `available_spare_threshold` | CRITICAL |
| `nvme.wear` | `percentage_used` ≥ 90 | WARNING |
| `gpu.nvidia.temp` | ≥ 87 °C | WARNING |
| `gpu.nvidia.vram` | VRAM ≥ 95 % for 60 s | WARNING |
| `gpu.xid` | any Xid in the journal | CRITICAL (see Xid table) |
| `gpu.reset` | GPU reset / driver error in journal | CRITICAL |
| `thermal.fan_stalled` | fan = 0 rpm while its zone ≥ 70 °C for 60 s | CRITICAL |
| `battery.health` | `energy_full / energy_full_design` < 70 % | WARNING |
| `battery.critical` | ≤ 5 % and discharging | CRITICAL |
| `net.iface_down` | a previously-up managed iface goes down unexpectedly | WARNING |
| `net.flapping` | ≥ 3 up/down transitions in 300 s | WARNING |
| `net.errors` | error+drop rate > 1 % of packets over 60 s | WARNING |
| `net.dns_fail` | DNS unreachable while gateway reachable | WARNING |
| `net.no_internet` | gateway reachable, internet not, for 120 s | NOTICE |
| `service.failed` | a systemd unit enters `failed` | WARNING |
| `service.flapping` | ≥ 5 restarts in 600 s | WARNING |
| `bt.controller_error` | Bluetooth controller error in journal | WARNING |
| `audio.service_down` | PipeWire or WirePlumber not active | WARNING |
| `device.missing` | a learned internal device disappears | WARNING |
| `suspend.failed` | suspend aborted, or resume with device errors | WARNING |
| `suspend.drain` | > 2 %/h battery drain while suspended | NOTICE |

Every threshold is overridable per rule from the UI (`threshold_override` table); the
alert text always shows the threshold actually in force.

### Xid handling

NVIDIA Xid codes are not equally serious, and treating them all as CRITICAL is how
monitoring tools train users to ignore them. Codes are classified:

| Xid | Meaning | Level |
|---|---|---|
| 13, 31 | Graphics engine exception / MMU fault — usually an app bug | WARNING |
| 43 | Channel reset by user app | NOTICE |
| 45 | Preemptive channel removal (often normal on Ctrl-C) | INFO |
| 48, 63, 64 | Double-bit ECC | CRITICAL |
| 79 | **GPU has fallen off the bus** | CRITICAL |
| 92, 94, 95 | Contained/uncontained ECC | WARNING / CRITICAL |
| 119, 120 | GSP RPC timeout | WARNING |
| other | unclassified | WARNING |

---

## Layer 2 — learned baselines

### What is learned

For a selected set of *continuous, meaningfully-variable* metrics only — not everything.
Learning a baseline for "number of CPUs" is noise:

`power.system_w`, `cpu.usage_pct`, `cpu.package_temp_c`, `mem.used_bytes`,
`net.rx_bps` / `net.tx_bps` per interface, `disk.write_bps`, `gpu.*.power_w`,
`fan.*_rpm`, `nvme.temp_c`.

### Context partitioning

A sample is only ever compared against a baseline from the **same context**:

```
context = (ac | bat) × (idle | interactive | load)

load         ⇔  cpu_5min_avg ≥ 60 %  ∨  gpu ≥ 50 %
idle         ⇔  cpu_5min_avg < 8 %  ∧  gpu < 5 %  ∧  disk io < 2 MB/s
interactive  ⇔  everything in between
```

Six contexts. This is the single biggest false-positive reducer, and the three-way
activity split matters as much as the power split:

* "31 W" is alarming on battery-idle and completely normal on AC-active. Without the
  power axis the app cries wolf every time you unplug.
* **A game or a compile must never be compared against an idle baseline.** With only
  idle-versus-active, an evening of gaming and an afternoon of typing share a
  distribution, and the merged baseline is wide enough to hide real faults while still
  firing on ordinary work. `load` gets its own distribution entirely.

A test asserts that all thirty context keys — six, times the optional four time-of-day
buckets, plus the un-partitioned form — are distinct, and specifically that a high-load
context can never collide with an idle one.

Optional time-of-day partitioning (`[baseline] time_of_day = true`, default off) adds a
four-bucket split — night 00–06, morning 06–12, afternoon 12–18, evening 18–24 — computed
in **local** time via `localtime_r`, because a baseline split by "night" is meaningless if
night is computed in UTC. It is off by default because it multiplies warm-up time by four
for little gain on a personal laptop; turning it on takes effect on the next config
reload.

### The statistics

Robust statistics, because system metrics are heavy-tailed and a single compile will
otherwise poison a mean-and-stddev model for hours.

* **Median** and **MAD** (median absolute deviation) maintained over a bounded reservoir
  (default 512 samples per metric per context, reservoir-sampled so it stays
  representative without unbounded memory).
* Robust z-score: `z = 0.6745 × (x − median) / MAD`
  (0.6745 makes MAD a consistent estimator of σ for normally-distributed data).
* `p05` / `p95` are reported to the user as the "normal range", because a percentile
  range is something a non-expert can read; the z-score decides *whether* to fire, the
  percentile range explains *why*.

### Guards against nonsense

A statistical detector with no guards produces garbage in exactly these five ways, so
each is handled explicitly:

1. **Warm-up.** No baseline alert until `n ≥ 120` samples in that context
   (~20 min at the medium tier). Until then the Coverage page shows "learning".
2. **Degenerate MAD.** If `MAD == 0` (a metric that has been perfectly constant), the
   z-score is infinite. Floor MAD at a per-metric `min_mad` (e.g. 0.5 W for power,
   1 °C for temperature) so a constant metric needs a *real* change to fire.
3. **Absolute floor.** A relative rule also needs an absolute delta to matter. Idle power
   going from 9.0 W to 9.8 W can be a 4σ event and is still irrelevant. Every learned rule
   carries `min_abs_delta`.
4. **Dwell time.** The deviation must persist (default 600 s for power, 300 s for memory).
   No alert on a single spike.
5. **Self-exclusion.** Samples taken while an alert of the same rule is already firing are
   *not* fed back into the baseline, so an ongoing fault cannot slowly normalise itself.

### Learned rules

| Rule id | Fires when | Level |
|---|---|---|
| `power.idle_high` | `power.system_w` z > 3.5 ∧ Δ ≥ 5 W ∧ activity = idle ∧ 600 s | NOTICE |
| `power.discharge_high` | discharge > p95 ∧ Δ ≥ 8 W ∧ 300 s | NOTICE |
| `mem.leak` | one process' RSS monotonically ↑ over 30 min with slope ≥ 100 MB/h and total growth ≥ 1 GB | NOTICE |
| `mem.growth` | `mem.used_bytes` z > 3.5 ∧ Δ ≥ 2 GB ∧ 600 s | NOTICE |
| `net.traffic_spike` | rx or tx > max(p95 × 4, 5 MB/s) for 120 s | NOTICE |
| `thermal.unusual` | temp z > 3.5 ∧ Δ ≥ 10 °C ∧ 300 s | NOTICE |
| `gpu.idle_awake` | dGPU `runtime_status = active` ∧ util = 0 % ∧ no GPU process ∧ 300 s | NOTICE |
| `disk.write_spike` | write throughput > p95 × 5 ∧ ≥ 50 MB/s ∧ 300 s | NOTICE |

Note `mem.leak` is a **trend** test (Theil–Sen slope over the 30-minute window), not a
z-score — a leak is defined by monotone growth, not by being far from the median.

---

## Explanation is part of the alert, not an afterthought

Every rule must produce an `Explanation` struct. The type system enforces it — there is no
constructor for an alert without one:

```rust
pub struct Explanation {
    pub what:      String,            // the measurement, with units
    pub expected:  Option<String>,    // learned range or threshold in force
    pub since:     Duration,          // how long it has been true
    pub evidence:  Vec<Evidence>,     // contributing measurements
    pub likely_cause: Option<String>, // only when confidence is high
    pub actions:   Vec<String>,       // 1-2 safe, read-only diagnostics
}
```

`likely_cause` is populated only by **correlation rules with a stated precondition** —
never guessed. For example `power.idle_high` checks whether the dGPU is awake, whether a
process is above its own baseline, and whether a USB device was just added, and names only
the one that actually correlates. If nothing correlates, the field stays `None` and the
alert says so rather than inventing a cause.

The rendered result:

> **NOTICE — Abnormally high power draw while idle**
> Battery discharge is **28.4 W**. Your learned idle range on battery is
> **8.7 – 13.5 W** (median 10.9 W, from 3 412 samples over 6 days).
> This has been true for **11 minutes**.
> The NVIDIA GPU is **active** and drawing **6.2 W** although its utilisation is **0 %**
> and no process is using it.
> *Suggested checks:* `cat /sys/class/drm/card2/device/power/runtime_status` ·
> open the GPU page to see which process is holding the device awake.

Never: "Anomaly detected."

---

## Alert lifecycle: dedup, rate limiting, resolution

```
        fire()
          │
          ▼
   ┌─────────────┐  same fingerprint within cooldown?  ┌──────────────┐
   │  candidate  │ ──────────── yes ─────────────────▶ │ coalesce:    │
   └──────┬──────┘                                     │ count += 1   │
          │ no                                         │ last_ts = now│
          ▼                                            └──────────────┘
   suppressed? (mute / snooze / ignore rule / ignore instance)
          │ no
          ▼
   token bucket (per severity)  ─── empty ──▶  drop + increment "suppressed" counter
          │ ok
          ▼
   INSERT ... ON CONFLICT(fingerprint) WHERE resolved_ts IS NULL
          │
          ▼
   notify (desktop) + push to connected UIs
```

* **Fingerprint** = `rule_id + ':' + instance` (e.g. `disk.full:/home`). Stable across
  daemon restarts, so a restart does not re-notify.
* **Cooldown** per severity: INFO 300 s, NOTICE 300 s, WARNING 180 s, CRITICAL 60 s.
* **Token bucket** per severity: 10 tokens, refill 1 per 60 s. Prevents a flapping sensor
  from producing hundreds of notifications; the suppressed count is shown in the UI so
  suppression is visible rather than silent.
* **Hysteresis on resolve.** An alert resolves only when the condition has been false for
  `max(dwell, 120 s)`. Without this, a metric oscillating around the threshold produces
  an endless fire/resolve stream.
* **Escalation** re-notifies once if severity increases (NOTICE → WARNING → CRITICAL).

## Noise control for the journal

The journal is the noisiest source on any Linux laptop, so it is filtered three times:

1. **At the source** — `journalctl -p 4` (warning and above) so info-level chatter never
   crosses the pipe.
2. **Pattern classification** — a table of compiled regexes maps a message to a
   `kind` + severity. Anything unmatched is recorded as an `event` but does **not**
   create an alert.
3. **Known-benign list** — recurring harmless firmware messages on this hardware
   (ACPI `BIOS bug` notices, `ACPI Error: ... _PSS`, expected `iwlwifi`/`rtw89` debug
   resets, `psmouse` probe noise) are shipped as a default suppression list, editable in
   the UI. Identical messages are additionally collapsed with a 300 s window and a count.
