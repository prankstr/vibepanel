//! Bar and workspace CSS.
//!
//! Note: This module requires config values for screen_margin and spacing,
//! so it returns a formatted String rather than a static str.

/// Return bar CSS with config values interpolated.
pub fn css(screen_margin: u32, spacing: u32) -> String {
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
    background: var(--color-background-bar);
    border-radius: var(--radius-bar);
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

/* Widget - individual widget containers */
.widget {{
    background-color: var(--color-background-widget);
    border-radius: var(--radius-widget);
    padding: var(--widget-padding-y) 10px;
    min-height: var(--widget-height);
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
    padding: 0 4px;
    min-width: 1em;
    min-height: 0.2em;
    border-radius: var(--radius-pill);
    color: var(--color-foreground-muted);
    opacity: 0.5;
}}

.workspace-indicator-minimal {{
    background-color: var(--color-foreground-muted);
}}

.workspace-indicator.active {{
    color: var(--color-accent-text, #fff);
    background-color: var(--color-accent-primary);
    opacity: 1;
}}
"#
    )
}
