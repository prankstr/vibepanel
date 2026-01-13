//! Bluetooth card for Quick Settings panel.
//!
//! This module contains:
//! - Bluetooth icon helpers (merged from qs_bluetooth_helpers.rs)
//! - Bluetooth details panel building
//! - Device list population
//! - Device action handling

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, GestureClick, Label, ListBox, Orientation, Popover, ScrolledWindow,
};
use tracing::debug;

use super::components::ListRow;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, add_placeholder_row, build_scan_button, clear_list_box,
    create_qs_list_box, create_row_action_label, create_row_menu_action, create_row_menu_button,
    set_icon_active, set_subtitle_active,
};
use crate::services::bluetooth::{BluetoothDevice, BluetoothService, BluetoothSnapshot};
use crate::services::icons::IconsService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{color, icon, qs, row, state, surface};
use crate::widgets::base::configure_popover;

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
    /// Bluetooth scan button.
    pub scan_button: RefCell<Option<Button>>,
    /// Bluetooth scan label.
    pub scan_label: RefCell<Option<Label>>,
}

impl BluetoothCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            scan_button: RefCell::new(None),
            scan_label: RefCell::new(None),
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
    pub scan_button: Button,
    pub scan_label: Label,
}

/// Build the Bluetooth details section with scan button and device list.
pub fn build_bluetooth_details(state: &Rc<BluetoothCardState>) -> BluetoothDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 0);

    // Scan button
    let scan_result = build_scan_button("Scan");
    let scan_button = scan_result.button;
    let scan_label = scan_result.label;

    {
        scan_button.connect_clicked(move |_| {
            BluetoothService::global().scan_for_devices();
        });
    }

    container.append(&scan_button);

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
    populate_bluetooth_list(state, &list_box, &snapshot);

    BluetoothDetailsResult {
        container,
        list_box,
        scan_button,
        scan_label,
    }
}

/// Populate the Bluetooth list with device data from snapshot.
pub fn populate_bluetooth_list(
    _state: &BluetoothCardState,
    list_box: &ListBox,
    snapshot: &BluetoothSnapshot,
) {
    clear_list_box(list_box);

    if !snapshot.has_adapter {
        add_placeholder_row(list_box, "Bluetooth unavailable");
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

    for dev in &snapshot.devices {
        let mut subtitle_parts = Vec::new();
        if dev.connected {
            subtitle_parts.push("Connected".to_string());
        }
        if dev.paired {
            subtitle_parts.push("Paired".to_string());
        }
        let subtitle = if subtitle_parts.is_empty() {
            None
        } else {
            Some(subtitle_parts.join(" \u{2022} "))
        };

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
        let icon_handle = icons.create_icon(icon_name, &[icon::TEXT, row::QS_ICON, color::PRIMARY]);
        let leading_icon = icon_handle.widget();

        let right_widget = create_bluetooth_action_widget(dev);

        let row_result = ListRow::builder()
            .title(&title)
            .subtitle_opt(subtitle.as_deref())
            .leading_widget(leading_icon)
            .trailing_widget(right_widget)
            .css_class(qs::BT_ROW)
            .build();

        {
            let path = dev.path.clone();
            let paired = dev.paired;
            let connected = dev.connected;
            row_result.row.connect_activate(move |_| {
                let bt = BluetoothService::global();
                if connected {
                    bt.disconnect_device(&path);
                } else if paired {
                    bt.connect_device(&path);
                } else {
                    bt.pair_device(&path);
                }
            });
        }

        list_box.append(&row_result.row);
    }
}

/// Create the action widget for a Bluetooth device row.
fn create_bluetooth_action_widget(dev: &BluetoothDevice) -> gtk4::Widget {
    let path = dev.path.clone();
    let paired = dev.paired;
    let connected = dev.connected;

    // Unpaired devices: single "Pair" label (same style as Wi-Fi "Connect")
    if !paired {
        let label = create_row_action_label("Pair");
        let path_clone = path.clone();
        let gesture = GestureClick::new();
        gesture.set_button(1);
        gesture.connect_pressed(move |_, _, _, _| {
            let bt = BluetoothService::global();
            bt.pair_device(&path_clone);
        });
        label.add_controller(gesture);
        return label.upcast();
    }

    // Paired devices: hamburger menu (Connect/Disconnect/Forget)
    let menu_btn = create_row_menu_button();

    let path_for_connect = path.clone();
    let path_for_disconnect = path.clone();
    let path_for_forget = path.clone();

    menu_btn.connect_clicked(move |btn| {
        let popover = Popover::new();
        configure_popover(&popover);

        let panel = GtkBox::new(Orientation::Vertical, 0);
        panel.add_css_class(surface::WIDGET_MENU_CONTENT);

        let content_box = GtkBox::new(Orientation::Vertical, 2);
        content_box.add_css_class(qs::ROW_MENU_CONTENT);

        if connected {
            let path = path_for_disconnect.clone();
            let action = create_row_menu_action("Disconnect", move || {
                let bt = BluetoothService::global();
                debug!("bt_disconnect_from_menu path={}", path);
                bt.disconnect_device(&path);
            });
            content_box.append(&action);
        } else {
            let path = path_for_connect.clone();
            let action = create_row_menu_action("Connect", move || {
                let bt = BluetoothService::global();
                debug!("bt_connect_from_menu path={}", path);
                bt.connect_device(&path);
            });
            content_box.append(&action);
        }

        let path = path_for_forget.clone();
        let action = create_row_menu_action("Forget", move || {
            let bt = BluetoothService::global();
            debug!("bt_forget_from_menu path={}", path);
            bt.forget_device(&path);
        });
        content_box.append(&action);

        panel.append(&content_box);
        SurfaceStyleManager::global().apply_surface_styles(&panel, true, None);

        popover.set_child(Some(&panel));
        popover.set_parent(btn);
        popover.popup();
    });

    menu_btn.upcast()
}

/// Handle Bluetooth state changes from BluetoothService.
pub fn on_bluetooth_changed(state: &BluetoothCardState, snapshot: &BluetoothSnapshot) {
    // Update toggle state and sensitivity
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        let should_be_active = snapshot.powered && snapshot.has_adapter;
        if toggle.is_active() != should_be_active {
            toggle.set_active(should_be_active);
        }
        toggle.set_sensitive(snapshot.has_adapter);
    }

    // Update Bluetooth card icon and its active state class
    if let Some(icon_handle) = state.base.card_icon.borrow().as_ref() {
        let icon_name = bt_icon_name(snapshot.powered, snapshot.connected_devices);
        icon_handle.set_icon(icon_name);
        set_icon_active(icon_handle, snapshot.connected_devices > 0);
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

    // Update scan button UI
    if let Some(label) = state.scan_label.borrow().as_ref() {
        if snapshot.scanning {
            label.set_label("Scanning\u{2026}");
            label.add_css_class(state::SCANNING);
        } else {
            label.set_label("Scan");
            label.remove_css_class(state::SCANNING);
        }
    }

    if let Some(button) = state.scan_button.borrow().as_ref() {
        button.set_sensitive(snapshot.has_adapter && !snapshot.scanning);
    }

    // Update device list
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        populate_bluetooth_list(state, list_box, snapshot);
        // Apply Pango font attrs to dynamically created list rows
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}
