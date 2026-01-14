//! CSS for vibepanel bar, panels, and widgets.
//!
//! This module contains all CSS generation for vibepanel:
//! - `utility_css()` - Shared utility classes (colors, focus suppression, popovers)
//! - `widget_css()` - Widget-specific styling (bar, cards, sliders, etc.)

use vibepanel_core::Config;

/// Return shared utility CSS.
///
/// These are truly shared styles that apply across multiple surfaces
/// (bar, popovers, quick settings, etc).
pub fn utility_css() -> String {
    r#"
/* ===== SHARED UTILITY CSS ===== */

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
    border-radius: 50%;
    color: var(--color-foreground-primary);
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

/* Make popover shell transparent so our content shows */
/* Note: border-radius is applied by SurfaceStyleManager::apply_surface_styles() */
popover.widget-menu {
    background: transparent;
    border: none;
    box-shadow: none;
}

popover.widget-menu > contents,
popover.widget-menu.background > contents {
    background: transparent;
    border: none;
    box-shadow: none;
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
    border-radius: 50%;
    transition: background 150ms ease-out;
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
    border-radius: calc(var(--slider-height) / 2);
    background-color: var(--color-slider-track);
}

.slider-row scale highlight {
    background-image: image(var(--color-accent-slider, var(--color-accent-primary)));
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border: none;
    min-height: var(--slider-height);
    border-radius: calc(var(--slider-height) / 2);
}

.slider-row scale slider {
    min-width: 16px;
    min-height: 16px;
    margin: -5px;
    padding: 0;
    background-color: var(--color-accent-primary);
    border-radius: 50%;
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
    border-radius: 50%;
}
.slider-row .qs-toggle-more:hover {
    background: var(--color-card-overlay-hover);
}
"#
    .to_string()
}

/// Generate all widget CSS.
pub fn widget_css(config: &Config) -> String {
    let outer_margin = config.bar.outer_margin;
    let widget_spacing = config.bar.widget_spacing;

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
    padding-left: {outer_margin}px;
    padding-right: {outer_margin}px;
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
    padding: 0px 10px;
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
    margin-right: {widget_spacing}px;
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
    border-radius: calc(var(--radius-pill) * 1.2);
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

/* ===== SYSTEM TRAY ===== */

/* Tray item hover - subtle scale up */
.tray-item {{
    transition: transform 100ms ease-out;
}}
.tray-item:hover {{
    transform: scale(1.15);
}}

/* Ensure tray item images have no visual artifacts during updates */
.tray-item image,
.tray-item .icon-root,
.tray-item .icon-root image {{
    border: none;
    box-shadow: none;
    outline: none;
    background: transparent;
}}

.tray-menu {{
    padding: 6px;
    font-family: var(--font-family);
    font-size: var(--font-size);
}}

/* Row menu items - extends tray-menu-button pattern */
.qs-row-menu-item,
.tray-menu-button {{
    background: transparent;
    border: none;
    box-shadow: none;
    padding: 4px 8px;
}}

.qs-row-menu-item:hover,
.tray-menu-button:hover {{
    background-color: var(--color-card-overlay-hover);
}}

.tray-menu-button:disabled {{
    opacity: 0.5;
}}

.tray-menu-button:disabled:hover {{
    background: transparent;
}}

/* ===== BUTTONS ===== */

/* Reset button - strips GTK chrome (background, border, shadow) */
button.vp-btn-reset,
button.vp-btn-compact {{
    background: transparent;
    border: none;
    box-shadow: none;
    outline: none;
}}

/* Compact button - reset + zero padding/margin for icon-only buttons */
button.vp-btn-compact {{
    padding: 0;
    margin: 0;
    min-width: 0;
    min-height: 0;
}}

button.vp-btn-reset:focus,
button.vp-btn-reset:focus-visible,
button.vp-btn-compact:focus,
button.vp-btn-compact:focus-visible {{
    outline: none;
    border: none;
    box-shadow: none;
}}

button.vp-btn-accent {{
    background: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
}}

button.vp-btn-accent:hover {{
    opacity: 0.85;
}}

button.vp-btn-card {{
    background: var(--color-card-overlay);
    color: var(--color-foreground-primary);
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
}}

button.vp-btn-card:hover {{
    background: var(--color-card-overlay-hover);
}}

/* Link-style button - text only, no background */
button.vp-btn-link,
.vp-btn-link {{
    background: transparent;
    border: none;
    box-shadow: none;
    color: var(--color-accent-primary);
    padding: 0;
    min-height: 0;
}}

button.vp-btn-link:hover,
.vp-btn-link:hover {{
    background: transparent;
    text-decoration: underline;
}}

/* Ghost button - transparent with hover effect */
button.vp-btn-ghost {{
    background: transparent;
    border: none;
    box-shadow: none;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
}}

button.vp-btn-ghost:hover {{
    background: var(--color-card-overlay-hover);
}}

/* ===== CALENDAR ===== */

/* Note: padding comes from apply_surface_styles() in base.rs */
.calendar-popover {{
}}

calendar.view {{
    background: transparent;
    border: none;
    color: var(--color-foreground-primary);
}}

calendar.view grid {{
    background: transparent;
}}

calendar.view grid label.week-number {{
        font-size: var(--font-size-xs);
        color: var(--color-foreground-muted);
    }}

calendar.view grid label.today {{
    background: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
    border-radius: var(--radius-pill);
    box-shadow: none;
}}

calendar.view grid label.day-number:focus {{
    outline: none;
    border: none;
    box-shadow: none;
}}

calendar.view grid *:selected:not(.today) {{
    background: transparent;
    color: inherit;
    box-shadow: none;
}}

calendar.view grid label.day-number {{
    margin: 1px 2px;
    min-width: 24px;
    min-height: 24px;
}}

.week-number-header {{
    font-size: var(--font-size-xs);
    color: var(--color-foreground-muted);
    margin-left: 20px; /* Align with week numbers column */
    margin-top: 16px; /* Align vertically with day headers (M T W...) */
}}

/* ===== QUICK SETTINGS ===== */

/* Window transparency */
window.quick-settings-window {{
    background: transparent;
}}

/* QS window container - extra top padding to compensate for 0 top margin
   (top margin must be 0 for correct popover_offset positioning) */
.qs-window-container {{
    padding-top: 4px;
}}

/* Click catcher overlay */
.vp-click-catcher {{
    background: var(--color-click-catcher-overlay);
}}

/* Cards */
.vp-card {{
    background: var(--color-card-overlay);
    border-radius: var(--radius-widget);
    /* No padding here - children handle their own padding for better click targets */
}}

/* Card hover state */
.vp-card:hover,
.qs-row:hover {{
    background: var(--color-card-overlay-hover);
}}

/* Toggle button fills card and provides its own padding */
.vp-card > .vp-btn-reset {{
    padding: 8px 10px;
}}

/* Expander chevron padding */
.vp-card > .qs-toggle-more {{
    margin-right: 8px;
}}

/* Toggle card icon spacing */
.qs-toggle-icon {{
    margin-left: 2px;
    margin-right: 4px;
}}

/* Row icon spacing */
.qs-row-icon {{
    margin-left: 1px;
    margin-right: 3px;
}}

/* Wi-Fi disabled state override */
.qs-wifi-disabled-icon {{
    color: var(--color-foreground-muted);
    opacity: 0.5;
}}

/* Reset styling for QS buttons - extends vp-btn-reset */
.qs-toggle-more,
.qs-scan-button {{
    background: transparent;
    border: none;
    box-shadow: none;
}}

/* Expander chevron button */
.qs-toggle-more {{
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: 50%;
}}

.qs-toggle-more:hover {{
    background: var(--color-card-overlay-hover);
}}

/* List items */
.qs-list {{
    background: transparent;
}}

.qs-row {{
    background: var(--color-card-overlay);
    border-radius: var(--radius-widget);
    padding: 6px 10px;
    margin: 3px 0;
}}

/* Row menu content */
.qs-row-menu-content {{
    font-family: var(--font-family);
    font-size: var(--font-size);
    border-radius: var(--radius-surface);
}}

/* Row hamburger menu button */
.qs-row-menu-button {{
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: 50%;
}}

.qs-row-menu-button:hover {{
    background: var(--color-card-overlay-hover);
}}

/* Accent colors - state override for active icons/toggles */
.qs-icon-active {{
    color: var(--color-accent-primary);
}}

/* Row titles - color via vp-primary */
.qs-row-title {{
    font-size: var(--font-size-md);
    margin-top: 1px;
}}

/* Row action labels - color via vp-accent */
.qs-row-action-label {{
    font-size: var(--font-size-sm);
}}

/* Subtitles - secondary info, color via vp-muted */
.qs-toggle-subtitle,
.qs-row-subtitle {{
    font-size: var(--font-size-sm);
}}

/* Accent color state override for active subtitles */
.qs-subtitle-active {{
    color: var(--color-accent-primary);
}}

.qs-scan-button:hover {{
    background: var(--color-card-overlay-hover);
    border-radius: var(--radius-pill);
}}

/* Scan label */
.qs-scan-label {{
    margin-top: 4px;
    margin-bottom: 2px;
}}

/* Scanning state - state override */
.qs-scan-label-scanning {{
    color: var(--color-foreground-muted);
}}

/* Chevron animation */
.qs-toggle-more-icon {{
    transition: transform 200ms ease;
    font-size: 1.25em;
    font-weight: bold;
    -gtk-icon-style: symbolic;
    margin-top: 2px;
}}

.qs-toggle-more-icon.expanded {{
    margin-top: -2px;
    transform: rotate(180deg);
}}

/* Power card hold-to-confirm progress */
.qs-power-progress {{
    background-color: transparent;
    min-width: 0;
    border-radius: var(--radius-widget);
}}

.qs-power-progress.qs-power-confirming {{
    background-color: var(--color-accent-primary);
}}

/* Power action rows - remove padding since overlay content provides it */
.qs-power-row {{
    padding: 0;
}}

/* Power row content - needs padding since it's an overlay above progress */
.qs-power-row-content {{
    padding: 6px 10px;
}}

/* Power details container - add spacing from toggle card */
.qs-power-details {{
    margin-top: 6px;
}}

/* Progress bar inside power rows */
.qs-power-row .qs-power-progress {{
    border-radius: var(--radius-widget);
}}

        /* ===== BATTERY ===== */

        /* Battery state classes - applied directly to the backend widget */
        .battery-icon.battery-charging {{
            color: var(--color-accent-primary);
        }}

        .battery-icon.battery-low {{
            color: var(--color-state-urgent);
        }}

        /* Battery popover */
        .battery-popover-percent {{
            font-size: var(--font-size-lg);
            font-weight: 700;
        }}

        .battery-popover-state {{
            font-weight: 500;
        }}

        .battery-popover-time,
        .battery-popover-power {{
            font-size: var(--font-size-sm);
        }}

        .battery-popover-profile-button {{
            font-size: var(--font-size-sm);
            border-radius: var(--radius-widget);
        }}

        .battery-popover-profile-button:hover {{
            background: var(--color-card-overlay-hover);
        }}

        /* ===== NOTIFICATIONS ===== */
        /* Shared styles for both popover rows and toasts */

        /* Bell icon states */
        .notification-icon.has-critical {{
            color: var(--color-state-warning);
        }}

        .notification-icon.backend-unavailable {{
            opacity: 0.4;
        }}

        /* Badge indicator dot */
        .notification-badge {{
            margin-right: 2px;
            margin-top: 3px;
        }}

        .notification-badge-dot {{
            min-width: 8px;
            min-height: 8px;
            padding: 0;
            border-radius: 9999px;
            background-color: var(--color-accent-primary);
        }}

        /* Shared icon styling (row + toast) */
        .notification-row-icon,
        .notification-toast-icon {{
            margin-top: 2px;
            min-width: 48px;
            min-height: 48px;
            border-radius: 9999px;
        }}

        /* Shared typography (row + toast) */
        .notification-app-name,
        .notification-toast-app {{
            font-size: var(--font-size-sm);
            font-weight: 600;
        }}

        .notification-summary,
        .notification-toast-summary {{
            font-size: var(--font-size-md);
            font-weight: 500;
        }}

        .notification-body,
        .notification-toast-body {{
            font-size: var(--font-size-sm);
            margin-top: 2px;
        }}

        /* Shared dismiss button styling (row + toast) */
        .notification-dismiss-btn,
        .notification-toast-dismiss {{
            min-width: 24px;
            min-height: 24px;
            padding: 0;
            opacity: 0.7;
            border-radius: 50%;
        }}

        .notification-dismiss-btn:hover,
        .notification-toast-dismiss:hover {{
            opacity: 1;
            background: var(--color-card-overlay-hover);
        }}

        .notification-dismiss-btn {{
            margin-left: 4px;
        }}

        /* Shared urgency styling (row + toast) */
        .notification-row.notification-critical,
        .notification-toast-critical {{
            border-left: 3px solid var(--color-state-warning);
        }}

        .notification-row.notification-critical {{
            background-color: var(--color-row-critical-background);
        }}

        .notification-toast-critical {{
            background-color: var(--color-toast-critical-background);
        }}

        .notification-row.notification-low {{
            opacity: 0.8;
        }}

        .notification-toast-low {{
            opacity: 0.9;
        }}

        /* === Popover-specific === */

        /* Note: padding comes from apply_surface_styles() in base.rs */
        .notification-popover {{
        }}

        .notification-header {{
            padding: 0 0 8px 0;
            margin: 0;
        }}

        .notification-clear-btn {{
            padding: 4px 8px;
            min-height: 0;
            border-radius: var(--radius-widget);
        }}

        .notification-clear-btn:hover {{
            background: var(--color-card-overlay-hover);
        }}

        .notification-clear-btn:active {{
            opacity: 0.7;
        }}

        .notification-clear-label {{
            font-size: var(--font-size-sm);
        }}

        .notification-list {{
            padding: 8px 0 0 0;
        }}

        /* Empty state */
        .notification-empty {{
            padding: 32px 16px;
        }}

        .notification-empty-label {{
            font-size: var(--font-size-sm);
        }}

        /* Notification row */
        .notification-row {{
            padding: 6px;
            margin-bottom: 4px;
        }}

        .notification-row:last-child {{
            margin-bottom: 0;
        }}

        .notification-timestamp {{
            font-size: var(--font-size-xs);
        }}

        /* Action buttons */
        .notification-actions {{
            margin-top: 6px;
        }}

        .notification-action-btn {{
            padding: 0;
            min-height: 0;
            min-width: 0;
            border-radius: var(--radius-widget);
        }}

        .notification-action-btn:hover {{
            background: var(--color-card-overlay-hover);
        }}

        .notification-action-btn > label {{
            font-size: var(--font-size-sm);
            padding: 2px 6px;
        }}

        /* === Toast-specific === */

        window.notification-toast,
        .notification-toast {{
            background: transparent;
        }}

        .notification-toast-container {{
            padding: 12px 14px;
            min-width: 300px;
        }}

        .notification-toast-actions {{
            margin-top: 10px;
            padding-top: 8px;
        }}

        .notification-toast-action {{
            font-size: var(--font-size-sm);
            padding: 4px 8px;
            min-height: 0;
            border-radius: var(--radius-widget);
        }}

        .notification-toast-action:hover {{
            background: var(--color-card-overlay-hover);
        }}

        /* ===== OSD ===== */

        /* Window must be transparent so container shows properly */
        .osd-window {{
            background: transparent;
        }}

        /* Container - tight padding for compact appearance */
        .osd-container {{
            border-radius: var(--radius-surface);
            padding: 12px 16px;
        }}

        /* Slider styling - slightly thicker for better visual weight */
        .osd-slider trough {{
            background-color: var(--color-slider-track);
            border-radius: var(--radius-pill);
            min-height: 10px;
            min-width: 10px;
        }}

        .osd-slider trough highlight {{
            background-color: var(--color-accent-slider, var(--color-accent-primary));
            border-radius: var(--radius-pill);
            min-height: 10px;
            min-width: 10px;
        }}

        /* Hide the slider knob/thumb */
        .osd-slider slider {{
            min-width: 0;
            min-height: 0;
            margin: 0;
            padding: 0;
            background: transparent;
            border: none;
            box-shadow: none;
        }}

        /* OSD unavailable state - colors via vp-muted */
        .osd-unavailable-icon {{
            opacity: 0.6;
        }}

        .osd-unavailable-label {{
            font-size: var(--font-size-sm);
        }}

        /* ===== QUICK SETTINGS AUDIO UNAVAILABLE ===== */

        /* Audio row disabled state - gray out everything */
        .qs-audio-row-disabled {{
            opacity: 0.5;
        }}

        .qs-audio-row-disabled .slider-icon-btn {{
            color: var(--color-foreground-muted);
        }}

        .qs-audio-row-disabled scale trough highlight {{
            background-color: var(--color-foreground-muted);
        }}

        /* Audio hint text - color via vp-muted */
        .qs-audio-hint {{
            font-size: var(--font-size-xs);
            font-style: italic;
            padding: 4px 0;
        }}

        /* ===== SYSTEM POPOVER ===== */

        .system-popover {{
            padding: 16px;
        }}

        /* Section cards */
        .system-section-card {{
            padding: 12px;
        }}

        /* Disable hover on non-interactive section cards */
        .system-section-card:hover {{
            background: var(--color-card-overlay);
        }}

        /* Section title with icon */
        .system-section-title {{
            margin-bottom: 4px;
        }}

        .system-section-icon {{
            font-size: 1.1em;
            opacity: 0.9;
        }}

        /* System progress bars - accent color fill */
        .system-progress-bar trough,
        .system-core-bar trough {{
            background-color: var(--color-slider-track);
            border-radius: var(--radius-pill);
            min-height: 6px;
        }}

        .system-progress-bar trough {{
            min-height: 8px;
        }}

        .system-progress-bar trough progress,
        .system-core-bar trough progress {{
            background-color: var(--color-accent-slider, var(--color-accent-primary));
            border-radius: var(--radius-pill);
            min-height: 6px;
        }}

        .system-progress-bar trough progress {{
            min-height: 8px;
        }}

        .system-expander-content {{
            padding: 12px;
            margin-top: 4px;
            background: var(--color-card-overlay);
            border-radius: var(--radius-widget);
        }}

        .system-expander-header {{
            margin-top: 4px;
        }}

        .system-expander-header:hover {{
            background: var(--color-card-overlay-hover);
            border-radius: var(--radius-widget);
        }}

        .system-network-icon {{
            font-size: 0.9em;
        }}

"#
    )
}
