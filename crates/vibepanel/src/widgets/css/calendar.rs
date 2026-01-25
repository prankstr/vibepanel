//! Calendar widget CSS.

/// Return calendar CSS.
pub fn css() -> &'static str {
    r#"
/* ===== CALENDAR ===== */

/* Note: padding comes from apply_surface_styles() in base.rs */
.calendar-popover {
}

calendar.view {
    background: transparent;
    border: none;
    color: var(--color-foreground-primary);
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

calendar.view grid label.day-number {
    margin: 1px 2px;
    min-width: 24px;
    min-height: 24px;
}

.week-number-header {
    font-size: var(--font-size-xs);
    color: var(--color-foreground-muted);
    margin-left: 20px; /* Align with week numbers column */
    margin-top: 16px; /* Align vertically with day headers (M T W...) */
}
"#
}
