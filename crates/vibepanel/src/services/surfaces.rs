//! Surface styling helpers for vibepanel.
//!
//! This module owns runtime surface helpers that cannot live in static CSS,
//! such as shadow margins, animated outlines, and optional Pango font styling.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::Label;
use gtk4::pango::{AttrFontDesc, AttrList, FontDescription};
use gtk4::prelude::*;
use tracing::debug;
use vibepanel_core::SurfaceStyles;

use crate::services::config_manager::ConfigManager;
use crate::styles::{icon, surface};
use crate::widgets::scale_box::ScaleBox;

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
        font_family: "monospace".to_string(),
        font_size: 14,
        shadows_enabled: true,
    }
}

/// Process-wide surface styling manager.
///
/// Provides runtime styling helpers for shadow margins, GSK outline coordination,
/// Pango font workarounds, and tray contrast adjustments.
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

    /// Initialize the global SurfaceStyleManager with styles and config options.
    ///
    /// Should be called during application startup after loading config:
    /// ```ignore
    /// let palette = ThemePalette::from_config(&config, None, None);
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

    /// Get the current background color.
    pub fn background_color(&self) -> String {
        self.styles.borrow().background_color.clone()
    }

    /// Get the theme's foreground/text color (e.g., "#1a1a1a" for light mode).
    pub fn text_color(&self) -> String {
        self.styles.borrow().text_color.clone()
    }

    /// Whether CSS box-shadows are enabled on surfaces.
    pub fn shadows_enabled(&self) -> bool {
        self.styles.borrow().shadows_enabled
    }

    /// Return `base_margin` when shadows are enabled, otherwise `0`.
    pub fn shadow_margin(&self, base_margin: i32) -> i32 {
        if self.shadows_enabled() {
            base_margin
        } else {
            0
        }
    }

    /// Apply the temporary ScaleBox outline used while floating surfaces animate.
    ///
    /// The real CSS border is suppressed only when there is a GSK outline to
    /// draw, so disabled outlines keep their normal resting CSS appearance.
    pub fn apply_animated_surface_outline(
        &self,
        shell: &ScaleBox,
        widget_name: &str,
        active: bool,
    ) {
        let config = ConfigManager::global();
        let width = config.surface_outline_width();
        let color = if active && width > 0.0 {
            debug_assert!(
                shell.child_has_css_class(surface::POPOVER),
                "animated surface outline suppression expects a popover surface child"
            );
            config.surface_outline_rgba_for_widget(widget_name, shell)
        } else {
            gtk4::gdk::RGBA::TRANSPARENT
        };

        shell.set_animated_outline(
            active,
            config.surface_border_radius() as f32,
            width,
            color,
            surface::SUPPRESS_CSS_OUTLINE,
        );
    }

    /// Apply shadow-aware margins to a popover/overlay container.
    ///
    /// The bar-adjacent side gets 0 margin (tight against bar), the opposite
    /// side and both horizontal sides get the shadow margin.
    pub fn apply_shadow_margins(&self, widget: &impl gtk4::prelude::WidgetExt, base_margin: i32) {
        let m = self.shadow_margin(base_margin);
        let is_bottom = crate::services::config_manager::ConfigManager::global().bar_is_bottom();
        if is_bottom {
            widget.set_margin_top(m);
            widget.set_margin_bottom(0);
        } else {
            widget.set_margin_top(0);
            widget.set_margin_bottom(m);
        }
        widget.set_margin_start(m);
        widget.set_margin_end(m);
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
}
