//! Bar and workspace CSS.
//!
//! Note: This module requires config values for screen_margin and spacing,
//! so it returns a formatted String rather than a static str.

use super::{WIDGET_BG_HOVER, WIDGET_BG_WITH_OPACITY};
use crate::widgets::workspaces::{
    INDICATOR_ACTIVE_MULT, INDICATOR_HEIGHT_MULT, INDICATOR_INACTIVE_MULT, LONG_INDICATOR_HPAD,
};

/// Return bar CSS with config values interpolated.
///
/// `workspace_animations` is the resolved per-widget animation flag: it
/// equals the explicit `[widgets.workspaces] animate` value when set,
/// otherwise falls back to the global `theme.animations` setting.  This
/// lets `workspaces.animate = true` keep workspace indicator CSS transitions
/// alive even when `theme.animations = false`.
pub fn css(screen_margin: u32, spacing: u32, workspace_animations: bool) -> String {
    let widget_bg = WIDGET_BG_WITH_OPACITY;
    let widget_bg_hover = WIDGET_BG_HOVER;
    let inactive_mult = INDICATOR_INACTIVE_MULT;
    let active_mult = INDICATOR_ACTIVE_MULT;
    let height_mult = INDICATOR_HEIGHT_MULT;
    let long_hpad = LONG_INDICATOR_HPAD;
    let workspace_transition = if workspace_animations {
        "transition: min-width 200ms linear, background-color 100ms ease;"
    } else {
        "transition: none;"
    };
    format!(
        r#"
/* ===== BAR ===== */

/* Window must be transparent so bar background shows */
.bar-window {{
    background: transparent;
}}

/* Shell containers transparent */
.bar-shell,
.bar-shell-inner,
.bar-margin-spacer {{
    background: transparent;
}}

.bar-shell-inner {{
    padding-left: {screen_margin}px;
    padding-right: {screen_margin}px;
}}

/* Bar container - the visible bar */
sectioned-bar.bar {{
    min-height: var(--bar-height);
    padding-top: var(--bar-padding-y);
    padding-bottom: var(--bar-padding-y-bottom);
    background: var(--color-background-bar);
    border-radius: var(--radius-bar);
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

/* Widget wrapper — transparent so only .widget paints a background layer */
.widget-wrapper {{
    min-height: var(--widget-height);
    background: transparent;
}}

/* Widget — visual surface */
.widget {{
    background-color: {widget_bg};
    border-radius: var(--radius-widget);
}}

/* Padding on .content (not the container) so the ripple overlay
   fills the entire widget background area edge-to-edge.
   Passive widgets (merge groups) have no .widget surface — target
   their .content directly via the third selector. */
.widget:not(.widget-group) .content,
.widget-group > .content > .widget-item .content,
.widget-item.passive > .content {{
    padding: var(--widget-padding-y) 10px;
}}

/* Widget groups - remove padding so hover can extend to edges */
.widget.widget-group {{
    padding: 0;
}}

/* Hover targets the wrapper but paints on the surface child */
.widget-wrapper.clickable:hover > .widget:not(.widget-group) {{
    background-color: {widget_bg_hover};
}}

/* Pull non-first items left to overlap adjacent .content padding (2 × 10px).
   Merge groups (.widget-merge-group) are also direct children of .content,
   so they need the same treatment when they follow another item. */
.widget-group .content > .widget-item:not(:first-child),
.widget-group .content > .widget-merge-group:not(:first-child) {{
    margin-left: -20px;
}}

/* Base border-radius for grouped items — must be present in the non-hover
   state so the radius doesn't snap on/off during the background transition */
.widget-group .content > .widget-item {{
    border-radius: var(--radius-widget);
}}

/* Nested surfaces transparent — theme-priority fallback for grouped active
   widgets.  The primary suppression is a scoped CSS provider in bar.rs
   (transient priority), but this catches edge cases at theme priority. */
.widget.widget-group .widget {{
    background-color: transparent;
    border-radius: inherit;
}}

/* Grouped item hover — tint only (group surface provides base background) */
.widget-group .content > .widget-item.clickable:hover {{
    background-color: color-mix(in srgb, transparent 92%, var(--widget-hover-tint));
}}

/* ===== MERGE GROUP ===== */

/* Merge group wrapper — acts as a single visual button for adjacent
   same-popover widgets. Rounded corners clip the shared ripple effect.
   overflow:hidden is set in Rust (not a valid GTK4 CSS property). */
.widget-merge-group {{
    border-radius: var(--radius-widget);
}}

/* Merge group hover — shared background for the entire merged button */
.widget-merge-group.clickable:hover {{
    background-color: color-mix(in srgb, transparent 92%, var(--widget-hover-tint));
}}

/* Passive items in merge groups don't show their own hover */
.merge-group-content > .widget-item.passive:hover {{
    background-color: transparent;
}}

/* Pull non-first items left to overlap adjacent .content padding (2 × 10px) */
.merge-group-content > .widget-item:not(:first-child) {{
    margin-left: -20px;
}}

/* Spacing between items inside widgets */
.widget .content > *:not(:last-child),
.widget-group .content .content > *:not(:last-child) {{
    margin-right: var(--spacing-widget-gap);
}}

/* Section widget spacing via margins (Box spacing=0 to allow spacer to have no gaps) */
.bar-section--left > *:not(:last-child):not(.spacer),
.bar-section--right > *:not(:last-child):not(.spacer) {{
    margin-right: {spacing}px;
}}

/* Spacer widget - no margins so it doesn't create extra gaps */
.spacer {{
    min-width: 0;
}}

/* ===== WORKSPACE ===== */

.workspace-indicator {{
    padding: 0;
    min-width: calc(var(--widget-height) * {inactive_mult});
    min-height: calc(var(--widget-height) * {height_mult});
    border-radius: calc(var(--radius-pill) * 1.2);
    color: var(--color-foreground-faint);
    {workspace_transition}
    /* min-width duration must match INDICATOR_ANIM_DURATION_US in workspaces.rs */
}}

/* Override ripple overlay fallback radius (overlay.vp-ripple-wrap uses --radius-widget) */
overlay.workspace-indicator {{
    border-radius: calc(var(--radius-pill) * 1.2);
}}

/* Workspace indicator hover — background-color fades via the 100ms ease
   transition above.  Accent state uses --color-accent-hover-bg (pre-computed in
   the theme with luminance-aware tint direction and ratio). */
.workspace-indicator.clickable:hover {{
    background-color: var(--color-card-overlay-hover);
}}

.workspace-indicator-minimal.clickable:hover {{
    background-color: color-mix(in srgb, var(--color-foreground-faint) 80%, var(--widget-hover-tint));
}}

.workspace-indicator.active.clickable:hover {{
    background-color: var(--color-accent-hover-bg);
}}

.workspace-indicator-minimal {{
    background-color: var(--color-foreground-faint);
}}

.workspace-indicator.active {{
    color: var(--color-accent-text, #fff);
    background-color: var(--color-accent-primary);
    min-width: calc(var(--widget-height) * {active_mult});
}}

.workspace-indicator-long {{
    padding: 0 {long_hpad}px;
}}

/* Grow-in: forces zero width + no transition so container animation handles it.
   Loaded at USER+200 priority by load_transient_css() so user CSS can't defeat it. */

"#
    )
}
