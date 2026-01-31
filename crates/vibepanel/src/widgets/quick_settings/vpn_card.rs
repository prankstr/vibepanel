//! VPN card for Quick Settings panel.
//!
//! This module contains:
//! - VPN icon helpers (merged from qs_vpn_helpers.rs)
//! - VPN details panel building
//! - Connection list population
//! - Connection action handling

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, ListBox, Orientation, ScrolledWindow};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, add_placeholder_row, build_accent_subtitle, clear_list_box,
    create_qs_list_box, create_row_action_label, set_icon_active, set_subtitle_active,
};
use super::window::QuickSettingsWindow;
use crate::services::icons::IconsService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::vpn::{VpnConnection, VpnService, VpnSnapshot};
use crate::styles::{color, icon, qs, row};

// Global state for VPN keyboard grab management.
// This needs to be global because QuickSettingsWindow is recreated each time it opens,
// but we need to track pending connects across those recreations.

/// Manages keyboard grab state during VPN authentication and tracks pending actions.
///
/// When a VPN connection is initiated that may require a password dialog,
/// we release the keyboard grab so the dialog can receive input. This struct
/// tracks which connections are pending and whether the grab was released.
struct VpnKeyboardState {
    /// UUIDs of VPN connections we initiated a connect for.
    pending_connects: HashSet<String>,
    /// UUIDs of VPN connections we initiated a disconnect for.
    pending_disconnects: HashSet<String>,
    /// Whether we've temporarily released keyboard grab.
    keyboard_released: bool,
    /// Weak reference to the QuickSettingsWindow for keyboard grab management.
    /// This is set when the QS window is created and cleared when it closes.
    qs_window: Option<Weak<QuickSettingsWindow>>,
}

impl VpnKeyboardState {
    fn new() -> Self {
        Self {
            pending_connects: HashSet::new(),
            pending_disconnects: HashSet::new(),
            keyboard_released: false,
            qs_window: None,
        }
    }

    /// Set the QuickSettingsWindow reference for keyboard grab management.
    fn set_qs_window(&mut self, qs: Weak<QuickSettingsWindow>) {
        self.qs_window = Some(qs);
    }

    /// Clear the QuickSettingsWindow reference (called when QS closes).
    fn clear_qs_window(&mut self) {
        self.qs_window = None;
    }

    /// Add a pending connect and release keyboard grab.
    fn begin_connect(&mut self, uuid: &str) {
        self.pending_connects.insert(uuid.to_string());
        if let Some(ref weak) = self.qs_window
            && let Some(qs) = weak.upgrade()
        {
            debug!("VPN: Releasing keyboard grab for pending connect");
            qs.release_keyboard_grab();
            self.keyboard_released = true;
        }
    }

    /// Add a pending disconnect.
    fn begin_disconnect(&mut self, uuid: &str) {
        self.pending_disconnects.insert(uuid.to_string());
    }

    /// Restore keyboard grab if it was released.
    fn restore_if_released(&mut self) {
        if self.keyboard_released {
            debug!("VPN: Restoring keyboard mode");
            if let Some(ref weak) = self.qs_window
                && let Some(qs) = weak.upgrade()
            {
                qs.restore_keyboard_mode();
            }
            self.keyboard_released = false;
        }
    }

    /// Clear all state (called when panel closes).
    fn clear(&mut self) {
        self.restore_if_released();
        self.pending_connects.clear();
        self.pending_disconnects.clear();
        self.clear_qs_window();
    }

    /// Check and resolve pending connections based on VPN snapshot.
    /// Returns (any_action_completed, should_restore_keyboard).
    fn check_pending(&mut self, snapshot: &VpnSnapshot) -> (bool, bool) {
        use crate::services::vpn::VpnState;

        let has_pending = !self.pending_connects.is_empty() || !self.pending_disconnects.is_empty();
        if !has_pending {
            return (false, false);
        }

        let mut any_action_completed = false;
        let mut should_restore = false;

        // Check pending connects
        if !self.pending_connects.is_empty() && self.keyboard_released {
            let mut resolved = Vec::new();

            for uuid in &self.pending_connects {
                if let Some(conn) = snapshot.connections.iter().find(|c| &c.uuid == uuid) {
                    match conn.state {
                        VpnState::Activated => {
                            resolved.push(uuid.clone());
                            any_action_completed = true;
                            should_restore = true;
                        }
                        VpnState::Deactivated | VpnState::Unknown => {
                            resolved.push(uuid.clone());
                            should_restore = true;
                        }
                        VpnState::Activating | VpnState::Deactivating => {
                            // Still in progress, keep waiting
                        }
                    }
                } else {
                    // Connection no longer in snapshot (failed/cancelled)
                    resolved.push(uuid.clone());
                    should_restore = true;
                }
            }

            for uuid in resolved {
                self.pending_connects.remove(&uuid);
            }
        }

        // Check pending disconnects
        if !self.pending_disconnects.is_empty() {
            let mut resolved = Vec::new();

            for uuid in &self.pending_disconnects {
                if let Some(conn) = snapshot.connections.iter().find(|c| &c.uuid == uuid) {
                    match conn.state {
                        VpnState::Deactivated | VpnState::Unknown => {
                            resolved.push(uuid.clone());
                            any_action_completed = true;
                        }
                        VpnState::Activated | VpnState::Activating | VpnState::Deactivating => {
                            // Still active or in progress, keep waiting
                        }
                    }
                } else {
                    // Connection no longer in snapshot - disconnected
                    resolved.push(uuid.clone());
                    any_action_completed = true;
                }
            }

            for uuid in resolved {
                self.pending_disconnects.remove(&uuid);
            }
        }

        (any_action_completed, should_restore)
    }
}

thread_local! {
    /// Global state for VPN keyboard grab management.
    ///
    /// This is thread-local (not per-QS-window) because QuickSettingsWindow is
    /// recreated on each open, but pending connect/disconnect tracking must
    /// survive those recreations. State is cleared when the panel closes via
    /// `restore_keyboard_if_released()`.
    static VPN_KEYBOARD_STATE: RefCell<VpnKeyboardState> = RefCell::new(VpnKeyboardState::new());
}

/// Set the QuickSettingsWindow reference for VPN keyboard grab management.
///
/// Called when QuickSettingsWindow is created to enable proper keyboard
/// release/restore during VPN authentication dialogs.
pub fn set_quick_settings_window(qs: Weak<QuickSettingsWindow>) {
    VPN_KEYBOARD_STATE.with(|state| state.borrow_mut().set_qs_window(qs));
}

/// Add a VPN UUID to the pending connects set (for toggle-initiated connections).
pub fn add_pending_connect(uuid: &str) {
    VPN_KEYBOARD_STATE.with(|state| state.borrow_mut().pending_connects.insert(uuid.to_string()));
}

/// Restore keyboard mode if it was released for VPN password dialogs.
/// Called when Quick Settings panel is hidden.
pub fn restore_keyboard_if_released() {
    VPN_KEYBOARD_STATE.with(|state| state.borrow_mut().clear());
}

/// Return an icon name for VPN state.
///
/// Uses standard GTK/Adwaita icon names. Currently returns a fixed icon name
/// since VPN state variants aren't widely supported across themes.
pub fn vpn_icon_name() -> &'static str {
    // Always returns "network-vpn" - some themes have state variants but
    // they're not widely supported.
    "network-vpn"
}

/// State for the VPN card in the Quick Settings panel.
///
/// Uses `ExpandableCardBase` for common expandable card fields.
/// Note: `pending_connects` and `keyboard_grab_released` are now thread-local globals
/// to survive QuickSettingsWindow recreations.
pub struct VpnCardState {
    /// Common expandable card state (toggle, icon, subtitle, list_box, revealer, arrow).
    pub base: ExpandableCardBase,
    /// Guard flag to prevent feedback loops when programmatically updating toggle.
    pub updating_toggle: Cell<bool>,
}

impl VpnCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            updating_toggle: Cell::new(false),
        }
    }
}

impl Default for VpnCardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for VpnCardState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

/// Result of building VPN details section.
pub struct VpnDetailsResult {
    pub container: GtkBox,
    pub list_box: ListBox,
}

/// Build the VPN details section with connection list.
pub fn build_vpn_details(state: &Rc<VpnCardState>) -> VpnDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    // Small top margin for visual spacing
    container.set_margin_top(6);

    // VPN connection list (no scan button needed)
    let list_box = create_qs_list_box();

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_child(Some(&list_box));
    scroller.set_max_content_height(360);
    scroller.set_propagate_natural_height(true);

    container.append(&scroller);

    // Populate with current VPN state
    let snapshot = VpnService::global().snapshot();
    populate_vpn_list(state, &list_box, &snapshot);

    VpnDetailsResult {
        container,
        list_box,
    }
}

/// Populate the VPN list with connection data from snapshot.
pub fn populate_vpn_list(state: &Rc<VpnCardState>, list_box: &ListBox, snapshot: &VpnSnapshot) {
    clear_list_box(list_box);

    if !snapshot.is_ready {
        add_placeholder_row(list_box, "Loading VPN state...");
        return;
    }

    if snapshot.connections.is_empty() {
        add_placeholder_row(list_box, "No VPN connections");
        return;
    }

    let icons = IconsService::global();

    for conn in &snapshot.connections {
        // Build extra parts (Autoconnect, VPN type)
        let mut extra_parts = Vec::new();
        if conn.autoconnect {
            extra_parts.push("Autoconnect");
        }
        // Show VPN type
        if conn.vpn_type == "wireguard" {
            extra_parts.push("WireGuard");
        } else if conn.vpn_type == "vpn" {
            extra_parts.push("OpenVPN");
        }

        let icon_color = if conn.active {
            color::ACCENT
        } else {
            color::PRIMARY
        };
        let icon_handle = icons.create_icon("network-vpn", &[icon::TEXT, row::QS_ICON, icon_color]);
        let leading_icon = icon_handle.widget();

        let right_widget = create_vpn_action_widget(state, conn);

        let mut row_builder = ListRow::builder()
            .title(&conn.name)
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::VPN_ROW);

        if conn.active {
            // Active: accent "Active" + muted extras
            let subtitle_widget = build_accent_subtitle("Active", &extra_parts);
            row_builder = row_builder.subtitle_widget(subtitle_widget.upcast());
        } else {
            // Inactive: plain muted subtitle
            let mut parts = vec!["Inactive"];
            parts.extend(extra_parts);
            let subtitle = parts.join(" \u{2022} ");
            row_builder = row_builder.subtitle(&subtitle);
        }

        let row_result = row_builder.build();

        // Note: Click handling is done by the action widget's gesture,
        // not by row activation, to avoid double-triggering.

        list_box.append(&row_result.row);
    }
}

/// Create the action widget for a VPN connection row.
fn create_vpn_action_widget(_state: &Rc<VpnCardState>, conn: &VpnConnection) -> gtk4::Widget {
    let uuid = conn.uuid.clone();
    let is_active = conn.active;

    // Single action: "Disconnect" or "Connect" as accent-colored text
    let action_text = if is_active { "Disconnect" } else { "Connect" };
    let action_label = create_row_action_label(action_text);

    action_label.connect_clicked(move |_| {
        let vpn = VpnService::global();

        if is_active {
            // Track pending disconnect for close-on-action
            VPN_KEYBOARD_STATE.with(|state| state.borrow_mut().begin_disconnect(&uuid));
        } else {
            // When connecting, release keyboard grab to allow external password dialogs
            // (nm-applet, keyring unlock, etc.) to receive input.
            // The grab will be restored when the VPN state changes or the panel closes.
            VPN_KEYBOARD_STATE.with(|state| state.borrow_mut().begin_connect(&uuid));
        }

        vpn.set_connection_state(&uuid, !is_active);
    });

    action_label.upcast()
}

/// Handle VPN state changes from VpnService.
///
/// Returns `true` if a pending action completed (connect or disconnect),
/// so caller can close the panel if configured.
pub fn on_vpn_changed(state: &Rc<VpnCardState>, snapshot: &VpnSnapshot) -> bool {
    let primary = snapshot.primary();
    let has_connections = !snapshot.connections.is_empty();

    // Check if any pending action completed and restore keyboard if needed
    let (pending_action_completed, should_restore) =
        VPN_KEYBOARD_STATE.with(|s| s.borrow_mut().check_pending(snapshot));

    if should_restore {
        VPN_KEYBOARD_STATE.with(|s| s.borrow_mut().restore_if_released());
    }

    // Update toggle state and sensitivity
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        let should_be_active = primary.map(|p| p.active).unwrap_or(false);
        if toggle.is_active() != should_be_active {
            state.updating_toggle.set(true);
            toggle.set_active(should_be_active);
            state.updating_toggle.set(false);
        }
        toggle.set_sensitive(has_connections);
    }

    // Update VPN card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let icon_name = vpn_icon_name();
        icon_handle.set_icon(icon_name);
        set_icon_active(icon_handle, snapshot.any_active);
    }

    // Update VPN subtitle
    if let Some(label) = state.base.subtitle.borrow().as_ref() {
        let subtitle = if !snapshot.is_ready {
            "VPN".to_string()
        } else if let Some(p) = primary {
            if p.active {
                p.name.clone()
            } else {
                "Disconnected".to_string()
            }
        } else {
            "No connections".to_string()
        };
        label.set_label(&subtitle);
        set_subtitle_active(label, snapshot.any_active);
    }

    // Update connection list
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        populate_vpn_list(state, list_box, snapshot);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }

    pending_action_completed
}
