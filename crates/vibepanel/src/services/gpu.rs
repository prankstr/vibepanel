//! GpuService - polling-based GPU resource monitoring with multi-GPU support.
//!
//! This service provides GPU utilization, VRAM usage, temperature, clock speed,
//! and power draw by reading vendor-specific interfaces:
//!
//! - **AMD**: sysfs files under `/sys/class/drm/cardN/device/` plus AMDGPU ioctls
//! - **NVIDIA**: NVML via the `nvml-wrapper` crate (runtime-loaded
//!   `libnvidia-ml.so`, see [`init_nvml`])
//!
//! All GPUs are discovered at startup, but only the configured display set is
//! polled while the widget or popover is visible.
//!
//! 1. **`devices = "auto"`** (default): shows only the preferred GPU.
//! 2. **`devices = "all"`**: shows all detected GPUs, ordered with the
//!    preferred GPU first.
//! 3. **`devices = [N, M]`** or **`devices = N`**: shows explicit GPU indices
//!    in the configured order.
//! 4. **Legacy compatibility**: `device = N` remains accepted as an alias for a
//!    single explicit GPU selection.
//!
//! Preferred GPU auto-selection uses the existing heuristic: prefer the primary
//! discrete GPU, then any discrete GPU, then the primary GPU.
//! AMD discrete detection uses the AMDGPU FUSION flag with sysfs fallback markers.
//! AMD primary detection uses `boot_vga` sysfs.
//! NVIDIA GPUs are always treated as discrete.
//! Falls back to index 0 if no better match is found.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let service = GpuService::global();
//! service.connect(|snapshot| {
//!     if let Some(usage) = snapshot.gpu_usage {
//!         println!("GPU: {:.0}%", usage);
//!     }
//! });
//! ```

use std::cell::{Cell, RefCell};
use std::fs::{self, OpenOptions};
use std::mem;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::glib::{self, SourceId};
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use tracing::{debug, trace, warn};

use super::callbacks::{CallbackId, Callbacks};
use super::config_manager::ConfigManager;

const DEFAULT_POLL_INTERVAL_SECS: u32 = 3;

/// Threshold above which GPU usage is considered "high" (higher than CPU since sustained GPU load is normal).
pub(crate) const GPU_HIGH_USAGE_THRESHOLD: f32 = 90.0;

const DRM_CLASS_PATH: &str = "/sys/class/drm";
const DRI_DEV_PATH: &str = "/dev/dri";

const NVML_PATH_ENV: &str = "VIBEPANEL_NVML_PATH";

/// Fallback locations used when `libnvidia-ml.so.1` is not on the loader path.
const NVML_LIB_CANDIDATES: &[&str] = &[
    // NixOS
    "/run/opengl-driver/lib/libnvidia-ml.so.1",
    // Debian/Ubuntu multiarch
    "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
    // Fedora/RHEL/openSUSE
    "/usr/lib64/libnvidia-ml.so.1",
    // Arch
    "/usr/lib/libnvidia-ml.so.1",
];

// Linux DRM/AMDGPU UAPI constants from drm.h and amdgpu_drm.h.
// Kept local to avoid adding a libdrm binding dependency for one query.
const DRM_COMMAND_BASE: u8 = 0x40;
const DRM_AMDGPU_INFO: u8 = 0x05;
const AMDGPU_INFO_DEV_INFO: u32 = 0x16;
const AMDGPU_IDS_FLAGS_FUSION: u64 = 0x1;

const IOC_NRBITS: u8 = 8;
const IOC_TYPEBITS: u8 = 8;
const IOC_SIZEBITS: u8 = 14;
const IOC_WRITE: u8 = 1;

const IOC_NRSHIFT: u8 = 0;
const IOC_TYPESHIFT: u8 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u8 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u8 = IOC_SIZESHIFT + IOC_SIZEBITS;

const fn linux_iow(nr: u8, size: usize) -> libc::c_ulong {
    ((IOC_WRITE as libc::c_ulong) << IOC_DIRSHIFT)
        | ((b'd' as libc::c_ulong) << IOC_TYPESHIFT)
        | ((nr as libc::c_ulong) << IOC_NRSHIFT)
        | ((size as libc::c_ulong) << IOC_SIZESHIFT)
}

#[repr(C)]
#[derive(Default)]
struct DrmAmdgpuInfo {
    return_pointer: u64,
    return_size: u32,
    query: u32,
    // The largest input union member in drm_amdgpu_info is 16 bytes.
    payload: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
struct DrmAmdgpuInfoDeviceIds {
    device_id: u32,
    chip_rev: u32,
    external_rev: u32,
    pci_rev: u32,
    family: u32,
    num_shader_engines: u32,
    num_shader_arrays_per_engine: u32,
    gpu_counter_freq: u32,
    max_engine_clock: u64,
    max_memory_clock: u64,
    cu_active_number: u32,
    cu_ao_mask: u32,
    cu_bitmap: [[u32; 4]; 4],
    enabled_rb_pipes_mask: u32,
    num_rb_pipes: u32,
    num_hw_gfx_contexts: u32,
    pcie_gen: u32,
    ids_flags: u64,
}

/// GPU hardware power state, read from sysfs `power/runtime_status`.
///
/// Used to skip NVML/sysfs polling when the GPU is in D3cold sleep.
/// NVML calls (even `device_by_index`) count as device activity and
/// prevent NVIDIA GPUs from entering power-saving states.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GpuPowerState {
    /// GPU is powered on and active.
    Active,
    /// GPU is in runtime suspend (D3cold/D3hot).
    Suspended,
    /// Could not determine power state (sysfs not available).
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct GpuDeviceSnapshot {
    /// Hardware power state (active, suspended, or unknown).
    pub power_state: GpuPowerState,
    /// GPU utilization percentage (0.0 - 100.0).
    pub gpu_usage: Option<f32>,
    /// Used VRAM in bytes.
    pub vram_used: Option<u64>,
    /// Total VRAM in bytes.
    pub vram_total: Option<u64>,
    /// GPU temperature in degrees Celsius.
    pub temperature: Option<f32>,
    /// GPU clock speed in MHz.
    pub clock_mhz: Option<u64>,
    /// GPU power draw in watts.
    pub power_watts: Option<f32>,
    /// Device name (product name, or `vendor:device` PCI ID fallback).
    pub device_name: Option<String>,
}

impl GpuDeviceSnapshot {
    fn is_gpu_high(&self) -> bool {
        self.gpu_usage
            .map(|u| u >= GPU_HIGH_USAGE_THRESHOLD)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    pub available: bool,
    /// Hardware power state (active, suspended, or unknown).
    pub power_state: GpuPowerState,
    /// GPU utilization percentage (0.0 - 100.0).
    pub gpu_usage: Option<f32>,
    /// Used VRAM in bytes.
    pub vram_used: Option<u64>,
    /// Total VRAM in bytes.
    pub vram_total: Option<u64>,
    /// GPU temperature in degrees Celsius.
    pub temperature: Option<f32>,
    /// GPU clock speed in MHz.
    pub clock_mhz: Option<u64>,
    /// GPU power draw in watts.
    pub power_watts: Option<f32>,
    /// Device name (product name, or `vendor:device` PCI ID fallback).
    pub device_name: Option<String>,
    /// Per-device snapshots in display order (preferred GPU first).
    pub devices: Vec<GpuDeviceSnapshot>,
}

impl GpuSnapshot {
    /// Returns a snapshot representing an unknown/unavailable GPU.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Returns true if GPU usage is above the high threshold.
    pub fn is_gpu_high(&self) -> bool {
        if self.devices.is_empty() {
            self.gpu_usage
                .map(|u| u >= GPU_HIGH_USAGE_THRESHOLD)
                .unwrap_or(false)
        } else {
            self.devices.iter().any(GpuDeviceSnapshot::is_gpu_high)
        }
    }

    /// Returns the per-device GPU snapshots, falling back to the summary fields
    /// for older callers when the service has not yet populated the device list.
    pub fn devices_for_display(&self) -> Vec<GpuDeviceSnapshot> {
        if !self.devices.is_empty() {
            return self.devices.clone();
        }

        if !self.available {
            return Vec::new();
        }

        vec![GpuDeviceSnapshot {
            power_state: self.power_state,
            gpu_usage: self.gpu_usage,
            vram_used: self.vram_used,
            vram_total: self.vram_total,
            temperature: self.temperature,
            clock_mhz: self.clock_mhz,
            power_watts: self.power_watts,
            device_name: self.device_name.clone(),
        }]
    }

    pub fn all_devices_suspended(&self) -> bool {
        let devices = self.devices_for_display();
        !devices.is_empty()
            && devices
                .iter()
                .all(|device| device.power_state == GpuPowerState::Suspended)
    }

    fn from_devices(devices: Vec<GpuDeviceSnapshot>) -> Self {
        let Some(summary) = devices.first().cloned() else {
            return Self::unknown();
        };

        Self {
            available: true,
            power_state: summary.power_state,
            gpu_usage: summary.gpu_usage,
            vram_used: summary.vram_used,
            vram_total: summary.vram_total,
            temperature: summary.temperature,
            clock_mhz: summary.clock_mhz,
            power_watts: summary.power_watts,
            device_name: summary.device_name.clone(),
            devices,
        }
    }
}

struct AmdGpuDevice {
    /// e.g., `/sys/class/drm/card1/device`
    device_path: PathBuf,

    /// Cached hwmon directory path (e.g., `/sys/class/drm/card1/device/hwmon/hwmon3`).
    /// `None` if hwmon was not found (metrics like temp/clock/power won't be available).
    hwmon_path: Option<PathBuf>,

    /// Sysfs `power/runtime_status` path for checking hardware power state.
    runtime_status_path: Option<PathBuf>,

    device_name: Option<String>,

    /// Whether this is a discrete GPU (determined via AMDGPU FUSION flag or fallback markers).
    is_discrete: bool,

    /// Whether this is the primary GPU (`boot_vga == 1`).
    is_primary: bool,
}

struct NvidiaGpuDevice {
    /// Kept alive for the lifetime of the service; `Device` handles are
    /// re-acquired each poll via `device_by_index` to avoid lifetime complexity.
    nvml: Rc<Nvml>,

    device_index: u32,
    device_name: Option<String>,

    /// Sysfs `power/runtime_status` path for checking hardware power state.
    runtime_status_path: Option<PathBuf>,

    /// Whether this is the primary GPU (`boot_vga == 1`).
    is_primary: bool,
}

enum GpuDevice {
    Amd(AmdGpuDevice),
    Nvidia(Box<NvidiaGpuDevice>), // boxed to keep enum size small (Nvml is ~11KB)
}

impl GpuDevice {
    fn name(&self) -> Option<&str> {
        match self {
            GpuDevice::Amd(d) => d.device_name.as_deref(),
            GpuDevice::Nvidia(d) => d.device_name.as_deref(),
        }
    }

    fn is_discrete(&self) -> bool {
        match self {
            GpuDevice::Amd(d) => d.is_discrete,
            // NVIDIA GPUs on Linux are always discrete (no NVIDIA iGPUs exist).
            GpuDevice::Nvidia(_) => true,
        }
    }

    fn is_primary(&self) -> bool {
        match self {
            GpuDevice::Amd(d) => d.is_primary,
            GpuDevice::Nvidia(d) => d.is_primary,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum GpuDisplaySelection {
    #[default]
    Auto,
    All,
    Explicit(Vec<usize>),
}

/// Shared, process-wide GPU monitoring service.
///
/// Discovers all available GPUs at startup and polls them at regular intervals
/// via vendor-specific backends (AMD sysfs, NVIDIA NVML).
/// Notifies registered callbacks whenever the snapshot updates.
///
/// Unlike other services, GPU polling is demand-driven: callers must use
/// `request_polling()`/`release_polling()` to start/stop the timer. This is
/// because NVML calls (even `device_by_index()`) count as device activity and
/// prevent NVIDIA GPUs from entering D3cold power savings.
pub struct GpuService {
    snapshot: RefCell<GpuSnapshot>,
    callbacks: Callbacks<GpuSnapshot>,

    /// Timer source for periodic polling.
    timer_source: RefCell<Option<SourceId>>,

    /// All discovered GPU devices.
    devices: Vec<GpuDevice>,

    /// GPU indices currently configured for display and polling.
    display_indices: RefCell<Vec<usize>>,

    /// Polling interval in seconds.
    poll_interval: Cell<u32>,

    /// Reference count for polling requests. Polling runs only while > 0.
    poll_requests: Cell<u32>,
}

impl GpuService {
    fn new() -> Rc<Self> {
        debug!("GpuService: initializing");

        let devices = Self::discover_all_gpus();
        let display_selection = Self::read_display_config();
        let display_indices = Self::resolve_display_indices(&devices, &display_selection);

        Self::log_display_selection(&devices, &display_selection, &display_indices);

        let initial_snapshot = if !display_indices.is_empty() {
            GpuSnapshot {
                available: true,
                ..Default::default()
            }
        } else {
            GpuSnapshot::unknown()
        };

        Rc::new(Self {
            snapshot: RefCell::new(initial_snapshot),
            callbacks: Callbacks::new(),
            timer_source: RefCell::new(None),
            devices,
            display_indices: RefCell::new(display_indices),
            poll_interval: Cell::new(DEFAULT_POLL_INTERVAL_SECS),
            poll_requests: Cell::new(0),
        })
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<GpuService> = GpuService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the GPU snapshot changes.
    ///
    /// The callback is immediately invoked with the current snapshot.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&GpuSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    pub fn snapshot(&self) -> GpuSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn reconfigure(&self) {
        if !self.refresh_display_selection() {
            return;
        }

        if self.poll_requests.get() > 0 {
            self.poll();
            return;
        }

        let snapshot = if self.display_indices.borrow().is_empty() {
            GpuSnapshot::unknown()
        } else {
            GpuSnapshot {
                available: true,
                ..Default::default()
            }
        };

        *self.snapshot.borrow_mut() = snapshot;
        self.callbacks.notify(&self.snapshot.borrow());
    }

    fn start_polling(this: &Rc<Self>) {
        this.poll();

        let this_weak = Rc::downgrade(this);
        let interval = this.poll_interval.get();

        debug!("GpuService: starting polling every {}s", interval);

        let source_id = glib::timeout_add_seconds_local(interval, move || {
            if let Some(this) = this_weak.upgrade() {
                this.poll();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });

        *this.timer_source.borrow_mut() = Some(source_id);
    }

    fn stop_polling(&self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            debug!("GpuService: stopping polling");
            source_id.remove();
        }
    }

    /// Request that GPU polling be active. Polling starts on the first request
    /// (0 -> 1 transition) and stops when all requests are released.
    ///
    /// Requires `&Rc<Self>` because `start_polling` creates a weak reference
    /// for the timer closure.
    pub fn request_polling(this: &Rc<Self>) {
        if this.devices.is_empty() {
            return;
        }
        let prev = this.poll_requests.get();
        this.poll_requests.set(prev + 1);
        if prev == 0 {
            debug!("GpuService: first poll request, starting polling");
            Self::start_polling(this);
        }
    }

    /// Release a polling request. Polling stops when the last request is released
    /// (1 -> 0 transition).
    pub fn release_polling(&self) {
        let prev = self.poll_requests.get();
        if prev == 0 {
            debug!("GpuService: release_polling called with no outstanding requests");
            return;
        }
        self.poll_requests.set(prev - 1);
        if prev == 1 {
            debug!("GpuService: last poll request released, stopping polling");
            self.stop_polling();
        }
    }

    fn poll(&self) {
        let snapshot = {
            let display_indices = self.display_indices.borrow();
            if display_indices.is_empty() {
                GpuSnapshot::unknown()
            } else {
                let mut snapshots = Vec::with_capacity(display_indices.len());

                for &idx in display_indices.iter() {
                    if let Some(device) = self.devices.get(idx) {
                        snapshots.push(Self::poll_device(idx, device));
                    }
                }

                GpuSnapshot::from_devices(snapshots)
            }
        };

        *self.snapshot.borrow_mut() = snapshot;
        self.callbacks.notify(&self.snapshot.borrow());
    }

    fn poll_device(idx: usize, device: &GpuDevice) -> GpuDeviceSnapshot {
        trace!("GpuService: polling GPU {} metrics", idx);

        let runtime_path = match device {
            GpuDevice::Amd(d) => d.runtime_status_path.as_deref(),
            GpuDevice::Nvidia(d) => d.runtime_status_path.as_deref(),
        };
        let power_state = runtime_path
            .map(read_runtime_status)
            .unwrap_or(GpuPowerState::Unknown);

        if power_state == GpuPowerState::Suspended {
            trace!("GpuService: GPU {} is suspended, skipping vendor poll", idx);
            return GpuDeviceSnapshot {
                power_state: GpuPowerState::Suspended,
                device_name: device.name().map(str::to_string),
                ..Default::default()
            };
        }

        let mut snapshot = match device {
            GpuDevice::Amd(amd) => Self::poll_amd(amd),
            GpuDevice::Nvidia(nvidia) => Self::poll_nvidia(nvidia),
        };
        snapshot.power_state = power_state;
        snapshot
    }

    fn poll_amd(device: &AmdGpuDevice) -> GpuDeviceSnapshot {
        let gpu_usage =
            read_sysfs_u32(&device.device_path.join("gpu_busy_percent")).map(|v| v.min(100) as f32);

        let vram_used = read_sysfs_u64(&device.device_path.join("mem_info_vram_used"));
        let vram_total = read_sysfs_u64(&device.device_path.join("mem_info_vram_total"));

        let (temperature, clock_mhz, power_watts) = if let Some(ref hwmon) = device.hwmon_path {
            let temp = read_sysfs_u32(&hwmon.join("temp1_input")).map(|v| v as f32 / 1000.0);

            let clock = read_sysfs_u64(&hwmon.join("freq1_input")).map(|v| v / 1_000_000);

            let power =
                read_sysfs_u64(&hwmon.join("power1_average")).map(|v| v as f32 / 1_000_000.0);

            (temp, clock, power)
        } else {
            (None, None, None)
        };

        GpuDeviceSnapshot {
            gpu_usage,
            vram_used,
            vram_total,
            temperature,
            clock_mhz,
            power_watts,
            device_name: device.device_name.clone(),
            ..Default::default()
        }
    }

    fn poll_nvidia(nvidia: &NvidiaGpuDevice) -> GpuDeviceSnapshot {
        let device = match nvidia.nvml.device_by_index(nvidia.device_index) {
            Ok(d) => d,
            Err(e) => {
                warn!("GpuService: failed to acquire NVIDIA device handle: {e}");
                return GpuDeviceSnapshot {
                    device_name: nvidia.device_name.clone(),
                    ..Default::default()
                };
            }
        };

        let gpu_usage = device
            .utilization_rates()
            .ok()
            .map(|u| (u.gpu as f32).min(100.0));

        let (vram_used, vram_total) = device
            .memory_info()
            .ok()
            .map(|m| (Some(m.used), Some(m.total)))
            .unwrap_or((None, None));

        let temperature = device
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as f32);

        let clock_mhz = device.clock_info(Clock::Graphics).ok().map(|c| c as u64);

        let power_watts = device.power_usage().ok().map(|mw| mw as f32 / 1000.0);

        GpuDeviceSnapshot {
            gpu_usage,
            vram_used,
            vram_total,
            temperature,
            clock_mhz,
            power_watts,
            device_name: nvidia.device_name.clone(),
            ..Default::default()
        }
    }

    /// Discover all supported GPUs (AMD via sysfs, NVIDIA via NVML).
    fn discover_all_gpus() -> Vec<GpuDevice> {
        let mut devices = Vec::new();

        for amd in Self::discover_all_amdgpu() {
            devices.push(GpuDevice::Amd(amd));
        }

        for nvidia in Self::discover_all_nvidia() {
            devices.push(GpuDevice::Nvidia(Box::new(nvidia)));
        }

        if devices.is_empty() {
            debug!("GpuService: no supported GPU found");
        }

        devices
    }

    /// Scan `/sys/class/drm/card*` for AMD GPUs using the `amdgpu` driver.
    fn discover_all_amdgpu() -> Vec<AmdGpuDevice> {
        let drm_path = Path::new(DRM_CLASS_PATH);
        if !drm_path.exists() {
            return Vec::new();
        }

        let entries = match fs::read_dir(drm_path) {
            Ok(it) => it,
            Err(err) => {
                warn!("GpuService: failed to read {}: {err}", DRM_CLASS_PATH);
                return Vec::new();
            }
        };

        // Exclude connector nodes (e.g. card0-HDMI-A-1)
        let mut cards: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("card") && !name_str.contains('-') {
                cards.push(entry.path());
            }
        }

        // Sort by card number for deterministic ordering
        cards.sort();

        let mut devices = Vec::new();

        for card_path in cards {
            let device_path = card_path.join("device");
            if !device_path.exists() {
                continue;
            }

            let driver_link = device_path.join("driver");
            if let Ok(driver_target) = fs::read_link(&driver_link) {
                let driver_name = driver_target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();

                if driver_name == "amdgpu" {
                    let hwmon_path = discover_hwmon(&device_path);
                    let device_name = read_device_name(&device_path);

                    let is_discrete = is_amd_discrete_gpu(&device_path);
                    // boot_vga: 1 = primary VGA device, 0 = non-primary.
                    let boot_vga = read_sysfs_u32(&device_path.join("boot_vga"));
                    let is_primary = boot_vga == Some(1);

                    // Resolve the PCI device path for runtime_status.
                    // device_path is a symlink like /sys/class/drm/card1/device ->
                    // ../../devices/pci.../XXXX:XX:XX.X; canonicalize to get the real path.
                    let runtime_status_path = fs::canonicalize(&device_path)
                        .ok()
                        .map(|p| p.join("power/runtime_status"))
                        .filter(|p| p.exists());

                    debug!(
                        "GpuService: found AMD GPU {:?} at {:?} (discrete: {})",
                        device_name, device_path, is_discrete,
                    );

                    devices.push(AmdGpuDevice {
                        device_path,
                        hwmon_path,
                        runtime_status_path,
                        device_name,
                        is_discrete,
                        is_primary,
                    });
                }
            }
        }

        devices
    }

    /// Discover all NVIDIA GPUs via NVML (runtime-loads `libnvidia-ml.so`).
    fn discover_all_nvidia() -> Vec<NvidiaGpuDevice> {
        let nvml = match init_nvml() {
            Some(n) => Rc::new(n),
            None => return Vec::new(),
        };

        let count = match nvml.device_count() {
            Ok(0) => return Vec::new(),
            Ok(c) => c,
            Err(e) => {
                warn!("GpuService: NVML device_count failed: {e}");
                return Vec::new();
            }
        };

        let mut devices = Vec::new();

        for device_index in 0..count {
            let device = match nvml.device_by_index(device_index) {
                Ok(dev) => dev,
                Err(e) => {
                    warn!("GpuService: NVML device_by_index({device_index}) failed: {e}");
                    continue;
                }
            };

            let device_name = device.name().ok();

            let pci_device_path = device
                .pci_info()
                .ok()
                .and_then(|pci| sysfs_pci_device_path_from_nvml_bus_id(&pci.bus_id));

            let runtime_status_path = pci_device_path
                .as_ref()
                .map(|path| path.join("power/runtime_status"))
                .filter(|p| p.exists());

            let is_primary = pci_device_path
                .as_ref()
                .and_then(|path| read_sysfs_u32(&path.join("boot_vga")))
                == Some(1);

            debug!(
                "GpuService: found NVIDIA GPU {:?} (nvml_index: {})",
                device_name, device_index,
            );

            devices.push(NvidiaGpuDevice {
                nvml: nvml.clone(),
                device_index,
                device_name,
                runtime_status_path,
                is_primary,
            });
        }

        devices
    }

    /// Read the `devices` config option from `[widgets.gpu]`.
    fn read_display_config() -> GpuDisplaySelection {
        let config = ConfigManager::global();

        if let Some(value) = config.get_widget_option("gpu", "devices") {
            return Self::parse_display_selection_value(&value, "devices").unwrap_or_default();
        }

        if let Some(value) = config.get_widget_option("gpu", "device") {
            debug!("GpuService: using legacy 'device' option; prefer 'devices'");
            return Self::parse_display_selection_value(&value, "device").unwrap_or_default();
        }

        GpuDisplaySelection::Auto
    }

    fn parse_display_selection_value(
        value: &toml::Value,
        option_name: &str,
    ) -> Option<GpuDisplaySelection> {
        match value {
            toml::Value::Integer(index) if *index >= 0 => {
                Some(GpuDisplaySelection::Explicit(vec![*index as usize]))
            }
            toml::Value::Integer(index) => {
                warn!(
                    "GpuService: invalid '{}' config value {} (must be >= 0), using auto",
                    option_name, index,
                );
                None
            }
            toml::Value::String(mode) => {
                if let Some(selection) = Self::parse_display_selection_mode(mode) {
                    Some(selection)
                } else {
                    warn!(
                        "GpuService: invalid '{}' config value {:?}, expected 'auto', 'all', an integer, or an array of integers; using auto",
                        option_name, mode,
                    );
                    None
                }
            }
            toml::Value::Array(entries) => {
                let mut indices = Vec::new();
                for entry in entries {
                    match entry {
                        toml::Value::Integer(index) if *index >= 0 => {
                            let index = *index as usize;
                            if !indices.contains(&index) {
                                indices.push(index);
                            }
                        }
                        other => {
                            warn!(
                                "GpuService: ignoring invalid '{}' array entry {other}; expected non-negative integers",
                                option_name,
                            );
                        }
                    }
                }

                if indices.is_empty() {
                    warn!(
                        "GpuService: '{}' array contained no valid GPU indices, using auto",
                        option_name,
                    );
                    None
                } else {
                    Some(GpuDisplaySelection::Explicit(indices))
                }
            }
            other => {
                warn!(
                    "GpuService: invalid '{}' config value: {other}, expected 'auto', 'all', an integer, or an array of integers; using auto",
                    option_name,
                );
                None
            }
        }
    }

    fn parse_display_selection_mode(mode: &str) -> Option<GpuDisplaySelection> {
        match mode.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(GpuDisplaySelection::Auto),
            "all" => Some(GpuDisplaySelection::All),
            _ => None,
        }
    }

    fn display_selection_description(selection: &GpuDisplaySelection) -> String {
        match selection {
            GpuDisplaySelection::Auto => "auto".to_string(),
            GpuDisplaySelection::All => "config (devices = all)".to_string(),
            GpuDisplaySelection::Explicit(indices) if indices.len() == 1 => {
                format!("config (devices = {})", indices[0])
            }
            GpuDisplaySelection::Explicit(indices) => {
                format!("config (devices = {:?})", indices)
            }
        }
    }

    fn log_display_selection(
        devices: &[GpuDevice],
        selection: &GpuDisplaySelection,
        display_indices: &[usize],
    ) {
        if let Some(idx) = display_indices.first().copied() {
            debug!(
                "GpuService: selected GPU {} ({:?}) via {}",
                idx,
                devices[idx].name().unwrap_or("unknown"),
                Self::display_selection_description(selection),
            );
            if display_indices.len() > 1 {
                debug!("GpuService: displaying GPU indices {:?}", display_indices);
            }
        } else {
            debug!("GpuService: no GPU selected");
        }
    }

    fn refresh_display_selection(&self) -> bool {
        let display_selection = Self::read_display_config();
        let display_indices = Self::resolve_display_indices(&self.devices, &display_selection);
        let changed = *self.display_indices.borrow() != display_indices;

        if changed {
            Self::log_display_selection(&self.devices, &display_selection, &display_indices);
            *self.display_indices.borrow_mut() = display_indices;
        }

        changed
    }

    fn resolve_display_indices(
        devices: &[GpuDevice],
        selection: &GpuDisplaySelection,
    ) -> Vec<usize> {
        match selection {
            GpuDisplaySelection::Auto => Self::auto_select(devices).into_iter().collect(),
            GpuDisplaySelection::All => {
                let Some(selected_index) = Self::auto_select(devices) else {
                    return Vec::new();
                };

                let mut display_indices = Vec::with_capacity(devices.len());
                display_indices.push(selected_index);
                display_indices.extend((0..devices.len()).filter(|idx| *idx != selected_index));
                display_indices
            }
            GpuDisplaySelection::Explicit(indices) => {
                let mut valid_indices = Vec::with_capacity(indices.len());
                for &idx in indices {
                    if idx < devices.len() {
                        if !valid_indices.contains(&idx) {
                            valid_indices.push(idx);
                        }
                    } else {
                        warn!(
                            "GpuService: configured GPU index {} out of range (have {} GPU(s)), ignoring it",
                            idx,
                            devices.len(),
                        );
                    }
                }

                if valid_indices.is_empty() {
                    warn!(
                        "GpuService: configured GPU list {:?} contains no valid indices (have {} GPU(s)), falling back to auto",
                        indices,
                        devices.len(),
                    );
                    Self::auto_select(devices).into_iter().collect()
                } else {
                    valid_indices
                }
            }
        }
    }

    /// Auto-select a GPU: prefer the primary discrete GPU, then any discrete
    /// GPU, then the primary GPU, then index 0.
    fn auto_select(devices: &[GpuDevice]) -> Option<usize> {
        if devices.is_empty() {
            return None;
        }

        if let Some(idx) = devices
            .iter()
            .position(|d| d.is_discrete() && d.is_primary())
        {
            debug!(
                "GpuService: auto-selected primary discrete GPU at index {}",
                idx
            );
            return Some(idx);
        }

        // Prefer the first discrete GPU.
        if let Some(idx) = devices.iter().position(|d| d.is_discrete()) {
            debug!("GpuService: auto-selected discrete GPU at index {}", idx);
            return Some(idx);
        }

        if let Some(idx) = devices.iter().position(|d| d.is_primary()) {
            debug!("GpuService: auto-selected primary GPU at index {}", idx);
            return Some(idx);
        }

        // Fall back to the first GPU.
        debug!("GpuService: no primary/discrete GPU found, defaulting to index 0");
        Some(0)
    }
}

impl Drop for GpuService {
    fn drop(&mut self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

/// `None` is the normal outcome on AMD/Intel-only systems, hence debug logging.
fn init_nvml() -> Option<Nvml> {
    if let Some(path) = std::env::var_os(NVML_PATH_ENV).filter(|p| !p.is_empty()) {
        match Nvml::builder().lib_path(&path).init() {
            Ok(nvml) => {
                debug!("GpuService: NVML loaded from {NVML_PATH_ENV}={path:?}");
                return Some(nvml);
            }
            Err(e) => {
                // Explicit user intent, so warn rather than debug.
                warn!("GpuService: NVML init from {NVML_PATH_ENV}={path:?} failed: {e}");
            }
        }
    }

    match Nvml::init() {
        Ok(nvml) => return Some(nvml),
        Err(e) => debug!("GpuService: NVML init via default search path failed: {e}"),
    }

    for candidate in NVML_LIB_CANDIDATES {
        if !Path::new(candidate).exists() {
            continue;
        }
        match Nvml::builder().lib_path(candidate.as_ref()).init() {
            Ok(nvml) => {
                debug!("GpuService: NVML loaded from {candidate}");
                return Some(nvml);
            }
            Err(e) => debug!("GpuService: NVML init from {candidate} failed: {e}"),
        }
    }

    debug!("GpuService: no usable NVML found (no NVIDIA driver?)");
    None
}

/// Find the first `hwmon/hwmon*` directory under a device path.
fn discover_hwmon(device_path: &Path) -> Option<PathBuf> {
    let hwmon_parent = device_path.join("hwmon");
    if !hwmon_parent.exists() {
        return None;
    }

    let entries = fs::read_dir(&hwmon_parent).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("hwmon") {
                trace!("GpuService: found hwmon at {}", path.to_string_lossy());
                return Some(path);
            }
        }
    }

    None
}

/// Tries `product_name` first (available on some AMD GPUs), then falls back
/// to reading `vendor` + `device` IDs.
fn read_device_name(device_path: &Path) -> Option<String> {
    choose_device_name(
        read_sysfs_string(&device_path.join("product_name")),
        read_udev_model_name(device_path),
        read_sysfs_string(&device_path.join("vendor")),
        read_sysfs_string(&device_path.join("device")),
    )
}

fn read_udev_model_name(device_path: &Path) -> Option<String> {
    let syspath = fs::canonicalize(device_path).ok()?;
    let device = udev::Device::from_syspath(&syspath).ok()?;
    device
        .property_value("ID_MODEL_FROM_DATABASE")
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn choose_device_name(
    product_name: Option<String>,
    udev_model_name: Option<String>,
    vendor_id: Option<String>,
    device_id: Option<String>,
) -> Option<String> {
    if let Some(name) = product_name.filter(|name| !name.trim().is_empty()) {
        return Some(name);
    }

    if let Some(name) = udev_model_name.filter(|name| !name.trim().is_empty()) {
        return Some(name);
    }

    let vendor = vendor_id?;
    let device = device_id?;
    Some(format!(
        "GPU [{}:{}]",
        vendor.trim_start_matches("0x"),
        device.trim_start_matches("0x")
    ))
}

/// Prefer the AMDGPU kernel driver's FUSION flag for integrated/discrete
/// classification and fall back to sysfs files exposed for dedicated AMD GPUs.
fn is_amd_discrete_gpu(device_path: &Path) -> bool {
    match query_amd_fusion_flag(device_path) {
        Some(is_fusion) => !is_fusion,
        None => is_amd_discrete_gpu_sysfs_fallback(device_path),
    }
}

fn query_amd_fusion_flag(device_path: &Path) -> Option<bool> {
    let render_node = render_node_for_device(device_path)?;
    let file = OpenOptions::new().read(true).open(&render_node).ok()?;

    let mut device_info = DrmAmdgpuInfoDeviceIds::default();
    let mut query = DrmAmdgpuInfo {
        return_pointer: (&mut device_info as *mut DrmAmdgpuInfoDeviceIds) as u64,
        return_size: mem::size_of::<DrmAmdgpuInfoDeviceIds>() as u32,
        query: AMDGPU_INFO_DEV_INFO,
        payload: [0; 4],
    };
    let request = linux_iow(
        DRM_COMMAND_BASE + DRM_AMDGPU_INFO,
        mem::size_of::<DrmAmdgpuInfo>(),
    );

    // SAFETY: `file` is an open DRM render node, `query` points to a valid
    // drm_amdgpu_info-compatible struct, and `return_pointer` points to a
    // writable buffer sized by `return_size`.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, &mut query) };
    if result < 0 {
        debug!(
            "GpuService: AMDGPU_INFO_DEV_INFO failed for {:?}",
            render_node,
        );
        return None;
    }

    Some(device_info.ids_flags & AMDGPU_IDS_FLAGS_FUSION != 0)
}

fn render_node_for_device(device_path: &Path) -> Option<PathBuf> {
    let target = fs::canonicalize(device_path).ok()?;
    let entries = fs::read_dir(DRM_CLASS_PATH).ok()?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("renderD") {
            continue;
        }

        let render_device = fs::canonicalize(entry.path().join("device")).ok();
        if render_device.as_ref() == Some(&target) {
            return Some(Path::new(DRI_DEV_PATH).join(name));
        }
    }

    None
}

fn is_amd_discrete_gpu_sysfs_fallback(device_path: &Path) -> bool {
    device_path.join("mem_info_vram_vendor").exists()
        || device_path.join("board_info").exists()
        || device_path.join("unique_id").exists()
}

fn read_sysfs_u32(path: &Path) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

fn read_sysfs_u64(path: &Path) -> Option<u64> {
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<u64>().ok()
}

fn read_sysfs_string(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Read the GPU's PCI runtime power management status from sysfs.
fn read_runtime_status(path: &Path) -> GpuPowerState {
    match fs::read_to_string(path) {
        Ok(content) => match content.trim() {
            "active" => GpuPowerState::Active,
            "suspended" => GpuPowerState::Suspended,
            _ => GpuPowerState::Unknown,
        },
        Err(_) => GpuPowerState::Unknown,
    }
}

fn sysfs_pci_device_path_from_nvml_bus_id(bus_id: &str) -> Option<PathBuf> {
    let trimmed = bus_id.trim().trim_end_matches('\0');
    let (domain_hex, bus_slot) = trimmed.split_once(':')?;
    let domain = u32::from_str_radix(domain_hex, 16).ok()?;
    if domain > u16::MAX as u32 {
        return None;
    }

    Some(PathBuf::from(format!(
        "/sys/bus/pci/devices/{:04x}:{}",
        domain,
        bus_slot.to_lowercase()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn nvml_lib_candidates_are_absolute_sonames() {
        assert!(!NVML_LIB_CANDIDATES.is_empty());
        for candidate in NVML_LIB_CANDIDATES {
            assert!(
                Path::new(candidate).is_absolute(),
                "{candidate} must be absolute so it bypasses the loader search path"
            );
            assert!(
                candidate.ends_with("/libnvidia-ml.so.1"),
                "{candidate} must point at the versioned NVML soname"
            );
        }
    }

    fn device_vram_percent(snapshot: &GpuDeviceSnapshot) -> Option<f32> {
        match (snapshot.vram_used, snapshot.vram_total) {
            (Some(used), Some(total)) if total > 0 => Some(used as f32 / total as f32 * 100.0),
            _ => None,
        }
    }

    #[test]
    fn test_gpu_snapshot_defaults() {
        let snap = GpuSnapshot::default();
        assert!(!snap.available);
        assert!(snap.devices.is_empty());
        assert!(snap.gpu_usage.is_none());
        assert!(snap.vram_used.is_none());
        assert!(snap.vram_total.is_none());
        assert!(snap.temperature.is_none());
        assert!(snap.clock_mhz.is_none());
        assert!(snap.power_watts.is_none());
        assert!(snap.device_name.is_none());
    }

    #[test]
    fn test_is_gpu_high() {
        let mut snap = GpuSnapshot::default();
        assert!(!snap.is_gpu_high());

        snap.gpu_usage = Some(89.0);
        assert!(!snap.is_gpu_high());

        snap.gpu_usage = Some(90.0);
        assert!(snap.is_gpu_high());

        snap.gpu_usage = Some(100.0);
        assert!(snap.is_gpu_high());
    }

    #[test]
    fn test_choose_device_name_prefers_product_name_then_udev_then_ids() {
        assert_eq!(
            choose_device_name(
                Some("AMD Radeon 780M".to_string()),
                Some("Granite Ridge [Radeon Graphics]".to_string()),
                Some("0x1002".to_string()),
                Some("0x13c0".to_string()),
            ),
            Some("AMD Radeon 780M".to_string())
        );

        assert_eq!(
            choose_device_name(
                None,
                Some("Granite Ridge [Radeon Graphics]".to_string()),
                Some("0x1002".to_string()),
                Some("0x13c0".to_string()),
            ),
            Some("Granite Ridge [Radeon Graphics]".to_string())
        );

        assert_eq!(
            choose_device_name(
                None,
                None,
                Some("0x1002".to_string()),
                Some("0x13c0".to_string()),
            ),
            Some("GPU [1002:13c0]".to_string())
        );
    }

    #[test]
    fn test_snapshot_from_devices_uses_first_device_as_summary() {
        let snap = GpuSnapshot::from_devices(vec![
            GpuDeviceSnapshot {
                gpu_usage: Some(75.0),
                device_name: Some("Primary GPU".to_string()),
                ..Default::default()
            },
            GpuDeviceSnapshot {
                gpu_usage: Some(35.0),
                device_name: Some("Secondary GPU".to_string()),
                ..Default::default()
            },
        ]);

        assert!(snap.available);
        assert_eq!(snap.gpu_usage, Some(75.0));
        assert_eq!(snap.device_name.as_deref(), Some("Primary GPU"));
        assert_eq!(snap.devices.len(), 2);
    }

    #[test]
    fn test_is_gpu_high_checks_all_devices() {
        let snap = GpuSnapshot::from_devices(vec![
            GpuDeviceSnapshot {
                gpu_usage: Some(10.0),
                ..Default::default()
            },
            GpuDeviceSnapshot {
                gpu_usage: Some(95.0),
                ..Default::default()
            },
        ]);

        assert!(snap.is_gpu_high());
    }

    #[test]
    fn test_devices_for_display_falls_back_to_summary_fields() {
        let snap = GpuSnapshot {
            available: true,
            power_state: GpuPowerState::Active,
            gpu_usage: Some(64.0),
            device_name: Some("Fallback GPU".to_string()),
            ..Default::default()
        };

        let devices = snap.devices_for_display();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].gpu_usage, Some(64.0));
        assert_eq!(devices[0].device_name.as_deref(), Some("Fallback GPU"));
    }

    /// Helper to create a dummy AMD GPU device for selection tests.
    fn dummy_amd(name: &str, is_discrete: bool, is_primary: bool) -> GpuDevice {
        GpuDevice::Amd(AmdGpuDevice {
            device_path: PathBuf::from("/dev/null"),
            hwmon_path: None,
            runtime_status_path: None,
            device_name: Some(name.to_string()),
            is_discrete,
            is_primary,
        })
    }

    #[test]
    fn test_auto_select_empty() {
        assert_eq!(GpuService::auto_select(&[]), None);
    }

    #[test]
    fn test_auto_select_single_integrated() {
        let devices = vec![dummy_amd("iGPU", false, true)];
        assert_eq!(GpuService::auto_select(&devices), Some(0));
    }

    #[test]
    fn test_auto_select_prefers_discrete() {
        let devices = vec![
            dummy_amd("iGPU", false, true),
            dummy_amd("dGPU", true, false),
        ];
        assert_eq!(GpuService::auto_select(&devices), Some(1));
    }

    #[test]
    fn test_auto_select_prefers_primary_discrete() {
        let devices = vec![
            dummy_amd("secondary-dgpu", true, false),
            dummy_amd("primary-dgpu", true, true),
        ];
        assert_eq!(GpuService::auto_select(&devices), Some(1));
    }

    #[test]
    fn test_parse_display_selection_accepts_strings() {
        assert_eq!(
            GpuService::parse_display_selection_value(
                &toml::Value::String("auto".to_string()),
                "devices"
            ),
            Some(GpuDisplaySelection::Auto)
        );
        assert_eq!(
            GpuService::parse_display_selection_value(
                &toml::Value::String("all".to_string()),
                "devices"
            ),
            Some(GpuDisplaySelection::All)
        );
    }

    #[test]
    fn test_parse_display_selection_accepts_integer_and_array() {
        assert_eq!(
            GpuService::parse_display_selection_value(&toml::Value::Integer(2), "devices"),
            Some(GpuDisplaySelection::Explicit(vec![2]))
        );
        assert_eq!(
            GpuService::parse_display_selection_value(
                &toml::Value::Array(vec![toml::Value::Integer(2), toml::Value::Integer(1)]),
                "devices",
            ),
            Some(GpuDisplaySelection::Explicit(vec![2, 1]))
        );
    }

    #[test]
    fn test_resolve_display_indices_auto_shows_selected_only() {
        let devices = vec![
            dummy_amd("iGPU", false, true),
            dummy_amd("dGPU", true, false),
        ];

        assert_eq!(
            GpuService::resolve_display_indices(&devices, &GpuDisplaySelection::Auto),
            vec![1]
        );
    }

    #[test]
    fn test_resolve_display_indices_all_orders_selected_first() {
        let devices = vec![
            dummy_amd("iGPU", false, true),
            dummy_amd("dGPU", true, false),
            dummy_amd("dGPU-2", true, false),
        ];

        assert_eq!(
            GpuService::resolve_display_indices(&devices, &GpuDisplaySelection::All),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn test_resolve_display_indices_explicit_preserves_order() {
        let devices = vec![
            dummy_amd("gpu-0", false, true),
            dummy_amd("gpu-1", true, false),
            dummy_amd("gpu-2", true, false),
        ];

        assert_eq!(
            GpuService::resolve_display_indices(
                &devices,
                &GpuDisplaySelection::Explicit(vec![2, 1]),
            ),
            vec![2, 1]
        );
    }

    #[test]
    fn test_resolve_display_indices_invalid_list_falls_back_to_auto() {
        let devices = vec![
            dummy_amd("iGPU", false, true),
            dummy_amd("dGPU", true, false),
        ];

        assert_eq!(
            GpuService::resolve_display_indices(&devices, &GpuDisplaySelection::Explicit(vec![9]),),
            vec![1]
        );
    }

    #[test]
    fn test_nvml_bus_id_to_sysfs_path_normalizes_domain() {
        assert_eq!(
            sysfs_pci_device_path_from_nvml_bus_id("00000000:01:00.0"),
            Some(PathBuf::from("/sys/bus/pci/devices/0000:01:00.0"))
        );
        assert_eq!(
            sysfs_pci_device_path_from_nvml_bus_id("00000080:3B:00.0"),
            Some(PathBuf::from("/sys/bus/pci/devices/0080:3b:00.0"))
        );
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vibepanel-{name}-{nanos}"))
    }

    #[test]
    fn test_amd_ioctl_uapi_layout() {
        let request = linux_iow(
            DRM_COMMAND_BASE + DRM_AMDGPU_INFO,
            mem::size_of::<DrmAmdgpuInfo>(),
        );
        assert_eq!(mem::size_of::<DrmAmdgpuInfo>(), 32);
        assert_eq!(request, 0x4020_6445);

        let device_info = DrmAmdgpuInfoDeviceIds::default();
        let base = &device_info as *const _ as usize;
        let ids_flags = &device_info.ids_flags as *const _ as usize;
        assert_eq!(ids_flags - base, 136);
    }

    #[test]
    fn test_amd_discrete_sysfs_fallback_uses_dgpu_markers() {
        let root = unique_test_dir("amd-gpu-detection");
        let igpu = root.join("igpu");
        let dgpu = root.join("dgpu");
        fs::create_dir_all(&igpu).unwrap();
        fs::create_dir_all(&dgpu).unwrap();

        fs::write(dgpu.join("mem_info_vram_vendor"), "samsung\n").unwrap();

        assert!(!is_amd_discrete_gpu_sysfs_fallback(&igpu));
        assert!(is_amd_discrete_gpu_sysfs_fallback(&dgpu));

        fs::remove_dir_all(root).unwrap();
    }
}
