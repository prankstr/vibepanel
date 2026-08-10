//! System tray widget backed by the TrayService.
//!
//! Displays StatusNotifierItem icons in the bar, with context menu support.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, GestureClick, Image, Label, Orientation, Popover, PositionType,
    Separator, Widget,
};
use tracing::debug;
use vibepanel_core::config::BarPosition;
use vibepanel_core::config::WidgetEntry;
use vibepanel_core::{parse_hex_color, theme::relative_luminance};

use crate::services::background_effect::BackgroundEffectManager;
use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::tooltip::TooltipManager;
use crate::services::tray::{TrayItem, TrayMenuEntry, TrayPixmap, TrayService};
use crate::styles::{button as btn, color, icon, surface, widget};
use crate::widgets::WidgetConfig;
use crate::widgets::base::{BaseWidget, configure_popover};
use crate::widgets::warn_unknown_options;

const DEFAULT_MAX_ICONS: usize = 12;
const DEFAULT_PIXMAP_ICON_SIZE: i32 = 18;

const GRAYSCALE_TOLERANCE: u8 = 15;
const GRAYSCALE_DOMINANCE_PCT: usize = 90;

fn configure_tray_popover(popover: &Popover) {
    configure_popover(popover);

    let offset = ConfigManager::global().popover_offset() as i32;
    match ConfigManager::global().bar_position() {
        BarPosition::Top => {
            popover.set_position(PositionType::Bottom);
            popover.set_offset(0, offset);
        }
        BarPosition::Bottom => {
            popover.set_position(PositionType::Top);
            popover.set_offset(0, -offset);
        }
        BarPosition::Left => {
            popover.set_position(PositionType::Right);
            popover.set_offset(offset, 0);
        }
        BarPosition::Right => {
            popover.set_position(PositionType::Left);
            popover.set_offset(-offset, 0);
        }
    }
}

/// Configuration for the system tray widget.
#[derive(Debug, Clone)]
pub struct TrayConfig {
    /// Maximum number of tray icons to display.
    pub max_icons: usize,
    /// Icon size for pixmap icons (in pixels).
    pub pixmap_icon_size: i32,
}

impl Default for TrayConfig {
    fn default() -> Self {
        // TODO: Remove catch_unwind — move pixmap_icon_size theme read to
        // TrayWidget::new() (same pattern as TaskbarConfig/TaskbarLayout).
        let pixmap_icon_size = std::panic::catch_unwind(|| {
            ConfigManager::global().theme_sizes().pixmap_icon_size as i32
        })
        .unwrap_or(DEFAULT_PIXMAP_ICON_SIZE);

        Self {
            max_icons: DEFAULT_MAX_ICONS,
            pixmap_icon_size,
        }
    }
}

impl WidgetConfig for TrayConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("tray", entry, &["max_icons", "pixmap_icon_size"]);

        let defaults = Self::default();

        let max_icons = entry
            .options
            .get("max_icons")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize)
            .unwrap_or(defaults.max_icons);

        let pixmap_icon_size = entry
            .options
            .get("pixmap_icon_size")
            .and_then(|v| v.as_integer())
            .map(|v| v as i32)
            .unwrap_or(defaults.pixmap_icon_size);

        Self {
            max_icons,
            pixmap_icon_size,
        }
    }
}

struct MenuState {
    popover: Popover,
    container: GtkBox,
    identifier: String,
    stack: Vec<Vec<TrayMenuEntry>>,
    parent: Widget,
}

fn close_menu_popover(popover: &Popover) {
    if popover.parent().is_some() {
        popover.popdown();
        popover.unparent();
    }
}

impl Drop for MenuState {
    fn drop(&mut self) {
        // Safety net: remove the blur protocol object if this MenuState is
        // dropped without going through the connect_closed handler (e.g.
        // bar teardown on config reload, monitor disconnect, or shutdown).
        if let Some(blur) = BackgroundEffectManager::global() {
            blur.remove_blur_region(&self.popover);
        }
    }
}

#[derive(Clone, Copy)]
struct ContrastParams {
    bg_luminance: f64,
    fg: [u8; 3],
}

struct WidgetState {
    config: TrayConfig,
    buttons: HashMap<String, Button>,
    pixmap_cache: HashMap<String, gdk::Texture>,
    /// Cache for file-backed icons (theme path or absolute path) after contrast adjustment.
    /// Keyed by `"<path>:<mtime_nanos>"` to detect in-place file replacements.
    file_icon_cache: HashMap<String, gdk::Texture>,
    menu: Option<MenuState>,
    /// Track the current button order to avoid unnecessary rebuilds.
    /// This prevents menu flickering when animated icons update rapidly.
    button_order: Vec<String>,
    contrast_params: Option<ContrastParams>,
}

/// System tray widget displaying StatusNotifierItem icons.
pub struct TrayWidget {
    base: BaseWidget,
    state: Rc<RefCell<WidgetState>>,
    tray_callback_id: Option<CallbackId>,
    theme_callback_id: Option<CallbackId>,
}

fn compute_contrast_params() -> Option<ContrastParams> {
    let styles = SurfaceStyleManager::global();
    // GTK mode uses unresolved CSS colors; skip raster tinting rather than guess polarity.
    let (br, bg, bb) = parse_hex_color(&styles.background_color())?;
    let (fr, fg, fb) = parse_hex_color(&styles.text_color())?;

    Some(ContrastParams {
        bg_luminance: relative_luminance(br, bg, bb),
        fg: [fr, fg, fb],
    })
}

impl TrayWidget {
    /// Create a new system tray widget.
    pub fn new(config: TrayConfig) -> Self {
        let base = BaseWidget::new(&[widget::TRAY]);

        let state = Rc::new(RefCell::new(WidgetState {
            config,
            buttons: HashMap::new(),
            pixmap_cache: HashMap::new(),
            file_icon_cache: HashMap::new(),
            menu: None,
            button_order: Vec::new(),
            contrast_params: compute_contrast_params(),
        }));

        let mut widget = Self {
            base,
            state,
            tray_callback_id: None,
            theme_callback_id: None,
        };
        widget.bind_service();
        widget
    }

    /// Get the root GTK widget.
    pub fn widget(&self) -> &GtkBox {
        self.base.widget()
    }

    fn bind_service(&mut self) {
        let service = TrayService::global();
        let state = self.state.clone();
        let content = self.base.content().clone();
        let root = self.base.widget().clone();

        self.tray_callback_id = Some(service.connect(move |_svc| {
            let state = state.clone();
            let content = content.clone();
            let root = root.clone();
            glib::idle_add_local_once(move || {
                sync_items(&state, &content, &root);
            });
        }));

        // Subscribe to theme changes to invalidate pixmap cache
        {
            let state = self.state.clone();
            let content = self.base.content().clone();
            let root = self.base.widget().clone();
            let callback_id = ConfigManager::global().on_theme_change(move || {
                {
                    let mut st = state.borrow_mut();
                    st.contrast_params = compute_contrast_params();
                    st.pixmap_cache.clear();
                    st.file_icon_cache.clear();
                }
                let state = state.clone();
                let content = content.clone();
                let root = root.clone();
                glib::idle_add_local_once(move || {
                    sync_items(&state, &content, &root);
                });
            });
            self.theme_callback_id = Some(callback_id);
        }

        // Initial sync if service is already ready
        if service.is_ready() {
            let state = self.state.clone();
            let content = self.base.content().clone();
            let root = self.base.widget().clone();
            glib::idle_add_local_once(move || {
                sync_items(&state, &content, &root);
            });
        }
    }
}

impl Drop for TrayWidget {
    fn drop(&mut self) {
        if let Some(id) = self.tray_callback_id {
            TrayService::global().disconnect(id);
        }
        if let Some(id) = self.theme_callback_id {
            ConfigManager::global().disconnect_theme_callback(id);
        }
    }
}

fn sync_items(state: &Rc<RefCell<WidgetState>>, container: &GtkBox, root: &GtkBox) {
    let service = TrayService::global();
    // items() now returns a sorted Vec<(identifier, snapshot)>
    let items = service.items();

    let max_icons = state.borrow().config.max_icons;

    // Build desired list (already sorted by service)
    let desired: Vec<_> = items.iter().take(max_icons).collect();
    let desired_ids: std::collections::HashSet<_> =
        desired.iter().map(|(id, _)| id.as_str()).collect();

    // Remove buttons not in desired set
    {
        let mut st = state.borrow_mut();
        let to_remove: Vec<String> = st
            .buttons
            .keys()
            .filter(|id| !desired_ids.contains(id.as_str()))
            .cloned()
            .collect();

        // Collect buttons to remove and check if menu needs cleanup
        let mut buttons_to_remove = Vec::new();
        let mut menu_to_close: Option<Popover> = None;

        for identifier in to_remove {
            if let Some(button) = st.buttons.remove(&identifier) {
                // If menu is attached to this button, mark it for cleanup
                if let Some(ref menu) = st.menu
                    && menu.parent == button.clone().upcast::<Widget>()
                {
                    menu_to_close = Some(menu.popover.clone());
                }
                buttons_to_remove.push(button);
            }
        }

        // Clear menu state before popdown to avoid borrow conflict in closed signal
        if menu_to_close.is_some() {
            st.menu = None;
        }

        drop(st); // Release borrow before GTK operations

        // Now perform GTK operations (popdown triggers signals that may borrow state)
        if let Some(popover) = menu_to_close {
            close_menu_popover(&popover);
        }

        for button in buttons_to_remove {
            container.remove(&button);
        }
    }

    // Ensure buttons exist and update content
    for (identifier, snapshot) in &desired {
        let button_exists = state.borrow().buttons.contains_key(identifier.as_str());
        if !button_exists {
            let button = create_button(state, identifier);
            state
                .borrow_mut()
                .buttons
                .insert(identifier.clone(), button);
        }

        let button = state.borrow().buttons.get(identifier.as_str()).cloned();
        if let Some(button) = button {
            update_button(state, &button, snapshot);
        }
    }

    // Rebuild icon order
    let order: Vec<_> = desired.iter().map(|(id, _)| id.clone()).collect();
    rebuild_icon_order(state, container, &order);

    // Show/hide widget based on whether we have tray items
    let has_items = !state.borrow().buttons.is_empty();
    root.set_visible(has_items);
}

fn create_button(state: &Rc<RefCell<WidgetState>>, identifier: &str) -> Button {
    let button = Button::new();
    button.set_has_frame(false);
    button.set_focusable(false);
    button.set_focus_on_click(false);
    button.add_css_class(widget::TRAY_ITEM);
    button.add_css_class(btn::COMPACT); // Remove default button padding

    let image = Image::new();
    image.add_css_class(color::PRIMARY);
    let icon_size = state.borrow().config.pixmap_icon_size;
    image.set_pixel_size(icon_size);

    // Wrap in icon-root container for consistent sizing with other icons
    let icon_root = GtkBox::new(Orientation::Horizontal, 0);
    icon_root.add_css_class(icon::ROOT);
    if ConfigManager::global().bar_position().is_vertical() {
        button.set_halign(Align::Center);
        icon_root.set_halign(Align::Center);
        image.set_halign(Align::Center);
    }
    icon_root.append(&image);

    button.set_child(Some(&icon_root));

    // Left-click handler
    let identifier_owned = identifier.to_string();
    let state_for_click = state.clone();
    button.connect_clicked(move |btn| {
        TooltipManager::global().cancel_and_hide();
        on_button_clicked(&state_for_click, btn, &identifier_owned);
    });

    // Right-click handler
    let secondary = GestureClick::new();
    secondary.set_button(3); // GDK_BUTTON_SECONDARY
    let identifier_for_secondary = identifier.to_string();
    let state_for_secondary = state.clone();
    secondary.connect_released(move |gesture, _n_press, _x, _y| {
        if let Some(widget) = gesture.widget() {
            toggle_menu(&state_for_secondary, &identifier_for_secondary, &widget);
        }
    });
    button.add_controller(secondary);

    button
}

fn update_button(state: &Rc<RefCell<WidgetState>>, button: &Button, snapshot: &TrayItem) {
    let child = match button.child() {
        Some(c) => c,
        None => return,
    };

    // Navigate through icon-root container to find the Image
    let image = if let Some(icon_root) = child.downcast_ref::<GtkBox>() {
        icon_root
            .first_child()
            .and_then(|c| c.downcast::<Image>().ok())
    } else {
        // Fallback: direct Image child (legacy case)
        child.downcast::<Image>().ok()
    };

    let Some(image) = image else {
        return;
    };

    // Set tooltip
    let tooltip = snapshot
        .tooltip
        .clone()
        .or_else(|| {
            if !snapshot.title.is_empty() {
                Some(snapshot.title.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| snapshot.identifier.clone());

    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(button, &tooltip);

    // Determine which icon/pixmap to use
    let needs_attention = snapshot.status.to_lowercase() == "needsattention";
    let pixmap = if needs_attention {
        snapshot.attention_pixmap.as_ref()
    } else {
        snapshot.pixmap.as_ref()
    };
    let icon_name = if needs_attention {
        snapshot.attention_icon_name.as_ref()
    } else {
        snapshot.icon_name.as_ref()
    };

    // Prefer names so symbolic icons follow the active theme. Fall back to
    // pixmaps when the advertised name cannot be resolved on the host.
    if let Some(name) = icon_name
        && !name.is_empty()
    {
        if let Some(theme_path) = &snapshot.icon_theme_path
            && !theme_path.is_empty()
            && let Some(texture) = load_icon_from_theme_path(state, theme_path, name)
        {
            image.set_paintable(Some(&texture));
            return;
        }

        // Some apps set IconName to an absolute file path rather than a theme
        // name. Load it through the contrast pipeline, falling back to theme lookup.
        if name.starts_with('/')
            && let Some(texture) =
                get_cached_file_texture(state, std::path::Path::new(name.as_str()))
        {
            image.set_paintable(Some(&texture));
            return;
        }

        if gdk::Display::default()
            .is_some_and(|display| gtk4::IconTheme::for_display(&display).has_icon(name))
        {
            image.set_icon_name(Some(name));
            return;
        }
    }

    if let Some(pixmap) = pixmap
        && let Some(texture) = get_cached_texture(state, pixmap)
    {
        image.set_paintable(Some(&texture));
        return;
    }

    image.set_icon_name(Some("application-x-executable"));
}

fn rebuild_icon_order(state: &Rc<RefCell<WidgetState>>, container: &GtkBox, order: &[String]) {
    // Check if the order has actually changed to avoid unnecessary rebuilds.
    // This is important for animated icons (e.g., spinners) that update rapidly -
    // rebuilding the container disrupts popover menus parented to buttons.
    {
        let st = state.borrow();
        if st.button_order == order {
            return;
        }
    }

    // Remove all children
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    // Re-add in order and update tracked order
    let mut st = state.borrow_mut();
    for identifier in order {
        if let Some(button) = st.buttons.get(identifier) {
            container.append(button);
        }
    }
    st.button_order = order.to_vec();
}

fn get_cached_texture(
    state: &Rc<RefCell<WidgetState>>,
    pixmap: &TrayPixmap,
) -> Option<gdk::Texture> {
    let cache_key = format!("{}x{}:{}", pixmap.width, pixmap.height, pixmap.hash_key);

    if let Some(texture) = state.borrow().pixmap_cache.get(&cache_key).cloned() {
        return Some(texture);
    }

    let contrast_params = state.borrow().contrast_params;
    let texture = texture_from_pixmap(pixmap, contrast_params.as_ref())?;

    // Bounded size to prevent unbounded growth from animated icons
    {
        let mut st = state.borrow_mut();
        if st.pixmap_cache.len() >= 50 {
            st.pixmap_cache.clear();
        }
        st.pixmap_cache.insert(cache_key, texture.clone());
    }

    Some(texture)
}

fn texture_from_pixmap(
    pixmap: &TrayPixmap,
    params: Option<&ContrastParams>,
) -> Option<gdk::Texture> {
    if pixmap.width <= 0 || pixmap.height <= 0 {
        return None;
    }

    let mut rgba_data = argb_to_rgba(&pixmap.buffer, pixmap.width, pixmap.height)?;

    apply_contrast_adjustment(&mut rgba_data, params);

    Some(texture_from_rgba_data(
        rgba_data,
        pixmap.width,
        pixmap.height,
    ))
}

/// Adjust low-contrast grayscale icons toward the theme text color.
///
/// Shared by both the pixmap path and the file-backed icon path.
fn apply_contrast_adjustment(rgba_data: &mut [u8], params: Option<&ContrastParams>) {
    let Some(params) = params else {
        return;
    };
    let Some(analysis) = analyze_visible_pixels(rgba_data) else {
        return;
    };
    if !analysis.is_grayscale {
        return;
    }

    let anchor_luminance = relative_luminance(
        analysis.anchor_gray,
        analysis.anchor_gray,
        analysis.anchor_gray,
    );
    let contrast = calculate_contrast_ratio(anchor_luminance, params.bg_luminance);
    const MIN_CONTRAST: f64 = 3.0; // WCAG minimum for UI graphics
    if contrast >= MIN_CONTRAST {
        return;
    }

    // Soften the target 15% toward mid-gray.
    let target = params.fg.map(|c| ((c as u16 * 85 + 128 * 15) / 100) as u8);
    let target_luminance = relative_luminance(target[0], target[1], target[2]);
    if calculate_contrast_ratio(target_luminance, params.bg_luminance) <= contrast {
        debug!(
            "Skipping grayscale adjustment: anchor {} would not improve contrast",
            analysis.anchor_gray
        );
        return;
    }

    debug!(
        "Adjusting grayscale tray icon: contrast={:.2}:1 -> fg {:?}",
        contrast, target
    );
    adjust_grayscale_icon(rgba_data, target, analysis.anchor_gray);
}

/// Convert ARGB pixel data to RGBA format.
///
/// StatusNotifierItem pixmaps use ARGB format (network byte order),
/// but GTK expects RGBA. This function converts by reordering bytes.
fn argb_to_rgba(data: &glib::Bytes, width: i32, height: i32) -> Option<Vec<u8>> {
    let byte_count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    let mut result = data.as_ref().get(..byte_count)?.to_vec();

    for pixel in result.chunks_exact_mut(4) {
        pixel.rotate_left(1);
    }

    Some(result)
}

/// Check if an RGB pixel is grayscale (within tolerance).
fn is_grayscale_pixel(r: u8, g: u8, b: u8) -> bool {
    r.abs_diff(g) <= GRAYSCALE_TOLERANCE
        && g.abs_diff(b) <= GRAYSCALE_TOLERANCE
        && r.abs_diff(b) <= GRAYSCALE_TOLERANCE
}

struct IconAnalysis {
    is_grayscale: bool,
    anchor_gray: u8,
}

/// Analyze every visible pixel so sparse outlines are not missed.
fn analyze_visible_pixels(pixels: &[u8]) -> Option<IconAnalysis> {
    let mut grayscale_count = 0_usize;
    let mut visible_count = 0_usize;
    let mut grayscale_histogram = [0_usize; 256];

    // Derive the anchor from materially visible pixels. Low-alpha antialias noise
    // and rare bright outliers should not collapse the correction.
    const ALPHA_THRESHOLD: u8 = 128;

    for pixel in pixels.chunks_exact(4) {
        let [r, g, b, a] = [pixel[0], pixel[1], pixel[2], pixel[3]];
        if a < ALPHA_THRESHOLD {
            continue;
        }

        visible_count += 1;
        if is_grayscale_pixel(r, g, b) {
            grayscale_count += 1;
            let gray = ((r as u16 + g as u16 + b as u16) / 3) as usize;
            grayscale_histogram[gray] += 1;
        }
    }

    if visible_count == 0 {
        return None;
    }

    let rank = (grayscale_count * 9).div_ceil(10);
    let mut seen = 0;
    let anchor_gray = grayscale_histogram
        .iter()
        .position(|count| {
            seen += count;
            seen >= rank
        })
        .unwrap_or_default() as u8;

    Some(IconAnalysis {
        // Avoid recoloring mixed artwork with a large grayscale background.
        is_grayscale: grayscale_count * 100 > visible_count * GRAYSCALE_DOMINANCE_PCT,
        anchor_gray,
    })
}

/// Tint grayscale pixels while preserving relative brightness and alpha.
fn adjust_grayscale_icon(rgba_data: &mut [u8], target: [u8; 3], source_anchor: u8) {
    // All-black icons have no brightness structure to scale.
    let scale = (source_anchor != 0).then(|| target.map(|c| c as f32 / source_anchor as f32));

    for pixel in rgba_data.chunks_exact_mut(4) {
        let [r, g, b, a] = [pixel[0], pixel[1], pixel[2], pixel[3]];

        // Include low-alpha antialiased edges excluded from peak analysis.
        if a == 0 || !is_grayscale_pixel(r, g, b) {
            continue;
        }

        match scale {
            Some(scale) => {
                let original_gray =
                    (((r as u16 + g as u16 + b as u16) / 3) as u8).min(source_anchor) as f32;
                for channel in 0..3 {
                    pixel[channel] = (original_gray * scale[channel] + 0.5) as u8;
                }
            }
            None => pixel[..3].copy_from_slice(&target),
        }
    }
}

fn calculate_contrast_ratio(lum1: f64, lum2: f64) -> f64 {
    let (lighter, darker) = if lum1 > lum2 {
        (lum1, lum2)
    } else {
        (lum2, lum1)
    };

    (lighter + 0.05) / (darker + 0.05)
}

/// Normalize a `Pixbuf` to a packed RGBA `Vec<u8>`, handling rowstride padding
/// and RGB (no alpha) pixbufs.
fn normalize_pixbuf_to_rgba(pixbuf: &Pixbuf) -> Vec<u8> {
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    let rowstride = pixbuf.rowstride() as usize;
    let n_channels = pixbuf.n_channels() as usize;
    let has_alpha = pixbuf.has_alpha();

    let raw = pixbuf.read_pixel_bytes();
    let raw = raw.as_ref();

    let mut result = Vec::with_capacity(width * height * 4);

    for row in 0..height {
        let row_start = row * rowstride;
        for col in 0..width {
            let px = row_start + col * n_channels;
            let r = raw[px];
            let g = raw[px + 1];
            let b = raw[px + 2];
            let a = if has_alpha { raw[px + 3] } else { 255 };
            result.push(r);
            result.push(g);
            result.push(b);
            result.push(a);
        }
    }

    result
}

/// Build a `gdk::Texture` from packed RGBA bytes and dimensions.
fn texture_from_rgba_data(rgba_data: Vec<u8>, width: i32, height: i32) -> gdk::Texture {
    let stride = width * 4;
    let gbytes = glib::Bytes::from_owned(rgba_data);
    let pixbuf = Pixbuf::from_bytes(&gbytes, Colorspace::Rgb, true, 8, width, height, stride);
    gdk::Texture::for_pixbuf(&pixbuf)
}

/// Decode a file-backed icon, run contrast adjustment, and return a texture.
///
/// `icon_size` is used to rasterize SVGs at the correct display resolution rather than
/// their intrinsic/default size, which avoids blurry or oversized icons at tray scale.
fn texture_from_file(
    path: &std::path::Path,
    params: Option<&ContrastParams>,
    icon_size: i32,
) -> Option<gdk::Texture> {
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    let pixbuf = if is_svg {
        Pixbuf::from_file_at_scale(path, icon_size, icon_size, true).ok()?
    } else {
        Pixbuf::from_file(path).ok()?
    };

    let width = pixbuf.width();
    let height = pixbuf.height();

    let mut rgba_data = normalize_pixbuf_to_rgba(&pixbuf);
    apply_contrast_adjustment(&mut rgba_data, params);

    Some(texture_from_rgba_data(rgba_data, width, height))
}

/// Load a contrast-adjusted texture for a file-backed icon, using a cache keyed by
/// `"<path>:<mtime_nanos>"`. The mtime component ensures that in-place file replacements
/// (same path, new content) produce a cache miss and re-decode correctly.
/// The cache is cleared entirely on theme/contrast changes.
fn get_cached_file_texture(
    state: &Rc<RefCell<WidgetState>>,
    path: &std::path::Path,
) -> Option<gdk::Texture> {
    let mtime_nanos = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        })
        .unwrap_or(0);
    let cache_key = format!("{}:{}", path.display(), mtime_nanos);

    if let Some(texture) = state.borrow().file_icon_cache.get(&cache_key).cloned() {
        return Some(texture);
    }

    let contrast_params = state.borrow().contrast_params;
    let icon_size = state.borrow().config.pixmap_icon_size;
    let texture = texture_from_file(path, contrast_params.as_ref(), icon_size)?;

    {
        let mut st = state.borrow_mut();
        if st.file_icon_cache.len() >= 50 {
            st.file_icon_cache.clear();
        }
        st.file_icon_cache.insert(cache_key, texture.clone());
    }

    Some(texture)
}

/// Resolve an icon file path from a custom theme path provided by the application.
///
/// Tries common image extensions (.png, .svg, .xpm) to find the icon file.
fn resolve_icon_from_theme_path(theme_path: &str, icon_name: &str) -> Option<std::path::PathBuf> {
    use std::path::Path;

    let base_path = Path::new(theme_path);
    if !base_path.exists() {
        return None;
    }

    // Try common extensions
    for ext in &["png", "svg", "xpm"] {
        let icon_path = base_path.join(format!("{}.{}", icon_name, ext));
        if icon_path.exists() {
            return Some(icon_path);
        }
    }

    // Also try without extension (in case icon_name already has it)
    let direct_path = base_path.join(icon_name);
    if direct_path.exists() {
        return Some(direct_path);
    }

    None
}

/// Load and contrast-adjust an icon from a custom theme path provided by the application.
fn load_icon_from_theme_path(
    state: &Rc<RefCell<WidgetState>>,
    theme_path: &str,
    icon_name: &str,
) -> Option<gdk::Texture> {
    let path = resolve_icon_from_theme_path(theme_path, icon_name)?;
    let texture = get_cached_file_texture(state, &path)?;
    debug!("Loaded tray icon from custom path: {}", path.display());
    Some(texture)
}

fn on_button_clicked(state: &Rc<RefCell<WidgetState>>, button: &Button, identifier: &str) {
    let service = TrayService::global();
    let items = service.items();

    // Check if this item should show menu on left-click instead of activate
    if let Some((_, snapshot)) = items.iter().find(|(id, _)| id == identifier)
        && snapshot.item_is_menu
    {
        toggle_menu(state, identifier, button.upcast_ref::<Widget>());
        return;
    }

    service.activate(identifier, -1, -1);
}

fn toggle_menu(state: &Rc<RefCell<WidgetState>>, identifier: &str, parent: &Widget) {
    // If menu is already open for this identifier, close it
    {
        let mut st = state.borrow_mut();
        if let Some(ref menu) = st.menu
            && menu.identifier == identifier
        {
            let popover = menu.popover.clone();
            let parent = menu.parent.clone();
            st.menu = None; // Clear before popdown to avoid borrow conflict in closed signal
            drop(st);
            parent.remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
            close_menu_popover(&popover);
            return;
        }
    }

    // Close existing menu if any - extract surface first to avoid borrow conflict
    let old_menu = {
        let mut st = state.borrow_mut();
        st.menu.take()
    };
    if let Some(old_menu) = old_menu {
        old_menu
            .parent
            .remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
        close_menu_popover(&old_menu.popover);
    }

    // Fetch menu entries asynchronously, then create and show the popover
    let service = TrayService::global();
    let state_clone = state.clone();
    let identifier_owned = identifier.to_string();
    let parent_clone = parent.clone();

    service.get_menu(identifier, move |entries| {
        if entries.is_empty() {
            debug!("No menu entries for {}", identifier_owned);
            return;
        }

        // Check if parent is still valid (button might have been removed)
        if !parent_clone.is_realized() {
            debug!("Parent widget no longer realized for {}", identifier_owned);
            return;
        }

        // Check if a different menu was opened while we were fetching
        {
            let st = state_clone.borrow();
            if let Some(ref menu) = st.menu
                && menu.identifier != identifier_owned
            {
                // A different menu is now open, don't interrupt
                return;
            }
        }

        let container = GtkBox::new(Orientation::Vertical, 2);
        container.add_css_class(widget::TRAY_MENU);
        container.add_css_class(surface::POPOVER);
        container.add_css_class(surface::SURFACE_POPOVER);
        container.add_css_class(surface::WIDGET_MENU_CONTENT);

        // Add tray-specific popover class for CSS variable-based styling
        container.add_css_class("tray-popover");

        let popover = create_menu_popover(&state_clone, &parent_clone, &container);

        // Set up menu state
        {
            let mut st = state_clone.borrow_mut();
            // Close any existing menu first
            if let Some(old_menu) = st.menu.take() {
                old_menu
                    .parent
                    .remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
                close_menu_popover(&old_menu.popover);
            }
            st.menu = Some(MenuState {
                popover: popover.clone(),
                container: container.clone(),
                identifier: identifier_owned.clone(),
                stack: vec![entries],
                parent: parent_clone.clone(),
            });
        }

        // Render menu content
        render_menu_level(&state_clone);

        // Apply Pango font attributes to all labels if enabled in config.
        // This is the central hook for system tray menus - widgets create standard
        // GTK labels, and we apply Pango attributes here after the tree is built.
        SurfaceStyleManager::global().apply_pango_attrs_all(&container);

        // Add class to keep icon enlarged while menu is open
        parent_clone.add_css_class(widget::TRAY_ITEM_MENU_OPEN);

        popover.popup();
    });
}

fn create_menu_popover(
    state: &Rc<RefCell<WidgetState>>,
    parent: &Widget,
    container: &GtkBox,
) -> Popover {
    let popover = Popover::new();
    popover.set_parent(parent);
    popover.set_can_focus(false);
    configure_tray_popover(&popover);
    popover.set_child(Some(container));

    let state_for_close = state.clone();
    let parent_for_close = parent.clone();
    popover.connect_closed(move |p| {
        if let Some(blur) = BackgroundEffectManager::global() {
            blur.remove_blur_region(p);
        }
        state_for_close.borrow_mut().menu = None;
        parent_for_close.remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
        if p.parent().is_some() {
            p.unparent();
        }
    });

    let container_for_blur = container.clone();
    popover.connect_map(move |p| {
        if ConfigManager::global().blur_enabled()
            && let Some(blur) = BackgroundEffectManager::global()
        {
            blur.apply_blur_surface(p, &container_for_blur, || {
                ConfigManager::global().surface_border_radius() as i32
            });
        }
    });

    popover
}

fn render_menu_level(state: &Rc<RefCell<WidgetState>>) {
    // Extract what we need from the borrow
    let (container, stack_len, current_entries, identifier) = {
        let st = state.borrow();
        let menu = match st.menu.as_ref() {
            Some(m) => m,
            None => return,
        };
        (
            menu.container.clone(),
            menu.stack.len(),
            menu.stack.last().cloned().unwrap_or_default(),
            menu.identifier.clone(),
        )
    };

    // Clear existing children
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    // Add back button if we're in a submenu
    if stack_len > 1 {
        let back_btn = Button::with_label("← Back");
        back_btn.add_css_class(widget::TRAY_MENU_BACK);
        back_btn.add_css_class(btn::GHOST);
        let state_for_back = state.clone();
        back_btn.connect_clicked(move |_| {
            on_menu_back(&state_for_back);
        });
        container.append(&back_btn);
    }

    if current_entries.is_empty() {
        let empty = Label::new(Some("No menu entries"));
        empty.add_css_class(color::TEXT);
        empty.add_css_class(color::MUTED);
        container.append(&empty);
        return;
    }

    for entry in current_entries {
        if entry.is_separator {
            let separator = Separator::new(Orientation::Horizontal);
            container.append(&separator);
            continue;
        }

        let button = crate::widgets::base::vp_button();
        button.set_sensitive(entry.enabled);
        button.set_focus_on_click(false);
        button.add_css_class(widget::TRAY_MENU_BUTTON);

        // Build label text
        let mut text = entry.label.clone();
        if let Some(ref toggle_type) = entry.toggle_type
            && entry.toggle_state == Some(1)
        {
            let prefix = if toggle_type == "radio" { "●" } else { "✔" };
            text = if text.is_empty() {
                prefix.to_string()
            } else {
                format!("{} {}", prefix, text)
            };
        }
        if entry.has_children() {
            text = if text.is_empty() {
                "▶".to_string()
            } else {
                format!("{} ▶", text)
            };
            button.add_css_class(widget::TRAY_MENU_SUBMENU);
        }

        let label = Label::new(Some(&text));
        label.set_xalign(0.0);
        label.add_css_class(color::TEXT);
        label.add_css_class(color::PRIMARY);
        button.set_child(Some(&label));

        // Connect click handler
        let state_for_entry = state.clone();
        let entry_clone = entry.clone();
        let identifier_clone = identifier.clone();
        button.connect_clicked(move |_| {
            on_menu_entry_clicked(&state_for_entry, &entry_clone, &identifier_clone);
        });

        container.append(&button);
    }
}

fn on_menu_back(state: &Rc<RefCell<WidgetState>>) {
    {
        let mut st = state.borrow_mut();
        if let Some(ref mut menu) = st.menu {
            if menu.stack.len() <= 1 {
                return;
            }
            menu.stack.pop();
        }
    }
    render_menu_level(state);
}

fn on_menu_entry_clicked(
    state: &Rc<RefCell<WidgetState>>,
    entry: &TrayMenuEntry,
    identifier: &str,
) {
    if entry.has_children() {
        // Push submenu
        {
            let mut st = state.borrow_mut();
            if let Some(ref mut menu) = st.menu {
                menu.stack.push(entry.children.clone());
            }
        }
        render_menu_level(state);
        return;
    }

    // Send event to service
    let service = TrayService::global();
    service.send_menu_event(identifier, entry.menu_id, "clicked");

    // Close menu - extract popover first to avoid holding borrow during close.
    let menu = state.borrow_mut().menu.take();
    if let Some(menu) = menu {
        menu.parent.remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
        close_menu_popover(&menu.popover);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_icon(width: usize, height: usize, x: usize, y: usize, gray: u8) -> Vec<u8> {
        let mut pixels = vec![0; width * height * 4];
        let offset = (y * width + x) * 4;
        pixels[offset..offset + 4].copy_from_slice(&[gray, gray, gray, 255]);
        pixels
    }

    #[test]
    fn sparse_grayscale_outline_is_analyzed() {
        let pixels = rgba_icon(8, 8, 1, 1, 48);
        let analysis = analyze_visible_pixels(&pixels).unwrap();

        assert!(analysis.is_grayscale);
        assert_eq!(analysis.anchor_gray, 48);
    }

    #[test]
    fn low_contrast_dark_outline_is_brightened_on_dark_background() {
        let mut pixels = rgba_icon(8, 8, 1, 1, 48);
        let offset = (8 + 1) * 4;

        apply_contrast_adjustment(
            &mut pixels,
            Some(&ContrastParams {
                bg_luminance: 0.0,
                fg: [255; 3],
            }),
        );

        assert_eq!(&pixels[offset..offset + 4], &[235, 235, 235, 255]);
    }

    #[test]
    fn high_contrast_outline_is_unchanged() {
        let mut pixels = rgba_icon(8, 8, 1, 1, 255);
        let original = pixels.clone();

        apply_contrast_adjustment(
            &mut pixels,
            Some(&ContrastParams {
                bg_luminance: 0.0,
                fg: [255; 3],
            }),
        );

        assert_eq!(pixels, original);
    }

    #[test]
    fn mixed_color_artwork_is_unchanged() {
        let mut pixels = [[48, 48, 48, 255]; 10].concat();
        pixels[..4].copy_from_slice(&[48, 0, 0, 255]);
        let original = pixels.clone();

        apply_contrast_adjustment(
            &mut pixels,
            Some(&ContrastParams {
                bg_luminance: 0.0,
                fg: [255; 3],
            }),
        );

        assert_eq!(pixels, original);
    }

    #[test]
    fn tinted_foreground_hue_is_preserved() {
        let mut pixels = rgba_icon(8, 8, 1, 1, 48);
        let offset = (8 + 1) * 4;
        let edge_offset = (8 + 2) * 4;
        pixels[edge_offset..edge_offset + 4].copy_from_slice(&[60, 60, 60, 100]);

        apply_contrast_adjustment(
            &mut pixels,
            Some(&ContrastParams {
                bg_luminance: 0.0,
                fg: [180, 200, 255],
            }),
        );

        // Foreground softened 15% toward mid-gray.
        assert_eq!(&pixels[offset..offset + 4], &[172, 189, 235, 255]);
        assert_eq!(&pixels[edge_offset..edge_offset + 4], &[172, 189, 235, 100]);
    }

    #[test]
    fn percentile_anchor_drives_contrast_gate() {
        let mut pixels = [[32, 32, 32, 255]; 20].concat();
        pixels[..8].copy_from_slice(&[255, 255, 255, 255, 255, 255, 255, 255]);

        apply_contrast_adjustment(
            &mut pixels,
            Some(&ContrastParams {
                bg_luminance: 0.0,
                fg: [255; 3],
            }),
        );

        assert!(
            pixels
                .chunks_exact(4)
                .all(|pixel| pixel == [235, 235, 235, 255])
        );
    }

    #[test]
    fn argb_conversion_validates_extent() {
        let short = glib::Bytes::from_owned(vec![255; 7]);
        assert!(argb_to_rgba(&short, 2, 1).is_none());

        let trailing = glib::Bytes::from_owned(vec![40, 10, 20, 30, 255]);
        assert_eq!(argb_to_rgba(&trailing, 1, 1), Some(vec![10, 20, 30, 40]));
    }
}
