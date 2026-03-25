//! System resource popover - detailed CPU, memory, GPU, network, and load information.
//!
//! This popover is shared between the CPU, Memory, and GPU widgets, showing
//! comprehensive system resource information when any of those widgets is clicked.
//!
//! Layout:
//! ```text
//! ┌─────────────────────────────┐
//! │ ┌───────────┐ ┌───────────┐ │
//! │ │  CPU      │ │  Memory   │ │
//! │ └───────────┘ └───────────┘ │
//! ├─────────────────────────────┤
//! │ ┌───────────────────────────┤  (conditional: only if GPU detected)
//! │ │  GPU                      │
//! │ └───────────────────────────┤
//! ├─────────────────────────────┤
//! │ ┌───────────┐ ┌───────────┐ │
//! │ │  Load     │ │  Network  │ │
//! │ └───────────┘ └───────────┘ │
//! └─────────────────────────────┘
//! ```
//!
//! The CPU section has an expandable per-core breakdown that spans full width.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Label, Orientation, ProgressBar, Revealer, RevealerTransitionType, Widget,
};

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::gpu::{GpuService, GpuSnapshot};
use crate::services::icons::{IconHandle, IconsService};
use crate::services::system::{SystemService, SystemSnapshot, format_bytes_long, format_speed};
use crate::styles::{button, card, color, icon, surface, system_popover as sp};

/// A single pre-allocated per-core row with its updatable widgets.
#[derive(Clone)]
struct CoreRow {
    bar: ProgressBar,
    pct_label: Label,
}

/// Controller owning the system popover UI elements and update logic.
#[derive(Clone)]
pub struct SystemPopoverController {
    // CPU section
    cpu_usage_label: Label,
    cpu_temp_label: Label,
    cpu_progress: ProgressBar,
    cores_expander_label: Label,
    cores_expander_chevron: IconHandle,
    cores_revealer: Revealer,
    cpu_cores_box: GtkBox,
    cores_expanded: Rc<Cell<bool>>,
    core_rows: Rc<RefCell<Vec<CoreRow>>>,

    // Memory section
    memory_usage_label: Label,
    memory_detail_label: Label,
    memory_progress: ProgressBar,

    // Network section
    net_download_label: Label,
    net_upload_label: Label,

    // Load average section
    load_1_label: Label,
    load_5_label: Label,
    load_15_label: Label,

    // GPU section (conditional: only present when GPU is detected)
    gpu_card: GtkBox,
    gpu_metrics_label: Label,
    gpu_usage_label: Label,
    gpu_progress: ProgressBar,
    gpu_vram_value_label: Label,
    gpu_vram_progress: ProgressBar,
    gpu_vram_detail_label: Label,
}

impl SystemPopoverController {
    /// Update all labels and progress bars from the latest snapshot.
    pub fn update_from_snapshot(&self, snapshot: &SystemSnapshot) {
        // CPU
        self.cpu_usage_label
            .set_label(&format!("{:.1}%", snapshot.cpu_usage));
        self.cpu_temp_label.set_label(&match snapshot.cpu_temp {
            Some(temp) => format!("{:.0}°C", temp),
            None => String::new(),
        });
        self.cpu_progress
            .set_fraction(snapshot.cpu_usage as f64 / 100.0);

        // Update cores expander label
        let core_count = snapshot.cpu_per_core.len();
        self.cores_expander_label
            .set_label(&format!("{} cores", core_count));

        // Update per-core display
        self.update_core_bars(snapshot);

        // Memory
        self.memory_usage_label
            .set_label(&format!("{:.1}%", snapshot.memory_percent));
        self.memory_detail_label.set_label(&format!(
            "{} / {}",
            format_bytes_long(snapshot.memory_used),
            format_bytes_long(snapshot.memory_total)
        ));
        self.memory_progress
            .set_fraction(snapshot.memory_percent as f64 / 100.0);

        // Network
        self.net_download_label
            .set_label(&format_speed(snapshot.net_download_speed));
        self.net_upload_label
            .set_label(&format_speed(snapshot.net_upload_speed));

        // Load average
        let (one, five, fifteen) = snapshot.load_avg;
        self.load_1_label.set_label(&format!("{:.2}", one));
        self.load_5_label.set_label(&format!("{:.2}", five));
        self.load_15_label.set_label(&format!("{:.2}", fifteen));
    }

    /// Update the GPU card from the latest GPU snapshot.
    pub fn update_from_gpu_snapshot(&self, snapshot: &GpuSnapshot) {
        if !snapshot.available {
            self.gpu_card.set_visible(false);
            return;
        }
        self.gpu_card.set_visible(true);

        if let Some(usage) = snapshot.gpu_usage {
            self.gpu_usage_label.set_label(&format!("{:.1}%", usage));
            self.gpu_progress.set_fraction(usage as f64 / 100.0);
        } else {
            self.gpu_usage_label.set_label("--");
            self.gpu_progress.set_fraction(0.0);
        }

        // Clock + power + temperature on metrics row
        let mut metrics_parts = Vec::new();
        if let Some(mhz) = snapshot.clock_mhz {
            metrics_parts.push(format!("{} MHz", mhz));
        }
        if let Some(watts) = snapshot.power_watts {
            metrics_parts.push(format!("{:.1} W", watts));
        }
        if let Some(temp) = snapshot.temperature {
            metrics_parts.push(format!("{:.0}°C", temp));
        }
        let metrics_text = metrics_parts.join(" \u{00B7} ");
        self.gpu_metrics_label.set_label(&metrics_text);
        self.gpu_metrics_label.set_visible(!metrics_text.is_empty());

        // VRAM bar + detail
        let vram_pct = snapshot.vram_percent();
        if let Some(pct) = vram_pct {
            self.gpu_vram_value_label.set_label(&format!("{:.1}%", pct));
            self.gpu_vram_progress.set_fraction(pct as f64 / 100.0);
        } else {
            self.gpu_vram_value_label.set_label("--");
            self.gpu_vram_progress.set_fraction(0.0);
        }

        let vram_detail = match (snapshot.vram_used, snapshot.vram_total) {
            (Some(used), Some(total)) => {
                format!("{} / {}", format_bytes_long(used), format_bytes_long(total))
            }
            (Some(used), None) => format!("{} used", format_bytes_long(used)),
            _ => "--".to_string(),
        };
        self.gpu_vram_detail_label.set_label(&vram_detail);
    }

    /// Toggle the cores expander visibility.
    fn toggle_cores(&self) {
        let expanded = !self.cores_expanded.get();
        self.cores_expanded.set(expanded);
        self.cores_revealer.set_reveal_child(expanded);

        let chevron = if expanded {
            "pan-up-symbolic"
        } else {
            "pan-down-symbolic"
        };
        self.cores_expander_chevron.set_icon(chevron);
    }

    /// Update the per-core CPU bars.
    fn update_core_bars(&self, snapshot: &SystemSnapshot) {
        let mut core_rows = self.core_rows.borrow_mut();
        let core_count = snapshot.cpu_per_core.len();

        // If core count changed, rebuild rows
        if core_rows.len() != core_count {
            while let Some(child) = self.cpu_cores_box.first_child() {
                self.cpu_cores_box.remove(&child);
            }
            core_rows.clear();

            for i in 0..core_count {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                row.add_css_class(sp::CORE_ROW);

                let label = Label::new(Some(&format!("Core {}", i)));
                label.add_css_class(color::MUTED);
                label.set_width_chars(7);
                label.set_xalign(0.0);
                row.append(&label);

                let bar = ProgressBar::new();
                bar.add_css_class(sp::CORE_BAR);
                bar.set_hexpand(true);
                bar.set_valign(gtk4::Align::Center);
                row.append(&bar);

                let pct_label = Label::new(Some("--"));
                pct_label.add_css_class(color::MUTED);
                pct_label.set_width_chars(4);
                pct_label.set_xalign(1.0);
                row.append(&pct_label);

                self.cpu_cores_box.append(&row);
                core_rows.push(CoreRow { bar, pct_label });
            }
        }

        // Update values
        for (i, core_row) in core_rows.iter().enumerate() {
            if let Some(&usage) = snapshot.cpu_per_core.get(i) {
                core_row.bar.set_fraction(usage as f64 / 100.0);
                core_row.pct_label.set_label(&format!("{:.0}%", usage));
            }
        }
    }
}

/// Create a section title with icon and label.
fn section_title(icon_name: &str, text: &str, icons: &IconsService) -> GtkBox {
    let container = GtkBox::new(Orientation::Horizontal, 6);
    container.add_css_class(sp::SECTION_TITLE);
    container.set_halign(Align::Start);

    let icon_handle = icons.create_icon(icon_name, &[icon::TEXT, sp::SECTION_ICON]);
    container.append(&icon_handle.widget());

    let label = Label::new(Some(text));
    label.add_css_class(surface::POPOVER_TITLE);
    container.append(&label);

    container
}

/// Create a section title with icon, label, and a right-aligned value (for CPU temp).
fn section_title_with_value(icon_name: &str, text: &str, icons: &IconsService) -> (GtkBox, Label) {
    let container = GtkBox::new(Orientation::Horizontal, 6);
    container.add_css_class(sp::SECTION_TITLE);

    let icon_handle = icons.create_icon(icon_name, &[icon::TEXT, sp::SECTION_ICON]);
    container.append(&icon_handle.widget());

    let label = Label::new(Some(text));
    label.add_css_class(surface::POPOVER_TITLE);
    container.append(&label);

    let value = Label::new(Some(""));
    value.add_css_class(color::MUTED);
    value.set_hexpand(true);
    value.set_halign(Align::End);
    container.append(&value);

    (container, value)
}

/// Create a stat row with label and value.
fn stat_row(label_text: &str, value_width_chars: i32) -> (GtkBox, Label) {
    let row = GtkBox::new(Orientation::Horizontal, 8);

    let label = Label::new(Some(label_text));
    label.add_css_class(color::MUTED);
    label.set_halign(Align::Start);
    row.append(&label);

    let value = Label::new(Some("--"));
    value.set_halign(Align::End);
    value.set_hexpand(true);
    value.set_width_chars(value_width_chars);
    value.set_xalign(1.0);
    row.append(&value);

    (row, value)
}

/// Build a system resource popover content widget.
pub fn build_system_popover_with_controller() -> (Widget, SystemPopoverController) {
    let system_service = SystemService::global();
    let snapshot = system_service.snapshot();
    let icons = IconsService::global();

    let container = GtkBox::new(Orientation::Vertical, 0);
    container.add_css_class(sp::POPOVER);

    let top_row = GtkBox::new(Orientation::Horizontal, 8);
    top_row.set_homogeneous(true);

    let cpu_card = GtkBox::new(Orientation::Vertical, 0);
    cpu_card.add_css_class(card::BASE);
    cpu_card.add_css_class(sp::SECTION_CARD);

    let cpu_section = GtkBox::new(Orientation::Vertical, 8);

    let (cpu_title, cpu_temp_label) = section_title_with_value("cpu-symbolic", "CPU", &icons);
    cpu_section.append(&cpu_title);

    let (cpu_usage_row, cpu_usage_label) = stat_row("Usage", 6);
    cpu_section.append(&cpu_usage_row);

    let cpu_progress = ProgressBar::new();
    cpu_progress.add_css_class(sp::PROGRESS_BAR);
    cpu_section.append(&cpu_progress);

    // Cores expander
    let cores_expanded = Rc::new(Cell::new(false));
    let expander_row = GtkBox::new(Orientation::Horizontal, 0);

    let cores_expander_label = Label::new(Some("-- cores"));
    cores_expander_label.add_css_class(color::MUTED);
    cores_expander_label.set_halign(Align::Start);
    cores_expander_label.set_hexpand(true);
    expander_row.append(&cores_expander_label);

    let cores_expander_chevron =
        icons.create_icon("pan-down-symbolic", &[icon::TEXT, color::MUTED]);
    cores_expander_chevron.widget().set_margin_top(2);
    expander_row.append(&cores_expander_chevron.widget());

    let expander_btn = crate::widgets::base::vp_button();
    expander_btn.set_child(Some(&expander_row));
    expander_btn.add_css_class(button::COMPACT);
    expander_btn.add_css_class(sp::EXPANDER_HEADER);
    cpu_section.append(&expander_btn);

    cpu_card.append(&cpu_section);
    top_row.append(&cpu_card);

    let memory_card = GtkBox::new(Orientation::Vertical, 0);
    memory_card.add_css_class(card::BASE);
    memory_card.add_css_class(sp::SECTION_CARD);

    let memory_section = GtkBox::new(Orientation::Vertical, 8);
    memory_section.append(&section_title("ram-symbolic", "Memory", &icons));

    let (memory_usage_row, memory_usage_label) = stat_row("Usage", 6);
    memory_section.append(&memory_usage_row);

    let memory_progress = ProgressBar::new();
    memory_progress.add_css_class(sp::PROGRESS_BAR);
    memory_section.append(&memory_progress);

    let memory_detail_label = Label::new(Some("-- / --"));
    memory_detail_label.add_css_class(color::MUTED);
    memory_detail_label.set_halign(Align::Start);
    memory_section.append(&memory_detail_label);

    memory_card.append(&memory_section);
    top_row.append(&memory_card);
    container.append(&top_row);

    let cores_revealer = Revealer::new();
    cores_revealer.set_transition_type(RevealerTransitionType::SlideDown);
    cores_revealer.set_transition_duration(ConfigManager::global().animation_duration(200));
    cores_revealer.set_reveal_child(false);

    let cpu_cores_box = GtkBox::new(Orientation::Vertical, 4);
    cpu_cores_box.add_css_class(sp::EXPANDER_CONTENT);
    cores_revealer.set_child(Some(&cpu_cores_box));
    container.append(&cores_revealer);

    // GPU section (full-width card, conditionally visible)
    let gpu_service = GpuService::global();
    let gpu_available = gpu_service.snapshot().available;

    let gpu_card = GtkBox::new(Orientation::Vertical, 0);
    gpu_card.add_css_class(card::BASE);
    gpu_card.add_css_class(sp::SECTION_CARD);
    gpu_card.add_css_class(sp::GPU_CARD);
    gpu_card.set_margin_top(8);
    gpu_card.set_visible(gpu_available);

    let gpu_section = GtkBox::new(Orientation::Vertical, 8);

    // Row 1: [icon] GPU  <clock · power · temp>
    let gpu_title_row = GtkBox::new(Orientation::Horizontal, 6);
    gpu_title_row.add_css_class(sp::SECTION_TITLE);
    gpu_title_row.add_css_class(sp::GPU_TITLE);

    let gpu_icon = icons.create_icon("video-display-symbolic", &[icon::TEXT, sp::SECTION_ICON]);
    gpu_title_row.append(&gpu_icon.widget());

    let gpu_label = Label::new(Some("GPU"));
    gpu_label.add_css_class(surface::POPOVER_TITLE);
    gpu_title_row.append(&gpu_label);

    let gpu_metrics_label = Label::new(None);
    gpu_metrics_label.add_css_class(color::MUTED);
    gpu_metrics_label.add_css_class(sp::GPU_METRICS);
    gpu_metrics_label.set_hexpand(true);
    gpu_metrics_label.set_halign(Align::End);
    gpu_title_row.append(&gpu_metrics_label);

    gpu_section.append(&gpu_title_row);

    let (gpu_usage_row, gpu_usage_label) = stat_row("Usage", 6);
    gpu_section.append(&gpu_usage_row);

    let gpu_progress = ProgressBar::new();
    gpu_progress.add_css_class(sp::PROGRESS_BAR);
    gpu_section.append(&gpu_progress);

    let (gpu_vram_row, gpu_vram_value_label) = stat_row("VRAM", 6);
    gpu_section.append(&gpu_vram_row);

    let gpu_vram_progress = ProgressBar::new();
    gpu_vram_progress.add_css_class(sp::PROGRESS_BAR);
    gpu_section.append(&gpu_vram_progress);

    let gpu_vram_detail_label = Label::new(Some("-- / --"));
    gpu_vram_detail_label.add_css_class(color::MUTED);
    gpu_vram_detail_label.set_halign(Align::Start);
    gpu_section.append(&gpu_vram_detail_label);

    gpu_card.append(&gpu_section);
    container.append(&gpu_card);

    let bottom_row = GtkBox::new(Orientation::Horizontal, 8);
    bottom_row.set_homogeneous(true);
    bottom_row.set_margin_top(8);

    let load_card = GtkBox::new(Orientation::Vertical, 0);
    load_card.add_css_class(card::BASE);
    load_card.add_css_class(sp::SECTION_CARD);

    let load_section = GtkBox::new(Orientation::Vertical, 8);
    load_section.append(&section_title("system-monitor-symbolic", "Load", &icons));

    let load_grid = GtkBox::new(Orientation::Horizontal, 12);
    load_grid.set_halign(Align::Fill);

    let col_1 = GtkBox::new(Orientation::Vertical, 2);
    let label_1 = Label::new(Some("1m"));
    label_1.add_css_class(color::MUTED);
    label_1.set_halign(Align::Start);
    col_1.append(&label_1);
    let load_1_label = Label::new(Some("--"));
    load_1_label.set_halign(Align::Start);
    load_1_label.set_width_chars(5);
    load_1_label.set_xalign(0.0);
    col_1.append(&load_1_label);
    col_1.set_hexpand(true);
    load_grid.append(&col_1);

    let col_5 = GtkBox::new(Orientation::Vertical, 2);
    let label_5 = Label::new(Some("5m"));
    label_5.add_css_class(color::MUTED);
    label_5.set_halign(Align::Start);
    col_5.append(&label_5);
    let load_5_label = Label::new(Some("--"));
    load_5_label.set_halign(Align::Start);
    load_5_label.set_width_chars(5);
    load_5_label.set_xalign(0.0);
    col_5.append(&load_5_label);
    col_5.set_hexpand(true);
    load_grid.append(&col_5);

    let col_15 = GtkBox::new(Orientation::Vertical, 2);
    let label_15 = Label::new(Some("15m"));
    label_15.add_css_class(color::MUTED);
    label_15.set_halign(Align::Start);
    col_15.append(&label_15);
    let load_15_label = Label::new(Some("--"));
    load_15_label.set_halign(Align::Start);
    load_15_label.set_width_chars(5);
    load_15_label.set_xalign(0.0);
    col_15.append(&load_15_label);
    col_15.set_hexpand(true);
    load_grid.append(&col_15);

    load_section.append(&load_grid);
    load_card.append(&load_section);
    bottom_row.append(&load_card);

    let network_card = GtkBox::new(Orientation::Vertical, 0);
    network_card.add_css_class(card::BASE);
    network_card.add_css_class(sp::SECTION_CARD);

    let network_section = GtkBox::new(Orientation::Vertical, 8);
    network_section.append(&section_title(
        "network-transmit-receive-symbolic",
        "Network",
        &icons,
    ));

    let net_grid = GtkBox::new(Orientation::Horizontal, 12);
    net_grid.set_halign(Align::Fill);

    let col_down = GtkBox::new(Orientation::Vertical, 2);
    let down_header = GtkBox::new(Orientation::Horizontal, 4);
    let down_icon = icons.create_icon(
        "go-down-symbolic",
        &[icon::TEXT, color::MUTED, sp::NETWORK_ICON],
    );
    down_header.append(&down_icon.widget());
    let label_down = Label::new(Some("Down"));
    label_down.add_css_class(color::MUTED);
    down_header.append(&label_down);
    col_down.append(&down_header);
    let net_download_label = Label::new(Some("--"));
    net_download_label.set_halign(Align::Start);
    net_download_label.set_width_chars(10);
    net_download_label.set_xalign(0.0);
    col_down.append(&net_download_label);
    col_down.set_hexpand(true);
    net_grid.append(&col_down);

    let col_up = GtkBox::new(Orientation::Vertical, 2);
    let up_header = GtkBox::new(Orientation::Horizontal, 4);
    let up_icon = icons.create_icon(
        "go-up-symbolic",
        &[icon::TEXT, color::MUTED, sp::NETWORK_ICON],
    );
    up_header.append(&up_icon.widget());
    let label_up = Label::new(Some("Up"));
    label_up.add_css_class(color::MUTED);
    up_header.append(&label_up);
    col_up.append(&up_header);
    let net_upload_label = Label::new(Some("--"));
    net_upload_label.set_halign(Align::Start);
    net_upload_label.set_width_chars(10);
    net_upload_label.set_xalign(0.0);
    col_up.append(&net_upload_label);
    col_up.set_hexpand(true);
    net_grid.append(&col_up);

    network_section.append(&net_grid);
    network_card.append(&network_section);
    bottom_row.append(&network_card);
    container.append(&bottom_row);

    let controller = SystemPopoverController {
        cpu_usage_label,
        cpu_temp_label,
        cpu_progress,
        cores_expander_label,
        cores_expander_chevron,
        cores_revealer,
        cpu_cores_box,
        cores_expanded,
        core_rows: Rc::new(RefCell::new(Vec::new())),
        memory_usage_label,
        memory_detail_label,
        memory_progress,
        net_download_label,
        net_upload_label,
        load_1_label,
        load_5_label,
        load_15_label,
        gpu_card,
        gpu_metrics_label,
        gpu_usage_label,
        gpu_progress,
        gpu_vram_value_label,
        gpu_vram_progress,
        gpu_vram_detail_label,
    };

    let controller_clone = controller.clone();
    expander_btn.connect_clicked(move |_| {
        controller_clone.toggle_cores();
    });

    controller.update_from_snapshot(&snapshot);

    let gpu_snapshot = gpu_service.snapshot();
    controller.update_from_gpu_snapshot(&gpu_snapshot);

    (container.upcast::<Widget>(), controller)
}

/// A binding that manages the system popover lifecycle for bar widgets.
#[derive(Clone)]
pub struct SystemPopoverBinding {
    controller: Rc<RefCell<Option<SystemPopoverController>>>,
    /// Held to keep the `Rc` alive; managed via clones in open/close closures.
    #[allow(dead_code)]
    gpu_callback_id: Rc<Cell<Option<CallbackId>>>,
}

impl SystemPopoverBinding {
    /// Create a new binding and wire up the popover menu on the given base widget.
    ///
    /// GPU polling is started when the popover opens and stopped when it closes,
    /// so that NVML calls don't prevent the GPU from entering D3cold sleep.
    /// A GPU service callback is also registered while the popover is open so that
    /// GPU metrics update live (even when there is no GPU bar widget).
    pub fn new(base: &crate::widgets::base::BaseWidget) -> Self {
        let menu_handle = base.create_menu(|| {
            // Replaced by wire_lifecycle before the popover is shown
            gtk4::Label::new(None).upcast::<Widget>()
        });
        Self::wire_lifecycle(&menu_handle)
    }

    /// Create a binding that uses an existing `MenuHandle` instead of creating
    /// one from a `BaseWidget`.
    ///
    /// Used by the merge-group wrapper in `bar.rs`: the wrapper owns a single
    /// shared `MenuHandle` and all passive widgets in the group share this
    /// binding to update the popover when it's open.
    pub(crate) fn new_for_menu(menu_handle: &Rc<crate::widgets::base::MenuHandle>) -> Self {
        Self::wire_lifecycle(menu_handle)
    }

    /// Shared lifecycle wiring: installs the builder, reuse-content mode,
    /// and on-show/on-close callbacks for GPU polling on a `MenuHandle`.
    fn wire_lifecycle(menu_handle: &Rc<crate::widgets::base::MenuHandle>) -> Self {
        let controller: Rc<RefCell<Option<SystemPopoverController>>> = Rc::new(RefCell::new(None));
        let gpu_callback_id: Rc<Cell<Option<CallbackId>>> = Rc::new(Cell::new(None));

        let controller_for_builder = controller.clone();
        menu_handle.set_builder(move || {
            let (widget, ctrl) = build_system_popover_with_controller();
            *controller_for_builder.borrow_mut() = Some(ctrl);
            widget
        });

        menu_handle.set_reuse_content(true);

        // Start GPU polling and push fresh snapshots each time the popover opens.
        let controller_for_show = controller.clone();
        let gpu_cb_for_show = gpu_callback_id.clone();
        menu_handle.set_on_show(move || {
            let gpu_service = GpuService::global();
            GpuService::request_polling(&gpu_service);

            if let Some(ctrl) = controller_for_show.borrow().as_ref() {
                let sys_snapshot = SystemService::global().snapshot();
                ctrl.update_from_snapshot(&sys_snapshot);
                let gpu_snapshot = gpu_service.snapshot();
                ctrl.update_from_gpu_snapshot(&gpu_snapshot);
            }

            let controller_for_gpu = controller_for_show.clone();
            let cb_id = gpu_service.connect(move |snapshot: &GpuSnapshot| {
                if let Some(ctrl) = controller_for_gpu.borrow().as_ref() {
                    ctrl.update_from_gpu_snapshot(snapshot);
                }
            });
            gpu_cb_for_show.set(Some(cb_id));
        });

        // Stop GPU polling when the popover closes.
        let gpu_cb_for_close = gpu_callback_id.clone();
        menu_handle.set_on_close(move || {
            if let Some(cb_id) = gpu_cb_for_close.take() {
                GpuService::global().disconnect(cb_id);
            }
            GpuService::global().release_polling();
        });

        Self {
            controller,
            gpu_callback_id,
        }
    }

    /// Update the popover if it's currently open.
    pub fn update_if_open(&self, snapshot: &SystemSnapshot) {
        if let Some(controller) = self.controller.borrow().as_ref() {
            controller.update_from_snapshot(snapshot);
        }
    }

    /// Update the GPU section of the popover if it's currently open.
    pub fn update_gpu_if_open(&self, snapshot: &GpuSnapshot) {
        if let Some(controller) = self.controller.borrow().as_ref() {
            controller.update_from_gpu_snapshot(snapshot);
        }
    }
}
