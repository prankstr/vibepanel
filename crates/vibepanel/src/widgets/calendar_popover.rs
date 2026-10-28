use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Once;

use chrono::{Datelike, Local, NaiveDate, Timelike};
use gtk4::cairo;
use gtk4::gdk::RGBA;
use gtk4::glib;
use gtk4::glib::markup_escape_text;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Calendar, DrawingArea, Label, Orientation, Overlay, PolicyType,
    ScrolledWindow, SizeGroup, SizeGroupMode, Widget,
};
use tr::tr;

use crate::services::calendar::{CalendarEvent, CalendarService, CalendarSnapshot};
use crate::services::icons::IconsService;
use crate::styles::{calendar as cal, color, icon, surface};
use crate::widgets::weather_popover::build_weather_content_reactive;

pub type ClockPopoverRefresh = Rc<dyn Fn()>;
pub type CalendarSnapshotRefresh = Rc<dyn Fn(&CalendarSnapshot)>;

const EVENTS_LIST_MAX_HEIGHT: i32 = 360;
const EVENT_LABEL_WIDTH_CHARS: i32 = 26;
const GTK_CALENDAR_DAY_COUNT: usize = 42;
static INVALID_CALENDAR_GRID_WARNING: Once = Once::new();

/// Build a calendar popover for the clock widget.
///
/// Shows a month view calendar with custom previous/next navigation, a
/// "go to today" button, and a header label. Toggles a `show-today` CSS class
/// when the currently viewed month matches the real current month.
///
/// Returns the widget and a refresh callback. The refresh callback navigates
/// the calendar to the real current date — call it on each open so the user
/// always sees today's month, even when the widget is reused across cycles.
pub fn build_clock_calendar_popover(
    show_week_numbers: bool,
    show_weather: bool,
    events_enabled: bool,
) -> (
    Widget,
    ClockPopoverRefresh,
    Option<ClockPopoverRefresh>,
    Option<CalendarSnapshotRefresh>,
) {
    // "Today" is stored in a Cell so the on_show refresh callback can update
    // it when the popover is reused across midnight boundaries.
    let today = Rc::new(Cell::new(Local::now().date_naive()));
    let visible_month = Rc::new(Cell::new(today.get()));
    let selected_date = Rc::new(Cell::new(today.get()));
    // Flag to prevent signal handler from interfering during programmatic updates
    let updating = Rc::new(Cell::new(false));

    // Main container
    let container = GtkBox::new(Orientation::Vertical, 12);
    container.add_css_class(cal::POPOVER);

    let compact_size_groups = (show_weather && events_enabled).then(|| {
        (
            SizeGroup::new(SizeGroupMode::Horizontal),
            SizeGroup::new(SizeGroupMode::Horizontal),
        )
    });

    let calendar_card = GtkBox::new(Orientation::Vertical, 0);
    if show_weather || events_enabled {
        calendar_card.add_css_class(cal::CARD);
    }
    if let Some((left_size_group, _)) = &compact_size_groups {
        left_size_group.add_widget(&calendar_card);
    }

    // Header: left-aligned label + right-aligned navigation buttons
    let header_box = GtkBox::new(Orientation::Horizontal, 8);

    // Month/year label - left-aligned, expands to push nav buttons right
    let header_label = Label::new(None);
    header_label.add_css_class(surface::POPOVER_TITLE);
    header_label.set_valign(Align::Center);
    header_label.set_hexpand(true);
    header_label.set_xalign(0.0);

    header_box.append(&header_label);

    // Navigation button group: [prev] [today] [next]
    let nav_box = GtkBox::new(Orientation::Horizontal, 0);
    nav_box.set_valign(Align::Start);

    let prev_button = crate::widgets::base::vp_button_from_icon_name("go-previous-symbolic");
    prev_button.add_css_class(surface::POPOVER_ICON_BTN);
    prev_button.set_has_frame(false);
    prev_button.set_focus_on_click(false);

    let icons = IconsService::global();
    let today_icon = icons.create_icon("calendar-today", &[icon::ICON]);
    today_icon.widget().set_halign(Align::Center);
    today_icon.widget().set_valign(Align::Center);
    let today_button = crate::widgets::base::vp_button();
    today_button.set_child(Some(&today_icon.widget()));
    today_button.add_css_class(surface::POPOVER_ICON_BTN);
    today_button.set_has_frame(false);
    today_button.set_focus_on_click(false);
    today_button.set_tooltip_text(Some(&tr!("Go to today")));

    let next_button = crate::widgets::base::vp_button_from_icon_name("go-next-symbolic");
    next_button.add_css_class(surface::POPOVER_ICON_BTN);
    next_button.set_has_frame(false);
    next_button.set_focus_on_click(false);

    nav_box.append(&prev_button);
    nav_box.append(&today_button);
    nav_box.append(&next_button);

    header_box.append(&nav_box);
    calendar_card.append(&header_box);

    // Calendar widget
    let calendar = Calendar::new();
    calendar.set_show_heading(false);
    calendar.set_show_week_numbers(show_week_numbers);
    calendar.add_css_class(cal::WIDGET);
    calendar.add_css_class(cal::GRID);
    calendar.set_halign(Align::Fill);
    // Initially show today styling since we start in the current month
    calendar.add_css_class(cal::SHOW_TODAY);
    if events_enabled {
        calendar.add_css_class(cal::EVENTS_ENABLED);
    }
    // Picking a day only does something when it shows that day's events.
    calendar.set_focusable(events_enabled);
    if events_enabled {
        select_day_with_arrows(&calendar);
    }

    // GtkCalendar owns selection, navigation, locale, and accessibility; this overlay
    // only paints event dots above its existing day labels.
    let event_dot_state = Rc::new(RefCell::new(CalendarEventDotState::default()));
    let event_dots = DrawingArea::new();
    event_dots.set_hexpand(true);
    event_dots.set_vexpand(true);
    event_dots.set_halign(Align::Fill);
    event_dots.set_valign(Align::Fill);
    event_dots.set_can_target(false);
    {
        let calendar = calendar.downgrade();
        let event_dot_state = event_dot_state.clone();
        event_dots.set_draw_func(move |area, cr, _width, _height| {
            let Some(calendar) = calendar.upgrade() else {
                return;
            };
            draw_event_dots(area, &calendar, cr, &event_dot_state.borrow());
        });
    }

    // Wrapper to center the calendar overlay in the popover.
    let wrapper = GtkBox::new(Orientation::Vertical, 0);
    wrapper.set_halign(Align::Center);

    let overlay = Overlay::new();
    overlay.set_child(Some(&calendar));
    overlay.add_overlay(&event_dots);

    if show_week_numbers {
        // Align the week-number header with GtkCalendar's internal week-number column.
        let w_label = Label::new(Some("w"));
        w_label.add_css_class("week-number-header");
        w_label.set_halign(Align::Start);
        w_label.set_valign(Align::Start);

        overlay.add_overlay(&w_label);
    }
    wrapper.append(&overlay);

    calendar_card.append(&wrapper);

    let calendar_events_layout = if events_enabled {
        let layout = GtkBox::new(Orientation::Horizontal, 12);
        layout.add_css_class(cal::EVENTS_LAYOUT);
        layout.append(&calendar_card);
        Some(layout)
    } else {
        container.append(&calendar_card);
        None
    };

    let (events_content, events_title) = if events_enabled {
        let events_card = GtkBox::new(Orientation::Vertical, 8);
        events_card.add_css_class(cal::EVENTS_CARD);
        if let Some((_, right_size_group)) = &compact_size_groups {
            right_size_group.add_widget(&events_card);
        }

        let title = Label::new(None);
        title.add_css_class(surface::POPOVER_TITLE);
        title.set_xalign(0.0);
        events_card.append(&title);

        let content = GtkBox::new(Orientation::Vertical, 6);
        content.add_css_class(cal::EVENTS_LIST);
        let scroller = ScrolledWindow::new();
        scroller.set_policy(PolicyType::Never, PolicyType::Automatic);
        scroller.set_child(Some(&content));
        scroller.set_max_content_height(EVENTS_LIST_MAX_HEIGHT);
        scroller.set_propagate_natural_height(true);
        // Rows aren't interactive: a focus stop only when there's overflow
        // to scroll with Page Up/Down.
        scroller.set_can_focus(false);
        scroller.vadjustment().connect_changed(glib::clone!(
            #[weak]
            scroller,
            move |adj| scroller.set_can_focus(adj.upper() > adj.page_size())
        ));
        events_card.append(&scroller);
        if let Some(layout) = &calendar_events_layout {
            layout.append(&events_card);
        }
        (Some(content), Some(title))
    } else {
        (None, None)
    };

    let weather_result = if show_weather {
        let (weather, refresh) = build_weather_content_reactive(compact_size_groups);
        Some((weather, refresh))
    } else {
        None
    };

    let weather_refresh = match (calendar_events_layout, weather_result) {
        (Some(layout), Some((weather, refresh))) => {
            container.append(&layout);
            container.append(&weather);
            Some(refresh)
        }
        (Some(layout), None) => {
            container.append(&layout);
            None
        }
        (None, Some((weather, refresh))) => {
            container.append(&weather);
            Some(refresh)
        }
        (None, None) => None,
    };

    let update_events = {
        let events_content = events_content.clone();
        let selected_date = selected_date.clone();
        let visible_month = visible_month.clone();
        let calendar = calendar.downgrade();
        let event_dots = event_dots.clone();
        let event_dot_state = event_dot_state.clone();
        move |snapshot: &CalendarSnapshot| {
            let Some(calendar) = calendar.upgrade() else {
                return;
            };
            let visible_month = visible_month.get();
            if !snapshot.matches_focus_month(visible_month) {
                return;
            }
            apply_event_marks(&calendar, visible_month, snapshot);
            *event_dot_state.borrow_mut() =
                CalendarEventDotState::from_snapshot(visible_month, snapshot);
            event_dots.queue_draw();
            if let Some(content) = &events_content {
                rebuild_event_list(content, selected_date.get(), snapshot);
            }
        }
    };

    let events_refresh = if events_enabled {
        let update_events = update_events.clone();
        Some(
            Rc::new(move |snapshot: &CalendarSnapshot| update_events(snapshot))
                as CalendarSnapshotRefresh,
        )
    } else {
        None
    };

    // Helper closures --------------------------------------------------------

    // Update header label text from a NaiveDate (Month YYYY).
    let update_header = {
        let header_label = header_label.clone();
        move |date: NaiveDate| {
            header_label.set_label(&date.format("%B %Y").to_string());
        }
    };

    let set_calendar_date = {
        let calendar = calendar.downgrade();
        let updating = updating.clone();
        move |date: NaiveDate| {
            let Some(calendar) = calendar.upgrade() else {
                return;
            };
            if calendar_selected_date(&calendar) == Some(date) {
                return;
            }

            updating.set(true);
            calendar.set_day(1);
            calendar.set_year(date.year());
            calendar.set_month(date.month0() as i32);
            calendar.set_day(date.day() as i32);
            updating.set(false);
        }
    };

    let update_calendar_style = {
        let calendar = calendar.downgrade();
        let today = today.clone();
        move |date: NaiveDate| {
            let Some(calendar) = calendar.upgrade() else {
                return;
            };
            let today = today.get();
            let is_current_month = date.month() == today.month() && date.year() == today.year();
            if is_current_month {
                calendar.add_css_class(cal::SHOW_TODAY);
            } else {
                calendar.remove_css_class(cal::SHOW_TODAY);
            }
        }
    };

    let sync_calendar_state: Rc<dyn Fn(NaiveDate)> = {
        let visible_month = visible_month.clone();
        let selected_date = selected_date.clone();
        let events_title = events_title.clone();
        let update_header = update_header.clone();
        let update_calendar_style = update_calendar_style.clone();
        Rc::new(move |date| {
            let month = date.with_day(1).unwrap_or(date);
            selected_date.set(date);
            visible_month.set(month);
            if let Some(title) = &events_title {
                title.set_label(&date.format("%A, %B %-d").to_string());
            }
            update_header(month);
            update_calendar_style(month);
        })
    };

    let navigate_calendar: Rc<dyn Fn(NaiveDate)> = {
        let sync_calendar_state = sync_calendar_state.clone();
        Rc::new(move |date| {
            sync_calendar_state(date);
            set_calendar_date(date);
        })
    };

    // Initial sync to today's month.
    let initial_date = visible_month.get();
    navigate_calendar(initial_date);

    // Navigation button handlers ---------------------------------------------

    let change_month: Rc<dyn Fn(i32)> = {
        let selected_date = selected_date.clone();
        let navigate_calendar = navigate_calendar.clone();
        Rc::new(move |delta| {
            let new_date = add_months(selected_date.get(), delta);
            navigate_calendar(new_date);
            if events_enabled {
                CalendarService::global().refresh(new_date);
            }
        })
    };

    {
        let change_month = change_month.clone();
        prev_button.connect_clicked(move |_| change_month(-1));
    }

    {
        let navigate_calendar = navigate_calendar.clone();
        let today = today.clone();
        today_button.connect_clicked(move |_| {
            let today = today.get();
            navigate_calendar(today);
            if events_enabled {
                CalendarService::global().refresh(today);
            }
        });
    }

    {
        let change_month = change_month.clone();
        next_button.connect_clicked(move |_| change_month(1));
    }

    // GtkCalendar can change months through scrolling or spillover-day clicks
    // without emitting day-selected. The date property covers every such change.
    {
        let visible_month = visible_month.clone();
        let sync_calendar_state = sync_calendar_state.clone();
        let updating = updating.clone();
        let update_events = update_events.clone();
        calendar.connect_date_notify(move |calendar| {
            if updating.get() {
                return;
            }

            let Some(date) = calendar_selected_date(calendar) else {
                return;
            };
            let current = visible_month.get();
            let Some(notified_month) = NaiveDate::from_ymd_opt(date.year(), date.month(), 1) else {
                return;
            };
            let month_changed = notified_month != current;
            sync_calendar_state(date);
            if !events_enabled {
                return;
            }
            if month_changed {
                CalendarService::global().refresh(notified_month);
            } else {
                update_events(&CalendarService::global().snapshot());
            }
        });
    }

    // Refresh callback — navigates calendar to the real current date.
    // Called by on_show when the popover is reused across open/close cycles.
    let weather_refresh_for_show = weather_refresh.clone();
    let refresh: ClockPopoverRefresh = {
        let navigate_calendar = navigate_calendar.clone();
        Rc::new(move || {
            let new_today = Local::now().date_naive();
            today.set(new_today);
            navigate_calendar(new_today);
            if events_enabled {
                CalendarService::global().refresh(new_today);
            }
            if let Some(refresh_weather) = &weather_refresh_for_show {
                refresh_weather();
            }
        })
    };

    (
        container.upcast::<Widget>(),
        refresh,
        weather_refresh,
        events_refresh,
    )
}

/// The selected day is the keyboard position: arrows move it within the shown
/// month and leave the grid past it. GtkCalendar's own keyboard cursor only
/// moves without selecting, and has no API to sync it with the selection.
/// Handles hjkl too: the popover's navigation passes them through.
fn select_day_with_arrows(calendar: &Calendar) {
    use crate::widgets::layer_shell_popover::{
        ANY_MODIFIER, claim_arrow_keys, nav_arrow_key, plain_nav_key,
    };
    use gtk4::gdk::Key;
    use gtk4::glib::Propagation::{Proceed, Stop};
    claim_arrow_keys(calendar.upcast_ref());
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    keys.connect_key_pressed(|controller, keyval, _, state| {
        if state.intersects(ANY_MODIFIER) {
            return Proceed;
        }
        let Some(calendar) = controller.widget().and_downcast::<Calendar>() else {
            return Proceed;
        };
        let rtl = calendar.direction() == gtk4::TextDirection::Rtl;
        let keyval = nav_arrow_key(keyval, state).unwrap_or_else(|| plain_nav_key(keyval));
        let days = match keyval {
            Key::Left if rtl => 1,
            Key::Left => -1,
            Key::Right if rtl => -1,
            Key::Right => 1,
            Key::Up => -7,
            Key::Down => 7,
            // The day is already selected; GTK's would pick its stale cursor.
            Key::space | Key::KP_Space | Key::Return | Key::KP_Enter => return Stop,
            _ => return Proceed,
        };
        let Some(date) = calendar_selected_date(&calendar) else {
            return Proceed;
        };
        match date.checked_add_signed(chrono::Duration::days(days)) {
            // Not via set_calendar_date: its `updating` guard would skip the
            // date notify that refreshes the events list.
            Some(next) if next.month() == date.month() => calendar.set_day(next.day() as i32),
            _ => {
                if let Some(root) = calendar.root() {
                    crate::widgets::layer_shell_popover::move_focus_for_arrow(
                        root.upcast_ref(),
                        keyval,
                    );
                }
            }
        }
        Stop
    });
    calendar.add_controller(keys);
}

fn rebuild_event_list(content: &GtkBox, date: NaiveDate, snapshot: &CalendarSnapshot) {
    while let Some(child) = content.first_child() {
        content.remove(&child);
    }

    if let Some(error) = &snapshot.error {
        append_state_label(content, error);
        return;
    }

    let Some(events) = snapshot
        .events_by_date
        .get(&date)
        .filter(|events| !events.is_empty())
    else {
        let state_text = if snapshot.loading {
            tr!("Loading events...")
        } else {
            tr!("No events")
        };
        append_state_label(content, &state_text);
        return;
    };

    for event in events {
        append_event_row(content, event, date);
    }
}

fn apply_event_marks(calendar: &Calendar, visible_month: NaiveDate, snapshot: &CalendarSnapshot) {
    calendar.clear_marks();
    if snapshot.error.is_some() {
        return;
    }

    for date in snapshot.events_by_date.keys() {
        if date.year() == visible_month.year() && date.month() == visible_month.month() {
            calendar.mark_day(date.day());
        }
    }
}

#[derive(Default)]
struct CalendarEventDotState {
    visible_month: Option<NaiveDate>,
    dots_by_date: std::collections::BTreeMap<NaiveDate, Vec<RGBA>>,
}

impl CalendarEventDotState {
    fn from_snapshot(visible_month: NaiveDate, snapshot: &CalendarSnapshot) -> Self {
        let mut dots_by_date = std::collections::BTreeMap::new();
        if snapshot.error.is_some() {
            return Self {
                visible_month: Some(visible_month),
                dots_by_date,
            };
        }

        for (date, events) in &snapshot.events_by_date {
            let mut colors = Vec::new();
            for event in events {
                let Some(color) = event.color.as_deref().and_then(parse_event_dot_color) else {
                    continue;
                };
                if !colors.contains(&color) {
                    colors.push(color);
                }
                if colors.len() == 3 {
                    break;
                }
            }
            dots_by_date.insert(*date, colors);
        }

        Self {
            visible_month: Some(visible_month),
            dots_by_date,
        }
    }
}

fn draw_event_dots(
    area: &DrawingArea,
    calendar: &Calendar,
    cr: &cairo::Context,
    state: &CalendarEventDotState,
) {
    let Some(visible_month) = state.visible_month else {
        return;
    };
    if state.dots_by_date.is_empty() {
        return;
    }

    for (label, date) in visible_day_labels(calendar, visible_month) {
        let Some(colors) = state.dots_by_date.get(&date) else {
            continue;
        };
        let Some(bounds) = label.compute_bounds(area.upcast_ref::<Widget>()) else {
            continue;
        };
        let fallback_color = if colors.is_empty() {
            // The day label resolves its own foreground, including today-cell contrast.
            let mut foreground = label.color();
            foreground.set_alpha(0.78);
            Some(foreground)
        } else {
            None
        };

        let dot_radius = 2.0;
        let gap = 3.0;
        let dot_count = colors.len().max(1) as f32;
        let total_width = dot_count * dot_radius * 2.0 + (dot_count - 1.0) * gap;
        let mut x = bounds.x() + bounds.width() / 2.0 - total_width / 2.0 + dot_radius;
        let y = bounds.y() + bounds.height() - 4.5;

        for color in colors.iter().chain(fallback_color.iter()) {
            cr.set_source_rgba(
                f64::from(color.red()),
                f64::from(color.green()),
                f64::from(color.blue()),
                f64::from(color.alpha()),
            );
            cr.arc(
                f64::from(x),
                f64::from(y),
                f64::from(dot_radius),
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
            x += dot_radius * 2.0 + gap;
        }
    }
}

fn visible_day_labels(calendar: &Calendar, visible_month: NaiveDate) -> Vec<(Label, NaiveDate)> {
    let mut labels = Vec::new();
    collect_day_labels(calendar.upcast_ref::<Widget>(), &mut labels);
    // GTK localizes label text, so derive dates from fixed grid positions instead.
    let leading_days = labels
        .iter()
        .take_while(|label| label.has_css_class("other-month"))
        .count();
    let trailing_days = labels
        .iter()
        .rev()
        .take_while(|label| label.has_css_class("other-month"))
        .count();
    let Some(dates) = visible_grid_dates(visible_month, leading_days, trailing_days, labels.len())
    else {
        INVALID_CALENDAR_GRID_WARNING.call_once(|| {
            tracing::warn!(
                "GtkCalendar grid structure is incompatible; colored event dots are disabled"
            );
        });
        return Vec::new();
    };

    labels.into_iter().zip(dates).collect()
}

fn visible_grid_dates(
    visible_month: NaiveDate,
    leading_days: usize,
    trailing_days: usize,
    day_count: usize,
) -> Option<Vec<NaiveDate>> {
    if day_count != GTK_CALENDAR_DAY_COUNT || leading_days > 7 {
        return None;
    }
    let month_start = NaiveDate::from_ymd_opt(visible_month.year(), visible_month.month(), 1)?;
    let next_month = add_months(month_start, 1);
    let days_in_month = next_month.pred_opt()?.day() as usize;
    // GTK internals are not public API. Reject traversal changes rather than
    // attaching dots to incorrect dates.
    if leading_days + days_in_month + trailing_days != day_count {
        return None;
    }
    let first_date = month_start.checked_sub_signed(chrono::Duration::days(leading_days as i64))?;
    (0..day_count)
        .map(|offset| first_date.checked_add_signed(chrono::Duration::days(offset as i64)))
        .collect()
}

fn calendar_selected_date(calendar: &Calendar) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(
        calendar.year(),
        (calendar.month() + 1) as u32,
        calendar.day() as u32,
    )
}

fn collect_day_labels(widget: &Widget, labels: &mut Vec<Label>) {
    // GtkCalendar exposes day labels only through its CSS node tree.
    if let Some(label) = widget.downcast_ref::<Label>()
        && label.has_css_class("day-number")
    {
        labels.push(label.clone());
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        collect_day_labels(&current, labels);
        child = current.next_sibling();
    }
}

fn add_months(date: NaiveDate, delta: i32) -> NaiveDate {
    let total_months = date.year() * 12 + date.month0() as i32 + delta;
    let year = total_months.div_euclid(12);
    let month0 = total_months.rem_euclid(12) as u32;
    for day in (1..=date.day()).rev() {
        if let Some(result) = NaiveDate::from_ymd_opt(year, month0 + 1, day) {
            return result;
        }
    }
    date
}

fn parse_event_dot_color(color: &str) -> Option<RGBA> {
    parse_event_color(color).map(|mut rgba| {
        rgba.set_alpha(0.78);
        rgba
    })
}

fn parse_event_color(color: &str) -> Option<RGBA> {
    RGBA::parse(color.trim()).ok()
}

fn event_marker_color(color: &str) -> Option<String> {
    let rgba = parse_event_color(color)?;
    let channel = |value: f32| (value * 255.0).round() as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.red()),
        channel(rgba.green()),
        channel(rgba.blue())
    ))
}

fn append_state_label(content: &GtkBox, text: &str) {
    let label = Label::new(Some(text));
    label.add_css_class(cal::EVENTS_STATE);
    label.add_css_class(color::MUTED);
    label.set_xalign(0.0);
    label.set_width_chars(EVENT_LABEL_WIDTH_CHARS);
    label.set_max_width_chars(EVENT_LABEL_WIDTH_CHARS);
    label.set_wrap(true);
    label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    content.append(&label);
}

fn append_event_row(content: &GtkBox, event: &CalendarEvent, date: NaiveDate) {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.add_css_class(cal::EVENT_ROW);

    let marker = Label::new(Some("•"));
    marker.add_css_class(cal::EVENT_MARKER);
    marker.set_valign(Align::Start);
    if let Some(color) = event.color.as_deref().and_then(event_marker_color) {
        marker.set_markup(&format!("<span foreground=\"{color}\">•</span>"));
    }
    row.append(&marker);

    let text = GtkBox::new(Orientation::Vertical, 2);
    text.set_hexpand(true);

    let title = Label::new(None);
    title.add_css_class(cal::EVENT_TITLE);
    title.set_xalign(0.0);
    title.set_ellipsize(EllipsizeMode::End);
    title.set_width_chars(EVENT_LABEL_WIDTH_CHARS);
    title.set_max_width_chars(EVENT_LABEL_WIDTH_CHARS);
    title.set_markup(&format!("<b>{}</b>", markup_escape_text(&event.title)));
    text.append(&title);

    let detail_text = event_detail_text(event, date);
    if !detail_text.is_empty() {
        let detail = Label::new(Some(&detail_text));
        detail.add_css_class(cal::EVENT_DETAIL);
        detail.add_css_class(color::MUTED);
        detail.set_xalign(0.0);
        detail.set_ellipsize(EllipsizeMode::End);
        detail.set_width_chars(EVENT_LABEL_WIDTH_CHARS);
        detail.set_max_width_chars(EVENT_LABEL_WIDTH_CHARS);
        text.append(&detail);
    }

    row.append(&text);
    content.append(&row);
}

fn event_detail_text(event: &CalendarEvent, date: NaiveDate) -> String {
    let time = if event.all_day {
        tr!("All day")
    } else if date == event.start.date_naive() {
        format!("{:02}:{:02}", event.start.hour(), event.start.minute())
    } else if date != event.final_day() {
        tr!("Ongoing")
    } else if event.end.time() == chrono::NaiveTime::MIN {
        tr!("Until midnight")
    } else {
        tr!(
            "Until {time}",
            time = format!("{:02}:{:02}", event.end.hour(), event.end.minute())
        )
    };
    match (&event.calendar_name, &event.location) {
        (calendar, location) if !calendar.is_empty() && !location.is_empty() => {
            format!("{time} · {calendar} · {location}")
        }
        (calendar, _) if !calendar.is_empty() => format!("{time} · {calendar}"),
        (_, location) if !location.is_empty() => format!("{time} · {location}"),
        _ => time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::styles::weather_popover as wp;
    use crate::ui_regression_test_support::{find_descendant_with_class, init_gtk_or_skip};

    #[test]
    fn clock_calendar_popover_tracks_config() {
        if !init_gtk_or_skip("clock calendar popover test", None) {
            return;
        }

        let (with_week_numbers, _refresh, _weather_refresh, _events_refresh) =
            build_clock_calendar_popover(true, false, false);
        assert!(
            find_descendant_with_class(&with_week_numbers, "week-number-header").is_some(),
            "calendar popover should render the week-number header when week numbers are enabled"
        );

        let (without_week_numbers, _refresh, _weather_refresh, _events_refresh) =
            build_clock_calendar_popover(false, false, false);
        assert!(
            find_descendant_with_class(&without_week_numbers, "week-number-header").is_none(),
            "calendar popover should omit the week-number header when week numbers are disabled"
        );

        let (without_weather, _refresh, weather_refresh, events_refresh) =
            build_clock_calendar_popover(false, false, false);
        assert!(weather_refresh.is_none());
        assert!(events_refresh.is_none());
        assert!(find_descendant_with_class(&without_weather, wp::EMPTY).is_none());
        assert!(find_descendant_with_class(&without_weather, cal::EVENTS_ENABLED).is_none());

        let (with_weather, _refresh, weather_refresh, _events_refresh) =
            build_clock_calendar_popover(false, true, false);
        assert!(weather_refresh.is_some());
        assert!(find_descendant_with_class(&with_weather, wp::EMPTY).is_some());

        let (with_events, _refresh, _weather_refresh, events_refresh) =
            build_clock_calendar_popover(false, false, true);
        assert!(find_descendant_with_class(&with_events, cal::EVENTS_ENABLED).is_some());
        assert!(find_descendant_with_class(&with_events, cal::EVENTS_CARD).is_some());
        assert!(events_refresh.is_some());

        assert_today_button_resets_month(false);
        assert_today_button_resets_month(true);
        assert_month_navigation_preserves_selected_day();
        assert_month_property_change_updates_header();

        let state_content = GtkBox::new(Orientation::Vertical, 0);
        append_state_label(
            &state_content,
            "a long error without guaranteed word boundaries",
        );
        let state_label = state_content.first_child().and_downcast::<Label>().unwrap();
        assert!(state_label.wraps());
        assert_eq!(state_label.wrap_mode(), gtk4::pango::WrapMode::WordChar);
    }

    fn assert_today_button_resets_month(events_enabled: bool) {
        let (popover, _refresh, _weather_refresh, _events_refresh) =
            build_clock_calendar_popover(false, false, events_enabled);
        let mut buttons = Vec::new();
        collect_buttons(&popover, &mut buttons);
        assert!(
            buttons.len() >= 3,
            "calendar header should have navigation buttons"
        );

        let calendar = find_descendant_with_class(&popover, cal::WIDGET)
            .and_downcast::<Calendar>()
            .expect("calendar popover should contain GtkCalendar");
        let today = Local::now().date_naive();

        buttons[0].emit_clicked();
        assert_ne!(calendar.month(), today.month0() as i32);
        buttons[1].emit_clicked();
        assert_eq!(calendar.year(), today.year());
        assert_eq!(calendar.month(), today.month0() as i32);
    }

    fn collect_buttons(widget: &Widget, buttons: &mut Vec<gtk4::Button>) {
        if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
            buttons.push(button.clone());
        }

        let mut child = widget.first_child();
        while let Some(current) = child {
            collect_buttons(&current, buttons);
            child = current.next_sibling();
        }
    }

    fn assert_month_navigation_preserves_selected_day() {
        let (popover, _refresh, _weather_refresh, _events_refresh) =
            build_clock_calendar_popover(false, false, true);
        let calendar = find_descendant_with_class(&popover, cal::WIDGET)
            .and_downcast::<Calendar>()
            .expect("calendar popover should contain GtkCalendar");
        let title = find_descendant_with_class(&popover, cal::EVENTS_CARD)
            .and_then(|card| card.first_child())
            .and_downcast::<Label>()
            .expect("event card should contain a title");
        let mut buttons = Vec::new();
        collect_buttons(&popover, &mut buttons);

        calendar.set_day(15);
        buttons[2].emit_clicked();
        assert_eq!(calendar.day(), 15);
        let selected = calendar_selected_date(&calendar).unwrap();
        assert_eq!(title.label(), selected.format("%A, %B %-d").to_string());
    }

    fn assert_month_property_change_updates_header() {
        let (popover, _refresh, _weather_refresh, events_refresh) =
            build_clock_calendar_popover(false, false, false);
        let calendar = find_descendant_with_class(&popover, cal::WIDGET)
            .and_downcast::<Calendar>()
            .expect("calendar popover should contain GtkCalendar");
        let header = find_descendant_with_class(&popover, surface::POPOVER_TITLE)
            .and_downcast::<Label>()
            .expect("calendar popover should contain month header");
        let today = Local::now().date_naive();
        let target = add_months(today, if today.month() == 1 { 1 } else { -1 });

        calendar.set_day(1);
        calendar.set_month(target.month0() as i32);

        assert_eq!(header.label(), target.format("%B %Y").to_string());
        assert!(events_refresh.is_none());
    }

    #[test]
    fn event_marker_color_parses_and_serializes_trusted_rgb() {
        assert_eq!(event_marker_color("red").as_deref(), Some("#ff0000"));
        assert_eq!(event_marker_color("turquoise").as_deref(), Some("#40e0d0"));
        assert_eq!(event_marker_color(" #AABBCC ").as_deref(), Some("#aabbcc"));
        assert_eq!(event_marker_color("#123456\" size=\"999"), None);
        assert_eq!(event_marker_color("notacolor"), None);
    }

    #[test]
    fn event_dot_color_uses_fixed_alpha_after_parsing() {
        let color = parse_event_dot_color("red").expect("CSS color name should parse");
        assert_eq!(color.red(), 1.0);
        assert_eq!(color.green(), 0.0);
        assert_eq!(color.blue(), 0.0);
        assert!((color.alpha() - 0.78).abs() < f32::EPSILON);
    }

    #[test]
    fn colorless_events_use_day_label_foreground_fallback() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let event = test_event("2026-06-01 10:15:00", "2026-06-01 11:15:00", false);
        let snapshot = CalendarSnapshot {
            focus_month: Some(date),
            loading: false,
            error: None,
            backend_available: true,
            events_by_date: std::collections::BTreeMap::from([(
                date,
                vec![std::sync::Arc::new(event)],
            )]),
        };

        let state = CalendarEventDotState::from_snapshot(date, &snapshot);

        assert!(state.dots_by_date.get(&date).is_some_and(Vec::is_empty));
    }

    #[test]
    fn visible_grid_dates_are_contiguous_from_leading_grid_origin() {
        let visible_month = NaiveDate::from_ymd_opt(2026, 2, 15).unwrap();

        for leading_days in 0..=7 {
            let trailing_days = GTK_CALENDAR_DAY_COUNT - leading_days - 28;
            let dates = visible_grid_dates(
                visible_month,
                leading_days,
                trailing_days,
                GTK_CALENDAR_DAY_COUNT,
            )
            .unwrap();

            assert_eq!(dates.len(), GTK_CALENDAR_DAY_COUNT);
            assert_eq!(
                dates[leading_days],
                NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()
            );
            assert!(
                dates
                    .windows(2)
                    .all(|days| days[0].succ_opt() == Some(days[1]))
            );
        }
    }

    #[test]
    fn month_shift_preserves_and_clamps_selected_day() {
        let january_31 = NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();
        let march_31 = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let june_15 = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();

        assert_eq!(
            add_months(january_31, 1),
            NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
        );
        assert_eq!(
            add_months(march_31, -1),
            NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
        );
        assert_eq!(
            add_months(june_15, 1),
            NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
        );
    }

    #[test]
    fn visible_grid_dates_reject_unexpected_calendar_structure() {
        let visible_month = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();

        assert!(visible_grid_dates(visible_month, 8, 6, GTK_CALENDAR_DAY_COUNT).is_none());
        assert!(visible_grid_dates(visible_month, 0, 14, 41).is_none());
        assert!(visible_grid_dates(visible_month, 0, 13, GTK_CALENDAR_DAY_COUNT).is_none());
    }

    #[test]
    fn timed_event_details_follow_selected_continuation_day() {
        let event = test_event("2026-06-01 10:15:00", "2026-06-04 12:30:00", false);

        assert_eq!(
            event_detail_text(&event, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
            "10:15"
        );
        assert_eq!(
            event_detail_text(&event, NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            "Ongoing"
        );
        assert_eq!(
            event_detail_text(&event, NaiveDate::from_ymd_opt(2026, 6, 4).unwrap()),
            "Until 12:30"
        );
    }

    #[test]
    fn timed_event_details_treat_midnight_end_as_previous_final_day() {
        let event = test_event("2026-06-01 10:15:00", "2026-06-03 00:00:00", false);

        assert_eq!(
            event_detail_text(&event, NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            "Until midnight"
        );
    }

    #[test]
    fn all_day_event_details_ignore_selected_day() {
        let event = test_event("2026-06-01 00:00:00", "2026-06-04 00:00:00", true);

        assert_eq!(
            event_detail_text(&event, NaiveDate::from_ymd_opt(2026, 6, 3).unwrap()),
            "All day"
        );
    }

    fn test_event(start: &str, end: &str, all_day: bool) -> CalendarEvent {
        CalendarEvent {
            calendar_name: String::new(),
            color: None,
            title: "Event".to_string(),
            location: String::new(),
            start: chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_local_timezone(Local)
                .single()
                .unwrap(),
            end: chrono::NaiveDateTime::parse_from_str(end, "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_local_timezone(Local)
                .single()
                .unwrap(),
            all_day,
        }
    }
}
