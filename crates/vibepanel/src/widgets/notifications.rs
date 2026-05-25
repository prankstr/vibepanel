//! Notification widget - displays a bell icon with badge and provides toast popups.
//!
//! Features:
//! - Bell icon with unread notification badge
//! - CSS states: has-notifications, has-critical, backend-unavailable
//! - Popover with scrollable notification list and dismiss controls
//! - Toast overlay windows for new notifications (configurable screen position)
//!
//! This module is split into several files for maintainability:
//! - `notifications.rs` (this file): Widget implementation and badge logic
//! - `notifications_toast.rs`: Toast window management and queue
//! - `notifications_popover.rs`: Popover content and notification list
//! - `notifications_common.rs`: Shared constants and helper functions

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Application, Box as GtkBox, Orientation, Overlay, Widget};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::icons::IconHandle;
use crate::services::notification::{NotificationService, URGENCY_CRITICAL};
use crate::services::tooltip::TooltipManager;
use crate::styles::widget;
use crate::widgets::base::MenuHandle;
use crate::widgets::{BaseWidget, WidgetConfig, warn_unknown_options};

use super::notifications_popover::{ClosePopoverCallback, build_popover_content};
use super::notifications_toast::{NotificationToastManager, ToastPosition};

/// Configuration for the notification widget.
#[derive(Debug, Clone, Default)]
pub struct NotificationsConfig {
    toast_position: ToastPosition,
}

impl WidgetConfig for NotificationsConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("notifications", entry, &["toast_position"]);

        let toast_position = entry
            .options
            .get("toast_position")
            .and_then(|v| v.as_str())
            .and_then(|value| {
                let position = ToastPosition::parse(value);
                if position.is_none() {
                    warn!(
                        "Invalid toast_position '{}' for widget 'notifications' - expected one of: top-right, top-center, top-left, bottom-right, bottom-center, bottom-left; using top-right",
                        value
                    );
                }
                position
            })
            .unwrap_or_default();

        Self { toast_position }
    }
}

/// Shared inner state for the notification widget.
///
/// This is wrapped in Rc<RefCell<...>> to allow safe sharing with callbacks.
struct NotificationsWidgetInner {
    icon_handle: IconHandle,
    badge: Widget,
    container: GtkBox,
    /// Last-seen notification ids mapped to their timestamp. A bump in timestamp
    /// for an existing id means the notification was replaced via replaces_id and
    /// the toast should be re-shown with the new content.
    known_ids: RefCell<HashMap<u32, f64>>,
    toast_manager: RefCell<Option<Rc<NotificationToastManager>>>,
    last_seen_timestamp: Cell<f64>,
    app: RefCell<Option<Application>>,
    menu_handle: RefCell<Option<Rc<MenuHandle>>>,
    /// Last known (id, timestamp) snapshot. Used to skip popover rebuilds on
    /// mute-only changes while still detecting replacements (same id, newer
    /// timestamp via replaces_id) as a content change.
    last_notif_ids: RefCell<Vec<(u32, f64)>>,
    /// When set, the popover dismiss handler already removed the row in-place,
    /// so `on_service_update` should skip `refresh_if_visible`.
    suppress_rebuild: Rc<Cell<bool>>,
}

impl NotificationsWidgetInner {
    fn on_service_update(&self, service: &NotificationService) {
        let count = service.history_count();
        debug!(
            "NotificationsWidget: on_service_update called, count={}",
            count
        );

        // Show toasts for new notifications
        self.show_new_toasts(service);

        // Update badge: unread since last popover open
        // Badge is shown as a simple dot (no text), count is only in tooltip
        let unread = self.calculate_unread_count(service);
        debug!("NotificationsWidget: unread count = {}", unread);
        if unread > 0 {
            self.badge.set_visible(true);
        } else {
            self.badge.set_visible(false);
        }

        let has_critical = service
            .history_notifications()
            .iter()
            .any(|n| n.urgency == URGENCY_CRITICAL);

        if has_critical {
            self.icon_handle.add_css_class(widget::HAS_CRITICAL);
        } else {
            self.icon_handle.remove_css_class(widget::HAS_CRITICAL);
        }

        // Update backend availability visual state
        let tooltip_manager = TooltipManager::global();
        if !service.backend_available() {
            self.icon_handle.add_css_class(widget::BACKEND_UNAVAILABLE);
            tooltip_manager.set_styled_tooltip(
                &self.container,
                "Notification daemon unavailable (another daemon is running)",
            );
        } else {
            self.icon_handle
                .remove_css_class(widget::BACKEND_UNAVAILABLE);

            // Update icon based on mute state
            if service.is_muted() {
                self.icon_handle.set_icon("notifications-disabled");
            } else {
                self.icon_handle.set_icon("notifications");
            }

            if count > 0 {
                // Show unread count in tooltip (badge is just a dot)
                let tooltip = if unread > 0 {
                    if unread == 1 {
                        format!("1 new notification ({} total)", count)
                    } else {
                        format!("{} new notifications ({} total)", unread, count)
                    }
                } else if count == 1 {
                    "1 notification".to_string()
                } else {
                    format!("{} notifications", count)
                };
                tooltip_manager.set_styled_tooltip(&self.container, &tooltip);
            } else {
                tooltip_manager.set_styled_tooltip(&self.container, "No notifications");
            }
        }

        // Only rebuild the popover when the notification list changed; mute-only
        // updates are handled in-place by the popover button. Compare (id, ts)
        // so a replaces_id update (same id, newer ts) is also a content change.
        let mut current_ids: Vec<(u32, f64)> = service
            .history_notifications()
            .iter()
            .map(|n| (n.id, n.timestamp))
            .collect();
        current_ids.sort_unstable_by_key(|a| a.0);
        let list_changed = *self.last_notif_ids.borrow() != current_ids;
        *self.last_notif_ids.borrow_mut() = current_ids;

        if list_changed {
            // If the popover dismiss handler already removed the row in-place,
            // it set suppress_rebuild so we skip the full window rebuild.
            if self.suppress_rebuild.replace(false) {
                return;
            }
            if let Some(menu_handle) = self.menu_handle.borrow().as_ref() {
                menu_handle.refresh_if_visible();
            }
        }
    }

    fn calculate_unread_count(&self, service: &NotificationService) -> usize {
        if !service.backend_available() {
            debug!("NotificationsWidget: backend not available, returning 0");
            return 0;
        }

        let active_toast_ids = self
            .toast_manager
            .borrow()
            .as_ref()
            .map(|tm| tm.active_ids())
            .unwrap_or_default();

        let last_seen = self.last_seen_timestamp.get();

        debug!(
            "NotificationsWidget: calculate_unread_count - active_toast_ids={:?}, last_seen={}, notifications_count={}",
            active_toast_ids,
            last_seen,
            service.history_count()
        );

        service
            .history_notifications()
            .iter()
            .filter(|n| {
                // Skip if currently shown as toast
                if active_toast_ids.contains(&n.id) {
                    debug!("NotificationsWidget: skipping {} (active toast)", n.id);
                    return false;
                }

                // First run (never opened): count all non-toasted as unread
                if last_seen <= 0.0 {
                    debug!(
                        "NotificationsWidget: counting {} (never opened popover)",
                        n.id
                    );
                    return true;
                }

                // Count if delivered after last seen
                let is_unread = n.timestamp > last_seen;
                debug!(
                    "NotificationsWidget: {} timestamp={} > last_seen={} = {}",
                    n.id, n.timestamp, last_seen, is_unread
                );
                is_unread
            })
            .count()
    }

    fn show_new_toasts(&self, service: &NotificationService) {
        if !service.backend_available() {
            return;
        }

        // Snapshot current id -> timestamp so we can detect both new notifications
        // and replacements (same id, newer timestamp via replaces_id).
        let current: HashMap<u32, f64> = service
            .notifications()
            .iter()
            .map(|n| (n.id, n.timestamp))
            .collect();

        // Don't show toasts when muted
        if service.is_muted() {
            // Still update known IDs so we don't show stale toasts when unmuted
            *self.known_ids.borrow_mut() = current;
            return;
        }

        let known = self.known_ids.borrow().clone();

        // Identify ids to (re)toast: brand-new ids, plus replacements where the
        // timestamp has advanced since we last toasted.
        let to_toast: Vec<u32> = current
            .iter()
            .filter(|(id, ts)| match known.get(id) {
                None => true,
                Some(prev_ts) => *ts > prev_ts,
            })
            .map(|(id, _)| *id)
            .collect();

        // Note: We intentionally do NOT close toasts when notifications are removed.
        // Some apps (like Telegram) send a notification and then immediately close it,
        // expecting the notification daemon to still show it briefly. If we closed the
        // toast here, users would never see the notification.
        // Toasts will close naturally via their timeout or user dismissal.

        // Show toasts for new and replaced notifications
        if !to_toast.is_empty() {
            // Try to get the application from the widget's root window
            let app = self.get_application();

            // Lazily create toast manager - but we need to do this outside show_new_toasts
            // because we can't get a callback to self from here. See bind_service for the
            // proper initialization with callbacks.

            if let (Some(toast_manager), Some(app)) = (&*self.toast_manager.borrow(), app) {
                let monitor = self.toast_monitor();
                for id in &to_toast {
                    if let Some(notification) = service.get(*id) {
                        toast_manager.show(&app, monitor.as_ref(), &notification);
                    }
                }
            }
        }

        // Update known IDs (with current timestamps)
        *self.known_ids.borrow_mut() = current;
    }

    /// Get the GTK Application from the widget's root window.
    fn get_application(&self) -> Option<Application> {
        // First check the cached app
        if let Some(app) = self.app.borrow().as_ref() {
            return Some(app.clone());
        }

        // Try to get from the widget's root
        let root = self.container.root()?;
        let window = root.downcast_ref::<gtk4::Window>()?;
        let app = window.application()?;

        // Cache it
        *self.app.borrow_mut() = Some(app.clone());
        Some(app)
    }

    fn toast_monitor(&self) -> Option<gtk4::gdk::Monitor> {
        self.container
            .root()
            .and_then(|root| root.downcast_ref::<gtk4::Window>().cloned())
            .and_then(|window| window.surface())
            .and_then(|surface| {
                gtk4::gdk::Display::default()
                    .and_then(|display| display.monitor_at_surface(&surface))
            })
    }

    /// Mark notifications as seen (called when popover opens).
    fn mark_as_seen(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.last_seen_timestamp.set(now);
    }
}

/// Notification bell widget with popover showing notification list.
pub struct NotificationsWidget {
    base: BaseWidget,
    /// Kept for ownership: Weak references inside service/toast closures only
    /// remain valid as long as this Rc is alive.
    #[allow(dead_code)]
    inner: Rc<NotificationsWidgetInner>,
    /// Callback ID for NotificationService, used to disconnect on drop.
    service_callback_id: CallbackId,
}

impl NotificationsWidget {
    /// Create a new notification widget.
    pub fn new(config: NotificationsConfig) -> Self {
        let base = BaseWidget::new(&[widget::NOTIFICATIONS]);

        // Create an overlay for badge on top of icon
        let overlay = Overlay::new();
        overlay.set_halign(Align::Center);
        overlay.set_valign(Align::Center);
        overlay.set_vexpand(true);

        // Bell icon - use logical name that maps to Material "notifications" or GTK equivalent
        let icon_handle = base.add_icon("notifications", &[widget::NOTIFICATION_ICON]);

        // Remove icon from content box and put in overlay instead
        base.content().remove(&icon_handle.widget());
        overlay.set_child(Some(&icon_handle.widget()));

        // Badge indicator dot (hidden by default)
        // Use a fixed-size Box instead of Label to avoid text metric issues
        let badge = GtkBox::new(Orientation::Horizontal, 0);
        badge.add_css_class(widget::NOTIFICATION_BADGE);
        badge.add_css_class(widget::NOTIFICATION_BADGE_DOT);
        badge.set_visible(false);
        badge.set_halign(Align::End);
        badge.set_valign(Align::Start);
        // Set explicit size request to ensure square shape
        badge.set_size_request(8, 8);
        overlay.add_overlay(&badge);

        base.content().append(&overlay);

        base.set_tooltip("Notifications");

        let inner = Rc::new(NotificationsWidgetInner {
            icon_handle,
            badge: badge.upcast(),
            container: base.widget().clone(),
            known_ids: RefCell::new(HashMap::new()),
            toast_manager: RefCell::new(None),
            last_seen_timestamp: Cell::new(0.0),
            app: RefCell::new(None),
            menu_handle: RefCell::new(None),
            last_notif_ids: RefCell::new(Vec::new()),
            suppress_rebuild: Rc::new(Cell::new(false)),
        });

        // Build menu before constructing Self so we can move base/inner cleanly.
        let suppress_rebuild = Rc::clone(&inner.suppress_rebuild);
        let inner_for_menu = Rc::clone(&inner);
        let menu_handle = base.create_menu(|| GtkBox::new(Orientation::Vertical, 0).into());
        let handle_weak = Rc::downgrade(&menu_handle);
        menu_handle.set_builder_with_monitor(move |monitor| {
            inner_for_menu.mark_as_seen();
            let on_close: Option<ClosePopoverCallback> = handle_weak
                .upgrade()
                .map(|handle| Rc::new(move || handle.hide()) as ClosePopoverCallback);
            build_popover_content(on_close, Rc::clone(&suppress_rebuild), monitor)
        });
        *inner.menu_handle.borrow_mut() = Some(menu_handle);

        // Seed known_ids with the persistence-restored set. DBus deliveries
        // cannot arrive before bind time, so this is the full initial set.
        let service = NotificationService::global();
        *inner.known_ids.borrow_mut() = service
            .notifications()
            .iter()
            .map(|n| (n.id, n.timestamp))
            .collect();

        // Initialize toast manager. Closures use Weak to avoid keeping inner
        // alive after the widget is dropped.
        {
            let service_for_action = NotificationService::global();
            let on_action = move |id: u32, action_id: &str| {
                service_for_action.invoke_action(id, action_id);
            };

            // When a toast is removed (dismissed or timed out), we need to recalculate
            // the badge. However, we must NOT call on_service_update directly here
            // because that would cause infinite recursion:
            //   action → invoke_action → notify_listeners → on_service_update
            //   → show_new_toasts → close toast → on_toast_removed → on_service_update → ...
            //
            // Instead, we use idle_add to defer the update to the next main loop iteration.
            // This breaks the synchronous call chain and prevents stack overflow.
            let inner_weak_for_toast = Rc::downgrade(&inner);
            let on_toast_removed = move || {
                let inner_weak = inner_weak_for_toast.clone();
                glib::idle_add_local_once(move || {
                    if let Some(inner) = inner_weak.upgrade() {
                        let service = NotificationService::global();
                        inner.on_service_update(&service);
                    }
                });
            };

            let manager =
                NotificationToastManager::new(on_action, on_toast_removed, config.toast_position);
            *inner.toast_manager.borrow_mut() = Some(manager);
        }

        // Connect to notification service using Weak to avoid keeping inner alive
        // after the widget is dropped. The returned CallbackId is stored on the
        // outer struct and disconnected in Drop.
        let service_callback_id = {
            let inner_weak = Rc::downgrade(&inner);
            service.connect(move |svc| {
                if let Some(inner) = inner_weak.upgrade() {
                    inner.on_service_update(svc);
                }
            })
        };

        Self {
            base,
            inner,
            service_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }
}

impl Drop for NotificationsWidget {
    fn drop(&mut self) {
        NotificationService::global().disconnect(self.service_callback_id);
    }
}

impl Default for NotificationsWidget {
    fn default() -> Self {
        Self::new(NotificationsConfig::default())
    }
}
