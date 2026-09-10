//! The daemon: scheduler, event loop, IPC request handling.

use super::*;
use std::collections::HashMap;
use jamsys::dbusservice::{DbusService, ServiceAction, WidgetState};
use jamsys::types::Activity;

pub fn register_all(reg: &mut Registry, cfg: &Config) {
    reg.add(Box::new(collectors::cpu::CpuCollector::new()), cfg);
    reg.add(Box::new(collectors::memory::MemCollector::new()), cfg);
    reg.add(Box::new(collectors::network::NetCollector::new()), cfg);
    reg.add(Box::new(collectors::thermal::ThermalCollector::new()), cfg);
    reg.add(Box::new(collectors::power::PowerCollector::new()), cfg);
    reg.add(Box::new(collectors::gpu::GpuCollector::new()), cfg);
    reg.add(Box::new(collectors::storage::StorageCollector::new()), cfg);
    reg.add(Box::new(collectors::services::ServiceCollector::new()), cfg);
    reg.add(Box::new(collectors::devices::DeviceCollector::new()), cfg);
    reg.add(Box::new(collectors::bluetooth::BluetoothCollector::new()), cfg);
    reg.add(Box::new(collectors::process::ProcCollector::new()), cfg);
    reg.add(Box::new(collectors::inventory::InventoryCollector::new()), cfg);
    reg.add(Box::new(collectors::keyboard::KeyboardCollector::new()), cfg);
}

/// Per-tier deadline bookkeeping.
struct Scheduler {
    /// Next monotonic ms at which each tier is due.
    due: HashMap<Tier, i64>,
    multiplier: f64,
}

impl Scheduler {
    fn new(now: i64) -> Self {
        let mut due = HashMap::new();
        for t in Tier::SCHEDULED {
            // Stagger the first run so the daemon does not do all its work in the
            // first millisecond after login, when the desktop is already busy.
            let offset = match t {
                Tier::Fast => 0,
                Tier::Medium => 1_000,
                Tier::Slow => 3_000,
                Tier::Glacial => 20_000,
                Tier::Event => 0,
            };
            due.insert(t, now + offset);
        }
        Scheduler { due, multiplier: 1.0 }
    }

    fn interval(&self, t: Tier, cfg: &Config) -> i64 {
        (cfg.tier_ms(t) as f64 * self.multiplier) as i64
    }

    /// Tiers due at `now`, and the deadline of the soonest remaining one.
    fn take_due(&mut self, now: i64, cfg: &Config) -> (Vec<Tier>, i64) {
        let mut ready = Vec::new();
        for t in Tier::SCHEDULED {
            let d = *self.due.get(&t).unwrap_or(&now);
            if now >= d {
                ready.push(t);
                // Schedule from `now`, not from the missed deadline: after a suspend
                // the daemon must not try to catch up on hours of skipped ticks.
                self.due.insert(t, now + self.interval(t, cfg));
            }
        }
        let next = self.due.values().copied().min().unwrap_or(now + 1000);
        (ready, next)
    }

    /// Push every deadline out to `now + interval`, used after a resume.
    fn rebase(&mut self, now: i64, cfg: &Config) {
        for t in Tier::SCHEDULED {
            self.due.insert(t, now + self.interval(t, cfg) / 4);
        }
    }
}

pub struct Daemon {
    cfg: Arc<Config>,
    store: Store,
    reg: Registry,
    rules: RuleEngine,
    alerts: jamsys::anomaly::alerts::AlertManager,
    ipc: IpcServer,
    el: EventLoop,
    timer: Timer,
    signals: Signals,
    journal: Option<JournalStream>,
    nl_route: Option<NetlinkSocket>,
    nl_uevent: Option<NetlinkSocket>,
    /// Collector names indexed by `TOK_COLLECTOR_BASE + i`, for collectors that own
    /// an event fd and are woken by it rather than by their sampling tier.
    collector_fds: Vec<&'static str>,
    /// The GNOME Shell widget's D-Bus surface. Optional: no session bus is a normal
    /// condition for a daemon started outside a graphical session.
    dbus: Option<DbusService>,
    sched: Scheduler,
    suspend: clock::SuspendWatch,
    dedup: journal::Deduper,
    snap: Snapshot,
    cpu_recent: f64,
    started_ms: i64,
    started_mono: i64,
    /// Counts every epoll return, which is the honest wakeup metric.
    wakeups: u64,
    ticks: u64,
    /// Ticks spent in the reduced sampling regime. Reported separately because a
    /// client attaching to ask about it immediately ends that regime, so the
    /// instantaneous multiplier is unobservable over IPC by construction.
    idle_ticks: u64,
    /// Why the reduced regime is not currently active, for the Coverage page.
    idle_blocked_by: &'static str,
    /// Self-monitoring: external events seen and events discarded, so the Diagnostics
    /// page can show whether the daemon is keeping up.
    events_processed: u64,
    events_dropped: u64,
    widget_state: WidgetState,
    last_maintenance_mono: i64,
}

impl Daemon {
    pub fn new() -> Result<Daemon, String> {
        let cfg_path = Config::default_path();
        let (cfg, cfg_err) = Config::load_or_create(&cfg_path);
        if let Some(e) = cfg_err {
            log_warn!("using defaults, config could not be parsed: {e}");
        }
        let cfg = Arc::new(cfg);

        let data = config::data_dir();
        std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
        let db_path = data.join("history.db");
        let mut store = match Store::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                // A corrupt database must not stop monitoring. Move it aside and
                // start fresh; losing history is far better than losing the daemon.
                log_error!("database unusable ({e}); moving it aside and starting fresh");
                let bak = data.join(format!("history.db.corrupt.{}", clock::now_ms()));
                let _ = std::fs::rename(&db_path, &bak);
                Store::open(&db_path).map_err(|e| format!("cannot open database: {e}"))?
            }
        };

        store.set_live_window_ms(cfg.sampling.live_window_ms as i64);

        let mut reg = Registry::new();
        register_all(&mut reg, &cfg);

        let el = EventLoop::new().map_err(|e| e.to_string())?;
        let timer = Timer::new().map_err(|e| e.to_string())?;
        el.add(timer.fd(), TOK_TIMER).map_err(|e| e.to_string())?;
        let signals = Signals::new(&[libc::SIGTERM, libc::SIGINT, libc::SIGHUP]).map_err(|e| e.to_string())?;
        el.add(signals.fd(), TOK_SIGNAL).map_err(|e| e.to_string())?;

        let ipc = IpcServer::bind(&config::runtime_dir()).map_err(|e| format!("IPC bind: {e}"))?;
        el.add(ipc.fd(), TOK_IPC_LISTEN).map_err(|e| e.to_string())?;

        // Each event source is optional: none of them may prevent startup.
        let journal = match JournalStream::spawn(4) {
            Ok(j) => {
                el.add(j.fd(), TOK_JOURNAL).ok();
                log_info!("journal stream active (priority <= warning)");
                Some(j)
            }
            Err(e) => {
                log_warn!("journal unavailable ({e}); kernel events will not be seen");
                None
            }
        };
        let nl_route = match NetlinkSocket::route() {
            Ok(s) => {
                el.add(s.fd(), TOK_NETLINK_ROUTE).ok();
                Some(s)
            }
            Err(e) => {
                log_warn!("rtnetlink unavailable ({e}); network changes will be polled instead");
                None
            }
        };
        let nl_uevent = match NetlinkSocket::uevent() {
            Ok(s) => {
                el.add(s.fd(), TOK_NETLINK_UEVENT).ok();
                Some(s)
            }
            Err(e) => {
                log_warn!("uevent netlink unavailable ({e}); hotplug will be polled instead");
                None
            }
        };

        // The widget's bus surface. Registered in epoll so it costs nothing when idle.
        let dbus = DbusService::new();
        if let Some(d) = &dbus {
            if el.add(d.fd(), TOK_DBUS).is_err() {
                log_warn!("could not add the bus socket to the event loop");
            }
        }

        // Collectors that can be woken by their own fd rather than waiting for a
        // sampling tier. Bluetooth is the reason this exists: a headset that drops
        // and reconnects inside one sampling interval is invisible to polling.
        let mut collector_fds: Vec<&'static str> = Vec::new();
        for (name, fd) in reg.event_fds() {
            let tok = TOK_COLLECTOR_BASE + collector_fds.len() as u64;
            if el.add(fd, tok).is_ok() {
                collector_fds.push(name);
                log_info!("collector {name} is event-driven on fd {fd}");
            } else {
                log_warn!("could not watch {name}'s event fd; it will poll instead");
            }
        }

        let rules = RuleEngine::new(&cfg);
        let alerts = jamsys::anomaly::alerts::AlertManager::new(&cfg, &store);
        let now_mono = clock::mono_ms();

        log_info!(
            "jamsysd {} started — {} of {} collectors usable, socket {}",
            env!("CARGO_PKG_VERSION"),
            reg.usable_count(),
            reg.len(),
            ipc.path().display()
        );

        Ok(Daemon {
            cfg,
            store,
            reg,
            rules,
            alerts,
            ipc,
            el,
            timer,
            signals,
            journal,
            nl_route,
            nl_uevent,
            collector_fds,
            dbus,
            sched: Scheduler::new(now_mono),
            suspend: clock::SuspendWatch::new(),
            dedup: journal::Deduper::new(300_000),
            snap: Snapshot::default(),
            cpu_recent: 0.0,
            started_ms: clock::now_ms(),
            started_mono: now_mono,
            wakeups: 0,
            ticks: 0,
            idle_ticks: 0,
            idle_blocked_by: "not yet evaluated",
            events_processed: 0,
            events_dropped: 0,
            widget_state: WidgetState::default(),
            last_maintenance_mono: now_mono,
        })
    }

    pub fn run(&mut self) -> Result<(), String> {
        let mut ready = Vec::with_capacity(32);
        let mut running = true;

        // Record the inventory baseline immediately so the first-run report is useful.
        self.record_inventory();

        while running {
            let now_mono = clock::mono_ms();
            let (due, next_deadline) = self.sched.take_due(now_mono, &self.cfg);
            let sleep_ms = (next_deadline - now_mono).clamp(1, 60_000);
            if let Err(e) = self.timer.arm_in(sleep_ms as u64) {
                log_error!("failed to arm timer: {e}");
            }

            if !due.is_empty() {
                self.tick(due, now_mono);
            }

            self.el.wait(&mut ready, -1).map_err(|e| e.to_string())?;
            self.wakeups += 1;

            let events: Vec<Ready> = ready.clone();
            for r in events {
                match r.token {
                    TOK_TIMER => {
                        self.timer.consume();
                    }
                    TOK_SIGNAL => {
                        for s in self.signals.read() {
                            match s {
                                libc::SIGHUP => self.reload(),
                                _ => {
                                    log_info!("signal {s} received, shutting down");
                                    running = false;
                                }
                            }
                        }
                    }
                    TOK_JOURNAL => self.handle_journal(),
                    TOK_NETLINK_ROUTE => self.handle_netlink_route(),
                    TOK_NETLINK_UEVENT => self.handle_netlink_uevent(),
                    TOK_DBUS => self.handle_dbus(),
                    t if t >= TOK_COLLECTOR_BASE && t < TOK_CLIENT_BASE => {
                        self.handle_collector_fd(t)
                    }
                    TOK_IPC_LISTEN => {
                        for (tok, fd) in self.ipc.accept() {
                            if self.el.add(fd, tok).is_err() {
                                self.ipc.drop_client(tok);
                            }
                        }
                    }
                    t if t >= TOK_CLIENT_BASE => self.handle_client(t, r.hangup),
                    _ => {}
                }
            }
        }

        log_info!("flushing {} buffered aggregates", self.store.pending_len());
        // flush_final also closes the 10-second bucket still being accumulated.
        let _ = self.store.flush_final();
        Ok(())
    }

    /// One scheduler tick: run the due tiers, evaluate rules, persist.
    fn tick(&mut self, due: Vec<Tier>, now_mono: i64) {
        self.ticks += 1;

        // Suspend detection first, so collectors see the resume flag on this tick.
        let resumed = self.suspend.poll();
        let mut ctx = Ctx::new(self.cfg.clone());
        ctx.snap = self.snap.clone();

        if let Some(ev) = &resumed {
            let slept = ev.slept_ms / 1000;
            log_info!("resumed after {} of sleep", jamsys::util::human_duration(slept));
            ctx.resumed_ms = Some(ev.slept_ms);
            ctx.event(
                Event::new("power", "resume", format!("Resumed after {} suspended",
                    jamsys::util::human_duration(slept)), Severity::Info)
                    .with_detail(serde_json::json!({"slept_ms": ev.slept_ms})),
            );
            // Deadlines computed before the sleep are all in the past; rebase so the
            // daemon does not fire every tier at once on the first tick after waking.
            self.sched.rebase(now_mono, &self.cfg);
            let e = ExternalEvent::Resumed { slept_ms: ev.slept_ms };
            self.reg.dispatch_event(&e, &mut ctx);
            self.check_suspend_drain(ev.slept_ms, &mut ctx);
        }

        for t in due {
            self.reg.run_tier(t, &mut ctx, now_mono);
        }

        // Idle determination, which drives both the baseline context and the
        // adaptive sampling multiplier.
        self.cpu_recent = 0.8 * self.cpu_recent + 0.2 * ctx.snap.cpu.usage_pct;
        let disk_bps = ctx.snap.storage.total_read_bps + ctx.snap.storage.total_write_bps;
        ctx.snap.activity = Activity::classify(
            self.cpu_recent,
            ctx.snap.gpu.nvidia.util_pct.unwrap_or(0.0),
            disk_bps,
            self.cfg.sampling.idle_cpu_pct,
        );
        ctx.snap.idle = ctx.snap.activity.is_idle();
        ctx.snap.boot_time_ms = self.started_ms;
        ctx.snap.suspended_total_ms = clock::suspended_total_ms();

        let clients = self.ipc.client_count();
        let idle_now = ctx.snap.idle && ctx.snap.power.on_battery && clients == 0;
        self.idle_blocked_by = if idle_now {
            ""
        } else if !ctx.snap.power.on_battery {
            // On AC there is no battery to save, so full-rate sampling is correct.
            "on AC power"
        } else if clients > 0 {
            "a client is attached"
        } else if self.cpu_recent >= self.cfg.sampling.idle_cpu_pct {
            "the machine is busy"
        } else {
            "GPU or disk activity"
        };
        if idle_now {
            self.idle_ticks += 1;
        }
        self.sched.multiplier = if idle_now { self.cfg.sampling.idle_multiplier.max(1.0) } else { 1.0 };

        // Persist samples and feed the baselines.
        for s in &ctx.samples {
            self.store.push(ctx.ts_ms, &s.id, s.unit, s.value);
        }
        self.rules.learn_with(&ctx.snap, &ctx.samples, self.cfg.baseline.time_of_day);

        // Events: store and push.
        let events = std::mem::take(&mut ctx.events);
        self.events_processed += events.len() as u64;
        for e in &events {
            if self.store.insert_event(e).is_err() {
                self.events_dropped += 1;
            }
            let payload = serde_json::json!({
                "ts": e.ts_ms, "subsystem": e.subsystem, "kind": e.kind,
                "summary": e.summary, "severity": e.severity as i64, "detail": e.detail
            });
            for tok in self.ipc.broadcast("event", &payload) {
                self.drop_client(tok);
            }
        }

        // Rules.
        let ev = self.rules.evaluate(&ctx.snap, &self.cfg, now_mono);
        let firing: std::collections::HashSet<String> = ev.firing.iter().cloned().collect();
        for a in ev.alerts {
            // `raise` returns true only when this is genuinely new, an escalation, or
            // past the cooldown. A rule that keeps evaluating true must not push a
            // frame every couple of seconds — the UI would raise a toast each time for
            // a condition the user already knows about.
            let notified = self.alerts.raise(a.clone(), &self.store, &self.cfg);
            if !notified {
                continue;
            }
            let payload = serde_json::json!({
                "rule_id": a.rule_id, "fingerprint": a.fingerprint,
                "severity": a.severity as i64, "title": a.title,
                "explanation": a.explanation, "rendered": a.explanation.render(),
                "notified": notified,
            });
            for tok in self.ipc.broadcast("alert", &payload) {
                self.drop_client(tok);
            }
        }
        // Anything no longer firing may resolve (subject to hysteresis).
        let open: Vec<String> = self.open_fingerprints();
        for fp in open {
            if !firing.contains(&fp) {
                self.alerts.clear(&fp, &self.store);
            }
        }

        self.snap = ctx.snap;

        if self.store.due_for_flush(self.cfg.sampling.flush_ms) {
            match self.store.flush() {
                Ok(n) if n > 0 => jamsys::log_debug!("flushed {n} samples"),
                Err(e) => log_error!("sample flush failed: {e}"),
                _ => {}
            }
        }

        // Maintenance at the glacial cadence: rollups, retention, GC.
        if now_mono - self.last_maintenance_mono >= self.cfg.sampling.glacial_ms as i64 {
            self.last_maintenance_mono = now_mono;
            self.maintenance(now_mono);
        }

        for tok in self.ipc.broadcast("tick", &serde_json::json!({"ts": ctx.ts_ms})) {
            self.drop_client(tok);
        }

        self.publish_widget_state();
    }

    /// Rebuild the widget state and push it to the Shell extension, but only when a
    /// user would actually see a difference. The diffing lives in `DbusService`.
    fn publish_widget_state(&mut self) {
        let (warn, crit) = self.store.open_alert_count().unwrap_or((0, 0));
        let health = if crit > 0 { "critical" } else if warn > 0 { "attention" } else { "healthy" };
        let mut w = WidgetState::from_snapshot(&self.snap, health, (warn + crit) as u32);
        // Attach the single most severe open alert, which is all the widget shows.
        if let Ok(rows) = self.store.alerts(true, 50) {
            if let Some(top) = rows
                .iter()
                .max_by_key(|a| (a["severity"].as_i64().unwrap_or(0), a["last_ts"].as_i64().unwrap_or(0)))
            {
                let detail = top["detail"]["what"].as_str().unwrap_or("").to_string();
                let expected = top["detail"]["expected"].as_str().unwrap_or("").to_string();
                let rule = top["rule_id"].as_str().unwrap_or("");
                w = w.with_alert(
                    top["title"].as_str().unwrap_or(""),
                    &detail,
                    &expected,
                    subsystem_for_rule(rule),
                    Severity::from_i64(top["severity"].as_i64().unwrap_or(0)),
                );
            }
        }
        self.widget_state = w;
        if let Some(d) = self.dbus.as_mut() {
            d.publish(&self.widget_state);
        }
    }

    fn handle_dbus(&mut self) {
        let state = self.widget_state.clone();
        let actions = match self.dbus.as_mut() {
            Some(d) => d.handle(&state),
            None => return,
        };
        for a in actions {
            match a {
                ServiceAction::AckTopAlert => {
                    if let Ok(rows) = self.store.alerts(true, 50) {
                        if let Some(top) = rows.iter().max_by_key(|a| a["severity"].as_i64().unwrap_or(0)) {
                            if let Some(id) = top["id"].as_i64() {
                                let _ = self.store.ack_alert(id);
                            }
                        }
                    }
                }
                ServiceAction::None => {}
            }
        }
    }

    /// Open alerts, from memory. This used to issue a SQL query on every tick, which
    /// is a database round-trip every two seconds for information the alert manager
    /// already holds.
    fn open_fingerprints(&self) -> Vec<String> {
        self.alerts.open_fingerprints()
    }

    fn maintenance(&mut self, now_mono: i64) {
        let _ = self.store.flush();
        for b in [60_000i64, 300_000, 900_000] {
            if let Err(e) = self.store.build_rollups(b) {
                log_warn!("rollup {b} failed: {e}");
            }
        }
        match self.store.enforce_retention(&self.cfg.retention) {
            Ok(n) if n > 0 => log_info!("retention removed {n} rows"),
            Err(e) => log_warn!("retention failed: {e}"),
            _ => {}
        }
        self.dedup.gc(clock::now_ms());
        self.rules.dwell.gc(now_mono);
        self.record_inventory();

        // Respawn the journal follower if journald restarted under us.
        if let Some(j) = self.journal.as_mut() {
            if !j.is_alive() {
                log_warn!("journal follower exited; respawning");
                let old_fd = j.fd();
                let _ = self.el.remove(old_fd);
                self.journal = None;
                if let Ok(nj) = JournalStream::spawn(4) {
                    self.el.add(nj.fd(), TOK_JOURNAL).ok();
                    self.journal = Some(nj);
                }
            }
        }
    }

    fn record_inventory(&mut self) {
        for (k, v) in collectors::inventory::gather() {
            match self.store.record_inventory(&k, &v) {
                Ok(Some((Some(old), new))) => {
                    log_info!("inventory change: {k}: {old} -> {new}");
                }
                Err(e) => log_warn!("inventory write failed: {e}"),
                _ => {}
            }
        }
    }

    /// Battery consumed while asleep. A laptop that loses 15 % overnight has a real
    /// problem (usually s2idle instead of deep sleep) and this is the only place it
    /// can be observed.
    fn check_suspend_drain(&mut self, slept_ms: i64, ctx: &mut Ctx) {
        let hours = slept_ms as f64 / 3_600_000.0;
        if hours < 0.25 || !self.snap.power.has_battery {
            return;
        }
        let before = self.snap.power.percent;
        let after = ctx.snap.power.percent;
        if before <= 0.0 || after <= 0.0 || after > before {
            return;
        }
        let rate = (before - after) / hours;
        let limit = self.cfg.threshold("suspend.drain", "pct_per_hour", 2.0);
        ctx.sample("power", "suspend_drain_pct_h", "", "%/h", rate);
        if rate > limit {
            let a = Alert::new("suspend.drain", "", Severity::Notice,
                "The battery drained quickly while the laptop was asleep",
                Explanation::new(format!(
                    "The battery fell from {before:.0}% to {after:.0}% over {} of sleep — about {rate:.1}% per hour.",
                    jamsys::util::human_duration(slept_ms / 1000)))
                    .expected(format!("below {limit:.1}% per hour"))
                    .evidence("Sleep duration", jamsys::util::human_duration(slept_ms / 1000))
                    .cause("Most laptops that drain in sleep are using s2idle rather than deep suspend, or a USB device is keeping the system partly awake")
                    .action("Check which sleep state is in use:  cat /sys/power/mem_sleep")
                    .action("Look for wakeup sources:  cat /proc/acpi/wakeup"));
            self.alerts.raise(a, &self.store, &self.cfg);
        }
    }

    fn handle_journal(&mut self) {
        let entries = match self.journal.as_mut() {
            Some(j) => j.read_entries(),
            None => return,
        };
        if entries.is_empty() {
            return;
        }
        let mut ctx = Ctx::new(self.cfg.clone());
        ctx.snap = self.snap.clone();
        let now = clock::now_ms();

        for e in entries {
            // User-configured ignores come first: they are the escape hatch for
            // hardware that logs harmless noise forever.
            if self.cfg.journal_ignore.iter().any(|g| jamsys::util::glob_match(g, &e.message)) {
                continue;
            }
            // Collectors see the real entry. They are given it before classification,
            // because a line the daemon's own rules do not recognise can still be the
            // one a collector was waiting for -- bluetoothd's "Host is down" is not a
            // classified kind, but it is the only place the reason for a failed
            // reconnect ever appears.
            self.reg.dispatch_event(&ExternalEvent::Journal(e.clone()), &mut ctx);
            let Some(c) = journal::classify(&e.message) else {
                // Unclassified warnings are still worth recording as events, but they
                // never become alerts. This is what keeps the journal from being a
                // firehose of nuisance notifications.
                if e.priority <= 3 {
                    let _ = self.store.insert_event(&Event::new(
                        "journal", "kernel_error",
                        e.message.chars().take(300).collect::<String>(), Severity::Notice));
                }
                continue;
            };

            let (severity, kind) = if c.kind == "gpu_xid" {
                let (code, sev) = journal::classify_xid(&e.message);
                (sev, code.map(|x| format!("gpu_xid_{x}")).unwrap_or_else(|| "gpu_xid".into()))
            } else {
                (c.severity, c.kind.to_string())
            };

            // Collapse identical repeats.
            let Some(suppressed) = self.dedup.admit(&format!("{kind}:{}", e.message), now) else {
                continue;
            };
            let extra = if suppressed > 0 { format!(" (plus {suppressed} identical since the last report)") } else { String::new() };

            let mut summary = e.message.chars().take(280).collect::<String>();
            summary.push_str(&extra);
            let event = Event::new("journal", &kind, summary.clone(), severity)
                .with_detail(serde_json::json!({
                    "unit": e.unit, "priority": e.priority, "kernel": e.is_kernel,
                    "suppressed": suppressed
                }));
            let _ = self.store.insert_event(&event);
            let payload = serde_json::json!({
                "ts": event.ts_ms, "subsystem": "journal", "kind": kind,
                "summary": summary, "severity": severity as i64
            });
            for tok in self.ipc.broadcast("event", &payload) {
                self.drop_client(tok);
            }

            if c.alertable && severity >= Severity::Warning {
                let a = Alert::new(
                    &format!("journal.{kind}"),
                    "",
                    severity,
                    journal_title(&kind),
                    Explanation::new(format!("The kernel logged: {}", e.message.chars().take(240).collect::<String>()))
                        .expected("no messages of this kind")
                        .evidence("Source", if e.is_kernel { "kernel".to_string() } else { e.syslog_id.clone() })
                        .evidence("Priority", e.priority.to_string())
                        .evidence("Repeats suppressed", suppressed.to_string())
                        .action(journal_action(&kind)),
                );
                let notified = self.alerts.raise(a.clone(), &self.store, &self.cfg);
                if notified {
                    let p = serde_json::json!({
                        "rule_id": a.rule_id, "fingerprint": a.fingerprint,
                        "severity": a.severity as i64, "title": a.title,
                        "rendered": a.explanation.render(), "notified": notified
                    });
                    for tok in self.ipc.broadcast("alert", &p) {
                        self.drop_client(tok);
                    }
                }
            }
        }
        for e in ctx.events.drain(..) {
            let _ = self.store.insert_event(&e);
        }
    }

    /// A collector's own fd became readable. The collector drains it and refreshes
    /// its own state; the daemon does not interpret the bytes.
    fn handle_collector_fd(&mut self, tok: u64) {
        let idx = (tok - TOK_COLLECTOR_BASE) as usize;
        let Some(name) = self.collector_fds.get(idx).copied() else { return };
        let mut ctx = Ctx::new(self.cfg.clone());
        ctx.snap = self.snap.clone();
        self.reg
            .dispatch_event(&ExternalEvent::CollectorReadable { name }, &mut ctx);
        self.flush_ctx_events(&mut ctx);
        self.snap = ctx.snap;
    }

    fn handle_netlink_route(&mut self) {
        let Some(s) = &self.nl_route else { return };
        let mut any = false;
        for d in s.drain() {
            if !netlink::parse_route_events(&d).is_empty() {
                any = true;
            }
        }
        if !any {
            return;
        }
        let mut ctx = Ctx::new(self.cfg.clone());
        ctx.snap = self.snap.clone();
        self.reg.dispatch_event(&ExternalEvent::NetlinkLink, &mut ctx);
        // A link change makes the network state stale immediately; re-run its tier now
        // rather than waiting up to two seconds, so the event timeline is accurate.
        self.reg.run_tier(Tier::Fast, &mut ctx, clock::mono_ms());
        self.flush_ctx_events(&mut ctx);
        self.snap = ctx.snap;
    }

    fn handle_netlink_uevent(&mut self) {
        let Some(s) = &self.nl_uevent else { return };
        let msgs = s.drain();
        let mut ctx = Ctx::new(self.cfg.clone());
        ctx.snap = self.snap.clone();
        for m in msgs {
            if let Some(u) = netlink::parse_uevent(&m) {
                self.reg.dispatch_event(
                    &ExternalEvent::Uevent {
                        action: u.action.clone(),
                        subsystem: u.subsystem.clone(),
                        devpath: u.devpath.clone(),
                    },
                    &mut ctx,
                );
            }
        }
        self.flush_ctx_events(&mut ctx);
    }

    fn flush_ctx_events(&mut self, ctx: &mut Ctx) {
        for e in ctx.events.drain(..) {
            let _ = self.store.insert_event(&e);
            let payload = serde_json::json!({
                "ts": e.ts_ms, "subsystem": e.subsystem, "kind": e.kind,
                "summary": e.summary, "severity": e.severity as i64, "detail": e.detail
            });
            for tok in self.ipc.broadcast("event", &payload) {
                self.drop_client(tok);
            }
        }
    }

    fn drop_client(&mut self, tok: u64) {
        if let Some(fd) = self.ipc.drop_client(tok) {
            let _ = self.el.remove(fd);
        }
    }

    fn reload(&mut self) {
        let (cfg, err) = Config::load_or_create(&Config::default_path());
        match err {
            Some(e) => log_warn!("reload failed, keeping current config: {e}"),
            None => {
                log_info!("configuration reloaded");
                self.cfg = Arc::new(cfg);
                self.alerts.reload_suppressions(&self.store);
            }
        }
    }

    fn handle_client(&mut self, tok: u64, hangup: bool) {
        if hangup {
            self.drop_client(tok);
            return;
        }
        let reqs = match self.ipc.client(tok) {
            Some(c) => match c.read_requests() {
                Ok(r) => r,
                Err(_) => {
                    self.drop_client(tok);
                    return;
                }
            },
            None => return,
        };
        for r in reqs {
            let resp = self.dispatch(tok, &r);
            let failed = match self.ipc.client(tok) {
                Some(c) => c.send(&resp).is_err(),
                None => true,
            };
            if failed {
                self.drop_client(tok);
                return;
            }
        }
    }

    fn dispatch(&mut self, tok: u64, r: &jamsys::ipc::Request) -> Response {
        let p = &r.params;
        let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let i = |k: &str| p.get(k).and_then(|v| v.as_i64());
        let u = |k: &str, d: usize| p.get(k).and_then(|v| v.as_u64()).unwrap_or(d as u64) as usize;

        match r.op.as_str() {
            "ping" => Response::ok(r.id, serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "schema": 1,
                "pid": std::process::id(),
                "uptime_s": (clock::mono_ms() - self.started_mono) / 1000,
            })),
            "snapshot" => {
                let (warn, crit) = self.store.open_alert_count().unwrap_or((0, 0));
                let health = if crit > 0 { "critical" } else if warn > 0 { "attention" } else { "healthy" };
                Response::ok(r.id, serde_json::json!({
                    "health": health,
                    "open_warnings": warn,
                    "open_critical": crit,
                    "snapshot": self.snap,
                    "collectors_usable": self.reg.usable_count(),
                    "collectors_total": self.reg.len(),
                    "helper": collectors::privileged::available(),
                    "ts": clock::now_ms(),
                }))
            }
            "history" => {
                let sub = s("subsystem");
                let name = s("name");
                if sub.is_empty() || name.is_empty() {
                    return Response::err(r.id, "bad_request", "subsystem and name are required");
                }
                let until = i("until_ms").unwrap_or_else(clock::now_ms);
                let since = i("since_ms").unwrap_or(until - 3_600_000);
                let id = MetricId::new(
                    Box::leak(sub.into_boxed_str()),
                    Box::leak(name.into_boxed_str()),
                    s("instance"),
                );
                match self.store.history(&id, since, until, u("max_points", 300)) {
                    Ok(v) => Response::ok(r.id, v),
                    Err(e) => Response::err(r.id, "query_failed", e.to_string()),
                }
            }
            "alerts" => {
                let open = p.get("open_only").and_then(|v| v.as_bool()).unwrap_or(false);
                match self.store.alerts(open, u("limit", 100)) {
                    Ok(v) => Response::ok(r.id, serde_json::json!({"alerts": v})),
                    Err(e) => Response::err(r.id, "query_failed", e.to_string()),
                }
            }
            "events" => {
                let sub = s("subsystem");
                match self.store.recent_events(i("since_ms"), u("limit", 200),
                                               if sub.is_empty() { None } else { Some(&sub) }) {
                    Ok(v) => Response::ok(r.id, serde_json::json!({"events": v})),
                    Err(e) => Response::err(r.id, "query_failed", e.to_string()),
                }
            }
            "processes" => Response::ok(r.id, serde_json::json!({
                "top_cpu": self.snap.process.top_cpu,
                "top_mem": self.snap.process.top_mem,
                "total": self.snap.process.total,
            })),
            "coverage" => Response::ok(r.id, serde_json::json!({
                "collectors": self.reg.coverage(),
                "helper_installed": collectors::privileged::available(),
                "journal": self.journal.is_some(),
                "netlink_route": self.nl_route.is_some(),
                "netlink_uevent": self.nl_uevent.is_some(),
            })),
            "inventory" => {
                if p.get("changes").and_then(|v| v.as_bool()).unwrap_or(false) {
                    match self.store.inventory_changes(u("limit", 200)) {
                        Ok(v) => Response::ok(r.id, serde_json::json!({"changes": v})),
                        Err(e) => Response::err(r.id, "query_failed", e.to_string()),
                    }
                } else {
                    match self.store.inventory() {
                        Ok(v) => Response::ok(r.id, serde_json::json!({
                            "items": v.iter().map(|(k, val, ts)| serde_json::json!({"key": k, "value": val, "first_seen": ts})).collect::<Vec<_>>()
                        })),
                        Err(e) => Response::err(r.id, "query_failed", e.to_string()),
                    }
                }
            }
            "ack" => match i("alert_id") {
                Some(id) => {
                    let _ = self.store.ack_alert(id);
                    Response::ok(r.id, serde_json::json!({"acked": id}))
                }
                None => Response::err(r.id, "bad_request", "alert_id is required"),
            },
            "mute" => {
                let fp = s("fingerprint");
                if fp.is_empty() {
                    return Response::err(r.id, "bad_request", "fingerprint is required");
                }
                match self.store.add_suppression("fingerprint", &fp, i("until_ms"), Some("muted from the UI")) {
                    Ok(id) => {
                        self.alerts.reload_suppressions(&self.store);
                        Response::ok(r.id, serde_json::json!({"suppression_id": id}))
                    }
                    Err(e) => Response::err(r.id, "write_failed", e.to_string()),
                }
            }
            "suppress" => {
                let scope = s("scope");
                let pattern = s("pattern");
                if !["rule", "instance", "fingerprint"].contains(&scope.as_str()) || pattern.is_empty() {
                    return Response::err(r.id, "bad_request", "scope must be rule|instance|fingerprint and pattern must be set");
                }
                let reason = s("reason");
                match self.store.add_suppression(&scope, &pattern, i("until_ms"),
                                                 if reason.is_empty() { None } else { Some(&reason) }) {
                    Ok(id) => {
                        self.alerts.reload_suppressions(&self.store);
                        Response::ok(r.id, serde_json::json!({"suppression_id": id}))
                    }
                    Err(e) => Response::err(r.id, "write_failed", e.to_string()),
                }
            }
            "unsuppress" => match i("id") {
                Some(id) => {
                    let _ = self.store.remove_suppression(id);
                    self.alerts.reload_suppressions(&self.store);
                    Response::ok(r.id, serde_json::json!({"removed": id}))
                }
                None => Response::err(r.id, "bad_request", "id is required"),
            },
            "suppressions" => match self.store.load_suppressions() {
                Ok(v) => Response::ok(r.id, serde_json::json!({
                    "suppressions": v.iter().map(|x| serde_json::json!({
                        "id": x.id, "scope": x.scope, "pattern": x.pattern,
                        "until_ts": x.until_ts, "reason": x.reason
                    })).collect::<Vec<_>>()
                })),
                Err(e) => Response::err(r.id, "query_failed", e.to_string()),
            },
            "set_threshold" => {
                let rule = s("rule_id");
                let key = s("key");
                let Some(val) = p.get("value").and_then(|v| v.as_f64()) else {
                    return Response::err(r.id, "bad_request", "value must be a number");
                };
                if rule.is_empty() || key.is_empty() || !val.is_finite() {
                    return Response::err(r.id, "bad_request", "rule_id, key and a finite value are required");
                }
                // Written to config so it survives a restart, and applied immediately.
                let mut cfg = (*self.cfg).clone();
                cfg.alerts.thresholds.insert(format!("{rule}.{key}"), val);
                let path = Config::default_path();
                if let Err(e) = std::fs::write(&path, cfg.to_toml()) {
                    return Response::err(r.id, "write_failed", e.to_string());
                }
                self.cfg = Arc::new(cfg);
                Response::ok(r.id, serde_json::json!({"rule_id": rule, "key": key, "value": val}))
            }
            "set_collector" => {
                let name = s("name");
                let Some(en) = p.get("enabled").and_then(|v| v.as_bool()) else {
                    return Response::err(r.id, "bad_request", "enabled must be a boolean");
                };
                if !self.reg.set_enabled(&name, en) {
                    return Response::err(r.id, "not_found", format!("no collector named {name}"));
                }
                let mut cfg = (*self.cfg).clone();
                cfg.collectors.insert(name.clone(), en);
                let _ = std::fs::write(Config::default_path(), cfg.to_toml());
                self.cfg = Arc::new(cfg);
                Response::ok(r.id, serde_json::json!({"name": name, "enabled": en}))
            }
            "subscribe" => {
                let topics: Vec<String> = p
                    .get("topics")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_else(|| vec!["alert".into(), "event".into()]);
                if let Some(c) = self.ipc.client(tok) {
                    c.subscriptions = topics.clone();
                }
                Response::ok(r.id, serde_json::json!({"subscribed": topics}))
            }
            "stats" => {
                let (rss, threads) = self_rss_threads();
                let uptime = (clock::mono_ms() - self.started_mono).max(1);
                Response::ok(r.id, serde_json::json!({
                    "rss_bytes": rss,
                    "threads": threads,
                    "cpu_seconds": self_cpu_seconds(),
                    "uptime_s": uptime / 1000,
                    "wakeups": self.wakeups,
                    "events_processed": self.events_processed,
                    "events_dropped": self.events_dropped,
                    "live_points": self.store.live_points(),
                    "live_bytes": self.store.live_bytes(),
                    "activity": self.snap.activity.as_str(),
                    "context": self.snap.context_at(clock::now_ms(), self.cfg.baseline.time_of_day).key(),
                    "dbus_widget": self.dbus.is_some(),
                    "dbus_signals": self.dbus.as_ref().map(|d| d.signals_emitted).unwrap_or(0),
                    "dbus_calls": self.dbus.as_ref().map(|d| d.calls_served).unwrap_or(0),
                    "wakeups_per_s": self.wakeups as f64 / (uptime as f64 / 1000.0),
                    "ticks": self.ticks,
                    "db_bytes": self.store.db_size_bytes(),
                    "samples_pending": self.store.pending_len(),
                    "sample_rows": self.store.row_count("sample"),
                    "rollup_rows": self.store.row_count("rollup"),
                    "event_rows": self.store.row_count("event"),
                    "baselines": self.rules.baselines.len(),
                    "open_alerts": self.alerts.open_count(),
                    "suppressed_notifications": self.alerts.suppressed_counts(),
                    "notify_failures": self.alerts.notify_failures,
                    "ipc_clients": self.ipc.client_count(),
                    "sampling_multiplier": self.sched.multiplier,
                    "idle_ticks": self.idle_ticks,
                    "idle_tick_fraction": if self.ticks > 0 { self.idle_ticks as f64 / self.ticks as f64 } else { 0.0 },
                    "idle_blocked_by": self.idle_blocked_by,
                    "collector_timings": self.reg.timings().iter()
                        .map(|(k, (runs, us))| serde_json::json!({"name": k, "runs": runs, "total_us": us,
                             "avg_us": if *runs > 0 { us / runs } else { 0 }}))
                        .collect::<Vec<_>>(),
                }))
            }
            other => Response::err(r.id, "unknown_op", format!("unsupported operation: {other}")),
        }
    }
}

/// Map a rule id onto the UI page the widget should open when clicked.
fn subsystem_for_rule(rule: &str) -> &'static str {
    match rule.split('.').next().unwrap_or("") {
        "cpu" => "CPU",
        "mem" => "Memory",
        "gpu" => "GPU",
        "battery" | "power" | "suspend" => "Power",
        "thermal" | "nvme" if rule.starts_with("thermal") => "Thermals",
        "disk" | "nvme" => "Storage",
        "net" => "Network",
        "service" | "audio" => "Services",
        "device" | "bt" => "Devices",
        "journal" => "Events",
        _ => "Overview",
    }
}

fn journal_title(kind: &str) -> String {
    match kind {
        k if k.starts_with("gpu_xid") => "The graphics driver reported an error".into(),
        "gpu_off_bus" => "The graphics card has stopped responding".into(),
        "oom" => "The kernel killed a process to free memory".into(),
        "nvme_timeout" | "nvme_fail" => "The SSD is not responding".into(),
        "fs_error" | "fs_readonly" => "The filesystem reported an error".into(),
        "io_error" | "media_error" => "A storage read or write failed".into(),
        "thermal_critical" => "A component reached its critical temperature".into(),
        "mce" | "hw_error" => "The CPU reported a hardware error".into(),
        "pcie_error" => "A PCIe device reported an error".into(),
        "wifi_fw_error" => "The Wi-Fi driver reported a firmware error".into(),
        "suspend_failed" => "The system failed to suspend properly".into(),
        "resume_device_error" => "A device failed to resume from sleep".into(),
        "bt_hw_error" => "The Bluetooth controller reported a hardware error".into(),
        "audio_error" => "The audio driver reported an error".into(),
        "usb_error" => "A USB device is not being recognised".into(),
        _ => "The kernel logged an error".into(),
    }
}

fn journal_action(kind: &str) -> String {
    match kind {
        k if k.starts_with("gpu_xid") => "Xid errors usually come from the application that was running. If they repeat without one, check the driver version on the Coverage page.".into(),
        "oom" => "See the Memory page for what was using memory at the time.".into(),
        "nvme_timeout" | "nvme_fail" | "io_error" | "media_error" => "Back up your data, then check SMART on the Storage page.".into(),
        "fs_error" | "fs_readonly" => "Run a filesystem check from a live USB; do not fsck a mounted filesystem.".into(),
        "mce" | "hw_error" => "Run:  journalctl -k -b | grep -i 'machine check'".into(),
        "suspend_failed" | "resume_device_error" => "Run:  journalctl -b -0 | grep -i 'PM:'".into(),
        _ => "See the Events page for the full message and its context.".into(),
    }
}

/// The daemon's own RSS and thread count, for the self-audit.
fn self_rss_threads() -> (u64, u64) {
    let mut rss = 0;
    let mut threads = 0;
    if let Ok(t) = std::fs::read_to_string("/proc/self/status") {
        for line in t.lines() {
            if let Some(v) = line.strip_prefix("VmRSS:") {
                rss = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
            } else if let Some(v) = line.strip_prefix("Threads:") {
                threads = v.trim().parse().unwrap_or(0);
            }
        }
    }
    (rss, threads)
}

fn self_cpu_seconds() -> f64 {
    // SAFETY: sysconf(_SC_CLK_TCK) returns a positive constant on Linux.
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let close = s.rfind(')')?;
            let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
            let ut: f64 = f.get(11)?.parse().ok()?;
            let st: f64 = f.get(12)?.parse().ok()?;
            Some((ut + st) / if tck > 0.0 { tck } else { 100.0 })
        })
        .unwrap_or(0.0)
}
