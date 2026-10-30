//! Calendar widget CSS.

/// Return calendar CSS.
pub fn css() -> &'static str {
    r#"
/* ===== CALENDAR ===== */

/* Popover padding comes from the shared .popover rule. */
.calendar-popover-card {
    padding: 12px;
    border-radius: var(--radius-card);
    background: var(--color-card-overlay);
}

.calendar-popover .vp-popover-icon-btn {
    margin-top: 0;
}

/* Pull last nav button flush with popover edge */
.calendar-popover .vp-popover-icon-btn:last-child {
    margin-right: -8px;
}

.calendar-events-layout {
    min-width: 532px;
}

calendar.view {
    background: transparent;
    border: none;
    color: var(--color-foreground-primary);
    margin-left: -10px;
    margin-right: -4px;
}

calendar.view grid {
    background: transparent;
}

calendar.view grid label.week-number {
    font-size: var(--font-size-xs);
    color: var(--color-foreground-muted);
}

calendar.view grid label.today {
    background: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
    border-radius: var(--radius-widget);
    box-shadow: none;
}

calendar.view grid label.day-number {
    margin: 0 6px;
    min-width: calc(var(--font-size) * 1.75);
    min-height: calc(var(--font-size) * 1.75);
    padding: 4px;
    font-weight: 325;
}

calendar.view:not(.calendar-events-enabled) grid *:selected:not(.today) {
    background: transparent;
    color: inherit;
    box-shadow: none;
}

calendar.view.calendar-events-enabled grid label.day-number:checked:not(.today):not(:selected) {
    background: transparent;
    color: var(--color-accent-primary);
    font-weight: 600;
    box-shadow: none;
}

calendar.view.calendar-events-enabled grid label.day-number:hover:not(.today):not(:selected) {
    background: var(--color-card-overlay-hover);
    border-radius: var(--radius-widget);
}

calendar.view.calendar-events-enabled grid label.day-number:selected:not(.today) {
    background: var(--color-card-overlay-hover);
    color: var(--color-foreground-primary);
    border-radius: var(--radius-widget);
    box-shadow: none;
}

/* Arrows move the selection, so the ring marks the selected day while the
   calendar has visible focus. */
.popover calendar.view:focus-visible grid label.day-number:selected {
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    border-radius: var(--radius-widget);
}
/* Today is accent-filled. */
.popover calendar.view:focus-visible grid label.day-number.today:selected {
    outline-color: var(--color-accent-text, #fff);
}

/* Below .vp-no-focus's specificity, so hidden until keyboard navigation. */
.calendar-events-card > scrolledwindow:focus-visible {
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    border-radius: var(--radius-widget);
    transition: none;
}

.week-number-header {
    font-size: var(--font-size-xs);
    color: var(--color-foreground-muted);
    margin-left: 12px; /* Align with week numbers column */
    margin-top: 16px; /* Align vertically with day headers (M T W...) */
}

.calendar-events-card {
    padding: 12px;
    border-radius: var(--radius-card);
    background: var(--color-card-overlay);
    min-width: 260px;
}

.calendar-events-list {
    min-width: 236px;
}

.calendar-events-state {
    font-size: var(--font-size-sm);
}

.calendar-event-row {
    padding: 4px 0;
}

.calendar-event-marker {
    color: var(--color-accent-primary);
    opacity: 0.72;
    font-size: var(--font-size-lg);
    line-height: 1;
}

.calendar-event-title {
    font-size: var(--font-size-sm);
    color: var(--color-foreground-primary);
}

.calendar-event-detail {
    font-size: var(--font-size-xs);
}
"#
}
