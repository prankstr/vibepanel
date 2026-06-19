//! BatteryService - shared, event-driven battery state via UPower.
//!
//! - Asynchronously connects to the system DBus and UPower DisplayDevice
//! - Reads cached properties for initial state
//! - Listens for `PropertiesChanged` ("g-properties-changed") updates
//! - Notifies listeners on the GLib main loop with a canonical snapshot.

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use tracing::{debug, error, warn};

use super::callbacks::{CallbackId, Callbacks};

/// Path to the kernel's power supply sysfs directory.
const POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";

/// DBus constants for the UPower DisplayDevice.
const UPOWER_NAME: &str = "org.freedesktop.UPower";
const DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";

/// UPower state codes of interest.
/// See: https://upower.freedesktop.org/docs/Device.html#Device:state
/// Note: UPower returns State as u32, TimeToEmpty/TimeToFull as i64.
pub const STATE_CHARGING: u32 = 1;
pub const STATE_DISCHARGING: u32 = 2;
pub const STATE_EMPTY: u32 = 3;
pub const STATE_FULLY_CHARGED: u32 = 4;
pub const STATE_PENDING_DISCHARGE: u32 = 6;

/// Canonical snapshot of battery state.
#[derive(Debug, Clone)]
pub struct BatterySnapshot {
    /// Whether the UPower service is available.
    pub available: bool,
    /// Percentage in range 0.0-100.0 if known.
    pub percent: Option<f64>,
    /// Raw UPower state code, if known (u32 from DBus).
    pub state: Option<u32>,
    /// Power draw in Watts, if known.
    pub energy_rate: Option<f64>,
    /// Seconds until empty, if known (i64 from DBus).
    pub time_to_empty: Option<i64>,
    /// Seconds until full, if known (i64 from DBus).
    pub time_to_full: Option<i64>,
}

impl BatterySnapshot {
    pub fn unknown() -> Self {
        Self {
            available: false,
            percent: None,
            state: None,
            energy_rate: None,
            time_to_empty: None,
            time_to_full: None,
        }
    }

    pub fn is_discharging(&self) -> bool {
        matches!(
            self.state,
            Some(STATE_DISCHARGING | STATE_EMPTY | STATE_PENDING_DISCHARGE)
        )
    }
}

/// Round a floating-point percentage (0.0 - 100.0) to a u8, clamped.
///
/// NaN is treated as 0; infinities are clamped to the 0-100 range.
pub fn rounded_pct_value(percent: f64) -> u8 {
    if percent.is_nan() {
        return 0;
    }
    percent.clamp(0.0, 100.0).round() as u8
}

/// Return a logical icon name for the given battery level.
///
/// Returns names like "battery-full", "battery-high-charging", etc.
/// These are mapped to Material Symbols glyphs by `IconsService`.
///
/// Thresholds (8 levels to match Material icon granularity):
/// - full (>=95%), high (>=80%), medium-high (>=60%), medium (>=40%)
/// - medium-low (>=25%), low (>=10%), very-low (<10%)
pub fn battery_icon_name(percent: u8, charging: bool) -> String {
    let level = if percent >= 95 {
        "full"
    } else if percent >= 80 {
        "high"
    } else if percent >= 60 {
        "medium-high"
    } else if percent >= 40 {
        "medium"
    } else if percent >= 25 {
        "medium-low"
    } else if percent >= 10 {
        "low"
    } else {
        "very-low"
    };

    if charging {
        format!("battery-{level}-charging")
    } else {
        format!("battery-{level}")
    }
}

/// Normalize battery icon aliases from external notification themes to the
/// logical names used by `IconsService` and battery widgets.
pub fn normalize_battery_icon_name(icon_name: &str) -> Option<&'static str> {
    match icon_name {
        "battery-full" | "battery-level-100-symbolic" | "battery-full-symbolic" => {
            Some("battery-full")
        }
        "battery-high" | "battery-level-80-symbolic" | "battery-good-symbolic" => {
            Some("battery-high")
        }
        "battery-medium-high" | "battery-level-60-symbolic" => Some("battery-medium-high"),
        "battery-medium" | "battery-level-50-symbolic" => Some("battery-medium"),
        "battery-medium-low" | "battery-level-30-symbolic" => Some("battery-medium-low"),
        "battery-low" | "battery-level-20-symbolic" | "battery-low-symbolic" => Some("battery-low"),
        "battery-very-low" | "battery-level-10-symbolic" | "battery-empty-symbolic" => {
            Some("battery-very-low")
        }
        "battery-critical-alert" | "battery-caution-symbolic" => Some("battery-critical-alert"),
        "battery-symbolic" => Some("battery-medium"),
        _ => None,
    }
}

/// Shared, process-wide battery service.
pub struct BatteryService {
    proxy: RefCell<Option<gio::DBusProxy>>,
    snapshot: RefCell<BatterySnapshot>,
    callbacks: Callbacks<BatterySnapshot>,
}

impl BatteryService {
    fn new() -> Rc<Self> {
        let has_battery = Self::has_battery_device();

        // Set available = true immediately if we detected a battery device, so
        // that synchronous checks (e.g., widget factory) see the correct state
        // before the async D-Bus initialization completes.
        let initial_snapshot = if has_battery {
            BatterySnapshot {
                available: true,
                ..BatterySnapshot::unknown()
            }
        } else {
            BatterySnapshot::unknown()
        };

        let service = Rc::new(Self {
            proxy: RefCell::new(None),
            snapshot: RefCell::new(initial_snapshot),
            callbacks: Callbacks::new(),
        });

        if has_battery {
            Self::init_dbus(&service);
        } else {
            warn!("BatteryService: no battery device found; service disabled");
        }

        service
    }

    /// Check if any battery device exists under /sys/class/power_supply.
    fn has_battery_device() -> bool {
        let path = Path::new(POWER_SUPPLY_PATH);
        if !path.exists() {
            debug!("BatteryService: {} does not exist", POWER_SUPPLY_PATH);
            return false;
        }

        let entries = match fs::read_dir(path) {
            Ok(it) => it,
            Err(err) => {
                debug!(
                    "BatteryService: failed to read {}: {err}",
                    POWER_SUPPLY_PATH
                );
                return false;
            }
        };

        for entry in entries.flatten() {
            let entry_path = entry.path();
            let type_path = entry_path.join("type");

            // Check if this is a battery device
            let is_battery = fs::read_to_string(&type_path)
                .is_ok_and(|content| content.trim().eq_ignore_ascii_case("battery"));

            if !is_battery {
                continue;
            }

            // Exclude peripheral batteries (e.g., Logitech mice) by checking scope.
            // System batteries either have scope=System or no scope attribute at all.
            // Peripheral batteries have scope=Device.
            let scope_path = entry_path.join("scope");
            let is_peripheral = fs::read_to_string(&scope_path)
                .is_ok_and(|content| content.trim().eq_ignore_ascii_case("device"));

            if !is_peripheral {
                return true;
            }
        }

        debug!(
            "BatteryService: no battery type device found in {}",
            POWER_SUPPLY_PATH
        );
        false
    }

    /// Get the global BatteryService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<BatteryService> = BatteryService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the battery snapshot changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&BatterySnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current snapshot so widgets can render without
        // waiting for the next change.
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Return the current battery snapshot.
    pub fn snapshot(&self) -> BatterySnapshot {
        self.snapshot.borrow().clone()
    }

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        // Asynchronously create proxy on the system bus.
        gio::DBusProxy::for_bus(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            UPOWER_NAME,
            DISPLAY_PATH,
            DEVICE_IFACE,
            None::<&gio::Cancellable>,
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(this) => this,
                    None => return,
                };

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create UPower DBusProxy: {}", e);
                        // Leave snapshot as unknown; widgets will show fallback.
                        return;
                    }
                };

                this.proxy.replace(Some(proxy.clone()));

                // Initial snapshot.
                this.update_from_proxy();

                // Subscribe to property changes.
                let this_weak = Rc::downgrade(&this);
                proxy.connect_local("g-properties-changed", false, move |_values| {
                    if let Some(this) = this_weak.upgrade() {
                        this.update_from_proxy();
                    }
                    None
                });

                // Monitor for service appearing/disappearing (e.g., UPower restart).
                let this_weak = Rc::downgrade(&this);
                proxy.connect_local("notify::g-name-owner", false, move |values| {
                    let this = this_weak.upgrade()?;
                    let proxy = values[0].get::<gio::DBusProxy>().ok();
                    let has_owner = proxy.and_then(|p| p.name_owner()).is_some();
                    if has_owner {
                        // Service reappeared - refresh state.
                        this.update_from_proxy();
                    } else {
                        // Service disappeared - mark unavailable.
                        this.set_unavailable();
                    }
                    None
                });
            },
        );
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.available {
            return; // Already unavailable
        }
        *snapshot = BatterySnapshot::unknown();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    fn update_from_proxy(&self) {
        let Some(ref proxy) = *self.proxy.borrow() else {
            // No proxy yet; keep "unknown" snapshot.
            return;
        };

        fn variant_f64(v: Option<glib::Variant>) -> Option<f64> {
            v.and_then(|v| v.get::<f64>())
        }

        fn variant_u32(v: Option<glib::Variant>) -> Option<u32> {
            v.and_then(|v| v.get::<u32>())
        }

        fn variant_i64(v: Option<glib::Variant>) -> Option<i64> {
            v.and_then(|v| v.get::<i64>())
        }

        let energy = variant_f64(proxy.cached_property("Energy"));
        let full = variant_f64(proxy.cached_property("EnergyFull"));
        let percentage_prop = variant_f64(proxy.cached_property("Percentage"));
        let state = variant_u32(proxy.cached_property("State"));
        let energy_rate = variant_f64(proxy.cached_property("EnergyRate"));
        let time_to_empty = variant_i64(proxy.cached_property("TimeToEmpty"));
        let time_to_full = variant_i64(proxy.cached_property("TimeToFull"));

        let percent = match (energy, full) {
            (Some(e), Some(f)) if f > 0.0 => Some(((e / f) * 100.0).clamp(0.0, 100.0)),
            _ => percentage_prop,
        };

        let new_snapshot = BatterySnapshot {
            available: true,
            percent,
            state,
            energy_rate,
            time_to_empty,
            time_to_full,
        };

        let mut snapshot = self.snapshot.borrow_mut();
        if snapshot.available == new_snapshot.available
            && snapshot.percent == new_snapshot.percent
            && snapshot.state == new_snapshot.state
            && snapshot.energy_rate == new_snapshot.energy_rate
            && snapshot.time_to_empty == new_snapshot.time_to_empty
            && snapshot.time_to_full == new_snapshot.time_to_full
        {
            return;
        }

        *snapshot = new_snapshot;
        drop(snapshot); // Release borrow before notify
        self.callbacks.notify(&self.snapshot.borrow());
    }
}
