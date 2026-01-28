//! Notification widget - displays a bell icon with badge and provides toast popups.
//!
//! Features:
//! - Bell icon with unread notification badge
//! - CSS states: has-notifications, has-critical, backend-unavailable
//! - Popover with scrollable notification list and dismiss controls
//! - Toast overlay windows for new notifications (top-right stacked)
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
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;
use vibepanel_core::config::WidgetEntry;

use crate::services::icons::IconHandle;
use crate::services::notification::{NotificationService, URGENCY_CRITICAL};
use crate::services::tooltip::TooltipManager;
use crate::styles::widget;
use crate::widgets::base::MenuHandle;
use crate::widgets::{BaseWidget, WidgetConfig};

use super::notifications_popover::{ClosePopoverCallback, build_popover_content};
use super::notifications_toast::NotificationToastManager;

/// Configuration for the notification widget.
#[derive(Debug, Clone, Default)]
pub struct NotificationsConfig {}

impl WidgetConfig for NotificationsConfig {
    fn from_entry(_entry: &WidgetEntry) -> Self {
        Self {}
    }
}

/// Shared inner state for the notification widget.
///
/// This is wrapped in Rc<RefCell<...>> to allow safe sharing with callbacks.
struct NotificationsWidgetInner {
    icon_handle: IconHandle,
    badge: Widget,
    container: GtkBox,
    known_ids: RefCell<HashSet<u32>>,
    toast_manager: RefCell<Option<Rc<NotificationToastManager>>>,
    last_seen_timestamp: Cell<f64>,
    app: RefCell<Option<Application>>,
    menu_handle: RefCell<Option<Rc<MenuHandle>>>,
}

impl NotificationsWidgetInner {
    fn on_service_update(&self, service: &NotificationService) {
        let count = service.count();
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

        // Check for critical notifications
        let has_critical = service
            .notifications()
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

        // Refresh popover content if visible
        if let Some(menu_handle) = self.menu_handle.borrow().as_ref() {
            menu_handle.refresh_if_visible();
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
            service.notifications().len()
        );

        service
            .notifications()
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

        // Don't show toasts when muted
        if service.is_muted() {
            // Still update known IDs so we don't show stale toasts when unmuted
            let current_ids: HashSet<u32> = service.notifications().iter().map(|n| n.id).collect();
            *self.known_ids.borrow_mut() = current_ids;
            return;
        }

        let current_ids: HashSet<u32> = service.notifications().iter().map(|n| n.id).collect();
        let known_ids = self.known_ids.borrow().clone();

        let new_ids: HashSet<u32> = current_ids.difference(&known_ids).cloned().collect();
        // Note: We intentionally do NOT close toasts when notifications are removed.
        // Some apps (like Telegram) send a notification and then immediately close it,
        // expecting the notification daemon to still show it briefly. If we closed the
        // toast here, users would never see the notification.
        // Toasts will close naturally via their timeout or user dismissal.

        // Show toasts for new notifications
        if !new_ids.is_empty() {
            // Try to get the application from the widget's root window
            let app = self.get_application();

            // Lazily create toast manager - but we need to do this outside show_new_toasts
            // because we can't get a callback to self from here. See bind_service for the
            // proper initialization with callbacks.

            if let (Some(toast_manager), Some(app)) = (&*self.toast_manager.borrow(), app) {
                for id in &new_ids {
                    if let Some(notification) = service.get(*id) {
                        toast_manager.show(&app, &notification);
                    }
                }
            }
        }

        // Update known IDs
        *self.known_ids.borrow_mut() = current_ids;
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
    inner: Rc<NotificationsWidgetInner>,
}

impl NotificationsWidget {
    /// Create a new notification widget.
    pub fn new(_config: NotificationsConfig) -> Self {
        let base = BaseWidget::new(&[widget::NOTIFICATIONS]);

        // Create an overlay for badge on top of icon
        let overlay = Overlay::new();
        overlay.set_valign(Align::Center);

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
            known_ids: RefCell::new(HashSet::new()),
            toast_manager: RefCell::new(None),
            last_seen_timestamp: Cell::new(0.0),
            app: RefCell::new(None),
            menu_handle: RefCell::new(None),
        });

        let widget = Self { base, inner };

        widget.build_menu();

        // Connect to notification service (using safe Rc pattern)
        widget.bind_service();

        widget
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }

    fn build_menu(&self) {
        let inner = Rc::clone(&self.inner);

        // We need a reference to the menu handle inside the builder, but the handle
        // is created by create_menu. Use a RefCell to store it after creation.
        let menu_handle_cell: Rc<RefCell<Option<Rc<MenuHandle>>>> = Rc::new(RefCell::new(None));
        let menu_handle_for_builder = Rc::clone(&menu_handle_cell);

        let menu_handle = self.base.create_menu("notifications", move || {
            // Mark as seen when popover opens
            inner.mark_as_seen();

            // Create close callback that hides the popover
            let on_close: Option<ClosePopoverCallback> =
                menu_handle_for_builder.borrow().as_ref().map(|handle| {
                    let handle_clone = Rc::clone(handle);
                    Rc::new(move || handle_clone.hide()) as ClosePopoverCallback
                });

            build_popover_content(on_close)
        });

        // Store the menu handle in both places
        *menu_handle_cell.borrow_mut() = Some(Rc::clone(&menu_handle));
        *self.inner.menu_handle.borrow_mut() = Some(menu_handle);
    }

    fn bind_service(&self) {
        let service = NotificationService::global();

        // Initialize known_ids with restored notifications so they don't trigger toasts
        *self.inner.known_ids.borrow_mut() = service.restored_ids();

        // Clone inner Rc for the callback - this is safe because Rc handles
        // the reference counting properly
        let inner = Rc::clone(&self.inner);

        // Initialize toast manager with proper callbacks
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
            let inner_for_callback = Rc::clone(&self.inner);
            let on_toast_removed = move || {
                let inner_clone = Rc::clone(&inner_for_callback);
                glib::idle_add_local_once(move || {
                    let service = NotificationService::global();
                    inner_clone.on_service_update(&service);
                });
            };

            let manager = NotificationToastManager::new(on_action, on_toast_removed);
            *self.inner.toast_manager.borrow_mut() = Some(manager);
        }

        service.connect(move |svc| {
            inner.on_service_update(svc);
        });
    }
}

impl Default for NotificationsWidget {
    fn default() -> Self {
        Self::new(NotificationsConfig::default())
    }
}
