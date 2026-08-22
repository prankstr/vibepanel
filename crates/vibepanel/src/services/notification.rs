//! NotificationService - notification daemon implementing org.freedesktop.Notifications.
//!
//! This service owns the D-Bus name and receives notifications from all
//! applications. Notifications are stored in memory and exposed to widgets
//! via the standard callback mechanism.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use gtk4::gio::{self, prelude::*};
use gtk4::glib::Variant;
use gtk4::glib::variant::StaticVariantType;
use tracing::{debug, error, info, warn};

use super::callbacks::{CallbackId, Callbacks};
use super::state::{self, PersistedNotification};

const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const VIBEPANEL_NOTIFICATIONS_INTERFACE: &str = "io.github.vibepanel.Notifications1";
const UNREAD_METHOD: &str = "GetUnreadCount";
const DBUS_CALL_TIMEOUT_MS: i32 = 2_000;

/// D-Bus introspection XML for org.freedesktop.Notifications
const NOTIFICATIONS_XML: &str = r#"
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="Notify">
      <arg direction="in"  name="app_name" type="s"/>
      <arg direction="in"  name="replaces_id" type="u"/>
      <arg direction="in"  name="app_icon" type="s"/>
      <arg direction="in"  name="summary" type="s"/>
      <arg direction="in"  name="body" type="s"/>
      <arg direction="in"  name="actions" type="as"/>
      <arg direction="in"  name="hints" type="a{sv}"/>
      <arg direction="in"  name="expire_timeout" type="i"/>
      <arg direction="out" name="id" type="u"/>
    </method>
    <method name="CloseNotification">
      <arg direction="in" name="id" type="u"/>
    </method>
    <method name="GetCapabilities">
      <arg direction="out" name="capabilities" type="as"/>
    </method>
    <method name="GetServerInformation">
      <arg direction="out" name="name" type="s"/>
      <arg direction="out" name="vendor" type="s"/>
      <arg direction="out" name="version" type="s"/>
      <arg direction="out" name="spec_version" type="s"/>
    </method>
    <signal name="NotificationClosed">
      <arg name="id" type="u"/>
      <arg name="reason" type="u"/>
    </signal>
    <signal name="ActionInvoked">
      <arg name="id" type="u"/>
      <arg name="action_key" type="s"/>
    </signal>
    <signal name="ActivationToken">
      <arg name="id" type="u"/>
      <arg name="activation_token" type="s"/>
    </signal>
  </interface>
  <interface name="io.github.vibepanel.Notifications1">
    <method name="GetUnreadCount">
      <arg direction="out" name="count" type="u"/>
    </method>
  </interface>
</node>
"#;

pub const CLOSE_REASON_DISMISSED: u32 = 2;
pub const CLOSE_REASON_CLOSED: u32 = 3;

pub const URGENCY_LOW: u8 = 0;
pub const URGENCY_NORMAL: u8 = 1;
pub const URGENCY_CRITICAL: u8 = 2;

/// Server capabilities we advertise
const CAPABILITIES: &[&str] = &[
    "body",
    "body-markup",
    "actions",
    "persistence",
    "icon-static",
];

/// Maximum number of notifications to keep in memory.
/// When this limit is exceeded, the oldest notifications are removed.
const MAX_NOTIFICATIONS: usize = 100;

/// Snapshot of a single notification.
#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>, // [(action_id, label), ...]
    pub urgency: u8,
    pub timestamp: f64,      // seconds since UNIX epoch
    pub expire_timeout: i32, // ms, -1=default, 0=never
    /// Desktop entry ID from the "desktop-entry" hint (e.g. "org.telegram.desktop")
    pub desktop_entry: Option<String>,
    /// Optional image path hint (e.g. chat avatar path)
    pub image_path: Option<String>,
    /// Optional raw image data hint (e.g. freedesktop image-data)
    pub image_data: Option<NotificationImage>,
    /// Whether the notification is transient: skip popover history and persistence,
    /// only fire the toast.
    pub transient: bool,
    /// Whether a service-side close should also close any visible toast.
    pub close_toast_on_close: bool,
}

/// Raw image data for a notification, parsed from the
/// freedesktop.org "image-data" hint.
#[derive(Debug, Clone)]
pub struct NotificationImage {
    pub width: i32,
    pub height: i32,
    pub rowstride: i32,
    pub has_alpha: bool,
    pub channels: i32,
    pub data: Vec<u8>,
}

impl Notification {
    /// Convert to a persistable form (omits image_data which is binary)
    pub fn to_persisted(&self) -> PersistedNotification {
        PersistedNotification {
            id: self.id,
            app_name: self.app_name.clone(),
            app_icon: self.app_icon.clone(),
            summary: self.summary.clone(),
            body: self.body.clone(),
            actions: self.actions.clone(),
            urgency: self.urgency,
            timestamp: self.timestamp,
            expire_timeout: self.expire_timeout,
            desktop_entry: self.desktop_entry.clone(),
            image_path: self.image_path.clone(),
        }
    }
}

impl From<PersistedNotification> for Notification {
    fn from(p: PersistedNotification) -> Self {
        Notification {
            id: p.id,
            app_name: p.app_name,
            app_icon: p.app_icon,
            summary: p.summary,
            body: p.body,
            actions: p.actions,
            urgency: p.urgency,
            timestamp: p.timestamp,
            expire_timeout: p.expire_timeout,
            desktop_entry: p.desktop_entry,
            image_path: p.image_path,
            image_data: None, // Binary data is not persisted
            transient: false, // Transient notifications are never persisted
            close_toast_on_close: false,
        }
    }
}

/// Shared, process-wide notification service implementing org.freedesktop.Notifications.
pub struct NotificationService {
    self_weak: Weak<NotificationService>,

    /// D-Bus connection
    bus: RefCell<Option<gio::DBusConnection>>,
    /// Registration ID for the exported interface
    registration_id: RefCell<Option<gio::RegistrationId>>,
    /// Registration ID for the Vibepanel unread-query interface.
    unread_registration_id: RefCell<Option<gio::RegistrationId>>,

    /// Current notifications by ID
    notifications: RefCell<HashMap<u32, Notification>>,
    /// Next notification ID to assign
    next_id: Cell<u32>,

    /// Whether we successfully own the bus name
    backend_available: Cell<bool>,

    /// Whether notifications are muted (toasts suppressed, but notifications still stored)
    muted: Cell<bool>,

    /// Exact read state by ID, independent of wall-clock ordering.
    seen_ids: RefCell<HashSet<u32>>,
    /// Visible toast copies by notification ID. Multiple monitors may show the same toast.
    active_toasts: RefCell<HashMap<u32, u32>>,
    /// Coalesces unread-state listener refreshes on the GLib main loop.
    pending_unread_notify: Cell<bool>,

    /// Callbacks for state changes
    callbacks: Callbacks<NotificationService>,
    /// Whether the service is ready
    ready: Cell<bool>,
}

thread_local! {
    static INSTANCE: Rc<NotificationService> = NotificationService::new();
}

impl NotificationService {
    fn new() -> Rc<Self> {
        // Load persisted state
        let persisted = state::load();
        let notification_state = &persisted.notifications;

        // Restore notifications from persisted state
        let mut notifications = HashMap::new();
        let mut max_id: u32 = 0;
        for pn in &notification_state.history {
            max_id = max_id.max(pn.id);
            notifications.insert(pn.id, Notification::from(pn.clone()));
        }

        // Ensure next_id is greater than any restored notification ID
        let next_id = notification_state.next_id.max(max_id + 1);

        debug!(
            "NotificationService: restored {} notifications, muted={}, next_id={}",
            notifications.len(),
            notification_state.muted,
            next_id
        );

        let seen_ids = notification_state
            .seen_ids
            .iter()
            .copied()
            .filter(|id| notifications.contains_key(id))
            .collect();

        let service = Rc::new_cyclic(|weak| Self {
            self_weak: weak.clone(),
            bus: RefCell::new(None),
            registration_id: RefCell::new(None),
            unread_registration_id: RefCell::new(None),
            notifications: RefCell::new(notifications),
            next_id: Cell::new(next_id),
            backend_available: Cell::new(false),
            muted: Cell::new(notification_state.muted),
            seen_ids: RefCell::new(seen_ids),
            active_toasts: RefCell::new(HashMap::new()),
            pending_unread_notify: Cell::new(false),
            callbacks: Callbacks::new(),
            ready: Cell::new(false),
        });

        // Initialize the xdg-activation token service used by the
        // notifications ActivationToken D-Bus signal.
        super::activation::ActivationService::init_global();
        Self::init_dbus(&service);
        service
    }

    /// Get the global NotificationService singleton.
    pub fn global() -> Rc<Self> {
        INSTANCE.with(Rc::clone)
    }

    /// Register a callback to be invoked when notification state changes.
    ///
    /// Returns a `CallbackId` that can be passed to `disconnect` to unregister
    /// the callback when the caller is dropped.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&NotificationService) + 'static,
    {
        let id = self.callbacks.register(callback);

        // Immediately send current state if ready
        if self.ready.get() {
            self.callbacks.notify_single(id, self);
        }

        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Check if we successfully own the D-Bus name.
    pub fn backend_available(&self) -> bool {
        self.backend_available.get()
    }

    /// Check if notifications are muted (toasts suppressed).
    pub fn is_muted(&self) -> bool {
        self.muted.get()
    }

    /// Set the muted state. When muted, toasts are suppressed but
    /// notifications are still stored and visible in the popover.
    pub fn set_muted(&self, muted: bool) {
        if self.muted.get() != muted {
            debug!("NotificationService: set_muted({})", muted);
            self.muted.set(muted);
            self.save_state();
            self.notify_listeners();
        }
    }

    /// Toggle the muted state.
    pub fn toggle_muted(&self) {
        self.set_muted(!self.muted.get());
    }

    /// Get all notifications as a list.
    pub fn notifications(&self) -> Vec<Notification> {
        self.notifications.borrow().values().cloned().collect()
    }

    /// Notifications excluding transients (which are toast-only per spec).
    pub fn history_notifications(&self) -> Vec<Notification> {
        self.notifications
            .borrow()
            .values()
            .filter(|n| !n.transient)
            .cloned()
            .collect()
    }

    pub fn history_count(&self) -> usize {
        self.notifications
            .borrow()
            .values()
            .filter(|n| !n.transient)
            .count()
    }

    /// Count notifications not seen in a popover and no longer visible as toasts.
    pub fn unread_count(&self) -> usize {
        if !self.backend_available.get() {
            return 0;
        }

        let seen_ids = self.seen_ids.borrow();
        let active_toasts = self.active_toasts.borrow();
        self.notifications
            .borrow()
            .values()
            .filter(|notification| {
                !notification.transient
                    && !active_toasts.contains_key(&notification.id)
                    && !seen_ids.contains(&notification.id)
            })
            .count()
    }

    /// Mark all current notifications as seen globally across monitors.
    pub fn mark_as_seen(&self) {
        let seen_ids = self
            .notifications
            .borrow()
            .values()
            .filter(|notification| !notification.transient)
            .map(|notification| notification.id)
            .collect();

        if *self.seen_ids.borrow() == seen_ids {
            return;
        }

        *self.seen_ids.borrow_mut() = seen_ids;
        self.save_state();
        self.schedule_unread_notify();
    }

    /// Register a visible toast copy for a notification.
    pub fn toast_shown(&self, id: u32) {
        // Toasts are registered before unread state is rendered in the same service
        // update, so scheduling another listener refresh here would be redundant.
        let mut active_toasts = self.active_toasts.borrow_mut();
        *active_toasts.entry(id).or_insert(0) += 1;
    }

    /// Unregister a visible toast copy. Duplicate removals are ignored.
    pub fn toast_hidden(&self, id: u32) {
        let changed = {
            let mut active_toasts = self.active_toasts.borrow_mut();
            match active_toasts.get_mut(&id) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    true
                }
                Some(_) => {
                    active_toasts.remove(&id);
                    true
                }
                None => false,
            }
        };

        if changed {
            self.schedule_unread_notify();
        }
    }

    fn schedule_unread_notify(&self) {
        // With no registered listeners, there is nobody to notify. This also avoids
        // attaching a local source to another thread's main context in unit tests.
        if self.callbacks.is_empty() {
            return;
        }

        if self.pending_unread_notify.replace(true) {
            return;
        }

        let weak = self.self_weak.clone();
        gtk4::glib::idle_add_local_once(move || {
            if let Some(service) = weak.upgrade() {
                service.pending_unread_notify.set(false);
                service.notify_listeners();
            }
        });
    }

    /// Get a notification by ID.
    pub fn get(&self, id: u32) -> Option<Notification> {
        self.notifications.borrow().get(&id).cloned()
    }

    /// Check whether a notification is transient without cloning its payload.
    pub fn is_transient(&self, id: u32) -> bool {
        self.notifications
            .borrow()
            .get(&id)
            .is_some_and(|notification| notification.transient)
    }

    /// Close a notification by ID (user dismissed).
    pub fn close(&self, id: u32) {
        debug!("NotificationService: close() called for id={}", id);
        self.close_internal(id, CLOSE_REASON_DISMISSED);
    }

    /// Close all notifications.
    pub fn close_all(&self) {
        debug!("NotificationService: close_all() called");
        let ids: Vec<u32> = self.notifications.borrow().keys().cloned().collect();
        if ids.is_empty() {
            return;
        }

        for id in ids {
            if self.notifications.borrow_mut().remove(&id).is_some() {
                self.emit_notification_closed(id, CLOSE_REASON_DISMISSED);
            }
        }

        self.save_state();
        self.notify_listeners();
    }

    /// Invoke an action on a notification.
    pub fn invoke_action(&self, id: u32, action_key: &str) {
        debug!(
            "NotificationService: invoke_action() called for id={}, action_key={}",
            id, action_key
        );
        if !self.notifications.borrow().contains_key(&id) {
            return;
        }

        if let Some(token) = Self::create_activation_token() {
            self.emit_activation_token(id, &token);
        }

        self.emit_action_invoked(id, action_key);

        // Close the notification after action is invoked (common behavior)
        self.close_internal(id, CLOSE_REASON_CLOSED);
    }

    fn init_dbus(this: &Rc<Self>) {
        debug!("NotificationService: initializing D-Bus connection");

        let this_weak = Rc::downgrade(this);
        gio::bus_get(
            gio::BusType::Session,
            None::<&gio::Cancellable>,
            move |result| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let connection = match result {
                    Ok(c) => c,
                    Err(e) => {
                        error!("NotificationService: failed to get session bus: {}", e);
                        this.set_ready();
                        return;
                    }
                };

                *this.bus.borrow_mut() = Some(connection.clone());

                // Export interface before trying to own the name
                this.export_interface(&connection);

                // Try to own the name
                this.try_own_name(&connection);
            },
        );
    }

    fn export_interface(&self, connection: &gio::DBusConnection) {
        let node_info = match gio::DBusNodeInfo::for_xml(NOTIFICATIONS_XML) {
            Ok(n) => n,
            Err(e) => {
                error!("NotificationService: failed to parse XML: {}", e);
                return;
            }
        };

        let interface_info = match node_info.lookup_interface(NOTIFICATIONS_NAME) {
            Some(i) => i,
            None => {
                error!("NotificationService: interface not found in XML");
                return;
            }
        };

        // Object registration is connection-local; ownership of the well-known
        // notification bus name is handled separately below.
        let registration = connection
            .register_object(NOTIFICATIONS_PATH, &interface_info)
            .method_call(
                |_connection, _sender, _obj_path, _iface_name, method_name, params, invocation| {
                    let service = NotificationService::global();
                    service.handle_method_call(method_name, &params, invocation);
                },
            )
            .build();

        match registration {
            Ok(id) => {
                *self.registration_id.borrow_mut() = Some(id);
                debug!(
                    "NotificationService: exported interface at {}",
                    NOTIFICATIONS_PATH
                );
            }
            Err(e) => {
                error!("NotificationService: could not register object: {}", e);
            }
        }

        let unread_interface_info =
            match node_info.lookup_interface(VIBEPANEL_NOTIFICATIONS_INTERFACE) {
                Some(i) => i,
                None => {
                    error!("NotificationService: unread interface not found in XML");
                    return;
                }
            };

        let unread_registration = connection
            .register_object(NOTIFICATIONS_PATH, &unread_interface_info)
            .method_call(
                |_connection, _sender, _obj_path, _iface_name, method_name, _params, invocation| {
                    if method_name != UNREAD_METHOD {
                        invocation.return_error(
                            gio::IOErrorEnum::InvalidArgument,
                            &format!("Unknown method: {}", method_name),
                        );
                        return;
                    }

                    let count = NotificationService::global().unread_count();
                    let count = u32::try_from(count).unwrap_or(u32::MAX);
                    invocation.return_value(Some(&(count,).to_variant()));
                },
            )
            .build();

        match unread_registration {
            Ok(id) => {
                *self.unread_registration_id.borrow_mut() = Some(id);
                debug!(
                    "NotificationService: exported {} at {}",
                    VIBEPANEL_NOTIFICATIONS_INTERFACE, NOTIFICATIONS_PATH
                );
            }
            Err(e) => {
                error!(
                    "NotificationService: failed to export unread interface: {}",
                    e
                );
            }
        }
    }

    fn try_own_name(self: &Rc<Self>, connection: &gio::DBusConnection) {
        let this_weak1 = Rc::downgrade(self);
        let this_weak2 = Rc::downgrade(self);

        gio::bus_own_name_on_connection(
            connection,
            NOTIFICATIONS_NAME,
            gio::BusNameOwnerFlags::NONE,
            move |_connection, _name| {
                // Name acquired
                if let Some(this) = this_weak1.upgrade() {
                    this.on_name_acquired();
                }
            },
            move |_connection, _name| {
                // Name lost
                if let Some(this) = this_weak2.upgrade() {
                    this.on_name_lost();
                }
            },
        );
    }

    fn on_name_acquired(&self) {
        info!(
            "NotificationService: acquired {}, acting as notification daemon",
            NOTIFICATIONS_NAME
        );
        self.backend_available.set(true);
        self.set_ready();
        self.notify_listeners();
    }

    fn on_name_lost(&self) {
        if self.backend_available.get() {
            warn!("NotificationService: lost {}", NOTIFICATIONS_NAME);
            self.backend_available.set(false);
        } else {
            warn!(
                "NotificationService: could not acquire {} - another notification daemon is running",
                NOTIFICATIONS_NAME
            );
        }
        self.set_ready();
        self.notify_listeners();
    }

    fn handle_method_call(
        &self,
        method_name: &str,
        params: &Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        match method_name {
            "Notify" => self.handle_notify(params, invocation),
            "CloseNotification" => self.handle_close_notification(params, invocation),
            "GetCapabilities" => self.handle_get_capabilities(invocation),
            "GetServerInformation" => self.handle_get_server_information(invocation),
            _ => {
                invocation.return_error(
                    gio::IOErrorEnum::InvalidArgument,
                    &format!("Unknown method: {}", method_name),
                );
            }
        }
    }

    fn handle_notify(&self, params: &Variant, invocation: gio::DBusMethodInvocation) {
        // Parameters: (susssasa{sv}i)
        // app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout

        if params.n_children() < 8 {
            invocation.return_error(
                gio::IOErrorEnum::InvalidArgument,
                "Notify requires 8 arguments",
            );
            return;
        }

        let app_name = params.child_value(0).str().unwrap_or("Unknown").to_string();
        let replaces_id = params.child_value(1).get::<u32>().unwrap_or(0);
        let app_icon = params.child_value(2).str().unwrap_or("").to_string();
        let summary = params.child_value(3).str().unwrap_or("").to_string();
        let body = params.child_value(4).str().unwrap_or("").to_string();

        // Parse actions array
        let actions_variant = params.child_value(5);
        let mut actions: Vec<(String, String)> = Vec::new();
        let n_actions = actions_variant.n_children();
        let mut i = 0;
        while i + 1 < n_actions {
            let action_id = actions_variant
                .child_value(i)
                .str()
                .unwrap_or("")
                .to_string();
            let action_label = actions_variant
                .child_value(i + 1)
                .str()
                .unwrap_or("")
                .to_string();
            actions.push((action_id, action_label));
            i += 2;
        }

        // Parse hints dict for urgency, desktop-entry and image data
        let hints_variant = params.child_value(6);
        let mut urgency = URGENCY_NORMAL;
        let mut desktop_entry: Option<String> = None;
        let mut image_path: Option<String> = None;
        let mut image_data: Option<NotificationImage> = None;
        let mut transient = false;
        let mut close_toast_on_close = false;
        for j in 0..hints_variant.n_children() {
            let entry = hints_variant.child_value(j);
            if entry.n_children() >= 2
                && let Some(key) = entry.child_value(0).str()
            {
                let value = entry.child_value(1);
                // The value might be wrapped in a variant
                let actual_value = if value.type_().is_variant() {
                    value.child_value(0)
                } else {
                    value
                };

                match key {
                    "urgency" => {
                        if let Some(v) = actual_value.get::<u8>() {
                            urgency = v;
                        } else if let Some(v) = actual_value.get::<i32>() {
                            urgency = v.clamp(0, 2) as u8;
                        } else if let Some(v) = actual_value.get::<u32>() {
                            urgency = v.clamp(0, 2) as u8;
                        }
                    }
                    "desktop-entry" => {
                        if let Some(v) = actual_value.str() {
                            let v = v.to_string();
                            if !v.is_empty() {
                                desktop_entry = Some(v);
                            }
                        }
                    }
                    "image-path" => {
                        if let Some(v) = actual_value.str() {
                            let v = v.to_string();
                            if !v.is_empty() {
                                image_path = Some(v);
                            }
                        }
                    }
                    "image-data" => {
                        // freedesktop.org spec: (iiibiiay)
                        if let Some((w, h, row, alpha, _bps, ch, bytes)) =
                            actual_value.get::<(i32, i32, i32, bool, i32, i32, Vec<u8>)>()
                        {
                            image_data = Some(NotificationImage {
                                width: w,
                                height: h,
                                rowstride: row,
                                has_alpha: alpha,
                                channels: ch,
                                data: bytes,
                            });
                        }
                    }
                    "transient" => {
                        // Spec allows boolean or numeric (any non-zero) values.
                        if let Some(v) = actual_value.get::<bool>() {
                            transient = v;
                        } else if let Some(v) = actual_value.get::<u8>() {
                            transient = v != 0;
                        } else if let Some(v) = actual_value.get::<i32>() {
                            transient = v != 0;
                        } else if let Some(v) = actual_value.get::<u32>() {
                            transient = v != 0;
                        }
                    }
                    "x-vibepanel-close-toast-on-close" => {
                        if let Some(v) = actual_value.get::<bool>() {
                            close_toast_on_close = v;
                        } else if let Some(v) = actual_value.get::<u8>() {
                            close_toast_on_close = v != 0;
                        } else if let Some(v) = actual_value.get::<i32>() {
                            close_toast_on_close = v != 0;
                        } else if let Some(v) = actual_value.get::<u32>() {
                            close_toast_on_close = v != 0;
                        }
                    }
                    _ => {}
                }
            }
        }

        let expire_timeout = params.child_value(7).get::<i32>().unwrap_or(-1);

        // Determine notification ID
        let id = if replaces_id != 0 && self.notifications.borrow().contains_key(&replaces_id) {
            replaces_id
        } else {
            let id = self.next_id.get();
            self.next_id.set(id.wrapping_add(1));
            if self.next_id.get() == 0 {
                self.next_id.set(1); // Avoid 0
            }
            id
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let notification = Notification {
            id,
            app_name: if app_name.is_empty() {
                "Unknown".to_string()
            } else {
                app_name
            },
            app_icon,
            summary,
            body,
            actions,
            urgency,
            timestamp,
            expire_timeout,
            desktop_entry,
            image_path,
            image_data,
            transient,
            close_toast_on_close,
        };

        debug!(
            "NotificationService: notification {}: {} - {} (expire_timeout={}ms, urgency={})",
            id,
            notification.app_name,
            notification.summary,
            notification.expire_timeout,
            notification.urgency
        );

        let is_transient = notification.transient;

        self.store_notification(notification);

        // Return the notification ID
        invocation.return_value(Some(&(id,).to_variant()));

        // Muted transients never get a toast and never appear in the popover,
        // so without this they'd linger in the map until evicted.
        if is_transient && self.is_muted() {
            self.close_internal(id, CLOSE_REASON_DISMISSED);
        }
    }

    fn store_notification(&self, notification: Notification) {
        self.seen_ids.borrow_mut().remove(&notification.id);
        self.notifications
            .borrow_mut()
            .insert(notification.id, notification);
        self.enforce_notification_limit();
        self.save_state();
        self.notify_listeners();
    }

    fn handle_close_notification(&self, params: &Variant, invocation: gio::DBusMethodInvocation) {
        let id = params.child_value(0).get::<u32>().unwrap_or(0);
        debug!(
            "NotificationService: CloseNotification D-Bus method called for id={}",
            id
        );
        self.close_internal(id, CLOSE_REASON_CLOSED);
        invocation.return_value(None);
    }

    fn handle_get_capabilities(&self, invocation: gio::DBusMethodInvocation) {
        let caps: Vec<&str> = CAPABILITIES.to_vec();
        invocation.return_value(Some(&(caps,).to_variant()));
    }

    fn handle_get_server_information(&self, invocation: gio::DBusMethodInvocation) {
        invocation.return_value(Some(
            &(
                "vibepanel", // name
                "vibepanel", // vendor
                "1.0",       // version
                "1.2",       // Desktop Notifications spec version
            )
                .to_variant(),
        ));
    }

    fn close_internal(&self, id: u32, reason: u32) {
        if self.notifications.borrow_mut().remove(&id).is_none() {
            return;
        }

        self.emit_notification_closed(id, reason);
        self.save_state();
        self.notify_listeners();
    }

    /// Trim oldest history once the cap is exceeded. Transients are owned by
    /// the toast manager — they don't count toward the cap and aren't evicted.
    fn enforce_notification_limit(&self) {
        let mut notifications = self.notifications.borrow_mut();

        let history_count = notifications.values().filter(|n| !n.transient).count();
        if history_count <= MAX_NOTIFICATIONS {
            return;
        }

        let mut by_time: Vec<(u32, f64)> = notifications
            .iter()
            .filter(|(_, n)| !n.transient)
            .map(|(id, n)| (*id, n.timestamp))
            .collect();
        by_time.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let to_remove = history_count - MAX_NOTIFICATIONS;
        for (id, _) in by_time.into_iter().take(to_remove) {
            notifications.remove(&id);
            debug!(
                "NotificationService: evicted old notification id={} (limit={})",
                id, MAX_NOTIFICATIONS
            );
        }
    }

    fn emit_notification_closed(&self, id: u32, reason: u32) {
        debug!(
            "NotificationService: emitting NotificationClosed signal for id={}, reason={}",
            id, reason
        );
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS_NAME,
            "NotificationClosed",
            Some(&(id, reason).to_variant()),
        ) {
            error!(
                "NotificationService: failed to emit NotificationClosed: {}",
                e
            );
        }
    }

    fn create_activation_token() -> Option<String> {
        super::activation::ActivationService::global()?.create_token()
    }

    fn emit_activation_token(&self, id: u32, token: &str) {
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS_NAME,
            "ActivationToken",
            Some(&(id, token).to_variant()),
        ) {
            error!("NotificationService: failed to emit ActivationToken: {}", e);
        }
    }

    fn emit_action_invoked(&self, id: u32, action_key: &str) {
        debug!(
            "NotificationService: emitting ActionInvoked signal for id={}, action_key={}",
            id, action_key
        );
        let Some(ref bus) = *self.bus.borrow() else {
            return;
        };

        if let Err(e) = bus.emit_signal(
            None::<&str>,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS_NAME,
            "ActionInvoked",
            Some(&(id, action_key).to_variant()),
        ) {
            error!("NotificationService: failed to emit ActionInvoked: {}", e);
        }
    }

    fn set_ready(&self) {
        if !self.ready.get() {
            self.ready.set(true);
            self.notify_listeners();
        }
    }

    fn notify_listeners(&self) {
        self.callbacks.notify(self);
    }

    /// Save current notification state to disk.
    fn save_state(&self) {
        // Load existing state to preserve VPN state
        let mut persisted = state::load();

        // Update notification state. Transient notifications bypass persistence.
        let notifications = self.notifications.borrow();
        let mut history: Vec<PersistedNotification> = notifications
            .values()
            .filter(|n| !n.transient)
            .map(|n| n.to_persisted())
            .collect();

        // Sort by timestamp descending (most recent first)
        history.sort_by(|a, b| {
            b.timestamp
                .partial_cmp(&a.timestamp)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        persisted.notifications.muted = self.muted.get();
        persisted.notifications.next_id = self.next_id.get();
        let mut seen_ids = self.seen_ids.borrow_mut();
        seen_ids.retain(|id| notifications.contains_key(id));
        let mut persisted_seen_ids: Vec<u32> = seen_ids.iter().copied().collect();
        persisted_seen_ids.sort_unstable();
        drop(seen_ids);
        persisted.notifications.seen_ids = persisted_seen_ids;
        persisted.notifications.history = history;

        state::save(&persisted);
    }
}

impl Drop for NotificationService {
    fn drop(&mut self) {
        debug!("NotificationService dropped");
    }
}

/// Query the unread count from whichever process owns the notification D-Bus name.
pub fn unread_notification_count() -> Result<u32, String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|error| format!("could not connect to the session bus: {}", error))?;
    let reply = connection.call_sync(
        Some(NOTIFICATIONS_NAME),
        NOTIFICATIONS_PATH,
        VIBEPANEL_NOTIFICATIONS_INTERFACE,
        UNREAD_METHOD,
        None,
        Some(&<(u32,)>::static_variant_type()),
        gio::DBusCallFlags::NO_AUTO_START,
        DBUS_CALL_TIMEOUT_MS,
        None::<&gio::Cancellable>,
    );

    match reply {
        Ok(reply) => reply
            .get::<(u32,)>()
            .map(|(count,)| count)
            .ok_or_else(|| "notification daemon returned an invalid unread count".to_string()),
        Err(error) => match error.kind::<gio::DBusError>() {
            Some(gio::DBusError::ServiceUnknown | gio::DBusError::NameHasNoOwner) => {
                Err("no notification daemon is running".to_string())
            }
            Some(gio::DBusError::UnknownInterface | gio::DBusError::UnknownMethod) => Err(
                "the active notification daemon does not support Vibepanel unread queries"
                    .to_string(),
            ),
            _ => Err(format!(
                "could not query the active notification daemon: {}",
                error
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redirect XDG_STATE_HOME to a per-process tempdir so save_state writes
    /// don't clobber the developer's real notification state.
    fn redirect_state_home() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let tmp =
                std::env::temp_dir().join(format!("vibepanel-notif-test-{}", std::process::id()));
            // SAFETY: This runs exactly once before any test reads the env var,
            // and the redirect is harmless to any other test in this binary -
            // they'll just see a clean per-process state file location.
            unsafe {
                std::env::set_var("XDG_STATE_HOME", &tmp);
            }
        });
    }

    fn make_service() -> Rc<NotificationService> {
        redirect_state_home();
        Rc::new_cyclic(|weak| NotificationService {
            self_weak: weak.clone(),
            bus: RefCell::new(None),
            registration_id: RefCell::new(None),
            unread_registration_id: RefCell::new(None),
            notifications: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            backend_available: Cell::new(false),
            muted: Cell::new(false),
            seen_ids: RefCell::new(HashSet::new()),
            active_toasts: RefCell::new(HashMap::new()),
            pending_unread_notify: Cell::new(false),
            callbacks: Callbacks::new(),
            ready: Cell::new(false),
        })
    }

    fn make_notification(id: u32, transient: bool, timestamp: f64) -> Notification {
        Notification {
            id,
            app_name: "test".to_string(),
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            urgency: URGENCY_NORMAL,
            timestamp,
            expire_timeout: -1,
            desktop_entry: None,
            image_path: None,
            image_data: None,
            transient,
            close_toast_on_close: false,
        }
    }

    #[test]
    fn unread_query_wire_contract_matches_introspection() {
        let node_info = gio::DBusNodeInfo::for_xml(NOTIFICATIONS_XML).unwrap();
        let interface = node_info
            .lookup_interface(VIBEPANEL_NOTIFICATIONS_INTERFACE)
            .unwrap();
        assert!(interface.lookup_method(UNREAD_METHOD).is_some());
    }

    #[test]
    fn activation_token_signal_is_exposed() {
        let node_info = gio::DBusNodeInfo::for_xml(NOTIFICATIONS_XML).unwrap();
        let interface = node_info.lookup_interface(NOTIFICATIONS_NAME).unwrap();
        assert!(interface.lookup_signal("ActivationToken").is_some());
    }

    /// Mirror of the post-insert tail in handle_notify, which we can't call
    /// directly because it consumes D-Bus types.
    fn simulate_handle_notify(svc: &NotificationService, n: Notification) {
        let id = n.id;
        let is_transient = n.transient;
        svc.store_notification(n);
        if is_transient && svc.is_muted() {
            svc.close_internal(id, CLOSE_REASON_DISMISSED);
        }
    }

    #[test]
    fn muted_transient_is_dropped_from_map() {
        let svc = make_service();
        svc.muted.set(true);

        simulate_handle_notify(&svc, make_notification(1, true, 1.0));

        assert!(
            !svc.notifications.borrow().contains_key(&1),
            "muted transient should not linger in the map"
        );
    }

    #[test]
    fn unmuted_transient_remains_until_toast_lifecycle_ends() {
        let svc = make_service();
        // muted = false (default)

        simulate_handle_notify(&svc, make_notification(1, true, 1.0));

        assert!(
            svc.notifications.borrow().contains_key(&1),
            "unmuted transient must stay in the map - the toast manager closes it on dismiss/timeout"
        );
    }

    #[test]
    fn muted_non_transient_remains_in_map() {
        let svc = make_service();
        svc.muted.set(true);

        simulate_handle_notify(&svc, make_notification(1, false, 1.0));

        assert!(
            svc.notifications.borrow().contains_key(&1),
            "muted non-transients are stored as history (only toasts are suppressed)"
        );
    }

    #[test]
    fn enforce_limit_excludes_transients_from_count() {
        let svc = make_service();
        // Fill exactly to the cap with history, then add transients on top.
        for i in 0..(MAX_NOTIFICATIONS as u32) {
            svc.notifications
                .borrow_mut()
                .insert(i + 1, make_notification(i + 1, false, i as f64));
        }
        for i in 0..50u32 {
            let id = MAX_NOTIFICATIONS as u32 + 100 + i;
            svc.notifications
                .borrow_mut()
                .insert(id, make_notification(id, true, (1000 + i) as f64));
        }

        svc.enforce_notification_limit();

        assert_eq!(
            svc.notifications.borrow().len(),
            MAX_NOTIFICATIONS + 50,
            "transients should not trigger eviction even when total > cap"
        );
    }

    #[test]
    fn enforce_limit_evicts_oldest_history_only() {
        let svc = make_service();
        // 102 history (timestamps 0..102) + 5 transients with very old timestamps.
        for i in 0..102u32 {
            svc.notifications
                .borrow_mut()
                .insert(i + 1, make_notification(i + 1, false, i as f64));
        }
        for i in 0..5u32 {
            let id = 1000 + i;
            // Older than any history - would be first evicted if transients counted.
            svc.notifications
                .borrow_mut()
                .insert(id, make_notification(id, true, -100.0 - i as f64));
        }

        svc.enforce_notification_limit();

        let map = svc.notifications.borrow();
        // History over the cap (ids 1, 2 = oldest two) should be evicted.
        assert!(
            !map.contains_key(&1),
            "oldest history (id=1) should be evicted"
        );
        assert!(
            !map.contains_key(&2),
            "second-oldest history (id=2) should be evicted"
        );
        assert!(map.contains_key(&3), "third-oldest history must survive");
        // All transients survive despite older timestamps.
        for i in 0..5u32 {
            assert!(
                map.contains_key(&(1000 + i)),
                "transient id={} must not be evicted",
                1000 + i
            );
        }
    }

    #[test]
    fn unread_count_uses_seen_ids_and_excludes_transients() {
        let svc = make_service();
        svc.backend_available.set(true);
        svc.notifications
            .borrow_mut()
            .insert(1, make_notification(1, false, 10.0));
        svc.notifications
            .borrow_mut()
            .insert(2, make_notification(2, false, 20.0));
        svc.notifications
            .borrow_mut()
            .insert(3, make_notification(3, true, 30.0));

        assert_eq!(svc.unread_count(), 2);
        svc.seen_ids.borrow_mut().insert(1);
        assert_eq!(svc.unread_count(), 1);
        svc.seen_ids.borrow_mut().insert(2);
        assert_eq!(svc.unread_count(), 0);
    }

    #[test]
    fn unread_count_is_zero_without_notification_backend() {
        let svc = make_service();
        svc.notifications
            .borrow_mut()
            .insert(1, make_notification(1, false, 10.0));

        assert_eq!(svc.unread_count(), 0);
    }

    #[test]
    fn active_toasts_are_reference_counted_across_monitors() {
        let svc = make_service();
        svc.backend_available.set(true);
        svc.notifications
            .borrow_mut()
            .insert(1, make_notification(1, false, 10.0));

        svc.toast_shown(1);
        svc.toast_shown(1);
        assert_eq!(svc.unread_count(), 0);

        svc.toast_hidden(1);
        assert_eq!(svc.unread_count(), 0);

        svc.toast_hidden(1);
        assert_eq!(svc.unread_count(), 1);

        svc.toast_hidden(1);
        assert_eq!(svc.unread_count(), 1);
        assert!(!svc.active_toasts.borrow().contains_key(&1));
    }

    #[test]
    fn mark_as_seen_clears_current_unread_notifications() {
        let svc = make_service();
        svc.backend_available.set(true);
        svc.notifications
            .borrow_mut()
            .insert(1, make_notification(1, false, 1.0));

        assert_eq!(svc.unread_count(), 1);
        svc.mark_as_seen();
        assert_eq!(svc.unread_count(), 0);
        assert!(svc.seen_ids.borrow().contains(&1));
    }

    #[test]
    fn replacement_marks_notification_unread_again() {
        let svc = make_service();
        svc.backend_available.set(true);
        simulate_handle_notify(&svc, make_notification(1, false, 1.0));
        svc.mark_as_seen();

        simulate_handle_notify(&svc, make_notification(1, false, 2.0));

        assert_eq!(svc.unread_count(), 1);
        assert!(!svc.seen_ids.borrow().contains(&1));
    }
}
