//! GpuService - polling-based GPU resource monitoring via sysfs.
//!
//! This service provides GPU utilization, VRAM usage, temperature, clock speed,
//! and power draw for AMD GPUs by reading sysfs files directly.
//!
//! Currently supports AMD GPUs only (via the `amdgpu` kernel driver).
//! NVIDIA support would require the `nvml-wrapper` crate and is deferred.
//!
//! ## Sysfs Files (under `/sys/class/drm/cardN/device/`)
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
use tracing::{debug, trace, warn};

use super::callbacks::{CallbackId, Callbacks};

/// Default polling interval in seconds.
const DEFAULT_POLL_INTERVAL_SECS: u32 = 3;

/// Threshold above which GPU usage is considered "high".
///
/// Set higher than CPU (80%) because sustained high GPU usage is normal
/// during gaming, rendering, and compute workloads.
pub const GPU_HIGH_USAGE_THRESHOLD: f32 = 90.0;

/// Base path for DRM devices.
const DRM_CLASS_PATH: &str = "/sys/class/drm";

/// Canonical snapshot of GPU state.
#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    /// Whether a supported GPU was detected.
    pub available: bool,

    /// GPU utilization percentage (0.0 - 100.0).
    pub gpu_usage: Option<f32>,

    /// Used VRAM in bytes.
    pub vram_used: Option<u64>,

    /// Total VRAM in bytes.
    pub vram_total: Option<u64>,

    /// GPU temperature in Celsius.
    pub temperature: Option<f32>,

    /// GPU clock speed in MHz.
    pub clock_mhz: Option<u64>,

    /// GPU power draw in watts.
    pub power_watts: Option<f32>,

    /// Device name / product string (e.g., "AMD Radeon RX 7900 XTX").
    pub device_name: Option<String>,
}

impl GpuSnapshot {
    /// Create an initial "unknown" snapshot before first poll.
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

/// Discovered AMD GPU device paths.
struct AmdGpuDevice {
    /// Path to the device directory (e.g., `/sys/class/drm/card1/device`).
    device_path: PathBuf,

    /// Cached hwmon directory path (e.g., `/sys/class/drm/card1/device/hwmon/hwmon3`).
    /// `None` if hwmon was not found (metrics like temp/clock/power won't be available).
    hwmon_path: Option<PathBuf>,

    /// Device name read once at discovery time.
    device_name: Option<String>,
}

/// Shared, process-wide GPU monitoring service.
///
/// This service polls GPU metrics at regular intervals via sysfs and notifies
/// registered callbacks whenever the snapshot updates.
pub struct GpuService {
    /// Current GPU snapshot.
    snapshot: RefCell<GpuSnapshot>,

    /// Registered callbacks for snapshot updates.
    callbacks: Callbacks<GpuSnapshot>,

    /// Timer source for periodic polling.
    timer_source: RefCell<Option<SourceId>>,

    /// Discovered GPU device, if any.
    device: Option<AmdGpuDevice>,

    /// Polling interval in seconds.
    poll_interval: Cell<u32>,
}

impl GpuService {
    /// Create a new GpuService instance.
    fn new() -> Rc<Self> {
        debug!("GpuService: initializing");

        let device = Self::discover_amdgpu();

        if let Some(ref dev) = device {
            debug!(
                "GpuService: found AMD GPU at {:?} (hwmon: {:?}, name: {:?})",
                dev.device_path, dev.hwmon_path, dev.device_name
            );
        } else {
            debug!("GpuService: no AMD GPU found");
        }

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

    /// Get the global GpuService singleton.
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
        // Immediately send current snapshot so widgets can render
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Return the current GPU snapshot.
    pub fn snapshot(&self) -> GpuSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Start the periodic polling timer.
    fn start_polling(this: &Rc<Self>) {
        // Do an initial poll immediately
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

    /// Poll GPU metrics from sysfs and update the snapshot.
    fn poll(&self) {
        let Some(device) = &self.device else {
            return;
        };

        trace!("GpuService: polling GPU metrics");

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

        let snapshot = GpuSnapshot {
            available: true,
            gpu_usage,
            vram_used,
            vram_total,
            temperature,
            clock_mhz,
            power_watts,
            device_name: device.device_name.clone(),
        };

        *self.snapshot.borrow_mut() = snapshot.clone();
        self.callbacks.notify(&snapshot);
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

        // Collect card directories (card0, card1, ...) - skip render nodes
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

            // Check if this card uses the amdgpu driver by resolving the driver symlink
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

/// Read a device name from sysfs.
///
/// Tries `product_name` first (available on some AMD GPUs), then falls back
/// to reading `vendor` + `device` IDs.
fn read_device_name(device_path: &Path) -> Option<String> {
    // Try product_name first (not always available)
    if let Some(name) = read_sysfs_string(&device_path.join("product_name"))
        && !name.is_empty()
    {
        return Some(name);
    }

    // Fallback: read vendor/device PCI IDs
    let vendor = read_sysfs_string(&device_path.join("vendor"))?;
    let device = read_sysfs_string(&device_path.join("device"))?;
    Some(format!(
        "GPU [{}:{}]",
        vendor.trim_start_matches("0x"),
        device.trim_start_matches("0x")
    ))
}

/// Read a sysfs file and parse as `u32`.
fn read_sysfs_u32(path: &Path) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<u32>().ok()
}

/// Read a sysfs file and parse as `u64`.
fn read_sysfs_u64(path: &Path) -> Option<u64> {
    let content = fs::read_to_string(path).ok()?;
    content.trim().parse::<u64>().ok()
}

/// Read a sysfs file as a trimmed string.
fn read_sysfs_string(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Format VRAM bytes to a short human-readable string (e.g., "4.2 GB").
pub fn format_vram(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// Format VRAM bytes to a short bar label (e.g., "4.2G").
#[allow(dead_code)]
pub fn format_vram_short(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}G", b / GB)
    } else {
        format!("{:.0}M", b / MB)
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
    fn test_format_vram() {
        assert_eq!(format_vram(8 * 1024 * 1024 * 1024), "8.0 GB");
        assert_eq!(format_vram(512 * 1024 * 1024), "512 MB");
        assert_eq!(format_vram(1536 * 1024 * 1024), "1.5 GB");
    }

    #[test]
    fn test_format_vram_short() {
        assert_eq!(format_vram_short(8 * 1024 * 1024 * 1024), "8.0G");
        assert_eq!(format_vram_short(512 * 1024 * 1024), "512M");
    }
}
