//! Bar and workspace CSS.
//!
//! Note: This module requires config values for screen_margin and spacing,
//! so it returns a formatted String rather than a static str.

use super::WIDGET_BG_WITH_OPACITY;
use crate::widgets::workspaces::{
    INDICATOR_ACTIVE_MULT, INDICATOR_HEIGHT_MULT, INDICATOR_INACTIVE_MULT,
};

/// Return bar CSS with config values interpolated.
pub fn css(screen_margin: u32, spacing: u32) -> String {
    let widget_bg = WIDGET_BG_WITH_OPACITY;
    let inactive_mult = INDICATOR_INACTIVE_MULT;
    let active_mult = INDICATOR_ACTIVE_MULT;
    let height_mult = INDICATOR_HEIGHT_MULT;
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

/* Widget - individual widget containers */
/* color-mix() is inline here so per-widget --widget-background-color overrides work via CSS scoping */
.widget {{
    background-color: {widget_bg};
    border-radius: var(--radius-widget);
    padding: var(--widget-padding-y) 10px;
    min-height: var(--widget-height);
}}

/* Widget groups - remove padding so hover can extend to edges */
.widget.widget-group {{
    padding: 0;
}}

/* Widget hover state - standalone clickable widgets */
.widget.clickable:not(.widget-group):hover {{
    background-image: linear-gradient(var(--color-card-overlay-hover), var(--color-card-overlay-hover));
}}

/* Widget items inside groups - symmetric padding for hover area */
.widget-group > .content > .widget-item {{
    padding: var(--widget-padding-y) 10px;
}}

/* Pull non-first items left to overlap with previous item's right padding */
.widget-group > .content > .widget-item:not(:first-child) {{
    margin-left: -20px;
}}

/* Widget items inside groups - individual clickable hover targets */
.widget-group > .content > .widget-item.clickable:hover {{
    background-image: linear-gradient(var(--color-card-overlay-hover), var(--color-card-overlay-hover));
    border-radius: var(--radius-widget);
}}

/* Spacing between items inside widgets */
.widget > .content > *:not(:last-child),
.widget-group > .content .content > *:not(:last-child) {{
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
    transition: min-width 200ms linear, background-color 200ms ease;
}}

/* Workspace indicator hover state - clickable indicators */
.workspace-indicator.clickable:hover {{
    background-image: linear-gradient(var(--color-card-overlay-hover), var(--color-card-overlay-hover));
}}

.workspace-indicator-minimal {{
    background-color: var(--color-foreground-faint);
}}

.workspace-indicator.active {{
    color: var(--color-accent-text, #fff);
    background-color: var(--color-accent-primary);
    min-width: calc(var(--widget-height) * {active_mult});
}}

.workspace-indicator.workspace-no-animate {{
    transition: none;
}}

/* Collapsed state for enter animations — forces zero width so CSS transition grows it in */
.workspace-indicator.workspace-indicator-collapsed {{
    min-width: 0;
}}

"#
    )
}
