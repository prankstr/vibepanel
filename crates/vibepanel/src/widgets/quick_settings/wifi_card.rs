//! Wi-Fi card for Quick Settings panel.
//!
//! This module contains:
//! - Wi-Fi icon helpers (merged from qs_wifi_helpers.rs)
//! - Wi-Fi details panel building
//! - Network list population
//! - Password dialog handling
//! - Scan animation

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk4::glib::{self, WeakRef};
use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box as GtkBox, Button, Entry, GestureClick, Label, ListBox, ListBoxRow,
    Orientation, Overlay, Popover, ScrolledWindow,
};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, add_placeholder_row, build_scan_button, clear_list_box,
    create_qs_list_box, create_row_action_label, create_row_menu_action, create_row_menu_button,
    set_icon_active, set_subtitle_active,
};
use crate::services::icons::IconsService;
use crate::services::network::{NetworkService, NetworkSnapshot, WifiNetwork};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{color, icon, qs, row, state, surface};
use crate::widgets::base::configure_popover;

/// Return a simple connected/disconnected Wi-Fi icon.
///
/// The main card widget uses this for a stable "connected" icon,
/// while the per-network list rows use `wifi_strength_icon` for
/// detailed signal levels.
pub fn wifi_icon_name(connected: bool, wifi_enabled: bool) -> &'static str {
    if !wifi_enabled {
        "network-wireless-offline-symbolic"
    } else if connected {
        "network-wireless-signal-excellent-symbolic"
    } else {
        "network-wireless-offline-symbolic"
    }
}

/// Return a Wi-Fi icon name based on a raw signal strength percentage.
///
/// The list rows use this to express 1/2/3/4-bar states. The Material
/// icon mapping compresses these into the available glyph set.
pub fn wifi_strength_icon(level: i32) -> &'static str {
    if level >= 70 {
        "network-wireless-signal-excellent-symbolic"
    } else if level >= 60 {
        "network-wireless-signal-good-symbolic"
    } else if level >= 40 {
        "network-wireless-signal-ok-symbolic"
    } else if level >= 20 {
        "network-wireless-signal-weak-symbolic"
    } else {
        "network-wireless-signal-none-symbolic"
    }
}

/// Find the QuickSettingsWindow by searching all toplevels.
///
/// Returns the QuickSettingsWindow if found and still alive, None otherwise.
/// This searches through all application windows to find the one with
/// the "vibepanel-qs-window" data attached.
fn find_quick_settings_window() -> Option<Rc<super::window::QuickSettingsWindow>> {
    for toplevel in gtk4::Window::list_toplevels() {
        if let Ok(window) = toplevel.downcast::<ApplicationWindow>() {
            // SAFETY: We store a Weak<QuickSettingsWindow> on the window at creation
            // time with key "vibepanel-qs-window". upgrade() returns None if dropped.
            unsafe {
                if let Some(weak_ptr) =
                    window.data::<Weak<super::window::QuickSettingsWindow>>("vibepanel-qs-window")
                    && let Some(qs) = weak_ptr.as_ref().upgrade()
                {
                    return Some(qs);
                }
            }
        }
    }
    None
}

/// State for the Wi-Fi card in the Quick Settings panel.
///
/// Uses `ExpandableCardBase` for common expandable card fields and adds
/// Wi-Fi specific state (scan button, password dialog, animation).
pub struct WifiCardState {
    /// Common expandable card state (toggle, icon, subtitle, list_box, revealer, arrow).
    pub base: ExpandableCardBase,
    /// The Wi-Fi scan button.
    pub scan_button: RefCell<Option<Button>>,
    /// The Wi-Fi scan button label.
    pub scan_label: RefCell<Option<Label>>,
    /// Inline password box.
    pub password_box: RefCell<Option<GtkBox>>,
    /// Label in the password box.
    pub password_label: RefCell<Option<Label>>,
    /// Error/status label in the password box (shows errors or "Connecting...").
    pub password_error_label: RefCell<Option<Label>>,
    /// Password entry field.
    pub password_entry: RefCell<Option<Entry>>,
    /// Cancel button in password box.
    pub password_cancel_button: RefCell<Option<Button>>,
    /// Connect button in password box.
    pub password_connect_button: RefCell<Option<Button>>,
    /// Target SSID for the inline password prompt.
    pub password_target_ssid: RefCell<Option<String>>,
    /// Scan animation GLib source ID.
    pub scan_anim_source: RefCell<Option<glib::SourceId>>,
    /// Scan animation step counter.
    pub scan_anim_step: Cell<u8>,
    /// Connect animation GLib source ID.
    pub connect_anim_source: RefCell<Option<glib::SourceId>>,
    /// Connect animation step counter.
    pub connect_anim_step: Cell<u8>,
    /// Flag to prevent toggle signal handler from firing during programmatic updates.
    /// This prevents feedback loops when the service notifies us of state changes.
    pub updating_toggle: Cell<bool>,
}

impl WifiCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            scan_button: RefCell::new(None),
            scan_label: RefCell::new(None),
            password_box: RefCell::new(None),
            password_label: RefCell::new(None),
            password_error_label: RefCell::new(None),
            password_entry: RefCell::new(None),
            password_cancel_button: RefCell::new(None),
            password_connect_button: RefCell::new(None),
            password_target_ssid: RefCell::new(None),
            scan_anim_source: RefCell::new(None),
            scan_anim_step: Cell::new(0),
            connect_anim_source: RefCell::new(None),
            connect_anim_step: Cell::new(0),
            updating_toggle: Cell::new(false),
        }
    }
}

impl Default for WifiCardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for WifiCardState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

impl Drop for WifiCardState {
    fn drop(&mut self) {
        // Cancel any active scan animation timer
        if let Some(source_id) = self.scan_anim_source.borrow_mut().take() {
            source_id.remove();
            debug!("WifiCardState: scan animation timer cancelled on drop");
        }
        // Cancel any active connect animation timer
        if let Some(source_id) = self.connect_anim_source.borrow_mut().take() {
            source_id.remove();
            debug!("WifiCardState: connect animation timer cancelled on drop");
        }
    }
}

/// Result of building Wi-Fi details section.
pub struct WifiDetailsResult {
    pub container: GtkBox,
    pub list_box: ListBox,
    pub scan_button: Button,
    pub scan_label: Label,
}

/// Build the Wi-Fi details section with scan button, network list, and
/// inline password prompt.
pub fn build_wifi_details(
    state: &Rc<WifiCardState>,
    window: WeakRef<ApplicationWindow>,
) -> WifiDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    // Scan button
    let scan_result = build_scan_button("Scan");
    let scan_button = scan_result.button;
    let scan_label = scan_result.label;

    {
        scan_button.connect_clicked(move |_| {
            NetworkService::global().scan_networks();
        });
    }

    container.append(&scan_button);

    // Network list
    let list_box = create_qs_list_box();

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_child(Some(&list_box));
    scroller.set_max_content_height(360);
    scroller.set_propagate_natural_height(true);

    container.append(&scroller);

    // Inline password prompt box (initially hidden, reused as a row child)
    let pwd_box = GtkBox::new(Orientation::Vertical, 6);
    pwd_box.set_visible(false);

    let pwd_label = Label::new(Some(""));
    pwd_label.set_xalign(0.0);
    pwd_box.append(&pwd_label);

    let pwd_entry = Entry::new();
    pwd_entry.set_visibility(false);
    pwd_entry.set_input_purpose(gtk4::InputPurpose::Password);
    pwd_entry.set_can_focus(true);
    pwd_entry.set_focus_on_click(true);

    {
        let state_weak = Rc::downgrade(state);
        pwd_entry.connect_map(move |entry| {
            if let Some(state) = state_weak.upgrade() {
                on_password_entry_mapped(&state, entry);
            }
        });
    }
    {
        let state_weak = Rc::downgrade(state);
        let window_weak = window.clone();
        pwd_entry.connect_activate(move |_| {
            if let Some(state) = state_weak.upgrade() {
                on_password_connect_clicked(&state, window_weak.clone());
            }
        });
    }

    pwd_box.append(&pwd_entry);

    // Button row: [status label (expands)] [cancel] [connect]
    let btn_row = GtkBox::new(Orientation::Horizontal, 8);

    // Status label (shows "Connecting..." or "Wrong password")
    // Always visible but with empty text when idle - keeps buttons right-aligned
    let pwd_status_label = Label::new(Some(""));
    pwd_status_label.set_xalign(0.0);
    pwd_status_label.set_hexpand(true);
    btn_row.append(&pwd_status_label);

    let btn_cancel = Button::with_label("Cancel");
    let btn_ok = Button::with_label("Connect");

    // Apply Pango font attrs to fix text clipping on layer-shell surfaces
    let style_mgr = SurfaceStyleManager::global();
    style_mgr.apply_pango_attrs(&pwd_label);
    style_mgr.apply_pango_attrs(&pwd_status_label);

    {
        let state_weak = Rc::downgrade(state);
        btn_cancel.connect_clicked(move |_| {
            if let Some(state) = state_weak.upgrade() {
                on_password_cancel_clicked(&state);
            }
        });
    }

    {
        let state_weak = Rc::downgrade(state);
        let window_weak = window.clone();
        btn_ok.connect_clicked(move |_| {
            if let Some(state) = state_weak.upgrade() {
                on_password_connect_clicked(&state, window_weak.clone());
            }
        });
    }

    btn_row.append(&btn_cancel);
    btn_row.append(&btn_ok);
    pwd_box.append(&btn_row);

    // Store password widgets for later use
    *state.password_box.borrow_mut() = Some(pwd_box.clone());
    *state.password_label.borrow_mut() = Some(pwd_label.clone());
    *state.password_error_label.borrow_mut() = Some(pwd_status_label.clone());
    *state.password_entry.borrow_mut() = Some(pwd_entry.clone());
    *state.password_cancel_button.borrow_mut() = Some(btn_cancel.clone());
    *state.password_connect_button.borrow_mut() = Some(btn_ok.clone());

    // Populate with current network state
    let snapshot = NetworkService::global().snapshot();
    populate_wifi_list(state, &list_box, &snapshot);

    WifiDetailsResult {
        container,
        list_box,
        scan_button,
        scan_label,
    }
}

/// Populate the Wi-Fi list with network data from snapshot.
pub fn populate_wifi_list(state: &WifiCardState, list_box: &ListBox, snapshot: &NetworkSnapshot) {
    // Unparent and unrealize the password box BEFORE clearing the list.
    // This is critical: when clear_list_box removes rows, the password box would become
    // orphaned but still realized. Then when we try to add it to a new row, GTK fails
    // with "assertion failed: (!priv->realized)".
    if let Some(pwd_box) = state.password_box.borrow().as_ref()
        && pwd_box.parent().is_some()
    {
        pwd_box.unrealize();
        pwd_box.unparent();
    }

    clear_list_box(list_box);

    if !snapshot.is_ready {
        add_placeholder_row(list_box, "Scanning for networks...");
        return;
    }

    if snapshot.networks.is_empty() {
        add_placeholder_row(list_box, "No networks found");
        return;
    }

    let icons = IconsService::global();
    let target_ssid = state.password_target_ssid.borrow().clone();
    let connecting_ssid = snapshot.connecting_ssid.clone();
    let mut inserted_password_row = false;

    for net in &snapshot.networks {
        // Check if this network is currently being connected to
        let is_connecting = connecting_ssid.as_ref() == Some(&net.ssid);

        // Build subtitle
        let mut subtitle_parts = Vec::new();
        if is_connecting {
            subtitle_parts.push("Connecting...".to_string());
        } else if net.active {
            subtitle_parts.push("Connected".to_string());
        }
        if net.security != "open" {
            subtitle_parts.push("Secured".to_string());
        }
        // Don't show "Saved" while connecting (nmcli creates profile before auth completes)
        if net.known && !is_connecting {
            subtitle_parts.push("Saved".to_string());
        }
        subtitle_parts.push(format!("{}%", net.strength));
        let subtitle = subtitle_parts.join(" \u{2022} ");

        // Create signal strength icon
        let strength_icon_name = wifi_strength_icon(net.strength);

        // Check if this is a partial signal that needs the overlay treatment
        let needs_overlay = matches!(
            strength_icon_name,
            "network-wireless-signal-none-symbolic"
                | "network-wireless-signal-weak-symbolic"
                | "network-wireless-signal-ok-symbolic"
                | "network-wireless-signal-good-symbolic"
        );

        let leading_icon: gtk4::Widget = if icons.uses_material() && needs_overlay {
            // Create base icon (full signal, dimmed)
            let base_handle = icons.create_icon(
                "network-wireless-signal-excellent-symbolic",
                &[icon::TEXT, row::QS_ICON, qs::WIFI_BASE, color::MUTED],
            );

            // Create overlay icon (actual signal level, highlighted)
            let overlay_handle = icons.create_icon(
                strength_icon_name,
                &[icon::TEXT, row::QS_ICON, qs::WIFI_OVERLAY, color::PRIMARY],
            );

            // Stack them using Overlay
            let overlay = Overlay::new();
            overlay.set_child(Some(&base_handle.widget()));
            overlay.add_overlay(&overlay_handle.widget());
            overlay.upcast()
        } else {
            // Simple single icon for full signal or non-Material themes
            let icon_handle = icons.create_icon(
                strength_icon_name,
                &[icon::TEXT, row::QS_ICON, color::PRIMARY],
            );
            icon_handle.widget()
        };

        // Create action widget with click handler (or placeholder if connecting)
        let right_widget = if is_connecting {
            // Show a muted "Connecting..." label instead of action button
            let connecting_label = Label::new(Some("..."));
            connecting_label.add_css_class(color::MUTED);
            connecting_label.upcast::<gtk4::Widget>()
        } else {
            create_network_action_widget(net)
        };

        let row_result = ListRow::builder()
            .title(&net.ssid)
            .subtitle(&subtitle)
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::WIFI_ROW)
            .build();

        // Disable row activation if this network is currently connecting
        if is_connecting {
            row_result.row.set_activatable(false);
            row_result.row.set_sensitive(false);
        }

        // Connect row activation to the primary network action
        if !is_connecting {
            let ssid = net.ssid.clone();
            let security = net.security.clone();
            let known = net.known;
            let active = net.active;
            row_result.row.connect_activate(move |_| {
                let service = NetworkService::global();
                if active {
                    service.disconnect();
                } else if security == "open" || known {
                    service.connect_to_ssid(&ssid, None);
                } else {
                    // Secured, unknown network: show password prompt
                    let snapshot = service.snapshot();
                    if snapshot
                        .networks
                        .iter()
                        .any(|n| n.ssid == ssid && !n.known && n.security != "open")
                        && let Some(qs) = find_quick_settings_window()
                    {
                        qs.show_wifi_password_dialog(&ssid);
                    }
                }
            });
        }

        list_box.append(&row_result.row);

        // Insert password row directly under the matching network row
        if let Some(ref target) = target_ssid
            && !target.is_empty()
            && *target == net.ssid
            && let Some(pwd_box) = state.password_box.borrow().as_ref()
        {
            let pwd_row = ListBoxRow::new();
            pwd_row.set_activatable(false);
            pwd_row.set_focusable(true);
            pwd_box.set_visible(true);
            pwd_row.set_child(Some(pwd_box));
            list_box.append(&pwd_row);
            inserted_password_row = true;
        }
    }

    // Fallback: append password row at end if target SSID not found
    if let Some(target) = target_ssid
        && !target.is_empty()
        && !inserted_password_row
        && let Some(pwd_box) = state.password_box.borrow().as_ref()
    {
        let pwd_row = ListBoxRow::new();
        pwd_row.set_activatable(false);
        pwd_row.set_focusable(true);
        pwd_box.set_visible(true);
        pwd_row.set_child(Some(pwd_box));
        list_box.append(&pwd_row);
    }
}

/// Create the action widget for a network row.
fn create_network_action_widget(net: &WifiNetwork) -> gtk4::Widget {
    let ssid = net.ssid.clone();
    let is_active = net.active;
    let is_known = net.known;

    // Determine if we need a menu (multiple actions) or single action label
    let has_multiple_actions = is_active || is_known;

    if !has_multiple_actions {
        // Single action: just "Connect" as accent-colored text
        let action_label = create_row_action_label("Connect");
        let ssid_clone = ssid.clone();
        let is_secured = net.security != "open";
        let gesture = GestureClick::new();
        gesture.set_button(1);
        gesture.connect_pressed(move |_, _, _, _| {
            if is_secured {
                // Secured, unknown network: show password prompt
                if let Some(qs) = find_quick_settings_window() {
                    qs.show_wifi_password_dialog(&ssid_clone);
                }
            } else {
                // Open network: connect directly without password
                let network = NetworkService::global();
                network.connect_to_ssid(&ssid_clone, None);
            }
        });
        action_label.add_controller(gesture);
        return action_label.upcast();
    }

    // Known or active networks: hamburger menu with multiple actions.
    let menu_btn = create_row_menu_button();

    let is_active_clone = is_active;
    let is_known_clone = is_known;
    let ssid_for_actions = ssid.clone();

    menu_btn.connect_clicked(move |btn| {
        let popover = Popover::new();
        configure_popover(&popover);

        let panel = GtkBox::new(Orientation::Vertical, 0);
        panel.add_css_class(surface::WIDGET_MENU_CONTENT);

        let content_box = GtkBox::new(Orientation::Vertical, 2);
        content_box.add_css_class(qs::ROW_MENU_CONTENT);

        // Connect / Disconnect actions
        if is_active_clone {
            let ssid_clone = ssid_for_actions.clone();
            let popover_weak = popover.downgrade();
            let action = create_row_menu_action("Disconnect", move || {
                // Close popover first to avoid "still has children" warning
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
                let network = NetworkService::global();
                debug!("wifi_disconnect_from_menu ssid={}", ssid_clone);
                network.disconnect();
            });
            content_box.append(&action);
        } else {
            let ssid_clone = ssid_for_actions.clone();
            let popover_weak = popover.downgrade();
            let action = create_row_menu_action("Connect", move || {
                // Close popover first to avoid "still has children" warning
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
                let network = NetworkService::global();
                debug!("wifi_connect_from_menu ssid={}", ssid_clone);
                // Known networks connect without password prompt
                network.connect_to_ssid(&ssid_clone, None);
            });
            content_box.append(&action);
        }

        // Forget action for known networks
        if is_known_clone {
            let ssid_clone = ssid_for_actions.clone();
            let popover_weak = popover.downgrade();
            let action = create_row_menu_action("Forget", move || {
                // Close popover first to avoid "still has children" warning
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
                let network = NetworkService::global();
                debug!("wifi_forget_from_menu ssid={}", ssid_clone);
                network.forget_network(&ssid_clone);
            });
            content_box.append(&action);
        }

        panel.append(&content_box);
        let style_mgr = SurfaceStyleManager::global();
        style_mgr.apply_surface_styles(&panel, true, None);
        style_mgr.apply_pango_attrs_all(&content_box);

        popover.set_child(Some(&panel));
        popover.set_parent(btn);
        popover.popup();

        // Unparent popover when closed to avoid "still has children" warning
        // when the button is destroyed during list refresh
        popover.connect_closed(|p| {
            p.unparent();
        });
    });

    menu_btn.upcast()
}

/// Show inline Wi-Fi password dialog for the given SSID.
/// If `show_error` is true, displays "Wrong password" message.
pub fn show_password_dialog_with_error(state: &WifiCardState, ssid: &str, show_error: bool) {
    let ssid = ssid.trim();
    if ssid.is_empty() {
        return;
    }

    *state.password_target_ssid.borrow_mut() = Some(ssid.to_string());

    if let Some(label) = state.password_label.borrow().as_ref() {
        label.set_label(&format!("Enter password for {}", ssid));
    }

    // Show or clear the error label (always visible for layout, text controls display)
    if let Some(error_label) = state.password_error_label.borrow().as_ref() {
        if show_error {
            error_label.add_css_class(color::ERROR);
            error_label.set_label("Wrong password");
        } else {
            error_label.remove_css_class(color::ERROR);
            error_label.set_label("");
        }
    }

    if let Some(entry) = state.password_entry.borrow().as_ref() {
        entry.set_text("");
    }

    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        let snapshot = NetworkService::global().snapshot();
        populate_wifi_list(state, list_box, &snapshot);
    }
}

/// Show inline Wi-Fi password dialog for the given SSID.
pub fn show_password_dialog(state: &WifiCardState, ssid: &str) {
    show_password_dialog_with_error(state, ssid, false);
}

/// Called when the password entry is mapped; grabs focus if we have a target.
fn on_password_entry_mapped(state: &WifiCardState, entry: &Entry) {
    if state.password_target_ssid.borrow().is_some() {
        entry.grab_focus();
    }
}

/// Cancel the inline password prompt.
fn on_password_cancel_clicked(state: &WifiCardState) {
    hide_password_dialog(state);

    // Clear any failed connection state so we don't re-show the dialog
    NetworkService::global().clear_failed_state();
}

/// Hide the password dialog and reset its state.
fn hide_password_dialog(state: &WifiCardState) {
    if let Some(entry) = state.password_entry.borrow().as_ref() {
        entry.set_text("");
    }
    if let Some(box_) = state.password_box.borrow().as_ref() {
        box_.set_visible(false);
    }
    // Reset connecting state (re-enable inputs, stop animation)
    set_password_connecting_state(state, false, None);
    // Clear status label
    if let Some(error_label) = state.password_error_label.borrow().as_ref() {
        error_label.remove_css_class(color::ERROR);
        error_label.set_label("");
    }
    *state.password_target_ssid.borrow_mut() = None;

    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        let snapshot = NetworkService::global().snapshot();
        populate_wifi_list(state, list_box, &snapshot);
    }
}

/// Attempt to connect using the inline password prompt.
fn on_password_connect_clicked(state: &WifiCardState, window: WeakRef<ApplicationWindow>) {
    let ssid_opt = state.password_target_ssid.borrow().clone();
    let Some(ssid) = ssid_opt else {
        return;
    };

    let password = if let Some(entry) = state.password_entry.borrow().as_ref() {
        entry.text().to_string()
    } else {
        String::new()
    };

    if ssid.is_empty() {
        return;
    }

    // Show connecting state: disable inputs, start animation
    set_password_connecting_state(state, true, Some(window));

    let service = NetworkService::global();
    service.connect_to_ssid(&ssid, Some(&password));
}

/// Set the password dialog to connecting/idle state.
/// When `connecting` is true, `window` must be provided to start the animation.
/// When `connecting` is false, `window` can be None as we're just stopping.
fn set_password_connecting_state(
    state: &WifiCardState,
    connecting: bool,
    window: Option<WeakRef<ApplicationWindow>>,
) {
    if let Some(entry) = state.password_entry.borrow().as_ref() {
        entry.set_sensitive(!connecting);
    }
    if let Some(btn) = state.password_cancel_button.borrow().as_ref() {
        btn.set_sensitive(!connecting);
    }
    if let Some(btn) = state.password_connect_button.borrow().as_ref() {
        btn.set_sensitive(!connecting);
    }

    // Show "Connecting..." animation in the status label (same location as error)
    let mut source_opt = state.connect_anim_source.borrow_mut();
    if connecting {
        // Show status label with initial text (remove error styling)
        if let Some(label) = state.password_error_label.borrow().as_ref() {
            label.remove_css_class(color::ERROR);
            label.set_label("Connecting");
        }

        if source_opt.is_none()
            && let Some(window) = window
        {
            // Start a simple dot animation: "Connecting", "Connecting.", ...
            let step_cell = state.connect_anim_step.clone();
            let source_id = glib::timeout_add_local(std::time::Duration::from_millis(450), {
                move || {
                    if let Some(window) = window.upgrade() {
                        // SAFETY: We store a Weak<QuickSettingsWindow> on the window at creation
                        // time with key "vibepanel-qs-window". upgrade() returns None if dropped.
                        unsafe {
                            if let Some(weak_ptr) = window
                                .data::<Weak<super::window::QuickSettingsWindow>>(
                                    "vibepanel-qs-window",
                                )
                                && let Some(qs) = weak_ptr.as_ref().upgrade()
                                && let Some(label) = qs.wifi.password_error_label.borrow().as_ref()
                            {
                                let step = step_cell.get().wrapping_add(1) % 4;
                                step_cell.set(step);
                                let dots = match step {
                                    1 => ".",
                                    2 => "..",
                                    3 => "...",
                                    _ => "",
                                };
                                label.set_label(&format!("Connecting{}", dots));
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                }
            });
            *source_opt = Some(source_id);
        }
    } else {
        // Stop animation if running
        if let Some(id) = source_opt.take() {
            id.remove();
            state.connect_anim_step.set(0);
        }
        // Clear status label (will be set to error text by caller if needed)
        if let Some(label) = state.password_error_label.borrow().as_ref() {
            label.remove_css_class(color::ERROR);
            label.set_label("");
        }
    }
}

/// Update the Wi-Fi subtitle based on connection state.
pub fn update_subtitle(state: &WifiCardState, snapshot: &NetworkSnapshot) {
    let subtitle_ref = state.base.subtitle.borrow();
    let Some(label) = subtitle_ref.as_ref() else {
        return;
    };

    let enabled = snapshot.wifi_enabled.unwrap_or(false);
    let is_connecting = snapshot.connecting_ssid.is_some();

    let subtitle = if is_connecting {
        format!(
            "Connecting to {}",
            snapshot.connecting_ssid.as_ref().unwrap()
        )
    } else if let Some(ref ssid) = snapshot.ssid {
        ssid.clone()
    } else if enabled {
        "Enabled".to_string()
    } else {
        "Disabled".to_string()
    };

    let connected = snapshot.ssid.is_some() && !is_connecting;
    label.set_label(&subtitle);
    set_subtitle_active(label, enabled && connected);
}

/// Update the scan button UI and animate while scanning.
pub fn update_scan_ui(
    state: &WifiCardState,
    snapshot: &NetworkSnapshot,
    window: &ApplicationWindow,
) {
    let scanning = snapshot.scanning;

    // Update label text and CSS
    if let Some(label) = state.scan_label.borrow().as_ref() {
        if scanning {
            label.add_css_class(state::SCANNING);
        } else {
            label.set_label("Scan");
            label.remove_css_class(state::SCANNING);
        }
    }

    // Update button sensitivity
    if let Some(button) = state.scan_button.borrow().as_ref() {
        button.set_sensitive(!scanning);
    }

    // Manage animation timeout
    let mut source_opt = state.scan_anim_source.borrow_mut();
    if scanning {
        if source_opt.is_none() {
            // Start a simple dot animation: "Scanning", "Scanning.", ...
            let step_cell = state.scan_anim_step.clone();
            let source_id = glib::timeout_add_local(std::time::Duration::from_millis(450), {
                let window_weak = window.downgrade();
                move || {
                    if let Some(window) = window_weak.upgrade() {
                        // SAFETY: We store a Weak<QuickSettingsWindow> on the window at creation
                        // time with key "vibepanel-qs-window". upgrade() returns None if dropped.
                        unsafe {
                            if let Some(weak_ptr) = window
                                .data::<Weak<super::window::QuickSettingsWindow>>(
                                    "vibepanel-qs-window",
                                )
                                && let Some(qs) = weak_ptr.as_ref().upgrade()
                                && let Some(label) = qs.wifi.scan_label.borrow().as_ref()
                            {
                                let step = step_cell.get().wrapping_add(1) % 4;
                                step_cell.set(step);
                                let dots = match step {
                                    1 => ".",
                                    2 => "..",
                                    3 => "...",
                                    _ => "",
                                };
                                label.set_label(&format!("Scanning{}", dots));
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                }
            });
            *source_opt = Some(source_id);
        }
    } else if let Some(id) = source_opt.take() {
        id.remove();
        state.scan_anim_step.set(0);
    }
}

/// Handle network state changes from NetworkService.
pub fn on_network_changed(
    state: &WifiCardState,
    snapshot: &NetworkSnapshot,
    window: &ApplicationWindow,
) {
    // Handle password dialog state based on connection result
    let current_target = state.password_target_ssid.borrow().clone();
    if let Some(ref target_ssid) = current_target {
        if let Some(ref failed_ssid) = snapshot.failed_ssid {
            if failed_ssid == target_ssid {
                // Connection failed for our target - show error and re-enable form
                debug!("Connection failed for '{}', showing error", failed_ssid);
                set_password_connecting_state(state, false, None);
                if let Some(error_label) = state.password_error_label.borrow().as_ref() {
                    error_label.add_css_class(color::ERROR);
                    error_label.set_label("Wrong password");
                }
                // Clear the failed state so we don't re-trigger
                NetworkService::global().clear_failed_state();
            }
        } else if snapshot.ssid.as_ref() == Some(target_ssid) && snapshot.connecting_ssid.is_none()
        {
            // Successfully connected to target - hide dialog and clear state
            debug!(
                "Successfully connected to '{}', hiding password dialog",
                target_ssid
            );
            hide_password_dialog(state);
        }
        // If connecting_ssid matches target, keep showing animation (do nothing)
    } else if let Some(ref failed_ssid) = snapshot.failed_ssid {
        // No dialog open but connection failed - show dialog with error if window is mapped
        if window.is_mapped() {
            debug!(
                "Connection failed for '{}', showing password dialog with error",
                failed_ssid
            );
            show_password_dialog_with_error(state, failed_ssid, true);
        } else {
            debug!(
                "Connection failed for '{}', but window is closed - clearing failed state",
                failed_ssid
            );
            NetworkService::global().clear_failed_state();
        }
    }

    // Update Wi-Fi toggle state (with signal blocking to prevent feedback loop)
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        let enabled = snapshot.wifi_enabled.unwrap_or(false);
        if toggle.is_active() != enabled {
            // Set the flag to block the toggle signal handler
            state.updating_toggle.set(true);
            toggle.set_active(enabled);
            state.updating_toggle.set(false);
        }
    }

    // Update Wi-Fi card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let enabled = snapshot.wifi_enabled.unwrap_or(false);
        let icon_name = wifi_icon_name(snapshot.connected, enabled);
        icon_handle.set_icon(icon_name);

        let icon_active = enabled && snapshot.connected;
        set_icon_active(icon_handle, icon_active);

        // Additional disabled styling for Wi-Fi
        let backend = icon_handle.backend_widget();
        if !enabled {
            backend.add_css_class(qs::WIFI_DISABLED_ICON);
        } else {
            backend.remove_css_class(qs::WIFI_DISABLED_ICON);
        }
    }

    // Update Wi-Fi subtitle
    update_subtitle(state, snapshot);

    // Update scan button UI (label + animation)
    update_scan_ui(state, snapshot, window);

    // Update network list - but skip if password dialog is visible to avoid layout shifts
    let password_dialog_visible = state
        .password_box
        .borrow()
        .as_ref()
        .is_some_and(|b| b.is_visible());
    if !password_dialog_visible && let Some(list_box) = state.base.list_box.borrow().as_ref() {
        populate_wifi_list(state, list_box, snapshot);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wifi_icon_name_connected() {
        assert_eq!(
            wifi_icon_name(true, true),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_disconnected() {
        assert_eq!(
            wifi_icon_name(false, true),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_disabled() {
        assert_eq!(
            wifi_icon_name(true, false),
            "network-wireless-offline-symbolic"
        );
        assert_eq!(
            wifi_icon_name(false, false),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_wifi_strength_icon_excellent() {
        assert_eq!(
            wifi_strength_icon(100),
            "network-wireless-signal-excellent-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(80),
            "network-wireless-signal-excellent-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(70),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_wifi_strength_icon_good() {
        assert_eq!(
            wifi_strength_icon(69),
            "network-wireless-signal-good-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(60),
            "network-wireless-signal-good-symbolic"
        );
    }

    #[test]
    fn test_wifi_strength_icon_ok() {
        assert_eq!(
            wifi_strength_icon(59),
            "network-wireless-signal-ok-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(40),
            "network-wireless-signal-ok-symbolic"
        );
    }

    #[test]
    fn test_wifi_strength_icon_weak() {
        assert_eq!(
            wifi_strength_icon(39),
            "network-wireless-signal-weak-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(20),
            "network-wireless-signal-weak-symbolic"
        );
    }

    #[test]
    fn test_wifi_strength_icon_none() {
        assert_eq!(
            wifi_strength_icon(19),
            "network-wireless-signal-none-symbolic"
        );
        assert_eq!(
            wifi_strength_icon(0),
            "network-wireless-signal-none-symbolic"
        );
    }
}
