//! Shared utility CSS classes.
//!
//! These are truly shared styles that apply across multiple surfaces
//! (bar, popovers, quick settings, etc).

use super::POPOVER_BG_WITH_OPACITY;

/// Return shared utility CSS.
pub fn css(animations: bool) -> String {
    let popover_bg = POPOVER_BG_WITH_OPACITY;
    // Hover background-color transitions are disabled unconditionally.
    // CSS transitions on widgets with nested child widgets (e.g. Box > Label,
    // Box > Image) are observed to cause unbounded memory growth in GTK4.
    // Background-color changes still apply instantly on hover.
    // Possibly related: https://gitlab.gnome.org/GNOME/gtk/-/issues/7758
    let hover_transition = "transition: none;";
    let slider_transition = if animations {
        "transition: transform 100ms ease-out;"
    } else {
        "transition: none;"
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
    background-color: {popover_bg};
    border-radius: var(--radius-surface);
    box-shadow: var(--shadow-soft);
    padding: 16px;
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

popover.widget-menu,
box.popover-wrapper,
box.widget-menu-wrapper {{
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
/* When GTK's 3 s focus_visible timeout fires, the focused widget keeps :focus
   but loses :focus-visible.  Suppress Adwaita's residual :focus outline so no
   faint ring lingers after keyboard nav times out. */
.popover *:focus:not(:focus-visible),
.vp-surface-popover *:focus:not(:focus-visible) {{
    outline: none;
    box-shadow: none;
}}

/* Hide focus outlines in popovers - keyboard nav not primary interaction */
.vp-no-focus *:focus,
.vp-no-focus *:focus-visible,
.vp-no-focus *:focus-within {{
    outline: none;
    box-shadow: none;
}}
/* Also suppress card-level focus ring under .vp-no-focus */
.vp-no-focus .vp-card.vp-toggle-focused {{
    box-shadow: none;
}}

/* But preserve focus on text entries for usability */
.vp-no-focus entry:focus,
.vp-no-focus entry:focus-visible {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
}}

/* Suppress the toggle button's own :focus-visible outline inside a card —
   the card-level box-shadow (.vp-toggle-focused) already provides the ring.
   Only suppress .vp-btn-reset (the toggle), not the expander chevron. */
.vp-card > .vp-btn-reset:focus-visible {{
    outline: none;
}}

/* ===== KEYBOARD NAV FOCUS COLOR ===== */
/* Card-level focus ring: wraps the entire card (toggle + chevron) when
   the toggle button has focus. Uses box-shadow on the parent because a
   native outline on the toggle would only wrap the toggle itself.
   Toggled from Rust via the has-focus signal on the toggle button. */
.vp-card.vp-toggle-focused {{
    box-shadow: inset 0 0 0 2px var(--color-accent-primary);
    transition: none;
}}

/* Override Adwaita's built-in :focus-visible outline with the user's accent
   color.  Rules must be self-contained (full outline shorthand) because
   transition:none at our priority (USER=800) blocks Adwaita's
   outline-width animation from 0→2px at THEME=200.
   Scoped under .vp-surface-popover for specificity. */
.vp-surface-popover button:focus-visible,
.vp-surface-popover row:focus-visible,
.vp-surface-popover switch:focus-visible,
.vp-surface-popover entry:focus-visible {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    transition: none;
}}
.vp-surface-popover scale:focus-visible > trough > slider {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    transition: none;
}}
/* Suppress Adwaita's outline transition on entries so the accent color
   doesn't flash blue when focus leaves. */
.vp-surface-popover entry {{
    transition: none;
}}

/* Rows with inline action buttons delegate focus to the button.  GTK still
   sets :focus-visible on the row even when non-focusable, so suppress it.
   Must come after focus color rules and use .vp-surface-popover scope to
   beat the accent color rule's specificity. */
.vp-surface-popover row.vp-row-has-action,
.vp-surface-popover row.vp-row-has-action:focus,
.vp-surface-popover row.vp-row-has-action:focus-visible {{
    outline: none;
    box-shadow: none;
    transition: none;
}}

/* Suppress :focus-within outlines on rows whose children handle their own
   focus rings (e.g. password entry row).  The child widget (entry, button)
   already shows the accent-colored ring. */
.vp-surface-popover row:focus-within {{
    outline: none;
    box-shadow: none;
}}

/* Power action rows are directly focusable (hold-to-confirm on the row
   itself).  Restore the accent focus ring that :focus-within above kills. */
.vp-surface-popover row.qs-power-row:focus-visible {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    transition: none;
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

/* Inherit border-radius so the ripple clips to the rounded shape */
.widget > overlay,
.widget-item overlay {{
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

.widget {{
    {hover_transition}
}}

.widget-item {{
    {hover_transition}
}}
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
"#
    )
}
