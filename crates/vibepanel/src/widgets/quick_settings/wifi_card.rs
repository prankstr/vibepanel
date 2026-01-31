//! Wi-Fi card for Quick Settings panel.
//!
//! This module contains:
//! - Wi-Fi icon helpers (merged from qs_wifi_helpers.rs)
//! - Wi-Fi details panel building
//! - Network list population
//! - Password dialog handling

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk4::glib::{self, WeakRef};
use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box as GtkBox, Button, Entry, Label, ListBox, ListBoxRow, Orientation,
    Overlay, Popover, ScrolledWindow, Switch,
};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, ScanButton, add_placeholder_row, build_accent_subtitle,
    clear_list_box, create_qs_list_box, create_row_action_label, create_row_menu_action,
    create_row_menu_button, set_icon_active,
};
use super::window::current_quick_settings_window;
use crate::services::icons::IconsService;
use crate::services::network::{NetworkService, NetworkSnapshot, WifiNetwork};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, icon, qs, row, state, surface};
use crate::widgets::base::configure_popover;

/// Return a simple connected/disconnected Wi-Fi icon.
///
/// The main card widget uses this for a stable "connected" icon,
/// while the per-network list rows use `wifi_strength_icon` for
/// detailed signal levels.
pub fn wifi_icon_name(
    connected: bool,
    wifi_enabled: bool,
    wired_connected: bool,
    has_wifi_device: bool,
) -> &'static str {
    if wired_connected {
        "network-wired-symbolic"
    } else if !has_wifi_device {
        // Ethernet-only system, not connected - show lan icon (will be grayed out)
        "network-wired-symbolic"
    } else if !wifi_enabled {
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

/// Result of building the network card subtitle widget.
pub struct NetworkSubtitleResult {
    /// The container widget holding the label.
    pub container: GtkBox,
    /// Label for text (SSID or status).
    pub label: Label,
}

/// Build the subtitle widget for the network card.
///
/// Creates a label that shows connection status text like:
/// - "Ethernet • SSID" (both connected)
/// - "Ethernet" (wired only)
/// - "SSID" (Wi-Fi only)
/// - "Disconnected" / "Off"
pub fn build_network_subtitle(snapshot: &NetworkSnapshot) -> NetworkSubtitleResult {
    use gtk4::pango::EllipsizeMode;

    let container = GtkBox::new(Orientation::Horizontal, 4);
    container.add_css_class(qs::TOGGLE_SUBTITLE);

    let label = Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(EllipsizeMode::End);
    label.set_single_line_mode(true);
    label.add_css_class(color::MUTED);
    container.append(&label);

    // Set initial state
    update_network_subtitle(&label, snapshot);

    NetworkSubtitleResult { container, label }
}

/// Generate the subtitle text for the network card based on connection state.
///
/// Returns a string describing the current network status:
/// - Wired + connecting: "Ethernet · Connecting to {ssid}"
/// - Wired + Wi-Fi connected: "Ethernet · {ssid}"
/// - Wired only: "Ethernet"
/// - Wi-Fi connecting: "Connecting to {ssid}"
/// - Wi-Fi connected: "{ssid}"
/// - Disconnected (has Wi-Fi): "Disconnected"
/// - Wi-Fi disabled: "Off"
/// - Ethernet-only system, disconnected: "Disconnected"
pub fn get_network_subtitle_text(snapshot: &NetworkSnapshot) -> String {
    let wifi_enabled = snapshot.wifi_enabled.unwrap_or(false);
    let is_connecting = snapshot.connecting_ssid.is_some();

    match (snapshot.wired_connected, is_connecting, &snapshot.ssid) {
        // Wired connected cases
        (true, true, _) => format!(
            "Ethernet \u{2022} Connecting to {}",
            snapshot.connecting_ssid.as_ref().unwrap()
        ),
        (true, false, Some(ssid)) => format!("Ethernet \u{2022} {}", ssid),
        (true, false, None) => "Ethernet".to_string(),

        // Wi-Fi only cases
        (false, true, _) => format!(
            "Connecting to {}",
            snapshot.connecting_ssid.as_ref().unwrap()
        ),
        (false, false, Some(ssid)) => ssid.clone(),
        (false, false, None) if !snapshot.has_wifi_device => "Disconnected".to_string(),
        (false, false, None) if wifi_enabled => "Disconnected".to_string(),
        (false, false, None) => "Off".to_string(),
    }
}

/// Determine if the network subtitle should be styled as "active" (connected).
///
/// Returns true when any network is connected and not in a connecting state.
pub fn is_network_subtitle_active(snapshot: &NetworkSnapshot) -> bool {
    let wifi_connected = snapshot.ssid.is_some();
    let is_connecting = snapshot.connecting_ssid.is_some();
    let any_connected = snapshot.wired_connected || wifi_connected;

    any_connected && !is_connecting
}

/// Update the network subtitle label based on connection state.
pub fn update_network_subtitle(label: &Label, snapshot: &NetworkSnapshot) {
    label.set_label(&get_network_subtitle_text(snapshot));

    if is_network_subtitle_active(snapshot) {
        label.remove_css_class(color::MUTED);
        label.add_css_class(state::SUBTITLE_ACTIVE);
    } else {
        label.remove_css_class(state::SUBTITLE_ACTIVE);
        label.add_css_class(color::MUTED);
    }
}

/// State for the Wi-Fi card in the Quick Settings panel.
///
/// Uses `ExpandableCardBase` for common expandable card fields and adds
/// Wi-Fi specific state (scan button, password dialog, animation).
pub struct WifiCardState {
    /// Common expandable card state (toggle, icon, subtitle, list_box, revealer, arrow).
    pub base: ExpandableCardBase,
    /// Card title label (for updating between "Wi-Fi" and "Network").
    pub title_label: RefCell<Option<Label>>,
    /// Text label in the subtitle (SSID or status).
    pub subtitle_label: RefCell<Option<Label>>,
    /// The Wi-Fi scan button (self-contained with animation).
    pub scan_button: RefCell<Option<Rc<ScanButton>>>,
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
    /// Connect animation GLib source ID.
    pub connect_anim_source: RefCell<Option<glib::SourceId>>,
    /// Connect animation step counter.
    pub connect_anim_step: Cell<u8>,
    /// Flag to prevent toggle signal handler from firing during programmatic updates.
    /// This prevents feedback loops when the service notifies us of state changes.
    pub updating_toggle: Cell<bool>,
    /// The Wi-Fi switch row container (label + switch + scan button).
    pub wifi_switch_row: RefCell<Option<GtkBox>>,
    /// The Wi-Fi label in the expanded details section.
    /// Only visible when ethernet device is present.
    pub wifi_label: RefCell<Option<Label>>,
    /// The Wi-Fi switch in the expanded details section.
    /// Only visible when ethernet device is present.
    pub wifi_switch: RefCell<Option<Switch>>,
    /// Ethernet row container (shown above Wi-Fi controls when connected).
    pub ethernet_row: RefCell<Option<GtkBox>>,
}

impl WifiCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            title_label: RefCell::new(None),
            subtitle_label: RefCell::new(None),
            scan_button: RefCell::new(None),
            password_box: RefCell::new(None),
            password_label: RefCell::new(None),
            password_error_label: RefCell::new(None),
            password_entry: RefCell::new(None),
            password_cancel_button: RefCell::new(None),
            password_connect_button: RefCell::new(None),
            password_target_ssid: RefCell::new(None),
            connect_anim_source: RefCell::new(None),
            connect_anim_step: Cell::new(0),
            updating_toggle: Cell::new(false),
            wifi_switch_row: RefCell::new(None),
            wifi_label: RefCell::new(None),
            wifi_switch: RefCell::new(None),
            ethernet_row: RefCell::new(None),
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
    pub scan_button: Rc<ScanButton>,
    pub wifi_switch: Switch,
}

/// Build the Wi-Fi details section with scan button, network list, and
/// inline password prompt.
pub fn build_wifi_details(
    state: &Rc<WifiCardState>,
    window: WeakRef<ApplicationWindow>,
) -> WifiDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    // Get current network state for initial values
    let snapshot = NetworkService::global().snapshot();

    // Ethernet row (above Wi-Fi controls, shown only when connected)
    let ethernet_row = build_ethernet_row(&snapshot);
    container.append(&ethernet_row);

    // Store ethernet row reference for dynamic updates
    *state.ethernet_row.borrow_mut() = Some(ethernet_row);

    // Wi-Fi switch row: "Wi-Fi" label + switch + scan button
    // The label+switch are only visible when ethernet device present, but scan button always visible
    let wifi_switch_row = GtkBox::new(Orientation::Horizontal, 8);
    wifi_switch_row.add_css_class(qs::WIFI_SWITCH_ROW);
    // Disable baseline alignment to prevent GTK baseline issues with Switch widget
    wifi_switch_row.set_baseline_position(gtk4::BaselinePosition::Center);

    // Wi-Fi label + switch (only visible when ethernet device present)
    let wifi_label = Label::new(Some("Wi-Fi"));
    wifi_label.add_css_class(color::PRIMARY);
    wifi_label.add_css_class(qs::WIFI_SWITCH_LABEL);
    wifi_label.set_valign(gtk4::Align::Center);
    wifi_label.set_visible(snapshot.has_ethernet_device);
    wifi_switch_row.append(&wifi_label);

    let wifi_switch = Switch::new();
    wifi_switch.set_valign(gtk4::Align::Center);
    wifi_switch.set_active(snapshot.wifi_enabled.unwrap_or(false));
    wifi_switch.set_visible(snapshot.has_ethernet_device);
    wifi_switch_row.append(&wifi_switch);

    // Spacer to push scan button to the right
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    wifi_switch_row.append(&spacer);

    // Scan button (always visible)
    let scan_button = ScanButton::new(|| {
        NetworkService::global().scan_networks();
    });
    wifi_switch_row.append(scan_button.widget());

    container.append(&wifi_switch_row);

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
    btn_cancel.add_css_class(button::CARD);
    let btn_ok = Button::with_label("Connect");
    btn_ok.add_css_class(button::ACCENT);

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

    // Store switch references
    *state.wifi_switch_row.borrow_mut() = Some(wifi_switch_row);
    *state.wifi_label.borrow_mut() = Some(wifi_label);
    *state.wifi_switch.borrow_mut() = Some(wifi_switch.clone());

    // Populate with current network state
    populate_wifi_list(state, &list_box, &snapshot);

    WifiDetailsResult {
        container,
        list_box,
        scan_button,
        wifi_switch,
    }
}

/// Add "No network connections" empty state with icon.
fn add_no_connections_state(list_box: &ListBox) {
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 8);
    container.add_css_class(qs::NO_CONNECTIONS_STATE);
    container.set_valign(gtk4::Align::Center);
    container.set_halign(gtk4::Align::Center);
    container.set_hexpand(true);

    // Icon - use IconsService for proper Material icon mapping
    // GTK: network-offline-symbolic, Material: settings_ethernet (grayed out)
    let icon_handle = icons.create_icon(
        "network-offline-symbolic",
        &[qs::NO_CONNECTIONS_ICON, color::MUTED],
    );
    let icon_widget = icon_handle.widget();
    icon_widget.set_halign(gtk4::Align::Center);
    container.append(&icon_widget);

    // Message - centered like notifications empty state
    let label = Label::new(Some("No network connections"));
    label.add_css_class(qs::NO_CONNECTIONS_LABEL);
    label.add_css_class(color::MUTED);
    label.set_halign(gtk4::Align::Center);
    label.set_justify(gtk4::Justification::Center);
    container.append(&label);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.set_activatable(false);
    list_box.append(&row);
}

fn add_wifi_disabled_placeholder(list_box: &ListBox) {
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 6);
    container.add_css_class(qs::WIFI_DISABLED_STATE);
    container.set_valign(gtk4::Align::Center);
    container.set_halign(gtk4::Align::Center);
    container.set_hexpand(true);

    // Icon - disabled Wi-Fi icon, grayed out
    let icon_handle = icons.create_icon(
        "network-wireless-offline-symbolic",
        &[qs::WIFI_DISABLED_STATE_ICON, color::MUTED],
    );
    let icon_widget = icon_handle.widget();
    icon_widget.set_halign(gtk4::Align::Center);
    container.append(&icon_widget);

    // Message
    let label = Label::new(Some("Wi-Fi is disabled"));
    label.add_css_class(qs::WIFI_DISABLED_LABEL);
    label.add_css_class(color::MUTED);
    label.set_halign(gtk4::Align::Center);
    label.set_justify(gtk4::Justification::Center);
    container.append(&label);

    let row = ListBoxRow::new();
    row.set_child(Some(&container));
    row.set_activatable(false);
    list_box.append(&row);
}

/// Build a standalone Ethernet section widget (not in a ListBox).
/// Includes a header label and connection details row.
/// Returns a GtkBox that can be shown/hidden based on connection state.
fn build_ethernet_row(snapshot: &NetworkSnapshot) -> GtkBox {
    let icons = IconsService::global();

    // Main container for the entire Ethernet section
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(qs::ETHERNET_ROW_CONTAINER);

    // Header row with "Ethernet" label (matches Wi-Fi header style)
    let header_row = GtkBox::new(Orientation::Horizontal, 8);
    header_row.add_css_class(qs::WIFI_SWITCH_ROW);

    let header_label = Label::new(Some("Ethernet"));
    header_label.add_css_class(color::PRIMARY);
    header_label.add_css_class(qs::WIFI_SWITCH_LABEL);
    header_label.set_valign(gtk4::Align::Center);
    header_row.append(&header_label);

    container.append(&header_row);

    // Create ethernet icon with accent color (always connected when shown)
    let icon_handle = icons.create_icon(
        "network-wired-symbolic",
        &[icon::TEXT, row::QS_ICON, color::ACCENT],
    );

    // Get connection name for title, fallback to interface name, then generic
    let title = snapshot
        .wired_name
        .as_deref()
        .or(snapshot.wired_iface.as_deref())
        .unwrap_or("Wired Connection");

    // Build subtitle extra parts: interface name, speed
    let mut extra_parts: Vec<String> = Vec::new();
    if let Some(ref iface) = snapshot.wired_iface {
        extra_parts.push(iface.clone());
    }
    if let Some(speed) = snapshot.wired_speed {
        if speed >= 1000 {
            let gbps = speed as f64 / 1000.0;
            if gbps.fract() == 0.0 {
                extra_parts.push(format!("{} Gbps", speed / 1000));
            } else {
                extra_parts.push(format!("{:.1} Gbps", gbps));
            }
        } else {
            extra_parts.push(format!("{} Mbps", speed));
        }
    }

    // Build connected subtitle widget with accent "Connected" and muted extra parts
    let extra_refs: Vec<&str> = extra_parts.iter().map(|s| s.as_str()).collect();
    let subtitle_widget = build_accent_subtitle("Connected", &extra_refs);

    // Connection details row with connection name as title
    let row_result = ListRow::builder()
        .title(title)
        .subtitle_widget(subtitle_widget.upcast())
        .leading_widget(icon_handle.widget())
        .css_class(qs::WIFI_ROW)
        .build();

    // Connection row container with background styling
    let connection_row = GtkBox::new(Orientation::Vertical, 0);
    connection_row.add_css_class(row::QS);
    connection_row.add_css_class(qs::ETHERNET_CONNECTION_ROW);

    // Extract the row's child and put it in our container
    if let Some(child) = row_result.row.child() {
        row_result.row.set_child(None::<&gtk4::Widget>);
        connection_row.append(&child);
    }

    container.append(&connection_row);

    // Initially visible only if wired is connected
    container.set_visible(snapshot.wired_connected);

    container
}

/// Update the Ethernet row visibility and content based on connection state.
pub fn update_ethernet_row(state: &WifiCardState, snapshot: &NetworkSnapshot) {
    if let Some(ethernet_row) = state.ethernet_row.borrow().as_ref() {
        ethernet_row.set_visible(snapshot.wired_connected);

        // If connected and row is visible, we might want to update the subtitle
        // For now, the subtitle is static after creation. If we need dynamic updates,
        // we'd need to store subtitle label reference and update it here.
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

    // Check if Wi-Fi is disabled (or no Wi-Fi device exists)
    let wifi_enabled = snapshot.wifi_enabled.unwrap_or(false);
    let has_wifi = snapshot.has_wifi_device;

    if !wifi_enabled || !has_wifi {
        // Wi-Fi is off or unavailable
        if has_wifi && !wifi_enabled {
            // Device has Wi-Fi but it's disabled - show "Wi-Fi is disabled"
            add_wifi_disabled_placeholder(list_box);
        } else if !snapshot.wired_connected {
            // No Wi-Fi device and no Ethernet - show "No network connections"
            add_no_connections_state(list_box);
        }
        // If no Wi-Fi device but Ethernet is connected, nothing to show in Wi-Fi list
        return;
    }

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

        // Build subtitle parts (excluding "Connected" which gets special treatment)
        let mut extra_parts: Vec<String> = Vec::new();
        if is_connecting {
            extra_parts.push("Connecting...".to_string());
        }
        if net.security != "open" {
            extra_parts.push("Secured".to_string());
        }
        // Don't show "Saved" while connecting (nmcli creates profile before auth completes)
        if net.known && !is_connecting {
            extra_parts.push("Saved".to_string());
        }
        extra_parts.push(format!("{}%", net.strength));

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

        // Use accent color for active network icons
        let icon_color = if net.active {
            color::ACCENT
        } else {
            color::PRIMARY
        };

        let leading_icon: gtk4::Widget = if icons.uses_material() && needs_overlay {
            // Create base icon (full signal, dimmed)
            let base_handle = icons.create_icon(
                "network-wireless-signal-excellent-symbolic",
                &[icon::TEXT, row::QS_ICON, qs::WIFI_BASE, color::DISABLED],
            );

            // Create overlay icon (actual signal level, highlighted)
            let overlay_handle = icons.create_icon(
                strength_icon_name,
                &[icon::TEXT, row::QS_ICON, qs::WIFI_OVERLAY, icon_color],
            );

            // Stack them using Overlay
            let overlay = Overlay::new();
            overlay.set_child(Some(&base_handle.widget()));
            overlay.add_overlay(&overlay_handle.widget());
            overlay.upcast()
        } else {
            // Simple single icon for full signal or non-Material themes
            let icon_handle =
                icons.create_icon(strength_icon_name, &[icon::TEXT, row::QS_ICON, icon_color]);
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

        // Build row with either connected subtitle widget or plain text
        let mut row_builder = ListRow::builder()
            .title(&net.ssid)
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::WIFI_ROW);

        if net.active && !is_connecting {
            // Active network: accent "Connected" + muted extras
            let extra_refs: Vec<&str> = extra_parts.iter().map(|s| s.as_str()).collect();
            let subtitle_widget = build_accent_subtitle("Connected", &extra_refs);
            row_builder = row_builder.subtitle_widget(subtitle_widget.upcast());
        } else {
            // Not connected: plain subtitle
            let subtitle = extra_parts.join(" \u{2022} ");
            row_builder = row_builder.subtitle(&subtitle);
        }

        let row_result = row_builder.build();

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
                }
                // Secured, unknown networks: handled by the "Connect" button gesture
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
        action_label.connect_clicked(move |_| {
            if is_secured {
                // Secured, unknown network: show password prompt
                if let Some(qs) = current_quick_settings_window() {
                    qs.show_wifi_password_dialog(&ssid_clone);
                }
            } else {
                // Open network: connect directly without password
                let network = NetworkService::global();
                network.connect_to_ssid(&ssid_clone, None);
            }
        });
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
        style_mgr.apply_surface_styles(&panel, true);
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
    if let Some(label) = state.subtitle_label.borrow().as_ref() {
        update_network_subtitle(label, snapshot);
    }
}

/// Update the scan button UI and animate while scanning.
pub fn update_scan_ui(state: &WifiCardState, snapshot: &NetworkSnapshot) {
    let scanning = snapshot.scanning;
    let wifi_enabled = snapshot.wifi_enabled.unwrap_or(false);

    if let Some(scan_btn) = state.scan_button.borrow().as_ref() {
        scan_btn.set_visible(wifi_enabled);
        scan_btn.set_sensitive(!scanning);
        scan_btn.set_scanning(scanning);
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

    // Update Wi-Fi toggle and switch state (with signal blocking to prevent feedback loop)
    let enabled = snapshot.wifi_enabled.unwrap_or(false);
    state.updating_toggle.set(true);

    // Update card toggle
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        if toggle.is_active() != enabled {
            toggle.set_active(enabled);
        }
        // Card toggle is only sensitive on WiFi-only devices (no ethernet port)
        // When ethernet is present, users must use the switch in expanded view
        toggle.set_sensitive(snapshot.has_wifi_device && !snapshot.has_ethernet_device);
    }

    // Update Wi-Fi label and switch visibility (only show when ethernet device present)
    if let Some(wifi_label) = state.wifi_label.borrow().as_ref() {
        wifi_label.set_visible(snapshot.has_ethernet_device);
    }
    if let Some(wifi_switch) = state.wifi_switch.borrow().as_ref() {
        wifi_switch.set_visible(snapshot.has_ethernet_device);
        if wifi_switch.is_active() != enabled {
            wifi_switch.set_active(enabled);
        }
        // Switch should only be sensitive if Wi-Fi device exists
        wifi_switch.set_sensitive(snapshot.has_wifi_device);
    }

    state.updating_toggle.set(false);

    // Update card title based on whether ethernet device exists
    if let Some(title_label) = state.title_label.borrow().as_ref() {
        let expected_title = if snapshot.has_ethernet_device {
            "Network"
        } else {
            "Wi-Fi"
        };
        if title_label.label() != expected_title {
            title_label.set_label(expected_title);
        }
    }

    // Update Wi-Fi card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let enabled = snapshot.wifi_enabled.unwrap_or(false);
        let icon_name = wifi_icon_name(
            snapshot.connected,
            enabled,
            snapshot.wired_connected,
            snapshot.has_wifi_device,
        );
        icon_handle.set_icon(icon_name);

        let icon_active = (enabled && snapshot.connected) || snapshot.wired_connected;
        set_icon_active(icon_handle, icon_active);

        // Additional disabled styling for Wi-Fi
        if !enabled && !snapshot.wired_connected {
            icon_handle.add_css_class(qs::WIFI_DISABLED_ICON);
        } else {
            icon_handle.remove_css_class(qs::WIFI_DISABLED_ICON);
        }
    }

    // Update Wi-Fi subtitle
    update_subtitle(state, snapshot);

    // Update Ethernet row visibility
    update_ethernet_row(state, snapshot);

    // Update scan button UI (label + animation)
    update_scan_ui(state, snapshot);

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
            wifi_icon_name(true, true, false, true),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_disconnected() {
        assert_eq!(
            wifi_icon_name(false, true, false, true),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_disabled() {
        assert_eq!(
            wifi_icon_name(true, false, false, true),
            "network-wireless-offline-symbolic"
        );
        assert_eq!(
            wifi_icon_name(false, false, false, true),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_wired_connected() {
        // Wired connected takes precedence regardless of Wi-Fi state
        assert_eq!(
            wifi_icon_name(false, false, true, true),
            "network-wired-symbolic"
        );
        assert_eq!(
            wifi_icon_name(true, true, true, true),
            "network-wired-symbolic"
        );
        assert_eq!(
            wifi_icon_name(false, false, true, false),
            "network-wired-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_name_ethernet_only_disconnected() {
        // Ethernet-only system (no Wi-Fi device), not connected - shows lan icon (grayed)
        assert_eq!(
            wifi_icon_name(false, false, false, false),
            "network-wired-symbolic"
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

    // Helper to create a base snapshot for testing
    fn test_snapshot() -> NetworkSnapshot {
        NetworkSnapshot {
            available: true,
            wifi_enabled: Some(true),
            connected: false,
            wired_connected: false,
            has_wifi_device: true,
            has_ethernet_device: false,
            primary_connection_type: None,
            wired_iface: None,
            wired_name: None,
            wired_speed: None,
            ssid: None,
            strength: 0,
            scanning: false,
            is_ready: true,
            networks: Vec::new(),
            connecting_ssid: None,
            failed_ssid: None,
        }
    }

    // Tests for get_network_subtitle_text()

    #[test]
    fn test_subtitle_wired_only() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        assert_eq!(get_network_subtitle_text(&snapshot), "Ethernet");
    }

    #[test]
    fn test_subtitle_wired_and_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        snapshot.ssid = Some("MyNetwork".to_string());
        assert_eq!(
            get_network_subtitle_text(&snapshot),
            "Ethernet \u{2022} MyNetwork"
        );
    }

    #[test]
    fn test_subtitle_wired_and_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        snapshot.connecting_ssid = Some("MyNetwork".to_string());
        assert_eq!(
            get_network_subtitle_text(&snapshot),
            "Ethernet \u{2022} Connecting to MyNetwork"
        );
    }

    #[test]
    fn test_subtitle_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.ssid = Some("HomeWifi".to_string());
        assert_eq!(get_network_subtitle_text(&snapshot), "HomeWifi");
    }

    #[test]
    fn test_subtitle_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.connecting_ssid = Some("HomeWifi".to_string());
        assert_eq!(
            get_network_subtitle_text(&snapshot),
            "Connecting to HomeWifi"
        );
    }

    #[test]
    fn test_subtitle_wifi_disconnected() {
        let snapshot = test_snapshot();
        assert_eq!(get_network_subtitle_text(&snapshot), "Disconnected");
    }

    #[test]
    fn test_subtitle_wifi_disabled() {
        let mut snapshot = test_snapshot();
        snapshot.wifi_enabled = Some(false);
        assert_eq!(get_network_subtitle_text(&snapshot), "Off");
    }

    #[test]
    fn test_subtitle_ethernet_only_system_disconnected() {
        let mut snapshot = test_snapshot();
        snapshot.has_wifi_device = false;
        snapshot.has_ethernet_device = true;
        snapshot.wifi_enabled = None;
        assert_eq!(get_network_subtitle_text(&snapshot), "Disconnected");
    }

    // Tests for is_network_subtitle_active()

    #[test]
    fn test_subtitle_active_when_wired_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        assert!(is_network_subtitle_active(&snapshot));
    }

    #[test]
    fn test_subtitle_active_when_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.ssid = Some("Network".to_string());
        assert!(is_network_subtitle_active(&snapshot));
    }

    #[test]
    fn test_subtitle_active_when_both_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        snapshot.ssid = Some("Network".to_string());
        assert!(is_network_subtitle_active(&snapshot));
    }

    #[test]
    fn test_subtitle_not_active_when_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.connecting_ssid = Some("Network".to_string());
        assert!(!is_network_subtitle_active(&snapshot));
    }

    #[test]
    fn test_subtitle_not_active_when_disconnected() {
        let snapshot = test_snapshot();
        assert!(!is_network_subtitle_active(&snapshot));
    }

    #[test]
    fn test_subtitle_not_active_wired_but_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wired_connected = true;
        snapshot.connecting_ssid = Some("Network".to_string());
        // Even though wired is connected, we're in a "connecting" state for Wi-Fi
        // so subtitle should not be fully active (shows connecting animation)
        assert!(!is_network_subtitle_active(&snapshot));
    }
}
