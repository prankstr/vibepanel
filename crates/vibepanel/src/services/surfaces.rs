//! Surface styling helpers for vibepanel.
//!
//! This module owns `SurfaceStyles` derived from the theme and provides
//! helpers to apply consistent styling to popovers, menus, and other
//! overlay containers. It is intentionally separate from `TooltipManager`
//! so that tooltip concerns and general surface styling remain decoupled.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::Label;
use gtk4::pango::{AttrFontDesc, AttrList, FontDescription};
use gtk4::prelude::*;
use tracing::debug;
use vibepanel_core::SurfaceStyles;

use crate::styles::{icon, surface};

// GTK 4.10 deprecated widget-scoped style contexts but didn't provide a replacement.
// We need widget-scoped CSS to style individual surfaces without affecting the entire
// display. The display-scoped alternative (style_context_add_provider_for_display)
// would require unique CSS class names for every surface instance, which is impractical.
// This import is used for StyleContextExt::add_provider().
#[allow(deprecated)]
use gtk4::prelude::StyleContextExt;

// Thread-local singleton storage for SurfaceStyleManager
thread_local! {
    static SURFACE_STYLES_INSTANCE: RefCell<Option<Rc<SurfaceStyleManager>>> = const { RefCell::new(None) };
}

/// Default surface styles, used when init_global is not called.
/// Provides a reasonable dark-mode appearance as fallback.
fn default_surface_styles() -> SurfaceStyles {
    SurfaceStyles {
        background_color: "#111217".to_string(),
        text_color: "#ffffff".to_string(),
        font_family: "\"Cascadia Mono NF\", monospace".to_string(),
        font_size: 14,
        border_radius: 8,
        border_color: "rgba(255, 255, 255, 0.10)".to_string(),
        opacity: 1.0,
        shadow: "0 1px 2px rgba(0, 0, 0, 0.20), 0 1px 3px rgba(0, 0, 0, 0.24)".to_string(),
        is_dark_mode: true,
    }
}

/// Process-wide surface styling manager.
///
/// Provides `apply_surface_styles` for popovers, menus, and other containers
/// that should share the same visual language as widgets.
pub struct SurfaceStyleManager {
    styles: RefCell<SurfaceStyles>,
    /// Whether to use Pango attributes for font rendering instead of CSS.
    /// When true, applies Pango font attributes to labels as a workaround
    /// for GTK CSS font rendering issues in layer-shell surfaces.
    pango_font_rendering: Cell<bool>,
}

impl SurfaceStyleManager {
    /// Create a new manager with the given styles.
    fn new(styles: SurfaceStyles) -> Rc<Self> {
        Rc::new(Self {
            styles: RefCell::new(styles),
            pango_font_rendering: Cell::new(false),
        })
    }

    /// Initialize the global SurfaceStyleManager with styles from ThemePalette.
    ///
    /// Should be called during application startup after loading config:
    /// ```ignore
    /// let palette = ThemePalette::from_config(&config);
    /// SurfaceStyleManager::init_global(palette.surface_styles());
    /// ```
    #[allow(dead_code)]
    pub fn init_global(styles: SurfaceStyles) {
        Self::init_global_with_config(styles, false);
    }

    /// Initialize the global SurfaceStyleManager with styles and config options.
    ///
    /// Should be called during application startup after loading config:
    /// ```ignore
    /// let palette = ThemePalette::from_config(&config);
    /// SurfaceStyleManager::init_global_with_config(
    ///     palette.surface_styles(),
    ///     config.advanced.pango_font_rendering,
    /// );
    /// ```
    pub fn init_global_with_config(styles: SurfaceStyles, pango_font_rendering: bool) {
        SURFACE_STYLES_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_some() {
                debug!("SurfaceStyleManager already initialized, ignoring init_global call");
                return;
            }
            let manager = SurfaceStyleManager::new(styles);
            manager.pango_font_rendering.set(pango_font_rendering);
            *opt = Some(manager);
        });
    }

    /// Get the global SurfaceStyleManager singleton.
    ///
    /// If not initialized via `init_global`, uses default dark-mode styles.
    pub fn global() -> Rc<Self> {
        SURFACE_STYLES_INSTANCE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                debug!("SurfaceStyleManager not initialized, using defaults");
                *opt = Some(SurfaceStyleManager::new(default_surface_styles()));
            }
            opt.as_ref().unwrap().clone()
        })
    }

    /// Reconfigure the manager with new styles (for live config reload).
    ///
    /// This updates the internal styles. Note that surfaces already styled
    /// won't be automatically updated - they would need to call
    /// `apply_surface_styles` again. For most use cases, newly created
    /// surfaces will pick up the new styles automatically.
    pub fn reconfigure(&self, styles: SurfaceStyles, pango_font_rendering: bool) {
        debug!(
            "SurfaceStyleManager reconfiguring: bg={} -> {}, pango_font_rendering={}",
            self.styles.borrow().background_color,
            styles.background_color,
            pango_font_rendering,
        );
        *self.styles.borrow_mut() = styles;
        self.pango_font_rendering.set(pango_font_rendering);
    }

    /// Get the current font family.
    #[allow(dead_code)]
    pub fn font_family(&self) -> String {
        self.styles.borrow().font_family.clone()
    }

    /// Get the current font size for bar widgets.
    fn font_size(&self) -> u32 {
        self.styles.borrow().font_size
    }

    /// Get the CSS-computed font size for a label in pixels.
    ///
    /// Reads the font size from the label's Pango context, which reflects
    /// whatever CSS has resolved (including `em`, `%`, etc.). This allows
    /// preserving relative font sizes when applying Pango attributes.
    ///
    /// Returns `None` if the size couldn't be determined (e.g., styles not
    /// yet resolved, or no explicit size set).
    fn get_computed_font_size(&self, label: &Label) -> Option<u32> {
        let pango_context = label.pango_context();
        let font_desc = pango_context.font_description()?;
        let pango_size = font_desc.size();

        // size() returns 0 if size wasn't set explicitly
        if pango_size <= 0 {
            return None;
        }

        let size_px = if font_desc.is_size_absolute() {
            // Size is in device units (pixels * SCALE)
            pango_size as f64 / gtk4::pango::SCALE as f64
        } else {
            // Size is in points * SCALE, convert points to pixels (96 DPI)
            let size_pt = pango_size as f64 / gtk4::pango::SCALE as f64;
            size_pt * 96.0 / 72.0
        };

        Some((size_px.round() as u32).max(1))
    }

    /// Apply text styling with a specific font size.
    ///
    /// Use this for labels that need a different size than the standard bar font.
    ///
    /// Note: This is an internal method. External code should use `apply_pango_attrs()`
    /// which respects the `pango_font_rendering` config flag.
    fn style_label(&self, label: &Label, font_size_px: u32) {
        let styles = self.styles.borrow();
        let attrs = AttrList::new();

        // Use set_size() (DPI-aware, in points) instead of CSS which uses
        // set_absolute_size() internally. This avoids glyph clipping in
        // layer-shell surfaces at certain font sizes.
        //
        // Convert pixels to points: points = pixels * 72 / 96 (at standard DPI)
        // This gives us the same visual size as CSS `font-size: Npx`.
        let font_size_pt = (font_size_px as f64 * 72.0 / 96.0).round() as i32;
        let pango_size = font_size_pt * gtk4::pango::SCALE;

        let mut font_desc = FontDescription::new();
        font_desc.set_family(&styles.font_family);
        font_desc.set_size(pango_size);

        attrs.insert(AttrFontDesc::new(&font_desc));

        label.set_attributes(Some(&attrs));
    }

    /// Recursively apply Pango font styling to all Label widgets in a tree.
    ///
    /// This is useful for fixing font rendering in GTK widgets that have
    /// internal labels (like Calendar) where you can't easily replace the
    /// labels with custom ones.
    ///
    /// GTK CSS always uses `set_absolute_size()` for fonts, which can cause
    /// glyph clipping in layer-shell surfaces. This function applies Pango
    /// attributes with `set_size()` (DPI-aware points) to work around that.
    ///
    /// Each label's font size is read from its CSS-computed value, preserving
    /// relative sizes (em values, subtitles, etc.). Falls back to `base_font_size_px`
    /// if the computed size can't be determined.
    ///
    /// Note: This is an internal method. External code should use `apply_pango_attrs_all()`
    /// which respects the `pango_font_rendering` config flag.
    fn style_all_labels(&self, widget: &impl IsA<gtk4::Widget>, base_font_size_px: u32) {
        self.style_all_labels_recursive(widget.as_ref(), base_font_size_px);
    }

    fn style_all_labels_recursive(&self, widget: &gtk4::Widget, base_font_size_px: u32) {
        // If this widget is a Label, style it (unless it's a Material Symbol icon)
        if let Some(label) = widget.downcast_ref::<Label>() {
            // Skip Material Symbols icons - they use ligature-based font rendering
            // and applying Pango attributes breaks the icon→glyph mapping
            if !label.has_css_class(icon::MATERIAL_SYMBOL) {
                // Use CSS-computed size if available, otherwise fall back to base size.
                // This preserves relative sizing (em values, smaller subtitles, etc.)
                let font_size = self
                    .get_computed_font_size(label)
                    .unwrap_or(base_font_size_px);
                self.style_label(label, font_size);
            }
        }

        // Recurse into children
        let mut child = widget.first_child();
        while let Some(c) = child {
            self.style_all_labels_recursive(&c, base_font_size_px);
            child = c.next_sibling();
        }
    }

    /// Apply Pango font attributes to a single label if `pango_font_rendering` is enabled.
    ///
    /// This is a config-aware wrapper that reads the label's CSS-computed font size
    /// and applies it via Pango. If the config flag is disabled (default), this is a no-op.
    ///
    /// # Example
    /// ```ignore
    /// let label = Label::new(Some("Hello"));
    /// // ... CSS styling applied ...
    /// SurfaceStyleManager::global().apply_pango_attrs(&label);
    /// ```
    pub fn apply_pango_attrs(&self, label: &Label) {
        if self.pango_font_rendering.get() {
            // Use CSS-computed size if available, otherwise fall back to base size
            let font_size = self
                .get_computed_font_size(label)
                .unwrap_or_else(|| self.font_size());
            self.style_label(label, font_size);
        }
    }

    /// Apply Pango font attributes to all labels in a widget tree if `pango_font_rendering` is enabled.
    ///
    /// This is a config-aware wrapper around `style_all_labels()`. Call this
    /// after building a widget tree that uses CSS for fonts (e.g., Calendar,
    /// popovers with multiple labels). If the config flag is disabled (default),
    /// this is a no-op.
    ///
    /// # Example
    /// ```ignore
    /// let calendar = Calendar::new();
    /// // ... CSS styling applied ...
    /// SurfaceStyleManager::global().apply_pango_attrs_all(&calendar);
    /// ```
    pub fn apply_pango_attrs_all(&self, widget: &impl IsA<gtk4::Widget>) {
        if self.pango_font_rendering.get() {
            self.style_all_labels(widget, self.font_size());
        }
    }

    /// Apply tooltip-like surface styling to a widget.
    ///
    /// Use this for popovers, menus, or any container that should have the
    /// same visual language as widgets: dark background, rounded corners,
    /// readable text, subtle border.
    ///
    /// The styles are applied at base priority so CSS classes can override
    /// specific properties while GTK themes are still overridden.
    ///
    /// # Arguments
    /// * `widget` - The widget to style
    /// * `with_padding` - Whether to apply padding to the widget
    /// * `color_override` - Optional background color override (e.g., from parent widget)
    pub fn apply_surface_styles(
        &self,
        widget: &impl IsA<gtk4::Widget>,
        with_padding: bool,
        color_override: Option<&str>,
    ) {
        self.apply_surface_styles_inner(
            widget,
            with_padding,
            color_override,
            "var(--radius-surface)",
        );
    }

    /// Apply surface styles with a custom border radius.
    ///
    /// # Arguments
    /// * `widget` - The widget to style
    /// * `with_padding` - Whether to apply padding to the widget
    /// * `color_override` - Optional background color override (e.g., from parent widget)
    /// * `radius` - CSS value for border-radius (e.g., "var(--radius-widget)")
    pub fn apply_surface_styles_with_radius(
        &self,
        widget: &impl IsA<gtk4::Widget>,
        with_padding: bool,
        color_override: Option<&str>,
        radius: &str,
    ) {
        self.apply_surface_styles_inner(widget, with_padding, color_override, radius);
    }

    fn apply_surface_styles_inner(
        &self,
        widget: &impl IsA<gtk4::Widget>,
        with_padding: bool,
        color_override: Option<&str>,
        radius: &str,
    ) {
        let widget = widget.as_ref();

        let styles = self.styles.borrow();
        let padding_css = if with_padding {
            "padding: 16px;".to_string()
        } else {
            String::new()
        };

        // Use color override if provided, otherwise use CSS variable for consistency
        // with widget_opacity setting
        let bg = color_override.unwrap_or("var(--color-background-widget)");

        // Build CSS targeting the widget's CSS name
        // For Popover, we need to target both the popover and its contents
        // Use high-specificity selectors to override GTK themes
        let css_name = widget.css_name();
        let css = if css_name == "popover" {
            format!(
                r#"
popover.widget-menu,
popover.widget-menu.background {{
    background-color: {bg};
    background: {bg};
    background-image: none;
    border: none;
    border-radius: {radius};
    box-shadow: {shadow};
}}

popover.widget-menu > contents,
popover.widget-menu.background > contents {{
    background-color: transparent;
    background: transparent;
    background-image: none;
    border: none;
    border-radius: {radius};
    font-family: {font};
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
    {padding}
    margin: 0;
    box-shadow: none;
}}

popover.widget-menu > arrow,
popover.widget-menu.background > arrow {{
    background: transparent;
    background-color: transparent;
    border: none;
    box-shadow: none;
}}

popover.widget-menu *,
popover.widget-menu.background * {{
    font-family: inherit;
}}
"#,
                bg = bg,
                font = styles.font_family,
                padding = padding_css,
                shadow = styles.shadow,
                radius = radius,
            )
        } else {
            // Check if widget has the widget-menu-content class - if so, use
            // that as the selector for more specific targeting.
            let has_menu_content_class = widget.has_css_class(surface::WIDGET_MENU_CONTENT);

            let selector = if has_menu_content_class {
                format!(".{}", surface::WIDGET_MENU_CONTENT)
            } else {
                css_name.to_string()
            };

            format!(
                r#"
{selector} {{
    background-color: {bg};
    background-image: none;
    border-radius: {radius};
    font-family: {font};
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
    {padding}
    box-shadow: {shadow};
}}

{selector} * {{
    font-family: inherit;
}}
"#,
                selector = selector,
                bg = bg,
                font = styles.font_family,
                padding = padding_css,
                shadow = styles.shadow,
                radius = radius,
            )
        };

        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&css);

        // Apply styles to the widget's style context so they are scoped to
        // this surface hierarchy instead of the entire display. GTK will
        // propagate these styles to descendants automatically.
        //
        // NOTE: style_context() and add_provider() are deprecated in GTK 4.10+
        // but GTK provides no replacement for widget-scoped CSS. The alternative
        // (style_context_add_provider_for_display) applies CSS globally, which
        // would require unique class names per surface instance. We use the
        // deprecated API intentionally here as it's the only way to scope CSS
        // to a specific widget subtree.
        #[allow(deprecated)]
        widget
            .style_context()
            .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
    }
}
