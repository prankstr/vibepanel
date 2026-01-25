//! Button CSS classes.

/// Return button CSS.
pub fn css() -> &'static str {
    r#"
/* ===== BUTTONS ===== */

/* Reset button - strips GTK chrome (background, border, shadow) */
button.vp-btn-reset,
button.vp-btn-compact {
    background: transparent;
    border: none;
    box-shadow: none;
    outline: none;
}

/* Compact button - reset + zero padding/margin for icon-only buttons */
button.vp-btn-compact {
    padding: 0;
    margin: 0;
    min-width: 0;
    min-height: 0;
}

button.vp-btn-reset:focus,
button.vp-btn-reset:focus-visible,
button.vp-btn-compact:focus,
button.vp-btn-compact:focus-visible {
    outline: none;
    border: none;
    box-shadow: none;
}

button.vp-btn-accent {
    background: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
}

button.vp-btn-accent:hover {
    opacity: 0.85;
}

button.vp-btn-card {
    background: var(--color-card-overlay);
    color: var(--color-foreground-primary);
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
}

button.vp-btn-card:hover {
    background: var(--color-card-overlay-hover);
}

/* Link-style button - text only, no background */
button.vp-btn-link,
.vp-btn-link {
    background: transparent;
    border: none;
    box-shadow: none;
    color: var(--color-accent-primary);
    padding: 0;
    min-height: 0;
}

button.vp-btn-link:hover,
.vp-btn-link:hover {
    background: transparent;
    text-decoration: underline;
}

/* Ghost button - transparent with hover effect */
button.vp-btn-ghost {
    background: transparent;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
}

button.vp-btn-ghost:hover {
    background: var(--color-card-overlay-hover);
}
"#
}
