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
use gtk4::{Box as GtkBox, Button, Entry, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow};
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
use crate::services::vpn_secret_agent::VpnAuthRequest;
use crate::styles::{button, color, icon, qs, row, state};

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

    /// Add a pending connect. Keyboard grab release for legacy auth-dialogs
    /// is handled separately via `VpnUpdate::LegacyAuthDialogSpawned`.
    fn begin_connect(&mut self, uuid: &str) {
        self.pending_connects.insert(uuid.to_string());
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
    /// Returns (connect_completed, should_restore_keyboard).
    fn check_pending(&mut self, snapshot: &VpnSnapshot) -> (bool, bool) {
        use crate::services::vpn::VpnState;

        let has_pending = !self.pending_connects.is_empty() || !self.pending_disconnects.is_empty();
        if !has_pending {
            return (false, false);
        }

        let mut connect_completed = false;
        let mut should_restore = false;

        // Check pending connects
        if !self.pending_connects.is_empty() && self.keyboard_released {
            let mut resolved = Vec::new();

            for uuid in &self.pending_connects {
                if let Some(conn) = snapshot.connections.iter().find(|c| &c.uuid == uuid) {
                    match conn.state {
                        VpnState::Activated => {
                            resolved.push(uuid.clone());
                            connect_completed = true;
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
                        }
                        VpnState::Activated | VpnState::Activating | VpnState::Deactivating => {
                            // Still active or in progress, keep waiting
                        }
                    }
                } else {
                    // Connection no longer in snapshot - disconnected
                    resolved.push(uuid.clone());
                }
            }

            for uuid in resolved {
                self.pending_disconnects.remove(&uuid);
            }
        }

        (connect_completed, should_restore)
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

/// Track a user-initiated toggle action before requesting state change.
///
/// When connecting, this releases keyboard grab so external password dialogs
/// can receive input. When disconnecting, this tracks pending disconnect.
pub fn track_toggle_action(uuid: &str, target_active: bool) {
    VPN_KEYBOARD_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if target_active {
            state.begin_connect(uuid);
        } else {
            state.begin_disconnect(uuid);
        }
    });
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
    /// Inline auth prompt container (reusable, re-parented under the matching connection row).
    pub auth_box: RefCell<Option<GtkBox>>,
    /// Password entries keyed by secret field key (e.g., "password").
    pub auth_entries: RefCell<Vec<(String, Entry)>>,
    /// Label showing the connection name / prompt text.
    pub auth_label: RefCell<Option<Label>>,
    /// Status label (shows "Wrong password" on retry).
    pub auth_status_label: RefCell<Option<Label>>,
    /// UUID of the connection we're showing auth for.
    pub auth_target_uuid: RefCell<Option<String>>,
}

impl VpnCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            updating_toggle: Cell::new(false),
            auth_box: RefCell::new(None),
            auth_entries: RefCell::new(Vec::new()),
            auth_label: RefCell::new(None),
            auth_status_label: RefCell::new(None),
            auth_target_uuid: RefCell::new(None),
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

    // Build inline auth prompt box (initially hidden, re-parented as needed)
    build_vpn_auth_box(state);

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
    // Unparent and unrealize the auth box BEFORE clearing the list.
    // Same pattern as network_card.rs: prevents GTK assertion failures when
    // re-parenting the widget to a new row.
    if let Some(auth_box) = state.auth_box.borrow().as_ref()
        && auth_box.parent().is_some()
    {
        auth_box.unrealize();
        auth_box.unparent();
    }

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
    let target_uuid = state.auth_target_uuid.borrow().clone();
    let mut inserted_auth_row = false;

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

        // Insert auth row directly under the matching connection row
        if let Some(ref target) = target_uuid
            && !target.is_empty()
            && *target == conn.uuid
            && let Some(auth_box) = state.auth_box.borrow().as_ref()
        {
            let auth_row = ListBoxRow::new();
            auth_row.set_activatable(false);
            auth_row.set_focusable(true);
            auth_box.set_visible(true);
            auth_row.set_child(Some(auth_box));
            list_box.append(&auth_row);
            inserted_auth_row = true;
        }
    }

    // Fallback: append auth row at end if target UUID not found
    if let Some(target) = target_uuid
        && !target.is_empty()
        && !inserted_auth_row
        && let Some(auth_box) = state.auth_box.borrow().as_ref()
    {
        let auth_row = ListBoxRow::new();
        auth_row.set_activatable(false);
        auth_row.set_focusable(true);
        auth_box.set_visible(true);
        auth_row.set_child(Some(auth_box));
        list_box.append(&auth_row);
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
        let target_active = !is_active;
        track_toggle_action(&uuid, target_active);
        vpn.set_connection_state(&uuid, target_active);
    });

    action_label.upcast()
}

/// Handle VPN state changes from VpnService.
///
/// Returns `true` if a pending connect completed,
/// so caller can close the panel when configured to close on connect.
pub fn on_vpn_changed(state: &Rc<VpnCardState>, snapshot: &VpnSnapshot) -> bool {
    let primary = snapshot.primary();
    let has_connections = !snapshot.connections.is_empty();

    // Check if a pending connect completed and restore keyboard if needed
    let (pending_connect_completed, should_restore) =
        VPN_KEYBOARD_STATE.with(|s| s.borrow_mut().check_pending(snapshot));

    if should_restore {
        VPN_KEYBOARD_STATE.with(|s| s.borrow_mut().restore_if_released());
    }

    // Handle legacy auth-dialog keyboard grab management.
    // When a legacy auth-dialog (like OpenConnect) spawns its own GTK window,
    // we release the exclusive keyboard grab so the dialog can receive input.
    // When the dialog finishes, we restore the grab.
    VPN_KEYBOARD_STATE.with(|s| {
        let mut state = s.borrow_mut();
        if snapshot.legacy_auth_dialog_active && !state.keyboard_released {
            debug!("VPN: Releasing keyboard grab for legacy auth-dialog");
            if let Some(ref weak) = state.qs_window
                && let Some(qs) = weak.upgrade()
            {
                qs.release_keyboard_grab();
            }
            state.keyboard_released = true;
        } else if !snapshot.legacy_auth_dialog_active && state.keyboard_released {
            state.restore_if_released();
        }
    });

    // Handle auth request from NM SecretAgent
    if let Some(ref auth_request) = snapshot.auth_request {
        let current_target = state.auth_target_uuid.borrow().clone();
        if current_target.as_deref() != Some(&auth_request.uuid) {
            // New auth request — show the prompt
            show_vpn_auth_dialog(state, auth_request);
        }
    } else {
        // No auth request — hide prompt if one was showing
        let had_auth = state.auth_target_uuid.borrow().is_some();
        if had_auth {
            hide_vpn_auth_dialog(state);
        }
    }

    // Update toggle state and sensitivity
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        let should_be_active = primary.map(|p| p.active).unwrap_or(false);
        if toggle.is_active() != should_be_active {
            state.updating_toggle.set(true);
            toggle.set_active(should_be_active);
            state.updating_toggle.set(false);
        }
        // Disable toggle when service unavailable or no connections
        toggle.set_sensitive(snapshot.available && has_connections);
    }

    // Update VPN card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let icon_name = vpn_icon_name();
        icon_handle.set_icon(icon_name);

        // Service unavailable - use error styling
        if !snapshot.available {
            icon_handle.add_css_class(state::SERVICE_UNAVAILABLE);
            icon_handle.remove_css_class(state::ICON_ACTIVE);
        } else {
            icon_handle.remove_css_class(state::SERVICE_UNAVAILABLE);
            set_icon_active(icon_handle, snapshot.any_active);
        }
    }

    // Update VPN subtitle
    if let Some(label) = state.base.subtitle.borrow().as_ref() {
        let subtitle = if !snapshot.available {
            "Unavailable".to_string()
        } else if !snapshot.is_ready {
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
        set_subtitle_active(label, snapshot.available && snapshot.any_active);
    }

    // Update connection list
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        populate_vpn_list(state, list_box, snapshot);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }

    pending_connect_completed
}

// --- VPN Auth UI ---

/// Build the reusable inline auth prompt box.
///
/// This is created once and re-parented into the connection list beneath the
/// matching VPN row whenever NM sends a GetSecrets request. Follows the same
/// pattern as the WiFi password box in network_card.rs.
fn build_vpn_auth_box(state: &Rc<VpnCardState>) {
    let auth_box = GtkBox::new(Orientation::Vertical, 6);
    auth_box.set_visible(false);

    let auth_label = Label::new(Some(""));
    auth_label.set_xalign(0.0);
    auth_label.set_wrap(true);
    auth_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    auth_label.set_max_width_chars(40);
    auth_box.append(&auth_label);

    // Entries are created dynamically per-request (in show_vpn_auth_dialog)
    // because the number and type of fields varies per VPN connection.

    // Button row: [status label (expands)] [cancel] [connect]
    let btn_row = GtkBox::new(Orientation::Horizontal, 8);

    // Status label (shows "Wrong password" on retry)
    let status_label = Label::new(Some(""));
    status_label.set_xalign(0.0);
    status_label.set_hexpand(true);
    btn_row.append(&status_label);

    let btn_cancel = Button::with_label("Cancel");
    btn_cancel.add_css_class(button::CARD);
    let btn_ok = Button::with_label("Connect");
    btn_ok.add_css_class(button::ACCENT);

    // Apply Pango font attrs to fix text clipping on layer-shell surfaces
    let style_mgr = SurfaceStyleManager::global();
    style_mgr.apply_pango_attrs(&auth_label);
    style_mgr.apply_pango_attrs(&status_label);

    {
        let state_weak = Rc::downgrade(state);
        btn_cancel.connect_clicked(move |_| {
            if let Some(state) = state_weak.upgrade() {
                on_vpn_auth_cancel_clicked(&state);
            }
        });
    }

    {
        let state_weak = Rc::downgrade(state);
        btn_ok.connect_clicked(move |_| {
            if let Some(state) = state_weak.upgrade() {
                on_vpn_auth_connect_clicked(&state);
            }
        });
    }

    btn_row.append(&btn_cancel);
    btn_row.append(&btn_ok);
    auth_box.append(&btn_row);

    *state.auth_box.borrow_mut() = Some(auth_box);
    *state.auth_label.borrow_mut() = Some(auth_label);
    *state.auth_status_label.borrow_mut() = Some(status_label);
}

/// Show the VPN auth dialog for the given auth request.
fn show_vpn_auth_dialog(state: &Rc<VpnCardState>, request: &VpnAuthRequest) {
    *state.auth_target_uuid.borrow_mut() = Some(request.uuid.clone());

    // Update the prompt label
    if let Some(label) = state.auth_label.borrow().as_ref() {
        let text = if let Some(ref desc) = request.description {
            // Use description from auth-dialog (external-ui-mode)
            desc.clone()
        } else if request.is_retry {
            format!("Re-enter credentials for {}", request.name)
        } else {
            format!("Enter credentials for {}", request.name)
        };
        label.set_label(&text);
    }

    // Show retry error if applicable
    if let Some(status_label) = state.auth_status_label.borrow().as_ref() {
        if request.is_retry {
            status_label.add_css_class(color::ERROR);
            status_label.set_label("Wrong password");
        } else {
            status_label.remove_css_class(color::ERROR);
            status_label.set_label("");
        }
    }

    // Rebuild auth box contents: remove old entries, create new ones.
    // The auth box layout is: [label] [entry...] [btn_row]
    // We reconstruct by removing all children, then re-appending in order.
    if let Some(auth_box) = state.auth_box.borrow().as_ref() {
        let label = state.auth_label.borrow().as_ref().cloned();

        // Collect the button row (last child) — it contains status + cancel + connect
        let btn_row = {
            let mut last = auth_box.first_child();
            let mut prev = None;
            while let Some(w) = last {
                prev = Some(w.clone());
                last = w.next_sibling();
            }
            prev
        };

        // Remove all children
        while let Some(child) = auth_box.first_child() {
            auth_box.remove(&child);
        }

        // Re-append label
        if let Some(ref lbl) = label {
            auth_box.append(lbl);
        }

        // Create new entries for each requested field
        let mut entries = Vec::new();

        for field in &request.fields {
            let entry = Entry::new();
            entry.set_placeholder_text(Some(&field.label));
            entry.set_visibility(false);
            entry.set_input_purpose(gtk4::InputPurpose::Password);
            entry.set_can_focus(true);
            entry.set_focus_on_click(true);

            // Pre-fill with value from auth-dialog (external-ui-mode)
            if let Some(ref value) = field.value {
                entry.set_text(value);
            }

            // Enter key submits
            {
                let state_weak = Rc::downgrade(state);
                entry.connect_activate(move |_| {
                    if let Some(state) = state_weak.upgrade() {
                        on_vpn_auth_connect_clicked(&state);
                    }
                });
            }

            // Auto-focus first entry when mapped
            if entries.is_empty() {
                entry.connect_map(move |entry| {
                    entry.grab_focus();
                });
            }

            auth_box.append(&entry);
            entries.push((field.key.clone(), entry));
        }

        // Re-append button row
        if let Some(ref row) = btn_row {
            auth_box.append(row);
        }

        *state.auth_entries.borrow_mut() = entries;
    }

    // Expand the VPN card revealer so the auth row is visible
    if let Some(revealer) = state.base.revealer.borrow().as_ref() {
        revealer.set_reveal_child(true);
    }
    if let Some(arrow) = state.base.arrow.borrow().as_ref() {
        arrow.set_icon("keyboard_arrow_up");
    }

    // Repopulate list to position the auth box under the correct connection
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        let snapshot = VpnService::global().snapshot();
        populate_vpn_list(state, list_box, &snapshot);
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

/// Hide the VPN auth dialog and reset its state.
fn hide_vpn_auth_dialog(state: &Rc<VpnCardState>) {
    // Clear entries
    for (_key, entry) in state.auth_entries.borrow().iter() {
        entry.set_text("");
    }
    *state.auth_entries.borrow_mut() = Vec::new();

    if let Some(box_) = state.auth_box.borrow().as_ref() {
        box_.set_visible(false);
    }

    // Clear status label
    if let Some(status_label) = state.auth_status_label.borrow().as_ref() {
        status_label.remove_css_class(color::ERROR);
        status_label.set_label("");
    }

    *state.auth_target_uuid.borrow_mut() = None;

    // Repopulate list without auth row
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        let snapshot = VpnService::global().snapshot();
        populate_vpn_list(state, list_box, &snapshot);
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

/// Handle Cancel click on the VPN auth dialog.
fn on_vpn_auth_cancel_clicked(state: &Rc<VpnCardState>) {
    hide_vpn_auth_dialog(state);
    VpnService::global().cancel_vpn_auth();
}

/// Handle Connect click on the VPN auth dialog.
fn on_vpn_auth_connect_clicked(state: &Rc<VpnCardState>) {
    let secrets: Vec<(String, String)> = state
        .auth_entries
        .borrow()
        .iter()
        .map(|(key, entry)| (key.clone(), entry.text().to_string()))
        .collect();

    if secrets.is_empty() {
        return;
    }

    VpnService::global().submit_vpn_secrets(&secrets);
    hide_vpn_auth_dialog(state);
}
