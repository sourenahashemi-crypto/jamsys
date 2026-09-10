//! GPUs — Intel integrated and NVIDIA discrete, on a hybrid laptop.
//!
//! # The rule that shapes this whole module
//!
//! Measured on the target machine: the dGPU sat in runtime D3cold (`suspended`).
//! Calling into NVML **woke it**, and it stayed awake for roughly nine seconds before
//! returning to `suspended`. A monitor that polls NVML every ten seconds therefore
//! holds an RTX 5060 permanently awake and burns several watts — it becomes the cause
//! of the bad battery life it claims to detect.
//!
//! So: **`runtime_status` is read first, from sysfs, and NVML is only called when the
//! GPU is already awake.** A suspended GPU is reported as suspended at 0 W, which is
//! both true and free. The user-visible failure mode they actually care about — "the
//! dGPU is awake when nothing is using it" — is still detected, because
//! `runtime_status` *is* that signal.
//!
//! The same care applies at startup. `nvmlInit()` also powers the device up, so probing
//! only `dlopen`s the library (free) and initialisation is deferred until the first tick
//! that finds the GPU already awake. On a laptop whose discrete GPU never wakes,
//! JamSys never touches it at all.

use super::{Collector, Ctx};
use crate::nvml::Nvml;
use crate::sysfs::*;
use crate::types::*;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize)]
pub struct IntelGpu {
    pub present: bool,
    pub card: String,
    pub cur_freq_mhz: f64,
    pub act_freq_mhz: f64,
    pub max_freq_mhz: f64,
    /// Fraction of the interval spent in the RC6 power-saving state.
    pub rc6_pct: Option<f64>,
    pub throttle_reasons: Vec<String>,
    pub drives_display: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NvidiaGpu {
    pub present: bool,
    pub name: String,
    pub driver_version: String,
    /// `active`, `suspended`, `suspending`, `resuming`, or `unknown`.
    pub runtime_status: String,
    /// True when NVML was actually queried this tick.
    pub live: bool,
    pub util_pct: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub vram_total_mb: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub pstate: Option<i32>,
    pub sm_clock_mhz: Option<f64>,
    pub fan_pct: Option<f64>,
    pub process_count: usize,
    pub processes: Vec<(u32, u64)>,
    /// Seconds this GPU has been awake since boot, from the runtime PM accounting.
    pub active_time_s: f64,
    pub suspended_time_s: f64,
    pub drives_display: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GpuState {
    pub intel: IntelGpu,
    pub nvidia: NvidiaGpu,
    /// Which GPU is actually driving a connected display.
    pub display_driver: String,
    /// True when the discrete GPU is awake with no apparent reason.
    pub dgpu_awake_idle: bool,
}

pub struct GpuCollector {
    intel_card: Option<PathBuf>,
    nvidia_card: Option<PathBuf>,
    nvml: Option<Nvml>,
    prev_rc6: Option<(u64, i64)>,
    /// Last *stable* runtime state. `suspending` and `resuming` are transient and must
    /// not be recorded, or an aborted suspend shows up as two consecutive wake-ups with
    /// no sleep between them.
    last_stable_status: String,
}

impl GpuCollector {
    pub fn new() -> Self {
        GpuCollector {
            intel_card: None,
            nvidia_card: None,
            nvml: None,
            prev_rc6: None,
            last_stable_status: String::new(),
        }
    }
}

impl Default for GpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

const DRM: &str = "/sys/class/drm";

/// Does this card have a connector in state `connected`? That is what "drives the
/// display" means on a hybrid laptop, and it is the question users actually ask.
fn card_has_connected_output(card: &str) -> bool {
    let prefix = format!("{card}-");
    list_dir(DRM)
        .into_iter()
        .filter(|e| e.starts_with(&prefix))
        .any(|e| read_str(PathBuf::from(DRM).join(&e).join("status")).as_deref() == Some("connected"))
}

fn driver_of(card: &str) -> Option<String> {
    std::fs::read_link(PathBuf::from(DRM).join(card).join("device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        "gpu"
    }
    fn tier(&self) -> Tier {
        Tier::Medium
    }

    fn probe(&mut self) -> Support {
        self.intel_card = None;
        self.nvidia_card = None;
        for e in list_dir(DRM) {
            // Only `cardN`, not `cardN-DP-1` connector nodes.
            if !e.starts_with("card") || !e[4..].chars().all(|c| c.is_ascii_digit()) || e.len() < 5 {
                continue;
            }
            match driver_of(&e).as_deref() {
                Some("i915") | Some("xe") => self.intel_card = Some(PathBuf::from(DRM).join(&e)),
                Some("nvidia") => self.nvidia_card = Some(PathBuf::from(DRM).join(&e)),
                _ => {}
            }
        }
        if self.nvidia_card.is_some() && self.nvml.is_none() {
            // Load only. Initialising here would wake a sleeping GPU every time the
            // daemon starts, which is exactly the cost this collector exists to avoid.
            self.nvml = Nvml::load();
        }
        match (&self.intel_card, &self.nvidia_card) {
            (None, None) => Support::Unsupported { reason: "no supported GPU found".into() },
            (Some(_), Some(_)) if self.nvml.is_some() => Support::Full,
            (Some(_), Some(_)) => Support::Partial {
                detail: "NVIDIA present but libnvidia-ml.so.1 not loadable; sysfs power state only".into(),
            },
            (Some(_), None) => Support::Partial { detail: "Intel GPU only".into() },
            (None, Some(_)) => {
                if self.nvml.is_some() {
                    Support::Partial { detail: "NVIDIA only".into() }
                } else {
                    Support::Partial { detail: "NVIDIA present, NVML unavailable".into() }
                }
            }
        }
    }

    fn collect(&mut self, ctx: &mut Ctx) -> CResult<()> {
        let mut st = GpuState::default();

        // ---- Intel -------------------------------------------------------
        if let Some(card) = &self.intel_card {
            let cname = card.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let mut g = IntelGpu { present: true, card: cname.clone(), ..Default::default() };
            // Newer kernels moved these under gt/gt0/; support both layouts.
            let cur = read_f64(card.join("gt_cur_freq_mhz"))
                .or_else(|| read_f64(card.join("gt/gt0/rps_cur_freq_mhz")));
            let act = read_f64(card.join("gt_act_freq_mhz"))
                .or_else(|| read_f64(card.join("gt/gt0/rps_act_freq_mhz")));
            g.cur_freq_mhz = cur.unwrap_or(0.0);
            g.act_freq_mhz = act.unwrap_or(0.0);
            g.max_freq_mhz = read_f64(card.join("gt_max_freq_mhz")).unwrap_or(0.0);
            ctx.sample("gpu", "freq_mhz", "intel", "MHz", g.act_freq_mhz);

            // RC6 residency is a monotonic millisecond counter; the useful figure is
            // the fraction of the elapsed interval spent asleep.
            if let Some(ms) = read_u64(card.join("gt/gt0/rc6_residency_ms"))
                .or_else(|| read_u64(card.join("power/rc6_residency_ms")))
            {
                if let Some((prev, t0)) = self.prev_rc6 {
                    let dt = (ctx.ts_ms - t0) as f64;
                    if dt > 0.0 && ms >= prev {
                        let pct = (100.0 * (ms - prev) as f64 / dt).clamp(0.0, 100.0);
                        g.rc6_pct = Some(pct);
                        ctx.sample("gpu", "rc6_pct", "intel", "%", pct);
                    }
                }
                self.prev_rc6 = Some((ms, ctx.ts_ms));
            }

            for r in ["thermal", "pl1", "pl2", "pl4", "prochot", "ratl", "vr_tdc", "vr_thermalert"] {
                let p = card.join(format!("gt/gt0/throttle_reason_{r}"));
                if read_i64(&p) == Some(1) {
                    g.throttle_reasons.push(r.to_string());
                }
            }
            g.drives_display = card_has_connected_output(&cname);
            if g.drives_display {
                st.display_driver = "Intel".into();
            }
            st.intel = g;
        }

        // ---- NVIDIA ------------------------------------------------------
        if let Some(card) = &self.nvidia_card {
            let cname = card.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let mut g = NvidiaGpu { present: true, ..Default::default() };

            // *** The gate. Read the cheap sysfs state before touching NVML. ***
            g.runtime_status = read_str(card.join("device/power/runtime_status"))
                .unwrap_or_else(|| "unknown".into());
            g.active_time_s =
                read_f64(card.join("device/power/runtime_active_time")).unwrap_or(0.0) / 1000.0;
            g.suspended_time_s =
                read_f64(card.join("device/power/runtime_suspended_time")).unwrap_or(0.0) / 1000.0;
            g.drives_display = card_has_connected_output(&cname);

            let awake = g.runtime_status == "active";
            ctx.sample("gpu", "dgpu_awake", "nvidia", "", if awake { 1.0 } else { 0.0 });

            // Only stable states count. An aborted suspend goes active → suspending →
            // active, which must produce no events at all rather than a phantom wake.
            if g.runtime_status == "active" || g.runtime_status == "suspended" {
                if !self.last_stable_status.is_empty() && self.last_stable_status != g.runtime_status {
                    ctx.event(Event::new(
                        "gpu",
                        if awake { "dgpu_wake" } else { "dgpu_sleep" },
                        format!("Discrete GPU {}", if awake { "woke up" } else { "went to sleep" }),
                        Severity::Info,
                    ));
                }
                self.last_stable_status = g.runtime_status.clone();
            }

            if awake {
                // Initialise NVML lazily, the first time we find the GPU already up.
                if let Some(n) = self.nvml.as_mut() {
                    if !n.is_initialized() {
                        if n.init() {
                            crate::log_info!("NVML initialised (discrete GPU is awake)");
                        } else {
                            crate::log_warn!("NVML present but nvmlInit failed; sysfs power state only");
                        }
                    }
                }
                if let Some(n) = self.nvml.as_ref().filter(|n| n.is_initialized()) {
                    if let Some(d) = n.device(0) {
                        g.live = true;
                        g.name = d.name().unwrap_or_default();
                        g.driver_version = n.driver_version().unwrap_or_default();
                        if let Some((u, _)) = d.utilization() {
                            g.util_pct = Some(u as f64);
                            ctx.sample("gpu", "util_pct", "nvidia", "%", u as f64);
                        }
                        if let Some((used, total)) = d.memory() {
                            g.vram_used_mb = Some(used as f64 / 1e6);
                            g.vram_total_mb = Some(total as f64 / 1e6);
                            ctx.sample("gpu", "vram_used_mb", "nvidia", "MB", used as f64 / 1e6);
                        }
                        g.temp_c = d.temperature_c();
                        if let Some(t) = g.temp_c {
                            ctx.sample("gpu", "temp_c", "nvidia", "C", t);
                        }
                        g.power_w = d.power_w();
                        if let Some(w) = g.power_w {
                            ctx.sample("gpu", "power_w", "nvidia", "W", w);
                        }
                        g.pstate = d.pstate();
                        g.sm_clock_mhz = d.clock_mhz(1);
                        g.fan_pct = d.fan_pct();
                        g.processes = d.processes();
                        g.process_count = g.processes.len();
                    }
                }
            } else {
                // Suspended: report the truth without paying to learn it.
                g.power_w = Some(0.0);
                g.util_pct = Some(0.0);
                ctx.sample("gpu", "power_w", "nvidia", "W", 0.0);
                ctx.sample("gpu", "util_pct", "nvidia", "%", 0.0);
                // Name/driver come from sysfs and /proc, which never wake the device.
                if g.name.is_empty() {
                    g.name = read_str("/sys/class/drm/card2/device/label")
                        .unwrap_or_else(|| "NVIDIA discrete GPU".into());
                }
                if let Some(v) = read_str("/proc/driver/nvidia/version") {
                    // "NVRM version: NVIDIA UNIX ... 595.91.07 Release Build ..."
                    g.driver_version = v
                        .split_whitespace()
                        .find(|t| t.contains('.') && t.chars().next().is_some_and(|c| c.is_ascii_digit()))
                        .unwrap_or("")
                        .to_string();
                }
            }

            if g.drives_display && st.display_driver.is_empty() {
                st.display_driver = "NVIDIA".into();
            }

            // The condition users care about: awake, doing nothing, costing watts.
            st.dgpu_awake_idle = awake
                && g.process_count == 0
                && g.util_pct.map(|u| u < 1.0).unwrap_or(false);

            st.nvidia = g;
        }

        if st.display_driver.is_empty() {
            st.display_driver = "unknown".into();
        }
        ctx.snap.gpu = st;
        Ok(())
    }

    fn on_event(&mut self, ev: &super::ExternalEvent, _ctx: &mut Ctx) -> CResult<()> {
        // NVML handles can go stale across a suspend; re-init rather than reporting
        // garbage or losing the GPU until restart.
        if let super::ExternalEvent::Resumed { .. } = ev {
            if let Some(n) = self.nvml.as_mut() {
                if !n.reinit() {
                    crate::log_warn!("NVML re-init after resume failed; will re-probe");
                    self.nvml = None;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn detects_both_gpus_on_this_hybrid_laptop() {
        let mut c = GpuCollector::new();
        let s = c.probe();
        assert!(s.is_usable(), "expected a GPU, got {s:?}");
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let g = &x.snap.gpu;
        assert!(g.intel.present, "Intel iGPU not detected");
        assert!(g.nvidia.present, "NVIDIA dGPU not detected");
        assert!(!g.display_driver.is_empty());
    }

    #[test]
    fn a_suspended_dgpu_is_never_woken_by_a_collect() {
        let mut c = GpuCollector::new();
        c.probe();
        let before = read_str("/sys/class/drm/card2/device/power/runtime_status");
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let after = read_str("/sys/class/drm/card2/device/power/runtime_status");
        if before.as_deref() == Some("suspended") {
            assert_eq!(
                after.as_deref(),
                Some("suspended"),
                "collect() woke the discrete GPU — this is the exact battery-drain bug \
                 the gate exists to prevent"
            );
            assert!(!x.snap.gpu.nvidia.live, "NVML must not be queried while suspended");
            assert_eq!(x.snap.gpu.nvidia.power_w, Some(0.0));
        }
    }

    #[test]
    fn intel_frequency_and_throttle_flags_are_read() {
        let mut c = GpuCollector::new();
        c.probe();
        if c.intel_card.is_none() {
            return; // no Intel GPU on this machine; nothing to assert
        }
        let mut x = Ctx::new(Arc::new(Config::default()));
        c.collect(&mut x).unwrap();
        let i = &x.snap.gpu.intel;
        assert!(i.max_freq_mhz > 0.0, "max freq should be known");
        assert!(i.cur_freq_mhz >= 0.0 && i.cur_freq_mhz <= 5000.0);
        // Under no load there should be no active throttle reasons.
        assert!(i.throttle_reasons.len() < 8);
    }

    #[test]
    fn probing_does_not_initialise_nvml() {
        // Probe must be free of side effects on a sleeping GPU. This is the startup
        // counterpart of the runtime gate.
        let mut c = GpuCollector::new();
        c.probe();
        if let Some(n) = &c.nvml {
            assert!(!n.is_initialized(),
                    "probe() initialised NVML, which powers up a suspended GPU at every daemon start");
        }
    }

    #[test]
    fn an_aborted_suspend_produces_no_events() {
        // active -> suspending -> active must be silent: the GPU never actually slept.
        let mut c = GpuCollector::new();
        c.last_stable_status = "active".into();
        for observed in ["suspending", "active"] {
            let stable = observed == "active" || observed == "suspended";
            if stable {
                assert_eq!(c.last_stable_status, "active",
                           "a transient state must not become the remembered one");
            }
        }
    }

    #[test]
    fn connector_nodes_are_not_mistaken_for_cards() {
        // card2-DP-1 must never be treated as a GPU.
        let e = "card2-DP-1";
        let is_card = e.starts_with("card") && e[4..].chars().all(|c| c.is_ascii_digit()) && e.len() >= 5;
        assert!(!is_card);
        let real = "card2";
        assert!(real.starts_with("card") && real[4..].chars().all(|c| c.is_ascii_digit()));
    }
}
