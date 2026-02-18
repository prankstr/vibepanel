//! BrightnessService - screen backlight monitoring and control.
//!
//! - Discovers a backlight device in `/sys/class/backlight`
//! - Uses systemd-logind D-Bus API (`SetBrightness`) for setting brightness
//! - Falls back to direct sysfs writes if logind is unavailable
//! - Provides a GTK/GLib-friendly, callback-based API
//!
//! Uses libudev to monitor backlight device changes via the GLib main loop.
//! This is fully event-driven - no polling required.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::ToVariant;

use gtk4::gio;
use gtk4::glib;
use tracing::{debug, error, warn};

use super::callbacks::{CallbackId, Callbacks};

/// Logind D-Bus constants.
const LOGIND_BUS_NAME: &str = "org.freedesktop.login1";
const LOGIND_SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";

/// Path to the backlight class on Linux.
const BACKLIGHT_PATH: &str = "/sys/class/backlight";

/// Throttle interval (in ms) for collapsing multiple udev change
/// events into periodic brightness reads. 16ms gives ~60 updates/sec
/// which is smooth for slider dragging.
const THROTTLE_INTERVAL_MS: u64 = 16;

/// Snapshot of brightness service state for callbacks.
#[derive(Debug, Clone)]
pub struct BrightnessSnapshot {
    /// Current brightness as a percentage (0–100).
    pub percent: u32,
    /// Maximum exposed percentage (typically 100).
    pub max_percent: u32,
    /// Whether a usable backlight device was found.
    pub available: bool,
}

impl Default for BrightnessSnapshot {
    fn default() -> Self {
        Self {
            percent: 0,
            max_percent: 100,
            available: false,
        }
    }
}

impl BrightnessSnapshot {
    /// Convenience accessor for the current percentage.
    #[allow(dead_code)]
    pub fn percent(&self) -> u32 {
        self.percent
    }

    /// Whether this snapshot corresponds to a usable backend.
    #[allow(dead_code)]
    pub fn is_available(&self) -> bool {
        self.available
    }
}

/// Internal representation of the selected backlight device.
struct BacklightDevice {
    /// Device name (directory name under /sys/class/backlight).
    name: String,
    /// Path to the `brightness` file.
    brightness_path: PathBuf,
    /// Maximum raw brightness value read from sysfs.
    max_brightness_raw: u32,
}

/// Shared, process-wide brightness service.
pub struct BrightnessService {
    /// Currently selected backlight device, if any.
    device: Option<BacklightDevice>,
    /// Logind session D-Bus object path (e.g. "/org/freedesktop/login1/session/_32").
    /// None if logind is unavailable; falls back to direct sysfs writes.
    logind_session_path: RefCell<Option<String>>,
    /// D-Bus connection for logind calls, if available.
    dbus_connection: RefCell<Option<gio::DBusConnection>>,
    /// Latest snapshot of brightness state.
    current: RefCell<BrightnessSnapshot>,
    /// Registered callbacks.
    callbacks: Callbacks<BrightnessSnapshot>,
    /// Whether the service has completed initialization.
    ready: Cell<bool>,
    /// Udev monitor socket (must stay alive while monitoring).
    /// Stored as raw pointer since udev::MonitorSocket isn't easily stored in RefCell.
    udev_monitor: RefCell<Option<UdevMonitorState>>,
    /// GLib source ID for the udev fd watcher.
    udev_source_id: RefCell<Option<glib::SourceId>>,
    /// Whether we're currently in a throttle period (have fired recently).
    throttle_active: Cell<bool>,
    /// Whether another event arrived during the throttle period.
    pending_read: Cell<bool>,
}

/// State for the udev monitor (stored together to manage lifetimes).
struct UdevMonitorState {
    /// The udev monitor socket.
    socket: udev::MonitorSocket,
}

impl BrightnessService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            device: Self::discover_backlight(),
            logind_session_path: RefCell::new(None),
            dbus_connection: RefCell::new(None),
            current: RefCell::new(BrightnessSnapshot::default()),
            callbacks: Callbacks::new(),
            ready: Cell::new(false),
            udev_monitor: RefCell::new(None),
            udev_source_id: RefCell::new(None),
            throttle_active: Cell::new(false),
            pending_read: Cell::new(false),
        });

        // Initialize logind D-Bus connection for brightness control.
        Self::init_logind(&service);

        // Initialize snapshot and monitoring if a device is available.
        if service.device.is_some() {
            service.read_brightness();
            service.ready.set(true);
            service.start_udev_monitoring();
            debug!("BrightnessService initialized (device found)");
        } else {
            warn!("BrightnessService: no backlight device found; service disabled");
            // Still mark as ready so widgets can render "unavailable" state.
            service.ready.set(true);
        }

        service
    }

    /// Get the global BrightnessService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<BrightnessService> = BrightnessService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when brightness changes.
    ///
    /// The callback is executed on the GLib main loop and is called
    /// immediately with the current snapshot if the service is ready.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&BrightnessSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        if self.ready.get() {
            let snapshot = self.current.borrow().clone();
            self.callbacks.notify_single(id, &snapshot);
        }
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Get the current brightness snapshot.
    pub fn current(&self) -> BrightnessSnapshot {
        self.current.borrow().clone()
    }

    /// Get the current brightness percentage.
    #[allow(dead_code)]
    pub fn percent(&self) -> u32 {
        self.current.borrow().percent
    }

    /// Whether the service has completed initialization.
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.ready.get()
    }

    /// Set brightness as a percentage (0–100).
    ///
    /// Values are clamped to [0, 100]. If no device is available, this is a no-op.
    pub fn set_brightness(&self, percent: u32) {
        let device = match &self.device {
            Some(d) => d,
            None => {
                debug!("BrightnessService::set_brightness called with no device available");
                return;
            }
        };

        // Clamp to [0, 100].
        let value = percent.clamp(0, 100);

        // Convert percent to raw brightness value.
        let raw = ((value as f64) * (device.max_brightness_raw as f64) / 100.0).round() as u32;

        // Try logind first (privilege-safe), fall back to direct sysfs.
        if self.logind_session_path.borrow().is_some() {
            self.set_via_logind(&device.name, raw);
        } else {
            self.set_via_sysfs(raw);
        }
        // The file monitor (or fallback polling loop) will detect the change
        // and emit callbacks if needed.
    }

    /// Initialize logind D-Bus connection and discover session path.
    ///
    /// This enables privilege-safe brightness control via systemd-logind's
    /// SetBrightness method, which doesn't require brightnessctl or root.
    ///
    /// Uses the same session discovery strategy as `BrightnessCli`:
    /// 1. Try `GetSessionByPID` for the current process.
    /// 2. If that fails, call `ListSessions` and pick a graphical session (seat0).
    /// 3. Only fall back to sysfs if both methods fail.
    fn init_logind(this: &Rc<Self>) {
        // Skip if no backlight device (no point setting up D-Bus).
        if this.device.is_none() {
            return;
        }

        let this_weak = Rc::downgrade(this);

        // Connect to the system bus.
        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let connection = match res {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!(
                            "BrightnessService: failed to connect to system bus: {}; \
                         falling back to direct sysfs writes",
                            e
                        );
                        return;
                    }
                };

                // Helper: try to pick a logind session using the same strategy
                // as BrightnessCli::get_dbus_session (PID first, then ListSessions).
                fn pick_logind_session(connection: &gio::DBusConnection) -> Option<String> {
                    // First try GetSessionByPID for this process.
                    let pid = std::process::id();
                    if let Ok(result) = connection.call_sync(
                        Some(LOGIND_BUS_NAME),
                        "/org/freedesktop/login1",
                        "org.freedesktop.login1.Manager",
                        "GetSessionByPID",
                        Some(&(pid,).to_variant()),
                        Some(glib::VariantTy::new("(o)").unwrap()),
                        gio::DBusCallFlags::NONE,
                        5000,
                        None::<&gio::Cancellable>,
                    ) && let Some(session_path) = result.child_value(0).get::<String>()
                    {
                        return Some(session_path);
                    }

                    // Fall back to ListSessions and pick a graphical one (seat0),
                    // mirroring BrightnessCli::get_active_graphical_session.
                    let result = connection
                        .call_sync(
                            Some(LOGIND_BUS_NAME),
                            "/org/freedesktop/login1",
                            "org.freedesktop.login1.Manager",
                            "ListSessions",
                            None,
                            Some(glib::VariantTy::new("(a(susso))").unwrap()),
                            gio::DBusCallFlags::NONE,
                            5000,
                            None::<&gio::Cancellable>,
                        )
                        .ok()?;

                    // Result is (array of (session_id, uid, user_name, seat_id, object_path),)
                    let sessions = result.child_value(0);
                    let n_sessions = sessions.n_children();

                    // First pass: look for a session on seat0 (graphical seat).
                    for i in 0..n_sessions {
                        let session = sessions.child_value(i);
                        let seat_id: Option<String> = session.child_value(3).get();
                        let object_path: Option<String> = session.child_value(4).get();

                        if let (Some(seat), Some(path)) = (seat_id, object_path)
                            && seat == "seat0"
                        {
                            return Some(path);
                        }
                    }

                    // Second pass: take any session.
                    if n_sessions > 0 {
                        let session = sessions.child_value(0);
                        return session.child_value(4).get::<String>();
                    }

                    None
                }

                // Try to pick a session synchronously now that we have a connection.
                match pick_logind_session(&connection) {
                    Some(session_path) => {
                        debug!(
                            "BrightnessService: using logind session {} for brightness control",
                            session_path
                        );
                        *this.logind_session_path.borrow_mut() = Some(session_path);
                        *this.dbus_connection.borrow_mut() = Some(connection);
                    }
                    None => {
                        warn!(
                            "BrightnessService: no usable logind session found; \
                         falling back to direct sysfs writes"
                        );
                    }
                }
            },
        );
    }

    fn discover_backlight() -> Option<BacklightDevice> {
        let path = Path::new(BACKLIGHT_PATH);
        if !path.exists() {
            debug!("BrightnessService: {} does not exist", BACKLIGHT_PATH);
            return None;
        }

        let entries = match fs::read_dir(path) {
            Ok(it) => it,
            Err(err) => {
                error!(
                    "BrightnessService: failed to read {}: {err}",
                    BACKLIGHT_PATH
                );
                return None;
            }
        };

        let mut devices: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                devices.push(p);
            }
        }

        if devices.is_empty() {
            debug!("BrightnessService: no directories under {}", BACKLIGHT_PATH);
            return None;
        }

        // Sort by priority: intel*, amd*, acpi*, others.
        devices.sort_by_key(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_lowercase();

            if name.contains("intel") {
                0
            } else if name.contains("amd") {
                1
            } else if name.contains("acpi") {
                2
            } else {
                3
            }
        });

        for device in devices {
            let brightness_path = device.join("brightness");
            let max_brightness_path = device.join("max_brightness");
            if !brightness_path.exists() || !max_brightness_path.exists() {
                continue;
            }

            let name = device
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();

            let max_brightness_raw = match Self::read_u32_from_file(&max_brightness_path) {
                Some(v) if v > 0 => v,
                Some(_) => {
                    warn!(
                        "BrightnessService: max_brightness for device {} is zero; skipping",
                        name
                    );
                    continue;
                }
                None => {
                    warn!(
                        "BrightnessService: failed to read max_brightness for device {}; using 100",
                        name
                    );
                    100
                }
            };

            debug!("BrightnessService: using backlight device: {}", name);

            return Some(BacklightDevice {
                name,
                brightness_path,
                max_brightness_raw,
            });
        }

        None
    }

    fn read_u32_from_file(path: &Path) -> Option<u32> {
        let mut file = fs::File::open(path).ok()?;
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return None;
        }
        let trimmed = buf.trim();
        trimmed.parse::<u32>().ok()
    }

    fn start_udev_monitoring(self: &Rc<Self>) {
        let device = match &self.device {
            Some(d) => d,
            None => return,
        };

        // Create a udev monitor for the "backlight" subsystem.
        let socket = match udev::MonitorBuilder::new() {
            Ok(builder) => match builder.match_subsystem("backlight") {
                Ok(builder) => match builder.listen() {
                    Ok(socket) => socket,
                    Err(e) => {
                        warn!("BrightnessService: failed to start udev monitor: {}", e);
                        return;
                    }
                },
                Err(e) => {
                    warn!("BrightnessService: failed to filter udev subsystem: {}", e);
                    return;
                }
            },
            Err(e) => {
                warn!("BrightnessService: failed to create udev monitor: {}", e);
                return;
            }
        };

        // Get the file descriptor for the monitor socket.
        let fd = socket.as_raw_fd();

        // Store the socket to keep it alive.
        *self.udev_monitor.borrow_mut() = Some(UdevMonitorState { socket });

        let device_name = device.name.clone();
        let this_weak = Rc::downgrade(self);

        // Watch the fd using glib's unix_fd_add_local - fires when udev events arrive.
        let source_id = glib::unix_fd_add_local(fd, glib::IOCondition::IN, move |_fd, _cond| {
            let this = match this_weak.upgrade() {
                Some(t) => t,
                None => return glib::ControlFlow::Break,
            };

            // Read events from the monitor socket.
            // We need to borrow the monitor to call receive().
            let mut should_read = false;
            if let Some(ref state) = *this.udev_monitor.borrow() {
                // iter() yields events without blocking since fd is ready.
                for event in state.socket.iter() {
                    // Only care about "change" events on our device.
                    if event.event_type() != udev::EventType::Change {
                        continue;
                    }

                    // Check if this is our backlight device.
                    if let Some(name) = event.sysname().to_str()
                        && name == device_name
                    {
                        should_read = true;
                        // Don't break - drain all pending events.
                    }
                }
            }

            if should_read {
                this.schedule_debounced_read();
            }

            glib::ControlFlow::Continue
        });

        *self.udev_source_id.borrow_mut() = Some(source_id);
        debug!("BrightnessService: udev monitoring started for backlight subsystem");
    }

    fn schedule_debounced_read(self: &Rc<Self>) {
        // Throttle pattern: fire immediately on leading edge, then wait.
        // If more events arrive during the wait, do one final read at the end.

        if self.throttle_active.get() {
            // Already in throttle period - just mark that we need a trailing read.
            self.pending_read.set(true);
            return;
        }

        // Leading edge: read immediately.
        self.read_brightness();

        // Start throttle period.
        self.throttle_active.set(true);
        self.pending_read.set(false);

        let this_weak = Rc::downgrade(self);
        let interval = Duration::from_millis(THROTTLE_INTERVAL_MS);

        glib::timeout_add_local(interval, move || {
            if let Some(this) = this_weak.upgrade() {
                // End of throttle period.
                this.throttle_active.set(false);

                // If events arrived during throttle, do a trailing read.
                if this.pending_read.get() {
                    this.pending_read.set(false);
                    this.read_brightness();
                }
            }
            glib::ControlFlow::Break
        });
    }

    fn read_brightness(&self) {
        let device = match &self.device {
            Some(d) => d,
            None => {
                // Update snapshot to "unavailable" if needed.
                let mut current = self.current.borrow_mut();
                if current.available {
                    current.available = false;
                    current.percent = 0;
                    current.max_percent = 100;
                    self.callbacks.notify(&current);
                }
                return;
            }
        };

        let raw = match Self::read_u32_from_file(&device.brightness_path) {
            Some(v) => v,
            None => {
                error!(
                    "BrightnessService: failed to read brightness from {}",
                    device.brightness_path.display()
                );
                return;
            }
        };

        let percent = if device.max_brightness_raw > 0 {
            // Round to nearest integer percentage.
            ((raw as f64) * 100.0 / (device.max_brightness_raw as f64)).round() as u32
        } else {
            0
        }
        .clamp(0, 100);

        let mut current = self.current.borrow_mut();
        if current.percent == percent && current.available {
            // No change; nothing to do.
            return;
        }

        current.percent = percent;
        current.max_percent = 100;
        current.available = true;

        self.callbacks.notify(&current);
    }

    /// Set brightness via logind D-Bus API.
    ///
    /// Calls org.freedesktop.login1.Session.SetBrightness(subsystem, name, brightness).
    /// This is privilege-safe: Polkit allows the active session user to adjust
    /// their own backlight without a password prompt.
    fn set_via_logind(&self, device_name: &str, raw_brightness: u32) {
        let session_path = match self.logind_session_path.borrow().clone() {
            Some(p) => p,
            None => {
                // Shouldn't happen if we check before calling, but fall back.
                warn!("BrightnessService: logind session path not set; falling back to sysfs");
                self.set_via_sysfs(raw_brightness);
                return;
            }
        };

        let connection = match self.dbus_connection.borrow().clone() {
            Some(c) => c,
            None => {
                warn!("BrightnessService: no D-Bus connection; falling back to sysfs");
                self.set_via_sysfs(raw_brightness);
                return;
            }
        };

        // SetBrightness(subsystem: "backlight", name: device_name, brightness: raw_value)
        let params = ("backlight", device_name, raw_brightness).to_variant();
        let device_name_owned = device_name.to_string();

        connection.call(
            Some(LOGIND_BUS_NAME),
            &session_path,
            LOGIND_SESSION_INTERFACE,
            "SetBrightness",
            Some(&params),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    error!(
                        "BrightnessService: logind SetBrightness failed for {}: {}",
                        device_name_owned, e
                    );
                }
            },
        );
    }

    /// Set brightness via direct sysfs write (fallback).
    ///
    /// This may fail if the user lacks write permissions to the sysfs file.
    fn set_via_sysfs(&self, raw_brightness: u32) {
        let device = match &self.device {
            Some(d) => d,
            None => return,
        };

        if let Err(err) = fs::write(&device.brightness_path, raw_brightness.to_string()) {
            error!(
                "BrightnessService: failed to write brightness to {}: {err}",
                device.brightness_path.display()
            );
        }
    }
}

impl Drop for BrightnessService {
    fn drop(&mut self) {
        debug!(
            "BrightnessService dropped (device: {:?})",
            self.device.as_ref().map(|d| d.name.as_str())
        );

        // Remove the udev fd watcher from the main loop.
        if let Some(source_id) = self.udev_source_id.borrow_mut().take() {
            source_id.remove();
        }

        // Drop the udev monitor socket.
        self.udev_monitor.borrow_mut().take();

        // Clear D-Bus connection and session path.
        self.dbus_connection.borrow_mut().take();
        self.logind_session_path.borrow_mut().take();
    }
}

// CLI interface - synchronous, standalone (no GTK main loop required)

/// Synchronous brightness control for CLI usage.
///
/// This is a lightweight, standalone interface that doesn't require GTK or
/// a running main loop. D-Bus is only initialized lazily on the first write
/// operation, so read-only operations (get) have minimal overhead.
pub struct BrightnessCli {
    /// Device name (directory name under /sys/class/backlight).
    device_name: String,
    /// Path to the `brightness` file.
    brightness_path: PathBuf,
    /// Maximum raw brightness value.
    max_brightness: u32,
}

impl BrightnessCli {
    /// Create a new CLI brightness controller.
    ///
    /// Returns `None` if no backlight device is found.
    /// This only discovers the device; D-Bus is not initialized until needed.
    pub fn new() -> Option<Self> {
        let device = BrightnessService::discover_backlight()?;

        Some(Self {
            device_name: device.name,
            brightness_path: device.brightness_path,
            max_brightness: device.max_brightness_raw,
        })
    }

    /// Get the current brightness as a percentage (0-100).
    ///
    /// Reads directly from sysfs; no D-Bus or privileges required.
    pub fn get_percent(&self) -> u32 {
        let raw = BrightnessService::read_u32_from_file(&self.brightness_path).unwrap_or(0);
        if self.max_brightness > 0 {
            ((raw as f64) * 100.0 / (self.max_brightness as f64)).round() as u32
        } else {
            0
        }
    }

    /// Set brightness to a percentage (0-100).
    ///
    /// Uses logind D-Bus for privilege-safe writes; falls back to sysfs.
    pub fn set_percent(&self, percent: u32) -> Result<(), String> {
        let value = percent.clamp(0, 100);
        let raw = ((value as f64) * (self.max_brightness as f64) / 100.0).round() as u32;

        // Try logind D-Bus first, fall back to sysfs.
        if let Some((conn, session_path)) = Self::get_dbus_session() {
            self.set_via_logind_sync(&conn, &session_path, raw)
        } else {
            self.set_via_sysfs(raw)
        }
    }

    /// Connect to system D-Bus and get a session path (lazy, on-demand).
    fn get_dbus_session() -> Option<(gio::DBusConnection, String)> {
        let connection = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).ok()?;

        let session_path = Self::get_session_for_pid(&connection, std::process::id())
            .or_else(|| Self::get_active_graphical_session(&connection))?;

        Some((connection, session_path))
    }

    /// Get the session path for a specific PID.
    fn get_session_for_pid(connection: &gio::DBusConnection, pid: u32) -> Option<String> {
        let result = connection
            .call_sync(
                Some(LOGIND_BUS_NAME),
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "GetSessionByPID",
                Some(&(pid,).to_variant()),
                Some(glib::VariantTy::new("(o)").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        result.child_value(0).get::<String>()
    }

    /// Find an active graphical session (seat0) to use for brightness control.
    fn get_active_graphical_session(connection: &gio::DBusConnection) -> Option<String> {
        let result = connection
            .call_sync(
                Some(LOGIND_BUS_NAME),
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "ListSessions",
                None,
                Some(glib::VariantTy::new("(a(susso))").unwrap()),
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        // Result is (array of (session_id, uid, user_name, seat_id, object_path),)
        let sessions = result.child_value(0);
        let n_sessions = sessions.n_children();

        // First pass: look for a session on seat0 (graphical seat).
        for i in 0..n_sessions {
            let session = sessions.child_value(i);
            let seat_id: Option<String> = session.child_value(3).get();
            let object_path: Option<String> = session.child_value(4).get();

            if let (Some(seat), Some(path)) = (seat_id, object_path)
                && seat == "seat0"
            {
                return Some(path);
            }
        }

        // Second pass: take any session.
        if n_sessions > 0 {
            let session = sessions.child_value(0);
            return session.child_value(4).get::<String>();
        }

        None
    }

    /// Set brightness via logind D-Bus (synchronous).
    fn set_via_logind_sync(
        &self,
        connection: &gio::DBusConnection,
        session_path: &str,
        raw: u32,
    ) -> Result<(), String> {
        connection
            .call_sync(
                Some(LOGIND_BUS_NAME),
                session_path,
                LOGIND_SESSION_INTERFACE,
                "SetBrightness",
                Some(&("backlight", self.device_name.as_str(), raw).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::Cancellable>,
            )
            .map(|_| ())
            .map_err(|e| format!("logind SetBrightness failed: {}", e))
    }

    /// Set brightness via direct sysfs write (fallback).
    fn set_via_sysfs(&self, raw: u32) -> Result<(), String> {
        fs::write(&self.brightness_path, raw.to_string()).map_err(|e| {
            format!(
                "failed to write to {}: {}",
                self.brightness_path.display(),
                e
            )
        })
    }
}
