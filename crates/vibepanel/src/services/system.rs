//! SystemService - shared, polling-based system resource monitoring.
//!
//! This service provides CPU, memory, network, and disk I/O metrics by polling
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib::{self, SourceId};
use sysinfo::{Components, CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
use tracing::{debug, trace};

use super::callbacks::{CallbackId, Callbacks};
use super::sleep_watcher::SleepWatcher;

/// Default polling interval in seconds.
const DEFAULT_POLL_INTERVAL_SECS: u32 = 3;

/// Number of samples retained for system popover history graphs.
pub const SYSTEM_HISTORY_SAMPLES: usize = 60;

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

    // Disk I/O
    /// Aggregate physical disk read speed in bytes/sec.
    pub disk_read_speed: u64,

    /// Aggregate physical disk write speed in bytes/sec.
    pub disk_write_speed: u64,
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

/// One synchronized CPU, memory, network, and disk history sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemHistorySample {
    pub cpu_usage: Option<f32>,
    pub memory_percent: Option<f32>,
    pub net_download_speed: Option<u64>,
    pub net_upload_speed: Option<u64>,
    pub disk_read_speed: Option<u64>,
    pub disk_write_speed: Option<u64>,
}

impl SystemHistorySample {
    fn from_snapshot(snapshot: &SystemSnapshot) -> Self {
        if snapshot.available {
            Self {
                cpu_usage: Some(snapshot.cpu_usage),
                memory_percent: Some(snapshot.memory_percent),
                net_download_speed: Some(snapshot.net_download_speed),
                net_upload_speed: Some(snapshot.net_upload_speed),
                disk_read_speed: Some(snapshot.disk_read_speed),
                disk_write_speed: Some(snapshot.disk_write_speed),
            }
        } else {
            Self::default()
        }
    }

    fn is_gap(&self) -> bool {
        self.cpu_usage.is_none()
    }
}

fn push_history_sample(history: &mut VecDeque<SystemHistorySample>, sample: SystemHistorySample) {
    if history.len() == SYSTEM_HISTORY_SAMPLES {
        history.pop_front();
    }
    history.push_back(sample);
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> u64 {
    if elapsed.is_zero() {
        return 0;
    }
    (bytes as f64 / elapsed.as_secs_f64()) as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DiskCounters {
    sectors_read: u64,
    sectors_written: u64,
}

fn physical_disk_names() -> HashSet<String> {
    fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            fs::read_dir(entry.path().join("slaves"))
                .is_ok_and(|mut slaves| slaves.next().is_none())
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            !name.starts_with("loop") && !name.starts_with("ram") && !name.starts_with("zram")
        })
        .collect()
}

fn parse_diskstats(input: &str, devices: &HashSet<String>) -> HashMap<String, DiskCounters> {
    input
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let name = *fields.get(2)?;
            if !devices.contains(name) {
                return None;
            }
            Some((
                name.to_string(),
                DiskCounters {
                    sectors_read: fields.get(5)?.parse().ok()?,
                    sectors_written: fields.get(9)?.parse().ok()?,
                },
            ))
        })
        .collect()
}

fn disk_delta_bytes(
    previous: &HashMap<String, DiskCounters>,
    current: &HashMap<String, DiskCounters>,
) -> (u64, u64) {
    current
        .iter()
        .filter_map(|(name, counters)| previous.get(name).map(|old| (old, counters)))
        .fold((0u64, 0u64), |(read, write), (old, current)| {
            let read_delta = current
                .sectors_read
                .saturating_sub(old.sectors_read)
                .saturating_mul(512);
            let write_delta = current
                .sectors_written
                .saturating_sub(old.sectors_written)
                .saturating_mul(512);
            (
                read.saturating_add(read_delta),
                write.saturating_add(write_delta),
            )
        })
}

/// Shared, process-wide system monitoring service.
///
/// This service polls system metrics at regular intervals and notifies
/// registered callbacks whenever the snapshot updates.
pub struct SystemService {
    /// Current system snapshot.
    snapshot: RefCell<SystemSnapshot>,

    /// Synchronized history used by system popover graphs.
    history: RefCell<VecDeque<SystemHistorySample>>,

    /// Last poll time used for rate calculation and stale-history detection.
    last_poll_at: Cell<Option<Instant>>,

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

    /// Previous cumulative disk counters used to calculate throughput.
    disk_counters: RefCell<HashMap<String, DiskCounters>>,

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
            history: RefCell::new(VecDeque::with_capacity(SYSTEM_HISTORY_SAMPLES)),
            last_poll_at: Cell::new(None),
            callbacks: Callbacks::new(),
            timer_source: RefCell::new(None),
            sys: RefCell::new(sys),
            networks: RefCell::new(networks),
            components: RefCell::new(components),
            disk_counters: RefCell::new(HashMap::new()),
            poll_interval: Cell::new(DEFAULT_POLL_INTERVAL_SECS),
        });

        let weak = Rc::downgrade(&service);
        let _resume_callback_id = SleepWatcher::global().on_resume(move || {
            if let Some(service) = weak.upgrade() {
                service.record_history_break();
                service.last_poll_at.set(None);
                service.disk_counters.borrow_mut().clear();
                service.networks.borrow_mut().refresh(true);
            }
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

    /// Return retained system history, oldest sample first.
    pub fn history(&self) -> Vec<SystemHistorySample> {
        self.history.borrow().iter().copied().collect()
    }

    /// Insert a discontinuity so graphs do not connect samples across it.
    fn record_history_break(&self) {
        let mut history = self.history.borrow_mut();
        if !history.back().is_some_and(SystemHistorySample::is_gap) {
            push_history_sample(&mut history, SystemHistorySample::default());
        }
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

        let now = Instant::now();
        let previous_poll = self.last_poll_at.replace(Some(now));
        let elapsed = previous_poll
            .map(|previous| now.duration_since(previous))
            .unwrap_or_else(|| Duration::from_secs(u64::from(self.poll_interval.get())));

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
        let (net_download, net_upload) =
            networks.iter().fold((0u64, 0u64), |(dl, ul), (_, data)| {
                (dl + data.received(), ul + data.transmitted())
            });
        let net_download_speed = bytes_per_second(net_download, elapsed);
        let net_upload_speed = bytes_per_second(net_upload, elapsed);

        let current_disk_counters = fs::read_to_string("/proc/diskstats")
            .map(|input| parse_diskstats(&input, &physical_disk_names()))
            .unwrap_or_default();
        let (disk_read, disk_write) =
            disk_delta_bytes(&self.disk_counters.borrow(), &current_disk_counters);
        *self.disk_counters.borrow_mut() = current_disk_counters;
        let disk_read_speed = bytes_per_second(disk_read, elapsed);
        let disk_write_speed = bytes_per_second(disk_write, elapsed);

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
            disk_read_speed,
            disk_write_speed,
        };

        // Do not connect samples across a prolonged stall in the main loop.
        // Suspend is handled separately because Instant does not advance during it.
        let history_window = Duration::from_secs(
            u64::from(self.poll_interval.get()) * SYSTEM_HISTORY_SAMPLES as u64,
        );
        if elapsed > history_window {
            self.record_history_break();
        }
        push_history_sample(
            &mut self.history.borrow_mut(),
            SystemHistorySample::from_snapshot(&new_snapshot),
        );

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
///
/// Always uses KB/s as the minimum unit (e.g., 500 B/s → "0.5 KB/s") so that
/// all outputs share a uniform `N.N UNIT/s` structure, preventing visual jitter
/// when displayed in fixed-width bar widgets.
pub fn format_speed(bytes_per_sec: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes_per_sec >= GB {
        format!("{:.1} GB/s", bytes_per_sec as f64 / GB as f64)
    } else if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec as f64 / MB as f64)
    } else {
        format!("{:.1} KB/s", bytes_per_sec as f64 / KB as f64)
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
        assert_eq!(format_speed(0), "0.0 KB/s");
        assert_eq!(format_speed(500), "0.5 KB/s");
        assert_eq!(format_speed(1024), "1.0 KB/s");
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

    #[test]
    fn test_history_sample_uses_synchronized_snapshot_values() {
        let snapshot = SystemSnapshot {
            available: true,
            cpu_usage: 42.0,
            memory_percent: 63.0,
            net_download_speed: 2048,
            net_upload_speed: 1024,
            disk_read_speed: 4096,
            disk_write_speed: 512,
            ..Default::default()
        };

        assert_eq!(
            SystemHistorySample::from_snapshot(&snapshot),
            SystemHistorySample {
                cpu_usage: Some(42.0),
                memory_percent: Some(63.0),
                net_download_speed: Some(2048),
                net_upload_speed: Some(1024),
                disk_read_speed: Some(4096),
                disk_write_speed: Some(512),
            }
        );
    }

    #[test]
    fn test_history_discards_oldest_samples_beyond_the_cap() {
        let mut history = VecDeque::new();
        for value in 0..=SYSTEM_HISTORY_SAMPLES {
            push_history_sample(
                &mut history,
                SystemHistorySample {
                    net_download_speed: Some(value as u64),
                    ..Default::default()
                },
            );
        }

        assert_eq!(history.len(), SYSTEM_HISTORY_SAMPLES);
        assert_eq!(
            history.front().and_then(|sample| sample.net_download_speed),
            Some(1)
        );
    }

    #[test]
    fn test_rate_uses_actual_elapsed_time() {
        assert_eq!(bytes_per_second(3_000, Duration::from_secs(3)), 1_000);
        assert_eq!(bytes_per_second(3_000, Duration::ZERO), 0);
    }

    #[test]
    fn test_diskstats_parses_only_selected_devices() {
        let devices = HashSet::from(["nvme0n1".to_string()]);
        let counters = parse_diskstats(
            "259 0 nvme0n1 10 0 20 0 30 0 40 0 0 0 0 0 0 0 0\n\
             259 1 nvme0n1p1 5 0 8 0 7 0 9 0 0 0 0 0 0 0 0",
            &devices,
        );

        assert_eq!(
            counters.get("nvme0n1"),
            Some(&DiskCounters {
                sectors_read: 20,
                sectors_written: 40,
            })
        );
        assert!(!counters.contains_key("nvme0n1p1"));
    }

    #[test]
    fn test_disk_delta_aggregates_devices_and_handles_resets() {
        let previous = HashMap::from([
            (
                "sda".to_string(),
                DiskCounters {
                    sectors_read: 10,
                    sectors_written: 20,
                },
            ),
            (
                "sdb".to_string(),
                DiskCounters {
                    sectors_read: 20,
                    sectors_written: 40,
                },
            ),
        ]);
        let current = HashMap::from([
            (
                "sda".to_string(),
                DiskCounters {
                    sectors_read: 14,
                    sectors_written: 26,
                },
            ),
            (
                "sdb".to_string(),
                DiskCounters {
                    sectors_read: 1,
                    sectors_written: 2,
                },
            ),
        ]);

        assert_eq!(disk_delta_bytes(&previous, &current), (4 * 512, 6 * 512));
    }
}
