//! The layer-shell surface that shows bar tooltips.

use gtk4::prelude::*;
use gtk4::{Label, Orientation, Window, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vibepanel_core::config::BarPosition;

use super::placement::{Along, Placement};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::tooltip;
use crate::widgets::layer_shell_popover::popover_exclusive_zone;

pub(super) struct TooltipWindow {
    window: Window,
    label: Label,
}

impl TooltipWindow {
    pub(super) fn new() -> Self {
        let window = Window::builder().decorated(false).resizable(false).build();
        window.add_css_class(tooltip::WINDOW);

        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-tooltip"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);

        let label = Label::new(None);
        label.add_css_class(tooltip::LABEL);
        window.set_child(Some(&label));
        SurfaceStyleManager::global().apply_pango_attrs(&label);

        Self { window, label }
    }

    pub(super) fn set_text(&self, text: &str) {
        self.label.set_text(text);
    }

    /// Natural size of the tooltip. The CSS padding sits on the label, so
    /// measuring the label covers the whole surface.
    pub(super) fn size(&self) -> (i32, i32) {
        let (_, width, _, _) = self.label.measure(Orientation::Horizontal, -1);
        let (_, height, _, _) = self.label.measure(Orientation::Vertical, -1);
        (width, height)
    }

    pub(super) fn show(&self, monitor: &gdk::Monitor, position: BarPosition, placement: Placement) {
        let (bar_edge, start_edge, end_edge) = match position {
            BarPosition::Top => (Edge::Top, Edge::Left, Edge::Right),
            BarPosition::Bottom => (Edge::Bottom, Edge::Left, Edge::Right),
            BarPosition::Left => (Edge::Left, Edge::Top, Edge::Bottom),
            BarPosition::Right => (Edge::Right, Edge::Top, Edge::Bottom),
        };
        let (along_edge, along_margin) = match placement.along {
            Along::Start(margin) => (start_edge, margin),
            Along::End(margin) => (end_edge, margin),
        };

        for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            self.window.set_anchor(edge, false);
            self.window.set_margin(edge, 0);
        }
        self.window.set_monitor(Some(monitor));
        self.window.set_exclusive_zone(popover_exclusive_zone());
        self.window.set_anchor(bar_edge, true);
        self.window.set_margin(bar_edge, placement.inward);
        self.window.set_anchor(along_edge, true);
        self.window.set_margin(along_edge, along_margin);
        self.window.present();
    }

    pub(super) fn hide(&self) {
        self.window.set_visible(false);
    }
}
