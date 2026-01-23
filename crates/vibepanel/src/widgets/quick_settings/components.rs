//! Composable UI components for Quick Settings.
//!
//! This module provides low-level, reusable components that can be composed
//! together to build complex UI patterns:

// Allow unused builder methods - they provide API flexibility for future use
#![allow(dead_code)]
//!
//! - [`IconButton`] - Icon in a button, interactive or non-interactive
//! - [`AccentSlider`] - Slider with accent color styling
//! - [`ExpanderButton`] - Chevron button for expand/collapse
//! - [`SliderRow`] - Composer for icon + slider + optional trailing widget
//!
//! # Design Philosophy
//!
//! Components are designed to be:
//! - **Composable**: Mix and match to create new patterns
//! - **Consistent**: Shared styling via CSS classes
//! - **Simple**: Each component does one thing well
//!
//! # Example
//!
//! ```rust,ignore
//! // Simple brightness row
//! let row = SliderRow::builder()
//!     .icon("display-brightness-symbolic")
//!     .range(1.0, 100.0)
//!     .build();
//!
//! // Complex audio row with interactive mute and expander
//! let row = SliderRow::builder()
//!     .icon("audio-volume-high-symbolic")
//!     .interactive_icon(true)
//!     .range(0.0, 100.0)
//!     .with_expander(true)
//!     .build();
//! ```

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CssProvider, Label, ListBoxRow, Orientation, Scale, ToggleButton,
};

use crate::services::icons::{IconHandle, IconsService};

/// CSS class for slider row container.
const CSS_SLIDER_ROW: &str = "slider-row";

/// CSS class for icon buttons in slider rows.
const CSS_SLIDER_ICON_BTN: &str = "slider-icon-btn";

/// CSS class for invisible spacer buttons.
const CSS_SLIDER_SPACER: &str = "slider-spacer";

/// Result of building an icon button.
pub struct IconButtonResult {
    /// The button widget.
    pub button: Button,
    /// Handle to the icon (for dynamic updates).
    pub icon_handle: IconHandle,
}

/// Builder for icon buttons.
///
/// Creates a button containing an icon, which can be either interactive
/// (responds to clicks) or non-interactive (visual only).
///
/// # Example
///
/// ```rust,ignore
/// // Non-interactive icon (e.g., brightness indicator)
/// let result = IconButton::new("display-brightness-symbolic").build();
///
/// // Interactive icon (e.g., mute toggle)
/// let result = IconButton::new("audio-volume-high-symbolic")
///     .interactive(true)
///     .build();
/// ```
pub struct IconButton {
    icon_name: String,
    interactive: bool,
    css_classes: Vec<String>,
}

impl IconButton {
    /// Create a new icon button builder.
    pub fn new(icon_name: &str) -> Self {
        Self {
            icon_name: icon_name.to_string(),
            interactive: false,
            css_classes: vec!["vp-primary".to_string()],
        }
    }

    /// Set whether the button is interactive (clickable).
    ///
    /// Non-interactive buttons are disabled and serve as visual indicators only.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Add CSS classes to the icon.
    pub fn icon_classes(mut self, classes: &[&str]) -> Self {
        self.css_classes = classes.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Build the icon button.
    pub fn build(self) -> IconButtonResult {
        let button = Button::new();
        button.set_has_frame(false);
        button.add_css_class(CSS_SLIDER_ICON_BTN);
        // Prevent vertical stretching in horizontal boxes
        button.set_valign(gtk4::Align::Center);

        if !self.interactive {
            button.set_sensitive(false);
        } else {
            button.set_can_focus(true);
        }

        let icons = IconsService::global();
        let class_refs: Vec<&str> = self.css_classes.iter().map(|s| s.as_str()).collect();
        let icon_handle = icons.create_icon(&self.icon_name, &class_refs);

        // Center the icon within the button's padded area
        let icon_widget = icon_handle.widget();
        icon_widget.set_halign(gtk4::Align::Center);
        icon_widget.set_valign(gtk4::Align::Center);
        button.set_child(Some(&icon_widget));

        IconButtonResult {
            button,
            icon_handle,
        }
    }
}

/// Result of building an accent slider.
pub struct AccentSliderResult {
    /// The slider widget.
    pub slider: Scale,
}

/// Builder for sliders with accent color styling.
///
/// Creates a horizontal slider that uses the theme's accent color for
/// the filled portion and knob, overriding GTK theme defaults.
///
/// # Example
///
/// ```rust,ignore
/// let result = AccentSlider::new()
///     .range(0.0, 100.0)
///     .step(1.0)
///     .build();
/// ```
pub struct AccentSlider {
    min: f64,
    max: f64,
    step: f64,
}

impl AccentSlider {
    /// Create a new accent slider builder with default range (0-100).
    pub fn new() -> Self {
        Self {
            min: 0.0,
            max: 100.0,
            step: 1.0,
        }
    }

    /// Set the slider range (min, max).
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    /// Set the slider step increment.
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// Build the accent slider.
    pub fn build(self) -> AccentSliderResult {
        let slider = Scale::with_range(Orientation::Horizontal, self.min, self.max, self.step);
        slider.set_hexpand(true);
        slider.set_draw_value(false);
        apply_accent_styling(&slider);

        AccentSliderResult { slider }
    }
}

impl Default for AccentSlider {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of building an expander button.
pub struct ExpanderButtonResult {
    /// The button widget.
    pub button: Button,
    /// Handle to the chevron icon (for rotation animation).
    pub icon_handle: IconHandle,
}

/// Builder for expander (chevron) buttons.
///
/// Creates a button with a chevron icon that can be rotated to indicate
/// expanded/collapsed state.
///
/// # Example
///
/// ```rust,ignore
/// let result = ExpanderButton::new().build();
///
/// // Toggle expanded state
/// if expanded {
///     result.icon_handle.widget().add_css_class("expanded");
/// } else {
///     result.icon_handle.widget().remove_css_class("expanded");
/// }
/// ```
pub struct ExpanderButton {
    icon_name: String,
}

impl ExpanderButton {
    /// Create a new expander button builder.
    pub fn new() -> Self {
        Self {
            icon_name: "pan-down-symbolic".to_string(),
        }
    }

    /// Set a custom icon name (default: "pan-down-symbolic").
    pub fn icon(mut self, icon_name: &str) -> Self {
        self.icon_name = icon_name.to_string();
        self
    }

    /// Build the expander button.
    pub fn build(self) -> ExpanderButtonResult {
        let button = Button::new();
        button.set_has_frame(false);
        button.add_css_class(crate::styles::qs::TOGGLE_MORE);
        // Prevent vertical stretching in horizontal boxes
        button.set_valign(gtk4::Align::Center);

        let icons = IconsService::global();
        let icon_handle = icons.create_icon(
            &self.icon_name,
            &[crate::styles::qs::TOGGLE_MORE_ICON, "vp-primary"],
        );

        // Center the icon within the button's hover area
        let icon_widget = icon_handle.widget();
        icon_widget.set_halign(gtk4::Align::Center);
        icon_widget.set_valign(gtk4::Align::Center);
        button.set_child(Some(&icon_widget));

        ExpanderButtonResult {
            button,
            icon_handle,
        }
    }
}

impl Default for ExpanderButton {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of building a card label.
pub struct CardLabelResult {
    /// The container (vertical box with label + optional subtitle).
    pub container: GtkBox,
    /// The title label.
    pub title: Label,
    /// Optional subtitle label.
    pub subtitle: Option<Label>,
}

/// Builder for label + subtitle stacks used in cards and list rows.
///
/// Creates a vertical box containing a title label and optional subtitle,
/// with consistent ellipsis, alignment, and styling.
///
/// # Example
///
/// ```rust,ignore
/// // For toggle cards (narrower)
/// let result = CardLabel::new("Wi-Fi")
///     .subtitle("Connected")
///     .width_chars(16)
///     .title_class("qs-toggle-label")
///     .subtitle_class("qs-toggle-subtitle")
///     .build();
///
/// // For list rows (wider)
/// let result = CardLabel::new("My Network")
///     .subtitle("Secured • 85%")
///     .width_chars(22)
///     .title_class("qs-row-title")
///     .subtitle_class("qs-row-subtitle")
///     .build();
/// ```
pub struct CardLabel {
    title: String,
    subtitle: Option<String>,
    subtitle_widget: Option<gtk4::Widget>,
    width_chars: i32,
    subtitle_width_chars: i32,
    title_class: String,
    subtitle_class: String,
}

impl CardLabel {
    /// Create a new card label builder with the given title.
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            subtitle: None,
            subtitle_widget: None,
            width_chars: 16,
            subtitle_width_chars: 22,
            title_class: String::new(),
            subtitle_class: String::new(),
        }
    }

    /// Set the subtitle text.
    pub fn subtitle(mut self, subtitle: &str) -> Self {
        self.subtitle = Some(subtitle.to_string());
        self
    }

    /// Set optional subtitle (convenience for Option<&str>).
    pub fn subtitle_opt(mut self, subtitle: Option<&str>) -> Self {
        self.subtitle = subtitle.map(|s| s.to_string());
        self
    }

    /// Set a custom subtitle widget (takes precedence over subtitle text).
    pub fn subtitle_widget(mut self, widget: gtk4::Widget) -> Self {
        self.subtitle_widget = Some(widget);
        self
    }

    /// Set the width in characters for the title label.
    pub fn width_chars(mut self, width: i32) -> Self {
        self.width_chars = width;
        self
    }

    /// Set the width in characters for the subtitle label.
    pub fn subtitle_width_chars(mut self, width: i32) -> Self {
        self.subtitle_width_chars = width;
        self
    }

    /// Set the CSS class for the title label.
    pub fn title_class(mut self, class: &str) -> Self {
        self.title_class = class.to_string();
        self
    }

    /// Set the CSS class for the subtitle label.
    pub fn subtitle_class(mut self, class: &str) -> Self {
        self.subtitle_class = class.to_string();
        self
    }

    /// Build the card label.
    pub fn build(self) -> CardLabelResult {
        use crate::styles::color;
        use gtk4::Align;
        use gtk4::pango::EllipsizeMode;

        let container = GtkBox::new(Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_valign(Align::Center);

        // Title label
        let title = Label::new(Some(&self.title));
        title.set_xalign(0.0);
        title.set_ellipsize(EllipsizeMode::End);
        title.set_single_line_mode(true);
        title.set_width_chars(self.width_chars);
        title.set_max_width_chars(self.width_chars);
        if !self.title_class.is_empty() {
            title.add_css_class(&self.title_class);
        }
        title.add_css_class(color::PRIMARY);
        container.append(&title);

        // Optional subtitle: custom widget takes precedence over text
        let subtitle = if let Some(widget) = self.subtitle_widget {
            container.append(&widget);
            None // No Label to return when using custom widget
        } else if let Some(subtitle_text) = &self.subtitle {
            let sub = Label::new(Some(subtitle_text));
            sub.set_xalign(0.0);
            sub.set_ellipsize(EllipsizeMode::End);
            sub.set_single_line_mode(true);
            sub.set_width_chars(self.subtitle_width_chars);
            sub.set_max_width_chars(self.subtitle_width_chars);
            if !self.subtitle_class.is_empty() {
                sub.add_css_class(&self.subtitle_class);
            }
            sub.add_css_class(color::MUTED);
            if subtitle_text.is_empty() {
                sub.set_visible(false);
            }
            container.append(&sub);
            Some(sub)
        } else {
            None
        };

        CardLabelResult {
            container,
            title,
            subtitle,
        }
    }
}

/// Create an invisible spacer button for row alignment.
///
/// This matches the size of an expander button but is invisible,
/// useful for aligning rows that don't have an expander.
fn create_spacer() -> Button {
    let spacer = Button::new();
    spacer.set_has_frame(false);
    spacer.set_sensitive(false);
    spacer.set_opacity(0.0);
    spacer.add_css_class(CSS_SLIDER_SPACER);

    // Add invisible icon to match expander button size
    // Use same classes as expander icon for consistent sizing
    let icons = IconsService::global();
    let spacer_icon =
        icons.create_icon("pan-down-symbolic", &[crate::styles::qs::TOGGLE_MORE_ICON]);
    spacer_icon.widget().set_opacity(0.0);
    spacer.set_child(Some(&spacer_icon.widget()));

    spacer
}

/// Result of building a slider row.
pub struct SliderRowResult {
    /// The outer row container.
    pub container: GtkBox,
    /// The leading icon button.
    pub icon_button: Button,
    /// Handle to the leading icon.
    pub icon_handle: IconHandle,
    /// The slider widget.
    pub slider: Scale,
    /// The expander button (if requested).
    pub expander_button: Option<Button>,
    /// Handle to the expander icon (if requested).
    pub expander_icon: Option<IconHandle>,
}

/// Builder for slider rows.
///
/// Composes [`IconButton`], [`AccentSlider`], and optionally [`ExpanderButton`]
/// into a horizontal row layout.
///
/// # Example
///
/// ```rust,ignore
/// // Simple brightness row
/// let row = SliderRow::builder()
///     .icon("display-brightness-symbolic")
///     .range(1.0, 100.0)
///     .with_spacer(true)  // Align with audio row
///     .build();
///
/// // Audio row with mute toggle and expander
/// let row = SliderRow::builder()
///     .icon("audio-volume-high-symbolic")
///     .interactive_icon(true)
///     .range(0.0, 100.0)
///     .with_expander(true)
///     .build();
/// ```
pub struct SliderRow {
    icon_name: String,
    interactive_icon: bool,
    icon_classes: Vec<String>,
    min: f64,
    max: f64,
    step: f64,
    with_expander: bool,
    with_spacer: bool,
    spacing: i32,
}

impl SliderRow {
    /// Create a new slider row builder.
    pub fn builder() -> Self {
        Self {
            icon_name: String::new(),
            interactive_icon: false,
            icon_classes: vec!["vp-primary".to_string()],
            min: 0.0,
            max: 100.0,
            step: 1.0,
            with_expander: false,
            with_spacer: false,
            spacing: 4,
        }
    }

    /// Set the icon name for the leading button.
    pub fn icon(mut self, icon_name: &str) -> Self {
        self.icon_name = icon_name.to_string();
        self
    }

    /// Set whether the icon button is interactive (clickable).
    pub fn interactive_icon(mut self, interactive: bool) -> Self {
        self.interactive_icon = interactive;
        self
    }

    /// Set CSS classes for the icon.
    pub fn icon_classes(mut self, classes: &[&str]) -> Self {
        self.icon_classes = classes.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Set the slider range (min, max).
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    /// Set the slider step increment.
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// Add an expander button at the end of the row.
    pub fn with_expander(mut self, with_expander: bool) -> Self {
        self.with_expander = with_expander;
        self
    }

    /// Add an invisible spacer at the end of the row.
    ///
    /// Useful for aligning with rows that have an expander button.
    /// Ignored if `with_expander` is true.
    pub fn with_spacer(mut self, with_spacer: bool) -> Self {
        self.with_spacer = with_spacer;
        self
    }

    /// Set the horizontal spacing between children.
    pub fn spacing(mut self, spacing: i32) -> Self {
        self.spacing = spacing;
        self
    }

    /// Build the slider row.
    pub fn build(self) -> SliderRowResult {
        let container = GtkBox::new(Orientation::Horizontal, self.spacing);
        container.add_css_class(CSS_SLIDER_ROW);

        // Build icon button
        let class_refs: Vec<&str> = self.icon_classes.iter().map(|s| s.as_str()).collect();
        let icon_result = IconButton::new(&self.icon_name)
            .interactive(self.interactive_icon)
            .icon_classes(&class_refs)
            .build();
        container.append(&icon_result.button);

        // Build slider
        let slider_result = AccentSlider::new()
            .range(self.min, self.max)
            .step(self.step)
            .build();
        container.append(&slider_result.slider);

        // Build trailing widget (expander or spacer)
        let (expander_button, expander_icon) = if self.with_expander {
            let expander_result = ExpanderButton::new().build();
            container.append(&expander_result.button);
            (
                Some(expander_result.button),
                Some(expander_result.icon_handle),
            )
        } else if self.with_spacer {
            let spacer = create_spacer();
            container.append(&spacer);
            (None, None)
        } else {
            (None, None)
        };

        SliderRowResult {
            container,
            icon_button: icon_result.button,
            icon_handle: icon_result.icon_handle,
            slider: slider_result.slider,
            expander_button,
            expander_icon,
        }
    }
}

/// Result of building a toggle card.
pub struct ToggleCardResult {
    /// The outer card container box.
    pub card: GtkBox,
    /// The main toggle button (power on/off).
    pub toggle: ToggleButton,
    /// Handle to the icon for dynamic updates.
    pub icon_handle: IconHandle,
    /// Title label handle for dynamic renaming.
    pub title: Label,
    /// Optional subtitle label (e.g., "Connected" or SSID).
    pub subtitle: Option<Label>,
    /// Optional expander button.
    pub expander_button: Option<Button>,
    /// Optional expander arrow handle (chevron) for expandable sections.
    pub expander_icon: Option<IconHandle>,
}

/// Builder for toggle cards (Wi-Fi, Bluetooth, VPN, etc.).
///
/// Creates a card with a toggle button, icon, label, optional subtitle,
/// and optional expander chevron.
///
/// # Example
///
/// ```rust,ignore
/// let result = ToggleCard::builder()
///     .icon("network-wireless-signal-excellent-symbolic")
///     .label("Wi-Fi")
///     .subtitle("Connected")
///     .with_expander(true)
///     .build();
/// ```
pub struct ToggleCard {
    icon_name: String,
    label_text: String,
    subtitle_text: Option<String>,
    subtitle_widget: Option<gtk4::Widget>,
    active: bool,
    sensitive: bool,
    icon_active: bool,
    with_expander: bool,
}

impl ToggleCard {
    /// Create a new toggle card builder.
    pub fn builder() -> Self {
        Self {
            icon_name: String::new(),
            label_text: String::new(),
            subtitle_text: None,
            subtitle_widget: None,
            active: false,
            sensitive: true,
            icon_active: false,
            with_expander: true,
        }
    }

    /// Set the icon name.
    pub fn icon(mut self, icon_name: &str) -> Self {
        self.icon_name = icon_name.to_string();
        self
    }

    /// Set the main label text.
    pub fn label(mut self, label_text: &str) -> Self {
        self.label_text = label_text.to_string();
        self
    }

    /// Set the subtitle text.
    pub fn subtitle(mut self, subtitle_text: &str) -> Self {
        self.subtitle_text = Some(subtitle_text.to_string());
        self
    }

    /// Set a custom subtitle widget (takes precedence over subtitle text).
    pub fn subtitle_widget(mut self, widget: gtk4::Widget) -> Self {
        self.subtitle_widget = Some(widget);
        self
    }

    /// Set whether the toggle is active.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Set whether the toggle is sensitive (enabled).
    pub fn sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Set whether to apply active styling to the icon.
    pub fn icon_active(mut self, icon_active: bool) -> Self {
        self.icon_active = icon_active;
        self
    }

    /// Add an expander chevron.
    pub fn with_expander(mut self, with_expander: bool) -> Self {
        self.with_expander = with_expander;
        self
    }

    /// Build the toggle card.
    pub fn build(self) -> ToggleCardResult {
        use crate::styles::{button, card, color, icon, qs};
        use gtk4::{Align, ToggleButton};

        let card_box = GtkBox::new(Orientation::Horizontal, 4);
        card_box.add_css_class(card::QS);
        card_box.add_css_class(card::BASE);
        card_box.set_hexpand(true);

        // Main toggle button
        let toggle = ToggleButton::new();
        toggle.set_active(self.active);
        toggle.set_hexpand(true);
        toggle.set_vexpand(true);
        toggle.set_halign(Align::Fill);
        toggle.set_valign(Align::Fill);
        toggle.set_sensitive(self.sensitive);
        toggle.add_css_class(button::RESET);

        // Content inside the toggle
        let content = GtkBox::new(Orientation::Horizontal, 6);
        content.set_hexpand(true);

        // Icon
        let icons = IconsService::global();
        let icon_handle = icons.create_icon(
            &self.icon_name,
            &[icon::TEXT, qs::TOGGLE_ICON, color::PRIMARY],
        );
        if self.icon_active {
            icon_handle.add_css_class(crate::styles::state::ICON_ACTIVE);
            icon_handle.remove_css_class(color::PRIMARY);
        }
        content.append(&icon_handle.widget());

        // Label (with optional subtitle) using CardLabel
        let label_result = if let Some(widget) = self.subtitle_widget {
            CardLabel::new(&self.label_text)
                .subtitle_widget(widget)
                .width_chars(16)
                .title_class(qs::TOGGLE_LABEL)
                .subtitle_class(qs::TOGGLE_SUBTITLE)
                .build()
        } else {
            CardLabel::new(&self.label_text)
                .subtitle_opt(self.subtitle_text.as_deref())
                .width_chars(16)
                .title_class(qs::TOGGLE_LABEL)
                .subtitle_class(qs::TOGGLE_SUBTITLE)
                .build()
        };
        content.append(&label_result.container);

        toggle.set_child(Some(&content));
        card_box.append(&toggle);

        // Expander chevron
        let (expander_button, expander_icon) = if self.with_expander {
            let expander_result = ExpanderButton::new().build();
            card_box.append(&expander_result.button);
            (
                Some(expander_result.button),
                Some(expander_result.icon_handle),
            )
        } else {
            (None, None)
        };

        ToggleCardResult {
            card: card_box,
            toggle,
            icon_handle,
            title: label_result.title,
            subtitle: label_result.subtitle,
            expander_button,
            expander_icon,
        }
    }
}

/// Result of building a list row.
pub struct ListRowResult {
    /// The list box row widget.
    pub row: ListBoxRow,
    /// The title label.
    pub title: Label,
    /// Optional subtitle label.
    pub subtitle: Option<Label>,
}

/// Builder for list rows (Wi-Fi networks, Bluetooth devices, VPN connections).
///
/// Creates a row with a title, optional subtitle, optional leading icon,
/// and optional trailing widget.
///
/// # Example
///
/// ```rust,ignore
/// let result = ListRow::builder()
///     .title("My Network")
///     .subtitle("Connected • Secured")
///     .leading_widget(icon_widget)
///     .trailing_widget(menu_button)
///     .build();
/// ```
pub struct ListRow {
    title: String,
    subtitle: Option<String>,
    subtitle_widget: Option<gtk4::Widget>,
    leading_widget: Option<gtk4::Widget>,
    trailing_widget: Option<gtk4::Widget>,
    css_class: Option<String>,
}

impl ListRow {
    /// Create a new list row builder.
    pub fn builder() -> Self {
        Self {
            title: String::new(),
            subtitle: None,
            subtitle_widget: None,
            leading_widget: None,
            trailing_widget: None,
            css_class: None,
        }
    }

    /// Set the title text.
    pub fn title(mut self, title: &str) -> Self {
        self.title = title.to_string();
        self
    }

    /// Set the subtitle text.
    pub fn subtitle(mut self, subtitle: &str) -> Self {
        self.subtitle = Some(subtitle.to_string());
        self
    }

    /// Set optional subtitle (convenience for Option<&str>).
    pub fn subtitle_opt(mut self, subtitle: Option<&str>) -> Self {
        self.subtitle = subtitle.map(|s| s.to_string());
        self
    }

    /// Set a custom subtitle widget (takes precedence over subtitle text).
    pub fn subtitle_widget(mut self, widget: gtk4::Widget) -> Self {
        self.subtitle_widget = Some(widget);
        self
    }

    /// Set an optional leading widget (e.g., icon).
    pub fn leading_widget(mut self, widget: gtk4::Widget) -> Self {
        self.leading_widget = Some(widget);
        self
    }

    /// Set an optional trailing widget (e.g., menu button).
    pub fn trailing_widget(mut self, widget: gtk4::Widget) -> Self {
        self.trailing_widget = Some(widget);
        self
    }

    /// Add an extra CSS class to the row.
    pub fn css_class(mut self, css_class: &str) -> Self {
        self.css_class = Some(css_class.to_string());
        self
    }

    /// Build the list row.
    pub fn build(self) -> ListRowResult {
        use crate::styles::row;
        use gtk4::{Align, ListBoxRow};

        let list_row = ListBoxRow::new();
        list_row.add_css_class(row::QS);
        list_row.add_css_class(row::BASE);

        if let Some(css_class) = &self.css_class {
            list_row.add_css_class(css_class);
        }

        let hbox = GtkBox::new(Orientation::Horizontal, 6);
        hbox.add_css_class(row::QS_CONTENT);

        // Leading widget (e.g., icon)
        if let Some(leading) = self.leading_widget {
            hbox.append(&leading);
        }

        // Title and subtitle using CardLabel
        let mut label_builder = CardLabel::new(&self.title)
            .width_chars(22)
            .title_class(row::QS_TITLE)
            .subtitle_class(row::QS_SUBTITLE);

        // Custom subtitle widget takes precedence over text
        if let Some(widget) = self.subtitle_widget {
            label_builder = label_builder.subtitle_widget(widget);
        } else if let Some(ref text) = self.subtitle {
            label_builder = label_builder.subtitle(text);
        }

        let label_result = label_builder.build();
        hbox.append(&label_result.container);

        // Trailing widget (e.g., menu button)
        if let Some(trailing) = self.trailing_widget {
            trailing.set_halign(Align::End);
            hbox.append(&trailing);
        }

        list_row.set_child(Some(&hbox));
        list_row.set_activatable(true);
        list_row.set_focusable(true);

        ListRowResult {
            row: list_row,
            title: label_result.title,
            subtitle: label_result.subtitle,
        }
    }
}

/// Apply accent color styling to a slider's internal widgets.
///
/// This hooks into the slider's `realize` signal to directly style the
/// highlight and slider (knob) widgets, ensuring accent colors work
/// even with GTK themes that override them.
fn apply_accent_styling(scale: &Scale) {
    scale.connect_realize(|scale| {
        let accent = "var(--color-accent-slider, var(--color-accent-primary))";
        // Use high priority to override GTK theme styles
        let priority = u32::MAX / 2;

        // Traverse: scale -> trough -> {highlight, slider}
        let Some(trough) = scale.first_child() else {
            return;
        };
        let mut child = trough.first_child();
        while let Some(w) = child {
            let css = match w.css_name().as_str() {
                "highlight" => format!("highlight {{ background-image: image({accent}); }}"),
                "slider" => format!("slider {{ box-shadow: inset 0 0 0 2px {accent}; }}"),
                _ => {
                    child = w.next_sibling();
                    continue;
                }
            };
            let provider = CssProvider::new();
            provider.load_from_string(&css);
            #[allow(deprecated)]
            w.style_context().add_provider(&provider, priority);
            child = w.next_sibling();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_button_defaults() {
        let builder = IconButton::new("test-icon");
        assert_eq!(builder.icon_name, "test-icon");
        assert!(!builder.interactive);
    }

    #[test]
    fn test_icon_button_interactive() {
        let builder = IconButton::new("test-icon").interactive(true);
        assert!(builder.interactive);
    }

    #[test]
    fn test_accent_slider_defaults() {
        let builder = AccentSlider::new();
        assert_eq!(builder.min, 0.0);
        assert_eq!(builder.max, 100.0);
        assert_eq!(builder.step, 1.0);
    }

    #[test]
    fn test_accent_slider_range() {
        let builder = AccentSlider::new().range(1.0, 50.0).step(5.0);
        assert_eq!(builder.min, 1.0);
        assert_eq!(builder.max, 50.0);
        assert_eq!(builder.step, 5.0);
    }

    #[test]
    fn test_slider_row_defaults() {
        let builder = SliderRow::builder().icon("test-icon");
        assert_eq!(builder.icon_name, "test-icon");
        assert!(!builder.interactive_icon);
        assert!(!builder.with_expander);
        assert!(!builder.with_spacer);
    }

    #[test]
    fn test_slider_row_with_expander() {
        let builder = SliderRow::builder()
            .icon("audio-volume-high-symbolic")
            .interactive_icon(true)
            .range(0.0, 100.0)
            .with_expander(true);

        assert!(builder.interactive_icon);
        assert!(builder.with_expander);
        assert_eq!(builder.min, 0.0);
        assert_eq!(builder.max, 100.0);
    }

    #[test]
    fn test_slider_row_with_spacer() {
        let builder = SliderRow::builder()
            .icon("display-brightness-symbolic")
            .range(1.0, 100.0)
            .with_spacer(true);

        assert!(!builder.with_expander);
        assert!(builder.with_spacer);
    }
}
