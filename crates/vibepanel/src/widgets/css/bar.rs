//! Bar and workspace CSS.
//!
//! Note: This module requires config values for screen_margin and spacing,
//! so it returns a formatted String rather than a static str.

use super::WIDGET_BG_WITH_OPACITY;
use crate::widgets::workspaces::{INDICATOR_ACTIVE_MULT, LONG_INDICATOR_HPAD};

/// Return bar CSS with config values interpolated.
///
/// `workspace_animations` is the resolved per-widget animation flag: it
/// equals the explicit `[widgets.workspaces] animate` value when set,
/// otherwise falls back to the global `theme.animations` setting.  This
/// lets `workspaces.animate = true` keep workspace indicator CSS transitions
/// alive even when `theme.animations = false`.
pub fn css(screen_margin: u32, spacing: u32, workspace_animations: bool) -> String {
    let widget_bg = WIDGET_BG_WITH_OPACITY;
    let active_mult = INDICATOR_ACTIVE_MULT;
    let long_hpad = LONG_INDICATOR_HPAD;
    // Vertical workspace indicators grow along the physical Y axis, which reads
    // faster than horizontal growth at the same duration; tune it separately.
    let workspace_transition = if workspace_animations {
        "transition: min-width 200ms linear, min-height 200ms linear, background-color 100ms ease;"
    } else {
        "transition: none;"
    };
    let workspace_transition_vertical = if workspace_animations {
        "transition: min-width 150ms linear, min-height 150ms linear, background-color 100ms ease;"
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

.bar-shell-inner.bar--vertical {{
    padding-left: 0;
    padding-right: 0;
    padding-top: {screen_margin}px;
    padding-bottom: {screen_margin}px;
}}

/* Bar container - the visible bar */
sectioned-bar.bar {{
    min-height: var(--bar-height);
    padding-top: var(--bar-padding-top, var(--bar-padding-y, var(--vp-internal-bar-padding-top)));
    padding-right: var(--bar-padding-right, var(--bar-padding-x, var(--vp-internal-bar-padding-right)));
    padding-bottom: var(--bar-padding-bottom, var(--bar-padding-y, var(--vp-internal-bar-padding-bottom)));
    padding-left: var(--bar-padding-left, var(--bar-padding-x, var(--vp-internal-bar-padding-left)));
    background: var(--color-background-bar);
    background-clip: padding-box;
    border: var(--bar-outline-width) solid color-mix(in srgb, var(--bar-outline-color) var(--bar-outline-opacity), transparent);
    border-radius: var(--radius-bar);
    font-family: var(--font-family);
    font-size: var(--font-size);
    color: var(--color-foreground-primary);
}}

sectioned-bar.bar.bar--vertical {{
    min-width: var(--bar-height);
    min-height: 0;
}}

/* Widget wrapper — transparent so only .widget paints a background layer */
.widget-wrapper {{
    min-height: var(--widget-height);
    background: transparent;
}}

/* Widget — visual surface */
.widget {{
    background-color: {widget_bg};
    background-clip: padding-box;
    border: var(--widget-outline-width) solid color-mix(in srgb, var(--widget-outline-color) var(--widget-outline-opacity), transparent);
    border-radius: var(--radius-widget);
}}

/* Standalone widgets get a square floor. Short content (icon, one letter,
   etc.) stays round from border-radius alone; longer content grows into a pill. */
.widget:not(.widget-group) {{
    min-width: var(--bar-height);
    min-height: var(--bar-height);
}}

/* Widget spacing model:
   - root -h/-v tokens are orientation defaults from the theme;
   - .widget-item computes default effective tokens for passive/merge wrappers;
   - .widget > overlay > .content recomputes them so user overrides scoped to
     widget identity classes like `.clock` or `.battery` take effect;
   - vertical bars switch label/default widgets to compact -v tokens;
   - tray/workspaces intentionally keep roomy -h spacing even in vertical bars;
   - public --widget-content-padding-offset / --widget-content-gap-offset are additive offsets.
   Layout rules consume --vp-widget-content-padding / --vp-widget-content-gap only. */
.widget-item {{
    --vp-widget-content-padding: max(0px, calc(var(--vp-widget-content-padding-h) + var(--widget-content-padding-offset, 0px)));
    --vp-widget-content-gap: max(0px, calc(var(--vp-widget-content-gap-h) + var(--widget-content-gap-offset, 0px)));
}}

.bar--vertical .widget-item {{
    --vp-widget-content-padding: max(0px, calc(var(--vp-widget-content-padding-v) + var(--widget-content-padding-offset, 0px)));
    --vp-widget-content-gap: max(0px, calc(var(--vp-widget-content-gap-v) + var(--widget-content-gap-offset, 0px)));
}}

/* Widget identity classes live on .widget, not .widget-item, so recompute
   the final tokens on .content where widget-scoped user overrides inherit. */
.widget:not(.widget-group) > overlay > .content {{
    --vp-widget-content-padding: max(0px, calc(var(--vp-widget-content-padding-h) + var(--widget-content-padding-offset, 0px)));
    --vp-widget-content-gap: max(0px, calc(var(--vp-widget-content-gap-h) + var(--widget-content-gap-offset, 0px)));
    --font-size: calc(var(--widget-height) * var(--font-scale));
    font-size: var(--font-size);
}}

.bar--vertical .widget:not(.widget-group) > overlay > .content {{
    --vp-widget-content-padding: max(0px, calc(var(--vp-widget-content-padding-v) + var(--widget-content-padding-offset, 0px)));
    --vp-widget-content-gap: max(0px, calc(var(--vp-widget-content-gap-v) + var(--widget-content-gap-offset, 0px)));
}}

/* Icon/dot widgets lack line-height slack, so keep their roomier horizontal
   spacing tokens even when the bar is vertical. This equal-specificity override
   relies on source order to beat the generic vertical rule above. */
.bar--vertical .widget.tray > overlay > .content,
.bar--vertical .widget.workspaces > overlay > .content {{
    --vp-widget-content-padding: max(0px, calc(var(--vp-widget-content-padding-h) + var(--widget-content-padding-offset, 0px)));
    --vp-widget-content-gap: max(0px, calc(var(--vp-widget-content-gap-h) + var(--widget-content-gap-offset, 0px)));
}}

/* Padding on .content (not the container) so the ripple overlay
   fills the entire widget background area edge-to-edge.
   Passive widgets (merge groups) have no .widget surface — target
   their .content directly via the third selector. */
.widget:not(.widget-group) .content {{
    padding: var(--widget-padding-y) var(--vp-widget-content-padding);
}}

.bar--vertical .widget:not(.widget-group) .content {{
    padding: var(--vp-widget-content-padding) var(--widget-padding-y);
}}

.widget-group > .content > .widget-item .content,
.widget-item.passive > .content {{
    padding: var(--widget-padding-y) var(--vp-widget-content-padding);
    --font-size: calc(var(--widget-height) * var(--font-scale));
    font-size: var(--font-size);
}}

.bar--vertical .widget-group > .content > .widget-item .content,
.bar--vertical .widget-item.passive > .content {{
    padding: var(--vp-widget-content-padding) var(--widget-padding-y);
}}

/* Widget groups — surface is transparent; each child paints its own base
   so translucent hover composites over the bar background, not on a
   stacked group base. Outer pill shape comes from position-aware radius
   on the first/last children below. */
.widget.widget-group {{
    padding: 0;
    background-color: transparent;
    background-image: none;
}}

/* Hover targets the wrapper but paints on the surface child */
.widget-wrapper.clickable:hover > .widget:not(.widget-group) {{
    background-color: var(--color-widget-hover-bg);
}}

/* Each grouped child paints its own background. Both adjacent painted
   siblings (.widget-merge-group and .widget-item) sit at the same DOM
   depth (direct children of .widget-group > .content) so their painted
   regions meet flush in the parent Box. The inner .widget stays
   transparent — painting the outer .widget-item ensures the painted
   surface fills the item's allocation, eliminating subpixel seams that
   appear when paint depth differs between siblings. */
.widget-group > .content > .widget-item {{
    background-color: {widget_bg};
}}
.widget-group > .content > .widget-item > .widget {{
    background-color: transparent;
    box-shadow: none;
    border: none;
}}

/* Use the effective content gap at every seam inside an explicit group.
   Each item carries full `--vp-widget-content-padding` at the group edges, but
   interior seams should match the same gap users tune between child elements.
   Merge groups handle their collapsed seams separately below.
   - .widget-item: padding lives on .widget > overlay > .content.
   - .widget-merge-group: the leftmost/rightmost passive item's .content
     carries the seam-facing padding (interior passive items are already
     overlapped by margin-left).
   :not(:first-child) → has a previous sibling → halve left side.
   :not(:last-child)  → has a following sibling → halve right side. */
.widget-group > .content > .widget-item:not(:first-child) > .widget > overlay > .content,
.widget-group > .content > .widget-merge-group:not(:first-child) > .merge-group-content > .widget-item:first-child > .content {{
    padding-left: calc(var(--vp-widget-content-gap) / 2);
}}
.widget-group > .content > .widget-item:not(:last-child) > .widget > overlay > .content,
.widget-group > .content > .widget-merge-group:not(:last-child) > .merge-group-content > .widget-item:last-child > .content {{
    padding-right: calc(var(--vp-widget-content-gap) / 2);
}}

.bar--vertical .widget-group > .content > .widget-item:not(:first-child) > .widget > overlay > .content,
.bar--vertical .widget-group > .content > .widget-merge-group:not(:first-child) > .merge-group-content > .widget-item:first-child > .content {{
    padding-left: var(--widget-padding-y);
    padding-top: calc(var(--vp-widget-content-gap) / 2);
}}
.bar--vertical .widget-group > .content > .widget-item:not(:last-child) > .widget > overlay > .content,
.bar--vertical .widget-group > .content > .widget-merge-group:not(:last-child) > .merge-group-content > .widget-item:last-child > .content {{
    padding-right: var(--widget-padding-y);
    padding-bottom: calc(var(--vp-widget-content-gap) / 2);
}}

/* Position-aware pill shape: outer corners rounded only on the leading
   and trailing children; interior edges square so adjacent children meet
   flush. Applies to both plain items and merge wrappers. */
.widget-group > .content > .widget-item,
.widget-group > .content > .widget-merge-group {{
    border-radius: 0;
}}
.widget-group > .content > :first-child {{
    border-top-left-radius: var(--radius-widget);
    border-bottom-left-radius: var(--radius-widget);
}}
.widget-group > .content > :last-child {{
    border-top-right-radius: var(--radius-widget);
    border-bottom-right-radius: var(--radius-widget);
}}

.bar--vertical .widget-group > .content > :first-child {{
    border-top-left-radius: var(--radius-widget);
    border-top-right-radius: var(--radius-widget);
    border-bottom-left-radius: 0;
}}
.bar--vertical .widget-group > .content > :last-child {{
    border-top-right-radius: 0;
    border-bottom-left-radius: var(--radius-widget);
    border-bottom-right-radius: var(--radius-widget);
}}

/* Inner .widget surface inside grouped items must match its parent
   .widget-item's position-aware radius — the inner .widget has
   Overflow::Hidden which clips the ripple, so without this override
   the ripple would clip to the standalone --radius-widget shape and
   not reach the painted square corners at interior seams. */
.widget-group > .content > .widget-item > .widget {{
    border-radius: 0;
}}
.widget-group > .content > .widget-item:first-child > .widget {{
    border-top-left-radius: var(--radius-widget);
    border-bottom-left-radius: var(--radius-widget);
}}
.widget-group > .content > .widget-item:last-child > .widget {{
    border-top-right-radius: var(--radius-widget);
    border-bottom-right-radius: var(--radius-widget);
}}

.bar--vertical .widget-group > .content > .widget-item:first-child > .widget {{
    border-top-left-radius: var(--radius-widget);
    border-top-right-radius: var(--radius-widget);
    border-bottom-left-radius: 0;
}}
.bar--vertical .widget-group > .content > .widget-item:last-child > .widget {{
    border-top-right-radius: 0;
    border-bottom-left-radius: var(--radius-widget);
    border-bottom-right-radius: var(--radius-widget);
}}

/* Grouped child hover — clears the cell base and paints a rounded hover
   pill on the inner surface. The spread shadow restores the cell's base
   color behind the rounded corners, clipped by the child allocation, so
   translucent hover values composite over the bar background exactly once
   without exposing wallpaper at mixed-group seams. */
.widget-group > .content > .widget-item.clickable:hover {{
    background-color: transparent;
}}
.widget-group > .content > .widget-item.clickable:hover > .widget {{
    background-color: var(--color-widget-hover-bg);
    border-radius: var(--radius-widget);
    box-shadow: 0 0 0 9999px {widget_bg};
}}

/* ===== MERGE GROUP ===== */

/* Merge group wrapper — acts as a single visual button for adjacent
   same-popover widgets. Paints its own base; the parent group surface
   is transparent. overflow:hidden is set in Rust (not a valid GTK4 CSS
   property). */
.widget-merge-group {{
    background-color: {widget_bg};
    border-radius: var(--radius-widget);
}}
.widget-merge-group > .merge-group-content {{
    box-shadow: none;
}}

/* Ripple clip box (added in build_merge_group). Establishes a rounded clip
   for the merge-group ripple so it matches the inner pill shape — the
   merge-group itself uses position-aware radius and would otherwise leak
   the ripple past rounded hover edges at mixed-group seams. */
.widget-merge-group-ripple-clip {{
    border-radius: var(--radius-widget);
    background: transparent;
}}

.widget-group > .content > .widget-merge-group.clickable:hover {{
    background-color: transparent;
}}
.widget-group > .content > .widget-merge-group.clickable:hover > .merge-group-content {{
    background-color: var(--color-widget-hover-bg);
    border-radius: var(--radius-widget);
    box-shadow: 0 0 0 9999px {widget_bg};
}}

/* Passive items in merge groups don't show their own hover */
.merge-group-content > .widget-item.passive:hover {{
    background-color: transparent;
}}

/* Pull non-first items back over adjacent .content padding. GTK Box spacing
   already contributes the orientation's base gap, so only add the user gap
   offset delta here; with no offset this preserves the original overlap. */
.merge-group-content > .widget-item:not(:first-child) {{
    margin-left: calc(var(--vp-widget-content-gap) - var(--vp-widget-content-gap-h) - 2 * var(--vp-widget-content-padding));
}}

.bar--vertical .merge-group-content > .widget-item:not(:first-child) {{
    margin-left: 0;
    margin-top: calc(var(--vp-widget-content-gap) - var(--vp-widget-content-gap-v) - 2 * var(--vp-widget-content-padding));
}}

/* Spacers have no inner padding and can be zero-width; don't give them a
   negative margin or GTK reports a negative minimum size. The following
   widget pulls in far enough to collapse the extra GTK spacing seam. */
.merge-group-content > .widget-item.spacer:not(:first-child) {{
    margin-left: 0;
}}
.merge-group-content > .widget-item.spacer + .widget-item {{
    margin-left: calc(-3 * var(--vp-widget-content-padding));
}}

.bar--vertical .merge-group-content > .widget-item.spacer:not(:first-child) {{
    margin-top: 0;
}}
.bar--vertical .merge-group-content > .widget-item.spacer + .widget-item {{
    margin-left: 0;
    margin-top: calc(-3 * var(--vp-widget-content-padding));
}}

/* Spacing between items inside widgets (icon→label, etc.).
   Restricted to inner .content elements: standalone widgets' .content sits
   inside .widget (which is NOT .widget-group), and grouped items' inner
   .content sits inside .widget-item.passive (merge groups) or one extra
   level deep under .widget-group's outer .content. The outer .content
   directly under .widget.widget-group must NOT match — its direct children
   are the painted siblings (.widget-merge-group, .widget-item) that need
   to meet flush with zero margin between them. */
.widget:not(.widget-group):not(.taskbar) > overlay > .content > *:not(:last-child),
.widget:not(.widget-group):not(.taskbar) > .content > *:not(:last-child),
.widget-group .content .content > *:not(:last-child) {{
    margin-right: var(--vp-widget-content-gap);
}}

.bar--vertical .widget:not(.widget-group):not(.taskbar) > overlay > .content > *:not(:last-child),
.bar--vertical .widget:not(.widget-group):not(.taskbar) > .content > *:not(:last-child),
.bar--vertical .widget-group .content .content > *:not(:last-child) {{
    margin-right: 0;
    margin-bottom: var(--vp-widget-content-gap);
}}

/* Section widget spacing via margins (Box spacing=0 to allow spacer to have no gaps) */
.bar-section--left > *:not(:last-child):not(.spacer),
.bar-section--right > *:not(:last-child):not(.spacer),
.bar-section--center > *:not(:last-child):not(.spacer) {{
    margin-right: {spacing}px;
}}

.bar--vertical .bar-section--left > *:not(:last-child):not(.spacer),
.bar--vertical .bar-section--right > *:not(:last-child):not(.spacer),
.bar--vertical .bar-section--center > *:not(:last-child):not(.spacer) {{
    margin-right: 0;
    margin-bottom: {spacing}px;
}}

/* Spacer widget - no margins so it doesn't create extra gaps */
.spacer {{
    min-width: 0;
}}

/* ===== WORKSPACE ===== */

.workspace-indicator {{
    padding: 0;
    min-width: var(--icon-size);
    min-height: var(--icon-size);
    border-radius: calc(var(--radius-pill) * 1.2);
    color: var(--color-foreground-faint);
    {workspace_transition}
    /* horizontal: min-width duration matches INDICATOR_ANIM_DURATION_US in workspaces.rs */
}}

.bar--vertical .workspace-indicator {{
    {workspace_transition_vertical}
}}

/* Override ripple overlay fallback radius (overlay.vp-ripple-wrap uses --radius-widget) */
overlay.workspace-indicator {{
    border-radius: calc(var(--radius-pill) * 1.2);
}}

/* Workspace hover backgrounds use scoped tokens so active hover can differ from global accent hover. */
.workspace-indicator.clickable:hover {{
    background-color: var(--color-workspace-indicator-hover-bg, var(--color-workspace-indicator-hover-default-bg));
}}

.workspace-indicator.active.clickable:hover {{
    background-color: var(--color-workspace-indicator-active-hover-bg);
}}

.workspace-indicator.urgent.clickable:hover {{
    background-color: var(--color-workspace-indicator-urgent-hover-bg);
}}

.workspace-indicator-minimal {{
    --color-workspace-indicator-hover-default-bg: color-mix(in srgb, var(--color-foreground-faint) 80%, var(--widget-hover-tint));
    background-color: var(--color-foreground-faint);
}}

.workspace-indicator.active {{
    color: var(--color-accent-text, #fff);
    background-color: var(--color-accent-primary);
    min-width: calc(var(--widget-height) * {active_mult});
}}

.workspace-indicator.urgent {{
    color: var(--color-accent-text, #fff);
    background-color: var(--color-state-urgent);
}}

.bar--vertical .workspace-indicator.active {{
    min-width: var(--icon-size);
    min-height: calc(var(--widget-height) * {active_mult});
}}

.workspace-indicator-long {{
    padding: 0 {long_hpad}px;
}}

.bar--vertical .workspace-indicator-long {{
    padding: 0;
}}

workspace-container > .workspace-indicator:not(:last-child) {{
    margin-right: var(--vp-widget-content-gap);
}}

.bar--vertical workspace-container > .workspace-indicator:not(:last-child) {{
    margin-right: 0;
    margin-bottom: var(--vp-widget-content-gap);
}}

/* Grow-in: forces zero width + no transition so container animation handles it.
   Loaded at USER+200 priority by load_transient_css() so user CSS can't defeat it. */

/* ===== TASKBAR ===== */
/* Button padding + border-radius are applied via a shared CssProvider so
   they scale with icon_size and the theme's widget_radius_percent.
   Taskbar is excluded from generic child spacing because its direct
   children are buttons/separators, not the visual icons/labels that the
   generic rule targets. The runtime CSS in taskbar.rs sets per-instance
   internal --vp-taskbar-* variables as calc expressions derived from
   raw theme spacing tokens plus public offsets. Users can tune taskbar spacing via the
   public --widget-content-padding-offset / --widget-content-gap-offset hooks
   scoped to .taskbar. */

/* Override the generic `.widget:not(.widget-group) .content` flow-axis
   padding to use the per-instance --vp-taskbar-content-edge variable. The
   cross-axis padding (var(--widget-padding-y)) still comes from the generic
   rule. Specificity (.widget.taskbar:not(.widget-group) .content = 0,4,0)
   beats the generic rule (0,3,0). */
.widget.taskbar:not(.widget-group) .content {{
    padding-left: var(--vp-taskbar-content-edge, 0px);
    padding-right: var(--vp-taskbar-content-edge, 0px);
}}

.bar--vertical .widget.taskbar:not(.widget-group) .content {{
    padding-left: 0;
    padding-right: 0;
    padding-top: var(--vp-taskbar-content-edge, 0px);
    padding-bottom: var(--vp-taskbar-content-edge, 0px);
}}

.taskbar .content > .taskbar-separator {{
    min-width: 1px;
}}

.taskbar .content > .taskbar-button + .taskbar-button {{
    margin-left: var(--vp-taskbar-button-gap, 0px);
}}

.taskbar .content > .taskbar-separator {{
    margin-left: var(--vp-taskbar-separator-gap, 0px);
    margin-right: var(--vp-taskbar-separator-gap, 0px);
}}

.bar--vertical .taskbar .content > .taskbar-button + .taskbar-button {{
    margin-left: 0;
    margin-top: calc(var(--vp-taskbar-button-gap, 0px) + 1px);
}}

.bar--vertical .taskbar .content > .taskbar-button:last-child {{
    margin-bottom: 2px;
}}

.bar--vertical .taskbar .content > .taskbar-separator {{
    margin-left: 0;
    margin-right: 0;
    margin-top: calc(var(--vp-taskbar-separator-gap, 0px) + 2px);
    margin-bottom: calc(var(--vp-taskbar-separator-gap, 0px) + 2px);
}}

.bar--vertical .taskbar .content > .taskbar-separator-has-label {{
    margin-top: var(--vp-taskbar-separator-gap, 0px);
}}

.bar--vertical .taskbar .content > .taskbar-separator {{
    min-width: 0;
    min-height: 1px;
}}

.taskbar .content > .taskbar-separator > .taskbar-separator-line {{
    background-color: currentColor;
    opacity: 0.3;
    min-width: 1px;
    margin-top: 4px;
    margin-bottom: 4px;
}}

.taskbar .content > .taskbar-separator > .taskbar-separator-label {{
    color: currentColor;
    opacity: 0.65;
    font-size: 0.75em;
    font-weight: 600;
    min-width: 0;
    margin-left: 2px;
    margin-right: 2px;
}}

.taskbar .content > .taskbar-separator-active > .taskbar-separator-label {{
    color: var(--color-accent-primary);
    opacity: 1;
}}

.taskbar .content > .taskbar-separator-urgent > .taskbar-separator-label {{
    color: var(--color-state-urgent);
    opacity: 1;
}}

.taskbar .content > .taskbar-output-separator > .taskbar-separator-line {{
    opacity: 0.45;
    margin-top: 2px;
    margin-bottom: 2px;
}}

.bar--vertical .taskbar .content > .taskbar-separator > .taskbar-separator-line {{
    min-width: 0;
    min-height: 1px;
    margin-top: 0;
    margin-bottom: 0;
    margin-left: 4px;
    margin-right: 4px;
}}

.bar--vertical .taskbar .content > .taskbar-separator > .taskbar-separator-label {{
    margin-left: 0;
    margin-right: 0;
    margin-top: 2px;
    margin-bottom: 0;
}}

.bar--vertical .taskbar .content > .taskbar-separator:first-child > .taskbar-separator-label {{
    margin-top: 4px;
}}

.bar--vertical .taskbar .content > .taskbar-output-separator > .taskbar-separator-line {{
    margin-left: 2px;
    margin-right: 2px;
}}

.taskbar-button.clickable:hover {{
    background-color: var(--color-taskbar-button-hover-bg);
}}

.taskbar-button.active {{
    background-color: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
}}

.taskbar-button.active.clickable:hover {{
    background-color: var(--color-taskbar-button-active-hover-bg);
}}

.taskbar-button.urgent.clickable:hover {{
    background-color: var(--color-taskbar-button-urgent-hover-bg);
}}

.taskbar-button.urgent {{
    background-color: var(--color-state-urgent);
    color: var(--color-accent-text, #fff);
}}

"#
    )
}
