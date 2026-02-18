//! PowerProfileService - event-driven power profile state via power-profiles-daemon.
//!
//! - Talks directly to `net.hadess.PowerProfiles` on the system bus
//! - Tracks the active profile and available profiles from DBus properties
//! - Listens for `PropertiesChanged` updates
//! - Notifies listeners on the GLib main loop with a simple snapshot

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib::{self};
use gtk4::prelude::ToVariant;
use gtk4::prelude::*;
use tracing::{error, warn};

use super::callbacks::{CallbackId, Callbacks};

/// DBus constants for power-profiles-daemon.
const BUS_NAME: &str = "net.hadess.PowerProfiles";
const OBJECT_PATH: &str = "/net/hadess/PowerProfiles";
const INTERFACE: &str = "net.hadess.PowerProfiles";

/// Canonical snapshot of power profile state.
#[derive(Debug, Clone)]
pub struct PowerProfileSnapshot {
    /// Whether the power-profiles-daemon service is available.
    pub available: bool,
    /// Currently active profile ID (e.g. "power-saver", "balanced", "performance").
    pub current_profile: Option<String>,
    /// Available profile IDs.
    pub available_profiles: Vec<String>,
}

impl PowerProfileSnapshot {
    pub fn empty() -> Self {
        Self {
            available: false,
            current_profile: None,
            available_profiles: Vec::new(),
        }
    }
}

/// Shared, process-wide power profile service.
pub struct PowerProfileService {
    proxy: RefCell<Option<gio::DBusProxy>>,
    snapshot: RefCell<PowerProfileSnapshot>,
    callbacks: Callbacks<PowerProfileSnapshot>,
    /// D-Bus signal subscription (kept alive for the service lifetime).
    _signal_subscription: RefCell<Option<gio::SignalSubscription>>,
}

impl PowerProfileService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            proxy: RefCell::new(None),
            snapshot: RefCell::new(PowerProfileSnapshot::empty()),
            callbacks: Callbacks::new(),
            _signal_subscription: RefCell::new(None),
        });

        Self::init_dbus(&service);
        service
    }

    /// Get the global PowerProfileService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<PowerProfileService> = PowerProfileService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the power profile snapshot changes.
    /// The callback is always executed on the GLib main loop.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&PowerProfileSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);

        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        // Asynchronously create proxy on the system bus.
        gio::DBusProxy::for_bus(
            gio::BusType::System,
            gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None::<&gio::DBusInterfaceInfo>,
            BUS_NAME,
            OBJECT_PATH,
            INTERFACE,
            None::<&gio::Cancellable>,
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(this) => this,
                    None => return,
                };

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create PowerProfiles DBusProxy: {}", e);
                        return;
                    }
                };

                let connection = proxy.connection();

                this.proxy.replace(Some(proxy.clone()));

                // Prime initial state from cached properties.
                let changed = this.update_from_proxy(&proxy);
                if changed {
                    let snapshot = this.snapshot.borrow().clone();
                    this.callbacks.notify(&snapshot);
                }

                // Monitor for service appearing/disappearing.
                let this_weak = Rc::downgrade(&this);
                let proxy_for_notify = proxy.clone();
                proxy.connect_local("notify::g-name-owner", false, move |values| {
                    let this = this_weak.upgrade()?;
                    let proxy = values[0].get::<gio::DBusProxy>().ok();
                    let has_owner = proxy.and_then(|p| p.name_owner()).is_some();
                    if has_owner {
                        // Service reappeared - refresh state.
                        let changed = this.update_from_proxy(&proxy_for_notify);
                        if changed {
                            let snapshot = this.snapshot.borrow().clone();
                            this.callbacks.notify(&snapshot);
                        }
                    } else {
                        // Service disappeared - mark unavailable.
                        this.set_unavailable();
                    }
                    None
                });

                // Listen for PropertiesChanged on org.freedesktop.DBus.Properties.
                let this_weak = Rc::downgrade(&this);
                let proxy_for_cb = proxy.clone();
                let subscription = connection.subscribe_to_signal(
                    Some(BUS_NAME),
                    Some("org.freedesktop.DBus.Properties"),
                    Some("PropertiesChanged"),
                    Some(OBJECT_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_signal| {
                        if let Some(this) = this_weak.upgrade() {
                            let changed = this.update_from_proxy(&proxy_for_cb);
                            if changed {
                                let snapshot = this.snapshot.borrow().clone();
                                this.callbacks.notify(&snapshot);
                            }
                        }
                    },
                );

                // Store subscription to keep it alive
                this._signal_subscription.replace(Some(subscription));
            },
        );
    }

    fn variant_string(v: Option<glib::Variant>) -> Option<String> {
        v.and_then(|v| v.get::<String>())
    }

    fn parse_profiles_value(v: Option<glib::Variant>) -> Option<Vec<String>> {
        let entries: Vec<HashMap<String, glib::Variant>> =
            v.and_then(|v| v.get::<Vec<HashMap<String, glib::Variant>>>())?;

        let mut profiles = Vec::new();
        for entry in entries {
            if let Some(raw) = entry.get("Profile")
                && let Some(id) = raw.get::<String>()
            {
                profiles.push(id);
            }
        }

        if profiles.is_empty() {
            None
        } else {
            Some(profiles)
        }
    }

    fn update_from_proxy(&self, proxy: &gio::DBusProxy) -> bool {
        let mut changed_any = false;

        // Mark as available since we have a working proxy.
        {
            let mut snapshot = self.snapshot.borrow_mut();
            if !snapshot.available {
                snapshot.available = true;
                changed_any = true;
            }
        }

        let active = Self::variant_string(proxy.cached_property("ActiveProfile"));
        if let Some(active) = active {
            let mut snapshot = self.snapshot.borrow_mut();
            if snapshot.current_profile.as_deref() != Some(active.as_str()) {
                snapshot.current_profile = Some(active.clone());
                if !snapshot.available_profiles.contains(&active) {
                    snapshot.available_profiles.push(active);
                }
                changed_any = true;
            }
        }

        let profiles = Self::parse_profiles_value(proxy.cached_property("Profiles"));
        if let Some(profiles) = profiles {
            let mut snapshot = self.snapshot.borrow_mut();
            if snapshot.available_profiles != profiles {
                snapshot.available_profiles = profiles;
                changed_any = true;
            }
        }

        changed_any
    }

    fn set_unavailable(&self) {
        let mut snapshot = self.snapshot.borrow_mut();
        if !snapshot.available {
            return; // Already unavailable
        }
        *snapshot = PowerProfileSnapshot::empty();
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);

        // Clear proxy.
        self.proxy.replace(None);
    }

    /// Return a clone of the current snapshot.
    pub fn snapshot(&self) -> PowerProfileSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Request a profile change over DBus (non-blocking).
    ///
    /// Uses org.freedesktop.DBus.Properties.Set on ActiveProfile. Errors are
    /// logged and returned as false; on success we optimistically update our
    /// cached snapshot and notify listeners.
    pub fn set_profile(&self, profile: &str) -> bool {
        let Some(proxy) = self.proxy.borrow().clone() else {
            warn!("No DBus proxy; cannot set profile");
            return false;
        };

        let connection = proxy.connection();

        let profile_string = profile.to_string();
        let value_variant = profile_string.to_variant();
        let params = (INTERFACE, "ActiveProfile", value_variant).to_variant();

        // Optimistically update local cache so UI reacts immediately.
        {
            let mut snapshot = self.snapshot.borrow_mut();
            if snapshot.current_profile.as_deref() != Some(profile) {
                snapshot.current_profile = Some(profile_string.clone());
                if !snapshot.available_profiles.contains(&profile_string) {
                    snapshot.available_profiles.push(profile_string.clone());
                }
                let snapshot_cloned = snapshot.clone();
                drop(snapshot);
                self.callbacks.notify(&snapshot_cloned);
            }
        }

        connection.call(
            Some(BUS_NAME),
            OBJECT_PATH,
            "org.freedesktop.DBus.Properties",
            "Set",
            Some(&params),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    error!("Failed to set power profile '{}': {}", profile_string, e);
                }
            },
        );

        true
    }
}
