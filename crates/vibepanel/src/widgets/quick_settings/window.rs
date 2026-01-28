//! Quick Settings window - global control center panel.
//!
//! Each bar creates its own QuickSettingsWindow instance via the
//! QuickSettingsWindowHandle. The window is created on first open
//! and destroyed when closed, ensuring fresh state each time.

use gtk4::gdk::{self, Monitor};
use gtk4::glib::{self, ControlFlow, Propagation};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey, GestureClick, Label,
    Orientation, PolicyType, Revealer, RevealerTransitionType, ScrolledWindow,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::services::audio::AudioService;
use crate::services::bluetooth::BluetoothService;
use crate::services::brightness::BrightnessService;
use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;
use crate::services::idle_inhibitor::IdleInhibitorService;
use crate::services::network::NetworkService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::updates::UpdatesService;
use crate::services::vpn::VpnService;
use crate::styles::{class, qs, state, surface};

use super::audio_card::{
    self, AudioCardState, build_audio_details, build_audio_hint_label, build_audio_row,
};
use super::bar_widget::QuickSettingsCardsConfig;
use super::bluetooth_card::{self, BluetoothCardState, bt_icon_name, build_bluetooth_details};
use super::brightness_card::{self, BrightnessCardState, build_brightness_row};
use super::components::ToggleCard;
use super::idle_inhibitor_card::{self, IdleInhibitorCardState};
use super::mic_card::{self, MicCardState, build_mic_details, build_mic_hint_label, build_mic_row};
use super::power_card::{self, PowerCardBuildResult};
use super::ui_helpers::{AccordionManager, ExpandableCard};
use super::updates_card::{self, UpdatesCardState, build_updates_card};
use super::vpn_card::{self, VpnCardState, build_vpn_details, vpn_icon_name};
use super::wifi_card::{
    self, WifiCardState, build_network_subtitle, build_wifi_details, wifi_icon_name,
};

/// Full Quick Settings window.
///
/// A layer-shell surface with EXCLUSIVE keyboard mode, anchored below the bar
/// widget that was clicked. Uses a fullscreen click-catcher window for
/// click-outside-to-close behavior.
pub struct QuickSettingsWindow {
    window: ApplicationWindow,
    /// Fullscreen transparent click-catcher shown behind the panel.
    click_catcher: RefCell<Option<ApplicationWindow>>,
    /// Anchor X position: widget center X in monitor coordinates.
    anchor_x: Cell<i32>,
    /// Anchor Y position: widget bottom Y in monitor coordinates.
    anchor_y: Cell<i32>,
    /// Monitor the widget is on (if known).
    anchor_monitor: RefCell<Option<Monitor>>,

    /// Configuration for which cards are enabled.
    cards_config: QuickSettingsCardsConfig,

    /// Pending close timeout (for debounced focus-loss detection).
    pending_close: Cell<Option<glib::SourceId>>,

    /// Scrolled window container for height limiting.
    scroll_container: ScrolledWindow,

    // Card states (organized by panel)
    pub wifi: Rc<WifiCardState>,
    pub bluetooth: Rc<BluetoothCardState>,
    pub vpn: Rc<VpnCardState>,
    pub idle_inhibitor: Rc<IdleInhibitorCardState>,
    pub audio: Rc<AudioCardState>,
    pub mic: Rc<MicCardState>,
    pub brightness: Rc<BrightnessCardState>,
    pub updates: Rc<UpdatesCardState>,
}

impl QuickSettingsWindow {
    /// Create a new Quick Settings window bound to the given application.
    pub fn new(app: &Application, cards_config: QuickSettingsCardsConfig) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("vibepanel quick settings")
            .decorated(false)
            .resizable(false)
            .build();

        // This window is a floating control center panel.
        window.add_css_class(qs::WINDOW);

        // Layer shell configuration for overlay panel behavior.
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, false);
        window.set_anchor(Edge::Left, false);
        window.set_margin(Edge::Top, 0);
        window.set_margin(Edge::Right, 8);
        window.set_keyboard_mode(KeyboardMode::OnDemand);

        // Create scroll container for height limiting.
        // Max height will be set in update_position() based on monitor geometry.
        // propagate_natural_height allows it to grow to fit content, max_content_height caps it.
        let scroll_container = ScrolledWindow::new();
        scroll_container.set_hscrollbar_policy(PolicyType::Never);
        scroll_container.set_vscrollbar_policy(PolicyType::Automatic);
        scroll_container.set_propagate_natural_height(true);

        // Create the QuickSettingsWindow struct first (without content)
        let qs = Rc::new(Self {
            window: window.clone(),
            click_catcher: RefCell::new(None),
            anchor_x: Cell::new(0),
            anchor_y: Cell::new(0),
            anchor_monitor: RefCell::new(None),
            cards_config,
            pending_close: Cell::new(None),
            scroll_container,
            wifi: Rc::new(WifiCardState::new()),
            bluetooth: Rc::new(BluetoothCardState::new()),
            vpn: Rc::new(VpnCardState::new()),
            idle_inhibitor: Rc::new(IdleInhibitorCardState::new()),
            audio: Rc::new(AudioCardState::new()),
            mic: Rc::new(MicCardState::new()),
            brightness: Rc::new(BrightnessCardState::new()),
            updates: Rc::new(UpdatesCardState::new()),
        });

        // Build the control center content (uses qs.scroll_container internally)
        let outer = Self::build_content(&qs);
        window.set_child(Some(&outer));

        // Apply Pango font attributes to all labels if enabled in config.
        // This is the central hook for quick settings - widgets create standard
        // GTK labels, and we apply Pango attributes here after the tree is built.
        SurfaceStyleManager::global().apply_pango_attrs_all(&outer);

        // Store a back-reference on the window so callbacks can access the QuickSettingsWindow.
        // SAFETY: We own the Rc<QuickSettingsWindow> and store a Weak reference. The data lives
        // as long as the window, and we only access it via upgrade() which handles dropped refs.
        unsafe {
            qs.window
                .set_data("vibepanel-qs-window", Rc::downgrade(&qs));
        }

        // ESC key closes the panel.
        {
            let qs_weak = Rc::downgrade(&qs);
            let key_controller = EventControllerKey::new();
            key_controller.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gdk::Key::Escape {
                    if let Some(qs) = qs_weak.upgrade() {
                        qs.hide_panel();
                    }
                    Propagation::Stop
                } else {
                    Propagation::Proceed
                }
            });
            qs.window.add_controller(key_controller);
        }

        // Set up auto-close behavior based on compositor.
        //
        // Different compositors have different focus behavior for layer-shell surfaces:
        // - Niri/MangoWC: When external windows spawn, keyboard focus transfers away,
        //   causing is-active to become false. The is-active approach works well.
        // - Hyprland: Layer-shell surfaces retain keyboard focus even when other windows
        //   spawn. Moving mouse over click-catcher also triggers is-active=false.
        //   We use window-opened events instead.
        let compositor_manager = CompositorManager::global();
        let is_hyprland = compositor_manager.backend_name() == "Hyprland";

        if is_hyprland {
            // Hyprland: Subscribe to window-opened events and close when external window spawns
            let qs_weak = Rc::downgrade(&qs);
            compositor_manager.register_window_opened_callback(move |window_info| {
                let Some(qs) = qs_weak.upgrade() else {
                    return;
                };

                // Only close if QS is currently visible
                if !qs.window.is_visible() {
                    return;
                }

                // Don't close for vibepanel's own windows (class typically contains "vibepanel")
                // This handles cases where we might spawn our own dialogs
                let app_id_lower = window_info.app_id.to_lowercase();
                if app_id_lower.contains("vibepanel") {
                    return;
                }

                // External window opened - close QS panel
                qs.hide_panel();
            });
        } else {
            // Other compositors: Close panel when window loses focus (debounced to ignore
            // momentary focus changes from internal clicks). When focus moves to an external
            // window (e.g., VPN password dialog or terminal), is-active becomes false and
            // stays false. Internal clicks cause a brief false→true bounce within ~1ms.
            let qs_weak = Rc::downgrade(&qs);
            qs.window
                .connect_notify_local(Some("is-active"), move |window, _| {
                    let Some(qs) = qs_weak.upgrade() else {
                        return;
                    };

                    // Cancel any existing pending close
                    if let Some(source_id) = qs.pending_close.take() {
                        source_id.remove();
                    }

                    if !window.is_active() {
                        // Focus lost - schedule close after short delay.
                        // Will be cancelled if focus returns quickly (internal click).
                        let qs_weak = Rc::downgrade(&qs);
                        let source_id = glib::timeout_add_local_once(
                            std::time::Duration::from_millis(50),
                            move || {
                                if let Some(qs) = qs_weak.upgrade() {
                                    qs.pending_close.set(None);

                                    if !qs.window.is_active() {
                                        qs.hide_panel();
                                    }
                                }
                            },
                        );
                        qs.pending_close.set(Some(source_id));
                    }
                });
        }

        // Subscribe to services
        Self::subscribe_to_services(&qs);

        qs
    }

    /// Subscribe to all service updates.
    fn subscribe_to_services(qs: &Rc<Self>) {
        let cfg = &qs.cards_config;

        if cfg.wifi {
            let qs_weak = Rc::downgrade(qs);
            NetworkService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    wifi_card::on_network_changed(&qs.wifi, snapshot, &qs.window);
                }
            });
        }

        if cfg.bluetooth {
            let qs_weak = Rc::downgrade(qs);
            BluetoothService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    bluetooth_card::on_bluetooth_changed(&qs.bluetooth, snapshot);
                }
            });
        }

        if cfg.vpn {
            let qs_weak = Rc::downgrade(qs);
            VpnService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    vpn_card::on_vpn_changed(&qs.vpn, snapshot);
                }
            });
        }

        if cfg.idle_inhibitor {
            let qs_weak = Rc::downgrade(qs);
            IdleInhibitorService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    idle_inhibitor_card::on_idle_inhibitor_changed(&qs.idle_inhibitor, snapshot);
                }
            });
        }

        if cfg.audio {
            let qs_weak = Rc::downgrade(qs);
            AudioService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    audio_card::on_audio_changed(&qs.audio, snapshot);
                }
            });
        }

        if cfg.mic {
            let qs_weak = Rc::downgrade(qs);
            AudioService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    mic_card::on_mic_changed(&qs.mic, snapshot);
                }
            });
        }

        if cfg.brightness {
            let qs_weak = Rc::downgrade(qs);
            BrightnessService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    brightness_card::on_brightness_changed(&qs.brightness, snapshot);
                }
            });
        }

        if cfg.updates {
            let qs_weak = Rc::downgrade(qs);
            UpdatesService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    updates_card::on_updates_changed(&qs.updates, snapshot);
                }
            });
        }
    }

    /// Build the control center content.
    fn build_content(qs: &Rc<Self>) -> GtkBox {
        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.add_css_class(qs::WINDOW_CONTAINER);
        outer.add_css_class(surface::NO_FOCUS);
        outer.set_margin_top(0);
        outer.set_margin_bottom(4);
        outer.set_margin_start(4);
        outer.set_margin_end(4);

        // Apply surface styles - background now controlled via CSS variables
        outer.add_css_class("quick-settings-popover");
        SurfaceStyleManager::global().apply_surface_styles(&outer, true);

        let content = GtkBox::new(Orientation::Vertical, 0);
        content.add_css_class(qs::CONTROL_CENTER);
        content.add_css_class(surface::WIDGET_MENU_CONTENT);
        content.set_size_request(320, -1);

        let cfg = &qs.cards_config;

        // Collect toggle cards and their revealers.
        // These are the cards that appear in the 2-per-row grid.
        //
        // Cards with expandable state store a trait object for uniform accordion
        // registration. Cards that need custom expand/collapse behavior (e.g.,
        // Power card updating its subtitle) provide an on_toggle callback.
        struct ToggleCardInfo {
            card: GtkBox,
            revealer: Option<Revealer>,
            expander_button: Option<Button>,
            /// Expandable card state (if this card supports accordion behavior).
            expandable: Option<Rc<dyn ExpandableCard>>,
            /// Optional callback invoked after expand/collapse toggle.
            /// Receives `true` if expanding, `false` if collapsing.
            on_toggle: Option<Rc<dyn Fn(bool)>>,
        }

        let mut toggle_cards: Vec<ToggleCardInfo> = Vec::new();

        // Build enabled cards
        if cfg.wifi {
            let (card, revealer, expander_button) = Self::build_wifi_card(qs);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: Some(revealer),
                expander_button,
                expandable: Some(Rc::clone(&qs.wifi) as Rc<dyn ExpandableCard>),
                on_toggle: None,
            });
        }
        if cfg.bluetooth {
            let (card, revealer, expander_button) = Self::build_bluetooth_card(qs);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: Some(revealer),
                expander_button,
                expandable: Some(Rc::clone(&qs.bluetooth) as Rc<dyn ExpandableCard>),
                on_toggle: None,
            });
        }
        if cfg.vpn {
            let (card, revealer, expander_button) = Self::build_vpn_card(qs);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: Some(revealer),
                expander_button,
                expandable: Some(Rc::clone(&qs.vpn) as Rc<dyn ExpandableCard>),
                on_toggle: None,
            });
        }
        if cfg.idle_inhibitor {
            let card = Self::build_idle_inhibitor_card(qs);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: None,
                expander_button: None,
                expandable: None,
                on_toggle: None,
            });
        }
        if cfg.updates {
            let (card, revealer, expander_button) = build_updates_card(&qs.updates);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: Some(revealer),
                expander_button,
                expandable: Some(Rc::clone(&qs.updates) as Rc<dyn ExpandableCard>),
                on_toggle: None,
            });
        }
        // Power card (always last in the grid)
        if cfg.power {
            match power_card::build_power_card() {
                PowerCardBuildResult::Popover { card, state: _ } => {
                    toggle_cards.push(ToggleCardInfo {
                        card,
                        revealer: None,
                        expander_button: None,
                        expandable: None,
                        on_toggle: None,
                    });
                }
                PowerCardBuildResult::Expander {
                    card,
                    revealer,
                    state,
                    expander_button,
                } => {
                    // Power card needs custom subtitle behavior on expand/collapse.
                    // Capture state and borrow inside callback to handle cases where
                    // subtitle might be set after callback creation.
                    let state_clone = Rc::clone(&state);
                    toggle_cards.push(ToggleCardInfo {
                        card,
                        revealer: Some(revealer),
                        expander_button,
                        expandable: Some(state as Rc<dyn ExpandableCard>),
                        on_toggle: Some(Rc::new(move |expanding| {
                            if let Some(ref subtitle) = *state_clone.base.subtitle.borrow() {
                                subtitle.set_label(if expanding {
                                    "Hold to confirm"
                                } else {
                                    "Hold to shutdown"
                                });
                            }
                        })),
                    });
                }
            }
        }

        // Build rows dynamically with per-row accordion managers
        let mut is_first_row = true;
        for chunk in toggle_cards.chunks(2) {
            let row = GtkBox::new(Orientation::Horizontal, 8);
            row.add_css_class(qs::CARDS_ROW);
            row.set_homogeneous(true);
            if !is_first_row {
                row.set_margin_top(8);
            }
            is_first_row = false;

            // Create per-row accordion manager.
            // Note: row_accordion is not stored in a struct field, but it stays alive
            // because setup_expander_with_callback captures Rc<AccordionManager> in GTK
            // signal closures, which are prevent it from being dropped while the buttons exist.
            let row_accordion = Rc::new(AccordionManager::new());

            for tc in chunk {
                row.append(&tc.card);

                // Register expandable cards with this row's accordion
                if let (Some(expander_btn), Some(expandable)) =
                    (&tc.expander_button, &tc.expandable)
                {
                    row_accordion.register_dyn(Rc::clone(expandable));
                    AccordionManager::setup_expander_with_callback(
                        &row_accordion,
                        expandable,
                        expander_btn,
                        tc.on_toggle.clone(),
                    );
                }
            }

            // If odd number of cards in this row, add placeholder for consistent sizing
            if chunk.len() == 1 {
                let placeholder = GtkBox::new(Orientation::Horizontal, 0);
                row.append(&placeholder);
            }

            content.append(&row);

            // Add revealers after the row (they expand below the cards)
            for tc in chunk {
                if let Some(ref revealer) = tc.revealer {
                    content.append(revealer);
                }
            }
        }

        if cfg.audio {
            let (audio_row, audio_revealer, audio_hint_label) = Self::build_audio_section(qs);
            audio_row.set_margin_top(12);
            content.append(&audio_row);
            content.append(&audio_hint_label);
            content.append(&audio_revealer);
        }

        if cfg.mic {
            let (mic_row, mic_revealer, mic_hint_label) = Self::build_mic_section(qs);
            content.append(&mic_row);
            content.append(&mic_hint_label);
            content.append(&mic_revealer);
        }

        if cfg.brightness && BrightnessService::global().current().available {
            let brightness_row = Self::build_brightness_section(qs);
            content.append(&brightness_row);
        }

        // Wrap content in the scroll container for height limiting
        qs.scroll_container.set_child(Some(&content));
        outer.append(&qs.scroll_container);
        outer
    }

    /// Build the Wi-Fi card and its revealer.
    ///
    /// Returns `(card, revealer, expander_button)` - caller is responsible for
    /// accordion registration via `AccordionManager::setup_expander`.
    fn build_wifi_card(qs: &Rc<Self>) -> (GtkBox, Revealer, Option<Button>) {
        let network = NetworkService::global();
        let snapshot = network.snapshot();

        let wifi_enabled = snapshot.wifi_enabled.unwrap_or(false);
        let wifi_connected = snapshot.connected;
        let wired_connected = snapshot.wired_connected;
        let has_wifi_device = snapshot.has_wifi_device;

        // Build custom subtitle widget with connection status icons
        let subtitle_result = build_network_subtitle(&snapshot);

        let icon_name = wifi_icon_name(
            wifi_connected,
            wifi_enabled,
            wired_connected,
            has_wifi_device,
        );
        let icon_active = (wifi_enabled && wifi_connected) || wired_connected;

        // Card title: "Network" if ethernet device exists, "Wi-Fi" otherwise
        let card_title = if snapshot.has_ethernet_device {
            "Network"
        } else {
            "Wi-Fi"
        };

        let wifi_card = ToggleCard::builder()
            .icon(icon_name)
            .label(card_title)
            .subtitle_widget(subtitle_result.container.upcast())
            .active(wifi_enabled)
            .sensitive(true)
            .icon_active(icon_active)
            .with_expander(true)
            .build();

        // Add card identifier for CSS targeting
        wifi_card.card.add_css_class(qs::WIFI);

        // Disable toggle if no Wi-Fi device (toggle controls Wi-Fi, not ethernet)
        if !snapshot.has_wifi_device {
            wifi_card.toggle.set_sensitive(false);
        }

        if !wifi_enabled && !wired_connected {
            wifi_card
                .icon_handle
                .widget()
                .add_css_class(qs::WIFI_DISABLED_ICON);
        }

        {
            let toggle = wifi_card.toggle.clone();
            let wifi_state = Rc::clone(&qs.wifi);
            toggle.connect_toggled(move |toggle| {
                // Skip if this is a programmatic update (prevents feedback loops)
                if wifi_state.updating_toggle.get() {
                    return;
                }
                NetworkService::global().set_wifi_enabled(toggle.is_active());
            });
        }

        // Store references (use base fields)
        *qs.wifi.base.toggle.borrow_mut() = Some(wifi_card.toggle.clone());
        *qs.wifi.base.card_icon.borrow_mut() = Some(wifi_card.icon_handle.clone());
        *qs.wifi.base.arrow.borrow_mut() = wifi_card.expander_icon.clone();

        // Store title label for dynamic updates
        *qs.wifi.title_label.borrow_mut() = Some(wifi_card.title.clone());

        // Store subtitle label reference
        *qs.wifi.subtitle_label.borrow_mut() = Some(subtitle_result.label);

        // Build revealer
        let wifi_revealer = Revealer::new();
        wifi_revealer.set_reveal_child(false);
        wifi_revealer.set_transition_type(RevealerTransitionType::SlideDown);

        let wifi_state = Rc::clone(&qs.wifi);
        let wifi_details = build_wifi_details(&wifi_state, qs.window.downgrade());
        wifi_revealer.set_child(Some(&wifi_details.container));

        *qs.wifi.base.list_box.borrow_mut() = Some(wifi_details.list_box);
        *qs.wifi.base.revealer.borrow_mut() = Some(wifi_revealer.clone());
        *qs.wifi.scan_button.borrow_mut() = Some(wifi_details.scan_button);

        // Connect Wi-Fi switch to toggle Wi-Fi enabled state
        {
            let wifi_state = Rc::clone(&qs.wifi);
            wifi_details
                .wifi_switch
                .connect_state_set(move |_, enabled| {
                    // Skip if this is a programmatic update (prevents feedback loops)
                    if wifi_state.updating_toggle.get() {
                        return glib::Propagation::Proceed;
                    }
                    NetworkService::global().set_wifi_enabled(enabled);
                    glib::Propagation::Proceed
                });
        }

        (wifi_card.card, wifi_revealer, wifi_card.expander_button)
    }

    /// Build the Bluetooth card and its revealer.
    ///
    /// Returns `(card, revealer, expander_button)` - caller is responsible for
    /// accordion registration via `AccordionManager::setup_expander`.
    fn build_bluetooth_card(qs: &Rc<Self>) -> (GtkBox, Revealer, Option<Button>) {
        let bt_service = BluetoothService::global();
        let bt_snapshot = bt_service.snapshot();

        let bt_powered = bt_snapshot.powered;
        let bt_has_adapter = bt_snapshot.has_adapter;
        let bt_connected = bt_snapshot.connected_devices;

        let bt_subtitle_text = if !bt_has_adapter {
            "Unavailable".to_string()
        } else if !bt_snapshot.is_ready {
            "Bluetooth".to_string()
        } else if bt_connected > 0 {
            if bt_connected == 1 {
                bt_snapshot
                    .devices
                    .iter()
                    .find(|d| d.connected)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "Bluetooth".to_string())
            } else {
                format!("{} connected", bt_connected)
            }
        } else if bt_powered {
            "Enabled".to_string()
        } else {
            "Disabled".to_string()
        };

        let bt_icon_name = bt_icon_name(bt_powered, bt_connected);
        let bt_icon_active = bt_connected > 0;

        let bt_card = ToggleCard::builder()
            .icon(bt_icon_name)
            .label("Bluetooth")
            .subtitle(&bt_subtitle_text)
            .active(bt_powered && bt_has_adapter)
            .sensitive(bt_has_adapter)
            .icon_active(bt_icon_active)
            .with_expander(true)
            .build();

        // Add card identifier for CSS targeting
        bt_card.card.add_css_class(qs::BLUETOOTH);

        // Apply disabled styling when Bluetooth is off
        if !bt_powered {
            bt_card.icon_handle.add_css_class(qs::BT_DISABLED_ICON);
        }

        {
            let toggle = bt_card.toggle.clone();
            let bt_state = Rc::clone(&qs.bluetooth);
            toggle.connect_toggled(move |toggle| {
                // Skip if this is a programmatic update (prevents feedback loops)
                if bt_state.updating_toggle.get() {
                    return;
                }
                BluetoothService::global().set_powered(toggle.is_active());
            });
        }

        // Store references (use base fields)
        *qs.bluetooth.base.toggle.borrow_mut() = Some(bt_card.toggle.clone());
        *qs.bluetooth.base.card_icon.borrow_mut() = Some(bt_card.icon_handle.clone());
        *qs.bluetooth.base.subtitle.borrow_mut() = bt_card.subtitle.clone();
        *qs.bluetooth.base.arrow.borrow_mut() = bt_card.expander_icon.clone();

        // Build revealer
        let bt_revealer = Revealer::new();
        bt_revealer.set_reveal_child(false);
        bt_revealer.set_transition_type(RevealerTransitionType::SlideDown);

        let bt_state = Rc::clone(&qs.bluetooth);
        let bt_details = build_bluetooth_details(&bt_state);
        bt_revealer.set_child(Some(&bt_details.container));

        *qs.bluetooth.base.list_box.borrow_mut() = Some(bt_details.list_box);
        *qs.bluetooth.base.revealer.borrow_mut() = Some(bt_revealer.clone());
        *qs.bluetooth.scan_button.borrow_mut() = Some(bt_details.scan_button);

        (bt_card.card, bt_revealer, bt_card.expander_button)
    }

    /// Build the VPN card and its revealer.
    ///
    /// Returns `(card, revealer, expander_button)` - caller is responsible for
    /// accordion registration via `AccordionManager::setup_expander`.
    fn build_vpn_card(qs: &Rc<Self>) -> (GtkBox, Revealer, Option<Button>) {
        let vpn_service = VpnService::global();
        let vpn_snapshot = vpn_service.snapshot();

        let vpn_primary = vpn_snapshot.primary();
        let vpn_has_connections = !vpn_snapshot.connections.is_empty();
        let vpn_any_active = vpn_snapshot.any_active;

        let vpn_subtitle_text = if !vpn_snapshot.is_ready {
            "VPN".to_string()
        } else if let Some(p) = vpn_primary {
            if p.active {
                p.name.clone()
            } else {
                "Disconnected".to_string()
            }
        } else {
            "No connections".to_string()
        };

        let vpn_icon = vpn_icon_name(vpn_any_active);
        let vpn_icon_active = vpn_any_active;

        let vpn_card = ToggleCard::builder()
            .icon(vpn_icon)
            .label("VPN")
            .subtitle(&vpn_subtitle_text)
            .active(vpn_primary.map(|p| p.active).unwrap_or(false))
            .sensitive(vpn_has_connections)
            .icon_active(vpn_icon_active)
            .with_expander(true)
            .build();

        // Add card identifier for CSS targeting
        vpn_card.card.add_css_class(qs::VPN);

        {
            let toggle = vpn_card.toggle.clone();
            let vpn_state = Rc::clone(&qs.vpn);
            toggle.connect_toggled(move |toggle| {
                // Skip if this is a programmatic update (prevents feedback loops)
                if vpn_state.updating_toggle.get() {
                    return;
                }
                let vpn = VpnService::global();
                let snapshot = vpn.snapshot();
                if let Some(primary) = snapshot.primary() {
                    vpn.set_connection_state(&primary.uuid, toggle.is_active());
                }
            });
        }

        // Store references (use base fields)
        *qs.vpn.base.toggle.borrow_mut() = Some(vpn_card.toggle.clone());
        *qs.vpn.base.card_icon.borrow_mut() = Some(vpn_card.icon_handle.clone());
        *qs.vpn.base.subtitle.borrow_mut() = vpn_card.subtitle.clone();
        *qs.vpn.base.arrow.borrow_mut() = vpn_card.expander_icon.clone();

        // Build revealer
        let vpn_revealer = Revealer::new();
        vpn_revealer.set_reveal_child(false);
        vpn_revealer.set_transition_type(RevealerTransitionType::SlideDown);

        let vpn_state = Rc::clone(&qs.vpn);
        let vpn_details = build_vpn_details(&vpn_state);
        vpn_revealer.set_child(Some(&vpn_details.container));

        *qs.vpn.base.list_box.borrow_mut() = Some(vpn_details.list_box);
        *qs.vpn.base.revealer.borrow_mut() = Some(vpn_revealer.clone());

        (vpn_card.card, vpn_revealer, vpn_card.expander_button)
    }

    /// Build the Idle Inhibitor card (no revealer needed).
    fn build_idle_inhibitor_card(qs: &Rc<Self>) -> GtkBox {
        let idle_service = IdleInhibitorService::global();
        let idle_snapshot = idle_service.snapshot();

        let idle_active = idle_snapshot.active;
        let idle_available = idle_snapshot.available;

        let idle_subtitle_text = if idle_active {
            "Enabled".to_string()
        } else {
            "Disabled".to_string()
        };

        let idle_card = ToggleCard::builder()
            .icon("night-light-symbolic")
            .label("Idle Inhibitor")
            .subtitle(&idle_subtitle_text)
            .active(idle_active)
            .sensitive(idle_available)
            .icon_active(idle_active)
            .with_expander(false)
            .build();

        // Add card identifier for CSS targeting
        idle_card.card.add_css_class(qs::IDLE_INHIBITOR);

        {
            let toggle = idle_card.toggle.clone();
            toggle.connect_toggled(move |toggle| {
                IdleInhibitorService::global().set_active(toggle.is_active());
            });
        }

        // Store references
        *qs.idle_inhibitor.toggle.borrow_mut() = Some(idle_card.toggle.clone());
        *qs.idle_inhibitor.card_icon.borrow_mut() = Some(idle_card.icon_handle.clone());
        *qs.idle_inhibitor.subtitle.borrow_mut() = idle_card.subtitle.clone();

        idle_card.card
    }

    /// Build the audio section (row, revealer, hint label).
    fn build_audio_section(qs: &Rc<Self>) -> (GtkBox, Revealer, Label) {
        let audio_widgets = build_audio_row();
        let audio_details = build_audio_details();
        let audio_hint_label = build_audio_hint_label();

        // Add row identifier for CSS targeting
        audio_widgets.row.add_css_class(qs::AUDIO_OUTPUT);

        // Get initial audio state
        let audio_service = AudioService::global();
        let audio_snapshot = audio_service.current();

        audio_widgets.slider.set_value(audio_snapshot.volume as f64);

        let vol_icon = audio_card::volume_icon_name(audio_snapshot.volume, audio_snapshot.muted);
        audio_widgets.icon_handle.set_icon(vol_icon);

        // Set initial muted class
        if audio_snapshot.muted {
            audio_widgets
                .icon_handle
                .widget()
                .add_css_class(state::MUTED);
        }

        // Connect mute button
        {
            let mute_button = audio_widgets.mute_button.clone();
            mute_button.connect_clicked(move |_| {
                AudioService::global().toggle_mute();
            });
        }

        // Connect volume slider
        {
            let qs_weak = Rc::downgrade(qs);
            let slider = audio_widgets.slider.clone();
            slider.connect_value_changed(move |slider| {
                if let Some(qs) = qs_weak.upgrade()
                    && !qs.audio.updating.get()
                {
                    AudioService::global().set_volume(slider.value() as u32);
                }
            });
        }

        // Connect sink list row activation
        {
            audio_details.list_box.connect_row_activated(move |_, row| {
                audio_card::on_audio_sink_row_activated(row);
            });
        }

        // Populate initial sink list
        audio_card::populate_audio_sink_list(&audio_details.list_box, &audio_snapshot);

        // Check initial control availability
        let control_ok = audio_snapshot.available && audio_snapshot.control_available;
        audio_widgets.slider.set_sensitive(control_ok);
        audio_widgets.mute_button.set_sensitive(control_ok);
        if !control_ok {
            audio_widgets.row.add_css_class(qs::AUDIO_ROW_DISABLED);
        }
        audio_hint_label.set_visible(audio_snapshot.available && !audio_snapshot.control_available);

        // Store references
        *qs.audio.mute_button.borrow_mut() = Some(audio_widgets.mute_button.clone());
        *qs.audio.icon_handle.borrow_mut() = Some(audio_widgets.icon_handle.clone());
        *qs.audio.slider.borrow_mut() = Some(audio_widgets.slider.clone());
        *qs.audio.arrow.borrow_mut() = Some(audio_widgets.arrow_handle.clone());
        *qs.audio.revealer.borrow_mut() = Some(audio_details.revealer.clone());
        *qs.audio.list_box.borrow_mut() = Some(audio_details.list_box.clone());
        *qs.audio.row.borrow_mut() = Some(audio_widgets.row.clone());
        *qs.audio.hint_label.borrow_mut() = Some(audio_hint_label.clone());

        // Wire up expander button for audio sink list
        {
            let revealer = audio_details.revealer.clone();
            let arrow = audio_widgets.arrow_handle.clone();
            audio_widgets.expander_button.connect_clicked(move |_| {
                let expanding = !revealer.reveals_child();
                revealer.set_reveal_child(expanding);
                if expanding {
                    arrow.widget().add_css_class(state::EXPANDED);
                } else {
                    arrow.widget().remove_css_class(state::EXPANDED);
                }
            });
        }

        (audio_widgets.row, audio_details.revealer, audio_hint_label)
    }

    /// Build the mic section (row, revealer, hint label).
    fn build_mic_section(qs: &Rc<Self>) -> (GtkBox, Revealer, Label) {
        let mic_widgets = build_mic_row();
        let mic_details = build_mic_details();
        let mic_hint_label = build_mic_hint_label();

        // Add row identifier for CSS targeting
        mic_widgets.row.add_css_class(qs::AUDIO_MIC);

        // Get initial audio state (mic info comes from AudioService)
        let audio_service = AudioService::global();
        let audio_snapshot = audio_service.current();

        let mic_volume = audio_snapshot.mic_volume.unwrap_or(0);
        let mic_muted = audio_snapshot.mic_muted.unwrap_or(false);

        mic_widgets.slider.set_value(mic_volume as f64);

        let mic_icon = mic_card::mic_icon_name(mic_volume, mic_muted);
        mic_widgets.icon_handle.set_icon(mic_icon);

        // Set initial muted class
        if mic_muted {
            mic_widgets.icon_handle.widget().add_css_class(state::MUTED);
        }

        // Connect mute button
        {
            let mute_button = mic_widgets.mute_button.clone();
            mute_button.connect_clicked(move |_| {
                AudioService::global().toggle_mic_mute();
            });
        }

        // Connect mic volume slider
        {
            let qs_weak = Rc::downgrade(qs);
            let slider = mic_widgets.slider.clone();
            slider.connect_value_changed(move |slider| {
                if let Some(qs) = qs_weak.upgrade()
                    && !qs.mic.updating.get()
                {
                    AudioService::global().set_mic_volume(slider.value() as u32);
                }
            });
        }

        // Connect source list row activation
        {
            mic_details.list_box.connect_row_activated(move |_, row| {
                let audio_service = AudioService::global();
                let snapshot = audio_service.current();
                mic_card::on_mic_source_row_activated(row, &snapshot.sources);
            });
        }

        // Populate initial source list
        mic_card::populate_mic_source_list(&mic_details.list_box, &audio_snapshot.sources);

        // Check initial control availability
        let control_ok = audio_snapshot.available && audio_snapshot.mic_control_available;
        mic_widgets.slider.set_sensitive(control_ok);
        mic_widgets.mute_button.set_sensitive(control_ok);
        if !control_ok {
            mic_widgets.row.add_css_class(qs::AUDIO_ROW_DISABLED);
        }
        mic_hint_label
            .set_visible(audio_snapshot.available && !audio_snapshot.mic_control_available);

        // Store references
        *qs.mic.mute_button.borrow_mut() = Some(mic_widgets.mute_button.clone());
        *qs.mic.icon_handle.borrow_mut() = Some(mic_widgets.icon_handle.clone());
        *qs.mic.slider.borrow_mut() = Some(mic_widgets.slider.clone());
        *qs.mic.arrow.borrow_mut() = Some(mic_widgets.arrow_handle.clone());
        *qs.mic.revealer.borrow_mut() = Some(mic_details.revealer.clone());
        *qs.mic.list_box.borrow_mut() = Some(mic_details.list_box.clone());
        *qs.mic.row.borrow_mut() = Some(mic_widgets.row.clone());
        *qs.mic.hint_label.borrow_mut() = Some(mic_hint_label.clone());

        // Wire up expander button for mic source list
        {
            let revealer = mic_details.revealer.clone();
            let arrow = mic_widgets.arrow_handle.clone();
            mic_widgets.expander_button.connect_clicked(move |_| {
                let expanding = !revealer.reveals_child();
                revealer.set_reveal_child(expanding);
                if expanding {
                    arrow.widget().add_css_class(state::EXPANDED);
                } else {
                    arrow.widget().remove_css_class(state::EXPANDED);
                }
            });
        }

        (mic_widgets.row, mic_details.revealer, mic_hint_label)
    }

    /// Build the brightness section.
    fn build_brightness_section(qs: &Rc<Self>) -> GtkBox {
        let brightness_widgets = build_brightness_row();

        // Get initial brightness state
        let brightness_service = BrightnessService::global();
        let brightness_snapshot = brightness_service.current();

        if brightness_snapshot.available {
            brightness_widgets
                .slider
                .set_value(brightness_snapshot.percent as f64);
        }
        brightness_widgets
            .row
            .set_sensitive(brightness_snapshot.available);

        // Connect brightness slider
        {
            let qs_weak = Rc::downgrade(qs);
            let slider = brightness_widgets.slider.clone();
            slider.connect_value_changed(move |slider| {
                if let Some(qs) = qs_weak.upgrade()
                    && !qs.brightness.updating.get()
                {
                    BrightnessService::global().set_brightness(slider.value() as u32);
                }
            });
        }

        // Store references
        *qs.brightness.slider.borrow_mut() = Some(brightness_widgets.slider.clone());
        *qs.brightness.icon_handle.borrow_mut() = Some(brightness_widgets.icon_handle.clone());

        brightness_widgets.row
    }

    /// Show inline Wi-Fi password dialog for the given SSID.
    pub fn show_wifi_password_dialog(&self, ssid: &str) {
        wifi_card::show_password_dialog(&self.wifi, ssid);
    }

    // Position and visibility management

    /// Set the anchor position for the window.
    pub fn set_anchor_position(&self, x: i32, y: i32, monitor: Option<Monitor>) {
        self.anchor_x.set(x);
        self.anchor_y.set(y);
        *self.anchor_monitor.borrow_mut() = monitor;
    }

    /// Update window margins based on the current anchor position.
    fn update_position(&self) {
        let anchor_x = self.anchor_x.get();
        let anchor_y = self.anchor_y.get();

        let mut monitor_opt = self.anchor_monitor.borrow().clone();
        if monitor_opt.is_none()
            && let Some(display) = gdk::Display::default()
        {
            let monitors = display.monitors();
            if let Some(obj) = monitors.item(0)
                && let Ok(monitor) = obj.downcast::<Monitor>()
            {
                monitor_opt = Some(monitor);
            }
        }

        let Some(monitor) = monitor_opt else {
            return;
        };

        let geom = monitor.geometry();

        // Get bar dimensions from config
        let config_mgr = ConfigManager::global();
        let bar_size = config_mgr.bar_size() as i32;
        let screen_margin = config_mgr.screen_margin() as i32;
        let popover_offset = config_mgr.popover_offset() as i32;

        // Calculate and apply top margin
        let top_margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);
        self.window.set_margin(Edge::Top, top_margin);

        // Calculate max height for scroll container based on available screen space.
        // geom.height() is the full screen height, but QS is positioned relative to
        // the work area (after the bar's exclusive zone). Account for:
        // - bar_exclusive_zone: bar_size + 2*screen_margin + popover_offset
        // - container_padding: outer container has padding (16px from surface styles)
        //   and margin (4px margin_bottom), plus CSS padding-top (4px)
        // - bottom_margin: desired gap from screen bottom
        let bottom_margin = 8;
        let container_padding = 16 + 4 + 4; // surface padding-bottom + margin_bottom + padding-top
        let bar_exclusive_zone = bar_size + 2 * screen_margin + popover_offset;
        let max_height =
            geom.height() - bar_exclusive_zone - top_margin - container_padding - bottom_margin;

        if max_height > 100 {
            self.scroll_container.set_max_content_height(max_height);
        }

        if anchor_x > 0 {
            let monitor_width = geom.width();
            // Use actual width if available, otherwise estimate based on content width (320px)
            // plus margins/padding (~8px on each side)
            let window_width = {
                let w = self.window.width();
                if w > 20 { w } else { 336 }
            };
            let right_margin = monitor_width - anchor_x - window_width / 2;
            let max_margin = monitor_width.saturating_sub(window_width + 4);
            // Ensure min <= max to avoid clamp panic
            let clamped = if max_margin >= 4 {
                right_margin.clamp(4, max_margin)
            } else {
                // Window is too wide for monitor, just use minimum margin
                4.max(max_margin)
            };
            self.window.set_margin(Edge::Right, clamped);
        } else {
            self.window.set_margin(Edge::Right, 8);
        }
    }

    /// Show the panel and associated click-catcher.
    fn show_panel(&self) {
        if let Some(monitor) = self.anchor_monitor.borrow().as_ref() {
            self.window.set_monitor(Some(monitor));
        }

        // Create and show click-catcher
        let catcher = self.create_click_catcher();
        if let Some(monitor) = self.anchor_monitor.borrow().as_ref() {
            catcher.set_monitor(Some(monitor));
        }
        catcher.set_visible(true);
        *self.click_catcher.borrow_mut() = Some(catcher);

        // Start with opacity 0 to avoid flicker while positioning
        self.window.set_opacity(0.0);
        self.window.set_visible(true);
        self.window.present();

        // After the window is mapped and has its real size, update position and fade in
        let window_weak = self.window.downgrade();
        glib::idle_add_local(move || {
            if let Some(window) = window_weak.upgrade() {
                // SAFETY: We stored Weak<QuickSettingsWindow> at window creation with this key.
                // upgrade() safely returns None if the QuickSettingsWindow was dropped.
                unsafe {
                    if let Some(weak_ptr) =
                        window.data::<Weak<QuickSettingsWindow>>("vibepanel-qs-window")
                        && let Some(qs) = weak_ptr.as_ref().upgrade()
                    {
                        qs.update_position();
                        qs.window.set_opacity(1.0);
                    }
                }
            }
            ControlFlow::Break
        });
    }

    /// Hide and destroy the panel and associated click-catcher.
    ///
    /// This closes and destroys both windows, ensuring fresh state on next open.
    fn hide_panel(&self) {
        // Cancel any pending focus-loss close
        if let Some(source_id) = self.pending_close.take() {
            source_id.remove();
        }

        // Destroy click-catcher
        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
        }

        // Close the main window
        self.window.close();
    }

    /// Create the fullscreen click-catcher window.
    fn create_click_catcher(&self) -> ApplicationWindow {
        let app_opt = self.window.application();
        let app = app_opt
            .as_ref()
            .expect("QuickSettingsWindow must have an associated Application");

        let catcher = ApplicationWindow::builder()
            .application(app)
            .title("vibepanel quick settings click catcher")
            .decorated(false)
            .build();

        catcher.add_css_class(qs::CLICK_CATCHER);
        catcher.add_css_class(class::CLICK_CATCHER);

        catcher.init_layer_shell();
        catcher.set_layer(Layer::Overlay);
        catcher.set_exclusive_zone(-1);
        catcher.set_anchor(Edge::Top, true);
        catcher.set_anchor(Edge::Bottom, true);
        catcher.set_anchor(Edge::Left, true);
        catcher.set_anchor(Edge::Right, true);
        catcher.set_keyboard_mode(KeyboardMode::OnDemand);

        let overlay = GtkBox::new(Orientation::Vertical, 0);
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        catcher.set_child(Some(&overlay));

        let gesture = GestureClick::new();
        gesture.set_button(0);
        {
            let qs_weak = self.window.downgrade();
            // Use connect_released instead of connect_pressed to allow GTK to complete
            // the gesture lifecycle before hiding windows. Using connect_pressed causes
            // "Broken accounting of active state" warnings on some systems because the
            // gesture is interrupted mid-action when windows are hidden.
            gesture.connect_released(move |_, _, _, _| {
                if let Some(window) = qs_weak.upgrade() {
                    // SAFETY: We stored Weak<QuickSettingsWindow> at window creation.
                    // upgrade() safely returns None if dropped.
                    unsafe {
                        if let Some(weak_ptr) =
                            window.data::<Weak<QuickSettingsWindow>>("vibepanel-qs-window")
                            && let Some(qs) = weak_ptr.as_ref().upgrade()
                        {
                            qs.hide_panel();
                        }
                    }
                }
            });
        }
        catcher.add_controller(gesture);

        // ESC key closes the panel (needed for Hyprland where keyboard focus
        // may transfer to click-catcher when mouse moves over it)
        {
            let qs_weak = self.window.downgrade();
            let key_controller = EventControllerKey::new();
            key_controller.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gdk::Key::Escape {
                    if let Some(window) = qs_weak.upgrade() {
                        // SAFETY: We stored Weak<QuickSettingsWindow> at window creation.
                        // upgrade() safely returns None if dropped.
                        unsafe {
                            if let Some(weak_ptr) =
                                window.data::<Weak<QuickSettingsWindow>>("vibepanel-qs-window")
                                && let Some(qs) = weak_ptr.as_ref().upgrade()
                            {
                                qs.hide_panel();
                            }
                        }
                    }
                    Propagation::Stop
                } else {
                    Propagation::Proceed
                }
            });
            catcher.add_controller(key_controller);
        }

        catcher
    }
}

/// Handle passed to bar widgets so they can toggle the Quick Settings window.
///
/// The handle manages the window lifecycle: creating a fresh window on open
/// and destroying it on close. This ensures the window always starts with
/// fresh state (no remembered scroll positions, expanded sections, etc.).
#[derive(Clone)]
pub struct QuickSettingsWindowHandle {
    app: Application,
    cards_config: QuickSettingsCardsConfig,
    /// The current window instance (if open). Shared across clones.
    window: Rc<RefCell<Option<Rc<QuickSettingsWindow>>>>,
}

impl QuickSettingsWindowHandle {
    pub fn new(app: Application, cards_config: QuickSettingsCardsConfig) -> Self {
        Self {
            app,
            cards_config,
            window: Rc::new(RefCell::new(None)),
        }
    }

    pub fn toggle_at(&self, x: i32, y: i32, monitor: Option<Monitor>) {
        // Check if window exists and is visible, extracting it to avoid holding borrow
        // across GTK operations that might trigger callbacks.
        let existing_window = {
            let borrowed = self.window.borrow();
            if borrowed.as_ref().is_some_and(|w| w.window.is_visible()) {
                borrowed.clone()
            } else {
                None
            }
        };

        // If window exists and is visible, close it
        if let Some(window) = existing_window {
            // Clear our reference first, before hide_panel() triggers GTK signals
            *self.window.borrow_mut() = None;
            window.hide_panel();
            return;
        }

        // Window doesn't exist or was closed externally - create fresh one
        let qs = QuickSettingsWindow::new(&self.app, self.cards_config.clone());
        qs.set_anchor_position(x, y, monitor);
        qs.show_panel();
        *self.window.borrow_mut() = Some(qs);
    }
}

/// Calculate the top margin for the QS window given positioning parameters.
///
/// This is the pure math extracted from `update_position` for testing.
///
/// # Arguments
/// * `anchor_y` - Widget bottom Y from bar_widget (includes screen_margin adjustment)
/// * `bar_size` - Bar height in pixels
/// * `screen_margin` - Bar's screen margin from config
/// * `popover_offset` - Desired gap between bar and popover from config
///
/// # Returns
/// The layer-shell top margin to apply to the QS window (clamped to >= 0)
fn calculate_qs_top_margin(
    anchor_y: i32,
    bar_size: i32,
    screen_margin: i32,
    popover_offset: i32,
) -> i32 {
    // The bar occupies: screen_margin (top) + bar_size + screen_margin (bottom padding)
    let bar_total_height = bar_size + 2 * screen_margin;

    // anchor_y from bar_widget already includes screen_margin offset
    // Subtract bar_total_height to get actual screen position, then add popover_offset
    let top_margin = anchor_y - bar_total_height + popover_offset;
    top_margin.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qs_top_margin_typical_config() {
        // Typical config: bar_size=32, screen_margin=4, popover_offset=1
        // anchor_y from bar_widget = (widget_bottom_in_window) + screen_margin
        // Widget bottom in window ≈ screen_margin + bar_size = 4 + 32 = 36
        // So anchor_y ≈ 36 + 4 = 40

        let anchor_y = 40;
        let bar_size = 32;
        let screen_margin = 4;
        let popover_offset = 1;

        let margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);

        // bar_total_height = 32 + 8 = 40
        // top_margin = 40 - 40 + 1 = 1
        assert_eq!(margin, 1);
    }

    #[test]
    fn test_qs_top_margin_larger_offset() {
        let anchor_y = 40;
        let bar_size = 32;
        let screen_margin = 4;
        let popover_offset = 8;

        let margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);

        // top_margin = 40 - 40 + 8 = 8
        assert_eq!(margin, 8);
    }

    #[test]
    fn test_qs_top_margin_zero_offset() {
        let anchor_y = 40;
        let bar_size = 32;
        let screen_margin = 4;
        let popover_offset = 0;

        let margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);

        // top_margin = 40 - 40 + 0 = 0
        assert_eq!(margin, 0);
    }

    #[test]
    fn test_qs_top_margin_clamps_negative() {
        // Edge case: if anchor_y is unexpectedly small
        let anchor_y = 30; // Less than bar_total_height
        let bar_size = 32;
        let screen_margin = 4;
        let popover_offset = 1;

        let margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);

        // top_margin = 30 - 40 + 1 = -9, clamped to 0
        assert_eq!(margin, 0);
    }

    #[test]
    fn test_qs_top_margin_no_screen_margin() {
        // Config with no screen margin
        let anchor_y = 32; // Just bar_size
        let bar_size = 32;
        let screen_margin = 0;
        let popover_offset = 4;

        let margin = calculate_qs_top_margin(anchor_y, bar_size, screen_margin, popover_offset);

        // bar_total_height = 32 + 0 = 32
        // top_margin = 32 - 32 + 4 = 4
        assert_eq!(margin, 4);
    }
}
