//! GpuService - polling-based GPU resource monitoring with multi-GPU support.
//!
//! This service provides GPU utilization, VRAM usage, temperature, clock speed,
//! and power draw by reading vendor-specific interfaces:
//!
//! - **AMD**: sysfs files under `/sys/class/drm/cardN/device/`
//! - **NVIDIA**: NVML via the `nvml-wrapper` crate (runtime-loaded `libnvidia-ml.so`)
//!
//! All GPUs are discovered at startup. One is selected for active polling based on:
//!
//! 1. **Explicit config**: `device = N` in `[widgets.gpu]` selects a specific index.
//! 2. **Auto heuristic** (default): Prefers discrete GPUs over integrated.
//!    AMD discrete detection uses `boot_vga` sysfs (0 = discrete).
//!    NVIDIA GPUs are always treated as discrete.
//!    Falls back to index 0 if no discrete GPU is found.
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
use std::fs;
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
}

impl GpuSnapshot {
    /// Returns a snapshot representing an unknown/unavailable GPU.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Returns true if GPU usage is above the high threshold.
    pub fn is_gpu_high(&self) -> bool {
        self.gpu_usage
            .map(|u| u >= GPU_HIGH_USAGE_THRESHOLD)
            .unwrap_or(false)
    }

    /// VRAM usage as a percentage (0.0 - 100.0), if both used and total are known.
    pub fn vram_percent(&self) -> Option<f32> {
        match (self.vram_used, self.vram_total) {
            (Some(used), Some(total)) if total > 0 => Some(used as f32 / total as f32 * 100.0),
            _ => None,
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

    /// Whether this is a discrete GPU (determined via `boot_vga` sysfs attribute).
    is_discrete: bool,
}

struct NvidiaGpuDevice {
    /// Kept alive for the lifetime of the service; `Device` handles are
    /// re-acquired each poll via `device_by_index` to avoid lifetime complexity.
    nvml: Rc<Nvml>,

    device_index: u32,
    device_name: Option<String>,

    /// Sysfs `power/runtime_status` path for checking hardware power state.
    runtime_status_path: Option<PathBuf>,
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
}

/// Shared, process-wide GPU monitoring service.
///
/// Discovers all available GPUs at startup and polls one selected device
/// at regular intervals via vendor-specific backends (AMD sysfs, NVIDIA NVML).
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

    /// Index into `devices` of the currently polled GPU, or `None` if no GPU.
    selected_index: Cell<Option<usize>>,

    /// Polling interval in seconds.
    poll_interval: Cell<u32>,

    /// Reference count for polling requests. Polling runs only while > 0.
    poll_requests: Cell<u32>,
}

impl GpuService {
    fn new() -> Rc<Self> {
        debug!("GpuService: initializing");

        let devices = Self::discover_all_gpus();
        let device_selection = Self::read_device_config();

        let selected_index = Self::select_gpu(&devices, &device_selection);

        if let Some(idx) = selected_index {
            let method = match device_selection {
                Some(i) => format!("config (device = {})", i),
                None => "auto".to_string(),
            };
            debug!(
                "GpuService: selected GPU {} ({:?}) via {}",
                idx,
                devices[idx].name().unwrap_or("unknown"),
                method,
            );
        } else {
            debug!("GpuService: no GPU selected");
        }

        let initial_snapshot = if selected_index.is_some() {
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
            selected_index: Cell::new(selected_index),
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
        if this.selected_index.get().is_none() {
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
        let Some(idx) = self.selected_index.get() else {
            return;
        };
        let Some(device) = self.devices.get(idx) else {
            return;
        };

        trace!("GpuService: polling GPU {} metrics", idx);

        // Check hardware power state before touching vendor APIs.
        // NVML calls prevent NVIDIA GPUs from entering D3cold sleep.
        let runtime_path = match device {
            GpuDevice::Amd(d) => d.runtime_status_path.as_deref(),
            GpuDevice::Nvidia(d) => d.runtime_status_path.as_deref(),
        };
        let power_state = runtime_path
            .map(read_runtime_status)
            .unwrap_or(GpuPowerState::Unknown);

        if power_state == GpuPowerState::Suspended {
            trace!("GpuService: GPU {} is suspended, skipping vendor poll", idx);
            let snapshot = GpuSnapshot {
                available: true,
                power_state: GpuPowerState::Suspended,
                device_name: device.name().map(str::to_string),
                ..Default::default()
            };
            *self.snapshot.borrow_mut() = snapshot;
            self.callbacks.notify(&self.snapshot.borrow());
            return;
        }

        let mut snapshot = match device {
            GpuDevice::Amd(amd) => Self::poll_amd(amd),
            GpuDevice::Nvidia(nvidia) => Self::poll_nvidia(nvidia),
        };
        snapshot.power_state = power_state;

        *self.snapshot.borrow_mut() = snapshot;
        self.callbacks.notify(&self.snapshot.borrow());
    }

    fn poll_amd(device: &AmdGpuDevice) -> GpuSnapshot {
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

        GpuSnapshot {
            available: true,
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

    fn poll_nvidia(nvidia: &NvidiaGpuDevice) -> GpuSnapshot {
        let device = match nvidia.nvml.device_by_index(nvidia.device_index) {
            Ok(d) => d,
            Err(e) => {
                warn!("GpuService: failed to acquire NVIDIA device handle: {e}");
                return GpuSnapshot {
                    available: true,
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

        GpuSnapshot {
            available: true,
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

                    // boot_vga: 1 = primary (typically integrated), 0 = secondary (typically discrete).
                    let boot_vga = read_sysfs_u32(&device_path.join("boot_vga"));
                    let is_discrete = boot_vga.map(|v| v == 0).unwrap_or(false);

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
                    });
                }
            }
        }

        devices
    }

    /// Discover all NVIDIA GPUs via NVML (runtime-loads `libnvidia-ml.so`).
    fn discover_all_nvidia() -> Vec<NvidiaGpuDevice> {
        let nvml = match Nvml::init() {
            Ok(n) => Rc::new(n),
            Err(e) => {
                debug!("GpuService: NVML init failed (no NVIDIA driver?): {e}");
                return Vec::new();
            }
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

            // Get PCI bus ID for runtime_status path.
            // NVML uses an 8-char domain ("00000000:06:00.0") while sysfs uses
            // 4-char ("0000:06:00.0"). Normalize by stripping leading zeros and
            // re-adding a 4-char prefix.
            let runtime_status_path = device
                .pci_info()
                .ok()
                .map(|pci| {
                    let bus_id = pci.bus_id.trim_start_matches('0');
                    let bus_id = format!("0000{bus_id}").to_lowercase();
                    PathBuf::from(format!(
                        "/sys/bus/pci/devices/{bus_id}/power/runtime_status",
                    ))
                })
                .filter(|p| p.exists());

            debug!(
                "GpuService: found NVIDIA GPU {:?} (nvml_index: {})",
                device_name, device_index,
            );

            devices.push(NvidiaGpuDevice {
                nvml: nvml.clone(),
                device_index,
                device_name,
                runtime_status_path,
            });
        }

        devices
    }

    /// Read the `device` config option from `[widgets.gpu]`.
    fn read_device_config() -> Option<u32> {
        ConfigManager::global()
            .get_widget_option("gpu", "device")
            .and_then(|v| match v {
                toml::Value::Integer(i) if i >= 0 => Some(i as u32),
                toml::Value::String(ref s) if s == "auto" => None,
                other => {
                    warn!("GpuService: invalid 'device' config value: {other}, using auto");
                    None
                }
            })
    }

    /// Select GPU: explicit config index > first discrete > index 0.
    fn select_gpu(devices: &[GpuDevice], selection: &Option<u32>) -> Option<usize> {
        if devices.is_empty() {
            return None;
        }

        if let Some(i) = selection {
            let idx = *i as usize;
            if idx < devices.len() {
                return Some(idx);
            }
            warn!(
                "GpuService: configured device index {} out of range (have {} GPU(s)), falling back to auto",
                i,
                devices.len(),
            );
        }

        Self::auto_select(devices)
    }

    /// Auto-select a GPU: prefer discrete, then index 0.
    fn auto_select(devices: &[GpuDevice]) -> Option<usize> {
        if devices.is_empty() {
            return None;
        }

        // Prefer the first discrete GPU.
        if let Some(idx) = devices.iter().position(|d| d.is_discrete()) {
            debug!("GpuService: auto-selected discrete GPU at index {}", idx);
            return Some(idx);
        }

        // Fall back to the first GPU.
        debug!("GpuService: no discrete GPU found, defaulting to index 0");
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
    if let Some(name) = read_sysfs_string(&device_path.join("product_name")) {
        return Some(name);
    }

    let vendor = read_sysfs_string(&device_path.join("vendor"))?;
    let device = read_sysfs_string(&device_path.join("device"))?;
    Some(format!(
        "GPU [{}:{}]",
        vendor.trim_start_matches("0x"),
        device.trim_start_matches("0x")
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_snapshot_defaults() {
        let snap = GpuSnapshot::default();
        assert!(!snap.available);
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
    fn test_vram_percent() {
        let mut snap = GpuSnapshot::default();
        assert!(snap.vram_percent().is_none());

        snap.vram_used = Some(4 * 1024 * 1024 * 1024); // 4 GB
        snap.vram_total = Some(8 * 1024 * 1024 * 1024); // 8 GB
        let pct = snap.vram_percent().unwrap();
        assert!((pct - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_vram_percent_zero_total() {
        let snap = GpuSnapshot {
            vram_used: Some(0),
            vram_total: Some(0),
            ..Default::default()
        };
        assert!(snap.vram_percent().is_none());
    }

    /// Helper to create a dummy AMD GPU device for selection tests.
    fn dummy_amd(name: &str, is_discrete: bool) -> GpuDevice {
        GpuDevice::Amd(AmdGpuDevice {
            device_path: PathBuf::from("/dev/null"),
            hwmon_path: None,
            runtime_status_path: None,
            device_name: Some(name.to_string()),
            is_discrete,
        })
    }

    #[test]
    fn test_auto_select_empty() {
        assert_eq!(GpuService::auto_select(&[]), None);
    }

    #[test]
    fn test_auto_select_single_integrated() {
        let devices = vec![dummy_amd("iGPU", false)];
        assert_eq!(GpuService::auto_select(&devices), Some(0));
    }

    #[test]
    fn test_auto_select_prefers_discrete() {
        let devices = vec![dummy_amd("iGPU", false), dummy_amd("dGPU", true)];
        assert_eq!(GpuService::auto_select(&devices), Some(1));
    }

    #[test]
    fn test_select_gpu_explicit_index() {
        let devices = vec![dummy_amd("iGPU", false), dummy_amd("dGPU", true)];
        // Explicit config overrides auto-selection.
        assert_eq!(GpuService::select_gpu(&devices, &Some(0)), Some(0));
    }

    #[test]
    fn test_select_gpu_out_of_range_falls_back() {
        let devices = vec![dummy_amd("dGPU", true)];
        // Out-of-range index falls back to auto (which picks discrete at 0).
        assert_eq!(GpuService::select_gpu(&devices, &Some(5)), Some(0));
    }

    #[test]
    fn test_select_gpu_none_config_uses_auto() {
        let devices = vec![dummy_amd("iGPU", false), dummy_amd("dGPU", true)];
        assert_eq!(GpuService::select_gpu(&devices, &None), Some(1));
    }
}
