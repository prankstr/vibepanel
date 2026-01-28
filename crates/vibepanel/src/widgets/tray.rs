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
    Box as GtkBox, Button, GestureClick, Image, Label, Orientation, Popover, Separator, Widget,
};
use tracing::debug;
use vibepanel_core::config::WidgetEntry;
use vibepanel_core::{parse_hex_color, theme::relative_luminance};

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
        // Get pixmap_icon_size from theme, falling back to default if ConfigManager isn't initialized yet
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
}

#[derive(Clone, Copy)]
struct ContrastParams {
    bg_luminance: f64,
    target_gray: u8,
}

struct WidgetState {
    config: TrayConfig,
    buttons: HashMap<String, Button>,
    pixmap_cache: HashMap<String, gdk::Texture>,
    menu: Option<MenuState>,
    /// Track the current button order to avoid unnecessary rebuilds.
    /// This prevents menu flickering when animated icons update rapidly.
    button_order: Vec<String>,
    contrast_params: ContrastParams,
}

/// System tray widget displaying StatusNotifierItem icons.
pub struct TrayWidget {
    base: BaseWidget,
    state: Rc<RefCell<WidgetState>>,
    theme_callback_id: Option<CallbackId>,
}

fn compute_contrast_params() -> ContrastParams {
    let styles = SurfaceStyleManager::global();
    let bg_color = styles.background_color();
    let text_color = styles.text_color();

    let bg_luminance = parse_hex_color(&bg_color)
        .map(|(r, g, b)| relative_luminance(r, g, b))
        .unwrap_or(0.1); // Default to dark if parsing fails

    // Derive target gray from text color (sRGB average, consistent with adjust_grayscale_icon)
    let target_gray = parse_hex_color(&text_color)
        .map(|(r, g, b)| ((r as u16 + g as u16 + b as u16) / 3) as u8)
        .unwrap_or(if bg_luminance > 0.5 { 0 } else { 255 });

    ContrastParams {
        bg_luminance,
        target_gray,
    }
}

impl TrayWidget {
    /// Create a new system tray widget.
    pub fn new(config: TrayConfig) -> Self {
        let base = BaseWidget::new(&[widget::TRAY]);

        let state = Rc::new(RefCell::new(WidgetState {
            config,
            buttons: HashMap::new(),
            pixmap_cache: HashMap::new(),
            menu: None,
            button_order: Vec::new(),
            contrast_params: compute_contrast_params(),
        }));

        let mut widget = Self {
            base,
            state,
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

        service.connect(move |_svc| {
            let state = state.clone();
            let content = content.clone();
            let root = root.clone();
            glib::idle_add_local_once(move || {
                sync_items(&state, &content, &root);
            });
        });

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
                // If menu is parented to this button, mark it for cleanup
                if let Some(ref menu) = st.menu
                    && menu.popover.parent().as_ref() == Some(button.upcast_ref::<Widget>())
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
        if let Some(popover) = menu_to_close
            && popover.parent().is_some()
        {
            popover.popdown();
            popover.unparent();
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
    let icon_size = state.borrow().config.pixmap_icon_size;
    image.set_pixel_size(icon_size);

    // Wrap in icon-root container for consistent sizing with other icons
    let icon_root = GtkBox::new(Orientation::Horizontal, 0);
    icon_root.add_css_class(icon::ROOT);
    icon_root.append(&image);

    button.set_child(Some(&icon_root));

    // Left-click handler
    let identifier_owned = identifier.to_string();
    let state_for_click = state.clone();
    button.connect_clicked(move |btn| {
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

    // Try pixmap first, then icon name, then fallback
    if let Some(pixmap) = pixmap
        && let Some(texture) = get_cached_texture(state, pixmap)
    {
        image.set_paintable(Some(&texture));
        return;
    }

    // Try loading from custom icon theme path if provided
    if let Some(name) = icon_name
        && !name.is_empty()
        && let Some(theme_path) = &snapshot.icon_theme_path
        && !theme_path.is_empty()
        && let Some(texture) = load_icon_from_theme_path(theme_path, name)
    {
        image.set_paintable(Some(&texture));
        return;
    }

    if let Some(name) = icon_name
        && !name.is_empty()
    {
        image.set_icon_name(Some(name));
        return;
    }

    image.set_icon_name(Some("application-default-icon"));
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
    let texture = texture_from_pixmap(pixmap, &contrast_params)?;

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

fn texture_from_pixmap(pixmap: &TrayPixmap, params: &ContrastParams) -> Option<gdk::Texture> {
    if pixmap.width <= 0 || pixmap.height <= 0 {
        return None;
    }

    let stride = pixmap.width * 4;

    let mut rgba_data = argb_to_rgba(&pixmap.buffer);

    // Adjust low-contrast grayscale icons toward theme text color
    if let Some(edge_analysis) = analyze_edge_pixels(&rgba_data, pixmap.width, pixmap.height) {
        let contrast = calculate_contrast_ratio(edge_analysis.avg_luminance, params.bg_luminance);

        if edge_analysis.is_grayscale {
            const MIN_CONTRAST: f64 = 3.0; // WCAG minimum for UI graphics

            if contrast < MIN_CONTRAST {
                debug!(
                    "Adjusting grayscale tray icon: contrast={:.2}:1 -> gray {}",
                    contrast, params.target_gray
                );
                adjust_grayscale_icon(&mut rgba_data, params.target_gray);
            }
        }
    }

    let gbytes = glib::Bytes::from_owned(rgba_data);
    let pixbuf = Pixbuf::from_bytes(
        &gbytes,
        Colorspace::Rgb,
        true, // has_alpha
        8,    // bits_per_sample
        pixmap.width,
        pixmap.height,
        stride,
    );

    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Convert ARGB pixel data to RGBA format.
///
/// StatusNotifierItem pixmaps use ARGB format (network byte order),
/// but GTK expects RGBA. This function converts by reordering bytes.
fn argb_to_rgba(data: &glib::Bytes) -> Vec<u8> {
    let raw = data.as_ref();
    let len = raw.len();

    let mut result = Vec::with_capacity(len);

    let mut idx = 0;
    while idx + 3 < len {
        let a = raw[idx];
        let r = raw[idx + 1];
        let g = raw[idx + 2];
        let b = raw[idx + 3];
        result.push(r);
        result.push(g);
        result.push(b);
        result.push(a);
        idx += 4;
    }

    result
}

/// Check if an RGB pixel is grayscale (within tolerance).
fn is_grayscale_pixel(r: u8, g: u8, b: u8) -> bool {
    r.abs_diff(g) <= GRAYSCALE_TOLERANCE
        && g.abs_diff(b) <= GRAYSCALE_TOLERANCE
        && r.abs_diff(b) <= GRAYSCALE_TOLERANCE
}

struct EdgeAnalysis {
    avg_luminance: f64,
    is_grayscale: bool,
}

/// Sample edge pixels to analyze icon appearance.
fn analyze_edge_pixels(rgba_data: &[u8], width: i32, height: i32) -> Option<EdgeAnalysis> {
    let w = width as usize;
    let h = height as usize;

    if w < 2 || h < 2 {
        return None;
    }

    // Sample positions: outer edges, inner ring at 25%, and center region
    // This handles icons with transparent padding around them
    let w25 = w / 4;
    let w75 = w * 3 / 4;
    let h25 = h / 4;
    let h75 = h * 3 / 4;

    let positions = [
        // Outer corners
        (0, 0),
        (w - 1, 0),
        (0, h - 1),
        (w - 1, h - 1),
        // Outer edge midpoints
        (w / 2, 0),
        (w / 2, h - 1),
        (0, h / 2),
        (w - 1, h / 2),
        // Inner ring at 25% from edges (for icons with transparent padding)
        (w25, h25),
        (w75, h25),
        (w25, h75),
        (w75, h75),
        // Inner edge midpoints
        (w / 2, h25),
        (w / 2, h75),
        (w25, h / 2),
        (w75, h / 2),
        // Center
        (w / 2, h / 2),
    ];

    let mut total_luminance = 0.0;
    let mut grayscale_count = 0;
    let mut visible_count = 0;

    const ALPHA_THRESHOLD: u8 = 128;

    for (x, y) in positions {
        let idx = (y * w + x) * 4;
        if idx + 3 >= rgba_data.len() {
            continue;
        }

        let r = rgba_data[idx];
        let g = rgba_data[idx + 1];
        let b = rgba_data[idx + 2];
        let a = rgba_data[idx + 3];

        if a < ALPHA_THRESHOLD {
            continue;
        }

        visible_count += 1;
        total_luminance += relative_luminance(r, g, b);

        if is_grayscale_pixel(r, g, b) {
            grayscale_count += 1;
        }
    }

    if visible_count == 0 {
        return None;
    }

    Some(EdgeAnalysis {
        avg_luminance: total_luminance / visible_count as f64,
        // Consider grayscale if majority of visible pixels are grayscale
        is_grayscale: grayscale_count > visible_count / 2,
    })
}

/// Scale grayscale pixels toward target gray, preserving antialiasing.
fn adjust_grayscale_icon(rgba_data: &mut [u8], base_gray: u8) {
    // Blend 15% toward mid-gray (128) to soften the contrast
    let target_gray = ((base_gray as u16 * 85 + 128 * 15) / 100) as u8;

    // Scale factor: maps white (255) to target_gray, preserving relative brightness
    // For antialiasing: darker pixels stay proportionally darker than solid pixels
    let scale = target_gray as f32 / 255.0;

    let mut idx = 0;
    while idx + 3 < rgba_data.len() {
        let r = rgba_data[idx];
        let g = rgba_data[idx + 1];
        let b = rgba_data[idx + 2];

        if is_grayscale_pixel(r, g, b) {
            let original_gray = ((r as u16 + g as u16 + b as u16) / 3) as f32;
            let new_gray = (original_gray * scale + 0.5) as u8;
            rgba_data[idx] = new_gray;
            rgba_data[idx + 1] = new_gray;
            rgba_data[idx + 2] = new_gray;
        }

        idx += 4;
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

/// Load an icon from a custom theme path provided by the application.
///
/// Tries common image extensions (.png, .svg, .xpm) to find the icon file.
fn load_icon_from_theme_path(theme_path: &str, icon_name: &str) -> Option<gdk::Texture> {
    use std::path::Path;

    let base_path = Path::new(theme_path);
    if !base_path.exists() {
        return None;
    }

    // Try common extensions
    for ext in &["png", "svg", "xpm"] {
        let icon_path = base_path.join(format!("{}.{}", icon_name, ext));
        if icon_path.exists()
            && let Ok(texture) = gdk::Texture::from_filename(&icon_path)
        {
            debug!("Loaded tray icon from custom path: {}", icon_path.display());
            return Some(texture);
        }
    }

    // Also try without extension (in case icon_name already has it)
    let direct_path = base_path.join(icon_name);
    if direct_path.exists()
        && let Ok(texture) = gdk::Texture::from_filename(&direct_path)
    {
        debug!(
            "Loaded tray icon from custom path: {}",
            direct_path.display()
        );
        return Some(texture);
    }

    None
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
            st.menu = None; // Clear before popdown to avoid borrow conflict in closed signal
            drop(st);
            if popover.parent().is_some() {
                popover.popdown();
                popover.unparent();
            }
            return;
        }
    }

    // Close existing menu if any - extract popover first to avoid borrow conflict
    let old_popover = {
        let mut st = state.borrow_mut();
        st.menu.take().map(|m| m.popover)
    };
    if let Some(popover) = old_popover
        && popover.parent().is_some()
    {
        popover.popdown();
        popover.unparent();
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

        // Create the popover now that we have entries
        let popover = Popover::new();
        popover.set_parent(&parent_clone);
        popover.set_can_focus(false);
        configure_popover(&popover);

        let container = GtkBox::new(Orientation::Vertical, 2);
        container.add_css_class(widget::TRAY_MENU);
        container.add_css_class(surface::POPOVER);
        container.add_css_class(surface::WIDGET_MENU_CONTENT);

        // Add tray-specific popover class for CSS variable-based styling
        container.add_css_class("tray-popover");

        // Apply surface styling - background color comes from CSS variables
        // which may be overridden by the tray-popover class
        SurfaceStyleManager::global().apply_surface_styles(&container, true);

        popover.set_child(Some(&container));

        // Set up menu state
        {
            let mut st = state_clone.borrow_mut();
            // Close any existing menu first
            if let Some(old_menu) = st.menu.take()
                && old_menu.popover.parent().is_some()
            {
                old_menu.popover.popdown();
                old_menu.popover.unparent();
            }
            st.menu = Some(MenuState {
                popover: popover.clone(),
                container: container.clone(),
                identifier: identifier_owned.clone(),
                stack: vec![entries],
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

        // Connect closed signal
        let state_for_close = state_clone.clone();
        let parent_for_close = parent_clone.clone();
        popover.connect_closed(move |p| {
            state_for_close.borrow_mut().menu = None;
            parent_for_close.remove_css_class(widget::TRAY_ITEM_MENU_OPEN);
            if p.parent().is_some() {
                p.unparent();
            }
        });

        // Now popup with content
        popover.popup();
    });
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

        let button = Button::new();
        button.set_sensitive(entry.enabled);
        button.set_focusable(false);
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

    // Close menu - extract popover first to avoid holding borrow during popdown()
    // (popdown triggers the closed signal which also borrows state)
    let popover = state.borrow().menu.as_ref().map(|m| m.popover.clone());
    if let Some(popover) = popover {
        popover.popdown();
    }
    // Note: menu is set to None by the popover's closed signal handler
}
