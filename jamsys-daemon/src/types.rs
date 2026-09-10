//! Core value types shared by every layer of the daemon.

use serde::{Deserialize, Serialize};
use std::fmt;

/// How often a collector wants to run. The scheduler collapses all collectors in a
/// tier into a single timer wakeup.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// ~2 s: cheap /proc reads only.
    Fast,
    /// ~10 s: hwmon, battery, wifi signal, GPU.
    Medium,
    /// ~60 s: filesystems, processes, services, reachability.
    Slow,
    /// ~900 s: SMART, inventory, database maintenance.
    Glacial,
    /// Never scheduled; driven by an fd becoming readable.
    Event,
}

impl Tier {
    pub const SCHEDULED: [Tier; 4] = [Tier::Fast, Tier::Medium, Tier::Slow, Tier::Glacial];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Medium => "medium",
            Tier::Slow => "slow",
            Tier::Glacial => "glacial",
            Tier::Event => "event",
        }
    }
}

/// Whether a metric or collector actually works on *this* machine.
///
/// This is deliberately a first-class value rather than an error: "there is no battery"
/// is a normal state for a desktop, not a failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Support {
    /// Every metric this collector advertises is available.
    Full,
    /// Some metrics available; `detail` says which are missing and why.
    Partial { detail: String },
    /// Hardware or interface absent. Not an error.
    Unsupported { reason: String },
    /// Was working, then failed repeatedly. `reason` carries the last error.
    Quarantined { reason: String, failures: u32 },
    /// Disabled by the user in config or over IPC.
    Disabled,
}

impl Support {
    pub fn is_usable(&self) -> bool {
        matches!(self, Support::Full | Support::Partial { .. })
    }
    pub fn label(&self) -> &'static str {
        match self {
            Support::Full => "Full",
            Support::Partial { .. } => "Partial",
            Support::Unsupported { .. } => "Unavailable",
            Support::Quarantined { .. } => "Quarantined",
            Support::Disabled => "Disabled",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info = 0,
    Notice = 1,
    Warning = 2,
    Critical = 3,
}

impl Severity {
    pub fn from_i64(v: i64) -> Severity {
        match v {
            0 => Severity::Info,
            1 => Severity::Notice,
            2 => Severity::Warning,
            _ => Severity::Critical,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Notice => "NOTICE",
            Severity::Warning => "WARNING",
            Severity::Critical => "CRITICAL",
        }
    }
    /// Minimum seconds between repeat notifications for the same fingerprint.
    pub fn cooldown_s(self) -> i64 {
        match self {
            Severity::Info => 300,
            Severity::Notice => 300,
            Severity::Warning => 180,
            Severity::Critical => 60,
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identity of a time series. Interned into the `metric` table once at startup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricId {
    pub subsystem: &'static str,
    pub name: &'static str,
    pub instance: String,
}

impl MetricId {
    pub fn new(subsystem: &'static str, name: &'static str, instance: impl Into<String>) -> Self {
        MetricId { subsystem, name, instance: instance.into() }
    }
    pub fn global(subsystem: &'static str, name: &'static str) -> Self {
        MetricId { subsystem, name, instance: String::new() }
    }
    pub fn key(&self) -> String {
        if self.instance.is_empty() {
            format!("{}.{}", self.subsystem, self.name)
        } else {
            format!("{}.{}[{}]", self.subsystem, self.name, self.instance)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub id: MetricId,
    pub value: f64,
    pub unit: &'static str,
}

/// A discrete thing that happened, as opposed to a judgement about it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub ts_ms: i64,
    pub subsystem: String,
    pub kind: String,
    pub summary: String,
    pub detail: Option<serde_json::Value>,
    pub severity: Severity,
}

impl Event {
    pub fn new(subsystem: &str, kind: &str, summary: impl Into<String>, severity: Severity) -> Self {
        Event {
            ts_ms: crate::clock::now_ms(),
            subsystem: subsystem.to_string(),
            kind: kind.to_string(),
            summary: summary.into(),
            detail: None,
            severity,
        }
    }
    pub fn with_detail(mut self, d: serde_json::Value) -> Self {
        self.detail = Some(d);
        self
    }
}

/// A single supporting measurement shown inside an alert explanation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub label: String,
    pub value: String,
}

impl Evidence {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Evidence { label: label.into(), value: value.into() }
    }
}

/// Why an alert fired. There is no way to construct an `Alert` without one — that is
/// the mechanism that stops the daemon from ever emitting a bare "anomaly detected".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Explanation {
    /// The measurement, with units. e.g. "Battery discharge is 28.4 W".
    pub what: String,
    /// Learned range or the threshold actually in force.
    pub expected: Option<String>,
    /// How long the condition has held.
    pub since_s: i64,
    pub evidence: Vec<Evidence>,
    /// Only populated when a correlation rule with a stated precondition matched.
    pub likely_cause: Option<String>,
    /// One or two safe, read-only diagnostics.
    pub actions: Vec<String>,
}

impl Explanation {
    pub fn new(what: impl Into<String>) -> Self {
        Explanation {
            what: what.into(),
            expected: None,
            since_s: 0,
            evidence: Vec::new(),
            likely_cause: None,
            actions: Vec::new(),
        }
    }
    pub fn expected(mut self, e: impl Into<String>) -> Self {
        self.expected = Some(e.into());
        self
    }
    pub fn since(mut self, s: i64) -> Self {
        self.since_s = s;
        self
    }
    pub fn evidence(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.evidence.push(Evidence::new(label, value));
        self
    }
    pub fn cause(mut self, c: impl Into<String>) -> Self {
        self.likely_cause = Some(c.into());
        self
    }
    pub fn action(mut self, a: impl Into<String>) -> Self {
        self.actions.push(a.into());
        self
    }

    /// Human-readable rendering used for notifications and the CLI.
    pub fn render(&self) -> String {
        let mut s = self.what.clone();
        if let Some(e) = &self.expected {
            s.push_str(&format!("\nExpected: {e}"));
        }
        if self.since_s > 0 {
            s.push_str(&format!("\nOngoing for {}.", crate::util::human_duration(self.since_s)));
        }
        for ev in &self.evidence {
            s.push_str(&format!("\n  · {}: {}", ev.label, ev.value));
        }
        if let Some(c) = &self.likely_cause {
            s.push_str(&format!("\nLikely cause: {c}"));
        }
        if !self.actions.is_empty() {
            s.push_str("\nSuggested checks:");
            for a in &self.actions {
                s.push_str(&format!("\n  → {a}"));
            }
        }
        s
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alert {
    pub rule_id: String,
    /// `rule_id:instance` — stable across restarts, the dedup key.
    pub fingerprint: String,
    pub severity: Severity,
    pub title: String,
    pub explanation: Explanation,
}

impl Alert {
    pub fn new(
        rule_id: &str,
        instance: &str,
        severity: Severity,
        title: impl Into<String>,
        explanation: Explanation,
    ) -> Self {
        let fingerprint = if instance.is_empty() {
            rule_id.to_string()
        } else {
            format!("{rule_id}:{instance}")
        };
        Alert {
            rule_id: rule_id.to_string(),
            fingerprint,
            severity,
            title: title.into(),
            explanation,
        }
    }
}

/// Operating context a sample was taken in. Baselines are partitioned by this, because
/// "normal" on battery-idle and "normal" on AC-active are different distributions.
/// What the machine is doing. Comparing a gaming session against an idle baseline is
/// the single most obvious way to generate nonsense alerts, so this is part of the
/// baseline key rather than something the rules try to reason about afterwards.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    /// Nothing much happening: safe to compare idle power against.
    #[default]
    Idle,
    /// Ordinary desktop use — a browser, an editor, some scrolling.
    Interactive,
    /// A compile, a game, a video export. Its own distribution entirely.
    HighLoad,
}

impl Activity {
    /// Classify from the smoothed CPU average, GPU utilisation and disk throughput.
    pub fn classify(cpu_pct: f64, gpu_pct: f64, disk_bps: f64, idle_cpu_pct: f64) -> Activity {
        if cpu_pct >= 60.0 || gpu_pct >= 50.0 {
            Activity::HighLoad
        } else if cpu_pct < idle_cpu_pct && gpu_pct < 5.0 && disk_bps < 2e6 {
            Activity::Idle
        } else {
            Activity::Interactive
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Idle => "idle",
            Activity::Interactive => "interactive",
            Activity::HighLoad => "load",
        }
    }
    pub fn is_idle(self) -> bool {
        matches!(self, Activity::Idle)
    }
    fn index(self) -> usize {
        match self {
            Activity::Idle => 0,
            Activity::Interactive => 1,
            Activity::HighLoad => 2,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Context {
    pub on_battery: bool,
    pub activity: Activity,
    /// Optional time-of-day bucket, 0..=3 (night, morning, afternoon, evening), or
    /// `None` when time-of-day partitioning is switched off — which is the default,
    /// because it quadruples warm-up time for little gain on a personal laptop.
    pub tod: Option<u8>,
}

impl Context {
    pub fn new(on_battery: bool, activity: Activity) -> Self {
        Context { on_battery, activity, tod: None }
    }

    /// Convenience for the many call sites that only care whether the machine is quiet.
    pub fn simple(on_battery: bool, idle: bool) -> Self {
        Context::new(on_battery, if idle { Activity::Idle } else { Activity::Interactive })
    }

    pub fn idle(&self) -> bool {
        self.activity.is_idle()
    }

    /// Time-of-day bucket from a wall-clock epoch in milliseconds, in local time.
    pub fn bucket_for(ts_ms: i64) -> u8 {
        // Local offset is derived once from libc rather than assuming UTC: a baseline
        // split by "night" is meaningless if night is computed in the wrong timezone.
        let local = ts_ms / 1000 + local_utc_offset_seconds();
        let hour = ((local.rem_euclid(86_400)) / 3600) as u8;
        match hour {
            0..=5 => 0,
            6..=11 => 1,
            12..=17 => 2,
            _ => 3,
        }
    }

    pub fn with_time_of_day(mut self, ts_ms: i64, enabled: bool) -> Self {
        self.tod = enabled.then(|| Self::bucket_for(ts_ms));
        self
    }

    /// Stable key used to partition baselines. Enumerated rather than formatted so it
    /// stays a `&'static str` and costs no allocation on the hot path.
    ///
    /// Six power-and-activity contexts, optionally times four time-of-day buckets.
    pub fn key(&self) -> &'static str {
        const BASE: [&str; 6] = [
            "ac:idle", "ac:interactive", "ac:load",
            "bat:idle", "bat:interactive", "bat:load",
        ];
        const TOD: [[&str; 4]; 6] = [
            ["ac:idle:night", "ac:idle:morning", "ac:idle:afternoon", "ac:idle:evening"],
            ["ac:interactive:night", "ac:interactive:morning", "ac:interactive:afternoon", "ac:interactive:evening"],
            ["ac:load:night", "ac:load:morning", "ac:load:afternoon", "ac:load:evening"],
            ["bat:idle:night", "bat:idle:morning", "bat:idle:afternoon", "bat:idle:evening"],
            ["bat:interactive:night", "bat:interactive:morning", "bat:interactive:afternoon", "bat:interactive:evening"],
            ["bat:load:night", "bat:load:morning", "bat:load:afternoon", "bat:load:evening"],
        ];
        let row = (self.on_battery as usize) * 3 + self.activity.index();
        match self.tod {
            None => BASE[row],
            Some(b) => TOD[row][(b as usize).min(3)],
        }
    }
}

/// Seconds east of UTC for the current local time, from libc's timezone handling.
fn local_utc_offset_seconds() -> i64 {
    // SAFETY: `now` is a valid time_t; `tm` is a zeroed output struct; localtime_r is
    // thread-safe and writes only into `tm`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

impl Default for Context {
    fn default() -> Self {
        Context { on_battery: false, activity: Activity::Idle, tod: None }
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn keys_are_distinct_for_every_combination() {
        let mut seen = std::collections::HashSet::new();
        for bat in [false, true] {
            for act in [Activity::Idle, Activity::Interactive, Activity::HighLoad] {
                for tod in [None, Some(0), Some(1), Some(2), Some(3)] {
                    let c = Context { on_battery: bat, activity: act, tod };
                    assert!(seen.insert(c.key()), "duplicate context key {}", c.key());
                }
            }
        }
        assert_eq!(seen.len(), 30, "6 power/activity contexts x (1 + 4 time buckets)");
    }

    #[test]
    fn disabled_time_of_day_gives_the_plain_six_contexts() {
        assert_eq!(Context::new(true, Activity::Idle).key(), "bat:idle");
        assert_eq!(Context::new(true, Activity::Interactive).key(), "bat:interactive");
        assert_eq!(Context::new(true, Activity::HighLoad).key(), "bat:load");
        assert_eq!(Context::new(false, Activity::Idle).key(), "ac:idle");
        assert_eq!(Context::new(false, Activity::HighLoad).key(), "ac:load");
    }

    #[test]
    fn a_gaming_session_is_never_compared_against_idle() {
        // This is the point of the whole partition: the two must not share a key.
        let gaming = Context::new(false, Activity::HighLoad);
        let idle = Context::new(false, Activity::Idle);
        assert_ne!(gaming.key(), idle.key());
    }

    #[test]
    fn activity_classification_matches_the_intent() {
        let t = 8.0;
        assert_eq!(Activity::classify(2.0, 0.0, 0.0, t), Activity::Idle);
        assert_eq!(Activity::classify(20.0, 0.0, 0.0, t), Activity::Interactive);
        assert_eq!(Activity::classify(75.0, 0.0, 0.0, t), Activity::HighLoad);
        // A GPU workload is high load even when the CPU is nearly free.
        assert_eq!(Activity::classify(5.0, 90.0, 0.0, t), Activity::HighLoad);
        // Heavy disk activity with a quiet CPU is interactive, not idle.
        assert_eq!(Activity::classify(3.0, 0.0, 50e6, t), Activity::Interactive);
        // Just under the idle CPU threshold but with some GPU work.
        assert_eq!(Activity::classify(3.0, 20.0, 0.0, t), Activity::Interactive);
    }

    #[test]
    fn enabling_time_of_day_changes_the_key() {
        let c = Context::new(true, Activity::Idle).with_time_of_day(crate::clock::now_ms(), true);
        assert!(c.tod.is_some());
        assert!(c.key().starts_with("bat:idle:"), "got {}", c.key());
        let off = Context::new(true, Activity::Idle).with_time_of_day(crate::clock::now_ms(), false);
        assert_eq!(off.key(), "bat:idle");
    }

    #[test]
    fn buckets_split_the_day_into_four() {
        let off = local_utc_offset_seconds();
        let at = |hour: i64| ((hour * 3600) - off) * 1000;
        assert_eq!(Context::bucket_for(at(2)), 0, "02:00 is night");
        assert_eq!(Context::bucket_for(at(9)), 1, "09:00 is morning");
        assert_eq!(Context::bucket_for(at(15)), 2, "15:00 is afternoon");
        assert_eq!(Context::bucket_for(at(21)), 3, "21:00 is evening");
    }
}

#[derive(Debug)]
pub enum CollectorError {
    /// Interface vanished (driver reload, device unplugged). Triggers a re-probe.
    Gone(String),
    /// Value present but unusable.
    BadData(String),
    Io(std::io::Error),
    Other(String),
}

impl fmt::Display for CollectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CollectorError::Gone(s) => write!(f, "interface gone: {s}"),
            CollectorError::BadData(s) => write!(f, "bad data: {s}"),
            CollectorError::Io(e) => write!(f, "io: {e}"),
            CollectorError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl From<std::io::Error> for CollectorError {
    fn from(e: std::io::Error) -> Self {
        CollectorError::Io(e)
    }
}

pub type CResult<T> = Result<T, CollectorError>;
