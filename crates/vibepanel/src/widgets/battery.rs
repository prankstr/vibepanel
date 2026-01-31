//! Battery widget - displays current battery status via the shared
//! `BatteryService` (UPower-backed).
//!
//! The BatteryService is responsible for DBus/UPower integration and
//! exposes canonical snapshots; this widget subscribes to those
//! snapshots and renders icon/text/CSS/tooltip accordingly.
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed battery icons
//! - `TooltipManager` for styled tooltips

use gtk4::Label;
use gtk4::prelude::*;
use vibepanel_core::config::WidgetEntry;

use crate::services::battery::{
    BatteryService, BatterySnapshot, STATE_CHARGING, STATE_FULLY_CHARGED,
};
use crate::services::icons::IconHandle;
use crate::styles::{class, state, widget};
use std::cell::RefCell;
use std::rc::Rc;

use crate::services::power_profile::{PowerProfileService, PowerProfileSnapshot};
use crate::services::tooltip::TooltipManager;
use crate::widgets::WidgetConfig;
use crate::widgets::base::BaseWidget;
use crate::widgets::battery_popover::{
    BatteryPopoverController, build_battery_popover_with_controller,
};
use crate::widgets::warn_unknown_options;

const DEFAULT_SHOW_PERCENTAGE: bool = true;
const DEFAULT_SHOW_ICON: bool = true;

/// Configuration for the battery widget.
#[derive(Debug, Clone)]
pub struct BatteryConfig {
    /// Whether to show the textual percentage.
    pub show_percentage: bool,
    /// Whether to show an icon.
    pub show_icon: bool,
}

impl WidgetConfig for BatteryConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options("battery", entry, &["show_percentage", "show_icon"]);

        let show_percentage = entry
            .options
            .get("show_percentage")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_PERCENTAGE);

        let show_icon = entry
            .options
            .get("show_icon")
            .and_then(|v| v.as_bool())
            .unwrap_or(DEFAULT_SHOW_ICON);

        Self {
            show_percentage,
            show_icon,
        }
    }
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            show_percentage: DEFAULT_SHOW_PERCENTAGE,
            show_icon: DEFAULT_SHOW_ICON,
        }
    }
}

/// Battery widget that displays icon, percentage, and opens a popover on click.
pub struct BatteryWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Icon handle from IconsService (uses Material Symbols when available).
    icon_handle: IconHandle,
    /// Percentage text label.
    percentage_label: Label,
    /// Whether to show the textual percentage.
    show_percentage: bool,
    /// Whether to show an icon.
    show_icon: bool,
    /// Optional live controller used to update the popover while open.
    popover_controller: Rc<RefCell<Option<BatteryPopoverController>>>,
}

impl BatteryWidget {
    /// Create a new battery widget with the given configuration.
    pub fn new(config: BatteryConfig) -> Self {
        let base = BaseWidget::new(&[widget::BATTERY]);

        // Initial tooltip until the first snapshot arrives.
        base.set_tooltip("Battery: unknown");

        // Create icon via BaseWidget/IconsService for themed rendering
        let icon_handle = base.add_icon("battery-missing", &[widget::BATTERY_ICON]);

        let percentage_label =
            base.add_label(None, &[widget::BATTERY_PERCENTAGE, class::VCENTER_CAPS]);

        // Shared controller storage between the widget and the menu builder.
        let controller_cell: Rc<RefCell<Option<BatteryPopoverController>>> =
            Rc::new(RefCell::new(None));
        let controller_for_builder = controller_cell.clone();

        // Create a popover menu for detailed battery info.
        base.create_menu(move || {
            let (widget, controller) = build_battery_popover_with_controller();
            *controller_for_builder.borrow_mut() = Some(controller);
            widget
        });

        let widget = Self {
            base,
            icon_handle,
            percentage_label,
            show_percentage: config.show_percentage,
            show_icon: config.show_icon,
            popover_controller: controller_cell.clone(),
        };

        // Initial neutral state until the first snapshot arrives.
        widget.update_widgets_from_state(false, None, None);

        // Subscribe to the shared BatteryService for live updates.
        let battery_service = BatteryService::global();
        {
            let container = widget.base.widget().clone();
            let icon_handle = widget.icon_handle.clone();
            let percentage_label = widget.percentage_label.clone();
            let show_percentage = widget.show_percentage;
            let show_icon = widget.show_icon;
            let controller_for_cb = widget.popover_controller.clone();

            battery_service.connect(move |snapshot: &BatterySnapshot| {
                update_widgets_from_state_impl(
                    &container,
                    &icon_handle,
                    &percentage_label,
                    show_percentage,
                    show_icon,
                    snapshot.available,
                    snapshot.percent,
                    snapshot.state,
                );

                // If the popover content has been built, push live updates.
                if let Some(controller) = controller_for_cb.borrow().as_ref() {
                    let power_snapshot = PowerProfileService::global().snapshot();
                    controller.update_from_snapshots(snapshot, &power_snapshot);
                }
            });
        }

        // Subscribe to power profile updates so profile button styles stay in sync
        // even when changes are triggered externally.
        let power_service = PowerProfileService::global();
        {
            let controller_for_cb = widget.popover_controller.clone();
            power_service.connect(move |power_snapshot: &PowerProfileSnapshot| {
                if let Some(controller) = controller_for_cb.borrow().as_ref() {
                    let battery_snapshot = BatteryService::global().snapshot();
                    controller.update_from_snapshots(&battery_snapshot, power_snapshot);
                }
            });
        }

        widget
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }

    /// Update the GTK widgets from a logical battery state.
    ///
    /// - `available` is whether the UPower service is available
    /// - `percent` is 0.0-100.0 if known
    /// - `state` is the UPower state code (u32), if known
    fn update_widgets_from_state(&self, available: bool, percent: Option<f64>, state: Option<u32>) {
        update_widgets_from_state_impl(
            self.base.widget(),
            &self.icon_handle,
            &self.percentage_label,
            self.show_percentage,
            self.show_icon,
            available,
            percent,
            state,
        );
    }
}

/// Update the visual widget state given canonical battery info.
///
/// Uses `IconHandle` for icon updates, ensuring all theme mapping goes through
/// `IconsService`. CSS state classes are applied to the icon widget.
#[allow(clippy::too_many_arguments)]
fn update_widgets_from_state_impl(
    container: &gtk4::Box,
    icon_handle: &IconHandle,
    percentage_label: &Label,
    show_percentage: bool,
    show_icon: bool,
    available: bool,
    percent: Option<f64>,
    state: Option<u32>,
) {
    // Handle service unavailability (UPower not running)
    if !available {
        container.add_css_class(state::SERVICE_UNAVAILABLE);
        icon_handle.remove_css_class(widget::BATTERY_CHARGING);
        icon_handle.remove_css_class(widget::BATTERY_LOW);

        if show_icon {
            icon_handle.set_icon("battery-missing");
            icon_handle.widget().set_visible(true);
        } else {
            icon_handle.widget().set_visible(false);
        }

        if show_percentage {
            percentage_label.set_label("?");
            percentage_label.set_visible(true);
        } else {
            percentage_label.set_visible(false);
        }

        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(container, "Battery: Service unavailable");
        return;
    }
    container.remove_css_class(state::SERVICE_UNAVAILABLE);

    // Convert to a rounded 0-100 value if known.
    let rounded_opt = percent.map(rounded_pct_value);
    // Treat both "Charging" (1) and "Fully Charged" (4) as "plugged in" for accent color
    // and for the charging icon glyph. When the charger is connected, the icon should
    // reflect that state visually with both color and the charging variant icon.
    let plugged_in = matches!(state, Some(STATE_CHARGING) | Some(STATE_FULLY_CHARGED));
    let low = matches!(rounded_opt, Some(p) if p <= 20);

    // Update CSS state classes via IconHandle methods (survives theme switches).
    icon_handle.remove_css_class(widget::BATTERY_CHARGING);
    icon_handle.remove_css_class(widget::BATTERY_LOW);

    if plugged_in {
        icon_handle.add_css_class(widget::BATTERY_CHARGING);
    } else if low {
        icon_handle.add_css_class(widget::BATTERY_LOW);
    }

    // Icon - update via IconHandle (theme mapping handled internally)
    // Use plugged_in for the charging icon variant (shows bolt when charger connected)
    if show_icon {
        let icon_name = match rounded_opt {
            Some(pct) => battery_icon_name(pct, plugged_in),
            None => "battery-missing".to_string(),
        };
        icon_handle.set_icon(&icon_name);
        icon_handle.widget().set_visible(true);
    } else {
        icon_handle.widget().set_visible(false);
    }

    // Percentage text
    if show_percentage {
        let text = match rounded_opt {
            Some(pct) => readable_pct(pct),
            None => "?".to_string(),
        };
        percentage_label.set_label(&text);
        percentage_label.set_visible(true);
    } else {
        percentage_label.set_visible(false);
    }

    // Build tooltip text with battery percentage and state.
    // Use TooltipManager for styled tooltips.
    let tooltip = match (percent, state) {
        (None, _) => "Battery: unknown".to_string(),
        (Some(p), Some(s)) => {
            let pct = rounded_pct_value(p);
            let mut text = format!("Battery: {}", readable_pct(pct));
            let state_text = if s == STATE_CHARGING {
                "Charging"
            } else if s == STATE_FULLY_CHARGED {
                "Full"
            } else {
                "Discharging"
            };
            text.push_str("\nState: ");
            text.push_str(state_text);
            text
        }
        (Some(p), None) => {
            let pct = rounded_pct_value(p);
            format!("Battery: {}", readable_pct(pct))
        }
    };

    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(container, &tooltip);
}

/// Round a floating-point percentage (0.0 - 100.0) to a u8, clamped.
///
/// NaN is treated as 0; infinities are clamped to the 0-100 range.
pub fn rounded_pct_value(percent: f64) -> u8 {
    if percent.is_nan() {
        return 0;
    }
    let clamped = percent.clamp(0.0, 100.0);
    clamped.round() as u8
}

/// Format a rounded percentage value as readable text, e.g. "57%".
pub fn readable_pct(percent: u8) -> String {
    format!("{}%", percent)
}

/// Return a symbolic icon name for the given battery level.
///
/// Returns names like "battery-full", "battery-high-charging", etc.
/// These are then mapped to Material Symbols glyphs by `IconsService`.
///
/// Thresholds (8 levels to match Material icon granularity):
/// - full (>=95%), high (>=80%), medium-high (>=60%), medium (>=40%)
/// - medium-low (>=25%), low (>=10%), critical (<10%)
pub fn battery_icon_name(percent: u8, charging: bool) -> String {
    let level = if percent >= 95 {
        "full"
    } else if percent >= 80 {
        "high"
    } else if percent >= 60 {
        "medium-high"
    } else if percent >= 40 {
        "medium"
    } else if percent >= 25 {
        "medium-low"
    } else if percent >= 10 {
        "low"
    } else {
        "critical"
    };

    if charging {
        format!("battery-{}-charging", level)
    } else {
        format!("battery-{}", level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rounded_pct_value_basic() {
        assert_eq!(rounded_pct_value(0.0), 0);
        assert_eq!(rounded_pct_value(12.3), 12);
        assert_eq!(rounded_pct_value(12.5), 13);
        assert_eq!(rounded_pct_value(99.9), 100);
        assert_eq!(rounded_pct_value(150.0), 100);
        assert_eq!(rounded_pct_value(-5.0), 0);
    }

    #[test]
    fn test_rounded_pct_value_non_finite() {
        assert_eq!(rounded_pct_value(f64::NAN), 0);
        assert_eq!(rounded_pct_value(f64::INFINITY), 100);
        assert_eq!(rounded_pct_value(f64::NEG_INFINITY), 0);
    }

    #[test]
    fn test_readable_pct() {
        assert_eq!(readable_pct(0), "0%");
        assert_eq!(readable_pct(57), "57%");
        assert_eq!(readable_pct(100), "100%");
    }

    #[test]
    fn test_battery_icon_name_discharge() {
        assert_eq!(battery_icon_name(100, false), "battery-full");
        assert_eq!(battery_icon_name(95, false), "battery-full");
        assert_eq!(battery_icon_name(85, false), "battery-high");
        assert_eq!(battery_icon_name(67, false), "battery-medium-high");
        assert_eq!(battery_icon_name(50, false), "battery-medium");
        assert_eq!(battery_icon_name(30, false), "battery-medium-low");
        assert_eq!(battery_icon_name(15, false), "battery-low");
        assert_eq!(battery_icon_name(5, false), "battery-critical");
    }

    #[test]
    fn test_battery_icon_name_charging() {
        assert_eq!(battery_icon_name(95, true), "battery-full-charging");
        assert_eq!(battery_icon_name(65, true), "battery-medium-high-charging");
        assert_eq!(battery_icon_name(50, true), "battery-medium-charging");
        assert_eq!(battery_icon_name(30, true), "battery-medium-low-charging");
        assert_eq!(battery_icon_name(5, true), "battery-critical-charging");
    }

    #[test]
    fn test_battery_config_defaults() {
        let entry = WidgetEntry {
            name: "battery".to_string(),
            options: Default::default(),
        };
        let config = BatteryConfig::from_entry(&entry);
        assert!(config.show_percentage);
        assert!(config.show_icon);
    }
}
