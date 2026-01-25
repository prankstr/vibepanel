//! System popover CSS.

/// Return system popover CSS.
pub fn css() -> &'static str {
    r#"
/* ===== SYSTEM POPOVER ===== */

.system-popover {
    padding: 16px;
}

/* Section cards */
.system-section-card {
    padding: 12px;
    border-radius: var(--radius-card);
}

/* Disable hover on non-interactive section cards */
.system-section-card:hover {
    background: var(--color-card-overlay);
}

/* Section title with icon */
.system-section-title {
    margin-bottom: 4px;
}

.system-section-icon {
    font-size: 1.1em;
    opacity: 0.9;
}

/* System progress bars - accent color fill */
.system-progress-bar trough,
.system-core-bar trough {
    background-color: var(--color-slider-track);
    border-radius: var(--radius-track);
    min-height: var(--slider-height);
}

.system-progress-bar trough progress,
.system-core-bar trough progress {
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border-radius: var(--radius-track);
    min-height: var(--slider-height);
}

.system-expander-content {
    padding: 12px;
    margin-top: 4px;
    background: var(--color-card-overlay);
    border-radius: var(--radius-card);
}

.system-expander-header {
    margin-top: 4px;
}

.system-expander-header:hover {
    background: var(--color-card-overlay-hover);
    border-radius: var(--radius-card);
}

.system-network-icon {
    font-size: 0.9em;
}
"#
}
