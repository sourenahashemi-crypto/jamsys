//! NVML via `dlopen`.
//!
//! The library is opened at runtime rather than linked, so the daemon builds and runs
//! identically on a machine with no NVIDIA hardware and no driver installed. Every
//! symbol is optional; a missing one degrades that metric, not the collector.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};

type NvmlReturn = c_int;
const NVML_SUCCESS: NvmlReturn = 0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Utilization {
    pub gpu: c_uint,
    pub memory: c_uint,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct MemoryInfo {
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessInfo {
    pub pid: c_uint,
    pub used_gpu_memory: u64,
    pub gpu_instance_id: c_uint,
    pub compute_instance_id: c_uint,
}

type FnInit = unsafe extern "C" fn() -> NvmlReturn;
type FnShutdown = unsafe extern "C" fn() -> NvmlReturn;
type FnDeviceCount = unsafe extern "C" fn(*mut c_uint) -> NvmlReturn;
type FnHandleByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> NvmlReturn;
type FnName = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> NvmlReturn;
type FnUtil = unsafe extern "C" fn(*mut c_void, *mut Utilization) -> NvmlReturn;
type FnMem = unsafe extern "C" fn(*mut c_void, *mut MemoryInfo) -> NvmlReturn;
type FnTemp = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> NvmlReturn;
type FnPower = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> NvmlReturn;
type FnPstate = unsafe extern "C" fn(*mut c_void, *mut c_int) -> NvmlReturn;
type FnClock = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> NvmlReturn;
type FnDriverVersion = unsafe extern "C" fn(*mut c_char, c_uint) -> NvmlReturn;
type FnProcs = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut ProcessInfo) -> NvmlReturn;
type FnFanSpeed = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> NvmlReturn;
type FnPciInfo = unsafe extern "C" fn(*mut c_void, *mut u8) -> NvmlReturn;

pub struct Nvml {
    handle: *mut c_void,
    init: FnInit,
    /// `nvmlInit` has been called. Kept separate from loading because *initialising*
    /// NVML powers up a runtime-suspended GPU, while merely `dlopen`-ing the library
    /// does not. See `collectors::gpu` for why that distinction matters here.
    initialized: bool,
    shutdown: Option<FnShutdown>,
    device_count: Option<FnDeviceCount>,
    handle_by_index: Option<FnHandleByIndex>,
    name: Option<FnName>,
    util: Option<FnUtil>,
    mem: Option<FnMem>,
    temp: Option<FnTemp>,
    power: Option<FnPower>,
    pstate: Option<FnPstate>,
    clock: Option<FnClock>,
    driver_version: Option<FnDriverVersion>,
    procs: Option<FnProcs>,
    fan: Option<FnFanSpeed>,
    /// Resolved but not yet used. Kept so that adding multi-GPU disambiguation by PCI
    /// address later needs no change to the loader.
    #[allow(dead_code)]
    pci_info: Option<FnPciInfo>,
}

// NVML is documented as thread-safe, and the daemon is single-threaded regardless.
unsafe impl Send for Nvml {}

fn dlsym_opt<T>(h: *mut c_void, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    // SAFETY: `h` is a live handle from dlopen; `c` is a valid NUL-terminated name.
    let p = unsafe { libc::dlsym(h, c.as_ptr()) };
    if p.is_null() {
        None
    } else {
        // SAFETY: the caller states the correct ABI type for this symbol.
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }
}

impl Nvml {
    /// Load the library and resolve symbols **without** calling `nvmlInit`.
    ///
    /// This is safe to do at probe time on a machine whose discrete GPU is asleep:
    /// `dlopen` maps a shared object and touches no hardware. Call [`Nvml::init`]
    /// before the first query, and only once the GPU is known to be awake.
    ///
    /// Tries the versioned soname first — the unversioned `.so` only exists when the
    /// `-dev` package is installed, which it usually is not on an end-user machine.
    pub fn load() -> Option<Nvml> {
        for so in ["libnvidia-ml.so.1", "libnvidia-ml.so"] {
            let c = CString::new(so).ok()?;
            // SAFETY: standard dlopen with a valid name.
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if h.is_null() {
                continue;
            }
            // nvmlInit_v2 is the modern entry point; fall back for very old drivers.
            let init: FnInit = match dlsym_opt(h, "nvmlInit_v2").or_else(|| dlsym_opt(h, "nvmlInit")) {
                Some(f) => f,
                None => {
                    unsafe { libc::dlclose(h) };
                    continue;
                }
            };
            return Some(Nvml {
                handle: h,
                init,
                initialized: false,
                shutdown: dlsym_opt(h, "nvmlShutdown"),
                device_count: dlsym_opt(h, "nvmlDeviceGetCount_v2").or_else(|| dlsym_opt(h, "nvmlDeviceGetCount")),
                handle_by_index: dlsym_opt(h, "nvmlDeviceGetHandleByIndex_v2")
                    .or_else(|| dlsym_opt(h, "nvmlDeviceGetHandleByIndex")),
                name: dlsym_opt(h, "nvmlDeviceGetName"),
                util: dlsym_opt(h, "nvmlDeviceGetUtilizationRates"),
                mem: dlsym_opt(h, "nvmlDeviceGetMemoryInfo"),
                temp: dlsym_opt(h, "nvmlDeviceGetTemperature"),
                power: dlsym_opt(h, "nvmlDeviceGetPowerUsage"),
                pstate: dlsym_opt(h, "nvmlDeviceGetPerformanceState"),
                clock: dlsym_opt(h, "nvmlDeviceGetClockInfo"),
                driver_version: dlsym_opt(h, "nvmlSystemGetDriverVersion"),
                procs: dlsym_opt(h, "nvmlDeviceGetComputeRunningProcesses_v3")
                    .or_else(|| dlsym_opt(h, "nvmlDeviceGetComputeRunningProcesses_v2")),
                fan: dlsym_opt(h, "nvmlDeviceGetFanSpeed"),
                pci_info: dlsym_opt(h, "nvmlDeviceGetPciInfo_v3"),
            });
        }
        None
    }

    /// Call `nvmlInit`. This *will* power up a suspended GPU, so the caller must have
    /// already established that the device is awake.
    pub fn init(&mut self) -> bool {
        if self.initialized {
            return true;
        }
        // SAFETY: init has the NVML ABI and takes no arguments.
        self.initialized = unsafe { (self.init)() } == NVML_SUCCESS;
        self.initialized
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn device_count(&self) -> u32 {
        let Some(f) = self.device_count else { return 0 };
        let mut n: c_uint = 0;
        // SAFETY: `n` is a valid out-parameter.
        if unsafe { f(&mut n) } == NVML_SUCCESS { n } else { 0 }
    }

    pub fn device(&self, idx: u32) -> Option<Device<'_>> {
        let f = self.handle_by_index?;
        let mut h: *mut c_void = std::ptr::null_mut();
        // SAFETY: `h` is a valid out-parameter for an opaque handle.
        if unsafe { f(idx, &mut h) } != NVML_SUCCESS || h.is_null() {
            return None;
        }
        Some(Device { n: self, h })
    }

    pub fn driver_version(&self) -> Option<String> {
        let f = self.driver_version?;
        let mut buf = [0i8; 96];
        // SAFETY: buffer and length match.
        if unsafe { f(buf.as_mut_ptr(), buf.len() as c_uint) } != NVML_SUCCESS {
            return None;
        }
        // SAFETY: NVML NUL-terminates within the provided length.
        Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }

    /// Re-initialise after a suspend/resume, where handles can go stale. A no-op if
    /// NVML was never initialised, so a resume does not wake a sleeping GPU.
    pub fn reinit(&mut self) -> bool {
        if !self.initialized {
            return true;
        }
        // SAFETY: NVML permits repeated init calls; each pairs with a shutdown.
        self.initialized = unsafe { (self.init)() } == NVML_SUCCESS;
        self.initialized
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if let (Some(f), true) = (self.shutdown, self.initialized) {
            // SAFETY: matching shutdown for the successful init.
            unsafe { f() };
        }
        // SAFETY: handle came from dlopen and is not used again.
        unsafe { libc::dlclose(self.handle) };
    }
}

pub struct Device<'a> {
    n: &'a Nvml,
    h: *mut c_void,
}

impl Device<'_> {
    pub fn name(&self) -> Option<String> {
        let f = self.n.name?;
        let mut buf = [0i8; 128];
        // SAFETY: buffer and length match.
        if unsafe { f(self.h, buf.as_mut_ptr(), buf.len() as c_uint) } != NVML_SUCCESS {
            return None;
        }
        // SAFETY: NVML NUL-terminates within the provided length.
        Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }

    pub fn utilization(&self) -> Option<(u32, u32)> {
        let f = self.n.util?;
        let mut u = Utilization::default();
        // SAFETY: valid out-parameter of the declared repr(C) type.
        (unsafe { f(self.h, &mut u) } == NVML_SUCCESS).then_some((u.gpu, u.memory))
    }

    pub fn memory(&self) -> Option<(u64, u64)> {
        let f = self.n.mem?;
        let mut m = MemoryInfo::default();
        // SAFETY: valid out-parameter of the declared repr(C) type.
        (unsafe { f(self.h, &mut m) } == NVML_SUCCESS).then_some((m.used, m.total))
    }

    pub fn temperature_c(&self) -> Option<f64> {
        let f = self.n.temp?;
        let mut t: c_uint = 0;
        // 0 == NVML_TEMPERATURE_GPU.
        // SAFETY: valid out-parameter.
        if unsafe { f(self.h, 0, &mut t) } != NVML_SUCCESS {
            return None;
        }
        crate::util::sane(t as f64, -40.0, 150.0)
    }

    pub fn power_w(&self) -> Option<f64> {
        let f = self.n.power?;
        let mut mw: c_uint = 0;
        // SAFETY: valid out-parameter.
        if unsafe { f(self.h, &mut mw) } != NVML_SUCCESS {
            return None;
        }
        crate::util::sane(mw as f64 / 1000.0, 0.0, 1000.0)
    }

    /// Performance state as a small integer: 0 is maximum, 8 is deep idle, 32 unknown.
    pub fn pstate(&self) -> Option<i32> {
        let f = self.n.pstate?;
        let mut p: c_int = 32;
        // SAFETY: valid out-parameter.
        (unsafe { f(self.h, &mut p) } == NVML_SUCCESS && p < 32).then_some(p)
    }

    /// `kind`: 0 graphics, 1 SM, 2 memory, 3 video.
    pub fn clock_mhz(&self, kind: u32) -> Option<f64> {
        let f = self.n.clock?;
        let mut c: c_uint = 0;
        // SAFETY: valid out-parameter.
        (unsafe { f(self.h, kind, &mut c) } == NVML_SUCCESS).then(|| c as f64)
    }

    pub fn fan_pct(&self) -> Option<f64> {
        let f = self.n.fan?;
        let mut s: c_uint = 0;
        // SAFETY: valid out-parameter.
        if unsafe { f(self.h, &mut s) } != NVML_SUCCESS {
            return None;
        }
        crate::util::sane(s as f64, 0.0, 100.0)
    }

    /// PIDs with GPU memory allocated, and how much.
    pub fn processes(&self) -> Vec<(u32, u64)> {
        let Some(f) = self.n.procs else { return Vec::new() };
        let mut count: c_uint = 64;
        let mut buf = vec![
            ProcessInfo { pid: 0, used_gpu_memory: 0, gpu_instance_id: 0, compute_instance_id: 0 };
            count as usize
        ];
        // SAFETY: `count` states the buffer capacity and is updated to the used length.
        if unsafe { f(self.h, &mut count, buf.as_mut_ptr()) } != NVML_SUCCESS {
            return Vec::new();
        }
        buf.into_iter()
            .take((count as usize).min(64))
            .map(|p| (p.pid, p.used_gpu_memory))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_the_library_does_not_initialise_it() {
        // Loading must be free of side effects: this is what lets the daemon report
        // NVIDIA coverage at startup without powering up a sleeping GPU.
        //
        // Only the handle's own flag is asserted. `nvmlInit` state is process-global,
        // so a sibling test that initialises NVML would make an assertion about
        // `device_count()` order-dependent — and the flag is what actually gates
        // whether *this* code calls into the library.
        if let Some(n) = Nvml::load() {
            assert!(!n.is_initialized(), "load() must not call nvmlInit");
        }
    }

    #[test]
    fn opening_nvml_is_optional_and_never_panics() {
        // On a machine without NVIDIA this must return None rather than aborting.
        match Nvml::load().map(|mut n| { n.init(); n }) {
            None => { /* valid outcome */ }
            Some(n) => {
                let count = n.device_count();
                assert!(count < 64, "implausible device count {count}");
                if count > 0 {
                    let d = n.device(0).expect("device 0");
                    // Every getter is allowed to return None; none may panic.
                    let _ = d.name();
                    let _ = d.utilization();
                    let _ = d.memory();
                    let _ = d.temperature_c();
                    let _ = d.power_w();
                    let _ = d.pstate();
                    let _ = d.processes();
                }
            }
        }
    }

    #[test]
    fn out_of_range_device_index_is_none() {
        if let Some(mut n) = Nvml::load() {
            if n.init() {
                assert!(n.device(999).is_none());
            }
        }
    }
}
