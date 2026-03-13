//! Calendar widget CSS.

/// Return calendar CSS.
pub fn css() -> &'static str {
    r#"
/* ===== CALENDAR ===== */

/* Note: padding comes from apply_surface_styles() in base.rs */
.calendar-popover .vp-popover-icon-btn {
    margin-top: 0;
}

/* Pull last nav button flush with popover edge */
.calendar-popover .vp-popover-icon-btn:last-child {
    margin-right: -8px;
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

calendar.view grid label.day-number:focus {
    outline: none;
    border: none;
    box-shadow: none;
}

calendar.view grid *:selected:not(.today) {
    background: transparent;
    color: inherit;
    box-shadow: none;
}

.week-number-header {
    font-size: var(--font-size-xs);
    color: var(--color-foreground-muted);
    margin-left: 12px; /* Align with week numbers column */
    margin-top: 16px; /* Align vertically with day headers (M T W...) */
}
"#
}
