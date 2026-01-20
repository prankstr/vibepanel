//! Bar window management service with multi-monitor and live reload support.
//!
//! This service manages the bar window lifecycle across multiple monitors and
//! enables hot-reloading when the configuration changes. It holds references
//! to the GTK application and creates/destroys bar windows as monitors are
//! connected/disconnected or configuration changes.
//!
//! ## Usage
//!
//! The `BarManager` is initialized during application startup with a reference
//! to the GTK application. It then manages bars for each monitor via:
//!
//! - `sync_monitors()`: Creates bars for new monitors, removes bars for
//!   disconnected monitors, respects `bar.outputs` allow-list.
//! - `reconfigure_all()`: Destroys all bars and recreates them with new config.
//!
//! This allows live reload of structural changes like:
//! - Bar size, layout, margins
//! - Widget list changes
//! - Output allow-list changes

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::glib::SignalHandlerId;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};
use tracing::{debug, info};

use vibepanel_core::Config;

use crate::bar;
use crate::services::surfaces::SurfaceStyleManager;
use crate::widgets::BarState;

/// State for a single bar instance on a specific monitor.
struct BarInstance {
    /// The monitor this bar is displayed on.
    #[allow(dead_code)]
    monitor: gtk4::gdk::Monitor,
    /// The bar window.
    window: ApplicationWindow,
    /// Widget handles for this bar (timers, callbacks, etc.).
    state: BarState,
}

/// Manages bar window lifecycle across multiple monitors.
///
/// This is a singleton service that holds references to the GTK application
/// and manages bar windows for each monitor, enabling live reload when
/// configuration changes and dynamic monitor hot-plug handling.
pub struct BarManager {
    /// Reference to the GTK application.
    app: RefCell<Option<Application>>,
    /// Bar instances keyed by monitor connector name.
    bars: RefCell<HashMap<String, BarInstance>>,
}

// Thread-local singleton storage
thread_local! {
    static BAR_MANAGER_INSTANCE: RefCell<Option<Rc<BarManager>>> = const { RefCell::new(None) };
}

/// Get a stable key for a monitor.
///
/// Uses the connector name if available (e.g., "eDP-1", "DP-1"), otherwise
/// falls back to "unknown-N" where N is the monitor index. Monitors without
/// connector names cannot be reliably targeted via `bar.outputs`.
///
/// # Output Name Contract
///
/// This key is passed as `output_id` to widgets and must match the keys used by
/// compositor backends in `WorkspaceSnapshot.per_output` and `WindowInfo.output`.
/// Backends should use connector names (from `wl_output::Name` or equivalent) to
/// ensure per-monitor widget filtering works correctly. See `compositor::types`
/// module documentation for details.
fn monitor_key(monitor: &gtk4::gdk::Monitor, index: u32) -> String {
    if let Some(conn) = monitor.connector() {
        conn.to_string()
    } else {
        format!("unknown-{}", index)
    }
}

impl BarManager {
    /// Create a new BarManager.
    fn new() -> Rc<Self> {
        Rc::new(Self {
            app: RefCell::new(None),
            bars: RefCell::new(HashMap::new()),
        })
    }

    /// Get the global BarManager singleton.
    ///
    /// Initializes the singleton on first access.
    pub fn global() -> Rc<Self> {
        BAR_MANAGER_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(BarManager::new());
            }
            opt.as_ref().unwrap().clone()
        })
    }

    /// Initialize the bar manager with the GTK application reference.
    ///
    /// This should be called during application activation, before calling
    /// `sync_monitors()` to create initial bar windows.
    pub fn init(&self, app: &Application) {
        *self.app.borrow_mut() = Some(app.clone());
        debug!("BarManager initialized with app");
    }

    /// Create a bar for a specific monitor.
    ///
    /// Returns the monitor key used to identify this bar, or None if creation
    /// failed (e.g., app not initialized).
    pub fn create_bar_for_monitor(
        &self,
        monitor: &gtk4::gdk::Monitor,
        monitor_index: u32,
        config: &Config,
    ) -> Option<String> {
        let app = self.app.borrow();
        let app_ref = app.as_ref()?;
        let key = monitor_key(monitor, monitor_index);

        // Avoid duplicating bars if called redundantly
        if self.bars.borrow().contains_key(&key) {
            debug!("Bar already exists for monitor key={}", key);
            return Some(key);
        }

        let mut state = BarState::new();
        let window = bar::create_bar_window(app_ref, config, monitor, &key, &mut state);

        // Apply Pango font attributes to all labels if enabled in config.
        SurfaceStyleManager::global().apply_pango_attrs_all(&window);

        let instance = BarInstance {
            monitor: monitor.clone(),
            window: window.clone(),
            state,
        };

        self.bars.borrow_mut().insert(key.clone(), instance);

        info!(
            "Created bar for monitor key={} connector={:?}",
            key,
            monitor.connector()
        );

        Some(key)
    }

    /// Remove a bar by its monitor key.
    ///
    /// Closes the window and drops the BarState, cleaning up timers/callbacks.
    pub fn remove_bar(&self, key: &str) {
        if let Some(instance) = self.bars.borrow_mut().remove(key) {
            debug!("Removing bar for key={}", key);
            instance.window.close();
            // BarState is dropped here, cleaning up widget handles
        }
    }

    /// Synchronize bars with the current display monitors.
    ///
    /// This is the main entry point for managing bars. It:
    /// - Creates bars for new monitors (respecting `bar.outputs` allow-list)
    /// - Removes bars for disconnected monitors
    /// - Removes bars for monitors no longer in the allow-list
    ///
    /// Call this on initial activation and when monitors change.
    pub fn sync_monitors(&self, display: &gtk4::gdk::Display, config: &Config) {
        let monitors = display.monitors();
        let mut seen_keys = HashSet::new();

        for i in 0..monitors.n_items() {
            let Some(obj) = monitors.item(i) else {
                continue;
            };
            let Ok(monitor) = obj.downcast::<gtk4::gdk::Monitor>() else {
                continue;
            };
            let key = monitor_key(&monitor, i);

            // Check bar.outputs allow-list (empty = all monitors)
            if !config.bar.outputs.is_empty() && !config.bar.outputs.contains(&key) {
                debug!("Skipping monitor {} (not in bar.outputs)", key);
                continue;
            }

            seen_keys.insert(key.clone());

            // Create bar if it doesn't exist
            if !self.bars.borrow().contains_key(&key) {
                self.create_bar_for_monitor(&monitor, i, config);
            }
        }

        // Remove bars whose monitors no longer exist or are filtered out
        let existing_keys: Vec<String> = self.bars.borrow().keys().cloned().collect();
        for key in existing_keys {
            if !seen_keys.contains(&key) {
                info!("Removing bar for disconnected/filtered monitor: {}", key);
                self.remove_bar(&key);
            }
        }

        info!(
            "Monitor sync complete: {} bar(s) active, {} total widget handles",
            self.bars.borrow().len(),
            self.handle_count()
        );
    }

    /// Reconfigure all bars with new configuration.
    ///
    /// This destroys all existing bars and recreates them with the updated
    /// configuration. Use this for live reload when `bar.outputs` or other
    /// structural settings change.
    pub fn reconfigure_all(&self, display: &gtk4::gdk::Display, config: &Config) {
        info!("Reconfiguring all bars...");

        // Remove all existing bars
        let keys: Vec<String> = self.bars.borrow().keys().cloned().collect();
        for key in keys {
            self.remove_bar(&key);
        }

        // Recreate bars based on current monitors and config
        self.sync_monitors(display, config);

        info!(
            "Reconfiguration complete: {} bar(s) with {} widget handles",
            self.bars.borrow().len(),
            self.handle_count()
        );
    }

    /// Get the total number of widget handles across all bars.
    pub fn handle_count(&self) -> usize {
        self.bars
            .borrow()
            .values()
            .map(|instance| instance.state.handle_count())
            .sum()
    }

    /// Get the number of active bars.
    pub fn bar_count(&self) -> usize {
        self.bars.borrow().len()
    }

    /// Check if a bar exists for the given monitor key.
    #[allow(dead_code)]
    pub fn has_bar(&self, key: &str) -> bool {
        self.bars.borrow().contains_key(key)
    }

    /// Get all active monitor keys.
    #[allow(dead_code)]
    pub fn active_monitors(&self) -> Vec<String> {
        self.bars.borrow().keys().cloned().collect()
    }

    /// Hide all bars immediately.
    ///
    /// This is used during monitor hotplug to prevent bars from briefly
    /// appearing on the wrong monitor when the compositor reassigns surfaces.
    /// Call this immediately when a monitor change signal is received, before
    /// the delayed sync runs.
    pub fn hide_all(&self) {
        for instance in self.bars.borrow().values() {
            instance.window.set_opacity(0.0);
        }
        debug!("All bars hidden for monitor change");
    }

    /// Show all bars.
    ///
    /// Called after sync_monitors to reveal bars that weren't removed.
    pub fn show_all(&self) {
        for instance in self.bars.borrow().values() {
            instance.window.set_opacity(1.0);
        }
        debug!("All bars shown after monitor sync");
    }
}

/// Check if a monitor is fully ready (has connector and valid geometry).
fn monitor_is_ready(monitor: &gtk4::gdk::Monitor) -> bool {
    monitor.connector().is_some() && monitor.geometry().width() > 0
}

/// Get a unique identifier for a GDK monitor based on its pointer address.
///
/// This is used to track monitor identity when waiting for monitors to become ready,
/// ensuring we don't double-count a monitor if multiple signals fire for it.
fn monitor_id(monitor: &gtk4::gdk::Monitor) -> usize {
    monitor.as_ptr() as usize
}

/// Synchronize bars after monitor change, waiting for monitors to be ready.
///
/// When GDK first reports a new monitor, it may not have the connector name
/// or valid geometry yet. This function waits for all monitors to be fully
/// initialized before syncing, avoiding the need for arbitrary delays.
pub fn sync_monitors_when_ready(display: &gtk4::gdk::Display, config: &vibepanel_core::Config) {
    let monitors = display.monitors();

    // Find monitors that aren't fully ready yet, tracking them by identity
    let mut pending_monitors: Vec<gtk4::gdk::Monitor> = Vec::new();
    let mut pending_set: HashSet<usize> = HashSet::new();
    for i in 0..monitors.n_items() {
        let Some(obj) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        if !monitor_is_ready(&monitor) {
            pending_set.insert(monitor_id(&monitor));
            pending_monitors.push(monitor);
        }
    }

    if pending_monitors.is_empty() {
        // All monitors are ready, sync immediately
        info!("All monitors ready, syncing bars...");
        let manager = BarManager::global();
        manager.sync_monitors(display, config);
        manager.show_all();
    } else {
        // Wait for pending monitors to become ready
        debug!(
            "Waiting for {} monitor(s) to be fully initialized...",
            pending_monitors.len()
        );

        let display = display.clone();
        let config = config.clone();
        let pending_set = Rc::new(RefCell::new(pending_set));
        let signal_handlers: Rc<RefCell<Vec<(gtk4::gdk::Monitor, SignalHandlerId)>>> =
            Rc::new(RefCell::new(Vec::new()));

        for monitor in pending_monitors {
            let display = display.clone();
            let config = config.clone();
            let pending_set = pending_set.clone();
            let signal_handlers = signal_handlers.clone();

            // Closure to check if this monitor is now ready and trigger sync if all are done.
            // Using a HashSet ensures that even if multiple signals fire for the same monitor,
            // we only mark it as ready once (removing from a set is idempotent).
            let check_ready = {
                let display = display.clone();
                let config = config.clone();
                let pending_set = pending_set.clone();
                let signal_handlers = signal_handlers.clone();
                move |mon: &gtk4::gdk::Monitor| {
                    if monitor_is_ready(mon) {
                        let mut pending = pending_set.borrow_mut();
                        let id = monitor_id(mon);

                        // Only act if this monitor was still pending (idempotent removal)
                        if pending.remove(&id) {
                            debug!(
                                "Monitor ready: {:?} ({}x{}), {} remaining",
                                mon.connector(),
                                mon.geometry().width(),
                                mon.geometry().height(),
                                pending.len()
                            );

                            if pending.is_empty() {
                                // All monitors ready, sync now
                                drop(pending); // Release borrow before calling sync
                                info!("All monitors ready, syncing bars...");
                                let manager = BarManager::global();
                                manager.sync_monitors(&display, &config);
                                manager.show_all();

                                // Disconnect all signal handlers to avoid reference cycles
                                for (mon, handler) in signal_handlers.borrow_mut().drain(..) {
                                    mon.disconnect(handler);
                                }
                            }
                        }
                    }
                }
            };

            // Listen to both connector and geometry changes.
            // Both handlers share the same check_ready logic which uses HashSet
            // for idempotent tracking - multiple signals for the same monitor are safe.
            let check_ready_connector = check_ready.clone();
            let handler_connector = monitor.connect_connector_notify(move |mon| {
                check_ready_connector(mon);
            });

            let check_ready_geometry = check_ready;
            let handler_geometry = monitor.connect_geometry_notify(move |mon| {
                check_ready_geometry(mon);
            });

            let mut handlers = signal_handlers.borrow_mut();
            handlers.push((monitor.clone(), handler_connector));
            handlers.push((monitor.clone(), handler_geometry));
        }
    }
}
