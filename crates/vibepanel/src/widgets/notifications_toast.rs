//! Notification toast windows for displaying new notifications.
//!
//! This module handles floating toast windows that appear when new notifications
//! arrive. Toasts stack vertically from the configured screen edge and
//! auto-dismiss after a timeout (except for critical notifications).

use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{Align, Application, Box as GtkBox, Button, Image, Label, Orientation, Window, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use tracing::debug;

use crate::services::notification::{
    Notification, NotificationService, URGENCY_CRITICAL, URGENCY_LOW,
};

/// Type alias for toast notification callbacks.
type ToastCallback = Rc<dyn Fn(u32)>;
/// Type alias for toast action callbacks.
type ToastActionCallback = Rc<dyn Fn(u32, &str)>;
use crate::services::background_effect::attach_blur_surface_lifecycle;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, notification as notif};

use super::notifications_common::{
    POPOVER_WIDTH, SURFACE_SHADOW_MARGIN, TOAST_EDGE_MARGIN, TOAST_ESTIMATED_HEIGHT, TOAST_GAP,
    TOAST_SIDE_MARGIN, TOAST_TIMEOUT_CRITICAL_MS, TOAST_TIMEOUT_MS,
    create_notification_image_widget, sanitize_body_markup,
};

fn toast_surface_margin() -> i32 {
    SURFACE_SHADOW_MARGIN
}

/// Configurable screen position for notification toasts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ToastPosition {
    #[default]
    TopRight,
    TopCenter,
    TopLeft,
    BottomRight,
    BottomCenter,
    BottomLeft,
}

impl ToastPosition {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "top-right" => Some(Self::TopRight),
            "top-center" => Some(Self::TopCenter),
            "top-left" => Some(Self::TopLeft),
            "bottom-right" => Some(Self::BottomRight),
            "bottom-center" => Some(Self::BottomCenter),
            "bottom-left" => Some(Self::BottomLeft),
            _ => None,
        }
    }

    fn vertical_edge(self) -> Edge {
        match self {
            Self::TopRight | Self::TopCenter | Self::TopLeft => Edge::Top,
            Self::BottomRight | Self::BottomCenter | Self::BottomLeft => Edge::Bottom,
        }
    }

    fn horizontal_edge(self) -> Option<Edge> {
        match self {
            Self::TopRight | Self::BottomRight => Some(Edge::Right),
            Self::TopLeft | Self::BottomLeft => Some(Edge::Left),
            Self::TopCenter | Self::BottomCenter => None,
        }
    }

    fn is_centered(self) -> bool {
        matches!(self, Self::TopCenter | Self::BottomCenter)
    }
}

fn calculate_center_margin(monitor_width: i32, window_width: i32) -> i32 {
    ((monitor_width - window_width) / 2).max(0)
}

fn toast_horizontal_layout(
    position: ToastPosition,
    monitor_width: Option<i32>,
    side_margin: i32,
) -> (Option<Edge>, i32) {
    if position.is_centered() {
        return monitor_width
            .map(|width| {
                (
                    Some(Edge::Left),
                    calculate_center_margin(width, POPOVER_WIDTH),
                )
            })
            .unwrap_or((None, 0));
    }

    (position.horizontal_edge(), side_margin)
}

struct ToastWindowContext<'a> {
    app: &'a Application,
    monitor: Option<&'a gdk::Monitor>,
    layout: ToastLayout,
}

#[derive(Debug, Clone, Copy)]
struct ToastLayout {
    position: ToastPosition,
    initial_margin: i32,
}

/// Floating toast window for displaying a single notification.
pub(super) struct NotificationToast {
    window: Window,
    notification_id: u32,
    timeout_source: RefCell<Option<SourceId>>,
    current_bar_margin: Cell<i32>,
    animation_source: RefCell<Option<SourceId>>,
    bar_edge: Edge,
    /// Actual rendered height, measured after window is mapped
    height: Cell<i32>,
    /// Theme-change callback guard; disconnected automatically on `Drop`.
    theme_callback_guard: RefCell<Option<ThemeCallbackGuard>>,
}

impl NotificationToast {
    const ANIMATION_DURATION_MS: i32 = 150;
    const ANIMATION_STEP_MS: u32 = 16; // ~60fps

    fn new(
        context: ToastWindowContext<'_>,
        notification: &Notification,
        on_dismiss: ToastCallback,
        on_action: ToastActionCallback,
        on_timeout: ToastCallback,
        on_height_measured: ToastCallback,
    ) -> Rc<Self> {
        let window = Window::builder()
            .application(context.app)
            .decorated(false)
            .resizable(false)
            .default_width(POPOVER_WIDTH)
            .build();

        window.add_css_class(notif::TOAST_WRAPPER);

        let layout = context.layout;
        let vertical_edge = layout.position.vertical_edge();

        // Initialize layer shell
        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-toast"));
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::None);

        if let Some(monitor) = context.monitor {
            window.set_monitor(Some(monitor));
        }

        window.set_anchor(Edge::Top, vertical_edge == Edge::Top);
        window.set_anchor(Edge::Bottom, vertical_edge == Edge::Bottom);
        let side_margin = (TOAST_SIDE_MARGIN - toast_surface_margin()).max(0);
        let (horizontal_edge, horizontal_margin) = toast_horizontal_layout(
            layout.position,
            context.monitor.map(|m| m.geometry().width()),
            side_margin,
        );

        window.set_anchor(Edge::Left, horizontal_edge == Some(Edge::Left));
        window.set_anchor(Edge::Right, horizontal_edge == Some(Edge::Right));
        window.set_margin(vertical_edge, layout.initial_margin);
        if let Some(horizontal_edge) = horizontal_edge {
            window.set_margin(horizontal_edge, horizontal_margin);
        }

        let notification_id = notification.id;
        let toast = Rc::new(Self {
            window,
            notification_id,
            timeout_source: RefCell::new(None),
            current_bar_margin: Cell::new(layout.initial_margin),
            animation_source: RefCell::new(None),
            bar_edge: vertical_edge,
            height: Cell::new(TOAST_ESTIMATED_HEIGHT),
            theme_callback_guard: RefCell::new(None),
        });

        toast.build_content(notification, on_dismiss.clone(), on_action);
        toast.schedule_timeout(notification, on_timeout);

        // Measure actual height after window is mapped and laid out.
        // We use idle_add to defer measurement until after GTK has completed layout.
        let toast_weak = Rc::downgrade(&toast);
        let notification_id = notification.id;
        toast.window.connect_map(move |win| {
            let win_clone = win.clone();
            let toast_weak = toast_weak.clone();
            let on_height_measured = on_height_measured.clone();
            glib::idle_add_local_once(move || {
                let height = win_clone.height();
                if let Some(toast) = toast_weak.upgrade()
                    && height > 0
                {
                    toast.height.set(height);
                    on_height_measured(notification_id);
                }
            });
        });

        let theme_callback_guard = attach_blur_surface_lifecycle(
            &toast.window,
            |win: &Window| win.child(),
            || ConfigManager::global().surface_border_radius() as i32,
        );
        toast
            .theme_callback_guard
            .borrow_mut()
            .replace(theme_callback_guard);

        toast
    }

    /// Refresh the toast's content, dismiss/action wiring, and timer to reflect a
    /// notification that was replaced via `replaces_id`. The on-screen position
    /// is preserved; height is re-measured after layout so neighbour toasts can
    /// reposition if the new content is a different size.
    pub fn update(
        self: &Rc<Self>,
        notification: &Notification,
        on_dismiss: ToastCallback,
        on_action: ToastActionCallback,
        on_timeout: ToastCallback,
        on_height_measured: ToastCallback,
    ) {
        // Drop the existing timeout before scheduling a fresh one - replacement
        // resets the auto-dismiss countdown.
        if let Some(source_id) = self.timeout_source.borrow_mut().take() {
            source_id.remove();
        }

        self.build_content(notification, on_dismiss, on_action);
        self.schedule_timeout(notification, on_timeout);

        // The window is already mapped, so connect_map won't fire again. Defer a
        // re-measurement to the next idle so GTK has time to lay out the new
        // child; if the height changed, the manager repositions the stack.
        let toast_weak = Rc::downgrade(self);
        let notification_id = notification.id;
        glib::idle_add_local_once(move || {
            if let Some(toast) = toast_weak.upgrade() {
                let height = toast.window.height();
                if height > 0 && height != toast.height.get() {
                    toast.height.set(height);
                    on_height_measured(notification_id);
                }
            }
        });
    }

    fn schedule_timeout(self: &Rc<Self>, notification: &Notification, on_timeout: ToastCallback) {
        let timeout_ms = if notification.urgency == URGENCY_CRITICAL {
            TOAST_TIMEOUT_CRITICAL_MS
        } else if notification.expire_timeout > 0 {
            notification.expire_timeout as u32
        } else {
            TOAST_TIMEOUT_MS
        };

        debug!(
            "NotificationToast: id={} timeout_ms={} (urgency={}, expire_timeout={})",
            notification.id, timeout_ms, notification.urgency, notification.expire_timeout
        );

        if timeout_ms == 0 {
            return;
        }

        let toast_weak = Rc::downgrade(self);
        let notification_id = notification.id;
        let source_id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(timeout_ms as u64),
            move || {
                debug!(
                    "NotificationToast: timeout fired for id={}",
                    notification_id
                );
                if let Some(toast) = toast_weak.upgrade() {
                    debug!(
                        "NotificationToast: toast still alive, closing window for id={}",
                        notification_id
                    );
                    // Clear the source ID since it's already been removed by glib
                    toast.timeout_source.borrow_mut().take();
                    on_timeout(toast.notification_id);
                    toast.window.close();
                } else {
                    debug!(
                        "NotificationToast: toast was dropped, cannot close for id={}",
                        notification_id
                    );
                }
            },
        );
        *self.timeout_source.borrow_mut() = Some(source_id);
    }

    fn build_content(
        &self,
        notification: &Notification,
        on_dismiss: ToastCallback,
        on_action: ToastActionCallback,
    ) {
        let outer = build_toast_content(notification, on_dismiss, on_action, &self.window);
        self.window.set_child(Some(&outer));
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Get the measured height of this toast (or estimated if not yet measured)
    pub fn height(&self) -> i32 {
        self.height.get()
    }

    fn cancel_animation(&self) {
        if let Some(source_id) = self.animation_source.borrow_mut().take() {
            source_id.remove();
        }
    }

    pub fn update_bar_margin(self: &Rc<Self>, target_margin: i32, animate: bool) {
        let current = self.current_bar_margin.get();

        if !animate || current == target_margin {
            self.current_bar_margin.set(target_margin);
            self.window.set_margin(self.bar_edge, target_margin);
            return;
        }

        // Cancel existing animation
        self.cancel_animation();

        // Animate position change
        let start_margin = current;
        let total_steps = (Self::ANIMATION_DURATION_MS / Self::ANIMATION_STEP_MS as i32).max(1);
        let current_step = Rc::new(Cell::new(0));
        let toast_weak = Rc::downgrade(self);

        let source_id = glib::timeout_add_local(
            std::time::Duration::from_millis(Self::ANIMATION_STEP_MS as u64),
            move || {
                let Some(toast) = toast_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };

                let step = current_step.get() + 1;
                current_step.set(step);

                let progress = (step as f32 / total_steps as f32).min(1.0);
                // Ease-out cubic
                let eased = 1.0 - (1.0 - progress).powi(3);

                let new_margin =
                    start_margin + ((target_margin - start_margin) as f32 * eased) as i32;
                toast.current_bar_margin.set(new_margin);
                toast.window.set_margin(toast.bar_edge, new_margin);

                if progress >= 1.0 {
                    *toast.animation_source.borrow_mut() = None;
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            },
        );
        *self.animation_source.borrow_mut() = Some(source_id);
    }
}

fn build_toast_content(
    notification: &Notification,
    on_dismiss: ToastCallback,
    on_action: ToastActionCallback,
    close_window: &Window,
) -> GtkBox {
    let outer = GtkBox::new(Orientation::Vertical, 0);
    outer.add_css_class(notif::TOAST_CONTAINER);
    outer.add_css_class(notif::TOAST);

    // Add urgency styling before the widget is attached so static CSS can match
    // the final toast class set immediately.
    if notification.urgency == URGENCY_CRITICAL {
        outer.add_css_class(notif::TOAST_CRITICAL);
    } else if notification.urgency == URGENCY_LOW {
        outer.add_css_class(notif::TOAST_LOW);
    }

    // Keep fixed layout slack around the toast surface. This prevents CSS box
    // shadows from clipping when enabled, and keeps small user CSS offsets from
    // clipping when shadows are disabled.
    let sm = toast_surface_margin();
    outer.set_margin_top(sm);
    outer.set_margin_bottom(sm);
    outer.set_margin_start(sm);
    outer.set_margin_end(sm);

    let has_default_action = notification.actions.iter().any(|(id, _)| id == "default");

    let main_row = GtkBox::new(Orientation::Horizontal, 10);

    // App icon / avatar in a centered column
    let icon_container = GtkBox::new(Orientation::Vertical, 0);
    icon_container.set_halign(Align::Center);
    icon_container.set_valign(Align::Start);
    icon_container.set_width_request(56);

    let icon = create_notification_image_widget(notification);
    icon.add_css_class(notif::TOAST_ICON);
    icon.set_halign(Align::Center);
    icon_container.append(&icon);

    main_row.append(&icon_container);

    let content = GtkBox::new(Orientation::Vertical, 2);
    content.set_hexpand(true);
    content.add_css_class(notif::TOAST_CONTENT);

    let app_label = Label::new(Some(&notification.app_name));
    app_label.add_css_class(notif::TOAST_APP);
    app_label.add_css_class(color::MUTED);
    app_label.set_xalign(0.0);
    app_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    app_label.set_margin_bottom(4);
    content.append(&app_label);

    if !notification.summary.is_empty() {
        let summary_label = Label::new(Some(&notification.summary));
        summary_label.add_css_class(notif::TOAST_SUMMARY);
        summary_label.set_xalign(0.0);
        summary_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        summary_label.set_single_line_mode(true);
        content.append(&summary_label);
    }

    if !notification.body.is_empty() {
        let body_markup = sanitize_body_markup(&notification.body);
        let body_label = Label::new(None);
        body_label.set_markup(&body_markup);
        body_label.add_css_class(notif::TOAST_BODY);
        body_label.add_css_class(color::MUTED);
        body_label.set_xalign(0.0);
        body_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        body_label.set_lines(2);
        body_label.set_wrap(true);
        body_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        content.append(&body_label);
    }

    main_row.append(&content);

    let dismiss_btn = Button::new();
    dismiss_btn.set_has_frame(false);
    dismiss_btn.add_css_class(notif::TOAST_DISMISS);
    dismiss_btn.add_css_class(button::RESET);
    dismiss_btn.set_valign(Align::Start);
    dismiss_btn.set_focusable(false);

    let dismiss_icon = Image::from_icon_name("window-close-symbolic");
    dismiss_icon.set_halign(Align::Center);
    dismiss_icon.set_valign(Align::Center);
    dismiss_btn.set_child(Some(&dismiss_icon));

    let notification_id = notification.id;
    let window = close_window.clone();
    let on_dismiss_for_btn = on_dismiss.clone();
    dismiss_btn.connect_clicked(move |_| {
        on_dismiss_for_btn(notification_id);
        window.close();
    });

    main_row.append(&dismiss_btn);

    // Handle default action click
    if has_default_action {
        // Make the content area clickable
        let click_gesture = gtk4::GestureClick::new();
        click_gesture.set_button(1); // Only respond to left mouse button
        let on_action_clone = on_action.clone();
        let on_dismiss_clone = on_dismiss.clone();
        let notification_id = notification.id;
        let window_for_action = close_window.clone();
        // Use connect_pressed instead of connect_released to ensure it's a real click
        // that started within the widget (released can fire from drags ending on widget)
        click_gesture.connect_pressed(move |gesture, n_press, _, _| {
            // Only respond to single clicks (not double-clicks, etc.)
            if n_press == 1 {
                // Stop propagation to prevent accidental triggers
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                on_action_clone(notification_id, "default");
                on_dismiss_clone(notification_id);
                window_for_action.close();
            }
        });
        content.add_controller(click_gesture);
        content.add_css_class(notif::TOAST_CLICKABLE);
    }

    outer.append(&main_row);

    // Action buttons at the bottom
    let non_default_actions: Vec<_> = notification
        .actions
        .iter()
        .filter(|(id, _)| id != "default")
        .collect();

    if !non_default_actions.is_empty() {
        let actions_box = GtkBox::new(Orientation::Horizontal, 8);
        actions_box.add_css_class(notif::TOAST_ACTIONS);
        actions_box.set_halign(Align::End);

        for (action_id, action_label) in non_default_actions {
            let action_btn = crate::widgets::base::vp_button_with_label(action_label);
            action_btn.add_css_class(notif::TOAST_ACTION);
            action_btn.add_css_class(button::GHOST);
            action_btn.set_focusable(false);

            let on_action_clone = on_action.clone();
            let on_dismiss_clone = on_dismiss.clone();
            let notification_id = notification.id;
            let action_id = action_id.clone();
            let window_for_action = close_window.clone();
            action_btn.connect_clicked(move |_| {
                on_action_clone(notification_id, &action_id);
                on_dismiss_clone(notification_id);
                window_for_action.close();
            });

            actions_box.append(&action_btn);
        }

        outer.append(&actions_box);
    }

    // Apply Pango font attributes to all labels if enabled in config.
    // This is the central hook for notification toasts - widgets create standard
    // GTK labels, and we apply Pango attributes here after the tree is built.
    SurfaceStyleManager::global().apply_pango_attrs_all(&outer);

    outer
}

impl Drop for NotificationToast {
    fn drop(&mut self) {
        // Cancel any pending animation to free resources promptly
        if let Some(source_id) = self.animation_source.borrow_mut().take() {
            source_id.remove();
        }
        // Cancel any pending timeout (may already be cleared by glib)
        if let Some(source_id) = self.timeout_source.borrow_mut().take() {
            source_id.remove();
        }
        // ThemeCallbackGuard handles disconnect_theme_callback on drop.
        drop(self.theme_callback_guard.borrow_mut().take());
    }
}

/// Manages notification toast windows with vertical stacking.
pub(super) struct NotificationToastManager {
    toasts: RefCell<HashMap<u32, Rc<NotificationToast>>>,
    toast_order: RefCell<Vec<u32>>,
    on_action: ToastActionCallback,
    on_toast_removed: Rc<dyn Fn()>,
    position: ToastPosition,
}

impl NotificationToastManager {
    pub fn new(
        on_action: impl Fn(u32, &str) + 'static,
        on_toast_removed: impl Fn() + 'static,
        position: ToastPosition,
    ) -> Rc<Self> {
        Rc::new(Self {
            toasts: RefCell::new(HashMap::new()),
            toast_order: RefCell::new(Vec::new()),
            on_action: Rc::new(on_action),
            on_toast_removed: Rc::new(on_toast_removed),
            position,
        })
    }

    pub fn show(
        self: &Rc<Self>,
        app: &Application,
        monitor: Option<&gdk::Monitor>,
        notification: &Notification,
    ) {
        // Transient notifications must be removed from the service when their toast
        // disappears (dismiss or timeout) so they never leak into the popover
        // history. Close the service entry *before* tearing down the toast so the
        // synchronous on_service_update fired by `service.close` does the work;
        // the deferred update from `remove_toast` then sees no change and no-ops.
        let is_transient = notification.transient;

        let manager = Rc::clone(self);
        let on_dismiss: Rc<dyn Fn(u32)> = Rc::new(move |id| {
            if is_transient {
                NotificationService::global().close(id);
            }
            manager.remove_toast(id);
        });

        // When toast times out, we need to remove it and notify the widget to update badge
        let manager_for_timeout = Rc::clone(self);
        let on_timeout: Rc<dyn Fn(u32)> = Rc::new(move |id| {
            if is_transient {
                NotificationService::global().close(id);
            }
            manager_for_timeout.remove_toast(id);
        });

        // When toast height is measured, reposition all toasts. Constructed up
        // front because both the in-place update path and the new-toast path
        // need it.
        let manager_for_height = Rc::clone(self);
        let on_height_measured: Rc<dyn Fn(u32)> = Rc::new(move |_id| {
            manager_for_height.reposition_toasts();
        });

        // If a toast for this id is already on screen, mutate it in place so a
        // notification replaced via `replaces_id` updates the existing toast
        // (text, actions, timer) rather than stacking a new one on top.
        let existing = self.toasts.borrow().get(&notification.id).cloned();
        if let Some(toast) = existing {
            toast.update(
                notification,
                on_dismiss,
                Rc::clone(&self.on_action),
                on_timeout,
                on_height_measured,
            );
            return;
        }

        // Calculate initial margin from existing toasts.
        // Each toast window includes shadow margins (sm on each side), making
        // the window taller than the visible content.  Subtract the shadow
        // margins from the height contribution so the visual gap between
        // content boxes matches TOAST_GAP.
        let sm = toast_surface_margin();
        let initial_margin = {
            let order = self.toast_order.borrow();
            let toasts = self.toasts.borrow();
            let mut y_offset = (TOAST_EDGE_MARGIN - sm).max(0);
            for &id in order.iter() {
                if let Some(toast) = toasts.get(&id) {
                    y_offset += (toast.height() - 2 * sm).max(0) + TOAST_GAP;
                }
            }
            y_offset
        };

        let toast = NotificationToast::new(
            ToastWindowContext {
                app,
                monitor,
                layout: ToastLayout {
                    position: self.position,
                    initial_margin,
                },
            },
            notification,
            on_dismiss,
            Rc::clone(&self.on_action),
            on_timeout,
            on_height_measured,
        );

        self.toasts
            .borrow_mut()
            .insert(notification.id, Rc::clone(&toast));
        self.toast_order.borrow_mut().push(notification.id);
        toast.present();
    }

    pub fn remove_toast(&self, notification_id: u32) {
        let had_toast = self.toasts.borrow_mut().remove(&notification_id).is_some();

        if had_toast {
            // Note: toast.close() is not called here because the toast may have
            // already been closed (e.g., window.close() was called directly).
            // The timeout source is already cleared by the toast itself.
        }

        self.toast_order
            .borrow_mut()
            .retain(|&id| id != notification_id);
        self.reposition_toasts();

        // Notify widget to recalculate badge
        (self.on_toast_removed)();
    }

    pub fn close_toast(&self, notification_id: u32) {
        if let Some(toast) = self.toasts.borrow_mut().remove(&notification_id) {
            toast.window.close();
        }

        self.toast_order
            .borrow_mut()
            .retain(|&id| id != notification_id);
        self.reposition_toasts();
        (self.on_toast_removed)();
    }

    fn reposition_toasts(&self) {
        let order = self.toast_order.borrow();
        let toasts = self.toasts.borrow();
        let sm = toast_surface_margin();
        let mut y_offset = (TOAST_EDGE_MARGIN - sm).max(0);
        for &id in order.iter() {
            if let Some(toast) = toasts.get(&id) {
                toast.update_bar_margin(y_offset, ConfigManager::global().animations_enabled());
                y_offset += (toast.height() - 2 * sm).max(0) + TOAST_GAP;
            }
        }
    }

    pub fn active_ids(&self) -> HashSet<u32> {
        self.toasts.borrow().keys().cloned().collect()
    }
}

#[cfg(test)]
#[path = "notifications_toast_tests.rs"]
mod tests;
