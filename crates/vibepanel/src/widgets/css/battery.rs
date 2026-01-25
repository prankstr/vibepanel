//! Battery widget CSS.

/// Return battery CSS.
pub fn css() -> &'static str {
    r#"
/* ===== BATTERY ===== */

/* Battery state classes - applied directly to the backend widget */
.battery-icon.battery-charging {
    color: var(--color-accent-primary);
}

.battery-icon.battery-low {
    color: var(--color-state-urgent);
}

/* Battery popover */
.battery-popover-percent {
    font-size: var(--font-size-lg);
    font-weight: 700;
}

.battery-popover-state {
    font-weight: 500;
}

.battery-popover-time,
.battery-popover-power {
    font-size: var(--font-size-sm);
}

.battery-popover-profile-button {
    font-size: var(--font-size-sm);
    border-radius: var(--radius-widget);
}

.battery-popover-profile-button:hover {
    background: var(--color-card-overlay-hover);
}
"#
}
