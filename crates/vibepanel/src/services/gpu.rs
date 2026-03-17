//! GpuService - polling-based GPU resource monitoring.
//!
//! This service provides GPU utilization, VRAM usage, temperature, clock speed,
//! and power draw by reading vendor-specific interfaces:
//!
//! - **AMD**: sysfs files under `/sys/class/drm/cardN/device/`
//! - **NVIDIA**: NVML via the `nvml-wrapper` crate (runtime-loaded `libnvidia-ml.so`)
//!
//! Only the first detected GPU is monitored (AMD checked first, then NVIDIA).
//!
//! ## AMD Sysfs Files (under `/sys/class/drm/cardN/device/`)
//!
//! | File                          | Metric              | Parse            |
//! |-------------------------------|---------------------|------------------|
//! | `gpu_busy_percent`            | GPU utilization     | u32 → f32        |
//! | `mem_info_vram_total`         | Total VRAM (bytes)  | u64              |
//! | `mem_info_vram_used`          | Used VRAM (bytes)   | u64              |
//! | `hwmon/hwmon*/temp1_input`    | Temperature (m°C)   | u32 / 1000 → f32 |
//! | `hwmon/hwmon*/freq1_input`    | GPU clock (Hz)      | u64 / 1e6 → MHz  |
//! | `hwmon/hwmon*/power1_average` | Power draw (µW)     | u64 / 1e6 → W    |
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

const DEFAULT_POLL_INTERVAL_SECS: u32 = 3;

/// Threshold above which GPU usage is considered "high".
///
/// Set higher than CPU (80%) because sustained high GPU usage is normal
/// during gaming, rendering, and compute workloads.
pub(crate) const GPU_HIGH_USAGE_THRESHOLD: f32 = 90.0;

const DRM_CLASS_PATH: &str = "/sys/class/drm";

#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    pub available: bool,
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

    device_name: Option<String>,
}

struct NvidiaGpuDevice {
    /// Kept alive for the lifetime of the service; `Device` handles are
    /// re-acquired each poll via `device_by_index` to avoid lifetime complexity.
    nvml: Nvml,

    device_index: u32,
    device_name: Option<String>,
}

enum GpuDevice {
    Amd(AmdGpuDevice),
    Nvidia(Box<NvidiaGpuDevice>), // boxed to keep enum size small (Nvml is ~11KB)
}

/// Shared, process-wide GPU monitoring service.
///
/// Polls GPU metrics at regular intervals via vendor-specific backends
/// (AMD sysfs, NVIDIA NVML) and notifies registered callbacks whenever
/// the snapshot updates.
pub struct GpuService {
    snapshot: RefCell<GpuSnapshot>,
    callbacks: Callbacks<GpuSnapshot>,

    /// Timer source for periodic polling.
    timer_source: RefCell<Option<SourceId>>,

    device: Option<GpuDevice>,

    /// Polling interval in seconds.
    poll_interval: Cell<u32>,
}

impl GpuService {
    fn new() -> Rc<Self> {
        debug!("GpuService: initializing");

        let device = Self::discover_gpu();

        let initial_snapshot = if device.is_some() {
            GpuSnapshot {
                available: true,
                ..Default::default()
            }
        } else {
            GpuSnapshot::unknown()
        };

        let service = Rc::new(Self {
            snapshot: RefCell::new(initial_snapshot),
            callbacks: Callbacks::new(),
            timer_source: RefCell::new(None),
            device,
            poll_interval: Cell::new(DEFAULT_POLL_INTERVAL_SECS),
        });

        if service.device.is_some() {
            Self::start_polling(&service);
        }

        service
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

    fn poll(&self) {
        let Some(device) = &self.device else {
            return;
        };

        trace!("GpuService: polling GPU metrics");

        let snapshot = match device {
            GpuDevice::Amd(amd) => Self::poll_amd(amd),
            GpuDevice::Nvidia(nvidia) => Self::poll_nvidia(nvidia),
        };

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
        }
    }

    fn discover_gpu() -> Option<GpuDevice> {
        if let Some(amd) = Self::discover_amdgpu() {
            debug!(
                "GpuService: found AMD GPU at {:?} (hwmon: {:?}, name: {:?})",
                amd.device_path, amd.hwmon_path, amd.device_name
            );
            return Some(GpuDevice::Amd(amd));
        }

        if let Some(nvidia) = Self::discover_nvidia() {
            debug!(
                "GpuService: found NVIDIA GPU (index: {}, name: {:?})",
                nvidia.device_index, nvidia.device_name
            );
            return Some(GpuDevice::Nvidia(Box::new(nvidia)));
        }

        debug!("GpuService: no supported GPU found");
        None
    }

    /// Discover the first AMD GPU by scanning `/sys/class/drm/card*`.
    ///
    /// Checks for the `amdgpu` driver by reading the `driver` symlink under
    /// each card's `device/` directory.
    fn discover_amdgpu() -> Option<AmdGpuDevice> {
        let drm_path = Path::new(DRM_CLASS_PATH);
        if !drm_path.exists() {
            debug!("GpuService: {} does not exist", DRM_CLASS_PATH);
            return None;
        }

        let entries = match fs::read_dir(drm_path) {
            Ok(it) => it,
            Err(err) => {
                warn!("GpuService: failed to read {}: {err}", DRM_CLASS_PATH);
                return None;
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

                    return Some(AmdGpuDevice {
                        device_path,
                        hwmon_path,
                        device_name,
                    });
                }
            }
        }

        None
    }

    /// Discover an NVIDIA GPU via NVML.
    ///
    /// `Nvml::init()` runtime-loads `libnvidia-ml.so`. If the library isn't
    /// present (no NVIDIA driver installed), this returns `None` gracefully.
    fn discover_nvidia() -> Option<NvidiaGpuDevice> {
        let nvml = match Nvml::init() {
            Ok(n) => n,
            Err(e) => {
                debug!("GpuService: NVML init failed (no NVIDIA driver?): {e}");
                return None;
            }
        };

        let count = match nvml.device_count() {
            Ok(0) => {
                debug!("GpuService: NVML reports 0 devices");
                return None;
            }
            Ok(c) => c,
            Err(e) => {
                warn!("GpuService: NVML device_count failed: {e}");
                return None;
            }
        };

        let device_index = 0;
        let device_name = match nvml.device_by_index(device_index) {
            Ok(dev) => dev.name().ok(),
            Err(e) => {
                warn!("GpuService: NVML device_by_index({device_index}) failed: {e}");
                return None;
            }
        };

        debug!("GpuService: NVML found {count} device(s), using index {device_index}");

        Some(NvidiaGpuDevice {
            nvml,
            device_index,
            device_name,
        })
    }
}

impl Drop for GpuService {
    fn drop(&mut self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

/// Discover the hwmon directory under a device path.
///
/// The hwmon numbering (hwmon0, hwmon1, ...) is unstable across reboots,
/// so we glob for any `hwmon/hwmon*` directory and use the first one found.
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
                debug!("GpuService: discovered hwmon at {}", path.to_string_lossy());
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
}
