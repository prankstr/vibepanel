//! System resource popover - detailed CPU, memory, GPU, disk, and network information.
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
//! │ ┌───────────────────────────┤  (conditional: all GPUs in one card)
//! │ │  GPU                      │
//! │ │  AMD Radeon          76%  │
//! │ │  NVIDIA RTX          41%  │
//! │ └───────────────────────────┤
//! ├─────────────────────────────┤
//! │ ┌───────────┐ ┌───────────┐ │
//! │ │ Disk I/O  │ │  Network  │ │
//! │ └───────────┘ └───────────┘ │
//! └─────────────────────────────┘
//! ```
//!
//! The CPU section has an expandable per-core breakdown that spans full width.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Label, Orientation, PolicyType, ProgressBar, Revealer,
    RevealerTransitionType, ScrolledWindow, Widget,
};

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::gpu::{
    GpuDeviceSnapshot, GpuHistorySample, GpuPowerState, GpuService, GpuSnapshot,
};
use crate::services::icons::{IconHandle, IconsService};
use crate::services::system::{
    SYSTEM_HISTORY_SAMPLES, SystemService, SystemSnapshot, format_bytes_long, format_speed,
};
use crate::styles::{button, card, color, icon, surface, system_popover as sp};
use crate::widgets::gpu_format;
use crate::widgets::history_graph::{HistoryGraph, HistoryScale, HistorySeries};
use crate::widgets::layer_shell_popover::animate_reveal;

const GPU_TITLE_MAX_CHARS: i32 = 44;
const GPU_GRAPH_HEIGHT: i32 = 56;
const SYSTEM_GRAPH_HEIGHT: i32 = 56;
const GPU_VALUE_MAX_CHARS: i32 = 20;
const GPU_DEVICES_MAX_HEIGHT: i32 = 360;

/// A single pre-allocated per-core row with its updatable widgets.
#[derive(Clone)]
struct CoreRow {
    bar: ProgressBar,
    pct_label: Label,
}

#[derive(Clone)]
struct GpuDeviceRow {
    container: GtkBox,
    header: GtkBox,
    graph: HistoryGraph,
    title_label: Label,
    usage_label: Label,
    temp_label: Label,
    metrics: GtkBox,
    vram: GpuMetricBar,
    power: GpuMetricBar,
    clock: GpuMetricBar,
}

#[derive(Clone)]
struct GpuMetricBar {
    container: GtkBox,
    bar: ProgressBar,
    value: Label,
}

impl GpuMetricBar {
    fn new(caption: &str) -> Self {
        let container = GtkBox::new(Orientation::Vertical, 4);
        container.set_hexpand(true);

        let label = Label::new(Some(caption));
        label.add_css_class(color::MUTED);
        label.set_halign(Align::Start);
        container.append(&label);

        let bar = ProgressBar::new();
        bar.add_css_class(sp::PROGRESS_BAR);
        container.append(&bar);

        let value = Label::new(Some("--"));
        value.add_css_class(color::MUTED);
        value.set_halign(Align::Start);
        value.set_xalign(0.0);
        value.set_max_width_chars(GPU_VALUE_MAX_CHARS);
        value.set_ellipsize(EllipsizeMode::End);
        container.append(&value);

        Self {
            container,
            bar,
            value,
        }
    }

    fn show(&self, value: &str, fraction: Option<f64>) {
        self.value.set_label(value);
        self.bar
            .set_fraction(fraction.unwrap_or(0.0).clamp(0.0, 1.0));
        self.bar
            .set_opacity(if fraction.is_some() { 1.0 } else { 0.35 });
    }
}

/// Controller owning the system popover UI elements and update logic.
#[derive(Clone)]
pub struct SystemPopoverController {
    // CPU section
    cpu_usage_label: Label,
    cpu_temp_label: Label,
    cpu_graph: HistoryGraph,
    cores_expander_label: Label,
    cores_expander_chevron: IconHandle,
    cores_revealer: Revealer,
    cpu_cores_box: GtkBox,
    cores_expanded: Rc<Cell<bool>>,
    core_rows: Rc<RefCell<Vec<CoreRow>>>,

    // Memory section
    memory_usage_label: Label,
    memory_detail_label: Label,
    memory_graph: HistoryGraph,

    // Network section
    net_download_label: Label,
    net_upload_label: Label,
    net_graph: HistoryGraph,

    // Disk I/O section
    disk_read_label: Label,
    disk_write_label: Label,
    disk_graph: HistoryGraph,

    // GPU section (conditional: only present when GPUs are detected)
    gpu_card: GtkBox,
    gpu_usage_label: Label,
    gpu_temp_label: Label,
    gpu_devices_box: GtkBox,
    gpu_device_rows: Rc<RefCell<Vec<GpuDeviceRow>>>,
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

        let history = SystemService::global().history();
        self.cpu_graph.set_series(vec![HistorySeries::solid(
            history
                .iter()
                .map(|sample| sample.cpu_usage.map(f64::from))
                .collect(),
        )]);
        self.memory_graph.set_series(vec![HistorySeries::solid(
            history
                .iter()
                .map(|sample| sample.memory_percent.map(f64::from))
                .collect(),
        )]);

        // Network
        self.net_download_label
            .set_label(&format_speed(snapshot.net_download_speed));
        self.net_upload_label
            .set_label(&format_speed(snapshot.net_upload_speed));
        self.net_graph.set_series(vec![
            HistorySeries::solid(
                history
                    .iter()
                    .map(|sample| sample.net_download_speed.map(|value| value as f64))
                    .collect(),
            ),
            HistorySeries::dashed(
                history
                    .iter()
                    .map(|sample| sample.net_upload_speed.map(|value| value as f64))
                    .collect(),
            ),
        ]);

        self.disk_read_label
            .set_label(&format_speed(snapshot.disk_read_speed));
        self.disk_write_label
            .set_label(&format_speed(snapshot.disk_write_speed));
        self.disk_graph.set_series(vec![
            HistorySeries::solid(
                history
                    .iter()
                    .map(|sample| sample.disk_read_speed.map(|value| value as f64))
                    .collect(),
            ),
            HistorySeries::dashed(
                history
                    .iter()
                    .map(|sample| sample.disk_write_speed.map(|value| value as f64))
                    .collect(),
            ),
        ]);
    }

    /// Update the GPU card from the latest GPU snapshot.
    pub fn update_from_gpu_snapshot(&self, snapshot: &GpuSnapshot) {
        let devices = &snapshot.devices;
        if devices.is_empty() {
            self.gpu_card.set_visible(false);
            return;
        }
        self.gpu_card.set_visible(true);
        self.sync_gpu_device_rows(devices.len());

        let show_index = devices.len() > 1;
        self.gpu_usage_label.set_visible(!show_index);
        self.gpu_temp_label.set_visible(!show_index);
        if let [device] = devices.as_slice() {
            update_gpu_headline(&self.gpu_usage_label, &self.gpu_temp_label, device);
        }

        let rows = self.gpu_device_rows.borrow();
        let gpu_service = GpuService::global();
        for (row, device) in rows.iter().zip(devices.iter()) {
            let history = gpu_service.history(device.device_index);
            update_gpu_device_row(row, device, &history, show_index);
        }
    }

    fn sync_gpu_device_rows(&self, count: usize) {
        let mut rows = self.gpu_device_rows.borrow_mut();
        if rows.len() == count {
            return;
        }

        while let Some(child) = self.gpu_devices_box.first_child() {
            self.gpu_devices_box.remove(&child);
        }
        rows.clear();

        for _ in 0..count {
            let row = build_gpu_device_row();
            self.gpu_devices_box.append(&row.container);
            rows.push(row);
        }
    }

    /// Toggle the cores expander visibility.
    fn toggle_cores(&self) {
        let expanded = !self.cores_expanded.get();
        self.cores_expanded.set(expanded);
        animate_reveal(&self.cores_revealer, expanded);

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

fn section_title_with_values(
    icon_name: &str,
    text: &str,
    icons: &IconsService,
) -> (GtkBox, GtkBox) {
    let container = GtkBox::new(Orientation::Horizontal, 6);
    container.add_css_class(sp::SECTION_TITLE);

    let icon_handle = icons.create_icon(icon_name, &[icon::TEXT, sp::SECTION_ICON]);
    container.append(&icon_handle.widget());

    let label = Label::new(Some(text));
    label.add_css_class(surface::POPOVER_TITLE);
    container.append(&label);

    let values = GtkBox::new(Orientation::Horizontal, 8);
    values.set_hexpand(true);
    values.set_halign(Align::End);
    container.append(&values);

    (container, values)
}

fn build_gpu_device_row() -> GpuDeviceRow {
    let container = GtkBox::new(Orientation::Vertical, 6);
    let header = GtkBox::new(Orientation::Horizontal, 8);

    let title_label = Label::new(Some("GPU"));
    title_label.set_halign(Align::Start);
    title_label.set_xalign(0.0);
    title_label.set_hexpand(true);
    title_label.set_ellipsize(EllipsizeMode::End);
    title_label.set_single_line_mode(true);
    title_label.set_max_width_chars(GPU_TITLE_MAX_CHARS);
    header.append(&title_label);

    let temp_label = Label::new(None);
    temp_label.add_css_class(color::MUTED);
    temp_label.set_halign(Align::End);
    header.append(&temp_label);

    let usage_label = Label::new(Some("--"));
    usage_label.add_css_class(color::ACCENT);
    usage_label.set_halign(Align::End);
    usage_label.set_width_chars(4);
    usage_label.set_xalign(1.0);
    header.append(&usage_label);

    let graph = HistoryGraph::new(
        crate::services::gpu::GPU_HISTORY_SAMPLES,
        GPU_GRAPH_HEIGHT,
        HistoryScale::Fixed {
            min: 0.0,
            max: 100.0,
        },
    );
    graph.widget().add_css_class(color::ACCENT);

    let vram = GpuMetricBar::new("VRAM");
    let power = GpuMetricBar::new("Power");
    let clock = GpuMetricBar::new("Clock");
    let metrics = GtkBox::new(Orientation::Horizontal, 12);
    metrics.set_homogeneous(true);
    for metric in [&vram, &power, &clock] {
        metrics.append(&metric.container);
    }

    container.append(&header);
    container.append(graph.widget());
    container.append(&metrics);

    GpuDeviceRow {
        container,
        header,
        graph,
        title_label,
        usage_label,
        temp_label,
        metrics,
        vram,
        power,
        clock,
    }
}

fn update_gpu_device_row(
    row: &GpuDeviceRow,
    snapshot: &GpuDeviceSnapshot,
    history: &[GpuHistorySample],
    show_index: bool,
) {
    row.header.set_visible(show_index);
    if show_index {
        row.title_label
            .set_label(&gpu_format::device_title(snapshot, true));
        update_gpu_headline(&row.usage_label, &row.temp_label, snapshot);
    }
    row.graph.set_series(vec![HistorySeries::solid(
        history
            .iter()
            .map(|sample| sample.usage.map(f64::from))
            .collect(),
    )]);

    if snapshot.power_state == GpuPowerState::Suspended {
        row.graph.widget().set_visible(false);
        row.metrics.set_visible(false);
        return;
    }

    row.graph.widget().set_visible(true);
    row.metrics.set_visible(true);
    row.vram.show(
        &gpu_format::vram(snapshot).unwrap_or_else(|| "--".to_string()),
        gpu_vram_fraction(snapshot),
    );
    row.power.show(
        &gpu_format::power(snapshot).unwrap_or_else(|| "--".to_string()),
        gpu_power_fraction(snapshot),
    );
    row.clock.show(
        &gpu_format::clock(snapshot).unwrap_or_else(|| "--".to_string()),
        gpu_clock_fraction(snapshot),
    );
}

fn update_gpu_headline(usage: &Label, temperature: &Label, snapshot: &GpuDeviceSnapshot) {
    if snapshot.power_state == GpuPowerState::Suspended {
        usage.set_label("Idle");
        temperature.set_visible(false);
        return;
    }
    usage.set_label(
        &snapshot
            .gpu_usage
            .map_or_else(|| "--".to_string(), |v| format!("{v:.0}%")),
    );
    if let Some(value) = snapshot.temperature {
        temperature.set_label(&format!("{value:.0}°C"));
        temperature.set_visible(true);
    } else {
        temperature.set_visible(false);
    }
}

fn gpu_vram_fraction(snapshot: &GpuDeviceSnapshot) -> Option<f64> {
    snapshot
        .vram_percent()
        .map(|value| f64::from(value / 100.0))
}

fn gpu_power_fraction(snapshot: &GpuDeviceSnapshot) -> Option<f64> {
    match (snapshot.power_watts, snapshot.power_limit_watts) {
        (Some(value), Some(limit)) if limit > 0.0 => Some(f64::from(value / limit)),
        _ => None,
    }
}

fn gpu_clock_fraction(snapshot: &GpuDeviceSnapshot) -> Option<f64> {
    match (snapshot.clock_mhz, snapshot.max_clock_mhz) {
        (Some(value), Some(limit)) if limit > 0 => Some(value as f64 / limit as f64),
        _ => None,
    }
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

    let (cpu_title, cpu_values) = section_title_with_values("cpu-symbolic", "CPU", &icons);
    let cpu_temp_label = Label::new(None);
    cpu_temp_label.add_css_class(color::MUTED);
    cpu_values.append(&cpu_temp_label);
    let cpu_usage_label = Label::new(Some("--"));
    cpu_usage_label.add_css_class(color::ACCENT);
    cpu_usage_label.set_width_chars(6);
    cpu_usage_label.set_xalign(1.0);
    cpu_values.append(&cpu_usage_label);
    cpu_section.append(&cpu_title);

    let cpu_graph = HistoryGraph::new(
        SYSTEM_HISTORY_SAMPLES,
        SYSTEM_GRAPH_HEIGHT,
        HistoryScale::Fixed {
            min: 0.0,
            max: 100.0,
        },
    );
    cpu_graph.widget().add_css_class(color::ACCENT);
    cpu_section.append(cpu_graph.widget());

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
    let (memory_title, memory_values) = section_title_with_values("ram-symbolic", "Memory", &icons);
    let memory_usage_label = Label::new(Some("--"));
    memory_usage_label.add_css_class(color::ACCENT);
    memory_usage_label.set_width_chars(6);
    memory_usage_label.set_xalign(1.0);
    memory_values.append(&memory_usage_label);
    memory_section.append(&memory_title);

    let memory_graph = HistoryGraph::new(
        SYSTEM_HISTORY_SAMPLES,
        SYSTEM_GRAPH_HEIGHT,
        HistoryScale::Fixed {
            min: 0.0,
            max: 100.0,
        },
    );
    memory_graph.widget().add_css_class(color::ACCENT);
    memory_section.append(memory_graph.widget());

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

    // GPU section (all detected GPUs in one full-width card)
    let gpu_service = GpuService::global();
    let gpu_snapshot = gpu_service.snapshot();

    let gpu_card = GtkBox::new(Orientation::Vertical, 0);
    gpu_card.add_css_class(card::BASE);
    gpu_card.add_css_class(sp::SECTION_CARD);
    gpu_card.add_css_class(sp::GPU_CARD);
    gpu_card.set_margin_top(8);
    gpu_card.set_visible(gpu_snapshot.available());

    let gpu_section = GtkBox::new(Orientation::Vertical, 8);
    let (gpu_title, gpu_values) =
        section_title_with_values("video-display-symbolic", "GPU", &icons);
    gpu_title.add_css_class(sp::GPU_TITLE);
    let gpu_temp_label = Label::new(None);
    gpu_temp_label.add_css_class(color::MUTED);
    gpu_values.append(&gpu_temp_label);
    let gpu_usage_label = Label::new(Some("--"));
    gpu_usage_label.add_css_class(color::ACCENT);
    gpu_usage_label.set_width_chars(4);
    gpu_usage_label.set_xalign(1.0);
    gpu_values.append(&gpu_usage_label);
    gpu_section.append(&gpu_title);

    let gpu_devices_box = GtkBox::new(Orientation::Vertical, 14);
    let gpu_devices_scroller = ScrolledWindow::new();
    gpu_devices_scroller.set_policy(PolicyType::Never, PolicyType::Automatic);
    gpu_devices_scroller.set_propagate_natural_height(true);
    gpu_devices_scroller.set_max_content_height(GPU_DEVICES_MAX_HEIGHT);
    gpu_devices_scroller.set_child(Some(&gpu_devices_box));
    gpu_section.append(&gpu_devices_scroller);
    gpu_card.append(&gpu_section);
    container.append(&gpu_card);

    let bottom_row = GtkBox::new(Orientation::Horizontal, 8);
    bottom_row.set_homogeneous(true);
    bottom_row.set_margin_top(8);

    let disk_card = GtkBox::new(Orientation::Vertical, 0);
    disk_card.add_css_class(card::BASE);
    disk_card.add_css_class(sp::SECTION_CARD);

    let disk_section = GtkBox::new(Orientation::Vertical, 8);
    disk_section.set_vexpand(true);
    disk_section.append(&section_title("disk-symbolic", "Disk I/O", &icons));

    let disk_graph = HistoryGraph::new(
        SYSTEM_HISTORY_SAMPLES,
        SYSTEM_GRAPH_HEIGHT,
        HistoryScale::Automatic {
            min: 0.0,
            headroom: 0.1,
        },
    );
    disk_graph.widget().add_css_class(color::ACCENT);
    disk_section.append(disk_graph.widget());

    let disk_grid = GtkBox::new(Orientation::Horizontal, 12);
    disk_grid.set_halign(Align::Fill);
    disk_grid.set_valign(Align::End);

    let disk_read = GtkBox::new(Orientation::Horizontal, 4);
    disk_read.set_hexpand(true);
    let read_marker = Label::new(Some("R"));
    read_marker.add_css_class(color::ACCENT);
    disk_read.append(&read_marker);
    let disk_read_label = Label::new(Some("--"));
    disk_read_label.add_css_class(color::ACCENT);
    disk_read_label.set_width_chars(10);
    disk_read_label.set_xalign(0.0);
    disk_read.append(&disk_read_label);
    disk_grid.append(&disk_read);

    let disk_write = GtkBox::new(Orientation::Horizontal, 4);
    disk_write.set_hexpand(true);
    let write_marker = Label::new(Some("W"));
    write_marker.add_css_class(color::MUTED);
    disk_write.append(&write_marker);
    let disk_write_label = Label::new(Some("--"));
    disk_write_label.add_css_class(color::MUTED);
    disk_write_label.set_width_chars(10);
    disk_write_label.set_xalign(0.0);
    disk_write.append(&disk_write_label);
    disk_grid.append(&disk_write);

    disk_section.append(&disk_grid);
    disk_card.append(&disk_section);
    bottom_row.append(&disk_card);

    let network_card = GtkBox::new(Orientation::Vertical, 0);
    network_card.add_css_class(card::BASE);
    network_card.add_css_class(sp::SECTION_CARD);

    let network_section = GtkBox::new(Orientation::Vertical, 8);
    network_section.set_vexpand(true);
    network_section.append(&section_title(
        "network-transmit-receive-symbolic",
        "Network",
        &icons,
    ));

    let net_graph = HistoryGraph::new(
        SYSTEM_HISTORY_SAMPLES,
        SYSTEM_GRAPH_HEIGHT,
        HistoryScale::Automatic {
            min: 0.0,
            headroom: 0.1,
        },
    );
    net_graph.widget().add_css_class(color::ACCENT);
    network_section.append(net_graph.widget());

    let net_grid = GtkBox::new(Orientation::Horizontal, 12);
    net_grid.set_halign(Align::Fill);
    net_grid.set_valign(Align::End);

    let col_down = GtkBox::new(Orientation::Horizontal, 4);
    col_down.set_hexpand(true);
    let down_icon = icons.create_icon(
        "go-down-symbolic",
        &[icon::TEXT, color::ACCENT, sp::NETWORK_ICON],
    );
    col_down.append(&down_icon.widget());
    let net_download_label = Label::new(Some("--"));
    net_download_label.add_css_class(color::ACCENT);
    net_download_label.set_halign(Align::Start);
    net_download_label.set_width_chars(10);
    net_download_label.set_xalign(0.0);
    col_down.append(&net_download_label);
    net_grid.append(&col_down);

    let col_up = GtkBox::new(Orientation::Horizontal, 4);
    col_up.set_hexpand(true);
    let up_icon = icons.create_icon(
        "go-up-symbolic",
        &[icon::TEXT, color::MUTED, sp::NETWORK_ICON],
    );
    col_up.append(&up_icon.widget());
    let net_upload_label = Label::new(Some("--"));
    net_upload_label.add_css_class(color::MUTED);
    net_upload_label.set_halign(Align::Start);
    net_upload_label.set_width_chars(10);
    net_upload_label.set_xalign(0.0);
    col_up.append(&net_upload_label);
    net_grid.append(&col_up);

    network_section.append(&net_grid);
    network_card.append(&network_section);
    bottom_row.append(&network_card);
    container.append(&bottom_row);

    let controller = SystemPopoverController {
        cpu_usage_label,
        cpu_temp_label,
        cpu_graph,
        cores_expander_label,
        cores_expander_chevron,
        cores_revealer,
        cpu_cores_box,
        cores_expanded,
        core_rows: Rc::new(RefCell::new(Vec::new())),
        memory_usage_label,
        memory_detail_label,
        memory_graph,
        net_download_label,
        net_upload_label,
        net_graph,
        disk_read_label,
        disk_write_label,
        disk_graph,
        gpu_card,
        gpu_usage_label,
        gpu_temp_label,
        gpu_devices_box,
        gpu_device_rows: Rc::new(RefCell::new(Vec::new())),
    };

    let controller_clone = controller.clone();
    expander_btn.connect_clicked(move |_| {
        controller_clone.toggle_cores();
    });

    controller.update_from_snapshot(&snapshot);

    controller.update_from_gpu_snapshot(&gpu_snapshot);

    (container.upcast::<Widget>(), controller)
}

/// Create and wire the system popover menu on a bar widget.
pub(crate) fn wire_system_popover(base: &crate::widgets::base::BaseWidget) {
    let menu_handle = base.create_menu(|| {
        // Replaced by wire_system_popover_for_menu before the popover is shown.
        gtk4::Label::new(None).upcast::<Widget>()
    });
    wire_system_popover_for_menu(&menu_handle);
}

/// Install the system popover builder and service lifecycle on an existing menu.
pub(crate) fn wire_system_popover_for_menu(menu_handle: &Rc<crate::widgets::base::MenuHandle>) {
    // Start polling now so popover graphs have history on first open.
    SystemService::global();

    let controller: Rc<RefCell<Option<SystemPopoverController>>> = Rc::new(RefCell::new(None));
    let gpu_callback_id: Rc<Cell<Option<CallbackId>>> = Rc::new(Cell::new(None));
    let system_callback_id: Rc<Cell<Option<CallbackId>>> = Rc::new(Cell::new(None));

    let controller_for_builder = controller.clone();
    menu_handle.set_builder(move || {
        let (widget, ctrl) = build_system_popover_with_controller();
        *controller_for_builder.borrow_mut() = Some(ctrl);
        widget
    });

    menu_handle.set_reuse_content(true);

    // Subscribe to service updates while the popover is open.
    let controller_for_show = controller.clone();
    let gpu_cb_for_show = gpu_callback_id.clone();
    let system_cb_for_show = system_callback_id.clone();
    menu_handle.set_on_show(move || {
        let gpu_service = GpuService::global();

        // A close-animation reversal fires on_show before on_close, so retain
        // existing subscriptions and polling ownership across the reversal.
        if gpu_cb_for_show.get().is_none() {
            GpuService::request_polling(&gpu_service);
            let controller_for_gpu = controller_for_show.clone();
            let cb_id = gpu_service.connect(move |snapshot: &GpuSnapshot| {
                if let Some(ctrl) = controller_for_gpu.borrow().as_ref() {
                    ctrl.update_from_gpu_snapshot(snapshot);
                }
            });
            gpu_cb_for_show.set(Some(cb_id));
        }

        if system_cb_for_show.get().is_none() {
            let controller_for_system = controller_for_show.clone();
            let cb_id = SystemService::global().connect(move |snapshot: &SystemSnapshot| {
                if let Some(ctrl) = controller_for_system.borrow().as_ref() {
                    ctrl.update_from_snapshot(snapshot);
                }
            });
            system_cb_for_show.set(Some(cb_id));
        }
    });

    // Stop service updates and GPU polling when the popover closes.
    let gpu_cb_for_close = gpu_callback_id.clone();
    let system_cb_for_close = system_callback_id.clone();
    menu_handle.set_on_close(move || {
        if let Some(cb_id) = gpu_cb_for_close.take() {
            GpuService::global().disconnect(cb_id);
            GpuService::global().release_polling();
        }

        if let Some(cb_id) = system_cb_for_close.take() {
            SystemService::global().disconnect(cb_id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{gpu_clock_fraction, gpu_power_fraction};
    use crate::services::gpu::GpuDeviceSnapshot;

    #[test]
    fn test_gpu_metric_fractions_use_real_limits() {
        let snapshot = GpuDeviceSnapshot {
            power_watts: Some(120.0),
            power_limit_watts: Some(300.0),
            clock_mhz: Some(1500),
            max_clock_mhz: Some(2500),
            ..Default::default()
        };
        assert!(gpu_power_fraction(&snapshot).is_some_and(|value| (value - 0.4).abs() < 0.001));
        assert_eq!(gpu_clock_fraction(&snapshot), Some(0.6));
    }
}
