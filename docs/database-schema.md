# Data Schema

SQLite, WAL mode, one file: `~/.local/share/jamsys/history.db`.
All timestamps are `INTEGER` **Unix epoch milliseconds, UTC**.

Pragmas set at open:
```sql
PRAGMA journal_mode=WAL;      -- concurrent UI reads while daemon writes
PRAGMA synchronous=NORMAL;    -- WAL + NORMAL is crash-safe and avoids fsync per commit
PRAGMA busy_timeout=3000;
PRAGMA foreign_keys=ON;
PRAGMA auto_vacuum=INCREMENTAL;
```

## Why a narrow (long) table instead of a wide one

Metrics differ per machine — no battery on a desktop, no `asus` hwmon on a Dell, a
variable number of cores, disks and interfaces. A wide table would need a schema
migration for every hardware difference. So samples are stored **narrow**, with the
metric identity interned in a dimension table:

```sql
CREATE TABLE metric (
  id          INTEGER PRIMARY KEY,
  subsystem   TEXT NOT NULL,          -- 'cpu','memory','gpu','power','thermal',
                                      -- 'storage','network','process'
  name        TEXT NOT NULL,          -- 'usage_pct','temp_c','power_w','rx_bps'
  instance    TEXT NOT NULL DEFAULT '', -- 'core3','wlp108s0','nvme0n1','card2',''
  unit        TEXT NOT NULL DEFAULT '', -- '%','C','W','B/s','MHz','rpm'
  UNIQUE(subsystem, name, instance)
);
```

Interning means a sample row is 3 integers + 1 real — the id is resolved once at startup
and cached in a `HashMap` in the daemon, so the hot path does no `SELECT`.

## Time series

```sql
-- Full resolution. Written in one transaction every 15 s.
CREATE TABLE sample (
  ts          INTEGER NOT NULL,
  metric_id   INTEGER NOT NULL REFERENCES metric(id),
  value       REAL    NOT NULL,
  PRIMARY KEY (metric_id, ts)
) WITHOUT ROWID;

-- Aggregates. bucket = 60000 | 300000 | 900000 (ms)
CREATE TABLE rollup (
  bucket      INTEGER NOT NULL,
  ts          INTEGER NOT NULL,       -- start of the bucket
  metric_id   INTEGER NOT NULL REFERENCES metric(id),
  min_v       REAL NOT NULL,
  max_v       REAL NOT NULL,
  avg_v       REAL NOT NULL,
  p95_v       REAL NOT NULL,
  n           INTEGER NOT NULL,
  PRIMARY KEY (bucket, metric_id, ts)
) WITHOUT ROWID;
```

`WITHOUT ROWID` with a `(metric_id, ts)` primary key stores each series contiguously,
which is exactly the access pattern for "draw me the last 24 h of CPU temperature" and
keeps the file small.

### Resolution tiers

Full-resolution data **never reaches the disk**. At 111 series on a 2-second tier that
would be ~3.7 M rows and ~96 MB a day, for detail nobody looks at more than a few minutes
after the fact.

| Tier | Where | Default retention | Config key |
|---|---|---|---|
| Full resolution (2 s) | **memory only**, a bounded ring | 30 min | `live_window_ms` |
| 10-second means | `sample` | 24 h | `raw_hours` |
| 1-minute aggregates | `rollup` bucket 60000 | 7 days | `minute_days` |
| 5-minute aggregates | `rollup` bucket 300000 | 30 days | `five_min_days` |
| 15-minute aggregates | `rollup` bucket 900000 | 90 days | `fifteen_min_days` |
| Events | `event` | 90 days | `event_days` |
| Alerts | `alert` | 90 days | `alert_days` |

`push()` therefore has two destinations, neither of them the disk: the in-memory ring,
and a 10-second accumulator. When a bucket closes its mean moves to the pending queue,
and the queue is written in one transaction every 15 s. A ring entry is `(i64, f32)`,
capped at 1 200 points per series — measured at 21.8 kB for 1 196 live points.

`flush_final()` force-closes the bucket still being accumulated and is used at shutdown,
so stopping the daemon does not silently discard up to ten seconds of samples.

Retention is enforced by a maintenance pass at the glacial tier: roll up → delete expired
→ `PRAGMA incremental_vacuum(256)`. Deletion is chunked (`LIMIT 5000`) so a long-idle
machine catching up cannot stall the event loop.

## Events and alerts

```sql
-- Discrete things that happened. Distinct from alerts: an event is a fact,
-- an alert is a judgement.
CREATE TABLE event (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  subsystem TEXT NOT NULL,
  kind      TEXT NOT NULL,   -- 'wifi_down','wifi_up','suspend','resume','usb_add',
                             -- 'usb_remove','unit_failed','xid','oom','route_change'
  summary   TEXT NOT NULL,
  detail    TEXT,            -- JSON
  severity  INTEGER NOT NULL -- 0 INFO 1 NOTICE 2 WARNING 3 CRITICAL
);
CREATE INDEX event_ts ON event(ts DESC);

CREATE TABLE alert (
  id            INTEGER PRIMARY KEY,
  fingerprint   TEXT NOT NULL,     -- stable identity for dedup: rule + instance
  rule_id       TEXT NOT NULL,
  severity      INTEGER NOT NULL,
  first_ts      INTEGER NOT NULL,
  last_ts       INTEGER NOT NULL,
  resolved_ts   INTEGER,           -- NULL while firing
  count         INTEGER NOT NULL DEFAULT 1,
  title         TEXT NOT NULL,
  explanation   TEXT NOT NULL,     -- the "why", rendered with real numbers
  evidence      TEXT NOT NULL,     -- JSON: measured vs expected, contributors
  suggestion    TEXT,              -- 1-2 safe diagnostic actions
  acked_ts      INTEGER
);
CREATE UNIQUE INDEX alert_open ON alert(fingerprint) WHERE resolved_ts IS NULL;
```

The partial unique index is the deduplication mechanism *in the database*, not just in
memory: **at most one open alert can exist per fingerprint**, enforced by SQLite. A
re-fire updates `last_ts` and `count` via `ON CONFLICT`. Restarting the daemon cannot
duplicate an open alert.

## Learned baselines

```sql
CREATE TABLE baseline (
  metric_id  INTEGER NOT NULL REFERENCES metric(id),
  context    TEXT NOT NULL,  -- 'ac:idle','bat:idle','ac:active','bat:active'
  n          INTEGER NOT NULL,
  median     REAL NOT NULL,
  mad        REAL NOT NULL,  -- median absolute deviation (robust sigma)
  p05        REAL NOT NULL,
  p95        REAL NOT NULL,
  updated_ts INTEGER NOT NULL,
  sketch     BLOB,           -- compact reservoir for incremental percentiles
  PRIMARY KEY (metric_id, context)
) WITHOUT ROWID;
```

Baselines are per **context**, because "normal" genuinely differs between plugged-in and
on-battery, and between idle and active. Comparing a battery-idle sample against an
AC-active baseline is the classic source of false alerts, so context is part of the key.

## Inventory and suppressions

```sql
CREATE TABLE inventory (            -- current hardware/software baseline
  key       TEXT PRIMARY KEY,       -- 'cpu.model','bios.version','gpu.0.driver', ...
  value     TEXT NOT NULL,
  first_ts  INTEGER NOT NULL,
  last_ts   INTEGER NOT NULL
);
CREATE TABLE inventory_change (     -- append-only diff log, survives kernel updates
  id       INTEGER PRIMARY KEY,
  ts       INTEGER NOT NULL,
  key      TEXT NOT NULL,
  old_value TEXT,
  new_value TEXT
);

CREATE TABLE suppression (          -- mute / snooze / ignore, set from the UI
  id       INTEGER PRIMARY KEY,
  scope    TEXT NOT NULL,   -- 'rule' | 'instance' | 'fingerprint'
  pattern  TEXT NOT NULL,   -- e.g. 'service.failed' or 'snap.openshell.gateway.service'
  until_ts INTEGER,         -- NULL = forever ("ignore"), else snooze deadline
  reason   TEXT,
  created_ts INTEGER NOT NULL
);

CREATE TABLE threshold_override (   -- user-adjusted L1 thresholds
  rule_id  TEXT NOT NULL,
  key      TEXT NOT NULL,
  value    REAL NOT NULL,
  PRIMARY KEY (rule_id, key)
);

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);  -- schema_version, install_ts
```

## Sizing

**Measured** on the target machine with 111 active metric series at the default tiers,
after moving full resolution into memory:

| | Before (raw 2 s on disk) | After (10 s means on disk) |
|---|---|---|
| Rows per day | 3.71 M | ~0.74 M |
| Bytes per day | ~96 MB | **~19 MB** |

Steady state after 90 days at the default retention:

| Table | Kept | Approximate size |
|---|---|---|
| `sample` (10 s means) | 24 h | ~19 MB |
| `rollup` 1 min | 7 days | ~20 MB |
| `rollup` 5 min | 30 days | ~17 MB |
| `rollup` 15 min | 90 days | ~17 MB |
| everything else | 90 days | ~2 MB |
| **total** | | **~75 MB** |

Live observation after several hours of running: 1.9 MB holding 76 744 ten-second
samples and 5 795 rollups.

The rollups are what every chart longer than about an hour reads, and the in-memory ring
serves everything shorter, so the `sample` table is only consulted for the middle range.
Shortening `raw_hours` costs little beyond the resolution of "what exactly happened at
09:14 this morning". The Diagnostics page shows the live figures.
