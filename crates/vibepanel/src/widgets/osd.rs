//! On-Screen Display (OSD) overlay for brightness and volume changes.
//!
//! - Small overlay window with icon + slider
//! - Layer-shell OVERLAY, non-intrusive, auto-hiding
//! - Reacts to `BrightnessService` and `AudioService` changes, ignoring the initial sync

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::services::audio::AudioService;
use crate::services::brightness::BrightnessService;
use crate::styles::{color, osd};

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Application, Box as GtkBox, Image, Label, Orientation, Scale};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{debug, warn};

use vibepanel_core::config::OsdConfig;

use crate::services::audio::AudioSnapshot;
use crate::services::brightness::BrightnessSnapshot;
use crate::services::icons::IconsService;
use crate::services::osd_ipc::{OsdIpcListener, OsdMessage};
use crate::services::surfaces::SurfaceStyleManager;

/// Valid OSD positions for anchoring.
const VALID_POSITIONS: &[&str] = &["bottom", "left", "right", "top"];
const DEFAULT_POSITION: &str = "bottom";

fn normalize_position(position: &str) -> String {
    if VALID_POSITIONS.contains(&position) {
        position.to_string()
    } else {
        warn!(
            "Invalid OSD position '{}', using '{}'. Valid options: {}",
            position,
            DEFAULT_POSITION,
            VALID_POSITIONS.join(", ")
        );
        DEFAULT_POSITION.to_string()
    }
}

/// Simple OSD widget containing an icon and a fat slider.
///
/// This is a lightweight container without the full BaseWidget machinery.
pub struct OsdWidget {
    root: GtkBox,
    /// Normal content: icon + slider in a row
    normal_content: GtkBox,
    scale: Scale,
    /// Unavailable content: big icon + message centered
    unavailable_content: GtkBox,
    unavailable_icon: Image,
    unavailable_label: Label,
}

impl OsdWidget {
    pub fn new(orientation: Orientation, icon_size: i32) -> Self {
        let root = GtkBox::new(Orientation::Vertical, 0);
        root.add_css_class(osd::WIDGET);

        // === Normal content: icon + slider ===
        let normal_content = GtkBox::new(orientation, 12);
        normal_content.add_css_class(osd::NORMAL);

        let icon_image = Image::from_icon_name("audio-volume-medium-symbolic");
        icon_image.set_pixel_size(icon_size);
        icon_image.add_css_class(osd::ICON);
        icon_image.set_valign(Align::Center);
        icon_image.set_halign(Align::Center);
        normal_content.append(&icon_image);

        // Slider (display only)
        let scale = Scale::with_range(orientation, 0.0, 100.0, 1.0);
        scale.set_draw_value(false);
        scale.set_sensitive(false);
        scale.add_css_class(osd::SLIDER);

        if orientation == Orientation::Horizontal {
            scale.set_hexpand(true);
            scale.set_size_request(200, -1);
        } else {
            scale.set_vexpand(true);
            scale.set_size_request(-1, 200);
            // High values at top
            scale.set_inverted(true);
        }

        normal_content.append(&scale);
        root.append(&normal_content);

        // === Unavailable content: centered icon + label ===
        let unavailable_content = GtkBox::new(Orientation::Vertical, 8);
        unavailable_content.add_css_class(osd::UNAVAILABLE);
        unavailable_content.set_valign(Align::Center);
        unavailable_content.set_halign(Align::Center);
        unavailable_content.set_visible(false);

        let unavailable_icon = Image::from_icon_name("audio-volume-muted-symbolic");
        unavailable_icon.set_pixel_size(32);
        unavailable_icon.add_css_class(osd::UNAVAILABLE_ICON);
        unavailable_icon.add_css_class(color::MUTED);
        unavailable_content.append(&unavailable_icon);

        let unavailable_label = Label::new(Some("Unavailable"));
        unavailable_label.add_css_class(osd::UNAVAILABLE_LABEL);
        unavailable_label.add_css_class(color::MUTED);
        unavailable_content.append(&unavailable_label);

        root.append(&unavailable_content);

        Self {
            root,
            normal_content,
            scale,
            unavailable_content,
            unavailable_icon,
            unavailable_label,
        }
    }

    pub fn widget(&self) -> &GtkBox {
        &self.root
    }

    pub fn set_value(&self, value: u32) {
        let v = value.clamp(0, 100) as f64;
        self.scale.set_value(v);
        // Show normal content, hide unavailable
        self.normal_content.set_visible(true);
        self.unavailable_content.set_visible(false);
    }

    /// Set the widget to "unavailable" state with icon and message.
    pub fn set_unavailable(&self, icon_name: &str, message: &str) {
        // Update unavailable content
        self.unavailable_icon.set_icon_name(Some(icon_name));
        self.unavailable_label.set_text(message);
        // Show unavailable content, hide normal
        self.normal_content.set_visible(false);
        self.unavailable_content.set_visible(true);
    }

    pub fn set_icon(&self, icon_name: &str) {
        // Try IconsService first (for theme integration).
        let icons = IconsService::global();
        let handle = icons.create_icon(icon_name, &[osd::ICON]);
        // The icon handle sets size via CSS or internally, just use it
        // Replace first child of normal_content with themed icon
        if let Some(first_child) = self.normal_content.first_child() {
            self.normal_content.remove(&first_child);
        }
        self.normal_content.prepend(&handle.widget());
    }
}

/// Overlay window for displaying the OSD.
///
/// Uses layer-shell to create a floating overlay that:
/// - Appears above other windows (OVERLAY layer)
/// - Does not take keyboard focus
/// - Does not reserve screen space (exclusive_zone = 0)
/// - Auto-hides after a timeout
/// - Listens for IPC messages from CLI commands
pub struct OsdOverlay {
    window: gtk4::Window,
    osd_widget: OsdWidget,
    timeout_ms: u32,
    hide_source: RefCell<Option<glib::SourceId>>,

    // Brightness state tracking.
    brightness_baseline_seen: Cell<bool>,
    last_brightness: Cell<u32>,

    // Audio state tracking.
    audio_baseline_seen: Cell<bool>,
    last_volume: Cell<u32>,
    last_muted: Cell<bool>,

    // IPC listener for CLI commands (kept alive for the lifetime of the overlay).
    _ipc_listener: RefCell<Option<Rc<RefCell<OsdIpcListener>>>>,
}

impl OsdOverlay {
    /// Create a new OSD overlay bound to the given application and config.
    ///
    /// The overlay subscribes to the global `BrightnessService` and will
    /// show when the brightness percentage changes (after the initial sync).
    pub fn new(app: &Application, osd_config: &OsdConfig) -> Rc<Self> {
        let position = normalize_position(&osd_config.position);
        let timeout_ms = osd_config.timeout_ms;

        let window = gtk4::Window::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .build();

        window.add_css_class(osd::WINDOW);

        // Set up layer shell defaults.
        Self::setup_layer_shell_defaults(&window);

        // Layout/orientation based on position.
        let is_vertical = matches!(position.as_str(), "left" | "right");
        let orientation = if is_vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        };

        // Content container with surface styling.
        let container = GtkBox::new(Orientation::Vertical, 0);
        container.add_css_class(osd::CONTAINER);
        if is_vertical {
            container.add_css_class(osd::VERTICAL);
        } else {
            container.add_css_class(osd::HORIZONTAL);
        }

        // Apply theme surface styles with larger widget radius for pill shape at max radius.
        SurfaceStyleManager::global().apply_surface_styles_with_radius(
            &container,
            true,
            "var(--radius-widget-lg)",
        );

        // Child OSD widget.
        let osd_widget = OsdWidget::new(orientation, 24);
        container.append(osd_widget.widget());
        window.set_child(Some(&container));

        // Apply Pango font attributes to all labels if enabled in config.
        // This is the central hook for OSD - widgets create standard
        // GTK labels, and we apply Pango attributes here after the tree is built.
        SurfaceStyleManager::global().apply_pango_attrs_all(&container);

        // Anchor window according to position.
        Self::apply_position(&window, &position);

        let overlay = Rc::new(Self {
            window,
            osd_widget,
            timeout_ms,
            hide_source: RefCell::new(None),
            brightness_baseline_seen: Cell::new(false),
            last_brightness: Cell::new(0),
            audio_baseline_seen: Cell::new(false),
            last_volume: Cell::new(0),
            last_muted: Cell::new(false),
            _ipc_listener: RefCell::new(None),
        });

        overlay.connect_brightness();
        overlay.connect_audio();
        overlay.connect_ipc();

        overlay
    }

    /// Show the overlay with a specific icon + value.
    pub fn show_value(self: &Rc<Self>, icon_name: &str, value: u32) {
        self.osd_widget.set_icon(icon_name);
        self.osd_widget.set_value(value);

        self.window.set_visible(true);
        self.reset_hide_timer();
    }

    /// Brightness-specific helper: compute icon from percent and show.
    pub fn show_brightness(self: &Rc<Self>, value: u32) {
        let icon = if value == 0 {
            "display-brightness-off-symbolic"
        } else if value < 33 {
            "display-brightness-low-symbolic"
        } else if value < 67 {
            "display-brightness-medium-symbolic"
        } else {
            "display-brightness-high-symbolic"
        };
        self.show_value(icon, value);
    }

    /// Volume-specific helper: compute icon from volume/mute state and show.
    pub fn show_volume(self: &Rc<Self>, volume: u32, muted: bool) {
        let icon = if muted || volume == 0 {
            "audio-volume-muted-symbolic"
        } else if volume < 33 {
            "audio-volume-low-symbolic"
        } else if volume < 67 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        // Clamp to 100 for display, even though we allow overdrive internally.
        self.show_value(icon, volume.min(100));
    }

    /// Show OSD indicating volume control is unavailable (device not ready).
    pub fn show_volume_unavailable(self: &Rc<Self>) {
        self.osd_widget
            .set_unavailable("audio-volume-muted-symbolic", "Play audio to enable");

        self.window.set_visible(true);
        self.reset_hide_timer();
    }

    // Internal: layer shell

    fn setup_layer_shell_defaults(window: &gtk4::Window) {
        if gdk::Display::default().is_some() {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_exclusive_zone(0);

            if let Err(err) = std::panic::catch_unwind(|| {
                window.set_keyboard_mode(KeyboardMode::None);
            }) {
                debug!("OsdOverlay: failed to set keyboard mode: {:?}", err);
            }
        }
    }

    fn apply_position(window: &gtk4::Window, position: &str) {
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, false);
        }

        match position {
            "bottom" => {
                window.set_anchor(Edge::Bottom, true);
                window.set_margin(Edge::Bottom, 48);
            }
            "top" => {
                window.set_anchor(Edge::Top, true);
                window.set_margin(Edge::Top, 48);
            }
            "left" => {
                window.set_anchor(Edge::Left, true);
                window.set_margin(Edge::Left, 24);
            }
            "right" => {
                window.set_anchor(Edge::Right, true);
                window.set_margin(Edge::Right, 24);
            }
            // normalize_position guarantees only valid values, but match must be exhaustive
            _ => unreachable!("Invalid position after normalization"),
        }
    }

    fn reset_hide_timer(self: &Rc<Self>) {
        if self.timeout_ms == 0 {
            return;
        }

        if let Some(src) = self.hide_source.borrow_mut().take() {
            src.remove();
        }

        let timeout = self.timeout_ms;
        let this_weak = Rc::downgrade(self);

        let source_id = glib::timeout_add_local(Duration::from_millis(timeout as u64), move || {
            if let Some(this) = this_weak.upgrade() {
                this.window.set_visible(false);
                *this.hide_source.borrow_mut() = None;
            }
            glib::ControlFlow::Break
        });

        *self.hide_source.borrow_mut() = Some(source_id);
    }

    // Internal: brightness integration

    fn connect_brightness(self: &Rc<Self>) {
        let service = BrightnessService::global();
        let this_weak = Rc::downgrade(self);

        service.connect(move |snapshot: &BrightnessSnapshot| {
            if let Some(this) = this_weak.upgrade() {
                this.on_brightness_changed(snapshot);
            }
        });
    }

    fn on_brightness_changed(self: &Rc<Self>, snapshot: &BrightnessSnapshot) {
        // Ignore if brightness is not currently controllable/meaningful.
        if !snapshot.available {
            // Reset baseline so that when it becomes available again we treat
            // the next value as a fresh baseline.
            self.brightness_baseline_seen.set(false);
            return;
        }

        let value = snapshot.percent.clamp(0, 100);

        // Use an explicit readiness + baseline handshake instead of a
        // time-based grace period. We only start showing OSD once the
        // service reports itself as ready and we've captured a baseline.
        let service_ready = BrightnessService::global().is_ready();
        if !service_ready {
            self.brightness_baseline_seen.set(false);
            self.last_brightness.set(value);
            return;
        }

        if !self.brightness_baseline_seen.get() {
            self.brightness_baseline_seen.set(true);
            self.last_brightness.set(value);
            return;
        }

        if self.last_brightness.get() == value {
            return;
        }

        self.last_brightness.set(value);
        self.show_brightness(value);
    }

    // Internal: audio integration

    fn connect_audio(self: &Rc<Self>) {
        let service = AudioService::global();
        let this_weak = Rc::downgrade(self);

        service.connect(move |snapshot: &AudioSnapshot| {
            if let Some(this) = this_weak.upgrade() {
                this.on_audio_changed(snapshot);
            }
        });
    }

    fn on_audio_changed(self: &Rc<Self>, snapshot: &AudioSnapshot) {
        // Ignore if audio is not currently controllable/meaningful.
        if !snapshot.available {
            // Reset baseline so that when it becomes available again we treat
            // the next value as a fresh baseline.
            self.audio_baseline_seen.set(false);
            return;
        }

        let volume = snapshot.volume;
        let muted = snapshot.muted;
        let control_available = snapshot.control_available;

        let service = AudioService::global();

        // Keep the OSD quiet while the audio service is in its initial
        // post-connection settle period. Pulse may emit several updates as
        // devices are discovered and defaults are resolved. We track the
        // latest values during this time so that when the settle period
        // ends, we have a proper baseline to compare against.
        if service.in_initial_settle() {
            self.audio_baseline_seen.set(true);
            self.last_volume.set(volume);
            self.last_muted.set(muted);
            return;
        }

        // If we haven't seen any values yet (service wasn't ready), treat
        // this as baseline establishment.
        if !service.is_ready() || !self.audio_baseline_seen.get() {
            self.audio_baseline_seen.set(true);
            self.last_volume.set(volume);
            self.last_muted.set(muted);
            return;
        }

        // Check if anything changed from our tracked baseline.
        if self.last_volume.get() == volume && self.last_muted.get() == muted {
            return;
        }

        self.last_volume.set(volume);
        self.last_muted.set(muted);

        // If control is not available (sink suspended), show a "blocked" icon
        if !control_available {
            self.show_volume_unavailable();
            return;
        }

        self.show_volume(volume, muted);
    }

    // Internal: IPC integration (for CLI commands)

    fn connect_ipc(self: &Rc<Self>) {
        let listener = match OsdIpcListener::new() {
            Some(l) => l,
            None => {
                debug!("OSD IPC listener not available (non-fatal)");
                return;
            }
        };

        let this_weak = Rc::downgrade(self);

        listener.borrow().connect(move |msg| {
            let Some(this) = this_weak.upgrade() else {
                return;
            };

            match msg {
                OsdMessage::Volume { percent, muted } => {
                    debug!("OSD IPC: received volume {}% muted={}", percent, muted);
                    // Notify AudioService of the external volume request so
                    // behavioral detection can track whether the backend responded.
                    let audio = AudioService::global();
                    audio.note_external_volume_request(percent);

                    // Check if control is available before showing normal volume OSD
                    let snapshot = audio.current();
                    if snapshot.available && !snapshot.control_available {
                        // Backend is up but not accepting volume changes
                        this.show_volume_unavailable();
                    } else {
                        this.show_volume(percent, muted);
                    }
                }
                OsdMessage::VolumeUnavailable => {
                    debug!("OSD IPC: received volume_unavailable");
                    this.show_volume_unavailable();
                }
                OsdMessage::Brightness { percent } => {
                    debug!("OSD IPC: received brightness {}%", percent);
                    this.show_brightness(percent);
                }
            }
        });

        // Store the listener to keep it alive.
        *self._ipc_listener.borrow_mut() = Some(listener);
        debug!("OSD IPC listener connected");
    }
}
