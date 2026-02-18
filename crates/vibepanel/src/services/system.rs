//! SystemService - shared, polling-based system resource monitoring.
//!
//! This service provides CPU, memory, network, and load average metrics by polling
//! the system at a configurable interval (default: 3 seconds).
//!
//! Uses the `sysinfo` crate for cross-platform system information gathering.
//! The `sysinfo::System` instance is reused across polls for efficiency.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let service = SystemService::global();
//! service.connect(|snapshot| {
//!     println!("CPU: {:.1}%", snapshot.cpu_usage);
//!     println!("Memory: {:.1}%", snapshot.memory_percent);
//! });
//! ```

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib::{self, SourceId};
use sysinfo::{Components, CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
use tracing::{debug, trace};

use super::callbacks::{CallbackId, Callbacks};

/// Default polling interval in seconds.
const DEFAULT_POLL_INTERVAL_SECS: u32 = 3;

/// Threshold above which CPU/memory is considered "high" usage.
pub const HIGH_USAGE_THRESHOLD: f32 = 80.0;

/// Canonical snapshot of system resource state.
#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    /// Whether system information is available.
    pub available: bool,

    // CPU
    /// Global CPU usage percentage (0.0 - 100.0).
    pub cpu_usage: f32,

    /// Per-core CPU usage percentages (0.0 - 100.0 each).
    pub cpu_per_core: Vec<f32>,

    /// Number of physical CPU cores.
    pub cpu_core_count: usize,

    /// CPU/SoC temperature in Celsius, if available.
    pub cpu_temp: Option<f32>,

    // Memory
    /// Used memory in bytes.
    pub memory_used: u64,

    /// Total memory in bytes.
    pub memory_total: u64,

    /// Memory usage percentage (0.0 - 100.0).
    pub memory_percent: f32,

    // Network
    /// Network download speed in bytes/sec (aggregated across all interfaces).
    pub net_download_speed: u64,

    /// Network upload speed in bytes/sec (aggregated across all interfaces).
    pub net_upload_speed: u64,

    // Load Average
    /// System load averages: (1 min, 5 min, 15 min).
    pub load_avg: (f64, f64, f64),
}

impl SystemSnapshot {
    /// Create an initial "unknown" snapshot before first poll.
    ///
    /// This is equivalent to `Default::default()` but more descriptive in intent.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Returns true if CPU usage is above the high threshold.
    pub fn is_cpu_high(&self) -> bool {
        self.cpu_usage >= HIGH_USAGE_THRESHOLD
    }

    /// Returns true if memory usage is above the high threshold.
    pub fn is_memory_high(&self) -> bool {
        self.memory_percent >= HIGH_USAGE_THRESHOLD
    }
}

/// Shared, process-wide system monitoring service.
///
/// This service polls system metrics at regular intervals and notifies
/// registered callbacks whenever the snapshot updates.
pub struct SystemService {
    /// Current system snapshot.
    snapshot: RefCell<SystemSnapshot>,

    /// Registered callbacks for snapshot updates.
    callbacks: Callbacks<SystemSnapshot>,

    /// Timer source for periodic polling.
    timer_source: RefCell<Option<SourceId>>,

    /// Reusable sysinfo System instance.
    sys: RefCell<System>,

    /// Reusable sysinfo Networks instance.
    networks: RefCell<Networks>,

    /// Reusable sysinfo Components instance for temperature sensors.
    components: RefCell<Components>,

    /// Polling interval in seconds.
    poll_interval: Cell<u32>,
}

impl SystemService {
    /// Create a new SystemService instance.
    fn new() -> Rc<Self> {
        debug!("SystemService: initializing");

        // Create System with specific refresh kinds for efficiency
        let sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );

        // Create Networks instance for network monitoring
        let networks = Networks::new_with_refreshed_list();

        // Create Components instance for temperature sensors
        let components = Components::new_with_refreshed_list();

        let service = Rc::new(Self {
            snapshot: RefCell::new(SystemSnapshot::unknown()),
            callbacks: Callbacks::new(),
            timer_source: RefCell::new(None),
            sys: RefCell::new(sys),
            networks: RefCell::new(networks),
            components: RefCell::new(components),
            poll_interval: Cell::new(DEFAULT_POLL_INTERVAL_SECS),
        });

        // Start polling
        Self::start_polling(&service);

        service
    }

    /// Get the global SystemService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<SystemService> = SystemService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the system snapshot changes.
    ///
    /// The callback is immediately invoked with the current snapshot.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&SystemSnapshot) + 'static,
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

    /// Return the current system snapshot.
    pub fn snapshot(&self) -> SystemSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Start the periodic polling timer.
    fn start_polling(this: &Rc<Self>) {
        // Do an initial poll immediately
        this.poll();

        // Schedule periodic polls
        let this_weak = Rc::downgrade(this);
        let interval = this.poll_interval.get();

        debug!("SystemService: starting polling every {}s", interval);

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

    /// Poll system metrics and update the snapshot.
    fn poll(&self) {
        trace!("SystemService: polling system metrics");

        let mut sys = self.sys.borrow_mut();
        let mut networks = self.networks.borrow_mut();
        let mut components = self.components.borrow_mut();

        // Refresh CPU and memory data
        sys.refresh_cpu_all();
        sys.refresh_memory();

        // Refresh network data
        networks.refresh(true);

        // Refresh temperature sensors
        components.refresh(true);

        // Calculate global CPU usage (average of all cores)
        let cpus = sys.cpus();
        let cpu_usage = if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32
        };

        // Per-core usage
        let cpu_per_core: Vec<f32> = cpus.iter().map(|cpu| cpu.cpu_usage()).collect();
        let cpu_core_count = sys.physical_core_count().unwrap_or(cpus.len());

        // CPU temperature - find the most relevant sensor
        // Common labels: "Package id 0", "Tctl", "CPU", "Core 0", "k10temp Tctl", etc.
        let cpu_component = components.iter().find(|c| {
            let label = c.label().to_lowercase();
            label.contains("package")
                || label.contains("tctl")
                || label.contains("cpu")
                || label.contains("core 0")
                || label.contains("soc")
        });
        let cpu_temp = cpu_component.and_then(|c| c.temperature());

        // Memory
        let memory_total = sys.total_memory();
        let memory_used = sys.used_memory();
        let memory_percent = if memory_total > 0 {
            (memory_used as f64 / memory_total as f64 * 100.0) as f32
        } else {
            0.0
        };

        // Network speeds (aggregate across all interfaces)
        // received() and transmitted() return bytes since last refresh
        let poll_interval = self.poll_interval.get() as u64;
        let (net_download, net_upload) =
            networks.iter().fold((0u64, 0u64), |(dl, ul), (_, data)| {
                (dl + data.received(), ul + data.transmitted())
            });
        // Convert to bytes/sec
        let net_download_speed = if poll_interval > 0 {
            net_download / poll_interval
        } else {
            net_download
        };
        let net_upload_speed = if poll_interval > 0 {
            net_upload / poll_interval
        } else {
            net_upload
        };

        // Load average
        let load_avg = System::load_average();
        let load_avg_tuple = (load_avg.one, load_avg.five, load_avg.fifteen);

        // Update snapshot
        let new_snapshot = SystemSnapshot {
            available: true,
            cpu_usage,
            cpu_per_core,
            cpu_core_count,
            cpu_temp,
            memory_used,
            memory_total,
            memory_percent,
            net_download_speed,
            net_upload_speed,
            load_avg: load_avg_tuple,
        };

        // Store and notify
        *self.snapshot.borrow_mut() = new_snapshot;
        self.callbacks.notify(&self.snapshot.borrow());
    }
}

impl Drop for SystemService {
    fn drop(&mut self) {
        // Cancel the timer when the service is dropped
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

/// Format bytes as a human-readable string (e.g., "8.2G", "512M").
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.1}T", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

/// Format bytes as a human-readable string with full unit names.
pub fn format_bytes_long(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format bytes per second as a human-readable speed string (e.g., "1.5 MB/s").
pub fn format_speed(bytes_per_sec: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes_per_sec >= GB {
        format!("{:.1} GB/s", bytes_per_sec as f64 / GB as f64)
    } else if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec as f64 / MB as f64)
    } else if bytes_per_sec >= KB {
        format!("{:.0} KB/s", bytes_per_sec as f64 / KB as f64)
    } else {
        format!("{} B/s", bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(1024), "1K");
        assert_eq!(format_bytes(1024 * 1024), "1M");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0G");
        assert_eq!(
            format_bytes(8 * 1024 * 1024 * 1024 + 200 * 1024 * 1024),
            "8.2G"
        );
    }

    #[test]
    fn test_format_speed() {
        assert_eq!(format_speed(500), "500 B/s");
        assert_eq!(format_speed(1024), "1 KB/s");
        assert_eq!(format_speed(1024 * 1024), "1.0 MB/s");
        assert_eq!(format_speed(1536 * 1024), "1.5 MB/s");
    }

    #[test]
    fn test_snapshot_unknown() {
        let snapshot = SystemSnapshot::unknown();
        assert!(!snapshot.available);
        assert_eq!(snapshot.cpu_usage, 0.0);
        assert_eq!(snapshot.memory_percent, 0.0);
        assert_eq!(snapshot.net_download_speed, 0);
    }

    #[test]
    fn test_high_usage_threshold() {
        let mut snapshot = SystemSnapshot::unknown();
        snapshot.cpu_usage = 79.9;
        assert!(!snapshot.is_cpu_high());

        snapshot.cpu_usage = 80.0;
        assert!(snapshot.is_cpu_high());

        snapshot.memory_percent = 85.0;
        assert!(snapshot.is_memory_high());
    }
}
