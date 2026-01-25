//! Shared utility CSS classes.
//!
//! These are truly shared styles that apply across multiple surfaces
//! (bar, popovers, quick settings, etc).

/// Return shared utility CSS.
pub fn css() -> &'static str {
    r#"
/* ===== SHARED UTILITY CSS ===== */

/* 
 * Icon sizing strategy:
 * - .material-symbol uses font-size: inherit (set in icons.rs)
 * - .icon-root gets the default icon size
 * - Specific components can override with their own font-size on .icon-root or parents
 * - This allows users to style icons by setting font-size on parent elements
 */

/* Default icon size - applied to icon root containers */
.icon-root {
    font-size: var(--icon-size);
}

/* ===== NATIVE GTK TOOLTIPS ===== */
/* Style GTK's native tooltips (used in popovers/windows where layer-shell tooltips don't work) */
tooltip,
tooltip.background {
    background-color: color-mix(in srgb, var(--color-background-widget) 90%, transparent);
    border-radius: var(--radius-surface);
    border: none;
    padding: 0;
}

tooltip > box,
tooltip.background > box {
    padding: 6px 10px;
}

tooltip label,
tooltip.background label {
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}

/* Color utilities - applies to both text and icons */
.vp-primary { color: var(--color-foreground-primary); }
.vp-muted { color: var(--color-foreground-muted); }
.vp-accent { color: var(--color-accent-primary); }
.vp-error { color: var(--color-state-urgent); }

/* Standard Link Styling */
label link {
    color: var(--color-accent-primary);
    text-decoration: none;
}
label link:hover {
    text-decoration: underline;
    color: var(--color-accent-primary);
    opacity: 0.8;
}
label link:active {
    opacity: 0.6;
}

/* Popover header icon button - minimal styling for icon-only buttons in headers */
.vp-popover-icon-btn {
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 28px;
    min-height: 28px;
    padding: 4px;
    margin-top: -8px;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
    font-size: calc(var(--icon-size) * 1.15);
}

.vp-popover-icon-btn:hover {
    background: var(--color-card-overlay-hover);
}

.vp-popover-icon-btn:active {
    opacity: 0.7;
}

/* Popover title - consistent styling for popover headers */
.vp-popover-title {
    font-size: var(--font-size-lg);
}

/* Popover/surface background */
.vp-surface-popover {
    background-color: var(--color-background-widget);
    border-radius: var(--radius-surface);
    box-shadow: var(--shadow-soft);
}

popover.widget-menu {
    background: transparent;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-surface);
}

popover.widget-menu > contents,
popover.widget-menu.background > contents {
    background: transparent;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-surface);
    padding: 0;
    margin: 0;
}

/* ===== FOCUS SUPPRESSION ===== */
/* Hide focus outlines in popovers - keyboard nav not primary interaction */
.vp-no-focus *:focus,
.vp-no-focus *:focus-visible,
.vp-no-focus *:focus-within {
    outline: none;
    box-shadow: none;
}

/* But preserve focus on text entries for usability */
.vp-no-focus entry:focus,
.vp-no-focus entry:focus-visible {
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
}

/* ===== COMPONENT CLASSES ===== */
/* Reusable component patterns for cards, rows, sliders */

/* Slider row - horizontal layout with icon + slider + optional trailing widget */
.slider-row {
    padding: 4px 8px;
}

/* Icon button in slider row (A) */
.slider-row .slider-icon-btn {
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
    transition: background 150ms ease-out;
    font-size: calc(var(--icon-size) * 1.15);
}
.slider-row .slider-icon-btn:hover {
    background: var(--color-card-overlay-hover);
}

/* Slider styling with accent color */
.slider-row scale {
    margin-left: 4px;
    margin-right: 4px;
}

.slider-row scale trough {
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
    background-color: var(--color-slider-track);
}

.slider-row scale highlight {
    background-image: image(var(--color-accent-slider, var(--color-accent-primary)));
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border: none;
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
}

.slider-row scale slider {
    min-width: 16px;
    min-height: 16px;
    margin: -5px;
    padding: 0;
    background-color: var(--color-accent-primary);
    border-radius: var(--radius-pill);
    border: none;
    box-shadow: none;
    transition: transform 100ms ease-out;
}
.slider-row scale slider:active {
    transform: scale(1.15);
}

/* Muted state for slider row icons */
.slider-row .muted {
    color: var(--color-foreground-muted);
}

/* Trailing spacer in slider row - invisible, matches expander size */
.slider-row .slider-spacer {
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 24px;
    padding: 4px;
    opacity: 0;
}

/* Slider row expander (B) */
.slider-row .qs-toggle-more {
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
}
.slider-row .qs-toggle-more:hover {
    background: var(--color-card-overlay-hover);
}
"#
}
