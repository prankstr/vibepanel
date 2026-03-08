//! Battery popover - detailed battery information and power profile controls.

use std::cell::RefCell;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, Separator, Widget};

use crate::services::battery::{
    BatteryService, BatterySnapshot, STATE_CHARGING, STATE_FULLY_CHARGED,
};
use crate::services::power_profile::{PowerProfileService, PowerProfileSnapshot};
use crate::styles::{battery as bat, button, color, surface};

fn format_time(seconds: i64) -> String {
    if seconds <= 0 {
        return "Unknown".to_string();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

fn format_power(watts: Option<f64>) -> String {
    let Some(watts) = watts else {
        return "Unknown".to_string();
    };
    if watts < 1.0 {
        format!("{:.1} mW", watts * 1000.0)
    } else {
        format!("{:.2} W", watts)
    }
}

/// Convert UPower battery state code to human-readable text.
///
/// UPower state codes: 1=Charging, 2=Discharging, 3=Empty, 4=Fully Charged,
/// 5=Pending charge, 6=Pending discharge, 0/other=Unknown.
/// See: https://upower.freedesktop.org/docs/Device.html#Device:State
fn state_text(state: Option<u32>) -> String {
    match state {
        Some(STATE_CHARGING) => "Charging".to_string(),
        Some(2) => "Discharging".to_string(),
        Some(3) => "Empty".to_string(),
        Some(STATE_FULLY_CHARGED) => "Full".to_string(),
        Some(5) => "Pending charge".to_string(),
        Some(6) => "Pending discharge".to_string(),
        _ => "Unknown".to_string(),
    }
}

/// Controller owning the battery popover UI elements and update logic.
#[derive(Clone)]
pub struct BatteryPopoverController {
    percent_label: Label,
    state_label: Label,
    time_label: Label,
    power_label: Label,
    profile_buttons: RefCell<Vec<(Button, String)>>,
}

impl BatteryPopoverController {
    pub fn new(
        percent_label: &Label,
        state_label: &Label,
        time_label: &Label,
        power_label: &Label,
    ) -> Self {
        Self {
            percent_label: percent_label.clone(),
            state_label: state_label.clone(),
            time_label: time_label.clone(),
            power_label: power_label.clone(),
            profile_buttons: RefCell::new(Vec::new()),
        }
    }

    /// Rebuild the profile buttons for the given snapshot and return the
    /// updated profile section box. Callers are responsible for inserting
    /// it into the container.
    pub fn build_profile_section(&self, power_snapshot: &PowerProfileSnapshot) -> GtkBox {
        let section = GtkBox::new(Orientation::Vertical, 8);

        let title = Label::new(Some("Power Profile"));
        title.add_css_class(surface::POPOVER_TITLE);
        title.set_halign(Align::Start);
        section.append(&title);

        let profiles = &power_snapshot.available_profiles;
        let current = power_snapshot.current_profile.as_deref();

        self.profile_buttons.borrow_mut().clear();

        if profiles.is_empty() {
            let no_profiles = Label::new(Some("Power profiles not available"));
            no_profiles.add_css_class(bat::POPOVER_NO_PROFILES);
            no_profiles.add_css_class(color::MUTED);
            section.append(&no_profiles);
            return section;
        }

        let button_box = GtkBox::new(Orientation::Horizontal, 6);
        button_box.set_homogeneous(true);

        for profile in profiles {
            let label_text = title_case(&profile.replace('-', " "));
            let btn = crate::widgets::base::vp_button_with_label(&label_text);
            btn.add_css_class(bat::POPOVER_PROFILE_BUTTON);
            btn.set_hexpand(true);

            if Some(profile.as_str()) == current {
                btn.add_css_class(button::ACCENT);
            } else {
                btn.add_css_class(button::CARD);
            }

            self.profile_buttons
                .borrow_mut()
                .push((btn.clone(), profile.clone()));

            let profile_clone = profile.clone();
            btn.connect_clicked(move |_btn| {
                let svc = PowerProfileService::global();
                let _ = svc.set_profile(&profile_clone);
            });

            button_box.append(&btn);
        }

        section.append(&button_box);
        section
    }

    /// Refresh profile button CSS based on latest snapshot.
    pub fn refresh_profile_buttons(&self, power_snapshot: &PowerProfileSnapshot) {
        let current = power_snapshot.current_profile.as_deref();
        for (btn, profile_name) in self.profile_buttons.borrow_mut().iter_mut() {
            if Some(profile_name.as_str()) == current {
                btn.remove_css_class(button::CARD);
                btn.add_css_class(button::ACCENT);
            } else {
                btn.remove_css_class(button::ACCENT);
                btn.add_css_class(button::CARD);
            }
        }
    }

    /// Update text labels and profile buttons from the latest snapshots.
    pub fn update_from_snapshots(
        &self,
        battery_snapshot: &BatterySnapshot,
        power_snapshot: &PowerProfileSnapshot,
    ) {
        // Battery percentage
        if let Some(percent) = battery_snapshot.percent {
            self.percent_label
                .set_label(&format!("{:.0}%", percent.clamp(0.0, 100.0)));
        } else {
            self.percent_label.set_label("Unknown");
        }

        // Battery state
        self.state_label
            .set_label(&state_text(battery_snapshot.state));

        // Time remaining / until full
        if let Some(state) = battery_snapshot.state {
            if state == STATE_CHARGING {
                if let Some(ttf) = battery_snapshot.time_to_full {
                    self.time_label
                        .set_label(&format!("Time until full: {}", format_time(ttf)));
                } else {
                    self.time_label.set_label("Time until full: Unknown");
                }
            } else if state == 2 {
                if let Some(tte) = battery_snapshot.time_to_empty {
                    self.time_label
                        .set_label(&format!("Time remaining: {}", format_time(tte)));
                } else {
                    self.time_label.set_label("Time remaining: Unknown");
                }
            } else {
                self.time_label.set_label("Time: Unknown");
            }
        } else {
            self.time_label.set_label("Time: Unknown");
        }

        // Power draw
        self.power_label.set_label(&format!(
            "Power draw: {}",
            format_power(battery_snapshot.energy_rate)
        ));

        self.refresh_profile_buttons(power_snapshot);
    }
}

/// Title-case a string (capitalize first letter of each word).
fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a battery popover content widget bound to global services.
///
/// The widget is rebuilt each time the menu is shown so it always reflects
/// the most recent snapshots from BatteryService and PowerProfileService.
///
/// Returns both the root widget and a controller that can be used to
/// push live updates while the popover is open.
pub fn build_battery_popover_with_controller() -> (Widget, BatteryPopoverController) {
    let battery_service = BatteryService::global();
    let battery_snapshot = battery_service.snapshot();
    let power_service = PowerProfileService::global();
    let power_snapshot = power_service.snapshot();

    // Main container
    let container = GtkBox::new(Orientation::Vertical, 16);
    container.add_css_class(bat::POPOVER);

    // Battery info section
    let info_section = GtkBox::new(Orientation::Vertical, 8);
    let title = Label::new(Some("Battery Information"));
    title.add_css_class(surface::POPOVER_TITLE);
    title.set_halign(Align::Start);
    info_section.append(&title);

    let percent_label = Label::new(Some("--%"));
    percent_label.add_css_class(bat::POPOVER_PERCENT);
    percent_label.set_halign(Align::Start);
    info_section.append(&percent_label);

    let state_label = Label::new(Some("--"));
    state_label.add_css_class(bat::POPOVER_STATE);
    state_label.set_halign(Align::Start);
    info_section.append(&state_label);

    let time_label = Label::new(Some("--"));
    time_label.add_css_class(bat::POPOVER_TIME);
    time_label.add_css_class(color::MUTED);
    time_label.set_halign(Align::Start);
    info_section.append(&time_label);

    let power_label = Label::new(Some("--"));
    power_label.add_css_class(bat::POPOVER_POWER);
    power_label.add_css_class(color::MUTED);
    power_label.set_halign(Align::Start);
    info_section.append(&power_label);

    container.append(&info_section);

    // Separator
    let separator = Separator::new(Orientation::Horizontal);
    separator.add_css_class(bat::POPOVER_SEPARATOR);
    container.append(&separator);

    // Initialise controller and profile section
    let controller =
        BatteryPopoverController::new(&percent_label, &state_label, &time_label, &power_label);

    let profile_section = controller.build_profile_section(&power_snapshot);
    container.append(&profile_section);

    // Initial content update from current snapshots
    controller.update_from_snapshots(&battery_snapshot, &power_snapshot);

    (container.clone().upcast::<Widget>(), controller)
}
