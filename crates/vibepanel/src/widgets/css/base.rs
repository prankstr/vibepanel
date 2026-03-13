//! Shared utility CSS classes.
//!
//! These are truly shared styles that apply across multiple surfaces
//! (bar, popovers, quick settings, etc).

use super::{POPOVER_ANIMATION_MS, WIDGET_BG_WITH_OPACITY};

/// Return shared utility CSS.
pub fn css(animations: bool) -> String {
    let widget_bg = WIDGET_BG_WITH_OPACITY;
    let hover_transition = if animations {
        "transition: background-color 100ms ease;"
    } else {
        "transition: none;"
    };
    let slider_transition = if animations {
        "transition: transform 100ms ease-out;"
    } else {
        "transition: none;"
    };
    let popover_transition = if animations {
        format!(
            "transition: opacity {ms}ms cubic-bezier(0.2, 0, 0, 1),\n                transform {ms}ms cubic-bezier(0.2, 0, 0, 1);",
            ms = POPOVER_ANIMATION_MS,
        )
    } else {
        "transition: none;".to_string()
    };
    format!(
        r#"
/* ===== SHARED UTILITY CSS ===== */

/* Layer-shell popover window - transparent so content can have proper shadow */
window.layer-shell-popover {{
    background: transparent;
}}

/* Layer-shell click catcher - transparent overlay */
window.layer-shell-click-catcher {{
    background: transparent;
}}

/* 
 * Icon sizing strategy:
 * - .material-symbol uses font-size: inherit (set in icons.rs)
 * - .icon-root gets the default icon size
 * - Specific components can override with their own font-size on .icon-root or parents
 * - This allows users to style icons by setting font-size on parent elements
 */

/* Default icon size - applied to icon root containers */
.icon-root {{
    font-size: var(--icon-size);
}}

/* ===== NATIVE GTK TOOLTIPS ===== */
/* Style GTK's native tooltips (used in popovers/windows where layer-shell tooltips don't work) */
tooltip,
tooltip.background {{
    background-color: color-mix(in srgb, color-mix(in srgb, var(--widget-background-color) var(--widget-background-opacity), transparent) 90%, var(--widget-hover-tint));
    border-radius: var(--radius-surface);
    border: none;
    padding: 0;
    opacity: 0.90;
}}

tooltip > box,
tooltip.background > box {{
    padding: 6px 10px;
}}

tooltip label,
tooltip.background label {{
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

/* Color utilities - applies to both text and icons */
.vp-primary {{ color: var(--color-foreground-primary); }}
.vp-muted {{ color: var(--color-foreground-muted); }}
.vp-disabled {{ color: var(--color-foreground-disabled); }}
.vp-faint {{ color: var(--color-foreground-faint); }}
.vp-accent {{ color: var(--color-accent-primary); }}
.vp-error {{ color: var(--color-state-urgent); }}

/* Service unavailable state - disabled/gray to indicate unavailable service */
.service-unavailable {{
    color: var(--color-foreground-disabled);
}}

/* Standard Link Styling */
label link {{
    color: var(--color-accent-primary);
    text-decoration: none;
}}
label link:hover {{
    text-decoration: underline;
    color: var(--color-accent-primary);
    opacity: 0.8;
}}
label link:active {{
    opacity: 0.6;
}}

/* Popover header icon button - minimal styling for icon-only buttons in headers */
.vp-popover-icon-btn {{
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
    -gtk-icon-size: calc(var(--icon-size) * 0.85);
}}

.vp-popover-icon-btn:hover {{
    background: var(--color-card-overlay-hover);
}}

/* Popover title - consistent styling for popover headers */
.vp-popover-title {{
    font-size: var(--font-size-lg);
}}

/* Popover/surface background */
/* color-mix() is inline here so per-widget popover --widget-background-color overrides work via CSS scoping */
.vp-surface-popover {{
    background-color: {widget_bg};
    border-radius: var(--radius-surface);
    box-shadow: var(--shadow-soft);
    padding: 16px;
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

popover.widget-menu {{
    background: transparent;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-surface);
}}

popover.widget-menu > contents,
popover.widget-menu.background > contents {{
    background: transparent;
    border: none;
    box-shadow: var(--shadow-soft);
    border-radius: var(--radius-surface);
    padding: 0;
    margin: 0 6px 6px 6px;
}}

/* ===== FOCUS SUPPRESSION ===== */
/* Hide focus outlines in popovers - keyboard nav not primary interaction */
.vp-no-focus *:focus,
.vp-no-focus *:focus-visible,
.vp-no-focus *:focus-within {{
    outline: none;
    box-shadow: none;
}}

/* But preserve focus on text entries for usability */
.vp-no-focus entry:focus,
.vp-no-focus entry:focus-visible {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
}}

/* ===== COMPONENT CLASSES ===== */
/* Reusable component patterns for cards, rows, sliders */

/* ===== RIPPLE ANIMATION ===== */
/* Press feedback is rendered by Cairo via DrawingArea (click-origin
   expanding circle). The .vp-ripple-overlay class is kept for the DrawingArea
   element that sits in the gtk4::Overlay. */

/* Ripple overlay — transparent background so DrawingArea is invisible when idle */
.vp-ripple-overlay {{
    background: transparent;
}}

/* The Overlay wrapper inside .widget needs to inherit border-radius
   so overflow:hidden clips the ripple to the widget's rounded shape */
.widget overlay {{
    border-radius: inherit;
}}

/* Ripple wrapper overlay — fallback border-radius for standalone use
   (e.g. toggle cards where the overlay wraps the card) */
overlay.vp-ripple-wrap {{
    border-radius: var(--radius-widget);
}}

/* When inside a button or row, inherit the parent's actual radius
   (may differ from --radius-widget, e.g. --radius-pill on compact buttons) */
button.vp-has-ripple > overlay,
row.vp-has-ripple > overlay {{
    border-radius: inherit;
}}

/* ===== HOVER TRANSITIONS ===== */
/* GTK4 needs transition on BOTH base and :hover for bidirectional animation. */
button {{
    {hover_transition}
}}
button:hover {{
    {hover_transition}
}}

.widget,
.widget-item {{
    {hover_transition}
}}
.widget:hover,
.widget-item:hover {{
    {hover_transition}
}}

/* Slider row - horizontal layout with icon + slider + optional trailing widget */
.slider-row {{
    padding: 4px 8px;
}}

/* Icon button in slider row (A) */
.slider-row .slider-icon-btn {{
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
    font-size: calc(var(--icon-size) * 1.15);
}}
.slider-row .slider-icon-btn:hover {{
    background: var(--color-card-overlay-hover);
}}

/* Slider styling with accent color */
.slider-row scale {{
    margin-left: 4px;
    margin-right: 4px;
}}

.slider-row scale trough {{
    min-height: var(--slider-height);
    border-radius: var(--slider-radius);
    background-color: var(--color-slider-track);
}}

.slider-row scale highlight {{
    background-image: image(var(--color-accent-slider, var(--color-accent-primary)));
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border: none;
    min-height: var(--slider-height);
    border-radius: var(--slider-radius);
}}

.slider-row scale slider {{
    min-width: var(--slider-knob-size);
    min-height: var(--slider-knob-size);
    margin: -5px;
    padding: 0;
    background-color: var(--color-accent-primary);
    border-radius: var(--slider-knob-radius);
    border: none;
    box-shadow: none;
    {slider_transition}
}}
.slider-row scale slider:active {{
    transform: scale(1.15);
}}

/* Muted state for slider row icons */
.slider-row .muted {{
    color: var(--color-foreground-muted);
}}

/* Trailing spacer in slider row - invisible, matches expander size */
.slider-row .slider-spacer {{
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 24px;
    padding: 4px;
    opacity: 0;
}}

/* Slider row expander (B) */
.slider-row .qs-toggle-more {{
    min-width: calc(var(--icon-size) * 2);
    min-height: calc(var(--icon-size) * 2);
    padding: 0;
    border-radius: var(--radius-widget);
}}
.slider-row .qs-toggle-more:hover {{
    background: var(--color-card-overlay-hover);
}}

/* ===== POPOVER OPEN/CLOSE ANIMATION ===== */

.popover-animate {{
    {popover_transition}
    opacity: 1;
    transform: scale(1);
}}

.popover-animate.popover-hidden {{
    opacity: 0;
    transform: scale(0.95);
}}
"#
    )
}
