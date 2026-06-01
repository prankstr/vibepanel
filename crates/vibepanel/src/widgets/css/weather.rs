//! Weather popover CSS.

/// Return weather popover CSS.
pub fn css() -> &'static str {
    r#"
/* ===== WEATHER POPOVER ===== */

.popover.weather-popover {
    margin: 0;
}

/* Hero card: big icon + temperature on the left, metrics on the right */
.weather-popover-hero {
    padding: 12px;
    border-radius: var(--radius-card);
    background: var(--color-card-overlay);
}

.weather-popover-hero-icon {
    font-size: 3em;
    color: var(--color-accent-primary);
}

.weather-popover-temp {
    font-size: var(--font-size-lg);
    font-weight: 600;
}

.weather-popover-condition {
    font-size: var(--font-size-md);
}

.weather-popover-detail {
    font-size: var(--font-size-sm);
}

/* Metrics grid (wind / humidity / sunrise, precipitation / UV / sunset) */
.weather-popover-metric-icon {
    font-size: 1.2em;
    color: var(--color-accent-primary);
}

.weather-popover-metric-label {
    font-size: var(--font-size-xs);
}

.weather-popover-metric-value {
    font-size: var(--font-size-sm);
    font-weight: 500;
}

.weather-popover-day {
    padding: 10px 6px;
    border-radius: var(--radius-card);
    background: var(--color-card-overlay);
}

.weather-popover-day-name {
    font-size: var(--font-size-xs);
    font-weight: 500;
}

.weather-popover-day-icon {
    font-size: 1.85em;
    color: var(--color-accent-primary);
}

.weather-popover-day-high {
    font-size: var(--font-size-md);
    font-weight: 700;
}

.weather-popover-day-temps {
    margin-top: -2px;
}

.weather-popover-day-low {
    font-size: var(--font-size-sm);
    font-weight: 400;
    margin-top: -1px;
}

.weather-popover-day-metrics {
    margin-top: 2px;
}

.weather-popover-day-metric-icon {
    font-size: 0.85em;
    min-width: 1.1em;
    opacity: 0.8;
}

.weather-popover-day-metric-value {
    font-size: var(--font-size-xs);
    line-height: 0.9;
}

/* Status banner */
.weather-popover-banner {
    margin-top: 2px;
}

.weather-popover-banner label {
    font-size: var(--font-size-xs);
}

/* Empty / unavailable state */
.weather-popover-empty {
    padding: 16px 8px;
}

.weather-popover-empty-icon {
    font-size: 2.5em;
    opacity: 0.5;
}

.weather-popover-empty-label {
    font-size: var(--font-size-sm);
}
"#
}
