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
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{ApplicationWindow, Box as GtkBox, ListBox, Orientation, ScrolledWindow};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, add_placeholder_row, build_accent_subtitle, clear_list_box,
    create_qs_list_box, create_row_action_label, set_icon_active, set_subtitle_active,
};
use crate::services::icons::IconsService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::vpn::{VpnConnection, VpnService, VpnSnapshot};
use crate::styles::{color, icon, qs, row};

// Global state for VPN keyboard grab management.
// This needs to be global because QuickSettingsWindow is recreated each time it opens,
// but we need to track pending connects across those recreations.
thread_local! {
    /// UUIDs of VPN connections we initiated a connect for (survives window recreation).
    static PENDING_CONNECTS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Whether we've temporarily released keyboard grab for a pending connect.
    static KEYBOARD_GRAB_RELEASED: Cell<bool> = const { Cell::new(false) };
}

/// Add a VPN UUID to the pending connects set (for toggle-initiated connections).
pub fn add_pending_connect(uuid: &str) {
    PENDING_CONNECTS.with(|p| p.borrow_mut().insert(uuid.to_string()));
}

/// Restore keyboard mode if it was released for VPN password dialogs.
/// Called when Quick Settings panel is hidden.
pub fn restore_keyboard_if_released() {
    KEYBOARD_GRAB_RELEASED.with(|keyboard_cell| {
        if keyboard_cell.get() {
            debug!("VPN: Panel closing, restoring keyboard mode");
            if let Some(qs) = find_quick_settings_window() {
                qs.restore_keyboard_mode();
            }
            keyboard_cell.set(false);
            // Also clear pending connects since we're closing
            PENDING_CONNECTS.with(|p| p.borrow_mut().clear());
        }
    });
}

/// Return an icon name for VPN state.
///
/// Uses standard GTK/Adwaita icon names.
pub fn vpn_icon_name(_any_active: bool) -> &'static str {
    // Always returns "network-vpn" - some themes have state variants but
    // they're not widely supported.
    "network-vpn"
}

/// Find the QuickSettingsWindow by searching all toplevels.
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

        // When connecting (not disconnecting), release keyboard grab to allow
        // external password dialogs (nm-applet, keyring unlock, etc.) to receive input.
        // The grab will be restored when the VPN state changes.
        if !is_active {
            PENDING_CONNECTS.with(|p| p.borrow_mut().insert(uuid.clone()));

            // Release keyboard grab so password dialogs can receive input
            if let Some(qs) = find_quick_settings_window() {
                debug!("VPN: Releasing keyboard grab for pending connect");
                qs.release_keyboard_grab();
                KEYBOARD_GRAB_RELEASED.with(|k| k.set(true));
            } else {
                debug!("VPN: Could not find QuickSettingsWindow to release keyboard grab");
            }

            // Set a timeout to restore keyboard grab if the connection doesn't resolve.
            // This handles cases where the user cancels the password dialog or the
            // connection fails without triggering a state change we can detect.
            let uuid_timeout = uuid.clone();
            glib::timeout_add_local_once(Duration::from_secs(30), move || {
                // Remove this UUID from pending (it timed out)
                PENDING_CONNECTS.with(|p| p.borrow_mut().remove(&uuid_timeout));

                // Restore keyboard grab if we released it and no other pending connects
                let should_restore = KEYBOARD_GRAB_RELEASED.with(|k| k.get())
                    && PENDING_CONNECTS.with(|p| p.borrow().is_empty());

                if should_restore {
                    debug!("VPN: Timeout reached, restoring keyboard mode");
                    if let Some(qs) = find_quick_settings_window() {
                        qs.restore_keyboard_mode();
                    }
                    KEYBOARD_GRAB_RELEASED.with(|k| k.set(false));
                }
            });
        }

        vpn.set_connection_state(&uuid, !is_active);
    });

    action_label.upcast()
}

/// Handle VPN state changes from VpnService.
///
/// Returns `true` if a pending connect succeeded (caller should close panel if configured).
pub fn on_vpn_changed(state: &Rc<VpnCardState>, snapshot: &VpnSnapshot) -> bool {
    use crate::services::vpn::VpnState;

    let primary = snapshot.primary();
    let has_connections = !snapshot.connections.is_empty();

    // Check if any pending connect completed (succeeded or failed).
    // We use the VPN state to determine when authentication is complete:
    // - Activating (1): Still waiting for credentials - do NOT restore keyboard
    // - Activated (2): Fully connected - restore keyboard
    // - Deactivated (4): Connection failed/cancelled - restore keyboard
    let mut pending_connect_succeeded = false;
    let mut should_restore_keyboard = false;

    PENDING_CONNECTS.with(|pending_cell| {
        let mut pending = pending_cell.borrow_mut();
        let keyboard_released = KEYBOARD_GRAB_RELEASED.with(|k| k.get());

        debug!(
            "VPN on_vpn_changed: pending_connects = {:?}, keyboard_grab_released = {}",
            *pending, keyboard_released
        );

        if !pending.is_empty() && keyboard_released {
            // Check each pending UUID's state
            let mut resolved_uuids = Vec::new();

            for uuid in pending.iter() {
                // Find this connection in the snapshot
                if let Some(conn) = snapshot.connections.iter().find(|c| &c.uuid == uuid) {
                    debug!("VPN on_vpn_changed: {} state={:?}", uuid, conn.state);

                    match conn.state {
                        VpnState::Activated => {
                            // Fully connected - success!
                            debug!(
                                "VPN on_vpn_changed: {} is now ACTIVATED (fully connected)",
                                uuid
                            );
                            resolved_uuids.push(uuid.clone());
                            pending_connect_succeeded = true;
                            should_restore_keyboard = true;
                        }
                        VpnState::Deactivated | VpnState::Unknown => {
                            // Connection failed or was cancelled
                            debug!(
                                "VPN on_vpn_changed: {} is DEACTIVATED/UNKNOWN (failed)",
                                uuid
                            );
                            resolved_uuids.push(uuid.clone());
                            should_restore_keyboard = true;
                        }
                        VpnState::Activating => {
                            // Still waiting for credentials - keep keyboard released
                            debug!(
                                "VPN on_vpn_changed: {} is ACTIVATING (waiting for creds)",
                                uuid
                            );
                        }
                        VpnState::Deactivating => {
                            // Connection is being torn down
                            debug!("VPN on_vpn_changed: {} is DEACTIVATING", uuid);
                        }
                    }
                } else {
                    // Connection no longer in snapshot - it was removed (failed)
                    debug!(
                        "VPN on_vpn_changed: {} no longer in snapshot (failed/cancelled)",
                        uuid
                    );
                    resolved_uuids.push(uuid.clone());
                    should_restore_keyboard = true;
                }
            }

            // Remove resolved UUIDs from pending
            for uuid in resolved_uuids {
                pending.remove(&uuid);
            }
        }
    });

    // Restore keyboard mode if a pending connection was resolved
    if should_restore_keyboard {
        debug!("VPN on_vpn_changed: Connection resolved, restoring keyboard mode");
        if let Some(qs) = find_quick_settings_window() {
            qs.restore_keyboard_mode();
        }
        KEYBOARD_GRAB_RELEASED.with(|k| k.set(false));
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
        let icon_name = vpn_icon_name(snapshot.any_active);
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

    pending_connect_succeeded
}
