//! Network card for Quick Settings panel.
//!
//! This module contains:
//! - Network icon helpers (Wi-Fi, cellular, wired)
//! - Wi-Fi details panel and network list population
//! - Ethernet status display
//! - Mobile/cellular row with ModemManager integration
//! - Password dialog handling

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib::{self, WeakRef};
use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box as GtkBox, Button, Entry, Label, ListBox, ListBoxRow, Orientation,
    Overlay, Popover, ScrolledWindow, Switch,
};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, ScanButton, add_disabled_placeholder, add_placeholder_row,
    build_accent_subtitle, build_error_subtitle, clear_list_box, create_qs_list_box,
    create_row_action_label, create_row_menu_action, create_row_menu_button, set_icon_active,
};
use super::window::current_quick_settings_window;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::network::{
    AUTH_FAILURE_REASON, CONNECTION_FAILURE_REASON, NetworkConnectionState, NetworkService,
    NetworkSnapshot, WifiNetwork,
};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, icon, qs, row, state, surface};
use crate::widgets::base::configure_popover;

/// Snapshot of network state used to resolve bar and card icons.
pub struct NetworkIconContext {
    pub available: bool,
    pub connected: bool,
    pub wifi_enabled: bool,
    pub wired_connected: bool,
    pub has_wifi_device: bool,
    pub mobile_is_primary: bool,
    pub has_modem_device: bool,
    pub mobile_signal_quality: Option<u32>,
}

impl NetworkIconContext {
    /// Build a full icon context from a [`NetworkSnapshot`].
    pub fn from_snapshot(snapshot: &NetworkSnapshot) -> Self {
        Self {
            available: snapshot.available(),
            connected: snapshot.connected(),
            wifi_enabled: snapshot.wifi_enabled().unwrap_or(false),
            wired_connected: snapshot.wired_connected(),
            has_wifi_device: snapshot.has_wifi_device(),
            mobile_is_primary: snapshot.mobile_is_primary(),
            has_modem_device: snapshot.has_modem_device(),
            mobile_signal_quality: snapshot.mobile_signal_quality(),
        }
    }

    /// Build an icon context for the bar widget (excludes mobile — bar has
    /// a separate cellular icon). Mobile fields are zeroed so
    /// `network_icon_name()` produces only Wi-Fi/wired icons; the bar renders
    /// cellular status independently via `cellular_signal_icon_name()`.
    pub fn for_bar(snapshot: &NetworkSnapshot) -> Self {
        Self {
            available: snapshot.available(),
            connected: snapshot.connected(),
            wifi_enabled: snapshot.wifi_enabled().unwrap_or(false),
            wired_connected: snapshot.wired_connected(),
            has_wifi_device: snapshot.has_wifi_device(),
            mobile_is_primary: false,
            has_modem_device: false,
            mobile_signal_quality: None,
        }
    }
}

/// Pick the card icon based on connection state.
/// Precedence: unavailable → wired → mobile-primary → no-wifi fallback → wifi.
pub fn network_icon_name(ctx: &NetworkIconContext) -> &'static str {
    // Service unavailable - show offline icon regardless of device type
    if !ctx.available {
        return "network-wireless-offline-symbolic";
    }

    if ctx.wired_connected {
        "network-wired-symbolic"
    } else if ctx.mobile_is_primary {
        cellular_signal_icon_name(ctx.mobile_signal_quality.unwrap_or(0))
    } else if !ctx.has_wifi_device {
        if ctx.has_modem_device {
            "network-cellular-signal-none-symbolic"
        } else {
            // Ethernet-only system, not connected - show lan icon (will be grayed out)
            "network-wired-symbolic"
        }
    } else if !ctx.wifi_enabled {
        "network-wireless-offline-symbolic"
    } else if ctx.connected {
        "network-wireless-signal-excellent-symbolic"
    } else {
        "network-wireless-offline-symbolic"
    }
}

/// Return a Wi-Fi icon name based on signal strength percentage.
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

/// Map cellular signal quality (0–100%) to a symbolic icon name.
///
/// Thresholds (75/55/35/15) are higher than Wi-Fi (70/60/40/20) because
/// ModemManager reports a coarser, already-processed percentage where
/// values below ~15% typically mean no usable signal, whereas Wi-Fi
/// RSSI-derived percentages spread more evenly across the range.
pub fn cellular_signal_icon_name(quality: u32) -> &'static str {
    if quality >= 75 {
        "network-cellular-signal-excellent-symbolic"
    } else if quality >= 55 {
        "network-cellular-signal-good-symbolic"
    } else if quality >= 35 {
        "network-cellular-signal-ok-symbolic"
    } else if quality >= 15 {
        "network-cellular-signal-weak-symbolic"
    } else {
        "network-cellular-signal-none-symbolic"
    }
}

/// Returns the appropriate cellular icon name based on modem state.
pub fn mobile_state_icon_name(enabled: bool, active: bool, signal_quality: u32) -> &'static str {
    if !enabled {
        "network-cellular-offline-symbolic"
    } else if active {
        cellular_signal_icon_name(signal_quality)
    } else {
        "network-cellular-signal-none-symbolic"
    }
}

/// Check if Material unified icon mode is active for this snapshot.
///
/// Returns `true` when Material icons are enabled and the device has a modem,
/// meaning the network icon should use unified cellular/wifi glyphs.
pub fn is_material_unified(snapshot: &NetworkSnapshot) -> bool {
    snapshot.mobile_supported() && IconsService::global().uses_material()
}

/// Resolve the network icon for Material unified mode.
///
/// When Material icons are active and a modem is present, this picks the
/// appropriate unified icon: combined (cell_wifi), cellular-only, or
/// wifi/wired. Falls back to the standard [`network_icon_name`] when
/// Material unified mode is inactive.
pub fn resolve_material_network_icon(snapshot: &NetworkSnapshot) -> &'static str {
    let material_unified = is_material_unified(snapshot);
    let wifi_or_wired = snapshot.connected() || snapshot.wired_connected();

    if material_unified && snapshot.mobile_active() && wifi_or_wired {
        "network-wifi-cellular-symbolic"
    } else if material_unified && snapshot.mobile_active() {
        let quality = snapshot.mobile_signal_quality().unwrap_or(0);
        cellular_signal_icon_name(quality)
    } else {
        let ctx = NetworkIconContext::from_snapshot(snapshot);
        let icon_name = network_icon_name(&ctx);
        // In Material mode, use the regular wifi shape for disabled state —
        // the WIFI_DISABLED_ICON CSS class dims the color instead.
        if material_unified && icon_name == "network-wireless-offline-symbolic" {
            "network-wireless-signal-excellent-symbolic"
        } else {
            icon_name
        }
    }
}

/// Result of building the network card subtitle widget.
pub struct NetworkSubtitleResult {
    pub container: GtkBox,
    pub label: Label,
}

/// Build the subtitle widget for the network card.
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

    update_network_subtitle(&label, snapshot);

    NetworkSubtitleResult { container, label }
}

/// Generate the subtitle text for the network card based on connection state.
pub fn get_network_subtitle_text(snapshot: &NetworkSnapshot) -> String {
    if !snapshot.available() {
        return "Unavailable".to_string();
    }

    let wifi_enabled = snapshot.wifi_enabled().unwrap_or(false);
    let is_connecting = snapshot.connection_state() == NetworkConnectionState::Connecting;
    let mobile_active = snapshot.mobile_active();
    let mobile_connecting = snapshot.mobile_connecting();
    let carrier = snapshot.mobile_display_name();

    // Wired connected — may also have wifi and/or cellular
    if snapshot.wired_connected() {
        let mut parts = vec!["Ethernet".to_string()];
        if is_connecting {
            if let Some(ssid) = snapshot.active_ssid() {
                parts.push(format!("Connecting to {}", ssid));
            }
        } else if let Some(ssid) = snapshot.active_ssid() {
            parts.push(ssid.to_string());
        }
        if mobile_active {
            parts.push(carrier.to_string());
        }
        return parts.join(" \u{2022} ");
    }

    // Mobile active (has activated connection) — may also have wifi
    if mobile_active {
        if is_connecting {
            if let Some(ssid) = snapshot.active_ssid() {
                return format!("{} \u{2022} Connecting to {}", carrier, ssid);
            }
        } else if let Some(ssid) = snapshot.active_ssid() {
            return format!("{} \u{2022} {}", ssid, carrier);
        }
        return carrier.to_string();
    }

    // Wi-Fi only (no wired, no cellular active)
    if is_connecting {
        return if let Some(ssid) = snapshot.active_ssid() {
            format!("Connecting to {}", ssid)
        } else {
            "Connecting...".to_string()
        };
    }

    if let Some(ssid) = snapshot.active_ssid() {
        return ssid.to_string();
    }

    // Mobile connecting (not yet active)
    if mobile_connecting {
        return format!("{} \u{2022} Connecting...", carrier);
    }

    // Fallback disconnected/off states
    if !snapshot.has_wifi_device() || wifi_enabled {
        "Disconnected".to_string()
    } else {
        "Off".to_string()
    }
}

/// Whether the network subtitle should be styled as "active" (connected, not connecting).
pub fn is_network_subtitle_active(snapshot: &NetworkSnapshot) -> bool {
    let state = snapshot.connection_state();
    let is_connecting = state == NetworkConnectionState::Connecting;
    let any_connected = snapshot.wired_connected()
        || snapshot.mobile_active()
        || state == NetworkConnectionState::Connected;

    // If only mobile is connecting (no other connection active), not active.
    if !any_connected && snapshot.mobile_connecting() {
        return false;
    }

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

/// Cached widget references for the Ethernet row in the expanded details.
pub struct EthernetRowWidgets {
    pub container: GtkBox,
    pub title_label: Label,
    pub subtitle_box: GtkBox,
    /// Cached key for the subtitle content to skip redundant rebuilds.
    pub subtitle_key: RefCell<String>,
}

/// Cached widget references for the mobile/cellular row.
/// Grouped in a single `RefCell<Option<…>>` (in [`MobileRowState`]) to avoid per-field borrow overhead.
pub struct MobileRowWidgets {
    pub switch: Switch,
    pub action_button: Button,
    pub status_label: Label,
    pub accent_label: Label,
    pub details_label: Label,
    pub icon_handle: IconHandle,
    pub connection_row: GtkBox,
    pub title_label: Label,
}

/// State for the mobile/cellular row in the expanded details.
pub struct MobileRowState {
    pub row: RefCell<Option<GtkBox>>,
    pub widgets: RefCell<Option<MobileRowWidgets>>,
}

impl MobileRowState {
    fn new() -> Self {
        Self {
            row: RefCell::new(None),
            widgets: RefCell::new(None),
        }
    }
}

/// State for the network card in the Quick Settings panel.
pub struct NetworkCardState {
    pub base: ExpandableCardBase,
    pub title_label: RefCell<Option<Label>>,
    pub subtitle_label: RefCell<Option<Label>>,
    pub scan_button: RefCell<Option<Rc<ScanButton>>>,
    pub password_box: RefCell<Option<GtkBox>>,
    pub password_label: RefCell<Option<Label>>,
    pub password_error_label: RefCell<Option<Label>>,
    pub password_entry: RefCell<Option<Entry>>,
    pub password_cancel_button: RefCell<Option<Button>>,
    pub password_connect_button: RefCell<Option<Button>>,
    pub password_target_ssid: RefCell<Option<String>>,
    pub connect_anim_source: RefCell<Option<glib::SourceId>>,
    pub connect_anim_step: Cell<u8>,
    /// Prevents `state_set` handlers from dispatching during programmatic updates.
    pub updating_wifi_toggle: Cell<bool>,
    /// Same purpose as `updating_wifi_toggle` but scoped to the mobile switch.
    pub updating_mobile_switch: Cell<bool>,
    pub wifi_switch_row: RefCell<Option<GtkBox>>,
    pub wifi_label: RefCell<Option<Label>>,
    pub wifi_switch: RefCell<Option<Switch>>,
    pub ethernet: RefCell<Option<EthernetRowWidgets>>,
    pub mobile: MobileRowState,
    /// Prevents timer accumulation under rapid state transitions.
    pub wifi_failed_clear_source: RefCell<Option<glib::SourceId>>,
    /// Separate from Wi-Fi so simultaneous failures are cleared independently.
    pub mobile_failed_clear_source: RefCell<Option<glib::SourceId>>,
    /// Keeps the connecting WiFi row's `IconHandle` alive so the spinner
    /// timer isn't killed when the handle goes out of scope in `populate_wifi_list`.
    /// Without this, the `Rc<IconHandleInner>` drops at end of the loop body,
    /// which drops the `CairoSpinner`, which cancels the animation timer — leaving
    /// the spinner DrawingArea visible but frozen.
    pub wifi_connecting_icon: RefCell<Option<IconHandle>>,
}

impl NetworkCardState {
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
            updating_wifi_toggle: Cell::new(false),
            updating_mobile_switch: Cell::new(false),
            wifi_switch_row: RefCell::new(None),
            wifi_label: RefCell::new(None),
            wifi_switch: RefCell::new(None),
            ethernet: RefCell::new(None),
            mobile: MobileRowState::new(),
            wifi_failed_clear_source: RefCell::new(None),
            mobile_failed_clear_source: RefCell::new(None),
            wifi_connecting_icon: RefCell::new(None),
        }
    }
}

impl Default for NetworkCardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for NetworkCardState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

impl Drop for NetworkCardState {
    fn drop(&mut self) {
        // Cancel any active connect animation timer
        if let Some(source_id) = self.connect_anim_source.borrow_mut().take() {
            source_id.remove();
            debug!("NetworkCardState: connect animation timer cancelled on drop");
        }
        // Cancel any pending failed-state clear timers
        if let Some(source_id) = self.wifi_failed_clear_source.borrow_mut().take() {
            source_id.remove();
        }
        if let Some(source_id) = self.mobile_failed_clear_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

/// Result of building Wi-Fi details section.
pub struct NetworkDetailsResult {
    pub container: GtkBox,
    pub list_box: ListBox,
    pub scan_button: Rc<ScanButton>,
    pub wifi_switch: Switch,
}

/// Build the Wi-Fi details section with scan button, network list, and
/// inline password prompt.
pub fn build_wifi_details(
    state: &Rc<NetworkCardState>,
    window: WeakRef<ApplicationWindow>,
) -> NetworkDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    let snapshot = NetworkService::global().snapshot();

    // Ethernet row (shown only when connected)
    let ethernet_widgets = build_ethernet_row(&snapshot);
    container.append(&ethernet_widgets.container);

    // Store ethernet row reference for dynamic updates
    *state.ethernet.borrow_mut() = Some(ethernet_widgets);

    // Mobile row (above Wi-Fi controls, shown when mobile is supported)
    let mobile_row = build_mobile_row(state, &snapshot);
    container.append(&mobile_row);

    // Store mobile row reference for dynamic updates
    *state.mobile.row.borrow_mut() = Some(mobile_row);

    // Wi-Fi switch row: "Wi-Fi" label + switch + scan button
    // The label+switch are only visible when a non-WiFi device is present, but scan button always visible
    let wifi_switch_row = GtkBox::new(Orientation::Horizontal, 8);
    wifi_switch_row.add_css_class(qs::NETWORK_SECTION_ROW);
    // Disable baseline alignment to prevent GTK baseline issues with Switch widget
    wifi_switch_row.set_baseline_position(gtk4::BaselinePosition::Center);

    // Wi-Fi label + switch (only visible when a non-WiFi device is present)
    let wifi_label = Label::new(Some("Wi-Fi"));
    wifi_label.add_css_class(color::PRIMARY);
    wifi_label.add_css_class(qs::NETWORK_SECTION_LABEL);
    wifi_label.set_valign(gtk4::Align::Center);
    wifi_label.set_visible(snapshot.has_non_wifi_device());
    wifi_switch_row.append(&wifi_label);

    let wifi_switch = Switch::new();
    wifi_switch.set_valign(gtk4::Align::Center);
    wifi_switch.set_active(snapshot.wifi_enabled().unwrap_or(false));
    wifi_switch.set_visible(snapshot.has_non_wifi_device());
    wifi_switch_row.append(&wifi_switch);

    // Spacer to push scan button to the right
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    wifi_switch_row.append(&spacer);

    // Scan button (always visible)
    let scan_button = ScanButton::new(|| {
        NetworkService::global().scan();
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

    NetworkDetailsResult {
        container,
        list_box,
        scan_button,
        wifi_switch,
    }
}

/// Build a standalone Ethernet section widget.
fn build_ethernet_row(snapshot: &NetworkSnapshot) -> EthernetRowWidgets {
    let icons = IconsService::global();

    // Main container for the entire Ethernet section
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(qs::NETWORK_SECTION);

    // Header row with "Ethernet" label (matches Wi-Fi header style)
    let header_row = GtkBox::new(Orientation::Horizontal, 8);
    header_row.add_css_class(qs::NETWORK_SECTION_ROW);

    let header_label = Label::new(Some("Ethernet"));
    header_label.add_css_class(color::PRIMARY);
    header_label.add_css_class(qs::NETWORK_SECTION_LABEL);
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
        .wired_name()
        .or(snapshot.wired_iface())
        .unwrap_or("Wired Connection");

    // Build subtitle with connection details
    let subtitle_box = build_ethernet_subtitle(snapshot);

    // Connection details row with connection name as title
    let row_result = ListRow::builder()
        .title(title)
        .subtitle_widget(subtitle_box.clone().upcast())
        .leading_widget(icon_handle.widget())
        .css_class(qs::NETWORK_ROW)
        .build();

    // Connection row container with background styling
    let connection_row = GtkBox::new(Orientation::Vertical, 0);
    connection_row.add_css_class(row::QS);
    connection_row.add_css_class(qs::NETWORK_CONNECTION_ROW);

    // Extract the row's child and put it in our container
    if let Some(child) = row_result.row.child() {
        row_result.row.set_child(None::<&gtk4::Widget>);
        connection_row.append(&child);
    }

    container.append(&connection_row);

    // Initially visible only if wired is connected
    container.set_visible(snapshot.wired_connected());

    EthernetRowWidgets {
        container,
        title_label: row_result.title,
        subtitle_key: RefCell::new(ethernet_subtitle_key(snapshot)),
        subtitle_box,
    }
}

/// Build the Ethernet subtitle widget (accent "Connected" + muted details).
fn build_ethernet_subtitle(snapshot: &NetworkSnapshot) -> GtkBox {
    let mut extra_parts: Vec<String> = Vec::new();
    if let Some(ref iface) = snapshot.wired_iface() {
        extra_parts.push(iface.to_string());
    }
    if let Some(speed) = snapshot.wired_speed() {
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

    let extra_refs: Vec<&str> = extra_parts.iter().map(|s| s.as_str()).collect();
    build_accent_subtitle("Connected", &extra_refs)
}

/// Build a comparable key from the ethernet subtitle inputs (interface name + speed).
/// Used to skip redundant subtitle rebuilds when the data hasn't changed.
fn ethernet_subtitle_key(snapshot: &NetworkSnapshot) -> String {
    let iface = snapshot.wired_iface().unwrap_or("");
    let speed = snapshot.wired_speed().unwrap_or(0);
    format!("{iface}:{speed}")
}

/// Update the mobile subtitle labels for the current state.
fn set_mobile_subtitle(widgets: &MobileRowWidgets, snapshot: &NetworkSnapshot) {
    let mobile_enabled = snapshot.mobile_enabled().unwrap_or(false);

    let status_label = &widgets.status_label;
    let accent_label = &widgets.accent_label;
    let details_label = &widgets.details_label;

    if !mobile_enabled {
        status_label.set_text("Off");
        status_label.remove_css_class(color::ERROR);
        status_label.add_css_class(color::MUTED);
        status_label.set_visible(true);
        accent_label.set_visible(false);
        details_label.set_visible(false);
    } else if snapshot.mobile_connecting() {
        status_label.set_text("Connecting...");
        status_label.set_visible(true);
        status_label.remove_css_class(color::ERROR);
        status_label.add_css_class(color::MUTED);
        accent_label.set_visible(false);
        details_label.set_visible(false);
    } else if snapshot.mobile_active() {
        status_label.set_visible(false);
        accent_label.set_text("Connected");
        accent_label.set_visible(true);

        let signal = snapshot.mobile_signal_quality().unwrap_or(0);
        let mut extra_parts: Vec<String> = Vec::new();
        if let Some(tech) = snapshot.mobile_access_technology()
            && !tech.is_empty()
        {
            extra_parts.push(tech.to_string());
        }
        if signal > 0 {
            extra_parts.push(format!("{}%", signal));
        }
        if extra_parts.is_empty() {
            details_label.set_visible(false);
        } else {
            let rest = format!(" \u{2022} {}", extra_parts.join(" \u{2022} "));
            details_label.set_text(&rest);
            details_label.set_visible(true);
        }
    } else if snapshot.mobile_failed() {
        status_label.set_text(CONNECTION_FAILURE_REASON);
        status_label.remove_css_class(color::MUTED);
        status_label.add_css_class(color::ERROR);
        status_label.set_visible(true);
        accent_label.set_visible(false);
        details_label.set_visible(false);
    } else {
        status_label.set_text("Disconnected");
        status_label.remove_css_class(color::ERROR);
        status_label.add_css_class(color::MUTED);
        status_label.set_visible(true);
        accent_label.set_visible(false);
        details_label.set_visible(false);
    }
}

/// Build a standalone Mobile section widget (not in a ListBox).
fn build_mobile_row(state: &Rc<NetworkCardState>, snapshot: &NetworkSnapshot) -> GtkBox {
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(qs::NETWORK_SECTION);

    let header_row = GtkBox::new(Orientation::Horizontal, 8);
    header_row.add_css_class(qs::NETWORK_SECTION_ROW);

    let header_label = Label::new(Some("Mobile"));
    header_label.add_css_class(color::PRIMARY);
    header_label.add_css_class(qs::NETWORK_SECTION_LABEL);
    header_label.set_valign(gtk4::Align::Center);
    header_row.append(&header_label);

    let mobile_switch = Switch::new();
    mobile_switch.set_valign(gtk4::Align::Center);
    let mobile_enabled = snapshot.mobile_enabled().unwrap_or(false);
    mobile_switch.set_active(mobile_enabled);
    mobile_switch.set_sensitive(snapshot.available() && snapshot.has_modem_device());
    {
        let state_weak = Rc::downgrade(state);
        // `state_set` fires for both user and programmatic changes. When
        // `updating_mobile_switch` is set, the switch is being synced to
        // match D-Bus state — return `Proceed` to accept the visual change
        // but skip the `set_mobile_enabled` call to avoid a feedback loop.
        // We return `Proceed` (not `Stop`) in both branches because GTK4
        // `state_set` expects `Proceed` to let the switch actually transition;
        // `Stop` would reject the state change entirely.
        mobile_switch.connect_state_set(move |_, is_active| {
            if let Some(state) = state_weak.upgrade()
                && state.updating_mobile_switch.get()
            {
                return glib::Propagation::Proceed;
            }
            NetworkService::global().set_mobile_enabled(is_active);
            glib::Propagation::Proceed
        });
    }
    header_row.append(&mobile_switch);

    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header_row.append(&spacer);

    container.append(&header_row);

    let signal = snapshot.mobile_signal_quality().unwrap_or(0);
    let mobile_active = snapshot.mobile_active();
    let icon_handle = icons.create_icon(
        mobile_state_icon_name(mobile_enabled, mobile_active, signal),
        &[
            icon::TEXT,
            row::QS_ICON,
            if !mobile_enabled {
                color::MUTED
            } else if mobile_active {
                color::ACCENT
            } else {
                color::PRIMARY
            },
        ],
    );

    let title = snapshot.mobile_display_name();

    let subtitle_box = GtkBox::new(Orientation::Horizontal, 0);

    let status_label = Label::new(None);
    status_label.add_css_class(color::MUTED);
    status_label.add_css_class(row::QS_SUBTITLE);
    subtitle_box.append(&status_label);

    let accent_label = Label::new(None);
    accent_label.add_css_class(color::ACCENT);
    accent_label.add_css_class(row::QS_SUBTITLE);
    subtitle_box.append(&accent_label);

    let details_label = Label::new(None);
    details_label.add_css_class(color::MUTED);
    details_label.add_css_class(row::QS_SUBTITLE);
    details_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    subtitle_box.append(&details_label);

    let action_button = create_row_action_label(if mobile_active {
        "Disconnect"
    } else {
        "Connect"
    });
    action_button.set_sensitive(!snapshot.mobile_connecting() && mobile_enabled);
    action_button.connect_clicked(move |_| {
        let service = NetworkService::global();
        let snap = service.snapshot();
        if snap.mobile_active() {
            service.disconnect_mobile();
        } else {
            service.connect_mobile();
        }
    });

    let row_result = ListRow::builder()
        .title(title)
        .subtitle_widget(subtitle_box.clone().upcast())
        .leading_widget(icon_handle.widget())
        .trailing_widget(action_button.clone().upcast())
        .css_class(qs::NETWORK_ROW)
        .build();

    let connection_row = GtkBox::new(Orientation::Vertical, 0);
    connection_row.add_css_class(row::QS);
    connection_row.add_css_class(qs::NETWORK_CONNECTION_ROW);
    connection_row.set_visible(mobile_enabled);

    if let Some(child) = row_result.row.child() {
        row_result.row.set_child(None::<&gtk4::Widget>);
        connection_row.append(&child);
    }

    container.append(&connection_row);
    container.set_visible(snapshot.mobile_supported());

    let widgets = MobileRowWidgets {
        switch: mobile_switch,
        action_button,
        status_label,
        accent_label,
        details_label,
        icon_handle,
        connection_row,
        title_label: row_result.title,
    };
    set_mobile_subtitle(&widgets, snapshot);
    *state.mobile.widgets.borrow_mut() = Some(widgets);

    container
}

/// Update the Ethernet row visibility and content.
pub fn update_ethernet_row(state: &NetworkCardState, snapshot: &NetworkSnapshot) {
    let ethernet_ref = state.ethernet.borrow();
    let Some(w) = ethernet_ref.as_ref() else {
        return;
    };

    w.container.set_visible(snapshot.wired_connected());

    if !snapshot.wired_connected() {
        return;
    }

    // Update title
    let new_title = snapshot
        .wired_name()
        .or(snapshot.wired_iface())
        .unwrap_or("Wired Connection");
    if w.title_label.text().as_str() != new_title {
        w.title_label.set_text(new_title);
    }

    // Rebuild subtitle only when the underlying data changes.
    // The subtitle is "Connected • <iface> • <speed>" — derive a key from the inputs
    // and compare against the cached value to avoid unnecessary re-renders.
    let new_key = ethernet_subtitle_key(snapshot);
    if *w.subtitle_key.borrow() != new_key {
        *w.subtitle_key.borrow_mut() = new_key;
        let new_subtitle = build_ethernet_subtitle(snapshot);
        while let Some(child) = w.subtitle_box.first_child() {
            w.subtitle_box.remove(&child);
        }
        while let Some(child) = new_subtitle.first_child() {
            new_subtitle.remove(&child);
            w.subtitle_box.append(&child);
        }
    }
}

/// Update the Mobile row visibility and content based on connection state.
pub fn update_mobile_row(state: &NetworkCardState, snapshot: &NetworkSnapshot) {
    let mobile = &state.mobile;
    let enabled = snapshot.mobile_enabled().unwrap_or(false);

    if let Some(mobile_row) = mobile.row.borrow().as_ref() {
        mobile_row.set_visible(snapshot.mobile_supported());
    }

    let mut widgets_ref = mobile.widgets.borrow_mut();
    let Some(w) = widgets_ref.as_mut() else {
        return;
    };

    // Update the row title (operator name may change, e.g., roaming).
    let new_title = snapshot.mobile_display_name();
    if w.title_label.text().as_str() != new_title {
        w.title_label.set_text(new_title);
    }

    // Hide the connection details row when modem is disabled (only header+switch remain)
    w.connection_row.set_visible(enabled);

    if w.switch.is_active() != enabled {
        w.switch.set_active(enabled);
    }
    w.switch
        .set_sensitive(snapshot.available() && snapshot.has_modem_device());

    w.action_button.set_label(if snapshot.mobile_active() {
        "Disconnect"
    } else {
        "Connect"
    });
    w.action_button
        .set_sensitive(!snapshot.mobile_connecting() && enabled);

    set_mobile_subtitle(w, snapshot);

    let signal = snapshot.mobile_signal_quality().unwrap_or(0);
    let connecting = snapshot.mobile_connecting();
    let active = snapshot.mobile_active();

    // Update icon and spinner state
    w.icon_handle
        .set_icon(mobile_state_icon_name(enabled, active, signal));
    w.icon_handle.set_spinning(connecting);

    // Update CSS color class
    let (add, remove1, remove2) = if !enabled {
        (color::MUTED, color::ACCENT, color::PRIMARY)
    } else if connecting || active {
        (color::ACCENT, color::PRIMARY, color::MUTED)
    } else {
        (color::PRIMARY, color::ACCENT, color::MUTED)
    };
    w.icon_handle.remove_css_class(remove1);
    w.icon_handle.remove_css_class(remove2);
    w.icon_handle.add_css_class(add);
}

/// Populate the Wi-Fi list with network data from snapshot.
pub fn populate_wifi_list(
    state: &NetworkCardState,
    list_box: &ListBox,
    snapshot: &NetworkSnapshot,
) {
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

    // Drop any previously-stored connecting icon handle (its row was just removed).
    *state.wifi_connecting_icon.borrow_mut() = None;

    // Check if Wi-Fi is disabled (or no Wi-Fi device exists)
    let wifi_enabled = snapshot.wifi_enabled().unwrap_or(false);
    let has_wifi = snapshot.has_wifi_device();

    if !wifi_enabled || !has_wifi {
        // Wi-Fi is off or unavailable
        if has_wifi && !wifi_enabled {
            // Device has Wi-Fi but it's disabled - show "Wi-Fi is disabled"
            add_disabled_placeholder(
                list_box,
                "network-wireless-offline-symbolic",
                "Wi-Fi is disabled",
            );
        } else if !snapshot.wired_connected() && !snapshot.mobile_active() {
            // No Wi-Fi device and no Ethernet - show "No network connections"
            add_disabled_placeholder(
                list_box,
                "network-offline-symbolic",
                "No network connections",
            );
        }
        // If no Wi-Fi device but Ethernet is connected, nothing to show in Wi-Fi list
        return;
    }

    if !snapshot.is_ready() {
        add_placeholder_row(list_box, "Scanning for networks...");
        return;
    }

    if snapshot.networks().is_empty() {
        add_placeholder_row(list_box, "No networks found");
        return;
    }

    let icons = IconsService::global();
    let target_ssid = state.password_target_ssid.borrow().clone();
    let connecting_ssid = snapshot.connecting_ssid();
    let failed_ssid = snapshot.failed_ssid();
    let failed_reason = snapshot.failed_reason();
    let mut inserted_password_row = false;

    for net in snapshot.networks() {
        // Check if this network is currently being connected to
        let is_connecting = connecting_ssid == Some(net.ssid.as_str());

        // Build subtitle parts (excluding "Connected" which gets special treatment)
        let mut extra_parts: Vec<String> = Vec::new();
        if is_connecting {
            extra_parts.push("Connecting...".to_string());
        }
        if net.security.is_secured() {
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

        let icon_color = if net.active || is_connecting {
            color::ACCENT
        } else {
            color::PRIMARY
        };

        let leading_icon: gtk4::Widget = if is_connecting {
            // Connecting: show a spinner in place of the signal icon.
            // Store the handle in state so the Rc<IconHandleInner> stays alive —
            // if it drops, the CairoSpinner timer is cancelled and the spinner freezes.
            let icon_handle =
                icons.create_icon(strength_icon_name, &[icon::TEXT, row::QS_ICON, icon_color]);
            icon_handle.set_spinning(true);
            *state.wifi_connecting_icon.borrow_mut() = Some(icon_handle.clone());
            icon_handle.widget()
        } else if icons.uses_material() && needs_overlay {
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

        // Check if this network has a non-password failure to show inline
        let is_failed = failed_ssid == Some(net.ssid.as_str());

        // Build row with either connected subtitle widget, error subtitle, or plain text
        let mut row_builder = ListRow::builder()
            .title(&net.ssid)
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::NETWORK_ROW);

        if net.active && !is_connecting {
            // Active network: accent "Connected" + muted extras
            let extra_refs: Vec<&str> = extra_parts.iter().map(|s| s.as_str()).collect();
            let subtitle_widget = build_accent_subtitle("Connected", &extra_refs);
            row_builder = row_builder.subtitle_widget(subtitle_widget.upcast());
        } else if is_failed {
            // Failed network: error-colored reason + muted extras
            let reason = failed_reason.unwrap_or(CONNECTION_FAILURE_REASON);
            let extra_refs: Vec<&str> = extra_parts.iter().map(|s| s.as_str()).collect();
            let subtitle_widget = build_error_subtitle(reason, &extra_refs);
            row_builder = row_builder.subtitle_widget(subtitle_widget.upcast());
        } else {
            // Not connected: plain subtitle
            let subtitle = extra_parts.join(" \u{2022} ");
            row_builder = row_builder.subtitle(&subtitle);
        }

        let row_result = row_builder.build();

        // Disable row activation if this network is currently connecting.
        // Only set activatable to false — setting sensitive to false would
        // dim the row and prevent the spinner DrawingArea from redrawing.
        if is_connecting {
            row_result.row.set_activatable(false);
        }

        // Connect row activation to the primary network action
        if !is_connecting {
            let ssid = net.ssid.clone();
            let security = net.security;
            let known = net.known;
            let active = net.active;
            let path = net.path.clone();
            row_result.row.connect_activate(move |_| {
                let service = NetworkService::global();
                if active {
                    service.disconnect();
                } else if !security.is_secured() || known {
                    service.connect_to_network(&ssid, None, path.as_deref());
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
        let is_secured = net.security.is_secured();
        let path = net.path.clone();
        action_label.connect_clicked(move |_| {
            let service = NetworkService::global();
            if is_secured {
                // IWD: connect first, agent callback shows password dialog.
                // NM: show password dialog first, then connect.
                let snapshot = service.snapshot();
                // Backend-specific: IWD uses connect-then-prompt (agent callback
                // shows password dialog), NM uses prompt-then-connect. These flows
                // are fundamentally different and cannot be unified.
                if matches!(snapshot, NetworkSnapshot::Iwd(_)) {
                    service.connect_to_network(&ssid_clone, None, path.as_deref());
                } else if let Some(qs) = current_quick_settings_window() {
                    qs.show_wifi_password_dialog(&ssid_clone);
                }
            } else {
                // Open network: connect directly without password
                service.connect_to_network(&ssid_clone, None, path.as_deref());
            }
        });
        return action_label.upcast();
    }

    // Known or active networks: hamburger menu with multiple actions.
    let menu_btn = create_row_menu_button();

    let is_active_clone = is_active;
    let is_known_clone = is_known;
    let ssid_for_actions = ssid.clone();
    let path_for_actions = net.path.clone();
    let path_for_forget = net.known_network_path.clone();

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
            let path_clone = path_for_actions.clone();
            let popover_weak = popover.downgrade();
            let action = create_row_menu_action("Connect", move || {
                // Close popover first to avoid "still has children" warning
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
                let network = NetworkService::global();
                debug!("wifi_connect_from_menu ssid={}", ssid_clone);
                // Known networks connect without password prompt
                network.connect_to_network(&ssid_clone, None, path_clone.as_deref());
            });
            content_box.append(&action);
        }

        // Forget action for known networks
        if is_known_clone {
            let ssid_clone = ssid_for_actions.clone();
            let path_clone = path_for_forget.clone();
            let popover_weak = popover.downgrade();
            let action = create_row_menu_action("Forget", move || {
                // Close popover first to avoid "still has children" warning
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
                let network = NetworkService::global();
                debug!("wifi_forget_from_menu ssid={}", ssid_clone);
                network.forget(&ssid_clone, path_clone.as_deref());
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
/// If `error_message` is provided, displays it as an error message.
pub fn show_password_dialog_with_error(
    state: &NetworkCardState,
    ssid: &str,
    error_message: Option<&str>,
) {
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
        if let Some(msg) = error_message {
            error_label.add_css_class(color::ERROR);
            error_label.set_label(msg);
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
pub fn show_password_dialog(state: &NetworkCardState, ssid: &str) {
    show_password_dialog_with_error(state, ssid, None);
}

/// Called when the password entry is mapped; grabs focus if we have a target.
fn on_password_entry_mapped(state: &NetworkCardState, entry: &Entry) {
    if state.password_target_ssid.borrow().is_some() {
        entry.grab_focus();
    }
}

/// Cancel the inline password prompt.
fn on_password_cancel_clicked(state: &NetworkCardState) {
    hide_password_dialog(state);

    // Cancel any pending IWD auth, abort active connection, and clear failed state
    let service = NetworkService::global();
    service.cancel_auth();
    if service.snapshot().connection_state() == NetworkConnectionState::Connecting {
        service.disconnect();
    }
    service.clear_failed_state();
}

/// Schedule a delayed clear of a failed-connection state after 5 seconds.
fn schedule_failed_clear<F: FnOnce() + 'static>(
    source: &RefCell<Option<glib::SourceId>>,
    clear_fn: F,
) {
    let mut source = source.borrow_mut();
    if let Some(prev) = source.take() {
        prev.remove();
    }
    *source = Some(glib::timeout_add_local_once(
        std::time::Duration::from_secs(5),
        clear_fn,
    ));
}

/// Hide the password dialog and reset its state.
fn hide_password_dialog(state: &NetworkCardState) {
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
fn on_password_connect_clicked(state: &NetworkCardState, window: WeakRef<ApplicationWindow>) {
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
    let snapshot = service.snapshot();

    // Check if this is an IWD auth request (agent callback pending)
    // Verify the auth request SSID matches our target to avoid submitting
    // a password for the wrong network in case of a race.
    if let Some(auth_ssid) = snapshot.auth_request_ssid() {
        if auth_ssid == ssid {
            // IWD agent pattern: submit the password to the pending D-Bus invocation
            service.submit_password(&password);
        } else {
            // SSID mismatch — the pending auth is for a different network.
            // Connect directly instead (this will trigger a new auth flow).
            debug!(
                "Auth request SSID '{}' doesn't match target '{}', connecting directly",
                auth_ssid, ssid
            );
            let path = snapshot
                .networks()
                .iter()
                .find(|n| n.ssid == ssid)
                .and_then(|n| n.path.clone());
            service.connect_to_network(&ssid, Some(&password), path.as_deref());
        }
    } else {
        // No pending IWD auth request — connect with password directly.
        // NetworkManager doesn't need a path; IWD does, so look it up from
        // the current snapshot.
        let path = snapshot
            .networks()
            .iter()
            .find(|n| n.ssid == ssid)
            .and_then(|n| n.path.clone());
        service.connect_to_network(&ssid, Some(&password), path.as_deref());
    }
}

/// Set the password dialog to connecting/idle state.
/// When `connecting` is true, `window` must be provided to start the animation.
/// When `connecting` is false, `window` can be None as we're just stopping.
fn set_password_connecting_state(
    state: &NetworkCardState,
    connecting: bool,
    window: Option<WeakRef<ApplicationWindow>>,
) {
    if let Some(entry) = state.password_entry.borrow().as_ref() {
        entry.set_sensitive(!connecting);
    }
    // Cancel button stays enabled during connect so the user can abort.
    if !connecting && let Some(btn) = state.password_cancel_button.borrow().as_ref() {
        btn.set_sensitive(true);
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
                    if let Some(window) = window.upgrade()
                        && let Some(qs) = super::window::get_qs_window_data(&window)
                        && let Some(label) = qs.network.password_error_label.borrow().as_ref()
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
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
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
pub fn update_subtitle(state: &NetworkCardState, snapshot: &NetworkSnapshot) {
    if let Some(label) = state.subtitle_label.borrow().as_ref() {
        update_network_subtitle(label, snapshot);
    }
}

/// Update the scan button UI and animate while scanning.
pub fn update_scan_ui(state: &NetworkCardState, snapshot: &NetworkSnapshot) {
    let scanning = snapshot.scanning();
    let wifi_enabled = snapshot.wifi_enabled().unwrap_or(false);

    if let Some(scan_btn) = state.scan_button.borrow().as_ref() {
        scan_btn.set_visible(wifi_enabled);
        scan_btn.set_sensitive(!scanning);
        scan_btn.set_scanning(scanning);
    }
}

/// Handle network state changes from NetworkService.
pub fn on_network_changed(
    state: &NetworkCardState,
    snapshot: &NetworkSnapshot,
    window: &ApplicationWindow,
) {
    // Backend-specific: NM provides the password upfront and tracks connection
    // failure via failed_ssid. This block handles the NM password dialog lifecycle
    // (error display, success dismiss, stale dialog cleanup). Cannot be unified with
    // the IWD block below because NM has no agent-based auth flow.
    if let NetworkSnapshot::NetworkManager(nm_snap) = snapshot {
        let current_target = state.password_target_ssid.borrow().clone();
        if let Some(ref target_ssid) = current_target {
            if let Some(ref failed_ssid) = nm_snap.wifi.failed_ssid {
                if failed_ssid == target_ssid {
                    // Connection failed for our target - show error and re-enable form
                    debug!("Connection failed for '{}', showing error", failed_ssid);
                    set_password_connecting_state(state, false, None);
                    if let Some(error_label) = state.password_error_label.borrow().as_ref() {
                        error_label.add_css_class(color::ERROR);
                        error_label.set_label(CONNECTION_FAILURE_REASON);
                    }
                    // Repopulate the network list so the previously-connected network
                    // no longer shows "Connected" (the list is normally skipped when
                    // the password dialog is visible).
                    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
                        populate_wifi_list(state, list_box, snapshot);
                        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
                    }
                    // Clear the failed state so we don't re-trigger
                    NetworkService::global().clear_failed_state();
                }
            } else if nm_snap.wifi.ssid.as_ref() == Some(target_ssid)
                && nm_snap.wifi.connecting_ssid.is_none()
            {
                // Successfully connected to target - hide dialog and clear state
                debug!(
                    "Successfully connected to '{}', hiding password dialog",
                    target_ssid
                );
                hide_password_dialog(state);
            } else if nm_snap.wifi.connected
                && nm_snap.wifi.ssid.as_ref() != Some(target_ssid)
                && nm_snap.wifi.connecting_ssid.is_none()
            {
                // Connected to a different network while password dialog was open
                // (user clicked a saved network). Hide the stale dialog.
                debug!(
                    "NM connected to '{}' while password dialog was open for '{}', hiding dialog",
                    nm_snap.wifi.ssid.as_deref().unwrap_or("?"),
                    target_ssid
                );
                hide_password_dialog(state);
            }
            // If connecting_ssid matches target, keep showing animation (do nothing)
        } else if let Some(ref failed_ssid) = nm_snap.wifi.failed_ssid {
            // NM doesn't provide failure reasons, so prompting for password is misleading.
            // Show inline error instead.
            debug!(
                "NM connection failed for '{}', showing inline error",
                failed_ssid
            );
            schedule_failed_clear(&state.wifi_failed_clear_source, || {
                NetworkService::global().clear_failed_state();
            });
        }
    }

    // Backend-specific: IWD uses agent-based auth (the daemon calls back requesting
    // the password mid-connection) and provides richer failure reasons. This block
    // handles auth request display, failure categorization (auth vs generic), and
    // retry prompts. Cannot be unified with the NM block above because the auth
    // flows are architecturally different.
    if let NetworkSnapshot::Iwd(iwd_snap) = snapshot {
        let current_target = state.password_target_ssid.borrow().clone();

        // Check for auth request (IWD is asking for password)
        if let Some(ref auth_request) = iwd_snap.auth_request
            && current_target.as_ref() != Some(&auth_request.ssid)
        {
            // New auth request - show password dialog
            if window.is_mapped() {
                debug!(
                    "IWD requesting passphrase for '{}', showing password dialog",
                    auth_request.ssid
                );
                show_password_dialog(state, &auth_request.ssid);
            } else {
                // Window not yet mapped — defer. show_panel()'s idle callback
                // re-delivers the snapshot after mapping, which re-enters here
                // with is_mapped() == true. AUTH_TIMEOUT_SECS is the backstop
                // if the panel is never opened.
                debug!(
                    "IWD requesting passphrase for '{}', but window not mapped - deferring to post-map re-check",
                    auth_request.ssid
                );
            }
        }

        // Check for failed connection
        if let Some(ref target_ssid) = current_target {
            if let Some(ref failed_ssid) = iwd_snap.failed_ssid {
                if failed_ssid == target_ssid {
                    // Connection failed for our target - show error and re-enable form
                    let reason = iwd_snap
                        .failed_reason
                        .as_deref()
                        .unwrap_or(CONNECTION_FAILURE_REASON);
                    debug!(
                        "IWD connection failed for '{}': {}, showing error",
                        failed_ssid, reason
                    );
                    set_password_connecting_state(state, false, None);
                    if let Some(error_label) = state.password_error_label.borrow().as_ref() {
                        error_label.add_css_class(color::ERROR);
                        error_label.set_label(reason);
                    }
                    // Repopulate the network list so the previously-connected network
                    // no longer shows "Connected" (the list is normally skipped when
                    // the password dialog is visible).
                    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
                        populate_wifi_list(state, list_box, snapshot);
                        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
                    }
                    // Clear the failed state so we don't re-trigger
                    NetworkService::global().clear_failed_state();
                }
            } else if iwd_snap.ssid.as_ref() == Some(target_ssid) && iwd_snap.connected() {
                // Successfully connected to target - hide dialog and clear state
                debug!(
                    "IWD successfully connected to '{}', hiding password dialog",
                    target_ssid
                );
                hide_password_dialog(state);
            } else if iwd_snap.connected() && iwd_snap.ssid.as_deref() != Some(target_ssid) {
                // Connected to a different network while password dialog was open
                // (user clicked a saved network). Hide the stale dialog.
                debug!(
                    "IWD connected to '{}' while password dialog was open for '{}', hiding dialog",
                    iwd_snap.ssid.as_deref().unwrap_or("?"),
                    target_ssid
                );
                hide_password_dialog(state);
            }
        } else if let Some(ref failed_ssid) = iwd_snap.failed_ssid {
            // Only prompt for password on auth failures. Other failures show inline error.
            let reason = iwd_snap
                .failed_reason
                .as_deref()
                .unwrap_or(CONNECTION_FAILURE_REASON);
            let is_auth_failure = reason == AUTH_FAILURE_REASON;

            if is_auth_failure && window.is_mapped() {
                debug!(
                    "IWD auth failed for '{}', showing password dialog with error",
                    failed_ssid
                );
                show_password_dialog_with_error(state, failed_ssid, Some(reason));
            } else if is_auth_failure {
                debug!(
                    "IWD auth failed for '{}', but window is closed - clearing failed state",
                    failed_ssid
                );
                NetworkService::global().clear_failed_state();
            } else {
                // Non-auth failure: show inline on network row (handled by populate_wifi_list).
                // Schedule delayed clear so the error is visible for a few seconds.
                debug!(
                    "IWD connection failed for '{}': {}, showing inline error",
                    failed_ssid, reason
                );
                schedule_failed_clear(&state.wifi_failed_clear_source, || {
                    NetworkService::global().clear_failed_state();
                });
            }
        }
    }

    // Update Wi-Fi toggle and switch state (with signal blocking to prevent feedback loop)
    let enabled = snapshot.wifi_enabled().unwrap_or(false);
    state.updating_wifi_toggle.set(true);
    state.updating_mobile_switch.set(true);

    // Update card toggle
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        if toggle.is_active() != enabled {
            toggle.set_active(enabled);
        }
        // Card toggle is only sensitive on WiFi-only devices (no ethernet port, no usable modem)
        // When ethernet or a supported modem (SIM + profile) is present, users must use the switch in expanded view
        toggle.set_sensitive(
            snapshot.available()
                && snapshot.has_wifi_device()
                && !snapshot.has_ethernet_device()
                && !snapshot.mobile_supported(),
        );
    }

    // Update Wi-Fi label and switch visibility (only show when alternative network device present)
    if let Some(wifi_label) = state.wifi_label.borrow().as_ref() {
        wifi_label.set_visible(snapshot.has_non_wifi_device());
    }
    if let Some(wifi_switch) = state.wifi_switch.borrow().as_ref() {
        wifi_switch.set_visible(snapshot.has_non_wifi_device());
        if wifi_switch.is_active() != enabled {
            wifi_switch.set_active(enabled);
        }
        // Switch should only be sensitive if Wi-Fi device exists and service is available
        wifi_switch.set_sensitive(snapshot.available() && snapshot.has_wifi_device());
    }

    // Update card title based on whether ethernet/modem device exists
    if let Some(title_label) = state.title_label.borrow().as_ref() {
        let expected_title = if snapshot.has_non_wifi_device() {
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
        // Service unavailable - use warning styling
        if !snapshot.available() {
            icon_handle.set_icon("network-wireless-offline-symbolic");
            icon_handle.set_spinning(false);
            icon_handle.add_css_class(state::SERVICE_UNAVAILABLE);
            icon_handle.remove_css_class(qs::WIFI_DISABLED_ICON);
            icon_handle.remove_css_class(state::ICON_ACTIVE);
        } else {
            icon_handle.remove_css_class(state::SERVICE_UNAVAILABLE);

            let material_unified = is_material_unified(snapshot);
            icon_handle.set_icon(resolve_material_network_icon(snapshot));

            // Spinner: show when wifi or cellular is connecting, but only
            // when the expanded details aren't visible (they have their own
            // per-row spinners, so showing both would be redundant).
            let wifi_connecting = snapshot.wifi_connecting();
            let is_connecting = wifi_connecting || snapshot.mobile_connecting();
            let expanded = state
                .base
                .revealer
                .borrow()
                .as_ref()
                .is_some_and(|r| r.reveals_child());
            icon_handle.set_spinning(is_connecting && !expanded);

            let icon_active = (enabled && snapshot.connected())
                || snapshot.wired_connected()
                || snapshot.mobile_active();
            set_icon_active(icon_handle, icon_active);

            // Disabled styling: only when actually showing a wifi icon, not
            // when displaying a cellular or combined icon.
            let showing_wifi_icon =
                !material_unified || (!snapshot.mobile_active() && !snapshot.mobile_connecting());
            if showing_wifi_icon && !enabled && !snapshot.wired_connected() {
                icon_handle.add_css_class(qs::WIFI_DISABLED_ICON);
            } else {
                icon_handle.remove_css_class(qs::WIFI_DISABLED_ICON);
            }
        }
    }

    update_subtitle(state, snapshot);

    update_ethernet_row(state, snapshot);
    update_mobile_row(state, snapshot);

    // Auto-clear mobile connection failure after 5 seconds (matches Wi-Fi pattern).
    if snapshot.mobile_failed() {
        schedule_failed_clear(&state.mobile_failed_clear_source, || {
            NetworkService::global().clear_mobile_failed_state();
        });
    }

    // IMPORTANT: This must remain AFTER all switch/toggle updates (wifi toggle,
    // wifi switch, mobile switch) to prevent their `state_set` signal handlers
    // from triggering recursive state changes during programmatic updates.
    state.updating_wifi_toggle.set(false);
    state.updating_mobile_switch.set(false);

    update_scan_ui(state, snapshot);

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
    use crate::services::network::iwd::StationState;
    use crate::services::network::network_manager::{MobileState, WifiState, WiredState};
    use crate::services::network::{IwdSnapshot, NmSnapshot};

    /// Default icon context for tests: available Wi-Fi system, nothing connected.
    /// Tests override only the fields relevant to the scenario being tested.
    fn test_icon_ctx() -> NetworkIconContext {
        NetworkIconContext {
            available: true,
            connected: false,
            wifi_enabled: true,
            wired_connected: false,
            has_wifi_device: true,
            mobile_is_primary: false,
            has_modem_device: false,
            mobile_signal_quality: None,
        }
    }

    #[test]
    fn test_network_icon_name_connected() {
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                connected: true,
                ..test_icon_ctx()
            }),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_network_icon_name_disconnected() {
        assert_eq!(
            network_icon_name(&test_icon_ctx()),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_network_icon_name_disabled() {
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                connected: true,
                wifi_enabled: false,
                ..test_icon_ctx()
            }),
            "network-wireless-offline-symbolic"
        );
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                wifi_enabled: false,
                ..test_icon_ctx()
            }),
            "network-wireless-offline-symbolic"
        );
    }

    #[test]
    fn test_network_icon_name_wired_connected() {
        // Wired connected takes precedence regardless of Wi-Fi state
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                wifi_enabled: false,
                wired_connected: true,
                ..test_icon_ctx()
            }),
            "network-wired-symbolic"
        );
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                connected: true,
                wired_connected: true,
                ..test_icon_ctx()
            }),
            "network-wired-symbolic"
        );
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                wifi_enabled: false,
                wired_connected: true,
                has_wifi_device: false,
                ..test_icon_ctx()
            }),
            "network-wired-symbolic"
        );
    }

    #[test]
    fn test_network_icon_name_ethernet_only_disconnected() {
        // Ethernet-only system (no Wi-Fi device), not connected - shows lan icon (grayed)
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                wifi_enabled: false,
                has_wifi_device: false,
                ..test_icon_ctx()
            }),
            "network-wired-symbolic"
        );
    }

    #[test]
    fn test_network_icon_name_service_unavailable() {
        // Service unavailable - always shows wireless offline icon regardless of other state
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                available: false,
                wifi_enabled: false,
                has_wifi_device: false,
                ..test_icon_ctx()
            }),
            "network-wireless-offline-symbolic"
        );
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                available: false,
                connected: true,
                ..test_icon_ctx()
            }),
            "network-wireless-offline-symbolic"
        );
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                available: false,
                wifi_enabled: false,
                wired_connected: true,
                has_wifi_device: false,
                ..test_icon_ctx()
            }),
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

    // Helper to create a base snapshot for testing
    fn test_snapshot() -> NmSnapshot {
        NmSnapshot {
            available: true,
            wifi: WifiState {
                enabled: Some(true),
                connected: false,
                has_device: true,
                ssid: None,
                strength: 0,
                scanning: false,
                is_ready: true,
                networks: Vec::new(),
                connecting_ssid: None,
                failed_ssid: None,
                device_state: None,
            },
            wired: WiredState {
                connected: false,
                has_device: false,
                iface: None,
                name: None,
                speed: None,
            },
            mobile: MobileState {
                is_primary: false,
                active: false,
                connecting: false,
                supported: false,
                enabled: Some(true),
                has_device: false,
                name: None,
                operator: None,
                access_technology: None,
                signal_quality: None,
                failed: false,
            },
            primary_connection_type: None,
        }
    }

    // Tests for get_network_subtitle_text()

    #[test]
    fn test_subtitle_wired_only() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Ethernet");
    }

    #[test]
    fn test_subtitle_wired_and_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.wifi.ssid = Some("MyNetwork".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Ethernet \u{2022} MyNetwork"
        );
    }

    #[test]
    fn test_subtitle_wired_and_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.wifi.connecting_ssid = Some("MyNetwork".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Ethernet \u{2022} Connecting to MyNetwork"
        );
    }

    #[test]
    fn test_subtitle_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "HomeWifi");
    }

    #[test]
    fn test_subtitle_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.connecting_ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Connecting to HomeWifi"
        );
    }

    #[test]
    fn test_subtitle_wifi_disconnected() {
        let snapshot = test_snapshot();
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Disconnected");
    }

    #[test]
    fn test_subtitle_wifi_disabled() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.enabled = Some(false);
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Off");
    }

    #[test]
    fn test_subtitle_ethernet_only_system_disconnected() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.has_device = false;
        snapshot.wired.has_device = true;
        snapshot.wifi.enabled = None;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Disconnected");
    }

    #[test]
    fn test_subtitle_service_unavailable() {
        let mut snapshot = test_snapshot();
        snapshot.available = false;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Unavailable");
    }

    // Tests for is_network_subtitle_active()

    #[test]
    fn test_subtitle_active_when_wired_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_active_when_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.connected = true;
        snapshot.wifi.ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_active_when_both_connected() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.wifi.ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_not_active_when_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wifi.connecting_ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(!is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_not_active_when_disconnected() {
        let snapshot = test_snapshot();
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(!is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_not_active_wired_but_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.wifi.connecting_ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        // Even though wired is connected, we're in a "connecting" state for Wi-Fi
        // so subtitle should not be fully active (shows connecting animation)
        assert!(!is_network_subtitle_active(&wrapped));
    }

    // --- IWD-specific tests ---

    /// Create a base IWD snapshot for testing.
    fn iwd_snapshot() -> IwdSnapshot {
        IwdSnapshot {
            available: true,
            ssid: None,
            state: None,
            wifi_enabled: Some(true),
            scanning: false,
            networks: Vec::new(),
            auth_request: None,
            failed_ssid: None,
            failed_reason: None,
            initial_scan_complete: true,
        }
    }

    #[test]
    fn test_iwd_subtitle_connected() {
        let mut snap = iwd_snapshot();
        snap.state = Some(StationState::Connected);
        snap.ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert_eq!(get_network_subtitle_text(&wrapped), "HomeWifi");
    }

    #[test]
    fn test_iwd_subtitle_connecting() {
        let mut snap = iwd_snapshot();
        snap.state = Some(StationState::Connecting);
        snap.ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Connecting to HomeWifi"
        );
    }

    #[test]
    fn test_iwd_subtitle_disconnected() {
        let snap = iwd_snapshot();
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert_eq!(get_network_subtitle_text(&wrapped), "Disconnected");
    }

    #[test]
    fn test_iwd_subtitle_disabled() {
        let mut snap = iwd_snapshot();
        snap.wifi_enabled = Some(false);
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert_eq!(get_network_subtitle_text(&wrapped), "Off");
    }

    #[test]
    fn test_iwd_subtitle_unavailable() {
        let mut snap = iwd_snapshot();
        snap.available = false;
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert_eq!(get_network_subtitle_text(&wrapped), "Unavailable");
    }

    #[test]
    fn test_iwd_subtitle_active_when_connected() {
        let mut snap = iwd_snapshot();
        snap.state = Some(StationState::Connected);
        snap.ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert!(is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_iwd_subtitle_not_active_when_connecting() {
        let mut snap = iwd_snapshot();
        snap.state = Some(StationState::Connecting);
        snap.ssid = Some("Network".to_string());
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert!(!is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_iwd_subtitle_not_active_when_disconnected() {
        let snap = iwd_snapshot();
        let wrapped = NetworkSnapshot::Iwd(snap);
        assert!(!is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_iwd_subtitle_roaming() {
        let mut snap = iwd_snapshot();
        snap.state = Some(StationState::Roaming);
        snap.ssid = Some("RoamNet".to_string());
        let wrapped = NetworkSnapshot::Iwd(snap);
        // Roaming is considered connected
        assert_eq!(get_network_subtitle_text(&wrapped), "RoamNet");
        assert!(is_network_subtitle_active(&wrapped));
    }

    // ---- cellular_signal_icon_name tests ----

    #[test]
    fn test_cellular_signal_excellent() {
        assert_eq!(
            cellular_signal_icon_name(75),
            "network-cellular-signal-excellent-symbolic"
        );
        assert_eq!(
            cellular_signal_icon_name(100),
            "network-cellular-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_cellular_signal_good() {
        assert_eq!(
            cellular_signal_icon_name(55),
            "network-cellular-signal-good-symbolic"
        );
        assert_eq!(
            cellular_signal_icon_name(74),
            "network-cellular-signal-good-symbolic"
        );
    }

    #[test]
    fn test_cellular_signal_ok() {
        assert_eq!(
            cellular_signal_icon_name(35),
            "network-cellular-signal-ok-symbolic"
        );
        assert_eq!(
            cellular_signal_icon_name(54),
            "network-cellular-signal-ok-symbolic"
        );
    }

    #[test]
    fn test_cellular_signal_weak() {
        assert_eq!(
            cellular_signal_icon_name(15),
            "network-cellular-signal-weak-symbolic"
        );
        assert_eq!(
            cellular_signal_icon_name(34),
            "network-cellular-signal-weak-symbolic"
        );
    }

    #[test]
    fn test_cellular_signal_none() {
        assert_eq!(
            cellular_signal_icon_name(14),
            "network-cellular-signal-none-symbolic"
        );
        assert_eq!(
            cellular_signal_icon_name(0),
            "network-cellular-signal-none-symbolic"
        );
    }

    // ---- Mobile subtitle text tests ----

    #[test]
    fn test_subtitle_mobile_connected_with_operator() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        snapshot.mobile.operator = Some("MyCarrier".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "MyCarrier");
    }

    #[test]
    fn test_subtitle_mobile_connected_with_name_only() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        snapshot.mobile.name = Some("My SIM".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "My SIM");
    }

    #[test]
    fn test_subtitle_mobile_connected_no_name() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Mobile");
    }

    #[test]
    fn test_subtitle_mobile_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.connecting = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Mobile \u{2022} Connecting..."
        );
    }

    #[test]
    fn test_subtitle_mobile_connected_and_wifi_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        snapshot.wifi.connecting_ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Mobile \u{2022} Connecting to HomeWifi"
        );
    }

    #[test]
    fn test_subtitle_mobile_connected_and_wifi_connected() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        snapshot.wifi.ssid = Some("HomeWifi".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "HomeWifi \u{2022} Mobile"
        );
    }

    #[test]
    fn test_subtitle_active_mobile_connected() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.is_primary = true;
        snapshot.mobile.active = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(is_network_subtitle_active(&wrapped));
    }

    #[test]
    fn test_subtitle_not_active_mobile_connecting() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.connecting = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(!is_network_subtitle_active(&wrapped));
    }

    // ---- Mobile subtitle: active (not just primary) ----

    #[test]
    fn test_subtitle_mobile_active_not_primary_with_operator() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.active = true;
        snapshot.mobile.operator = Some("Vodafone".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(get_network_subtitle_text(&wrapped), "Vodafone");
    }

    #[test]
    fn test_subtitle_wired_and_mobile_active() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.mobile.active = true;
        snapshot.mobile.operator = Some("MyCarrier".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Ethernet \u{2022} MyCarrier"
        );
    }

    #[test]
    fn test_subtitle_wired_wifi_and_mobile_active() {
        let mut snapshot = test_snapshot();
        snapshot.wired.connected = true;
        snapshot.wifi.ssid = Some("HomeWifi".to_string());
        snapshot.mobile.active = true;
        snapshot.mobile.operator = Some("MyCarrier".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "Ethernet \u{2022} HomeWifi \u{2022} MyCarrier"
        );
    }

    #[test]
    fn test_subtitle_mobile_connecting_with_operator() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.connecting = true;
        snapshot.mobile.operator = Some("AT&T".to_string());
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert_eq!(
            get_network_subtitle_text(&wrapped),
            "AT&T \u{2022} Connecting..."
        );
    }

    #[test]
    fn test_subtitle_active_mobile_active_not_primary() {
        let mut snapshot = test_snapshot();
        snapshot.mobile.active = true;
        let wrapped = NetworkSnapshot::NetworkManager(snapshot);
        assert!(is_network_subtitle_active(&wrapped));
    }

    // ---- network_icon_name with mobile ----

    #[test]
    fn test_wifi_icon_mobile_connected_with_signal() {
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                mobile_is_primary: true,
                has_modem_device: true,
                mobile_signal_quality: Some(80),
                ..test_icon_ctx()
            }),
            "network-cellular-signal-excellent-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_mobile_connected_no_signal() {
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                mobile_is_primary: true,
                has_modem_device: true,
                ..test_icon_ctx()
            }),
            "network-cellular-signal-none-symbolic"
        );
    }

    #[test]
    fn test_wifi_icon_modem_only_no_wifi() {
        // Modem-only system without mobile connection shows cellular none icon
        assert_eq!(
            network_icon_name(&NetworkIconContext {
                wifi_enabled: false,
                has_wifi_device: false,
                has_modem_device: true,
                ..test_icon_ctx()
            }),
            "network-cellular-signal-none-symbolic"
        );
    }
}
