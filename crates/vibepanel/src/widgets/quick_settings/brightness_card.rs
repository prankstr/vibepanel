//! Brightness card for Quick Settings panel.
//!
//! This module contains:
//! - Brightness row building (icon, slider)
//! - State change handling

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Scale};

use super::components::SliderRow;
use crate::services::brightness::BrightnessSnapshot;
use crate::services::icons::IconHandle;
use crate::styles::qs;

pub fn brightness_icon_name(percent: u32) -> &'static str {
    if percent == 0 {
        "display-brightness-off-symbolic"
    } else if percent < 33 {
        "display-brightness-low-symbolic"
    } else if percent < 67 {
        "display-brightness-medium-symbolic"
    } else {
        "display-brightness-high-symbolic"
    }
}
/// State for the Brightness card in the Quick Settings panel.
pub struct BrightnessCardState {
    /// Brightness slider.
    pub slider: RefCell<Option<Scale>>,
    /// Brightness icon handle.
    pub icon_handle: RefCell<Option<IconHandle>>,
    /// Flag to prevent slider feedback loop.
    pub updating: Cell<bool>,
}

impl BrightnessCardState {
    pub fn new() -> Self {
        Self {
            slider: RefCell::new(None),
            icon_handle: RefCell::new(None),
            updating: Cell::new(false),
        }
    }
}

impl Default for BrightnessCardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Container for brightness row widgets.
pub struct BrightnessRowWidgets {
    /// The outer row container.
    pub row: GtkBox,
    /// Handle to the brightness icon.
    pub icon_handle: IconHandle,
    /// The brightness slider.
    pub slider: Scale,
}

/// Build the brightness row with icon and slider.
///
/// Uses `SliderRow` for consistent styling with other slider rows.
pub fn build_brightness_row() -> BrightnessRowWidgets {
    let result = SliderRow::builder()
        .icon("display-brightness-symbolic")
        .range(1.0, 100.0) // Min 1 to avoid black screen
        .step(1.0)
        .with_spacer(true) // Match audio row width
        .build();

    // Add row identifier for CSS targeting
    result.container.add_css_class(qs::BRIGHTNESS);

    BrightnessRowWidgets {
        row: result.container,
        icon_handle: result.icon_handle,
        slider: result.slider,
    }
}

/// Handle Brightness state changes from BrightnessService.
pub fn on_brightness_changed(state: &BrightnessCardState, snapshot: &BrightnessSnapshot) {
    // Update brightness slider (with flag to prevent feedback loop)
    if let Some(slider) = state.slider.borrow().as_ref() {
        state.updating.set(true);
        slider.set_value(snapshot.percent as f64);
        state.updating.set(false);
        slider.set_sensitive(snapshot.available);
    }
    // Update brightness icon based on current level
    if let Some(icon_handle) = state.icon_handle.borrow().as_ref() {
        let icon_name = brightness_icon_name(snapshot.percent);
        icon_handle.set_icon(icon_name);
    }
}
