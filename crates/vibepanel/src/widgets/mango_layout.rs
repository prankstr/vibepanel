//! Mango window-layout indicator and chooser.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Grid, Label, Orientation};
use tracing::warn;
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::compositor::mango_layouts::{self, MANGO_LAYOUTS, MangoLayout};
use crate::services::compositor::{CompositorManager, WindowLayoutInfo};
use crate::services::icons::{IconsService, material_symbol_name};
use crate::services::tooltip::TooltipManager;
use crate::styles::{button, class, icon, surface, widget};
use crate::widgets::base::{BaseWidget, MenuHandle, vp_button};
use crate::widgets::{WidgetConfig, warn_unknown_options};

const DEFAULT_SHOW_ICON: bool = true;
const DEFAULT_SHOW_LABEL: bool = true;
#[derive(Debug, Clone)]
pub struct MangoLayoutConfig {
    pub show_icon: bool,
    pub show_label: bool,
    pub layouts: Vec<&'static MangoLayout>,
}

impl WidgetConfig for MangoLayoutConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "mango_layout",
            entry,
            &["show_icon", "show_label", "layouts"],
        );

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|value| value.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);
        let show_label = entry
            .options
            .get("show_label")
            .and_then(|value| value.as_bool())
            .unwrap_or(DEFAULT_SHOW_LABEL);
        let layouts = parse_layouts(entry.options.get("layouts"));

        Self {
            show_icon,
            show_label,
            layouts,
        }
    }
}

impl Default for MangoLayoutConfig {
    fn default() -> Self {
        Self {
            show_icon: DEFAULT_SHOW_ICON,
            show_label: DEFAULT_SHOW_LABEL,
            layouts: MANGO_LAYOUTS.iter().collect(),
        }
    }
}

fn parse_layouts(value: Option<&toml::Value>) -> Vec<&'static MangoLayout> {
    let Some(values) = value.and_then(toml::Value::as_array) else {
        return MANGO_LAYOUTS.iter().collect();
    };

    let mut seen = HashSet::new();
    let mut layouts = Vec::new();
    for value in values {
        let Some(name) = value.as_str() else {
            warn!("Ignoring non-string entry in mango_layout layouts option");
            continue;
        };
        let Some(layout) = mango_layouts::by_name(name) else {
            warn!("Ignoring unknown Mango layout '{}'", name);
            continue;
        };
        if seen.insert(layout.name) {
            layouts.push(layout);
        }
    }

    if layouts.is_empty() {
        warn!("mango_layout layouts option has no valid entries; using all layouts");
        MANGO_LAYOUTS.iter().collect()
    } else {
        layouts
    }
}

pub struct MangoLayoutWidget {
    base: BaseWidget,
    callback_id: CallbackId,
}

impl MangoLayoutWidget {
    pub fn new(config: MangoLayoutConfig, output_id: Option<String>) -> Self {
        let base = BaseWidget::new(&[widget::MANGO_LAYOUT]);
        base.set_tooltip("Mango layout: unknown");

        let icon = material_icon("tile", widget::MANGO_LAYOUT_ICON);
        icon.root.set_visible(config.show_icon);
        base.content().append(&icon.root);
        let label = base.add_label(
            Some("?"),
            &[widget::MANGO_LAYOUT_LABEL, class::VCENTER_CAPS],
        );
        label.set_visible(config.show_label);

        let current_info: Rc<RefCell<Option<WindowLayoutInfo>>> = Rc::new(RefCell::new(None));
        let controller: Rc<RefCell<Option<MangoLayoutPopoverController>>> =
            Rc::new(RefCell::new(None));
        let controller_for_builder = Rc::clone(&controller);
        let current_for_builder = Rc::clone(&current_info);
        let layouts = config.layouts.clone();
        let menu_handle = base.create_menu(|| GtkBox::new(Orientation::Vertical, 0).upcast());
        let menu_weak = Rc::downgrade(&menu_handle);
        menu_handle.set_builder(move || {
            let (content, popover_controller) =
                build_popover(&layouts, Rc::clone(&current_for_builder), menu_weak.clone());
            popover_controller.set_state(current_for_builder.borrow().as_ref());
            *controller_for_builder.borrow_mut() = Some(popover_controller);
            content.upcast()
        });
        menu_handle.set_reuse_content(true);

        let callback_id = {
            let container = base.widget().clone();
            let configured_output = output_id;
            let current_info = Rc::clone(&current_info);
            let controller = Rc::clone(&controller);
            CompositorManager::global().register_window_layout_callback(move |snapshot| {
                let output = configured_output
                    .as_deref()
                    .or(snapshot.active_output.as_deref());
                let info = output.and_then(|name| snapshot.per_output.get(name));
                *current_info.borrow_mut() = info.cloned();
                update_widget(&container, &icon, &label, info);
                if let Some(controller) = controller.borrow().as_ref() {
                    controller.set_state(info);
                }
            })
        };

        Self { base, callback_id }
    }

    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }

    pub(crate) fn edge_interaction(&self) -> Option<crate::widgets::EdgeInteraction> {
        self.base.edge_interaction()
    }
}

impl Drop for MangoLayoutWidget {
    fn drop(&mut self) {
        CompositorManager::global().unregister_window_layout_callback(self.callback_id);
    }
}

fn update_widget(
    container: &gtk4::Box,
    icon: &MaterialLayoutIcon,
    label: &Label,
    info: Option<&WindowLayoutInfo>,
) {
    if let Some(info) = info.filter(|info| is_overview(info)) {
        icon.set_layout("grid");
        label.set_label("OV");
        TooltipManager::global()
            .set_styled_tooltip(container, &format!("Mango overview ({})", info.output));
        return;
    }

    let (symbol, tooltip) = match info {
        Some(info) => (
            info.symbol.as_str(),
            format!("Mango layout: {} ({})", display_name(info), info.output),
        ),
        None => ("?", "Mango layout: unavailable".to_string()),
    };
    icon.set_layout(info.map(|info| info.layout_name.as_str()).unwrap_or("grid"));
    label.set_label(symbol);
    TooltipManager::global().set_styled_tooltip(container, &tooltip);
}

fn display_name(info: &WindowLayoutInfo) -> &str {
    mango_layouts::by_name(&info.layout_name)
        .map(|layout| layout.label)
        .unwrap_or(&info.layout_name)
}

struct MangoLayoutPopoverController {
    title: Label,
    buttons: Vec<(Button, &'static str)>,
}

impl MangoLayoutPopoverController {
    fn set_state(&self, info: Option<&WindowLayoutInfo>) {
        let overview = info.is_some_and(is_overview);
        let enabled = info.is_some() && !overview;
        self.title.set_label(&popover_title(info));
        self.set_current(
            info.filter(|_| !overview)
                .map(|layout| layout.layout_name.as_str()),
        );
        for (button, _) in &self.buttons {
            button.set_sensitive(enabled);
        }
    }

    fn set_current(&self, current: Option<&str>) {
        for (button, layout_name) in self.buttons.iter() {
            if current == Some(*layout_name) {
                button.remove_css_class(button::CARD);
                button.add_css_class(button::ACCENT);
                button.add_css_class(widget::MANGO_LAYOUT_SELECTED);
            } else {
                button.remove_css_class(button::ACCENT);
                button.remove_css_class(widget::MANGO_LAYOUT_SELECTED);
                button.add_css_class(button::CARD);
            }
        }
    }
}

fn build_popover(
    layouts: &[&'static MangoLayout],
    current_info: Rc<RefCell<Option<WindowLayoutInfo>>>,
    menu: Weak<MenuHandle>,
) -> (GtkBox, MangoLayoutPopoverController) {
    let root = GtkBox::new(Orientation::Vertical, 12);

    let title = Label::new(Some("Window Layout"));
    title.add_css_class(surface::POPOVER_TITLE);
    title.set_halign(Align::Start);
    root.append(&title);

    let grid = Grid::new();
    grid.add_css_class(widget::MANGO_LAYOUT_GRID);
    grid.set_column_homogeneous(true);
    grid.set_column_spacing(8);
    grid.set_row_spacing(8);

    let mut buttons = Vec::with_capacity(layouts.len());
    for (index, layout) in layouts.iter().copied().enumerate() {
        let button_widget = vp_button();
        button_widget.add_css_class(button::CARD);
        button_widget.add_css_class(widget::MANGO_LAYOUT_TILE);

        let content = GtkBox::new(Orientation::Vertical, 3);
        content.set_halign(Align::Fill);
        let icon = material_icon(layout.name, widget::MANGO_LAYOUT_TILE_ICON);
        content.append(&icon.root);
        let label = Label::new(Some(layout.label));
        label.add_css_class(widget::MANGO_LAYOUT_TILE_LABEL);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        content.append(&label);
        button_widget.set_child(Some(&content));

        let current = Rc::clone(&current_info);
        let menu = menu.clone();
        let layout_name = layout.name;
        button_widget.connect_clicked(move |_| {
            if let Some(info) = current.borrow().as_ref() {
                CompositorManager::global().set_window_layout(&info.output, layout_name);
                if let Some(menu) = menu.upgrade() {
                    menu.hide();
                }
            }
        });

        grid.attach(&button_widget, (index % 3) as i32, (index / 3) as i32, 1, 1);
        buttons.push((button_widget, layout.name));
    }
    root.append(&grid);

    (root, MangoLayoutPopoverController { title, buttons })
}

fn popover_title(info: Option<&WindowLayoutInfo>) -> String {
    let Some(info) = info else {
        return "Window Layout".to_string();
    };

    let tags = info
        .active_tags
        .iter()
        .copied()
        .filter(|tag| *tag > 0)
        .collect::<Vec<_>>();
    match tags.as_slice() {
        [tag] => format!("Layout for tag {tag} on {}", info.output),
        [] if is_overview(info) => format!("Overview on {}", info.output),
        [] => format!("Layout on {}", info.output),
        _ => format!("Layout for current view on {}", info.output),
    }
}

fn is_overview(info: &WindowLayoutInfo) -> bool {
    info.active_tags.contains(&0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconTransform {
    None,
    MirrorHorizontal,
    RotatePositive90,
    RotateNegative90,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutIcon {
    name: &'static str,
    transform: IconTransform,
}

struct MaterialLayoutIcon {
    root: GtkBox,
    label: Label,
}

impl MaterialLayoutIcon {
    fn set_layout(&self, layout_name: &str) {
        let icon = layout_icon(layout_name)
            .unwrap_or_else(|| layout_icon("grid").expect("grid Material Symbol mapping"));
        self.label.set_label(material_symbol_name(icon.name));
        apply_icon_transform(&self.root, icon.transform);
    }
}

fn layout_icon(layout_name: &str) -> Option<LayoutIcon> {
    let (name, transform) = match layout_name {
        "tile" => ("mango-layout-tile", IconTransform::None),
        "scroller" => ("mango-layout-scroller", IconTransform::None),
        "grid" => ("mango-layout-grid", IconTransform::None),
        "monocle" => ("mango-layout-monocle", IconTransform::None),
        "deck" => ("mango-layout-deck", IconTransform::None),
        "center_tile" => ("mango-layout-center-tile", IconTransform::None),
        "right_tile" => ("mango-layout-right-tile", IconTransform::MirrorHorizontal),
        "vertical_scroller" => (
            "mango-layout-vertical-scroller",
            IconTransform::RotatePositive90,
        ),
        "vertical_tile" => (
            "mango-layout-vertical-tile",
            IconTransform::RotatePositive90,
        ),
        "vertical_grid" => (
            "mango-layout-vertical-grid",
            IconTransform::RotatePositive90,
        ),
        "vertical_deck" => (
            "mango-layout-vertical-deck",
            IconTransform::RotateNegative90,
        ),
        "dwindle" => ("mango-layout-dwindle", IconTransform::None),
        "fair" => ("mango-layout-fair", IconTransform::RotateNegative90),
        "vertical_fair" => ("mango-layout-vertical-fair", IconTransform::None),
        _ => return None,
    };
    Some(LayoutIcon { name, transform })
}

fn material_icon(layout_name: &str, css_class: &str) -> MaterialLayoutIcon {
    let label = IconsService::global().create_material_symbol("mango-layout-grid", &[]);
    label.set_halign(Align::Center);
    let root = GtkBox::new(Orientation::Horizontal, 0);
    root.add_css_class(icon::ROOT);
    root.add_css_class(css_class);
    root.set_halign(Align::Center);
    root.set_hexpand(true);
    root.append(&label);
    let icon = MaterialLayoutIcon { root, label };
    icon.set_layout(layout_name);
    icon
}

fn apply_icon_transform(root: &GtkBox, transform: IconTransform) {
    root.remove_css_class(widget::MANGO_LAYOUT_MIRROR_HORIZONTAL);
    root.remove_css_class(widget::MANGO_LAYOUT_ROTATE_POSITIVE_90);
    root.remove_css_class(widget::MANGO_LAYOUT_ROTATE_NEGATIVE_90);
    match transform {
        IconTransform::None => {}
        IconTransform::MirrorHorizontal => {
            root.add_css_class(widget::MANGO_LAYOUT_MIRROR_HORIZONTAL);
        }
        IconTransform::RotatePositive90 => {
            root.add_css_class(widget::MANGO_LAYOUT_ROTATE_POSITIVE_90);
        }
        IconTransform::RotateNegative90 => {
            root.add_css_class(widget::MANGO_LAYOUT_ROTATE_NEGATIVE_90);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::icons::has_material_mapping;

    fn entry_with_layouts(values: Vec<toml::Value>) -> WidgetEntry {
        let mut options = std::collections::HashMap::new();
        options.insert("layouts".to_string(), toml::Value::Array(values));
        WidgetEntry {
            name: "mango_layout".to_string(),
            options,
        }
    }

    #[test]
    fn config_defaults_to_all_layouts() {
        let config = MangoLayoutConfig::default();
        assert!(config.show_icon);
        assert!(config.show_label);
        assert_eq!(config.layouts.len(), MANGO_LAYOUTS.len());
    }

    #[test]
    fn configured_layouts_preserve_order_and_deduplicate() {
        let entry = entry_with_layouts(vec![
            toml::Value::String("grid".into()),
            toml::Value::String("tile".into()),
            toml::Value::String("grid".into()),
            toml::Value::String("unknown".into()),
        ]);
        let config = MangoLayoutConfig::from_entry(&entry);
        assert_eq!(
            config
                .layouts
                .iter()
                .map(|layout| layout.name)
                .collect::<Vec<_>>(),
            vec!["grid", "tile"]
        );
    }

    #[test]
    fn every_layout_has_a_material_icon() {
        for layout in MANGO_LAYOUTS {
            let icon = layout_icon(layout.name)
                .unwrap_or_else(|| panic!("missing icon for {}", layout.name));
            assert!(
                has_material_mapping(icon.name),
                "missing Material mapping for {}",
                layout.name
            );
        }
    }

    #[test]
    fn directional_layouts_have_expected_transforms() {
        assert_eq!(
            layout_icon("right_tile").unwrap().transform,
            IconTransform::MirrorHorizontal
        );
        assert_eq!(
            layout_icon("fair").unwrap().transform,
            IconTransform::RotateNegative90
        );
        assert_eq!(
            layout_icon("vertical_tile").unwrap().transform,
            IconTransform::RotatePositive90
        );
        assert_eq!(
            layout_icon("vertical_scroller").unwrap().transform,
            IconTransform::RotatePositive90
        );
        assert_eq!(
            layout_icon("vertical_grid").unwrap().transform,
            IconTransform::RotatePositive90
        );
        assert_eq!(
            layout_icon("vertical_deck").unwrap().transform,
            IconTransform::RotateNegative90
        );
        assert_eq!(
            layout_icon("vertical_fair").unwrap().transform,
            IconTransform::None
        );
    }

    #[test]
    fn title_describes_single_tag_and_output() {
        let info = WindowLayoutInfo {
            output: "eDP-1".to_string(),
            active_tags: vec![1],
            layout_name: "tile".to_string(),
            symbol: "T".to_string(),
        };

        assert_eq!(popover_title(Some(&info)), "Layout for tag 1 on eDP-1");
    }

    #[test]
    fn title_describes_multi_tag_view() {
        let info = WindowLayoutInfo {
            output: "DP-1".to_string(),
            active_tags: vec![2, 3],
            layout_name: "grid".to_string(),
            symbol: "G".to_string(),
        };

        assert_eq!(
            popover_title(Some(&info)),
            "Layout for current view on DP-1"
        );
    }

    #[test]
    fn title_describes_overview() {
        let info = WindowLayoutInfo {
            output: "DP-1".to_string(),
            active_tags: vec![0],
            layout_name: "grid".to_string(),
            symbol: "G".to_string(),
        };

        assert_eq!(popover_title(Some(&info)), "Overview on DP-1");
        assert!(is_overview(&info));
    }
}
