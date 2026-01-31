//! Updates card for Quick Settings panel.
//!
//! This module contains:
//! - Updates card state
//! - Card building with expandable details
//! - Update list population
//! - Refresh and upgrade button handlers

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::pango::{EllipsizeMode, WrapMode};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Label, Orientation, PolicyType, Revealer, ScrolledWindow};
use tracing::debug;

use super::components::ToggleCard;
use super::ui_helpers::{
    ExpandableCard, ExpandableCardBase, ScanButton, clear_list_box, create_qs_list_box,
    set_icon_active, set_subtitle_active,
};
use super::window::current_quick_settings_window;
use crate::services::surfaces::SurfaceStyleManager;
use crate::services::updates::{UpdatesService, UpdatesSnapshot};
use crate::styles::{color, qs, row};
use crate::widgets::updates_common::{
    format_last_check, format_repo_summary, icon_for_state, spawn_upgrade_terminal,
};

/// State for the Updates card in the Quick Settings panel.
pub struct UpdatesCardState {
    pub base: ExpandableCardBase,
    pub card_box: RefCell<Option<GtkBox>>,
    /// Refresh button (self-contained with animation).
    pub refresh_button: RefCell<Option<Rc<ScanButton>>>,
    /// Last check label in the details.
    pub last_check_label: RefCell<Option<Label>>,
}

impl UpdatesCardState {
    pub fn new() -> Self {
        Self {
            base: ExpandableCardBase::new(),
            card_box: RefCell::new(None),
            refresh_button: RefCell::new(None),
            last_check_label: RefCell::new(None),
        }
    }
}

impl Default for UpdatesCardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpandableCard for UpdatesCardState {
    fn base(&self) -> &ExpandableCardBase {
        &self.base
    }
}

/// Build the Updates card and revealer for the Quick Settings panel.
///
/// Returns `(card, revealer, expander_button)` - caller is responsible for
/// accordion registration via `AccordionManager::setup_expander`.
pub fn build_updates_card(state: &Rc<UpdatesCardState>) -> (GtkBox, Revealer, Option<Button>) {
    let snapshot = UpdatesService::global().snapshot();

    let subtitle_text = format_repo_summary(&snapshot);
    let icon_name = icon_for_state(&snapshot);
    let has_updates = snapshot.update_count > 0;

    let card = ToggleCard::builder()
        .icon(icon_name)
        .label("Updates")
        .subtitle(&subtitle_text)
        .active(has_updates)
        .sensitive(snapshot.available)
        .icon_active(has_updates)
        .with_expander(true)
        .build();

    // Add card identifier for CSS targeting
    card.card.add_css_class(qs::UPDATES);

    // Store references
    *state.card_box.borrow_mut() = Some(card.card.clone());
    *state.base.toggle.borrow_mut() = Some(card.toggle.clone());
    *state.base.card_icon.borrow_mut() = Some(card.icon_handle.clone());
    *state.base.subtitle.borrow_mut() = card.subtitle.clone();
    *state.base.arrow.borrow_mut() = card.expander_icon.clone();

    // Connect toggle handler - opens terminal with upgrade command
    {
        let toggle = card.toggle.clone();
        toggle.connect_toggled(move |toggle| {
            // Only act on activation, not deactivation
            if toggle.is_active() {
                let snapshot = UpdatesService::global().snapshot();
                if let Some(pm) = snapshot.package_manager {
                    // Close the panel before spawning terminal
                    if let Some(qs) = current_quick_settings_window() {
                        qs.hide_panel();
                    }

                    if let Err(e) = spawn_upgrade_terminal(pm, None) {
                        tracing::error!("Failed to spawn upgrade terminal: {}", e);
                    }
                }
                // Reset toggle state (it's not a persistent toggle)
                toggle.set_active(false);
            }
        });
    }

    // Build revealer with details
    let revealer = Revealer::new();
    revealer.set_reveal_child(false);
    revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);

    let details = build_updates_details(state);
    revealer.set_child(Some(&details.container));

    *state.base.revealer.borrow_mut() = Some(revealer.clone());

    (card.card, revealer, card.expander_button)
}

/// Result of building updates details section.
pub struct UpdatesDetailsResult {
    pub container: GtkBox,
}

/// Build the updates details section with refresh button and update list.
pub fn build_updates_details(state: &Rc<UpdatesCardState>) -> UpdatesDetailsResult {
    let container = GtkBox::new(Orientation::Vertical, 4);
    container.add_css_class(qs::UPDATES_DETAILS);
    container.set_margin_top(4);

    // Top row: refresh button on left, last check on right
    let top_row = GtkBox::new(Orientation::Horizontal, 8);

    // Refresh button (styled like wifi/bluetooth scan button)
    let refresh_btn = ScanButton::with_label("Refresh", || {
        debug!("Updates: refresh button clicked");
        UpdatesService::global().refresh();
    });

    top_row.append(refresh_btn.widget());
    *state.refresh_button.borrow_mut() = Some(refresh_btn);

    // Last check label (right side)
    let last_check_label = Label::new(None);
    last_check_label.add_css_class(qs::UPDATES_LAST_CHECK);
    last_check_label.add_css_class(row::QS_SUBTITLE);
    last_check_label.add_css_class(color::MUTED);
    last_check_label.set_hexpand(true);
    last_check_label.set_xalign(1.0);
    top_row.append(&last_check_label);
    *state.last_check_label.borrow_mut() = Some(last_check_label);

    container.append(&top_row);

    // Scrolled window for update list
    let scrolled = ScrolledWindow::new();
    scrolled.set_policy(PolicyType::Never, PolicyType::Automatic);
    scrolled.set_max_content_height(200);
    scrolled.set_propagate_natural_height(true);
    scrolled.add_css_class(qs::UPDATES_SCROLL);

    let list_box = create_qs_list_box();
    list_box.add_css_class(qs::UPDATES_LIST);
    scrolled.set_child(Some(&list_box));
    container.append(&scrolled);

    *state.base.list_box.borrow_mut() = Some(list_box.clone());

    // Populate initial state
    let snapshot = UpdatesService::global().snapshot();
    populate_updates_list(state, &snapshot);

    UpdatesDetailsResult { container }
}

/// Handle updates snapshot changes.
pub fn on_updates_changed(state: &UpdatesCardState, snapshot: &UpdatesSnapshot) {
    // Update card icon
    if let Some(icon) = state.base.card_icon.borrow().as_ref() {
        let icon_name = icon_for_state(snapshot);
        icon.set_icon(icon_name);
        set_icon_active(icon, snapshot.update_count > 0);
    }

    // Update subtitle
    if let Some(subtitle) = state.base.subtitle.borrow().as_ref() {
        let text = format_repo_summary(snapshot);
        subtitle.set_label(&text);
        subtitle.set_visible(!text.is_empty());
        set_subtitle_active(subtitle, snapshot.update_count > 0);
    }

    // Update toggle sensitivity
    let is_actionable = snapshot.available && snapshot.update_count > 0;
    if let Some(toggle) = state.base.toggle.borrow().as_ref() {
        toggle.set_sensitive(is_actionable);
        toggle.set_active(false);
    }

    if let Some(card_box) = state.card_box.borrow().as_ref() {
        if is_actionable {
            card_box.remove_css_class(qs::CARD_DISABLED);
        } else {
            card_box.add_css_class(qs::CARD_DISABLED);
        }
    }

    // Update refresh button label and animation
    update_refresh_ui(state, snapshot);

    // Update last check label
    if let Some(label) = state.last_check_label.borrow().as_ref() {
        let text = format!("Last check: {}", format_last_check(snapshot.last_check));
        label.set_label(&text);
    }

    // Update list
    populate_updates_list(state, snapshot);
    // Apply Pango font attrs to dynamically created list rows
    if let Some(list_box) = state.base.list_box.borrow().as_ref() {
        SurfaceStyleManager::global().apply_pango_attrs_all(list_box);
    }
}

/// Update the refresh button UI and animate while checking.
fn update_refresh_ui(state: &UpdatesCardState, snapshot: &UpdatesSnapshot) {
    if let Some(refresh_btn) = state.refresh_button.borrow().as_ref() {
        refresh_btn.set_sensitive(!snapshot.checking && snapshot.available);
        refresh_btn.set_scanning(snapshot.checking);
    }
}

/// Populate the updates list from a snapshot.
fn populate_updates_list(state: &UpdatesCardState, snapshot: &UpdatesSnapshot) {
    let Some(list_box) = state.base.list_box.borrow().as_ref().cloned() else {
        return;
    };

    clear_list_box(&list_box);

    // Handle error state
    if let Some(ref error) = snapshot.error {
        let row = create_message_row(&format!("Error: {}", error));
        row.add_css_class(qs::UPDATES_ERROR);
        list_box.append(&row);
        return;
    }

    // Handle checking state
    if snapshot.checking && snapshot.update_count == 0 {
        let row = create_message_row("Checking for updates...");
        list_box.append(&row);
        return;
    }

    // Handle no updates
    if snapshot.update_count == 0 {
        let row = create_message_row("System is up to date");
        list_box.append(&row);
        return;
    }

    // Build a single text block with all packages grouped by repo
    let mut repos: Vec<_> = snapshot.updates_by_repo.iter().collect();
    repos.sort_by_key(|(name, _)| *name);

    for (repo, updates) in repos {
        // Collect all package names, one per line
        let pkg_names: Vec<&str> = updates.iter().map(|u| u.name.as_str()).collect();
        let pkg_list = pkg_names.join("\n");

        // Repo as title, packages as wrapping subtitle
        let title = format!("{} ({})", repo, updates.len());
        let row = create_updates_row(&title, &pkg_list);
        list_box.append(&row);
    }
}

/// Create a simple message row.
fn create_message_row(text: &str) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.add_css_class(row::QS);
    row.add_css_class(row::BASE);
    row.set_activatable(false);

    let label = Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_wrap(true);
    label.set_wrap_mode(WrapMode::WordChar);
    label.add_css_class(row::QS_TITLE);
    label.add_css_class(color::PRIMARY);
    row.set_child(Some(&label));
    row
}

/// Create an updates row with a wrapping subtitle for package names.
fn create_updates_row(title: &str, packages: &str) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.add_css_class(row::QS);
    row.add_css_class(row::BASE);
    row.set_activatable(false);

    let vbox = GtkBox::new(Orientation::Vertical, 2);
    vbox.add_css_class(row::QS_CONTENT);

    // Title with ellipsis to prevent long repo names from expanding the window
    let title_label = Label::new(Some(title));
    title_label.set_xalign(0.0);
    title_label.set_hexpand(true);
    title_label.set_ellipsize(EllipsizeMode::End);
    title_label.add_css_class(row::QS_TITLE);
    title_label.add_css_class(color::PRIMARY);
    vbox.append(&title_label);

    // Package names wrap within the available width
    let pkg_label = Label::new(Some(packages));
    pkg_label.set_xalign(0.0);
    pkg_label.set_hexpand(true);
    pkg_label.set_wrap(true);
    pkg_label.set_wrap_mode(WrapMode::WordChar);
    pkg_label.add_css_class(row::QS_SUBTITLE);
    pkg_label.add_css_class(color::MUTED);
    vbox.append(&pkg_label);

    row.set_child(Some(&vbox));
    row
}
