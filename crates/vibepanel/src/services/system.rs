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
use std::ffi::OsStr;
use std::fs;
use std::hash::Hash;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gio;
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

/// Threshold above which a filesystem is considered nearly full.
pub const DISK_HIGH_THRESHOLD: f32 = 90.0;

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

    /// Mounted block-device filesystems, sorted by mount point.
    pub mounts: Vec<MountUsage>,
}

/// One mounted block-device filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountUsage {
    /// Device name, e.g. `nvme0n1p2` or `sdb1`; the mapper name (e.g.
    /// `luks-root`) for device-mapper nodes.
    pub device: String,
    pub mount_point: String,
    pub fs_type: String,
    pub removable: bool,
    /// `None` if its usage could not be read (e.g. the query is hung).
    pub usage: Option<SpaceUsage>,
}

/// Space usage of a filesystem, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpaceUsage {
    pub total: u64,
    pub used: u64,
    /// Space available to unprivileged users.
    pub available: u64,
}

impl SpaceUsage {
    /// Usage percentage as reported by `df` (reserved blocks count as used).
    pub fn percent(&self) -> f32 {
        let usable = self.used + self.available;
        if usable == 0 {
            0.0
        } else {
            (self.used as f64 / usable as f64 * 100.0) as f32
        }
    }
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

/// Decode the octal escapes (`\040` etc.) used in `/proc/self/mounts`.
fn unescape_mount_field(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && let Some(value) = field
                .get(i + 1..i + 4)
                .and_then(|octal| std::str::from_utf8(octal).ok())
                .and_then(|octal| u8::from_str_radix(octal, 8).ok())
        {
            out.push(value);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    out
}

/// A parsed mount record: `(source, mount_point, fs_type)`. Paths stay raw
/// bytes because the kernel does not escape non-UTF-8 bytes, and `statvfs`
/// needs the exact path.
type MountRecord = (Vec<u8>, Vec<u8>, String);

/// Parse `/proc/self/mounts` into block-device mounts, one entry per device
/// at its shortest mount point, sorted by mount point.
///
/// Only `/dev/` sources are kept, so network and virtual filesystems whose
/// `statvfs` could hang or that report no real capacity are never queried.
fn parse_mounts(input: &[u8]) -> Vec<MountRecord> {
    let mut by_device: HashMap<Vec<u8>, (Vec<u8>, String)> = HashMap::new();
    for line in input.split(|&byte| byte == b'\n') {
        let mut fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let (Some(source), Some(mount_point), Some(fs_type)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let fs_type = String::from_utf8_lossy(fs_type).into_owned();
        if !source.starts_with(b"/dev/") || matches!(fs_type.as_str(), "squashfs" | "erofs") {
            continue;
        }
        let mount_point = unescape_mount_field(mount_point);
        let entry = by_device
            .entry(unescape_mount_field(source))
            .or_insert_with(|| (mount_point.clone(), fs_type));
        if mount_point.len() < entry.0.len() {
            entry.0 = mount_point;
        }
    }
    let mut mounts: Vec<_> = by_device
        .into_iter()
        .map(|(source, (mount_point, fs_type))| (source, mount_point, fs_type))
        .collect();
    mounts.sort_by(|a, b| a.1.cmp(&b.1));
    mounts
}

/// Whether a block device's sysfs path belongs to a removable drive: anything
/// on USB, or an SD card (whose MMC card device reports `SD`; eMMC reports `MMC`).
fn is_removable_block(sys_path: &Path, read_type: impl Fn(&Path) -> Option<String>) -> bool {
    let is_usb = sys_path
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with("usb"));
    is_usb
        || sys_path
            .ancestors()
            .find(|dir| {
                dir.file_name()
                    .map(|name| name.to_string_lossy())
                    .is_some_and(|name| name.starts_with("mmc") && name.contains(':'))
            })
            .and_then(|card| read_type(&card.join("type")))
            .is_some_and(|kind| kind.trim() == "SD")
}

/// Name to show for a kernel block device: the mapper name for device-mapper
/// nodes (`dm-0` -> `luks-root`, `vg-home`), otherwise the kernel name.
fn display_device(device: String, read: impl Fn(&Path) -> Option<String>) -> String {
    if !device.starts_with("dm-") {
        return device;
    }
    read(&Path::new("/sys/class/block").join(&device).join("dm/name"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or(device)
}

/// Find mounted block devices without touching their filesystems, so this
/// cannot block on a hung mount. Keyed by raw mount point.
fn discover_mounts() -> Vec<(Vec<u8>, MountUsage)> {
    let input = fs::read("/proc/self/mounts").unwrap_or_default();
    parse_mounts(&input)
        .into_iter()
        .map(|(source, mount_point, fs_type)| {
            let device = fs::canonicalize(OsStr::from_bytes(&source))
                .ok()
                .and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
                .unwrap_or_else(|| String::from_utf8_lossy(&source).into_owned());
            let removable = fs::canonicalize(format!("/sys/class/block/{device}"))
                .is_ok_and(|path| is_removable_block(&path, |file| fs::read_to_string(file).ok()));
            let mount = MountUsage {
                device: display_device(device, |file| fs::read_to_string(file).ok()),
                mount_point: String::from_utf8_lossy(&mount_point).into_owned(),
                fs_type,
                removable,
                usage: None,
            };
            (mount_point, mount)
        })
        .collect()
}

/// How long a filesystem query may run before its result is shown as unavailable.
const STORAGE_STALE_AFTER: Duration = Duration::from_secs(10);

/// One watched filesystem: its watcher count, when its running query started,
/// and its last result (`None` until the first query finishes).
#[derive(Debug, Default)]
struct WatchedPath {
    watchers: usize,
    started: Option<Instant>,
    usage: Option<Option<SpaceUsage>>,
}

/// Filesystems queried with `statvfs`, which can block on a hung mount
/// (NFS, flaky USB, FUSE). Each one is queried on its own thread, at most one
/// at a time, so a hung filesystem only affects itself. An unwatched entry
/// whose query is still running is kept with zero watchers until it returns,
/// so watching it again cannot start a second blocked query.
///
/// Known limitation: entries are keyed by path, so if a hung filesystem is
/// replaced by another mount at the same path, the path stays unavailable
/// until the old query returns (or the panel restarts).
#[derive(Debug)]
struct WatchedPaths<K>(HashMap<K, WatchedPath>);

impl<K> Default for WatchedPaths<K> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<K: Eq + Hash + Clone> WatchedPaths<K> {
    fn unwatch(&mut self, key: &K) {
        let Some(entry) = self.0.get_mut(key) else {
            return;
        };
        entry.watchers = entry.watchers.saturating_sub(1);
        if entry.watchers == 0 && entry.started.is_none() {
            self.0.remove(key);
        }
    }

    /// Watch exactly `keys`, once each.
    fn sync(&mut self, keys: &HashSet<K>) {
        for key in keys {
            self.0.entry(key.clone()).or_default().watchers = 1;
        }
        let gone: Vec<K> = self
            .0
            .iter()
            .filter(|(key, entry)| entry.watchers > 0 && !keys.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            self.unwatch(&key);
        }
    }

    /// Claim every watched entry with no query running.
    fn claim(&mut self, now: Instant) -> Vec<K> {
        self.0
            .iter_mut()
            .filter(|(_, entry)| entry.watchers > 0 && entry.started.is_none())
            .map(|(key, entry)| {
                entry.started = Some(now);
                key.clone()
            })
            .collect()
    }

    /// Record a finished query; the result is dropped if no longer watched.
    /// Returns true for an entry's first result.
    fn finish(&mut self, key: &K, usage: Option<SpaceUsage>) -> bool {
        let Some(entry) = self.0.get_mut(key) else {
            return false;
        };
        entry.started = None;
        if entry.watchers == 0 {
            self.0.remove(key);
            false
        } else {
            entry.usage.replace(usage).is_none()
        }
    }

    /// Usage as shown: `None` while not watched or before the first result,
    /// `Some(None)` if unavailable or its query has been stuck too long.
    fn get(&self, key: &K, now: Instant) -> Option<Option<SpaceUsage>> {
        let entry = self.0.get(key).filter(|entry| entry.watchers > 0)?;
        let stuck = entry
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= STORAGE_STALE_AFTER);
        if stuck { Some(None) } else { entry.usage }
    }
}

/// Mounts as shown in the snapshot. Mounts without a result yet and mounts
/// reporting no capacity are left out.
fn published_storage(
    drives: &[(Vec<u8>, MountUsage)],
    mount_usage: &WatchedPaths<Vec<u8>>,
    now: Instant,
) -> Vec<MountUsage> {
    drives
        .iter()
        .filter_map(|(key, mount)| {
            let usage = mount_usage.get(key, now)?;
            (usage.is_none_or(|usage| usage.total > 0)).then(|| MountUsage {
                usage,
                ..mount.clone()
            })
        })
        .collect()
}

/// Return space usage of the filesystem holding `path`.
fn statvfs_usage(path: &[u8]) -> Option<SpaceUsage> {
    let path = std::ffi::CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid C string and `stat` is written on success.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned success, so the struct is initialized.
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize;
    let total = stat.f_blocks * block;
    let free = stat.f_bfree * block;
    Some(SpaceUsage {
        total,
        used: total.saturating_sub(free),
        available: stat.f_bavail * block,
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

    /// Mounted drives from the last discovery.
    drives: RefCell<Vec<(Vec<u8>, MountUsage)>>,

    /// Usage of each discovered drive, queried independently.
    mount_usage: RefCell<WatchedPaths<Vec<u8>>>,

    /// A mount discovery is running.
    discovering: Cell<bool>,
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
            drives: RefCell::new(Vec::new()),
            mount_usage: RefCell::new(WatchedPaths::default()),
            discovering: Cell::new(false),
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

    /// Rediscover mounts and query every idle mount, each on its own thread.
    /// Results are shown on the next poll.
    fn refresh_storage(this: &Rc<Self>) {
        if this.discovering.replace(true) {
            return;
        }
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(discover_mounts).await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.discovering.set(false);
            let Ok(drives) = result else {
                return;
            };
            let keys = drives.iter().map(|(key, _)| key.clone()).collect();
            this.mount_usage.borrow_mut().sync(&keys);
            *this.drives.borrow_mut() = drives;
            Self::query_all(&this, |this| &this.mount_usage);
        });
    }

    /// Query every watched, idle entry of `paths` on its own worker thread.
    fn query_all<K>(this: &Rc<Self>, paths: fn(&Self) -> &RefCell<WatchedPaths<K>>)
    where
        K: AsRef<[u8]> + Eq + Hash + Clone + Send + 'static,
    {
        let claimed = paths(this).borrow_mut().claim(Instant::now());
        for key in claimed {
            let weak = Rc::downgrade(this);
            glib::spawn_future_local(async move {
                // A dedicated thread, not gio::spawn_blocking: statvfs can block
                // forever on a hung filesystem and must not hold a thread of
                // GIO's shared pool. A failed spawn or a panicked query (closed
                // channel) counts as unavailable.
                let query = key.clone();
                let (tx, rx) = async_channel::bounded(1);
                let spawned =
                    std::thread::Builder::new()
                        .name("vp-statvfs".into())
                        .spawn(move || {
                            let _ = tx.send_blocking(statvfs_usage(query.as_ref()));
                        });
                let usage = match spawned {
                    Ok(_) => rx.recv().await.ok().flatten(),
                    Err(_) => None,
                };
                let Some(this) = weak.upgrade() else {
                    return;
                };
                // Publish first results now instead of on the next poll.
                if paths(&this).borrow_mut().finish(&key, usage) {
                    this.apply_storage(&mut this.snapshot.borrow_mut());
                    this.callbacks.notify(&this.snapshot.borrow());
                }
            });
        }
    }

    fn apply_storage(&self, snapshot: &mut SystemSnapshot) {
        snapshot.mounts = published_storage(
            &self.drives.borrow(),
            &self.mount_usage.borrow(),
            Instant::now(),
        );
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
        Self::refresh_storage(this);

        // Schedule periodic polls
        let this_weak = Rc::downgrade(this);
        let interval = this.poll_interval.get();

        debug!("SystemService: starting polling every {}s", interval);

        let source_id = glib::timeout_add_seconds_local(interval, move || {
            if let Some(this) = this_weak.upgrade() {
                this.poll();
                Self::refresh_storage(&this);
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
        let mut new_snapshot = SystemSnapshot {
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
            ..Default::default()
        };
        self.apply_storage(&mut new_snapshot);

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
    fn test_parse_mounts_keeps_block_devices_once() {
        let mounts = parse_mounts(
            b"/dev/nvme0n1p6 /home btrfs rw 0 0\n\
             /dev/nvme0n1p6 / btrfs rw 0 0\n\
             /dev/nvme0n1p5 /boot ext4 rw 0 0\n\
             overlay /var/lib/docker/x overlay rw 0 0\n\
             tmpfs /tmp tmpfs rw 0 0\n\
             /dev/loop0 /snap/core squashfs ro 0 0\n\
             /dev/loop1 /sysroot erofs ro 0 0\n\
             /dev/sdb1 /run/media/me/USB\\040STICK vfat rw 0 0\n\
             /dev/sdc1 /run/media/me/caf\xe9 vfat rw 0 0",
        );
        let summary: Vec<_> = mounts
            .iter()
            .map(|(source, mount, fs)| (source.as_slice(), mount.as_slice(), fs.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (&b"/dev/nvme0n1p6"[..], &b"/"[..], "btrfs"),
                (b"/dev/nvme0n1p5", b"/boot", "ext4"),
                (b"/dev/sdb1", b"/run/media/me/USB STICK", "vfat"),
                (b"/dev/sdc1", b"/run/media/me/caf\xe9", "vfat"),
            ]
        );
    }

    #[test]
    fn test_removable_detects_usb_and_sd_but_not_emmc() {
        let card_type = |kind: &'static str| move |_: &Path| Some(format!("{kind}\n"));
        let usb = Path::new("/sys/devices/pci0000:00/usb2/2-1/host0/block/sdb/sdb1");
        let mmc =
            Path::new("/sys/devices/platform/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0/mmcblk0p1");
        let nvme = Path::new("/sys/devices/pci0000:00/nvme/nvme0/nvme0n1/nvme0n1p1");

        assert!(is_removable_block(usb, |_| None));
        assert!(is_removable_block(mmc, card_type("SD")));
        assert!(!is_removable_block(mmc, card_type("MMC")));
        assert!(!is_removable_block(nvme, card_type("SD")));
    }

    #[test]
    fn test_display_device_uses_mapper_name() {
        let sysfs = |name: &'static str| {
            move |file: &Path| {
                (file == Path::new("/sys/class/block/dm-0/dm/name")).then(|| name.to_string())
            }
        };
        assert_eq!(
            display_device("dm-0".into(), sysfs("luks-root\n")),
            "luks-root"
        );
        assert_eq!(display_device("dm-0".into(), sysfs("\n")), "dm-0");
        assert_eq!(display_device("sda1".into(), sysfs("luks-root\n")), "sda1");
    }

    fn space(total: u64) -> SpaceUsage {
        SpaceUsage {
            total,
            ..Default::default()
        }
    }

    fn key(path: &str) -> String {
        path.to_string()
    }

    #[test]
    fn test_hung_mount_stays_listed_without_hiding_others() {
        let now = Instant::now();
        let later = now + STORAGE_STALE_AFTER;
        let drive = |path: &str| {
            let mount = MountUsage {
                mount_point: path.to_string(),
                ..Default::default()
            };
            (path.as_bytes().to_vec(), mount)
        };
        let listed = |drives: &[(Vec<u8>, MountUsage)], usage: &WatchedPaths<Vec<u8>>, at| {
            published_storage(drives, usage, at)
                .into_iter()
                .map(|mount| (mount.mount_point, mount.usage.map(|usage| usage.total)))
                .collect::<Vec<_>>()
        };
        let sync = |usage: &mut WatchedPaths<Vec<u8>>, drives: &[(Vec<u8>, MountUsage)]| {
            usage.sync(&drives.iter().map(|(key, _)| key.clone()).collect());
        };

        let mut drives = vec![drive("/"), drive("/empty"), drive("/usb")];
        let mut usage = WatchedPaths::default();
        sync(&mut usage, &drives);
        assert_eq!(usage.claim(now).len(), 3);
        usage.finish(&b"/".to_vec(), Some(space(100)));
        usage.finish(&b"/empty".to_vec(), Some(space(0)));
        // "/usb" hangs: left out until it times out, then listed as unavailable.
        // The zero-capacity mount is always left out.
        assert_eq!(listed(&drives, &usage, now), [(key("/"), Some(100))]);
        assert_eq!(
            listed(&drives, &usage, later),
            [(key("/"), Some(100)), (key("/usb"), None)]
        );

        // Mounts still come and go while "/usb" hangs; unplugged, it disappears.
        drives = vec![drive("/"), drive("/data")];
        sync(&mut usage, &drives);
        usage.claim(later);
        usage.finish(&b"/data".to_vec(), Some(space(5)));
        assert_eq!(
            listed(&drives, &usage, later),
            [(key("/"), Some(100)), (key("/data"), Some(5))]
        );
        usage.finish(&b"/usb".to_vec(), Some(space(1)));
        assert!(!usage.0.contains_key(b"/usb".as_slice()));
    }

    #[test]
    fn test_usage_percent_matches_df() {
        let usage = SpaceUsage {
            total: 100,
            used: 45,
            available: 45,
        };
        assert_eq!(usage.percent(), 50.0);
        assert_eq!(SpaceUsage::default().percent(), 0.0);
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
