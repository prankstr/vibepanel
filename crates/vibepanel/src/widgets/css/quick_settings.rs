//! Quick Settings CSS.

/// Return quick settings CSS.
pub fn css() -> &'static str {
    r#"
/* ===== QUICK SETTINGS ===== */

/* Window transparency */
window.quick-settings-window {
    background: transparent;
}

/* QS window container - extra top padding to compensate for 0 top margin
   (top margin must be 0 for correct popover_offset positioning) */
.qs-window-container {
    padding-top: 4px;
}

/* Click catcher overlay */
.vp-click-catcher {
    background: var(--color-click-catcher-overlay);
}

/* Cards */
.vp-card {
    background: var(--color-card-overlay);
    border-radius: var(--radius-widget);
    /* No padding here - children handle their own padding for better click targets */
}

/* Card hover state */
.vp-card:hover,
.qs-row:hover {
    background: var(--color-card-overlay-hover);
}

.vp-card.qs-card-disabled:hover {
    background: var(--color-card-overlay);
}

/* Toggle button fills card and provides its own padding */
.vp-card > .vp-btn-reset {
    padding: 8px 10px;
}

/* Expander chevron padding */
.vp-card > .qs-toggle-more {
    margin-right: 8px;
}

/* Toggle card icon spacing */
.qs-toggle-icon {
    margin-left: 2px;
    margin-right: 4px;
    font-size: calc(var(--icon-size) * 1.15);
}

/* Row icon spacing */
.qs-row-icon {
    margin-left: 1px;
    margin-right: 3px;
    font-size: calc(var(--icon-size) * 0.9);
}

/* Wi-Fi disabled state override */
.qs-wifi-disabled-icon {
    color: var(--color-foreground-muted);
    opacity: 0.5;
}

/* Ethernet section in expanded details (above Wi-Fi controls) */
.qs-ethernet-section {
    /* Container for header + connection row */
    padding-top: 6px;
}

.qs-ethernet-section .qs-ethernet-connection-row {
    /* Override .qs-row margin, keep horizontal margin for alignment */
    margin-top: 8px;
    margin-bottom: 0;
    margin-left: 0;
    margin-right: 0;
}

/* Wi-Fi switch row in expanded details */
.qs-wifi-switch-row {
    padding: 0 8px;
    margin-top: 8px;
    margin-bottom: -4px;
}

.qs-wifi-switch-label {
    font-size: var(--font-size);
}

/* Wi-Fi switch styling - accent colored track when on */
.qs-wifi-switch-row switch {
    /* Switch track: rounder than slider to contain it */
    border-radius: calc(var(--radius-pill) * 1.2);
    margin-top: 2px;
}

.qs-wifi-switch-row switch:checked {
    background-color: var(--color-accent-primary);
    background-image: none;
}

.qs-wifi-switch-row switch:checked:backdrop {
    background-color: var(--color-accent-primary);
}

.qs-wifi-switch-row switch slider {
    border-radius: var(--radius-track-thick);
    min-width: 12px;
    min-height: 12px;
}

/* Bluetooth controls row in expanded details */
.qs-bt-controls-row {
    padding: 0 8px;
    margin-top: 8px;
    margin-bottom: -4px;
}

/* Network empty state (no connections) */
.qs-no-connections-state {
    padding: 24px 16px;
}

.qs-no-connections-icon {
    font-size: 32px;
    opacity: 0.5;
}

.qs-no-connections-label {
    font-size: var(--font-size-sm);
}

.qs-wifi-disabled-state {
    padding: 16px;
}

.qs-wifi-disabled-state-icon {
    font-size: 28px;
    opacity: 0.4;
}

.qs-wifi-disabled-label {
    font-size: var(--font-size-sm);
}

/* Generic disabled state placeholder (used by Bluetooth, etc.) */
.qs-disabled-state {
    padding: 16px;
}

.qs-disabled-state-icon {
    font-size: 28px;
    opacity: 0.4;
}

.qs-disabled-state-label {
    font-size: var(--font-size-sm);
}

/* Reset styling for QS buttons - extends vp-btn-reset */
.qs-toggle-more,
.qs-scan-button {
    background: transparent;
    border: none;
    box-shadow: none;
}

/* Expander chevron button */
.qs-toggle-more {
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
}

.qs-toggle-more:hover {
    background: var(--color-card-overlay-hover);
}

/* List items */
.qs-list {
    background: transparent;
}

.qs-row {
    background: var(--color-card-overlay);
    border-radius: var(--radius-widget);
    padding: 6px 10px;
    margin: 3px 0;
}

/* Updates list rows use smaller radius for larger card surfaces */
.qs-updates-list .qs-row {
    border-radius: var(--radius-pill);
}

/* Row menu content */
.qs-row-menu-content {
    font-family: var(--font-family);
    font-size: var(--font-size);
    border-radius: var(--radius-surface);
}

/* Row hamburger menu button */
.qs-row-menu-button {
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
}

.qs-row-menu-button:hover {
    background: var(--color-card-overlay-hover);
}

/* Accent colors - state override for active icons/toggles */
.qs-icon-active {
    color: var(--color-accent-primary);
}

/* Row titles - color via vp-primary */
.qs-row-title {
    font-size: var(--font-size-md);
    margin-top: 1px;
}

/* Row action labels - color via vp-accent */
.qs-row-action-label {
    font-size: var(--font-size-sm);
}

.qs-row-action-label:hover {
    background: var(--color-card-overlay-hover);
    border-radius: var(--radius-widget);
}

/* Subtitles - secondary info, color via vp-muted */
.qs-toggle-subtitle,
.qs-row-subtitle {
    font-size: var(--font-size-sm);
}

/* Accent color state override for active subtitles */
.qs-subtitle-active {
    color: var(--color-accent-primary);
}

/* Radio indicator for unselected audio/mic device rows.
 * Replaces the radio-symbolic icon so the shape scales with --radius-pill
 * (square at border_radius: 0, circular at border_radius: 50). */
.qs-radio-indicator {
    min-width: 12px;
    min-height: 12px;
    border: 1.5px solid var(--color-foreground-primary);
    /* Nearly-pill but subtly softer for visual distinction */
    border-radius: calc(var(--radius-pill) * 0.9);
    opacity: 0.6;
    margin: 2px 0;
}

/* Checkmark indicator background for selected audio/mic device rows */
.qs-row-indicator-bg {
    border-radius: var(--radius-pill);
    min-width: 16px;
    min-height: 16px;
}

/* Checkmark icon for selected state - floats above background */
.qs-row-indicator {
    color: var(--color-accent-primary);
    font-variation-settings: 'wght' 600;
    font-size: 20px;
}

.qs-scan-button {
    padding: 2px 8px;
    margin-bottom: 4px;
    min-height: 0;
    /* Extra-round pill shape for small action button */
    border-radius: calc(var(--radius-pill) * 1.3);
}

.qs-scan-button:hover {
    background: var(--color-card-overlay-hover);
}

/* Scan spinner - small inline spinner */
.qs-scan-spinner {
    min-width: 12px;
    min-height: 12px;
}

/* Chevron animation */
.qs-toggle-more-icon {
    transition: transform 200ms ease;
    font-size: calc(var(--icon-size) * 1.1);
    font-variation-settings: 'wght' 500;
    -gtk-icon-style: symbolic;
    margin-top: 2px;
}

.qs-toggle-more-icon.expanded {
    margin-top: -2px;
    transform: rotate(180deg);
}

/* Power card hold-to-confirm progress */
.qs-power-progress {
    background-color: transparent;
    min-width: 0;
    border-radius: var(--radius-widget);
}

.qs-power-progress.qs-power-confirming {
    background-color: var(--color-accent-primary);
}

/* Power action rows - remove padding since overlay content provides it */
.qs-power-row {
    padding: 0;
}

/* Power row content - needs padding since it's an overlay above progress */
.qs-power-row-content {
    padding: 6px 10px;
}

/* Power details container - add spacing from toggle card */
.qs-power-details {
    margin-top: 6px;
}

/* Progress bar inside power rows */
.qs-power-row .qs-power-progress {
    border-radius: var(--radius-widget);
}

/* ===== BLUETOOTH AUTH PROMPT ===== */

/* Auth prompt container - inline under device row */
.qs-bt-auth-prompt {
    padding: 8px 10px;
    margin: 0;
}

/* Auth prompt instruction label (direct child only, not button labels) */
.qs-bt-auth-prompt > label {
    font-size: var(--font-size);
    margin-bottom: 8px;
    color: var(--color-foreground-primary);
}

/* Character box container - horizontal layout for PIN/passkey digits */
.qs-bt-char-container {
    margin-bottom: 8px;
}

/* Individual character entry boxes - square with rounded corners */
.qs-bt-char-box {
    min-width: 36px;
    min-height: 0;
    padding: 8px 0;
    border-radius: var(--radius-widget);
    font-size: calc(var(--font-size) * 1.1);
    background: var(--color-card-overlay);
    border: 1px solid var(--color-border, rgba(255,255,255,0.1));
    margin: 0 2px;
    color: var(--color-foreground-primary);
}

.qs-bt-char-box:focus {
    border-color: var(--color-accent-primary);
    outline: none;
}

/* Read-only character boxes (for confirmation/display modes) */
.qs-bt-char-box:disabled {
    background: var(--color-card-overlay);
    color: var(--color-foreground-primary);
    opacity: 1;
}

/* Auth prompt button row */
.qs-bt-auth-buttons {
    margin-top: 4px;
}

/* ===== QUICK SETTINGS AUDIO UNAVAILABLE ===== */

/* Audio row disabled state - gray out everything */
.qs-audio-row-disabled {
    opacity: 0.5;
}

.qs-audio-row-disabled .slider-icon-btn {
    color: var(--color-foreground-muted);
}

.qs-audio-row-disabled scale trough highlight {
    background-color: var(--color-foreground-muted);
}

/* Audio hint text - color via vp-muted */
.qs-audio-hint {
    font-size: var(--font-size-xs);
    font-style: italic;
    padding: 4px 0;
}

/* ===== MARQUEE LABEL ===== */

/* Note: Overflow is handled by the GtkBox widget with set_overflow(Hidden),
   not CSS. Text wrapping is controlled via Label properties in Rust code. */
"#
}
