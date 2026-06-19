//! Low-battery heads-up coordinator.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use vibepanel_core::config::BatteryAlertConfig;

use crate::services::battery::{
    BatteryService, BatterySnapshot, battery_icon_name, rounded_pct_value,
};
use crate::services::callbacks::CallbackId;
use crate::services::desktop_notification::{self, Urgency};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlertLevel {
    None,
    Low,
    Critical,
}

pub struct BatteryAlertController {
    config: RefCell<BatteryAlertConfig>,
    alert_level: Cell<AlertLevel>,
    notification_id: Cell<Option<u32>>,
    notification_generation: Cell<u64>,
    callback_id: Cell<Option<CallbackId>>,
}

impl BatteryAlertController {
    fn new() -> Rc<Self> {
        let controller = Rc::new(Self {
            config: RefCell::new(BatteryAlertConfig::default()),
            alert_level: Cell::new(AlertLevel::None),
            notification_id: Cell::new(None),
            notification_generation: Cell::new(0),
            callback_id: Cell::new(None),
        });

        controller.connect_battery();
        controller
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<BatteryAlertController> = BatteryAlertController::new();
        }

        INSTANCE.with(|controller| controller.clone())
    }

    pub fn configure(self: &Rc<Self>, config: BatteryAlertConfig) {
        *self.config.borrow_mut() = config;
        self.alert_level.set(AlertLevel::None);
        self.close_active_notification();
        self.on_battery_changed(&BatteryService::global().snapshot());
    }

    fn connect_battery(self: &Rc<Self>) {
        let service = BatteryService::global();
        let this_weak = Rc::downgrade(self);
        let id = service.connect(move |snapshot| {
            if let Some(this) = this_weak.upgrade() {
                this.on_battery_changed(snapshot);
            }
        });
        self.callback_id.set(Some(id));
    }

    fn on_battery_changed(self: &Rc<Self>, snapshot: &BatterySnapshot) {
        let config = self.config.borrow();
        if !config.enabled || !battery_should_be_tracked(snapshot) {
            self.alert_level.set(AlertLevel::None);
            self.close_active_notification();
            return;
        }

        let percent = u32::from(rounded_pct_value(snapshot.percent.unwrap_or_default()));
        let level = alert_level_for_percent(percent, &config);

        if level == AlertLevel::None {
            self.alert_level.set(AlertLevel::None);
            self.close_active_notification();
            return;
        }

        let current_level = self.alert_level.get();
        if current_level == level {
            return;
        }

        self.alert_level.set(level);
        if current_level == AlertLevel::Critical && level == AlertLevel::Low {
            // Gauge jitter while discharging can move from Critical back to Low.
            // Keep the critical toast visible, but downgrade state so a later
            // re-entry into Critical can alert again.
            return;
        }

        drop(config);
        self.dispatch(percent, level);
    }

    fn dispatch(self: &Rc<Self>, percent: u32, level: AlertLevel) {
        self.close_active_notification();

        let (summary, body, urgency, icon) = match level {
            AlertLevel::Critical => (
                "Critical battery",
                format!("{percent}% remaining. Plug in now."),
                Urgency::Critical,
                "battery-caution-symbolic".to_string(),
            ),
            AlertLevel::Low => (
                "Low battery",
                format!("{percent}% remaining"),
                Urgency::Normal,
                battery_icon_name(percent.min(100) as u8, false),
            ),
            AlertLevel::None => unreachable!("dispatch only handles active alert levels"),
        };

        let generation = self.notification_generation.get();
        let this_weak = Rc::downgrade(self);
        desktop_notification::send_with_id(summary, &body, &icon, urgency, true, true, move |id| {
            let Some(this) = this_weak.upgrade() else {
                if let Some(id) = id {
                    desktop_notification::close(id);
                }
                return;
            };

            let Some(id) = id else {
                if this.notification_generation.get() == generation
                    && this.alert_level.get() == level
                {
                    this.alert_level.set(AlertLevel::None);
                }
                return;
            };

            if this.notification_generation.get() == generation
                && this.alert_level.get() != AlertLevel::None
            {
                this.notification_id.set(Some(id));
            } else {
                desktop_notification::close(id);
            }
        });
    }

    fn close_active_notification(&self) {
        self.notification_generation
            .set(self.notification_generation.get().wrapping_add(1));
        if let Some(id) = self.notification_id.take() {
            desktop_notification::close(id);
        }
    }
}

impl Drop for BatteryAlertController {
    fn drop(&mut self) {
        if let Some(id) = self.callback_id.take() {
            BatteryService::global().disconnect(id);
        }
    }
}

fn battery_should_be_tracked(snapshot: &BatterySnapshot) -> bool {
    snapshot.available && snapshot.percent.is_some() && snapshot.is_discharging()
}

fn alert_level_for_percent(percent: u32, config: &BatteryAlertConfig) -> AlertLevel {
    if percent <= config.critical_threshold {
        AlertLevel::Critical
    } else if percent <= config.low_threshold {
        AlertLevel::Low
    } else {
        AlertLevel::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::battery::{STATE_CHARGING, STATE_DISCHARGING, STATE_PENDING_DISCHARGE};

    fn snapshot(available: bool, percent: Option<f64>, state: Option<u32>) -> BatterySnapshot {
        BatterySnapshot {
            available,
            percent,
            state,
            energy_rate: None,
            time_to_empty: None,
            time_to_full: None,
        }
    }

    #[test]
    fn tracks_only_available_discharging_known_percent() {
        assert!(battery_should_be_tracked(&snapshot(
            true,
            Some(50.0),
            Some(STATE_DISCHARGING)
        )));
        assert!(battery_should_be_tracked(&snapshot(
            true,
            Some(50.0),
            Some(STATE_PENDING_DISCHARGE)
        )));
        assert!(!battery_should_be_tracked(&snapshot(
            false,
            Some(50.0),
            Some(STATE_DISCHARGING)
        )));
        assert!(!battery_should_be_tracked(&snapshot(
            true,
            None,
            Some(STATE_DISCHARGING)
        )));
        assert!(!battery_should_be_tracked(&snapshot(
            true,
            Some(50.0),
            Some(STATE_CHARGING)
        )));
    }

    #[test]
    fn classifies_low_and_critical_levels() {
        let config = BatteryAlertConfig {
            enabled: true,
            low_threshold: 20,
            critical_threshold: 5,
        };

        assert_eq!(alert_level_for_percent(21, &config), AlertLevel::None);
        assert_eq!(alert_level_for_percent(20, &config), AlertLevel::Low);
        assert_eq!(alert_level_for_percent(6, &config), AlertLevel::Low);
        assert_eq!(alert_level_for_percent(5, &config), AlertLevel::Critical);
    }
}
