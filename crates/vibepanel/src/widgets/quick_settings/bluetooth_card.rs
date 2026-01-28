//! Bluetooth card for Quick Settings panel.
//!
//! This module contains:
//! - Bluetooth icon helpers (merged from qs_bluetooth_helpers.rs)
//! - Bluetooth details panel building
//! - Device list population
//! - Device action handling
//! - Bluetooth pairing authentication prompts (PIN/passkey/confirmation)

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Entry, Label, ListBox, ListBoxRow, Orientation, Popover, ScrolledWindow,
};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, ScanButton, add_disabled_placeholder, add_placeholder_row,
    build_accent_subtitle, clear_list_box, create_qs_list_box, create_row_action_label,
    create_row_menu_action, create_row_menu_button, set_icon_active, set_subtitle_active,
};
use crate::services::bluetooth::{
    BluetoothAuthRequest, BluetoothDevice, BluetoothService, BluetoothSnapshot,
};
use crate::services::icons::IconsService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, icon, qs, row, surface};
use crate::widgets::base::configure_popover;

/// Identity of an auth request for cache invalidation.
/// Tracks (device_path, request_kind) to detect when a different auth prompt appears.
type AuthRequestId = (String, std::mem::Discriminant<BluetoothAuthRequest>);

/// Callback type for input change notifications.
type InputChangedCallback = Option<Rc<dyn Fn(&str)>>;

/// Return an icon name matching Bluetooth state.
///
/// Uses standard Adwaita/GTK icon names with -symbolic suffix.
pub fn bt_icon_name(powered: bool, connected_devices: usize) -> &'static str {
    if !powered {
        "bluetooth-disabled-symbolic"
    } else if connected_devices > 0 {
        // active-symbolic semantically means "in use / connected"
        "bluetooth-active-symbolic"
    } else {
        "bluetooth-symbolic"
    }
}

/// State for the Bluetooth card in the Quick Settings panel.
///
/// Uses `ExpandableCardBase` for common expandable card fields and adds
/// Bluetooth specific state (scan button).
pub struct BluetoothCardState {
    /// Common expandable card state (toggle, icon, subtitle, list_box, revealer, arrow).
    pub base: ExpandableCardBase,
    /// Bluetooth scan button (self-contained with animation).
    pub scan_button: RefCell<Option<Rc<ScanButton>>>,
    /// Guard to prevent feedback loop when programmatically updating toggle.
    pub updating_toggle: Cell<bool>,
    /// Cached user input for auth (preserved across list rebuilds).
    /// Cleared when auth request identity changes or is dismissed.
    /// Wrapped in Rc so it can be shared with entry change handlers.
    auth_input: Rc<RefCell<String>>,
    /// Identity of the current auth request (device_path, request_kind).
    /// Used to detect when we need to clear cached input.
    auth_request_id: RefCell<Option<AuthRequestId>>,
}

impl BluetoothCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            scan_button: RefCell::new(None),
            updating_toggle: Cell::new(false),
            auth_input: Rc::new(RefCell::new(String::new())),
            auth_request_id: RefCell::new(None),
        }
    }

    /// Clear cached auth input (called when auth request changes).
    fn clear_auth_input(&self) {
        self.auth_input.borrow_mut().clear();
    }

    /// Get the cached auth input.
    fn get_auth_input(&self) -> String {
        self.auth_input.borrow().clone()
    }

    /// Get a clone of the auth_input Rc for use in closures.
    fn auth_input_rc(&self) -> Rc<RefCell<String>> {
        Rc::clone(&self.auth_input)
    }

    /// Update auth request identity tracking. Returns true if identity changed
    /// (meaning cached input should be cleared).
    fn update_auth_request_id(&self, auth_request: Option<&BluetoothAuthRequest>) -> bool {
        let new_id = auth_request.map(|r| (r.device_path().to_string(), std::mem::discriminant(r)));
        let mut current_id = self.auth_request_id.borrow_mut();
        if *current_id != new_id {
            *current_id = new_id;
            true
        } else {
            false
        }
    }
}

impl Default for BluetoothCardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for BluetoothCardState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

/// Result of building Bluetooth details section.
pub struct BluetoothDetailsResult {
    pub container: GtkBox,
    pub list_box: ListBox,
    pub scan_button: Rc<ScanButton>,
}

/// Build the Bluetooth details section with scan button and device list.
pub fn build_bluetooth_details(state: &Rc<BluetoothCardState>) -> BluetoothDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    // Controls row: spacer + Scan button (right-aligned, matching Wi-Fi layout)
    let controls_row = GtkBox::new(Orientation::Horizontal, 8);
    controls_row.add_css_class(qs::BT_CONTROLS_ROW);

    // Spacer to push scan button to the right
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    controls_row.append(&spacer);

    // Scan button
    let scan_button = ScanButton::new(|| {
        BluetoothService::global().scan_for_devices();
    });

    controls_row.append(scan_button.widget());
    container.append(&controls_row);

    // Device list
    let list_box = create_qs_list_box();

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_child(Some(&list_box));
    scroller.set_max_content_height(360);
    scroller.set_propagate_natural_height(true);

    container.append(&scroller);

    // Populate with current Bluetooth state
    let snapshot = BluetoothService::global().snapshot();
    populate_bluetooth_list(&list_box, &snapshot, state);

    BluetoothDetailsResult {
        container,
        list_box,
        scan_button,
    }
}

/// Populate the Bluetooth list with device data from snapshot.
pub fn populate_bluetooth_list(
    list_box: &ListBox,
    snapshot: &BluetoothSnapshot,
    state: &BluetoothCardState,
) {
    clear_list_box(list_box);

    // Clear cached auth input if auth request identity changed (different device/type)
    // or if there's no auth request at all
    if state.update_auth_request_id(snapshot.auth_request.as_ref()) {
        state.clear_auth_input();
    }

    if !snapshot.has_adapter {
        add_placeholder_row(list_box, "Bluetooth unavailable");
        return;
    }

    if !snapshot.powered {
        add_disabled_placeholder(
            list_box,
            "bluetooth-disabled-symbolic",
            "Bluetooth is disabled",
        );
        return;
    }

    if !snapshot.is_ready {
        add_placeholder_row(list_box, "Scanning for devices...");
        return;
    }

    if snapshot.devices.is_empty() {
        add_placeholder_row(list_box, "No Bluetooth devices");
        return;
    }

    let icons = IconsService::global();

    // Get target device path from auth request (if any) - borrow to avoid allocation
    let auth_target_device = snapshot.auth_request.as_ref().map(|r| r.device_path());

    for dev in &snapshot.devices {
        // Use pairing_device_path from snapshot for accurate pairing state
        let is_pairing = snapshot
            .pairing_device_path
            .as_ref()
            .is_some_and(|p| p == &dev.path);

        let title = if !dev.name.is_empty() {
            dev.name.clone()
        } else if !dev.address.is_empty() {
            dev.address.clone()
        } else {
            "Unknown device".to_string()
        };

        let icon_name = dev.icon.as_deref().unwrap_or(if dev.connected {
            "bluetooth-active-symbolic"
        } else {
            "bluetooth-symbolic"
        });
        let icon_color = if dev.connected {
            color::ACCENT
        } else {
            color::PRIMARY
        };
        let icon_handle = icons.create_icon(icon_name, &[icon::TEXT, row::QS_ICON, icon_color]);
        let leading_icon = icon_handle.widget();

        let right_widget = create_bluetooth_action_widget(dev, is_pairing);

        let mut row_builder = ListRow::builder()
            .title(&title)
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::BT_ROW);

        if is_pairing {
            // Pairing in progress: show "Pairing..." subtitle
            row_builder = row_builder.subtitle("Pairing...");
        } else if dev.connected {
            // Connected: accent "Connected" + optional "Paired"
            let extra_parts: Vec<&str> = if dev.paired { vec!["Paired"] } else { vec![] };
            let subtitle_widget = build_accent_subtitle("Connected", &extra_parts);
            row_builder = row_builder.subtitle_widget(subtitle_widget.upcast());
        } else if dev.paired {
            // Paired only: plain muted subtitle
            row_builder = row_builder.subtitle("Paired");
        } else if dev.trusted {
            // Trusted only (known device): plain muted subtitle
            row_builder = row_builder.subtitle("Saved");
        }
        // Neither connected, paired, nor trusted: no subtitle

        let row_result = row_builder.build();

        {
            let path = dev.path.clone();
            let paired = dev.paired;
            let trusted = dev.trusted;
            let connected = dev.connected;
            row_result.row.connect_activate(move |_| {
                let bt = BluetoothService::global();
                if connected {
                    bt.disconnect_device(&path);
                } else if paired || trusted {
                    bt.connect_device(&path);
                }
                // Unpaired/untrusted devices: handled by the "Pair" button gesture
            });
        }

        list_box.append(&row_result.row);

        // Insert auth row directly under the matching device row
        if let Some(target) = auth_target_device
            && !target.is_empty()
            && target == dev.path
            && snapshot.auth_request.is_some()
        {
            let auth_row = build_auth_row(snapshot.auth_request.as_ref().unwrap(), state);
            list_box.append(&auth_row);
        }
    }

    // Fallback: append auth row at end if target device not found in list
    if let Some(target) = auth_target_device
        && !target.is_empty()
        && !snapshot.devices.iter().any(|d| d.path == target)
        && snapshot.auth_request.is_some()
    {
        let auth_row = build_auth_row(snapshot.auth_request.as_ref().unwrap(), state);
        list_box.append(&auth_row);
    }
}

/// Create the action widget for a Bluetooth device row.
fn create_bluetooth_action_widget(dev: &BluetoothDevice, is_pairing: bool) -> gtk4::Widget {
    let path = dev.path.clone();
    let paired = dev.paired;
    let trusted = dev.trusted;

    // If pairing is in progress, show nothing (hide the Pair button)
    if is_pairing {
        let placeholder = GtkBox::new(Orientation::Horizontal, 0);
        return placeholder.upcast();
    }

    // Unpaired/untrusted devices: single "Pair" label (same style as Wi-Fi "Connect")
    if !paired && !trusted {
        let label = create_row_action_label("Pair");
        let path_clone = path.clone();
        label.connect_clicked(move |_| {
            let bt = BluetoothService::global();
            bt.pair_device(&path_clone);
        });
        return label.upcast();
    }

    // Paired or trusted devices: hamburger menu (Connect/Disconnect/Forget)
    let menu_btn = create_row_menu_button();

    let path_for_menu = path.clone();

    menu_btn.connect_clicked(move |btn| {
        // Query fresh snapshot at click time to get current connected state
        let bt = BluetoothService::global();
        let snapshot = bt.snapshot();
        let connected = snapshot
            .devices
            .iter()
            .find(|d| d.path == path_for_menu)
            .map(|d| d.connected)
            .unwrap_or(false);

        let popover = Popover::new();
        configure_popover(&popover);

        let panel = GtkBox::new(Orientation::Vertical, 0);
        panel.add_css_class(surface::WIDGET_MENU_CONTENT);

        let content_box = GtkBox::new(Orientation::Vertical, 2);
        content_box.add_css_class(qs::ROW_MENU_CONTENT);

        if connected {
            let path = path_for_menu.clone();
            let action = create_row_menu_action("Disconnect", move || {
                let bt = BluetoothService::global();
                debug!("bt_disconnect_from_menu path={}", path);
                bt.disconnect_device(&path);
            });
            content_box.append(&action);
        } else {
            let path = path_for_menu.clone();
            let action = create_row_menu_action("Connect", move || {
                let bt = BluetoothService::global();
                debug!("bt_connect_from_menu path={}", path);
                bt.connect_device(&path);
            });
            content_box.append(&action);
        }

        let path = path_for_menu.clone();
        let action = create_row_menu_action("Forget", move || {
            let bt = BluetoothService::global();
            debug!("bt_forget_from_menu path={}", path);
            bt.forget_device(&path);
        });
        content_box.append(&action);

        panel.append(&content_box);
        SurfaceStyleManager::global().apply_surface_styles(&panel, true);

        popover.set_child(Some(&panel));
        popover.set_parent(btn);
        popover.popup();
    });

    menu_btn.upcast()
}

/// Validate auth input string. Returns true if input is valid for submission.
fn validate_auth_input(input: &str, char_count: usize, requires_passkey_parse: bool) -> bool {
    if input.len() != char_count {
        return false;
    }
    if requires_passkey_parse {
        return input.parse::<u32>().is_ok();
    }
    true
}

/// Collect input string from entries.
fn collect_entries_input(entries: &[Entry]) -> String {
    entries.iter().map(|e| e.text().to_string()).collect()
}

/// Build an inline auth row for the given auth request.
/// Rebuilds fresh each time, seeding entries from cached input in state.
fn build_auth_row(auth_request: &BluetoothAuthRequest, state: &BluetoothCardState) -> ListBoxRow {
    let device_name = auth_request.device_name();

    let auth_box = GtkBox::new(Orientation::Vertical, 6);
    auth_box.add_css_class(qs::BT_AUTH_PROMPT);

    // Label
    let label_text = match auth_request {
        BluetoothAuthRequest::RequestPinCode { .. } => {
            format!("Enter PIN for {}", device_name)
        }
        BluetoothAuthRequest::RequestPasskey { .. } => {
            format!("Enter passkey for {}", device_name)
        }
        BluetoothAuthRequest::RequestConfirmation { .. } => {
            format!("Confirm the code matches on {}", device_name)
        }
        BluetoothAuthRequest::DisplayPinCode { .. } => {
            format!("Enter the code on {}", device_name)
        }
        BluetoothAuthRequest::DisplayPasskey { .. } => {
            format!("Enter the code on {}", device_name)
        }
    };
    let auth_label = Label::new(Some(&label_text));
    auth_label.set_xalign(0.0);
    auth_box.append(&auth_label);

    // Apply Pango font attrs to fix text clipping on layer-shell surfaces
    SurfaceStyleManager::global().apply_pango_attrs(&auth_label);

    // Character entry container
    let char_container = GtkBox::new(Orientation::Horizontal, 0);
    char_container.add_css_class(qs::BT_CHAR_CONTAINER);
    char_container.set_halign(gtk4::Align::Center);
    auth_box.append(&char_container);

    // Button row: [spacer] [cancel] [confirm]
    let btn_row = GtkBox::new(Orientation::Horizontal, 8);
    btn_row.add_css_class(qs::BT_AUTH_BUTTONS);

    let btn_spacer = GtkBox::new(Orientation::Horizontal, 0);
    btn_spacer.set_hexpand(true);
    btn_row.append(&btn_spacer);

    let btn_cancel = Button::with_label("Cancel");
    btn_cancel.add_css_class(button::CARD);
    btn_cancel.connect_clicked(|_| {
        debug!("Auth cancelled by user");
        BluetoothService::global().cancel_auth();
    });

    let is_display_mode = auth_request.is_display_only();
    let is_confirmation = matches!(
        auth_request,
        BluetoothAuthRequest::RequestConfirmation { .. }
    );

    let btn_confirm = Button::with_label(if is_confirmation { "Confirm" } else { "Pair" });
    btn_confirm.add_css_class(button::ACCENT);

    if is_display_mode {
        // Display modes: hide confirm button, only show Cancel
        btn_confirm.set_visible(false);
    }

    // Check if this is a passkey request (requires numeric parse validation)
    let requires_passkey_parse =
        matches!(auth_request, BluetoothAuthRequest::RequestPasskey { .. });

    // Build character entries with cached input and validation callback
    let cached_input = state.get_auth_input();
    let auth_input_rc = state.auth_input_rc();

    // Use char_count from auth request to ensure consistency
    let char_count = auth_request.char_count();

    // Create validation callback that updates button sensitivity
    let validation_callback: InputChangedCallback = if !is_display_mode {
        let btn = btn_confirm.clone();
        Some(Rc::new(move |input: &str| {
            let enabled = validate_auth_input(input, char_count, requires_passkey_parse);
            btn.set_sensitive(enabled);
        }))
    } else {
        None
    };

    let entries = build_char_entries_inline(
        &char_container,
        auth_request,
        char_count,
        &cached_input,
        auth_input_rc,
        validation_callback,
    );

    // Set initial button sensitivity based on cached input
    if !is_display_mode {
        let current_input = collect_entries_input(&entries);
        let enabled = validate_auth_input(&current_input, char_count, requires_passkey_parse);
        btn_confirm.set_sensitive(enabled);
    }

    // Wire up confirm button
    let entries_for_confirm = entries.clone();
    let auth_request_for_confirm = auth_request.clone();
    btn_confirm.connect_clicked(move |_| {
        on_auth_confirm(&entries_for_confirm, &auth_request_for_confirm);
    });

    // Wire Enter key on last entry to click confirm
    if let Some(last_entry) = entries.last() {
        let btn_confirm_clone = btn_confirm.clone();
        last_entry.connect_activate(move |_| {
            btn_confirm_clone.emit_clicked();
        });
    }

    btn_row.append(&btn_cancel);
    btn_row.append(&btn_confirm);
    auth_box.append(&btn_row);

    let auth_row = ListBoxRow::new();
    auth_row.set_activatable(false);
    auth_row.set_focusable(true);
    auth_row.set_child(Some(&auth_box));

    auth_row
}

/// Build character entry boxes inline and return the Entry widgets.
/// Seeds entries from `cached_input` and wires them to sync back to `auth_input_rc`.
/// If `on_input_changed` is provided, it's called after each sync with the new input string.
fn build_char_entries_inline(
    container: &GtkBox,
    auth_request: &BluetoothAuthRequest,
    char_count: usize,
    cached_input: &str,
    auth_input_rc: Rc<RefCell<String>>,
    on_input_changed: InputChangedCallback,
) -> Vec<Entry> {
    // Determine entry configuration based on auth type
    let (read_only, display_value, is_numeric, uppercase) = match auth_request {
        BluetoothAuthRequest::RequestPinCode { .. } => (false, None, false, true),
        BluetoothAuthRequest::RequestPasskey { .. } => (false, None, true, false),
        BluetoothAuthRequest::RequestConfirmation { passkey, .. } => {
            (true, Some(format!("{:06}", passkey)), true, false)
        }
        BluetoothAuthRequest::DisplayPinCode { pincode, .. } => {
            (true, Some(pincode.clone()), false, true)
        }
        BluetoothAuthRequest::DisplayPasskey { passkey, .. } => {
            (true, Some(format!("{:06}", passkey)), true, false)
        }
    };

    // For editable entries, use cached input; for display-only, use display_value
    let initial_value = if read_only {
        display_value.unwrap_or_default()
    } else {
        cached_input.to_string()
    };

    let mut entries = Vec::new();
    let initial_chars: Vec<char> = initial_value.chars().collect();

    for i in 0..char_count {
        let entry = Entry::new();
        entry.add_css_class(qs::BT_CHAR_BOX);
        entry.set_max_length(1);
        entry.set_width_chars(1);
        entry.set_max_width_chars(1);
        gtk4::prelude::EditableExt::set_alignment(&entry, 0.5);
        entry.set_sensitive(!read_only);

        // Set initial value if provided
        if let Some(&c) = initial_chars.get(i) {
            entry.set_text(&c.to_string());
        }

        container.append(&entry);
        entries.push(entry);
    }

    // Wire up auto-advance and backspace navigation for editable entries
    if !read_only {
        let entries_rc = Rc::new(entries.clone());
        let navigating = Rc::new(Cell::new(false));

        // Helper to collect and sync current input to cached state, then notify
        let sync_to_cache = {
            let entries_for_sync = entries_rc.clone();
            let auth_input_for_sync = auth_input_rc.clone();
            let on_changed = on_input_changed.clone();
            move || {
                let input: String = entries_for_sync
                    .iter()
                    .map(|e| e.text().to_string())
                    .collect();
                *auth_input_for_sync.borrow_mut() = input.clone();
                if let Some(ref cb) = on_changed {
                    cb(&input);
                }
            }
        };

        for (i, entry) in entries.iter().enumerate() {
            let idx = i;
            let navigating_clone = navigating.clone();
            let sync_to_cache_clone = sync_to_cache.clone();

            // Filter/transform input, auto-advance, and sync to cache
            let entries_clone = entries_rc.clone();
            entry.connect_changed(move |e| {
                if navigating_clone.get() {
                    return;
                }

                let text = e.text();

                // Sync even on empty (handles backspace clearing)
                sync_to_cache_clone();

                if text.is_empty() {
                    return;
                }

                let filtered: String = if is_numeric {
                    text.chars().filter(|c| c.is_ascii_digit()).collect()
                } else if uppercase {
                    text.chars()
                        .filter(|c| c.is_alphanumeric())
                        .map(|c| c.to_ascii_uppercase())
                        .collect()
                } else {
                    text.to_string()
                };

                if filtered != text.as_str() {
                    let entry = e.clone();
                    gtk4::glib::idle_add_local_once(move || {
                        entry.set_text(&filtered);
                        entry.set_position(filtered.len() as i32);
                    });
                    return;
                }

                if filtered.len() == 1 && idx + 1 < entries_clone.len() {
                    let next = entries_clone[idx + 1].clone();
                    gtk4::glib::idle_add_local_once(move || {
                        next.grab_focus();
                    });
                }
            });

            // Handle backspace on empty box
            let entries_clone = entries_rc.clone();
            let navigating_clone = navigating.clone();
            let key_controller = gtk4::EventControllerKey::new();
            key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
            key_controller.connect_key_pressed(move |_, key, _, _| {
                if key == gtk4::gdk::Key::BackSpace
                    && idx > 0
                    && entries_clone[idx].text().is_empty()
                {
                    let prev = entries_clone[idx - 1].clone();
                    navigating_clone.set(true);
                    prev.set_text("");
                    prev.grab_focus();
                    navigating_clone.set(false);
                    return gtk4::glib::Propagation::Stop;
                }
                gtk4::glib::Propagation::Proceed
            });
            entry.add_controller(key_controller);
        }

        // Focus appropriate entry on initial map:
        // - First empty entry if some are pre-filled from cache
        // - Last entry if all filled (user was typing when rebuild happened)
        // - First entry if all empty
        let entries_for_focus = entries_rc.clone();
        if let Some(first_entry) = entries.first() {
            let focused = Rc::new(Cell::new(false));
            first_entry.connect_map(move |_| {
                if !focused.get() {
                    focused.set(true);
                    // Find first empty entry, or use last entry if all filled
                    let target = entries_for_focus
                        .iter()
                        .find(|e| e.text().is_empty())
                        .cloned()
                        .unwrap_or_else(|| entries_for_focus.last().unwrap().clone());
                    target.grab_focus();
                }
            });
        }
    }

    entries
}

/// Handle confirm button click for auth.
fn on_auth_confirm(entries: &[Entry], auth_request: &BluetoothAuthRequest) {
    let bt = BluetoothService::global();
    let value: String = entries.iter().map(|e| e.text().to_string()).collect();

    match auth_request {
        BluetoothAuthRequest::RequestPinCode { .. } => {
            bt.submit_pin(&value);
        }
        BluetoothAuthRequest::RequestPasskey { .. } => {
            if let Ok(passkey) = value.parse::<u32>() {
                bt.submit_passkey(passkey);
            }
        }
        BluetoothAuthRequest::RequestConfirmation { .. } => {
            bt.confirm_passkey();
        }
        BluetoothAuthRequest::DisplayPinCode { .. }
        | BluetoothAuthRequest::DisplayPasskey { .. } => {
            // Display modes - nothing to submit
        }
    }
}

/// Handle Bluetooth state changes from BluetoothService.
pub fn on_bluetooth_changed(state: &BluetoothCardState, snapshot: &BluetoothSnapshot) {
    // Update toggle state and sensitivity
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        let should_be_active = snapshot.powered && snapshot.has_adapter;
        if toggle.is_active() != should_be_active {
            state.updating_toggle.set(true);
            toggle.set_active(should_be_active);
            state.updating_toggle.set(false);
        }
        toggle.set_sensitive(snapshot.has_adapter);
    }

    // Update Bluetooth card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let icon_name = bt_icon_name(snapshot.powered, snapshot.connected_devices);
        icon_handle.set_icon(icon_name);
        set_icon_active(icon_handle, snapshot.connected_devices > 0);
        // Apply disabled styling when Bluetooth is off
        if !snapshot.powered {
            icon_handle.add_css_class(qs::BT_DISABLED_ICON);
        } else {
            icon_handle.remove_css_class(qs::BT_DISABLED_ICON);
        }
    }

    // Update Bluetooth subtitle
    if let Some(label) = state.base.subtitle.borrow().as_ref() {
        let subtitle = if !snapshot.has_adapter {
            "Unavailable".to_string()
        } else if !snapshot.is_ready {
            "Bluetooth".to_string()
        } else if snapshot.connected_devices > 0 {
            if snapshot.connected_devices == 1 {
                snapshot
                    .devices
                    .iter()
                    .find(|d| d.connected)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "Bluetooth".to_string())
            } else {
                format!("{} connected", snapshot.connected_devices)
            }
        } else if snapshot.powered {
            "Enabled".to_string()
        } else {
            "Disabled".to_string()
        };
        label.set_label(&subtitle);
        set_subtitle_active(label, snapshot.connected_devices > 0);
    }

    // Update scan button: hide when powered off, show otherwise
    if let Some(scan_btn) = state.scan_button.borrow().as_ref() {
        scan_btn.set_visible(snapshot.powered);
        scan_btn.set_sensitive(snapshot.has_adapter && !snapshot.scanning);
        scan_btn.set_scanning(snapshot.scanning);
    }

    // Update device list
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        populate_bluetooth_list(list_box, snapshot, state);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}
