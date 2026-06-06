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

/* Vertical spacing adjustment for GTK icons when the bar is in vertical mode */
.bar.bar--vertical .icon-root > .icon:not(.material-symbol) {{
    margin-top: 0.15em;
    margin-bottom: 0.15em;
}}

/* Material Symbols reserve the same amount of space no matter their shape, so narrow
 * glyphs like battery leave trailing whitespace before the next label.
 * These offsets are small optical corrections around the reserved glyph box. */

.bar.bar--horizontal .quick-settings .material-symbol-vpn_key {{
  margin-left: 0.2em;
  margin-right: 0.15em;
}}
.bar.bar--vertical .quick-settings .material-symbol-vpn_key {{
    margin-top: -0.3em;
    transform: translateY(0.125em);
}}

.bar.bar--horizontal .quick-settings .material-symbol-bluetooth_disabled,
.bar.bar--horizontal .quick-settings .material-symbol-bluetooth {{
  margin-left: -0.15em;
}}

.bar.bar--horizontal .material-symbol-memory {{
    margin-right: -0.1em;
    margin-left: -0.1em;
}}

.bar.bar--horizontal .material-symbol-download {{
    margin-right: -0.15em;
    margin-left: -0.15em;
    transform: translateY(0.05em);
}}

.bar.bar--horizontal .material-symbol-battery_full,
.bar.bar--horizontal .material-symbol-battery_6_bar,
.bar.bar--horizontal .material-symbol-battery_5_bar,
.bar.bar--horizontal .material-symbol-battery_4_bar,
.bar.bar--horizontal .material-symbol-battery_3_bar,
.bar.bar--horizontal .material-symbol-battery_2_bar,
.bar.bar--horizontal .material-symbol-battery_1_bar,
.bar.bar--horizontal .material-symbol-battery_unknown,
.bar.bar--horizontal .material-symbol-battery_charging_full,
.bar.bar--horizontal .material-symbol-battery_charging_90,
.bar.bar--horizontal .material-symbol-battery_charging_80,
.bar.bar--horizontal .material-symbol-battery_charging_60,
.bar.bar--horizontal .material-symbol-battery_charging_50,
.bar.bar--horizontal .material-symbol-battery_charging_30,
.bar.bar--horizontal .material-symbol-battery_charging_20,
.bar.bar--horizontal .material-symbol-notifications,
.bar.bar--horizontal .material-symbol-keyboard_arrow_up,
.bar.bar--horizontal .material-symbol-keyboard_arrow_down {{
    margin-right: -0.2em;
    margin-left: -0.2em;
}}

/* Digits/caps don't use descender space, so they sit visually high in the em
 * box — nudge down. Transform keeps layout and baseline unchanged. */
.bar.bar--horizontal .vcenter-caps {{
    transform: translateY(0.05em);
}}

/* Vertical bars are too narrow for 4-char speeds (e.g. "9.9M") at the default
 * font size — shrink the labels and drop horizontal padding to claw back room. */
.bar.bar--vertical .network-speed-dl-label,
.bar.bar--vertical .network-speed-ul-label {{
    font-size: 0.85em;
}}
.bar.bar--vertical .widget.network-speed:not(.widget-group) .content {{
    padding-left: 0;
    padding-right: 0;
    /* Extra flow padding on the bottom so the UL row doesn't crowd the edge. */
    padding-bottom: calc(var(--vp-widget-padding) + 1px);
}}

/* Wider gap between the DL and UL groups in vertical mode. */
.bar.bar--vertical .widget.network-speed:not(.widget-group) > overlay > .content > *:not(:last-child),
.bar.bar--vertical .widget.network-speed:not(.widget-group) > .content > *:not(:last-child) {{
    margin-bottom: calc(var(--vp-widget-gap) + 2px);
}}

/* Tighter icon ↔ DL gap so it visually matches the within-group arrow ↔ label
 * spacing instead of the wider DL ↔ UL gap. */
.bar.bar--vertical .widget.network-speed:not(.widget-group) > overlay > .content > .icon-root,
.bar.bar--vertical .widget.network-speed:not(.widget-group) > .content > .icon-root {{
    margin-bottom: 2px;
}}

/* Tighten the multi-line vertical clock so the dot separator doesn't add a
 * full font line of vertical air between HH and MM. */
.bar.bar--vertical .clock-label {{
    line-height: 1.0;
}}

/* Extra flow-axis breathing room around the vertical clock. Asymmetric
 * padding pushes the block down to compensate for digits centering on the
 * em box (which includes descender slack the digits never use), making the
 * block visually sit too high otherwise. */
.bar.bar--vertical .widget.clock:not(.widget-group) .content {{
    padding-top: calc(var(--vp-widget-padding) + 3px);
    padding-bottom: calc(var(--vp-widget-padding) + 1px);
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

/* Layer-shell tooltips use custom windows so they share the same styling as native tooltips. */
.vibepanel-tooltip {{
    background-color: color-mix(in srgb, {popover_bg} 90%, var(--widget-hover-tint));
    border-radius: var(--radius-surface);
    border: none;
    padding: 6px 10px;
    opacity: 0.90;
}}

.vibepanel-tooltip-label {{
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
/* color-mix() uses CSS custom properties so per-widget `.popover` descendants can override
   --widget-background-color and have the mixed value recomputed via CSS scoping */
/* NOTE: `.popover` is an explicit style class added by Vibepanel (LayerShellPopover etc.).
   GTK's native Popover widget is the `popover` CSS *node* (no leading dot) — no collision. */
.popover {{
    background-color: {popover_bg};
    background-image: none;
    background-clip: padding-box;
    border: var(--surface-outline-width) solid color-mix(in srgb, var(--surface-outline-color) var(--surface-outline-opacity), transparent);
    border-radius: var(--radius-surface);
    box-shadow: var(--shadow-soft);
    padding: 16px;
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

.notification-toast,
window.media-window > .media-content,
.osd {{
    background-color: {popover_bg};
    background-image: none;
    background-clip: padding-box;
    border: var(--surface-outline-width) solid color-mix(in srgb, var(--surface-outline-color) var(--surface-outline-opacity), transparent);
    border-radius: var(--radius-surface);
    box-shadow: var(--shadow-soft);
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

window.media-window > .media-content {{
    padding: 16px;
}}

.osd {{
    border-radius: var(--radius-widget-lg);
}}

/* Mirrors the generated surface rule for static popover styles. */
.popover.vp-suppress-css-outline {{
    border-color: transparent;
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

/* Inner panel inside a native widget-menu popover already gets the shadow
   from the contents node above; suppress the duplicate. */
popover.widget-menu .popover.widget-menu-content {{
    box-shadow: none;
}}

/* ===== FOCUS SUPPRESSION ===== */
/* When GTK's 3 s focus_visible timeout fires, the focused widget keeps :focus
   but loses :focus-visible.  Suppress Adwaita's residual :focus outline so no
   faint ring lingers after keyboard nav times out. */
.popover *:focus:not(:focus-visible) {{
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
   Scoped under .popover for specificity. */
.popover button:focus-visible,
.popover row:focus-visible,
.popover switch:focus-visible,
.popover entry:focus-visible {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    transition: none;
}}
.popover scale:focus-visible > trough > slider {{
    outline: 2px solid var(--color-accent-primary);
    outline-offset: -2px;
    transition: none;
}}
/* Suppress Adwaita's outline transition on entries so the accent color
   doesn't flash blue when focus leaves. */
.popover entry {{
    transition: none;
}}

/* Rows with inline action buttons delegate focus to the button.  GTK still
   sets :focus-visible on the row even when non-focusable, so suppress it.
   Must come after focus color rules and use .popover scope to
   beat the accent color rule's specificity. */
.popover row.vp-row-has-action,
.popover row.vp-row-has-action:focus,
.popover row.vp-row-has-action:focus-visible {{
    outline: none;
    box-shadow: none;
    transition: none;
}}

/* Suppress :focus-within outlines on rows whose children handle their own
   focus rings (e.g. password entry row).  The child widget (entry, button)
   already shows the accent-colored ring. */
.popover row:focus-within {{
    outline: none;
    box-shadow: none;
}}

/* Power action rows are directly focusable (hold-to-confirm on the row
   itself).  Restore the accent focus ring that :focus-within above kills. */
.popover row.qs-power-row:focus-visible {{
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
