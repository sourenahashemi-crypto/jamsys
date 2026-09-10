# IPC Protocol

Transport: `SOCK_STREAM` Unix socket at `$XDG_RUNTIME_DIR/jamsys/sock` (mode 0600 in a
0700 directory; peer UID additionally checked via `SO_PEERCRED`).

Framing: **newline-delimited JSON**. One request object per line, one response object per
line. Max 64 KiB per line. Unsolicited push frames may arrive at any time after
`subscribe`.

```jsonc
// request
{"id": 7, "op": "snapshot"}
// response
{"id": 7, "ok": true, "data": { ... }}
// error
{"id": 7, "ok": false, "error": {"code": "unsupported", "message": "..."}}
// push (id is absent)
{"push": "alert", "data": { ... }}
```

## Operations

| op | params | returns |
|---|---|---|
| `ping` | — | `{version, pid, uptime_s, schema}` |
| `snapshot` | — | full current state: every subsystem's latest values + health |
| `history` | `{subsystem, name, instance?, since_ms, until_ms?, max_points?}` | downsampled series, auto-selecting raw/1m/5m/15m by span |
| `alerts` | `{open_only?, limit?}` | alert list with explanation + evidence |
| `events` | `{since_ms?, limit?, subsystem?}` | event list |
| `processes` | `{sort?, limit?}` | process table with anomaly flags |
| `coverage` | — | per-metric support state + reason + quarantine info |
| `inventory` | `{changes?}` | current inventory or the change log |
| `ack` | `{alert_id}` | mark acknowledged |
| `mute` | `{fingerprint, until_ms?}` | mute/snooze |
| `suppress` | `{scope, pattern, until_ms?, reason?}` | ignore rule/instance |
| `unsuppress` | `{id}` | remove a suppression |
| `set_threshold` | `{rule_id, key, value}` | override an L1 threshold |
| `set_collector` | `{name, enabled}` | enable/disable a collector at runtime |
| `subscribe` | `{topics: ["alert","event","tick"]}` | begin push frames |
| `stats` | — | daemon self-metrics (CPU, RSS, wakeups, DB size) |

There is **no** operation that executes a command, reads an arbitrary path, or writes
outside the daemon's own state. `history` takes a metric *identity*, not SQL. Adding an
operation is the only way to extend the surface, which is the point.

## Versioning

`ping.schema` is an integer, currently `1`. The UI refuses to run against a newer major
schema and says so rather than misrendering. Unknown fields in responses must be ignored
by clients, so additive changes do not require a version bump.


---

# D-Bus surface (the GNOME Shell readout)

A second, deliberately tiny surface for the corner readout. The Shell extension runs
inside the compositor, so it gets eight numbers pushed to it rather than a protocol to
implement.

| | |
|---|---|
| Bus | session |
| Name | `org.jamsys.Daemon` |
| Object | `/org/jamsys/Daemon` |
| Interface | `org.jamsys.Daemon` |

The name is **not** `org.jamsys.Monitor`: that is the GTK window's GApplication id,
which must match its `.desktop` filename. Sharing one name makes GApplication mistake
the daemon for another copy of itself and refuse to start.

| Member | Type | Purpose |
|---|---|---|
| `GetState()` | `→ s` | current widget state as a JSON object |
| `GetVersion()` | `→ s` | daemon version |
| `AckTopAlert()` | `→ b` | acknowledge the highest-severity open alert |
| `StateChanged` | signal `s` | pushed **only** when a displayed value materially changed |

`org.freedesktop.DBus.Introspectable.Introspect` and `org.freedesktop.DBus.Peer` are
implemented, because every client calls Introspect before anything else and hangs
without it.

## The state object

```jsonc
{
  "health": "healthy",        // healthy | attention | critical | unknown
  "cpu_pct": 6.1, "cpu_temp_c": 52.0,
  "mem_pct": 20.5, "gpu_pct": 0.0,
  "power_w": 11.2,            // positive discharging, negative charging
  "battery_pct": 87.0, "on_battery": true, "has_battery": true,
  "net_ok": true, "net_label": "-48 dBm",
  "dgpu": "suspended",        // runtime power state, never a woken query
  "alert_title": "", "alert_detail": "", "alert_expected": "",
  "alert_subsystem": "",      // names the UI page to open
  "alert_severity": 0, "open_alerts": 0
}
```

## Why "materially changed" is decided here

Quantisation lives in the daemon, once, rather than in the extension: percentages to
whole numbers, temperature to whole degrees, power to 0.1 W. A reading that would not
alter a single rendered character produces no signal at all, so the panel does not
repaint several times a second for sensor noise. Attaching to the bus and watching
`StateChanged` is therefore a faithful picture of how often the widget actually redraws.

## Not exposed here

History, processes, inventory, coverage, suppressions and threshold changes are
Unix-socket operations only. The Shell extension has no business with any of them, and
keeping them off the bus keeps the surface reachable from the compositor as small as
it can be.
