//! Quick Settings window - global control center panel.
//!
//! Each bar creates its own QuickSettingsWindow instance via the
//! QuickSettingsWindowHandle. The window is lazily created on first open
//! and kept alive across close/re-open cycles using `set_visible(false)`
//! / `set_visible(true)` toggling. Service subscriptions stay alive
//! while hidden. UI state is reset on close so it opens fresh.

use gtk4::gdk::{self, Monitor};
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey, Label, Orientation,
    PolicyType, Revealer, RevealerTransitionType, ScrolledWindow,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::popover_tracker::{PopoverId, PopoverTracker};
use crate::services::audio::AudioService;
use crate::services::bluetooth::BluetoothService;
use crate::services::brightness::BrightnessService;
use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::idle_inhibitor::IdleInhibitorService;
use crate::services::network::NetworkService;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::updates::UpdatesService;
use crate::services::vpn::VpnService;
use crate::styles::{qs, state, surface};
use crate::widgets::layer_shell_popover::{
    ANIM_SCALE_FROM, AnimDirection, AnimState, Dismissible, calculate_bar_exclusive_zone,
    calculate_popover_bar_margin, calculate_popover_right_margin, create_click_catcher,
    is_keynav_key, popover_bar_edge, popover_keyboard_mode, setup_esc_handler,
};
use crate::widgets::scale_box::ScaleBox;

use super::audio_card::{
    self, AudioCardState, build_audio_details, build_audio_hint_label, build_audio_row,
};
use super::bar_widget::{QuickSettingsCardsConfig, QuickSettingsConfig};
use super::bluetooth_card::{self, BluetoothCardState, bt_icon_name, build_bluetooth_details};
use super::brightness_card::{self, BrightnessCardState, build_brightness_row};
use super::components::ToggleCard;
use super::idle_inhibitor_card::{self, IdleInhibitorCardState};
use super::mic_card::{self, MicCardState, build_mic_details, build_mic_hint_label, build_mic_row};
use super::network_card::{
    self, NetworkCardState, build_network_subtitle, build_wifi_details, is_material_unified,
    resolve_material_network_icon,
};
use super::power_card::{self, PowerCardBuildResult, PowerCardExpanderState};
use super::ui_helpers::{AccordionManager, ExpandableCard, collapse_revealer_instant};
use super::updates_card::{self, UpdatesCardState, build_updates_card};
use super::vpn_card::{self, VpnCardState, build_vpn_details, vpn_icon_name};

thread_local! {
    static CURRENT_QS_WINDOW: RefCell<Option<Weak<QuickSettingsWindow>>> = const { RefCell::new(None) };
}

pub(super) fn current_quick_settings_window() -> Option<Rc<QuickSettingsWindow>> {
    CURRENT_QS_WINDOW.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade()))
}

fn set_current_qs_window(qs: &Rc<QuickSettingsWindow>) {
    CURRENT_QS_WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(Rc::downgrade(qs));
    });
}

/// GObject data key for the back-reference to [`QuickSettingsWindow`].
const QS_WINDOW_DATA_KEY: &str = "vibepanel-qs-window";

/// Store a [`Weak`] back-reference to [`QuickSettingsWindow`] on a window.
///
/// Pairs with [`get_qs_window_data`] to encapsulate the `unsafe` type-tag invariant.
pub(super) fn set_qs_window_data(window: &ApplicationWindow, qs: &Rc<QuickSettingsWindow>) {
    unsafe {
        window.set_data(QS_WINDOW_DATA_KEY, Rc::downgrade(qs));
    }
}

/// Retrieve the [`QuickSettingsWindow`] back-reference from an [`ApplicationWindow`].
///
/// Returns `Some` if the window has a valid, still-alive reference.
pub(super) fn get_qs_window_data(window: &ApplicationWindow) -> Option<Rc<QuickSettingsWindow>> {
    unsafe {
        window
            .data::<Weak<QuickSettingsWindow>>(QS_WINDOW_DATA_KEY)
            .and_then(|ptr| ptr.as_ref().upgrade())
    }
}

/// Clear the current QuickSettingsWindow reference.
fn clear_current_qs_window() {
    CURRENT_QS_WINDOW.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

const QUICK_SETTINGS_CONTENT_WIDTH: i32 = 320;
/// CSS `padding: 16px` on `.vp-surface-popover` (both sides).
const QUICK_SETTINGS_POPOVER_PADDING: i32 = 32;
const QUICK_SETTINGS_OUTER_MARGIN: i32 = 4;
const QUICK_SETTINGS_FAR_EDGE_MARGIN: i32 = 8;
/// Container padding (surface padding + margins) for height calculation.
const QUICK_SETTINGS_CONTAINER_PADDING: i32 = 24;
const QUICK_SETTINGS_MIN_HEIGHT_THRESHOLD: i32 = 100;
const QUICK_SETTINGS_MIN_EDGE_MARGIN: i32 = 4;
const QUICK_SETTINGS_DEFAULT_RIGHT_MARGIN: i32 = 8;
const CARD_ROW_SPACING: i32 = 8;
const CARD_ROW_GAP: i32 = 8;
const AUDIO_SECTION_TOP_MARGIN: i32 = 12;

/// Full Quick Settings window.
///
/// ## Animation architecture
///
/// Open/close animations are driven by a **tick callback** on the animation
/// shell (`ScaleBox`), not by CSS `transition:` properties. CSS `transform:
/// scale()` transitions are observed to cause unbounded memory growth in
/// GTK4. See `LayerShellPopover` for the same pattern.
pub struct QuickSettingsWindow {
    window: ApplicationWindow,
    click_catcher: RefCell<Option<ApplicationWindow>>,
    /// Animation shell wrapping the outer container. Opacity and scale are
    /// animated via tick callback — no CSS transitions involved.
    anim_shell: ScaleBox,
    /// Wrapper between window and anim_shell that provides shadow margins
    /// so the ScaleBox grow-in clip animation is visible.
    margin_wrapper: GtkBox,
    /// Content container (surface styles, focus suppression).
    outer_container: RefCell<Option<GtkBox>>,
    /// Anchor X position in monitor coordinates.
    anchor_x: Cell<i32>,
    anchor_monitor: RefCell<Option<gdk::Monitor>>,
    /// Cached window width from the first successful map. Layer shell surfaces
    /// report 0 width when hidden, so we cache the real value for re-opens.
    cached_width: Cell<i32>,
    /// Whether the window has been mapped at least once (used to skip
    /// the opacity fade-in trick on subsequent shows).
    has_been_mapped: Cell<bool>,
    /// Whether a close animation is currently playing.
    is_animating_out: Cell<bool>,
    /// Logical open state. True from show_panel() until hide_panel() is called.
    /// Used by toggle_at() and Dismissible so clicks during close animation
    /// re-open the panel instead of being silently swallowed.
    logically_open: Cell<bool>,
    /// Shared animation state driven by the tick callback.
    anim_state: Rc<RefCell<AnimState>>,
    /// Generation counter incremented on every show/hide to cancel stale
    /// tick callbacks and idle callbacks.
    anim_generation: Rc<Cell<u32>>,
    cards_config: QuickSettingsCardsConfig,
    audio_scroll_percentage: i32,
    scroll_container: ScrolledWindow,
    /// Service callback IDs, used to disconnect/unsubscribe on close.
    network_callback_id: Cell<Option<CallbackId>>,
    bluetooth_callback_id: Cell<Option<CallbackId>>,
    vpn_callback_id: Cell<Option<CallbackId>>,
    idle_inhibitor_callback_id: Cell<Option<CallbackId>>,
    audio_output_callback_id: Cell<Option<CallbackId>>,
    audio_mic_callback_id: Cell<Option<CallbackId>>,
    brightness_callback_id: Cell<Option<CallbackId>>,
    updates_callback_id: Cell<Option<CallbackId>>,
    theme_callback_id: Cell<Option<CallbackId>>,

    // Card states
    pub network: Rc<NetworkCardState>,
    pub bluetooth: Rc<BluetoothCardState>,
    pub vpn: Rc<VpnCardState>,
    pub idle_inhibitor: Rc<IdleInhibitorCardState>,
    pub audio: Rc<AudioCardState>,
    pub mic: Rc<MicCardState>,
    pub brightness: Rc<BrightnessCardState>,
    pub updates: Rc<UpdatesCardState>,
    /// Power card state (expander variant only). Stored here so
    /// `reset_ui_state()` can collapse it without walking the widget tree.
    pub power: RefCell<Option<Rc<PowerCardExpanderState>>>,
    /// One-shot key controller installed by `prepare_keyboard_nav()`.
    /// Stored so `hide_panel()` can remove it if Tab was never pressed.
    deferred_kbd_controller: RefCell<Option<EventControllerKey>>,
}

impl QuickSettingsWindow {
    /// Create a new Quick Settings window bound to the given application.
    pub fn new(app: &Application, config: QuickSettingsConfig) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("vibepanel quick settings")
            .decorated(false)
            .resizable(false)
            .build();

        // This window is a floating control center panel.
        window.add_css_class(qs::WINDOW);

        // Layer shell configuration for panel behavior.
        // Use Top layer (not Overlay) to avoid appearing on top of fullscreen apps.
        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-quick-settings-popover"));
        window.set_layer(Layer::Top);
        window.set_exclusive_zone(0);
        let is_bottom = ConfigManager::global().bar_is_bottom();
        window.set_anchor(Edge::Top, !is_bottom);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, is_bottom);
        window.set_anchor(Edge::Left, false);
        window.set_margin(popover_bar_edge(), 0);
        window.set_margin(Edge::Right, 8);
        window.set_keyboard_mode(popover_keyboard_mode());

        // Create scroll container for height limiting.
        // Max height will be set in update_position() based on monitor geometry.
        // propagate_natural_height allows it to grow to fit content, max_content_height caps it.
        let scroll_container = ScrolledWindow::new();
        scroll_container.set_hscrollbar_policy(PolicyType::Never);
        scroll_container.set_vscrollbar_policy(PolicyType::Automatic);
        scroll_container.set_propagate_natural_height(true);

        // Create the animation shell — a ScaleBox that wraps the outer
        // container. Opacity and scale are animated via tick callback.
        let anim_shell = ScaleBox::new();
        anim_shell.set_opacity(0.0);
        anim_shell.set_scale(ANIM_SCALE_FROM);

        // Margin wrapper sits between window and anim_shell, providing
        // transparent padding so the ScaleBox clip animation is visible.
        let margin_wrapper = GtkBox::new(Orientation::Vertical, 0);
        margin_wrapper.add_css_class(surface::POPOVER_WRAPPER);
        margin_wrapper.add_css_class(surface::WIDGET_MENU_WRAPPER);
        SurfaceStyleManager::global()
            .apply_shadow_margins(&margin_wrapper, QUICK_SETTINGS_OUTER_MARGIN);

        // Content is built after construction.
        let qs = Rc::new(Self {
            window: window.clone(),
            click_catcher: RefCell::new(None),
            anim_shell: anim_shell.clone(),
            margin_wrapper: margin_wrapper.clone(),
            outer_container: RefCell::new(None),
            anchor_x: Cell::new(0),
            anchor_monitor: RefCell::new(None),
            cached_width: Cell::new(0),
            has_been_mapped: Cell::new(false),
            is_animating_out: Cell::new(false),
            logically_open: Cell::new(false),
            anim_state: Rc::new(RefCell::new(AnimState::new_idle())),
            anim_generation: Rc::new(Cell::new(0)),
            cards_config: config.cards,
            audio_scroll_percentage: config.audio_scroll_percentage,
            scroll_container,
            network_callback_id: Cell::new(None),
            bluetooth_callback_id: Cell::new(None),
            vpn_callback_id: Cell::new(None),
            idle_inhibitor_callback_id: Cell::new(None),
            audio_output_callback_id: Cell::new(None),
            audio_mic_callback_id: Cell::new(None),
            brightness_callback_id: Cell::new(None),
            updates_callback_id: Cell::new(None),
            theme_callback_id: Cell::new(None),
            network: Rc::new(NetworkCardState::new()),
            bluetooth: Rc::new(BluetoothCardState::new()),
            vpn: Rc::new(VpnCardState::new()),
            idle_inhibitor: Rc::new(IdleInhibitorCardState::new()),
            audio: Rc::new(AudioCardState::new()),
            mic: Rc::new(MicCardState::new()),
            brightness: Rc::new(BrightnessCardState::new()),
            updates: Rc::new(UpdatesCardState::new()),
            power: RefCell::new(None),
            deferred_kbd_controller: RefCell::new(None),
        });

        let outer = Self::build_content(&qs);

        // Hierarchy: window → margin_wrapper → anim_shell (ScaleBox) → outer
        // The margin wrapper provides transparent padding around the ScaleBox
        // so the rounded-clip grow animation is visible (same pattern as
        // LayerShellPopover).
        anim_shell.set_child(&outer);
        margin_wrapper.append(&anim_shell.clone().upcast::<gtk4::Widget>());
        window.set_child(Some(&margin_wrapper));

        // Store outer container reference.
        *qs.outer_container.borrow_mut() = Some(outer.clone());

        // Apply Pango font attributes to all labels if enabled in config.
        // This is the central hook for quick settings - widgets create standard
        // GTK labels, and we apply Pango attributes here after the tree is built.
        SurfaceStyleManager::global().apply_pango_attrs_all(&outer);

        // Store a back-reference on the window so callbacks can access the QuickSettingsWindow.
        set_qs_window_data(&qs.window, &qs);

        // ESC key closes the panel
        {
            let qs_weak = Rc::downgrade(&qs);
            setup_esc_handler(&qs.window, move || {
                if let Some(qs) = qs_weak.upgrade() {
                    qs.hide_panel();
                }
            });
        }

        // Apply blur when mapped.  On first map the surface has no size yet
        // so apply_blur_region defers via idle.  On re-show, anim_shell is at
        // opacity 0 (transparent) until the animation tick overwrites the region
        // with a scaled version within 1-2 frames.
        //
        // The else-branch removes any stale protocol object left from a
        // previous map cycle.  This handles the case where blur was enabled
        // when QS was last shown, then disabled while QS was hidden (unmapped).
        // `remove_blur_region` requires a mapped surface, so connect_map is
        // the earliest reliable cleanup point.
        //
        // Known limitation: config changes to `theme.blur` or border radius
        // while Quick Settings is open take effect on next open, not
        // immediately.  QS grabs focus so config edits are unlikely while open.
        window.connect_map(move |win| {
            if ConfigManager::global().blur_enabled() {
                if let Some(blur) =
                    crate::services::background_effect::BackgroundEffectManager::global()
                {
                    blur.apply_blur_region(win, QUICK_SETTINGS_OUTER_MARGIN);
                }
            } else if let Some(blur) =
                crate::services::background_effect::BackgroundEffectManager::global()
            {
                blur.remove_blur_region(win);
            }
        });

        // Subscribe to services
        Self::subscribe_to_services(&qs);

        // Update revealer durations when animations config is toggled at runtime.
        {
            let qs_weak = Rc::downgrade(&qs);
            let id = ConfigManager::global().on_theme_change(move || {
                if let Some(qs) = qs_weak.upgrade() {
                    Self::update_revealer_durations(&qs);
                }
            });
            qs.theme_callback_id.set(Some(id));
        }

        // Set VPN keyboard state's reference to this QS window for keyboard grab management
        vpn_card::set_quick_settings_window(Rc::downgrade(&qs));

        qs
    }

    /// Subscribe to all service updates.
    fn subscribe_to_services(qs: &Rc<Self>) {
        let cfg = &qs.cards_config;

        if cfg.network {
            let qs_weak = Rc::downgrade(qs);
            let id = NetworkService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    network_card::on_network_changed(&qs.network, snapshot, &qs.window);
                }
            });
            qs.network_callback_id.set(Some(id));
        }

        if cfg.bluetooth {
            let qs_weak = Rc::downgrade(qs);
            let id = BluetoothService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    bluetooth_card::on_bluetooth_changed(&qs.bluetooth, snapshot);
                }
            });
            qs.bluetooth_callback_id.set(Some(id));
        }

        if cfg.vpn {
            let qs_weak = Rc::downgrade(qs);
            let close_on_connect = cfg.vpn_close_on_connect;
            let id = VpnService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    let connect_completed = vpn_card::on_vpn_changed(&qs.vpn, snapshot);
                    if connect_completed && close_on_connect {
                        qs.hide_panel();
                    }
                }
            });
            qs.vpn_callback_id.set(Some(id));
        }

        if cfg.idle_inhibitor {
            let qs_weak = Rc::downgrade(qs);
            let id = IdleInhibitorService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    idle_inhibitor_card::on_idle_inhibitor_changed(&qs.idle_inhibitor, snapshot);
                }
            });
            qs.idle_inhibitor_callback_id.set(Some(id));
        }

        if cfg.audio {
            let qs_weak = Rc::downgrade(qs);
            let id = AudioService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    audio_card::on_audio_changed(&qs.audio, snapshot);
                }
            });
            qs.audio_output_callback_id.set(Some(id));
        }

        if cfg.mic {
            let qs_weak = Rc::downgrade(qs);
            let id = AudioService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    mic_card::on_mic_changed(&qs.mic, snapshot);
                }
            });
            qs.audio_mic_callback_id.set(Some(id));
        }

        if cfg.brightness {
            let qs_weak = Rc::downgrade(qs);
            let id = BrightnessService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    brightness_card::on_brightness_changed(&qs.brightness, snapshot);
                }
            });
            qs.brightness_callback_id.set(Some(id));
        }

        if cfg.updates {
            let qs_weak = Rc::downgrade(qs);
            let id = UpdatesService::global().connect(move |snapshot| {
                if let Some(qs) = qs_weak.upgrade() {
                    updates_card::on_updates_changed(&qs.updates, snapshot);
                }
            });
            qs.updates_callback_id.set(Some(id));
        }
    }

    /// Build the control center content.
    fn build_content(qs: &Rc<Self>) -> GtkBox {
        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.add_css_class(qs::WINDOW_CONTAINER);
        outer.add_css_class(surface::NO_FOCUS);

        // Apply surface styles - background now controlled via CSS variables
        outer.add_css_class("quick-settings-popover");
        outer.add_css_class(surface::POPOVER);
        outer.add_css_class(surface::SURFACE_POPOVER);
        outer.add_css_class(surface::WIDGET_MENU);
        SurfaceStyleManager::global().apply_surface_styles(&outer, true);

        let content = GtkBox::new(Orientation::Vertical, 0);
        content.add_css_class(qs::CONTROL_CENTER);
        content.add_css_class(surface::WIDGET_MENU_CONTENT);
        content.set_size_request(QUICK_SETTINGS_CONTENT_WIDTH, -1);

        let cfg = &qs.cards_config;

        // Collect toggle cards and their revealers.
        // These are the cards that appear in the 2-per-row grid.
        //
        // Cards with expandable state store a trait object for uniform accordion
        // registration. Cards that need custom expand/collapse behavior (e.g.,
        // Power card updating its subtitle) provide an on_toggle callback.
        struct ToggleCardInfo {
            card: gtk4::Widget,
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
        if cfg.network {
            let (card, revealer, expander_button) = Self::build_network_card(qs);
            toggle_cards.push(ToggleCardInfo {
                card,
                revealer: Some(revealer),
                expander_button,
                expandable: Some(Rc::clone(&qs.network) as Rc<dyn ExpandableCard>),
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
        if cfg.vpn && VpnService::global().snapshot().available {
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
                    // Store the power card state on the QS window for reset_ui_state()
                    *qs.power.borrow_mut() = Some(Rc::clone(&state));

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
                                    "Hold to shut down"
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
            let row = GtkBox::new(Orientation::Horizontal, CARD_ROW_GAP);
            row.add_css_class(qs::CARDS_ROW);
            row.set_homogeneous(true);
            if !is_first_row {
                row.set_margin_top(CARD_ROW_SPACING);
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
            audio_row.set_margin_top(AUDIO_SECTION_TOP_MARGIN);
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

    /// Build the network card and its revealer.
    ///
    /// Returns `(card, revealer, expander_button)` - caller is responsible for
    /// accordion registration via `AccordionManager::setup_expander`.
    fn build_network_card(qs: &Rc<Self>) -> (gtk4::Widget, Revealer, Option<Button>) {
        let network_service = NetworkService::global();
        let snapshot = network_service.snapshot();

        let wifi_enabled = snapshot.wifi_enabled().unwrap_or(false);
        let wifi_connected = snapshot.connected();
        let wired_connected = snapshot.wired_connected();

        // Build custom subtitle widget with connection status icons
        let subtitle_result = build_network_subtitle(&snapshot);

        let icon_name = resolve_material_network_icon(&snapshot);
        let icon_active =
            (wifi_enabled && wifi_connected) || wired_connected || snapshot.mobile_active();

        // Card title: "Network" if ethernet/modem device exists, "Wi-Fi" otherwise
        let card_title = if snapshot.has_non_wifi_device() {
            "Network"
        } else {
            "Wi-Fi"
        };

        let network_card = ToggleCard::builder()
            .icon(icon_name)
            .label(card_title)
            .subtitle_widget(subtitle_result.container.upcast())
            .active(wifi_enabled)
            .sensitive(true)
            .icon_active(icon_active)
            .with_expander(true)
            .build();

        // Add card identifier for CSS targeting
        network_card.card.add_css_class(qs::WIFI);

        // Disable toggle if no Wi-Fi device (toggle controls Wi-Fi, not ethernet)
        if !snapshot.has_wifi_device() {
            network_card.toggle.set_sensitive(false);
        }

        if !wifi_enabled && !wired_connected && !snapshot.mobile_active() {
            // Only apply wifi-disabled styling when actually showing a wifi icon
            let showing_wifi_icon = !is_material_unified(&snapshot)
                || (!snapshot.mobile_active() && !snapshot.mobile_connecting());
            if showing_wifi_icon {
                network_card
                    .icon_handle
                    .widget()
                    .add_css_class(qs::WIFI_DISABLED_ICON);
            }
        }

        // Show spinner when wifi or cellular is connecting/scanning
        let wifi_connecting = snapshot.wifi_connecting();
        let is_connecting = wifi_connecting || snapshot.mobile_connecting();
        if is_connecting {
            network_card.icon_handle.set_spinning(true);
        }

        {
            let toggle = network_card.toggle.clone();
            let network_state = Rc::clone(&qs.network);
            toggle.connect_toggled(move |toggle| {
                // Skip if this is a programmatic update (prevents feedback loops)
                if network_state.updating_wifi_toggle.get() {
                    return;
                }
                NetworkService::global().set_wifi_enabled(toggle.is_active());
            });
        }

        *qs.network.base.toggle.borrow_mut() = Some(network_card.toggle.clone());
        *qs.network.base.card_icon.borrow_mut() = Some(network_card.icon_handle.clone());
        *qs.network.base.arrow.borrow_mut() = network_card.expander_icon.clone();
        *qs.network.title_label.borrow_mut() = Some(network_card.title.clone());
        *qs.network.subtitle_label.borrow_mut() = Some(subtitle_result.label);

        let network_revealer = Revealer::new();
        network_revealer.set_reveal_child(false);
        network_revealer.set_transition_type(RevealerTransitionType::SlideDown);
        network_revealer.set_transition_duration(ConfigManager::global().animation_duration(250));
        let network_state = Rc::clone(&qs.network);

        let network_details = build_wifi_details(&network_state, qs.window.downgrade());
        network_revealer.set_child(Some(&network_details.container));

        *qs.network.base.list_box.borrow_mut() = Some(network_details.list_box);
        *qs.network.base.revealer.borrow_mut() = Some(network_revealer.clone());
        *qs.network.scan_button.borrow_mut() = Some(network_details.scan_button);

        // Connect Wi-Fi switch to toggle Wi-Fi enabled state
        {
            let network_state = Rc::clone(&qs.network);
            network_details
                .wifi_switch
                .connect_state_set(move |_, enabled| {
                    // Skip if this is a programmatic update (prevents feedback loops)
                    if network_state.updating_wifi_toggle.get() {
                        return glib::Propagation::Proceed;
                    }
                    NetworkService::global().set_wifi_enabled(enabled);
                    glib::Propagation::Proceed
                });
        }

        (
            network_card.card,
            network_revealer,
            network_card.expander_button,
        )
    }

    /// Build the Bluetooth card and its revealer.
    ///
    /// Returns `(card, revealer, expander_button)` - caller is responsible for
    /// accordion registration via `AccordionManager::setup_expander`.
    fn build_bluetooth_card(qs: &Rc<Self>) -> (gtk4::Widget, Revealer, Option<Button>) {
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

        *qs.bluetooth.base.toggle.borrow_mut() = Some(bt_card.toggle.clone());
        *qs.bluetooth.base.card_icon.borrow_mut() = Some(bt_card.icon_handle.clone());
        *qs.bluetooth.base.subtitle.borrow_mut() = bt_card.subtitle.clone();
        *qs.bluetooth.base.arrow.borrow_mut() = bt_card.expander_icon.clone();

        let bt_revealer = Revealer::new();
        bt_revealer.set_reveal_child(false);
        bt_revealer.set_transition_type(RevealerTransitionType::SlideDown);
        bt_revealer.set_transition_duration(ConfigManager::global().animation_duration(250));

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
    fn build_vpn_card(qs: &Rc<Self>) -> (gtk4::Widget, Revealer, Option<Button>) {
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

        let vpn_icon = vpn_icon_name();
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
                    let target_active = toggle.is_active();
                    vpn_card::track_toggle_action(&primary.uuid, target_active);
                    vpn.set_connection_state(&primary.uuid, target_active);
                }
            });
        }

        *qs.vpn.base.toggle.borrow_mut() = Some(vpn_card.toggle.clone());
        *qs.vpn.base.card_icon.borrow_mut() = Some(vpn_card.icon_handle.clone());
        *qs.vpn.base.subtitle.borrow_mut() = vpn_card.subtitle.clone();
        *qs.vpn.base.arrow.borrow_mut() = vpn_card.expander_icon.clone();

        let vpn_revealer = Revealer::new();
        vpn_revealer.set_reveal_child(false);
        vpn_revealer.set_transition_type(RevealerTransitionType::SlideDown);
        vpn_revealer.set_transition_duration(ConfigManager::global().animation_duration(250));

        let vpn_state = Rc::clone(&qs.vpn);
        let vpn_details = build_vpn_details(&vpn_state);
        vpn_revealer.set_child(Some(&vpn_details.container));

        *qs.vpn.base.list_box.borrow_mut() = Some(vpn_details.list_box);
        *qs.vpn.base.revealer.borrow_mut() = Some(vpn_revealer.clone());

        (vpn_card.card, vpn_revealer, vpn_card.expander_button)
    }

    /// Build the Idle Inhibitor card (no revealer needed).
    fn build_idle_inhibitor_card(qs: &Rc<Self>) -> gtk4::Widget {
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

        // Scroll wheel adjusts volume when hovering the audio row.
        audio_card::attach_volume_scroll_controller(&audio_widgets.row, qs.audio_scroll_percentage);

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

        *qs.brightness.slider.borrow_mut() = Some(brightness_widgets.slider.clone());
        *qs.brightness.icon_handle.borrow_mut() = Some(brightness_widgets.icon_handle.clone());

        brightness_widgets.row
    }

    /// Show inline Wi-Fi password dialog for the given SSID.
    pub fn show_wifi_password_dialog(&self, ssid: &str) {
        network_card::show_password_dialog(&self.network, ssid);
    }

    /// Update all revealer transition durations based on the current
    /// `theme.animations` config.
    ///
    /// Called from the `on_theme_change` callback so that toggling animations
    /// at runtime takes effect without restarting.
    fn update_revealer_durations(qs: &Rc<Self>) {
        let cfg = ConfigManager::global();

        // Toggle cards use the GTK default duration (250ms) when enabled.
        let card_duration = cfg.animation_duration(250);
        for revealer in [
            qs.network.base.revealer.borrow(),
            qs.bluetooth.base.revealer.borrow(),
            qs.vpn.base.revealer.borrow(),
            qs.updates.base.revealer.borrow(),
        ] {
            if let Some(r) = revealer.as_ref() {
                r.set_transition_duration(card_duration);
            }
        }

        // Power card (only present in expander variant).
        if let Some(ref power) = *qs.power.borrow()
            && let Some(r) = power.base.revealer.borrow().as_ref()
        {
            r.set_transition_duration(card_duration);
        }

        // Audio/mic use an explicit 200ms duration.
        let audio_duration = cfg.animation_duration(200);
        if let Some(r) = qs.audio.revealer.borrow().as_ref() {
            r.set_transition_duration(audio_duration);
        }
        if let Some(r) = qs.mic.revealer.borrow().as_ref() {
            r.set_transition_duration(audio_duration);
        }
    }

    /// Reset all UI state to its initial (collapsed) appearance.
    ///
    /// Called when hiding the panel so it opens fresh next time. This
    /// collapses all revealers, removes expanded arrow indicators, hides
    /// auth dialogs, and scrolls back to the top.
    fn reset_ui_state(&self) {
        // --- Collapse all toggle card revealers (network, bluetooth, vpn, updates, power) ---

        // Helper: collapse a revealer and remove the EXPANDED class from its arrow
        let collapse = |base: &super::ui_helpers::ExpandableCardBase| {
            if let Some(revealer) = base.revealer.borrow().as_ref() {
                collapse_revealer_instant(revealer);
            }
            if let Some(arrow) = base.arrow.borrow().as_ref() {
                arrow.widget().remove_css_class(state::EXPANDED);
            }
        };

        collapse(&self.network.base);
        collapse(&self.bluetooth.base);
        collapse(&self.vpn.base);
        collapse(&self.updates.base);

        // Power card (expander variant)
        if let Some(ref power_state) = *self.power.borrow() {
            collapse(&power_state.base);
            // Reset subtitle back to default
            if let Some(ref subtitle) = *power_state.base.subtitle.borrow() {
                subtitle.set_label("Hold to shut down");
            }
        }

        // --- Collapse audio and mic revealers ---
        if let Some(revealer) = self.audio.revealer.borrow().as_ref() {
            collapse_revealer_instant(revealer);
        }
        if let Some(arrow) = self.audio.arrow.borrow().as_ref() {
            arrow.widget().remove_css_class(state::EXPANDED);
        }

        if let Some(revealer) = self.mic.revealer.borrow().as_ref() {
            collapse_revealer_instant(revealer);
        }
        if let Some(arrow) = self.mic.arrow.borrow().as_ref() {
            arrow.widget().remove_css_class(state::EXPANDED);
        }

        // --- Hide auth dialogs ---
        network_card::hide_password_dialog(&self.network);
        vpn_card::hide_vpn_auth_dialog(&self.vpn);
        self.bluetooth.clear_auth_input();

        // --- Scroll to top ---
        self.scroll_container.vadjustment().set_value(0.0);

        // --- Clear focus from any focused widget ---
        gtk4::prelude::RootExt::set_focus(&self.window, None::<&gtk4::Widget>);
    }

    // Position and visibility management

    /// Set the anchor position for the window (horizontal positioning).
    pub fn set_anchor_position(&self, x: i32, monitor: Option<Monitor>) {
        self.anchor_x.set(x);
        *self.anchor_monitor.borrow_mut() = monitor;
    }

    /// Update window margins based on the current anchor position.
    fn update_position(&self) {
        let anchor_x = self.anchor_x.get();

        // Update shadow margins on the margin wrapper.
        SurfaceStyleManager::global()
            .apply_shadow_margins(&self.margin_wrapper, QUICK_SETTINGS_OUTER_MARGIN);

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

        // Get bar dimensions from config for height calculation
        let config_mgr = ConfigManager::global();
        let bar_size = config_mgr.bar_size() as i32;
        let bar_padding = config_mgr.bar_padding() as i32;
        let bar_opacity = config_mgr.bar_background_opacity();
        let screen_margin = config_mgr.screen_margin() as i32;
        let popover_offset = config_mgr.popover_offset() as i32;

        // Bar exclusive zone (matches bar.rs logic)
        let bar_exclusive_zone = if bar_opacity > 0.0 {
            bar_size + 2 * bar_padding + 2 * screen_margin + popover_offset
        } else {
            bar_size + 2 * screen_margin + popover_offset
        };

        // Set bar-edge margin using shared helper
        let bar_margin = calculate_popover_bar_margin();
        self.window.set_margin(popover_bar_edge(), bar_margin);

        // Max height: screen minus bar zone, margins, and container padding
        let max_height = geom.height()
            - bar_exclusive_zone
            - bar_margin
            - QUICK_SETTINGS_CONTAINER_PADDING
            - QUICK_SETTINGS_FAR_EDGE_MARGIN;

        if max_height > QUICK_SETTINGS_MIN_HEIGHT_THRESHOLD {
            self.scroll_container.set_max_content_height(max_height);
        }

        // Set right margin using shared helper
        if anchor_x > 0 {
            // Use cached width from a previous map, or fall back to the live
            // value.  Layer-shell surfaces report 0 when hidden, so the cache
            // is essential for re-opens.
            let w = self.window.width();
            let window_width = if w > 0 {
                self.cached_width.set(w);
                w
            } else if self.cached_width.get() > 0 {
                self.cached_width.get()
            } else {
                // Never mapped — estimate from content + CSS padding + shadow margins.
                let shadow_m =
                    SurfaceStyleManager::global().shadow_margin(QUICK_SETTINGS_OUTER_MARGIN);
                QUICK_SETTINGS_CONTENT_WIDTH + QUICK_SETTINGS_POPOVER_PADDING + 2 * shadow_m
            };
            let right_margin = calculate_popover_right_margin(
                anchor_x,
                geom.width(),
                window_width,
                QUICK_SETTINGS_MIN_EDGE_MARGIN,
            );
            self.window.set_margin(Edge::Right, right_margin);
        } else {
            self.window
                .set_margin(Edge::Right, QUICK_SETTINGS_DEFAULT_RIGHT_MARGIN);
        }
    }

    /// Show the panel and associated click-catcher.
    ///
    /// Handles both the first show (new window) and re-shows (hidden window
    /// being made visible again). On re-show, the window is already mapped so
    /// we skip the opacity fade-in trick and go straight to positioning.
    fn show_panel(self: &Rc<Self>) {
        // Mark as logically open immediately so toggle_at() works correctly
        // even if a close animation is still in flight.
        self.logically_open.set(true);

        // Note: unlike LayerShellPopover, we don't attempt mid-close reversal
        // here — not worth the complexity for a 150ms animation.
        self.is_animating_out.set(false);

        // Bump generation to cancel stale tick callbacks and idle callbacks.
        let generation = self.anim_generation.get().wrapping_add(1);
        self.anim_generation.set(generation);

        if let Some(monitor) = self.anchor_monitor.borrow().as_ref() {
            self.window.set_monitor(Some(monitor));
        }

        // Show click-catcher (persistent, created lazily).
        let catcher = self.ensure_click_catcher();
        if let Some(monitor) = self.anchor_monitor.borrow().as_ref() {
            catcher.set_monitor(Some(monitor));
        }
        catcher.set_margin(popover_bar_edge(), calculate_bar_exclusive_zone());
        catcher.set_visible(true);

        // Restore keyboard mode
        self.window.set_keyboard_mode(popover_keyboard_mode());

        // Set the global current QS window reference
        set_current_qs_window(self);

        // Set animation shell to hidden state for open animation.
        self.anim_shell.set_opacity(0.0);
        self.anim_shell.set_scale(ANIM_SCALE_FROM);

        if self.has_been_mapped.get() {
            // Re-show: window was previously mapped and hidden. The surface
            // already exists so we can position and show immediately without
            // the opacity fade-in trick.
            self.update_position();
            self.window.set_visible(true);
            self.window.present();

            // Install deferred Tab controller for keyboard nav on first Tab.
            self.prepare_keyboard_nav();

            // Start open animation + re-deliver service snapshots.
            let window_weak = self.window.downgrade();
            let gen_rc = Rc::clone(&self.anim_generation);
            glib::idle_add_local_once(move || {
                if gen_rc.get() != generation {
                    return;
                }
                if let Some(window) = window_weak.upgrade()
                    && let Some(qs) = get_qs_window_data(&window)
                {
                    if ConfigManager::global().animations_enabled() {
                        qs.start_animation(AnimDirection::Opening, generation);
                    } else {
                        qs.anim_shell.set_opacity(1.0);
                        qs.anim_shell.set_scale(1.0);
                    }
                    let snapshot = NetworkService::global().snapshot();
                    network_card::on_network_changed(&qs.network, &snapshot, &qs.window);
                }
            });
        } else {
            // First show: window hasn't been mapped yet. Use the opacity trick
            // to avoid flicker while GTK4 determines the real window size.
            self.has_been_mapped.set(true);
            self.window.set_opacity(0.0);
            self.window.set_visible(true);
            self.window.present();

            // Install deferred Tab controller for keyboard nav on first Tab.
            self.prepare_keyboard_nav();

            let window_weak = self.window.downgrade();
            let gen_rc = Rc::clone(&self.anim_generation);
            glib::idle_add_local_once(move || {
                if gen_rc.get() != generation {
                    return;
                }
                if let Some(window) = window_weak.upgrade()
                    && let Some(qs) = get_qs_window_data(&window)
                {
                    qs.update_position();
                    qs.window.set_opacity(1.0);

                    if ConfigManager::global().animations_enabled() {
                        qs.start_animation(AnimDirection::Opening, generation);
                    } else {
                        qs.anim_shell.set_opacity(1.0);
                        qs.anim_shell.set_scale(1.0);
                    }

                    let snapshot = NetworkService::global().snapshot();
                    network_card::on_network_changed(&qs.network, &snapshot, &qs.window);
                }
            });
        }
    }

    /// Enable keyboard navigation (remove focus suppression).
    ///
    /// Activated by the deferred keynav controller installed in `show_panel()`.
    /// On `hide_panel()`, `focus-visible` is reset and `.vp-no-focus` is
    /// restored so the next open starts focus-suppressed.
    fn enable_keyboard_nav(&self) {
        if let Some(ref outer) = *self.outer_container.borrow() {
            gtk4::prelude::GtkWindowExt::set_focus_visible(&self.window, true);
            outer.remove_css_class(surface::NO_FOCUS);
        }
    }

    /// Prepare deferred keyboard navigation.
    ///
    /// Clears auto-focus and installs a one-shot keynav controller.
    /// On the first keynav key (Tab, arrows, Home, End),
    /// `enable_keyboard_nav()` fires and focus lands on the first
    /// focusable widget.
    fn prepare_keyboard_nav(&self) {
        // Clear any auto-focus from present() so Tab starts from nothing.
        gtk4::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk4::Widget>);

        // Remove any previous deferred controller.
        self.remove_deferred_kbd_controller();

        let controller = EventControllerKey::new();
        let window_weak = self.window.downgrade();
        let ctrl_ref = controller.clone();
        controller.connect_key_pressed(move |_, keyval, _, _| {
            let is_keynav = is_keynav_key(keyval);
            if is_keynav
                && let Some(window) = window_weak.upgrade()
                && let Some(qs) = get_qs_window_data(&window)
            {
                qs.enable_keyboard_nav();
                window.remove_controller(&ctrl_ref);
                *qs.deferred_kbd_controller.borrow_mut() = None;
            }
            if keyval == gdk::Key::Tab || keyval == gdk::Key::ISO_Left_Tab {
                // Let Tab propagate — GTK focuses the first widget with
                // correct :focus-visible via its own keynav path.
                glib::Propagation::Proceed
            } else if is_keynav {
                // For arrows/Home/End, consume the key and simulate Tab's
                // focus behavior so we land on the first widget instead of
                // skipping it.
                if let Some(window) = window_weak.upgrade() {
                    window.child_focus(gtk4::DirectionType::TabForward);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });

        self.window.add_controller(controller.clone());
        *self.deferred_kbd_controller.borrow_mut() = Some(controller);
    }

    /// Remove the deferred keyboard nav controller if installed.
    fn remove_deferred_kbd_controller(&self) {
        if let Some(controller) = self.deferred_kbd_controller.borrow_mut().take() {
            self.window.remove_controller(&controller);
        }
    }

    /// Hide the panel with a close animation, keeping the window alive.
    ///
    /// The click-catcher is hidden and keyboard grab released immediately
    /// so the bar is interactive during the animation. The animation shell
    /// fades out via tick callback, then UI state is reset and the window hidden.
    /// Does NOT clear PopoverTracker — the caller is responsible for that.
    pub(super) fn hide_panel(&self) {
        if self.is_animating_out.get() {
            return;
        }

        // Mark as logically closed immediately so toggle_at() can re-open
        // during the close animation instead of swallowing the click.
        self.logically_open.set(false);

        // Restore focus suppression so the next open starts no-focus.
        gtk4::prelude::GtkWindowExt::set_focus_visible(&self.window, false);
        if let Some(ref outer) = *self.outer_container.borrow()
            && !outer.has_css_class(surface::NO_FOCUS)
        {
            outer.add_css_class(surface::NO_FOCUS);
        }
        self.remove_deferred_kbd_controller();

        // Restore keyboard mode if it was released for VPN password dialogs
        vpn_card::restore_keyboard_if_released();

        // Clear the global QS window reference so card-level actions
        // (e.g., show_wifi_password_dialog) don't fire while hidden.
        clear_current_qs_window();

        // Release keyboard grab while hidden
        self.window.set_keyboard_mode(KeyboardMode::None);

        // Hide click-catcher immediately so bar is interactive during animation.
        if let Some(ref catcher) = *self.click_catcher.borrow() {
            catcher.set_visible(false);
        }

        // Bump generation to cancel any pending idle callback from show_panel().
        let generation = self.anim_generation.get().wrapping_add(1);
        self.anim_generation.set(generation);

        self.is_animating_out.set(true);

        if !ConfigManager::global().animations_enabled() {
            // Animations disabled — snap closed immediately.
            self.anim_shell.set_opacity(0.0);
            self.anim_shell.set_scale(ANIM_SCALE_FROM);
            self.is_animating_out.set(false);
            self.reset_ui_state();
            // No explicit blur removal needed — unmapping suspends
            // compositor-side blur while the protocol object persists.
            // Blur is re-applied on next map via connect_map.
            self.window.set_visible(false);
            return;
        }

        // Ensure the window is fully visible (the idle callback from show_panel
        // may not have fired yet, leaving window.opacity at 0.0).
        self.window.set_opacity(1.0);

        // Remove blur immediately so the compositor stops drawing it while the
        // surface fades out.  Blur is a compositor effect independent of surface
        // opacity — if left in place it would remain visible as the content
        // becomes transparent.
        if let Some(blur) = crate::services::background_effect::BackgroundEffectManager::global() {
            blur.remove_blur_region(&self.window);
        }

        // Start (or reverse into) the close animation.
        self.start_animation(AnimDirection::Closing, generation);
    }

    /// Ensure the persistent click-catcher exists, creating it lazily.
    ///
    /// The click-catcher is shown/hidden each cycle rather than created/destroyed
    /// to avoid per-cycle allocation of an `ApplicationWindow` + layer-shell surface.
    fn ensure_click_catcher(self: &Rc<Self>) -> ApplicationWindow {
        if let Some(ref catcher) = *self.click_catcher.borrow() {
            return catcher.clone();
        }

        let app = self
            .window
            .application()
            .expect("QuickSettingsWindow must have an associated Application");

        let bar_zone = calculate_bar_exclusive_zone();
        let qs_weak = Rc::downgrade(self);
        let catcher = create_click_catcher(&app, bar_zone, move || {
            if let Some(qs) = qs_weak.upgrade() {
                qs.hide_panel();
            }
        });

        catcher.add_css_class(qs::CLICK_CATCHER);

        *self.click_catcher.borrow_mut() = Some(catcher.clone());
        catcher
    }

    /// Start or reverse the open/close animation via a tick callback.
    ///
    /// If an animation is already in flight (e.g., opening and user clicks to
    /// close), the current progress is captured and the animation reverses from
    /// that point with proportional timing — no snapping.
    fn start_animation(&self, direction: AnimDirection, generation: u32) {
        // Cache the current border radius for the duration of this animation.
        self.anim_shell
            .set_radius(ConfigManager::global().surface_border_radius() as f32);

        let start_time_us = self
            .anim_shell
            .frame_clock()
            .map(|fc| fc.frame_time())
            .unwrap_or(0);

        let need_tick = self.anim_state.borrow_mut().prepare(
            direction,
            generation,
            start_time_us,
            self.anim_shell.opacity(),
        );

        if !need_tick {
            return;
        }

        let anim_state = Rc::clone(&self.anim_state);
        let anim_gen = Rc::clone(&self.anim_generation);
        let window_weak = self.window.downgrade();
        let shell_clone = self.anim_shell.clone();

        self.anim_shell
            .add_tick_callback(move |shell, frame_clock| {
                if anim_gen.get() != generation {
                    return ControlFlow::Break;
                }

                let now_us = frame_clock.frame_time();
                let (progress, complete, direction) = {
                    let state = anim_state.borrow();
                    if !state.active {
                        return ControlFlow::Break;
                    }
                    (
                        state.current_progress(now_us),
                        state.is_complete(now_us),
                        state.direction,
                    )
                };

                // Apply visual state — opacity and scale, no CSS involvement.
                shell.set_opacity(progress);
                let scale = ANIM_SCALE_FROM + (1.0 - ANIM_SCALE_FROM) * progress;
                shell_clone.set_scale(scale);

                if direction == AnimDirection::Opening
                    && ConfigManager::global().blur_enabled()
                    && let Some(blur) =
                        crate::services::background_effect::BackgroundEffectManager::global()
                    && let Some(window) = window_weak.upgrade()
                {
                    blur.apply_open_animation_blur(
                        &window,
                        QUICK_SETTINGS_OUTER_MARGIN,
                        scale,
                        complete,
                    );
                }

                if complete {
                    anim_state.borrow_mut().active = false;

                    if direction == AnimDirection::Closing {
                        shell.set_opacity(0.0);
                        shell_clone.set_scale(ANIM_SCALE_FROM);
                        if let Some(window) = window_weak.upgrade()
                            && let Some(qs) = get_qs_window_data(&window)
                        {
                            qs.is_animating_out.set(false);
                            qs.reset_ui_state();
                            qs.window.set_visible(false);
                        }
                    } else {
                        shell.set_opacity(1.0);
                        shell_clone.set_scale(1.0);
                    }
                    return ControlFlow::Break;
                }

                ControlFlow::Continue
            });
    }

    /// Temporarily release exclusive keyboard grab to allow external dialogs
    /// (like password prompts) to receive keyboard input.
    ///
    /// This switches the keyboard mode to OnDemand on the main window only.
    /// The click-catcher always remains at KeyboardMode::None (it should never
    /// take keyboard focus). Call `restore_keyboard_mode()` when the external
    /// interaction is complete.
    pub(super) fn release_keyboard_grab(&self) {
        tracing::debug!("QuickSettings: Switching keyboard mode to OnDemand");
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        // Note: Don't touch click-catcher - it must always be KeyboardMode::None
    }

    /// Restore the default keyboard mode after releasing it temporarily.
    ///
    /// This switches the main window back to the compositor-appropriate keyboard
    /// mode (Exclusive for most compositors, OnDemand for Hyprland). The
    /// click-catcher always remains at KeyboardMode::None.
    pub(super) fn restore_keyboard_mode(&self) {
        let mode = popover_keyboard_mode();
        tracing::debug!("QuickSettings: Restoring keyboard mode to {:?}", mode);
        self.window.set_keyboard_mode(mode);
        // Note: Don't touch click-catcher - it must always be KeyboardMode::None
    }
}

impl Drop for QuickSettingsWindow {
    fn drop(&mut self) {
        // Unsubscribe from all services on final cleanup
        if let Some(id) = self.network_callback_id.take() {
            NetworkService::global().unsubscribe(id);
        }
        if let Some(id) = self.bluetooth_callback_id.take() {
            BluetoothService::global().disconnect(id);
        }
        if let Some(id) = self.vpn_callback_id.take() {
            VpnService::global().disconnect(id);
        }
        if let Some(id) = self.idle_inhibitor_callback_id.take() {
            IdleInhibitorService::global().disconnect(id);
        }
        if let Some(id) = self.audio_output_callback_id.take() {
            AudioService::global().disconnect(id);
        }
        if let Some(id) = self.audio_mic_callback_id.take() {
            AudioService::global().disconnect(id);
        }
        if let Some(id) = self.brightness_callback_id.take() {
            BrightnessService::global().disconnect(id);
        }
        if let Some(id) = self.updates_callback_id.take() {
            UpdatesService::global().disconnect(id);
        }
        if let Some(id) = self.theme_callback_id.take() {
            ConfigManager::global().disconnect_theme_callback(id);
        }

        clear_current_qs_window();

        // Destroy click-catcher if still alive
        if let Some(catcher) = self.click_catcher.borrow_mut().take() {
            catcher.close();
        }

        // Best-effort blur cleanup; primary removal happens at fade-start
        // in hide_panel().  May no-op if already unmapped.
        // See BackgroundEffectManager::remove_blur_region docs.
        if let Some(blur) = crate::services::background_effect::BackgroundEffectManager::global() {
            blur.remove_blur_region(&self.window);
        }

        // Destroy the GTK window
        self.window.close();
    }
}

/// Handle for toggling the keep-alive Quick Settings window from bar widgets.
///
/// Lazily creates the window on first open, then hides/re-shows on
/// subsequent toggles to avoid the rebuild cost.
#[derive(Clone)]
pub struct QuickSettingsWindowHandle {
    app: Application,
    config: QuickSettingsConfig,
    /// The keep-alive window instance. Created once on first open, kept
    /// alive across close/re-open cycles. Shared across clones via Rc.
    window: Rc<RefCell<Option<Rc<QuickSettingsWindow>>>>,
    /// ID returned from PopoverTracker when QS is active.
    ///
    /// Wrapped in `Rc<Cell<>>` because it's shared with `QuickSettingsDismissible`
    /// (which needs to clear it when dismissed) and mutated from multiple places
    /// (toggle_at close path and Dismissible::dismiss).
    tracker_id: Rc<Cell<Option<PopoverId>>>,
    /// Reference to the bar-side QS widget for deriving anchor position.
    /// Set after widget construction; `None` if the widget hasn't been built yet.
    bar_widget: Rc<RefCell<Option<gtk4::Widget>>>,
}

impl QuickSettingsWindowHandle {
    pub fn new(app: Application, config: QuickSettingsConfig) -> Self {
        Self {
            app,
            config,
            window: Rc::new(RefCell::new(None)),
            tracker_id: Rc::new(Cell::new(None)),
            bar_widget: Rc::new(RefCell::new(None)),
        }
    }

    /// Explicitly tear down the keep-alive window and clear PopoverTracker.
    ///
    /// Called during bar teardown to ensure the window and its service
    /// subscriptions are cleaned up even if a `QuickSettingsDismissible`
    /// still holds a reference to the shared `Rc<RefCell<Option<...>>>`.
    pub fn destroy(&self) {
        // Clear from PopoverTracker first
        if let Some(id) = self.tracker_id.take() {
            PopoverTracker::global().clear_if_active(id);
        }
        // Drop the window — triggers QuickSettingsWindow::drop which
        // unsubscribes all services and closes the GTK window.
        *self.window.borrow_mut() = None;
    }

    /// Store a reference to the bar-side QS widget for anchor derivation.
    pub fn set_bar_widget(&self, widget: gtk4::Widget) {
        *self.bar_widget.borrow_mut() = Some(widget);
    }

    /// Derive anchor position and monitor from the bar widget.
    ///
    /// Replicates the bar widget click handler's positioning logic
    /// (`compute_bounds` center + `screen_margin` adjustment).
    fn get_anchor_info(&self) -> (i32, Option<Monitor>) {
        let widget_ref = self.bar_widget.borrow();
        let Some(ref widget) = *widget_ref else {
            return (0, None);
        };
        let Some(native) = widget.native() else {
            return (0, None);
        };
        let Some(bounds) = widget.compute_bounds(&native) else {
            return (0, None);
        };

        let screen_margin = ConfigManager::global().screen_margin() as i32;
        let anchor_x = (bounds.x() + bounds.width() / 2.0) as i32 + screen_margin;

        let monitor = native
            .surface()
            .and_then(|s| s.display().monitor_at_surface(&s));

        (anchor_x, monitor)
    }

    pub fn toggle_at(&self, x: i32, monitor: Option<Monitor>) {
        // Check logical state, not window visibility — the window may still be
        // visible during a close animation but logically_open is already false.
        let is_visible = self
            .window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.logically_open.get());

        if is_visible {
            // Window is visible — hide it (keep alive for instant re-show)
            if let Some(qs) = self.window.borrow().as_ref() {
                qs.hide_panel();
            }
            // Clear from tracker using our stored ID
            if let Some(id) = self.tracker_id.take() {
                PopoverTracker::global().clear_if_active(id);
            }
            return;
        }

        // Dismiss any other active popup before opening QS
        PopoverTracker::global().dismiss_active();

        // Lazy-create: build window on first open, re-use on subsequent opens
        let needs_create = self.window.borrow().is_none();
        if needs_create {
            let qs = QuickSettingsWindow::new(&self.app, self.config.clone());
            *self.window.borrow_mut() = Some(qs);
        }

        // Update position and show
        if let Some(qs) = self.window.borrow().as_ref() {
            qs.set_anchor_position(x, monitor);
            qs.show_panel();
        }

        // Register with popup tracker for seamless transitions and store the ID
        let dismissible = QuickSettingsDismissible {
            window: self.window.clone(),
            tracker_id: self.tracker_id.clone(),
        };
        let id = PopoverTracker::global().set_active(Rc::new(dismissible));
        self.tracker_id.set(Some(id));
    }
}

impl crate::popover_registry::PopoverToggleable for QuickSettingsWindowHandle {
    fn ipc_show(&self) {
        if !self.ipc_is_visible() {
            let (x, monitor) = self.get_anchor_info();
            self.toggle_at(x, monitor);
        }
    }

    fn ipc_hide(&self) {
        if self.ipc_is_visible() {
            if let Some(qs) = self.window.borrow().as_ref() {
                qs.hide_panel();
            }
            if let Some(id) = self.tracker_id.take() {
                PopoverTracker::global().clear_if_active(id);
            }
        }
    }

    fn ipc_is_visible(&self) -> bool {
        self.window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.logically_open.get())
    }

    fn monitor_connector(&self) -> Option<String> {
        let widget_ref = self.bar_widget.borrow();
        widget_ref
            .as_ref()
            .and_then(|w| w.native())
            .and_then(|n| n.surface())
            .and_then(|s| s.display().monitor_at_surface(&s))
            .and_then(|m| m.connector())
            .map(|c| c.to_string())
    }
}

/// Adapter to make QuickSettingsWindowHandle work with PopoverTracker.
///
/// This wraps the shared window reference and implements `Dismissible` so that
/// other popups can dismiss Quick Settings when opening. The window is hidden
/// (not destroyed) — it stays alive for instant re-show.
struct QuickSettingsDismissible {
    window: Rc<RefCell<Option<Rc<QuickSettingsWindow>>>>,
    tracker_id: Rc<Cell<Option<PopoverId>>>,
}

impl Dismissible for QuickSettingsDismissible {
    fn dismiss(&self) {
        if let Some(qs) = self.window.borrow().as_ref() {
            qs.hide_panel();
        }
        // Clear our tracker ID since we've been dismissed
        self.tracker_id.set(None);
    }

    fn is_visible(&self) -> bool {
        self.window
            .borrow()
            .as_ref()
            .is_some_and(|w| w.logically_open.get())
    }
}
