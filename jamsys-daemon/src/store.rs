//! SQLite persistence: samples, rollups, events, alerts, baselines, inventory,
//! suppressions. See `docs/DATA-SCHEMA.md` for the rationale behind the narrow layout.

use crate::clock::now_ms;
use crate::config::Retention;
use crate::types::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, VecDeque};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    conn: Connection,
    /// MetricId -> row id. Resolved once so the hot path never issues a SELECT.
    ids: HashMap<MetricId, i64>,
    /// Closed 10-second aggregates awaiting the next disk flush. Bounded; see `push`.
    pending: Vec<(i64, i64, f64)>,
    last_flush_ms: i64,
    /// Full-resolution recent history, **memory only**. This is what the UI's live
    /// view and the sparklines read. Full-resolution data is never written to disk:
    /// at 111 series and a 2-second tier that would be ~96 MB/day, for detail nobody
    /// looks at more than a few minutes after the fact.
    live: HashMap<i64, VecDeque<(i64, f32)>>,
    /// The 10-second bucket currently being accumulated, per metric.
    agg: HashMap<i64, Accum>,
    /// Start of the bucket in `agg`.
    agg_bucket: i64,
    live_window_ms: i64,
}

/// Running aggregate for one metric over one 10-second bucket.
#[derive(Clone, Copy, Debug, Default)]
struct Accum {
    sum: f64,
    n: u32,
    min: f64,
    max: f64,
}

impl Accum {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        } else {
            if v < self.min { self.min = v }
            if v > self.max { self.max = v }
        }
        self.sum += v;
        self.n += 1;
    }
    fn mean(&self) -> f64 {
        if self.n == 0 { f64::NAN } else { self.sum / self.n as f64 }
    }
}

const MAX_PENDING: usize = 20_000;
/// Disk resolution. The spec's tier: 10-second samples retained for 24 hours.
pub const DISK_BUCKET_MS: i64 = 10_000;
/// Cap per series so the in-memory ring cannot grow without bound on a long uptime.
const LIVE_MAX_POINTS: usize = 1_200;

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    pub fn open_memory() -> rusqlite::Result<Store> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> rusqlite::Result<Store> {
        // WAL lets the UI read while the daemon writes. NORMAL avoids an fsync per
        // commit while remaining crash-safe under WAL.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 3000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        let mut s = Store {
            conn,
            ids: HashMap::new(),
            pending: Vec::new(),
            last_flush_ms: now_ms(),
            live: HashMap::new(),
            agg: HashMap::new(),
            agg_bucket: now_ms() / DISK_BUCKET_MS * DISK_BUCKET_MS,
            live_window_ms: 30 * 60 * 1000,
        };
        s.migrate()?;
        s.load_ids()?;
        Ok(s)
    }

    fn migrate(&mut self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS metric (
  id        INTEGER PRIMARY KEY,
  subsystem TEXT NOT NULL,
  name      TEXT NOT NULL,
  instance  TEXT NOT NULL DEFAULT '',
  unit      TEXT NOT NULL DEFAULT '',
  UNIQUE(subsystem, name, instance)
);

CREATE TABLE IF NOT EXISTS sample (
  ts        INTEGER NOT NULL,
  metric_id INTEGER NOT NULL REFERENCES metric(id),
  value     REAL NOT NULL,
  PRIMARY KEY (metric_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS rollup (
  bucket    INTEGER NOT NULL,
  ts        INTEGER NOT NULL,
  metric_id INTEGER NOT NULL REFERENCES metric(id),
  min_v REAL NOT NULL, max_v REAL NOT NULL, avg_v REAL NOT NULL,
  p95_v REAL NOT NULL, n INTEGER NOT NULL,
  PRIMARY KEY (bucket, metric_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS event (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  subsystem TEXT NOT NULL,
  kind      TEXT NOT NULL,
  summary   TEXT NOT NULL,
  detail    TEXT,
  severity  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS event_ts ON event(ts DESC);

CREATE TABLE IF NOT EXISTS alert (
  id          INTEGER PRIMARY KEY,
  fingerprint TEXT NOT NULL,
  rule_id     TEXT NOT NULL,
  severity    INTEGER NOT NULL,
  first_ts    INTEGER NOT NULL,
  last_ts     INTEGER NOT NULL,
  resolved_ts INTEGER,
  count       INTEGER NOT NULL DEFAULT 1,
  title       TEXT NOT NULL,
  explanation TEXT NOT NULL,
  evidence    TEXT NOT NULL,
  suggestion  TEXT,
  acked_ts    INTEGER
);
-- At most one OPEN alert per fingerprint, enforced by the database rather than by
-- daemon memory, so a restart cannot duplicate a firing alert.
CREATE UNIQUE INDEX IF NOT EXISTS alert_open ON alert(fingerprint) WHERE resolved_ts IS NULL;
CREATE INDEX IF NOT EXISTS alert_ts ON alert(last_ts DESC);

CREATE TABLE IF NOT EXISTS baseline (
  metric_id  INTEGER NOT NULL REFERENCES metric(id),
  context    TEXT NOT NULL,
  n          INTEGER NOT NULL,
  median     REAL NOT NULL,
  mad        REAL NOT NULL,
  p05        REAL NOT NULL,
  p95        REAL NOT NULL,
  updated_ts INTEGER NOT NULL,
  sketch     BLOB,
  PRIMARY KEY (metric_id, context)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS inventory (
  key      TEXT PRIMARY KEY,
  value    TEXT NOT NULL,
  first_ts INTEGER NOT NULL,
  last_ts  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS inventory_change (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  key       TEXT NOT NULL,
  old_value TEXT,
  new_value TEXT
);

CREATE TABLE IF NOT EXISTS suppression (
  id         INTEGER PRIMARY KEY,
  scope      TEXT NOT NULL,
  pattern    TEXT NOT NULL,
  until_ts   INTEGER,
  reason     TEXT,
  created_ts INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS threshold_override (
  rule_id TEXT NOT NULL,
  key     TEXT NOT NULL,
  value   REAL NOT NULL,
  PRIMARY KEY (rule_id, key)
);
"#,
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(k,v) VALUES('schema_version', ?1), ('install_ts', ?2)",
            params![SCHEMA_VERSION.to_string(), now_ms().to_string()],
        )?;
        Ok(())
    }

    fn load_ids(&mut self) -> rusqlite::Result<()> {
        let mut st = self.conn.prepare("SELECT id, subsystem, name, instance FROM metric")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?;
        for row in rows {
            let (id, sub, name, inst) = row?;
            // Leaking the two &'static str fields is bounded by the number of distinct
            // metric names the binary knows about (~60), not by runtime input.
            let m = MetricId {
                subsystem: Box::leak(sub.into_boxed_str()),
                name: Box::leak(name.into_boxed_str()),
                instance: inst,
            };
            self.ids.insert(m, id);
        }
        Ok(())
    }

    pub fn metric_id(&mut self, m: &MetricId, unit: &str) -> rusqlite::Result<i64> {
        if let Some(id) = self.ids.get(m) {
            return Ok(*id);
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO metric(subsystem,name,instance,unit) VALUES(?1,?2,?3,?4)",
            params![m.subsystem, m.name, m.instance, unit],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM metric WHERE subsystem=?1 AND name=?2 AND instance=?3",
            params![m.subsystem, m.name, m.instance],
            |r| r.get(0),
        )?;
        self.ids.insert(m.clone(), id);
        Ok(id)
    }

    /// Record a sample.
    ///
    /// Two destinations, neither of which is the disk:
    ///
    /// * the **live ring** keeps full resolution for the last 30 minutes in memory, for
    ///   the UI's real-time view;
    /// * the **10-second accumulator** collects the mean that will eventually be
    ///   written. Closed buckets move to `pending` and reach SQLite on the next flush.
    ///
    /// The fast tier runs every two seconds; writing at that rate would cost ~96 MB a
    /// day for detail that is only interesting while it is on screen.
    pub fn push(&mut self, ts: i64, m: &MetricId, unit: &str, value: f64) {
        if !value.is_finite() {
            return;
        }
        let Ok(id) = self.metric_id(m, unit) else { return };

        let ring = self.live.entry(id).or_default();
        ring.push_back((ts, value as f32));
        let cutoff = ts - self.live_window_ms;
        while ring.front().map(|(t, _)| *t < cutoff).unwrap_or(false) || ring.len() > LIVE_MAX_POINTS {
            ring.pop_front();
        }

        let bucket = ts / DISK_BUCKET_MS * DISK_BUCKET_MS;
        if bucket != self.agg_bucket {
            self.close_bucket();
            self.agg_bucket = bucket;
        }
        self.agg.entry(id).or_default().add(value);
    }

    /// Move the finished 10-second bucket into the disk queue.
    fn close_bucket(&mut self) {
        let ts = self.agg_bucket;
        for (id, a) in self.agg.drain() {
            let mean = a.mean();
            if mean.is_finite() {
                self.pending.push((id, ts, mean));
            }
        }
        if self.pending.len() >= MAX_PENDING {
            // Disk is wedged or the DB is locked. Drop the oldest rather than growing
            // without bound: a monitoring daemon must not OOM the machine it watches.
            let drop = MAX_PENDING / 4;
            self.pending.drain(0..drop);
            crate::log_warn!("sample buffer full, dropped {drop} oldest aggregates");
        }
    }

    /// Force the in-flight bucket out and write everything. Used at shutdown, where
    /// discarding up to ten seconds of samples would be a silent small data loss.
    pub fn flush_final(&mut self) -> rusqlite::Result<usize> {
        if !self.agg.is_empty() {
            self.close_bucket();
        }
        self.flush()
    }

    /// Full-resolution recent points for a metric, straight from memory.
    pub fn live_series(&mut self, m: &MetricId, since_ms: i64) -> Vec<(i64, f64)> {
        let Some(&id) = self.ids.get(m) else { return Vec::new() };
        self.live
            .get(&id)
            .map(|r| r.iter().filter(|(t, _)| *t >= since_ms).map(|(t, v)| (*t, *v as f64)).collect())
            .unwrap_or_default()
    }

    pub fn live_points(&self) -> usize {
        self.live.values().map(|r| r.len()).sum()
    }

    /// Approximate memory held by the live ring, for self-monitoring.
    pub fn live_bytes(&self) -> usize {
        self.live_points() * std::mem::size_of::<(i64, f32)>()
            + self.live.len() * std::mem::size_of::<VecDeque<(i64, f32)>>()
    }

    pub fn set_live_window_ms(&mut self, ms: i64) {
        self.live_window_ms = ms.clamp(60_000, 2 * 3_600_000);
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn due_for_flush(&self, flush_ms: u64) -> bool {
        !self.pending.is_empty() && now_ms() - self.last_flush_ms >= flush_ms as i64
    }

    /// Write buffered aggregates in a single transaction.
    pub fn flush(&mut self) -> rusqlite::Result<usize> {
        // A bucket that has elapsed but seen no new sample would otherwise sit in the
        // accumulator indefinitely on a quiet machine.
        let now_bucket = now_ms() / DISK_BUCKET_MS * DISK_BUCKET_MS;
        if now_bucket != self.agg_bucket && !self.agg.is_empty() {
            self.close_bucket();
            self.agg_bucket = now_bucket;
        }
        if self.pending.is_empty() {
            self.last_flush_ms = now_ms();
            return Ok(0);
        }
        let n = self.pending.len();
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO sample(metric_id,ts,value) VALUES(?1,?2,?3)")?;
            for (id, ts, v) in self.pending.iter() {
                st.execute(params![id, ts, v])?;
            }
        }
        tx.commit()?;
        self.pending.clear();
        self.last_flush_ms = now_ms();
        Ok(n)
    }

    // ---- events -----------------------------------------------------------

    pub fn insert_event(&self, e: &Event) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO event(ts,subsystem,kind,summary,detail,severity) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                e.ts_ms,
                e.subsystem,
                e.kind,
                e.summary,
                e.detail.as_ref().map(|d| d.to_string()),
                e.severity as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn recent_events(&self, since_ms: Option<i64>, limit: usize, subsystem: Option<&str>) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut sql = String::from("SELECT id,ts,subsystem,kind,summary,detail,severity FROM event WHERE 1=1");
        if since_ms.is_some() {
            sql.push_str(" AND ts >= ?1");
        }
        if subsystem.is_some() {
            sql.push_str(if since_ms.is_some() { " AND subsystem = ?2" } else { " AND subsystem = ?1" });
        }
        sql.push_str(" ORDER BY ts DESC LIMIT ");
        sql.push_str(&limit.min(5000).to_string());
        let mut st = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<serde_json::Value> {
            let detail: Option<String> = r.get(5)?;
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "subsystem": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "summary": r.get::<_, String>(4)?,
                "detail": detail.and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok()),
                "severity": r.get::<_, i64>(6)?,
            }))
        };
        let rows: Vec<serde_json::Value> = match (since_ms, subsystem) {
            (Some(s), Some(sub)) => st.query_map(params![s, sub], map)?.collect::<Result<_, _>>()?,
            (Some(s), None) => st.query_map(params![s], map)?.collect::<Result<_, _>>()?,
            (None, Some(sub)) => st.query_map(params![sub], map)?.collect::<Result<_, _>>()?,
            (None, None) => st.query_map([], map)?.collect::<Result<_, _>>()?,
        };
        Ok(rows)
    }

    // ---- alerts -----------------------------------------------------------

    /// Insert or coalesce. Returns `(row id, is_new)`.
    ///
    /// `is_new` drives notification: a re-fire of an already-open alert bumps `count`
    /// and `last_ts` but must not produce a second desktop notification.
    pub fn upsert_alert(&self, a: &Alert) -> rusqlite::Result<(i64, bool)> {
        let ts = now_ms();
        let existing: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT id, severity FROM alert WHERE fingerprint=?1 AND resolved_ts IS NULL",
                params![a.fingerprint],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let evidence = serde_json::to_string(&a.explanation).unwrap_or_else(|_| "{}".into());
        let suggestion = if a.explanation.actions.is_empty() {
            None
        } else {
            Some(a.explanation.actions.join(" · "))
        };
        match existing {
            Some((id, old_sev)) => {
                // `count` must mean "how many distinct times this fired", not "how
                // many ticks the condition has been true". A rule evaluated every two
                // seconds would otherwise show a failed service as having failed
                // hundreds of times, which is actively misleading. Only a re-fire
                // separated by at least the severity cooldown counts as a new
                // occurrence.
                let last_ts: i64 = self.conn
                    .query_row("SELECT last_ts FROM alert WHERE id=?1", params![id], |r| r.get(0))
                    .unwrap_or(ts);
                let distinct = ts - last_ts >= a.severity.cooldown_s() * 1000;
                self.conn.execute(
                    "UPDATE alert SET last_ts=?1, count=count+?8, severity=?2, title=?3,
                     explanation=?4, evidence=?5, suggestion=?6 WHERE id=?7",
                    params![ts, a.severity as i64, a.title, a.explanation.render(), evidence,
                            suggestion, id, if distinct { 1 } else { 0 }],
                )?;
                // Escalation counts as new so the user is told it got worse.
                Ok((id, (a.severity as i64) > old_sev))
            }
            None => {
                self.conn.execute(
                    "INSERT INTO alert(fingerprint,rule_id,severity,first_ts,last_ts,count,title,explanation,evidence,suggestion)
                     VALUES(?1,?2,?3,?4,?4,1,?5,?6,?7,?8)",
                    params![a.fingerprint, a.rule_id, a.severity as i64, ts, a.title, a.explanation.render(), evidence, suggestion],
                )?;
                Ok((self.conn.last_insert_rowid(), true))
            }
        }
    }

    pub fn resolve_alert(&self, fingerprint: &str) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE alert SET resolved_ts=?1 WHERE fingerprint=?2 AND resolved_ts IS NULL",
            params![now_ms(), fingerprint],
        )?;
        Ok(n > 0)
    }

    pub fn ack_alert(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE alert SET acked_ts=?1 WHERE id=?2", params![now_ms(), id])?;
        Ok(())
    }

    pub fn open_alert_count(&self) -> rusqlite::Result<(i64, i64)> {
        let warn: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM alert WHERE resolved_ts IS NULL AND severity IN (1,2)", [], |r| r.get(0))?;
        let crit: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM alert WHERE resolved_ts IS NULL AND severity >= 3", [], |r| r.get(0))?;
        Ok((warn, crit))
    }

    pub fn alerts(&self, open_only: bool, limit: usize) -> rusqlite::Result<Vec<serde_json::Value>> {
        let sql = format!(
            "SELECT id,fingerprint,rule_id,severity,first_ts,last_ts,resolved_ts,count,title,explanation,evidence,suggestion,acked_ts
             FROM alert {} ORDER BY (resolved_ts IS NULL) DESC, last_ts DESC LIMIT {}",
            if open_only { "WHERE resolved_ts IS NULL" } else { "" },
            limit.min(2000)
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            let ev: String = r.get(10)?;
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "fingerprint": r.get::<_, String>(1)?,
                "rule_id": r.get::<_, String>(2)?,
                "severity": r.get::<_, i64>(3)?,
                "first_ts": r.get::<_, i64>(4)?,
                "last_ts": r.get::<_, i64>(5)?,
                "resolved_ts": r.get::<_, Option<i64>>(6)?,
                "count": r.get::<_, i64>(7)?,
                "title": r.get::<_, String>(8)?,
                "explanation": r.get::<_, String>(9)?,
                "detail": serde_json::from_str::<serde_json::Value>(&ev).unwrap_or(serde_json::Value::Null),
                "suggestion": r.get::<_, Option<String>>(11)?,
                "acked_ts": r.get::<_, Option<i64>>(12)?,
            }))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    // ---- suppressions -----------------------------------------------------

    pub fn add_suppression(&self, scope: &str, pattern: &str, until_ts: Option<i64>, reason: Option<&str>) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO suppression(scope,pattern,until_ts,reason,created_ts) VALUES(?1,?2,?3,?4,?5)",
            params![scope, pattern, until_ts, reason, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn remove_suppression(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM suppression WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn load_suppressions(&self) -> rusqlite::Result<Vec<Suppression>> {
        let mut st = self.conn.prepare("SELECT id,scope,pattern,until_ts,reason FROM suppression")?;
        let rows = st.query_map([], |r| {
            Ok(Suppression {
                id: r.get(0)?,
                scope: r.get(1)?,
                pattern: r.get(2)?,
                until_ts: r.get(3)?,
                reason: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    // ---- baselines --------------------------------------------------------

    pub fn save_baseline(&self, metric_id: i64, context: &str, n: u32, med: f64, mad: f64, p05: f64, p95: f64, sketch: &[u8]) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO baseline(metric_id,context,n,median,mad,p05,p95,updated_ts,sketch)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(metric_id,context) DO UPDATE SET
               n=excluded.n, median=excluded.median, mad=excluded.mad,
               p05=excluded.p05, p95=excluded.p95, updated_ts=excluded.updated_ts,
               sketch=excluded.sketch",
            params![metric_id, context, n, med, mad, p05, p95, now_ms(), sketch],
        )?;
        Ok(())
    }

    pub fn load_baselines(&self) -> rusqlite::Result<Vec<(i64, String, u32, Vec<u8>)>> {
        let mut st = self.conn.prepare("SELECT metric_id,context,n,sketch FROM baseline")?;
        let rows = st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default()))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    // ---- inventory --------------------------------------------------------

    /// Upsert and return the change, if the value differs from what is stored.
    pub fn record_inventory(&self, key: &str, value: &str) -> rusqlite::Result<Option<(Option<String>, String)>> {
        let ts = now_ms();
        let old: Option<String> = self
            .conn
            .query_row("SELECT value FROM inventory WHERE key=?1", params![key], |r| r.get(0))
            .optional()?;
        match &old {
            Some(v) if v == value => {
                self.conn.execute("UPDATE inventory SET last_ts=?1 WHERE key=?2", params![ts, key])?;
                Ok(None)
            }
            _ => {
                self.conn.execute(
                    "INSERT INTO inventory(key,value,first_ts,last_ts) VALUES(?1,?2,?3,?3)
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value, last_ts=excluded.last_ts",
                    params![key, value, ts],
                )?;
                self.conn.execute(
                    "INSERT INTO inventory_change(ts,key,old_value,new_value) VALUES(?1,?2,?3,?4)",
                    params![ts, key, old.clone(), value],
                )?;
                Ok(Some((old, value.to_string())))
            }
        }
    }

    pub fn inventory(&self) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare("SELECT key,value,first_ts FROM inventory ORDER BY key")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    pub fn inventory_changes(&self, limit: usize) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut st = self.conn.prepare(
            &format!("SELECT ts,key,old_value,new_value FROM inventory_change ORDER BY ts DESC LIMIT {}", limit.min(1000)))?;
        let rows = st.query_map([], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_,i64>(0)?, "key": r.get::<_,String>(1)?,
                "old": r.get::<_,Option<String>>(2)?, "new": r.get::<_,String>(3)?,
            }))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    // ---- history ----------------------------------------------------------

    /// Pick the coarsest source that still yields at least `want` points over `span_ms`.
    ///
    /// Charting 24 h from raw 2-second samples would return 43 000 points to draw on a
    /// 600-pixel-wide widget; the rollups exist precisely so that does not happen.
    pub fn choose_bucket(span_ms: i64, want: usize) -> i64 {
        for b in [0i64, 60_000, 300_000, 900_000] {
            // Bucket 0 is the on-disk `sample` table, which now holds 10-second means.
            let step = if b == 0 { DISK_BUCKET_MS } else { b };
            if span_ms / step <= want as i64 * 3 {
                return b;
            }
        }
        900_000
    }

    pub fn history(&self, m: &MetricId, since: i64, until: i64, max_points: usize) -> rusqlite::Result<serde_json::Value> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM metric WHERE subsystem=?1 AND name=?2 AND instance=?3",
                params![m.subsystem, m.name, m.instance],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(serde_json::json!({"points": [], "bucket": 0, "unknown_metric": true}));
        };
        let mut bucket = Self::choose_bucket(until - since, max_points.max(1));
        // The chosen rollup may not exist yet — rollups are built at the glacial tier,
        // so for the first 15 minutes after install every span would chart as empty.
        // Fall back to the next finer source when the coarse one has nothing.
        while bucket > 0 {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM rollup WHERE bucket=?1 AND metric_id=?2 AND ts BETWEEN ?3 AND ?4",
                params![bucket, id, since, until], |r| r.get(0)).unwrap_or(0);
            if n > 0 {
                break;
            }
            bucket = match bucket {
                900_000 => 300_000,
                300_000 => 60_000,
                _ => 0,
            };
        }
        let points: Vec<(i64, f64)> = if bucket == 0 {
            let mut st = self.conn.prepare(
                "SELECT ts,value FROM sample WHERE metric_id=?1 AND ts BETWEEN ?2 AND ?3 ORDER BY ts")?;
            let v = st.query_map(params![id, since, until], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<(i64, f64)>, _>>()?;
            v
        } else {
            let mut st = self.conn.prepare(
                "SELECT ts,avg_v FROM rollup WHERE bucket=?1 AND metric_id=?2 AND ts BETWEEN ?3 AND ?4 ORDER BY ts")?;
            let v = st.query_map(params![bucket, id, since, until], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<(i64, f64)>, _>>()?;
            v
        };
        // Decimate rather than returning everything: a chart cannot use more points
        // than it has pixels, and the socket should not carry them.
        let step = (points.len() / max_points.max(1)).max(1);
        let out: Vec<serde_json::Value> =
            points.iter().step_by(step).map(|(t, v)| serde_json::json!([t, v])).collect();
        Ok(serde_json::json!({"points": out, "bucket": bucket, "metric": m.key()}))
    }

    /// Latest raw values for every metric of a subsystem — used to rebuild UI state.
    pub fn latest(&self, subsystem: &str) -> rusqlite::Result<HashMap<String, f64>> {
        let mut st = self.conn.prepare(
            "SELECT m.name, m.instance, s.value FROM metric m
             JOIN sample s ON s.metric_id = m.id
             WHERE m.subsystem = ?1
               AND s.ts = (SELECT MAX(ts) FROM sample WHERE metric_id = m.id)",
        )?;
        let rows = st.query_map(params![subsystem], |r| {
            let name: String = r.get(0)?;
            let inst: String = r.get(1)?;
            let v: f64 = r.get(2)?;
            Ok((if inst.is_empty() { name } else { format!("{name}[{inst}]") }, v))
        })?;
        let mut m = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            m.insert(k, v);
        }
        Ok(m)
    }

    // ---- maintenance ------------------------------------------------------

    /// Aggregate raw samples into `bucket`-sized rollups for all complete buckets
    /// newer than what already exists.
    pub fn build_rollups(&self, bucket: i64) -> rusqlite::Result<usize> {
        let now = now_ms();
        let last: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(ts),0) FROM rollup WHERE bucket=?1", params![bucket], |r| r.get(0))
            .unwrap_or(0);
        // Only aggregate buckets that have fully elapsed, otherwise the newest bucket
        // would be written from partial data and never corrected.
        let cutoff = (now / bucket) * bucket;
        let from = if last == 0 { 0 } else { last + bucket };
        if from >= cutoff {
            return Ok(0);
        }
        // p95 via a correlated subquery is far too slow over millions of rows; the
        // approximation used here is max-biased toward the 95th by ordering within
        // each group. SQLite has no percentile aggregate without an extension, so the
        // rollup stores an exact min/max/avg and a p95 computed from a bounded sample.
        let n = self.conn.execute(
            "INSERT OR REPLACE INTO rollup(bucket,ts,metric_id,min_v,max_v,avg_v,p95_v,n)
             SELECT ?1, (ts/?1)*?1, metric_id, MIN(value), MAX(value), AVG(value),
                    MIN(value) + (MAX(value)-MIN(value))*0.95, COUNT(*)
             FROM sample
             WHERE ts >= ?2 AND ts < ?3
             GROUP BY metric_id, ts/?1",
            params![bucket, from, cutoff],
        )?;
        Ok(n)
    }

    /// Delete data past its retention window. Chunked so a long-idle machine catching
    /// up cannot stall the event loop inside one enormous DELETE.
    pub fn enforce_retention(&self, r: &Retention) -> rusqlite::Result<usize> {
        let now = now_ms();
        let mut total = 0usize;
        let jobs: [(&str, i64, i64); 6] = [
            ("DELETE FROM sample WHERE ts < ?1 LIMIT 5000", now - r.raw_hours * 3_600_000, 0),
            ("DELETE FROM rollup WHERE bucket=60000 AND ts < ?1 LIMIT 5000", now - r.minute_days * 86_400_000, 0),
            ("DELETE FROM rollup WHERE bucket=300000 AND ts < ?1 LIMIT 5000", now - r.five_min_days * 86_400_000, 0),
            ("DELETE FROM rollup WHERE bucket=900000 AND ts < ?1 LIMIT 5000", now - r.fifteen_min_days * 86_400_000, 0),
            ("DELETE FROM event WHERE ts < ?1 LIMIT 5000", now - r.event_days * 86_400_000, 0),
            ("DELETE FROM alert WHERE resolved_ts IS NOT NULL AND last_ts < ?1 LIMIT 5000", now - r.alert_days * 86_400_000, 0),
        ];
        for (sql, cutoff, _) in jobs {
            // SQLite is built here with SQLITE_ENABLE_UPDATE_DELETE_LIMIT off in some
            // distributions; fall back to an unlimited delete if LIMIT is rejected.
            let n = match self.conn.execute(sql, params![cutoff]) {
                Ok(n) => n,
                Err(_) => {
                    let plain = sql.replace(" LIMIT 5000", "");
                    self.conn.execute(&plain, params![cutoff]).unwrap_or(0)
                }
            };
            total += n;
        }
        if total > 0 {
            let _ = self.conn.pragma_update(None, "incremental_vacuum", 256);
        }
        Ok(total)
    }

    pub fn db_size_bytes(&self) -> i64 {
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(0);
        page_count * page_size
    }

    pub fn row_count(&self, table: &str) -> i64 {
        // `table` is never user input: all call sites pass a literal.
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[derive(Clone, Debug)]
pub struct Suppression {
    pub id: i64,
    pub scope: String,
    pub pattern: String,
    pub until_ts: Option<i64>,
    pub reason: Option<String>,
}

impl Suppression {
    pub fn active(&self, now: i64) -> bool {
        self.until_ts.map(|u| u > now).unwrap_or(true)
    }
    /// Does this suppression cover `rule_id` / `instance` / `fingerprint`?
    pub fn covers(&self, rule_id: &str, instance: &str, fingerprint: &str) -> bool {
        match self.scope.as_str() {
            "rule" => crate::util::glob_match(&self.pattern, rule_id),
            "instance" => !instance.is_empty() && crate::util::glob_match(&self.pattern, instance),
            "fingerprint" => crate::util::glob_match(&self.pattern, fingerprint),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> Store {
        Store::open_memory().unwrap()
    }
    fn m(sub: &'static str, name: &'static str, inst: &str) -> MetricId {
        MetricId::new(sub, name, inst)
    }

    #[test]
    fn schema_is_created_and_versioned() {
        let s = mk();
        let v: String = s.conn.query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "1");
    }

    #[test]
    fn metric_ids_are_interned_and_stable() {
        let mut s = mk();
        let a = s.metric_id(&m("cpu", "usage_pct", ""), "%").unwrap();
        let b = s.metric_id(&m("cpu", "usage_pct", ""), "%").unwrap();
        let c = s.metric_id(&m("cpu", "usage_pct", "core0"), "%").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c, "instance is part of identity");
    }

    #[test]
    fn flush_final_does_not_lose_the_in_flight_bucket() {
        let mut s = mk();
        let id = m("cpu", "usage_pct", "");
        s.push(now_ms(), &id, "%", 42.0);
        s.flush().unwrap();
        assert_eq!(s.row_count("sample"), 0, "the current bucket is not finished yet");
        s.flush_final().unwrap();
        assert_eq!(s.row_count("sample"), 1, "shutdown must not discard it");
    }

    #[test]
    fn full_resolution_stays_in_memory_and_only_10s_means_reach_the_disk() {
        let mut s = mk();
        let id = m("cpu", "usage_pct", "");
        // 100 samples at 2 s = 200 s = 20 ten-second buckets.
        let base = 1_700_000_000_000i64;
        for i in 0..100 {
            s.push(base + i * 2000, &id, "%", i as f64);
        }
        assert_eq!(s.row_count("sample"), 0, "nothing written before flush");
        s.flush().unwrap();
        let rows = s.row_count("sample");
        assert!(
            (19..=20).contains(&rows),
            "expected ~20 ten-second aggregates from 100 two-second samples, got {rows}"
        );
        // The full-resolution data is still available, from memory.
        let live = s.live_series(&id, 0);
        assert_eq!(live.len(), 100, "live ring should hold every sample");
        assert_eq!(live[0].1, 0.0);
        assert_eq!(live[99].1, 99.0);
    }

    #[test]
    fn the_disk_row_is_the_mean_of_its_bucket() {
        let mut s = mk();
        let id = m("power", "system_w", "");
        let base = 1_700_000_000_000i64;
        // Five samples inside one 10-second bucket: 10,11,12,13,14 -> mean 12.
        for (i, v) in [10.0, 11.0, 12.0, 13.0, 14.0].iter().enumerate() {
            s.push(base + i as i64 * 2000, &id, "W", *v);
        }
        s.push(base + 10_000, &id, "W", 99.0); // closes the bucket
        s.flush().unwrap();
        let v: f64 = s
            .conn
            .query_row("SELECT value FROM sample WHERE ts=?1", params![base], |r| r.get(0))
            .unwrap();
        assert!((v - 12.0).abs() < 1e-9, "expected the bucket mean 12.0, got {v}");
    }

    #[test]
    fn the_live_ring_is_bounded_by_time_and_by_count() {
        let mut s = mk();
        let id = m("cpu", "usage_pct", "");
        s.set_live_window_ms(60_000); // one minute
        let base = 1_700_000_000_000i64;
        for i in 0..600 {
            s.push(base + i * 2000, &id, "%", 1.0);
        }
        let live = s.live_series(&id, 0);
        assert!(live.len() <= 31, "a 60 s window at 2 s should hold ~30 points, got {}", live.len());
        assert!(s.live_bytes() < 1_000_000, "live ring should stay small");
    }

    #[test]
    fn non_finite_samples_are_dropped_not_stored() {
        let mut s = mk();
        s.push(1000, &m("thermal", "temp_c", "x"), "C", f64::NAN);
        s.push(1001, &m("thermal", "temp_c", "x"), "C", f64::INFINITY);
        s.push(1002, &m("thermal", "temp_c", "x"), "C", 58.0);
        s.flush().unwrap();
        assert_eq!(s.row_count("sample"), 1);
    }

    #[test]
    fn aggregate_buffer_is_bounded() {
        let mut s = mk();
        // Each distinct metric in each distinct bucket produces one pending row.
        for i in 0..(MAX_PENDING + 5_000) {
            let inst = format!("c{}", i % 200);
            s.push(i as i64 * DISK_BUCKET_MS, &m("cpu", "core_pct", &inst), "%", 1.0);
        }
        assert!(s.pending_len() <= MAX_PENDING, "buffer grew without bound");
    }

    #[test]
    fn only_one_open_alert_per_fingerprint_can_exist() {
        let s = mk();
        let a = Alert::new("disk.full", "/home", Severity::Critical, "Disk full",
                           Explanation::new("/home is 96% full"));
        let (id1, new1) = s.upsert_alert(&a).unwrap();
        let (id2, new2) = s.upsert_alert(&a).unwrap();
        assert!(new1, "first fire is new");
        assert!(!new2, "re-fire must not re-notify");
        assert_eq!(id1, id2);
        assert_eq!(s.row_count("alert"), 1);
    }

    #[test]
    fn count_means_occurrences_not_evaluation_ticks() {
        // A rule is evaluated every couple of seconds. If every evaluation bumped the
        // counter, a service that failed once and stayed failed would be reported as
        // having failed hundreds of times, which is worse than showing nothing.
        let s = mk();
        let a = Alert::new("service.failed", "x.service", Severity::Warning, "failed",
                           Explanation::new("unit is in a failed state"));
        for _ in 0..50 {
            s.upsert_alert(&a).unwrap();
        }
        let count: i64 = s
            .conn
            .query_row("SELECT count FROM alert WHERE fingerprint=?1", params![a.fingerprint], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "50 evaluations of one continuous condition is one occurrence");

        // A genuinely separate occurrence, after the cooldown, does count.
        s.conn
            .execute("UPDATE alert SET last_ts = last_ts - 600000 WHERE fingerprint=?1", params![a.fingerprint])
            .unwrap();
        s.upsert_alert(&a).unwrap();
        let count2: i64 = s
            .conn
            .query_row("SELECT count FROM alert WHERE fingerprint=?1", params![a.fingerprint], |r| r.get(0))
            .unwrap();
        assert_eq!(count2, 2, "a re-fire after the cooldown is a new occurrence");
    }

    #[test]
    fn escalation_is_treated_as_new_so_the_user_is_told() {
        let s = mk();
        let warn = Alert::new("cpu.temp", "", Severity::Warning, "Hot", Explanation::new("91 C"));
        let crit = Alert::new("cpu.temp", "", Severity::Critical, "Very hot", Explanation::new("97 C"));
        assert!(s.upsert_alert(&warn).unwrap().1);
        assert!(s.upsert_alert(&crit).unwrap().1, "escalation must notify");
        assert!(!s.upsert_alert(&crit).unwrap().1, "but not repeatedly");
    }

    #[test]
    fn resolving_lets_the_same_fingerprint_fire_again_later() {
        let s = mk();
        let a = Alert::new("net.iface_down", "wlp108s0", Severity::Warning, "Wi-Fi down",
                           Explanation::new("carrier lost"));
        s.upsert_alert(&a).unwrap();
        assert!(s.resolve_alert("net.iface_down:wlp108s0").unwrap());
        assert!(!s.resolve_alert("net.iface_down:wlp108s0").unwrap(), "already resolved");
        let (_, is_new) = s.upsert_alert(&a).unwrap();
        assert!(is_new, "a fresh occurrence after resolution is new");
        assert_eq!(s.row_count("alert"), 2);
    }

    #[test]
    fn open_alert_counts_drive_the_health_light() {
        let s = mk();
        s.upsert_alert(&Alert::new("a", "", Severity::Notice, "n", Explanation::new("x"))).unwrap();
        s.upsert_alert(&Alert::new("b", "", Severity::Critical, "c", Explanation::new("x"))).unwrap();
        assert_eq!(s.open_alert_count().unwrap(), (1, 1));
        s.resolve_alert("b").unwrap();
        assert_eq!(s.open_alert_count().unwrap(), (1, 0));
    }

    #[test]
    fn inventory_records_only_real_changes() {
        let s = mk();
        assert!(s.record_inventory("kernel", "7.0.0-31").unwrap().is_some(), "first sighting is a change");
        assert!(s.record_inventory("kernel", "7.0.0-31").unwrap().is_none(), "same value is not");
        let ch = s.record_inventory("kernel", "7.0.0-32").unwrap().unwrap();
        assert_eq!(ch.0.as_deref(), Some("7.0.0-31"));
        assert_eq!(ch.1, "7.0.0-32");
        assert_eq!(s.row_count("inventory_change"), 2);
        assert_eq!(s.row_count("inventory"), 1, "inventory holds current state only");
    }

    #[test]
    fn retention_deletes_old_and_keeps_new() {
        let mut s = mk();
        let now = now_ms();
        let id = m("cpu", "usage_pct", "");
        s.push(now - 48 * 3_600_000, &id, "%", 1.0);
        s.push(now - 1_000, &id, "%", 2.0);
        // flush_final also closes the bucket still being accumulated, which is what
        // shutdown does; a plain flush would leave the newest bucket in memory.
        s.flush_final().unwrap();
        assert_eq!(s.row_count("sample"), 2, "two samples in two different buckets");
        let r = Retention { raw_hours: 24, ..Default::default() };
        s.enforce_retention(&r).unwrap();
        assert_eq!(s.row_count("sample"), 1, "48h-old sample should be gone");
    }

    #[test]
    fn rollups_aggregate_complete_buckets_only() {
        let mut s = mk();
        let id = m("power", "system_w", "");
        // Two full minutes, well in the past so both buckets have elapsed.
        let base = ((now_ms() - 3_600_000) / 60_000) * 60_000;
        // Two full minutes of 10-second aggregates.
        for i in 0..12 {
            s.push(base + i * DISK_BUCKET_MS, &id, "W", 10.0 + i as f64);
        }
        s.push(base + 130_000, &id, "W", 0.0); // close the last bucket
        s.flush().unwrap();
        let n = s.build_rollups(60_000).unwrap();
        assert!(n > 0, "expected rollup rows");
        let (mn, mx, avg): (f64, f64, f64) = s.conn.query_row(
            "SELECT min_v,max_v,avg_v FROM rollup WHERE bucket=60000 AND ts=?1", params![base],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(mn, 10.0, "first 10s aggregate in the minute");
        assert_eq!(mx, 15.0, "sixth 10s aggregate in the minute");
        assert!((avg - 12.5).abs() < 0.001);
    }

    #[test]
    fn bucket_selection_scales_with_span() {
        assert_eq!(Store::choose_bucket(5 * 60_000, 200), 0, "5 min -> raw");
        assert_eq!(Store::choose_bucket(24 * 3_600_000, 200), 300_000, "24 h -> 5 min");
        assert_eq!(Store::choose_bucket(90 * 86_400_000, 200), 900_000, "90 d -> 15 min");
    }

    #[test]
    fn history_decimates_to_the_requested_point_count() {
        let mut s = mk();
        let id = m("cpu", "usage_pct", "");
        let base = now_ms() - 3_600_000;
        for i in 0..300 {
            s.push(base + i * DISK_BUCKET_MS, &id, "%", i as f64);
        }
        s.push(base + 300 * DISK_BUCKET_MS, &id, "%", 0.0);
        s.flush().unwrap();
        let h = s.history(&id, base, now_ms(), 50).unwrap();
        let pts = h["points"].as_array().unwrap().len();
        assert!(pts <= 60, "returned {pts} points for a 50-point request");
        assert!(pts > 10);
    }

    #[test]
    fn history_of_an_unknown_metric_is_empty_not_an_error() {
        let s = mk();
        let h = s.history(&m("gpu", "power_w", "card9"), 0, now_ms(), 10).unwrap();
        assert_eq!(h["points"].as_array().unwrap().len(), 0);
        assert_eq!(h["unknown_metric"], true);
    }

    #[test]
    fn suppressions_match_by_scope() {
        let s = mk();
        s.add_suppression("instance", "snap.*.service", None, Some("known noisy")).unwrap();
        s.add_suppression("rule", "net.no_internet", Some(now_ms() + 3_600_000), None).unwrap();
        let sup = s.load_suppressions().unwrap();
        assert_eq!(sup.len(), 2);

        let by_inst = sup.iter().find(|x| x.scope == "instance").unwrap();
        assert!(by_inst.covers("service.failed", "snap.openshell.gateway.service", "service.failed:snap.openshell.gateway.service"));
        assert!(!by_inst.covers("service.failed", "user@1000.service", "service.failed:user@1000.service"));

        let by_rule = sup.iter().find(|x| x.scope == "rule").unwrap();
        assert!(by_rule.covers("net.no_internet", "", "net.no_internet"));
        assert!(by_rule.active(now_ms()));
        assert!(!by_rule.active(now_ms() + 7_200_000), "snooze must expire");
    }

    #[test]
    fn a_permanent_ignore_never_expires() {
        let s = Suppression { id: 1, scope: "rule".into(), pattern: "x".into(), until_ts: None, reason: None };
        assert!(s.active(now_ms()));
        assert!(s.active(now_ms() + 100 * 86_400_000));
    }

    #[test]
    fn events_round_trip_with_detail() {
        let s = mk();
        let e = Event::new("network", "wifi_down", "Wi-Fi disconnected", Severity::Warning)
            .with_detail(serde_json::json!({"iface": "wlp108s0"}));
        s.insert_event(&e).unwrap();
        let back = s.recent_events(None, 10, None).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["kind"], "wifi_down");
        assert_eq!(back[0]["detail"]["iface"], "wlp108s0");
        assert_eq!(s.recent_events(None, 10, Some("storage")).unwrap().len(), 0);
    }
}
