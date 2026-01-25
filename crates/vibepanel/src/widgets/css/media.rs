//! Media widget CSS.

/// Return media CSS.
pub fn css() -> &'static str {
    r#"
/* ===== MEDIA WIDGET ===== */

/* Album art thumbnail - RoundedPicture handles corner clipping via GSK */
.media-art-small {
    /* Size controlled via set_pixel_size() in Rust */
}

/* Add spacing after art only when followed by other elements */
.media-art-small:not(:last-child) {
    margin-right: 8px;
}

/* Player icon (app icon like Spotify, Firefox) in bar */
.media-player-icon {
    min-width: var(--icon-size);
    min-height: var(--icon-size);
}

/* Add spacing after icons only when followed by other elements */
.media-player-icon:not(:last-child) {
    margin-right: 8px;
}

.media-icon:not(:last-child) {
    margin-right: 4px;
}

/* Inline playback controls in bar */
.media .media-controls {
    margin-left: 4px;
}

.media-control-btn {
    min-width: 24px;
    min-height: 24px;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
}

.media-control-btn.media-control-btn-primary .icon-root {
    font-size: calc(var(--icon-size) * 1.1);
}

.media-control-btn:hover {
    background: var(--color-card-overlay-hover);
}

.media-label,
.media-title {
    font-size: var(--font-size);
}

/* Popover styling */
.media-popover {
    padding: 16px;
    min-width: 340px;
}

/* Popout button */
.media-popout-btn {
    min-width: 24px;
    min-height: 24px;
    padding: 0px;
    margin-top: -8px;
    margin-right: -8px;
}

/* Player selector button */
.media-player-selector-btn {
    min-width: 24px;
    min-height: 24px;
    padding: 0px;
    margin-top: -8px;
    margin-right: 4px;
}

/* Player selector menu - extends qs-row-menu-content */
.media-player-menu {
    font-family: var(--font-family);
    font-size: var(--font-size);
}

.media-player-menu * {
    font-family: inherit;
    font-size: inherit;
}

/* Player menu item - extends qs-row-menu-item */
.media-player-menu-item {
    border: none;
    outline: none;
    box-shadow: none;
}

.media-player-menu-title {
}

.media-player-menu-subtitle {
    font-size: var(--font-size-sm);
}

/* Check icon in player menu - slightly larger for visibility */
.media-player-menu-check {
    font-size: 1.15em;
}

/* Album art in popover/window */
.media-art {
    border-radius: var(--radius-widget);
    background: var(--color-card-overlay);
}

.media-art-placeholder {
    background: var(--color-card-overlay);
}

.media-empty-icon {
    font-size: 3em;
    opacity: 0.3;
}

.media-track-title {
    font-size: var(--font-size-lg);
    font-weight: 500;
}

.media-artist,
.media-album {
    font-size: var(--font-size-sm);
}

/* Playback controls in popover/window */
.media-popover .media-controls {
    padding: 0;
}

/* Window base styling */
.media-window {
    min-width: 280px;
}

.media-window .media-controls {
    padding: 8px 0;
}

.media-popover .media-control-btn,
.media-window .media-control-btn {
    background: transparent;
    border: none;
    box-shadow: none;
    min-width: 32px;
    min-height: 32px;
    padding: 0;
    border-radius: var(--radius-widget);
    color: var(--color-foreground-primary);
}

.media-popover .media-control-btn:hover,
.media-window .media-control-btn:hover {
    background: var(--color-card-overlay-hover);
}

/* Primary button (play/pause) - slightly larger with accent background */
.media-popover .media-control-btn.media-control-btn-primary,
.media-window .media-control-btn.media-control-btn-primary {
    min-width: 40px;
    min-height: 40px;
    background: var(--color-accent-primary);
    color: var(--color-accent-text, #fff);
}

.media-popover .media-control-btn.media-control-btn-primary:hover,
.media-window .media-control-btn.media-control-btn-primary:hover {
    opacity: 0.85;
    background: var(--color-accent-primary);
}

/* Seek bar */
.media-seek {
    margin-top: 4px;
}

.media-seek-slider {
    margin-left: -8px;
    margin-right: -8px;
}

.media-seek-slider trough {
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
    background-color: var(--color-slider-track);
}

.media-seek-slider highlight {
    background-image: image(var(--color-accent-slider, var(--color-accent-primary)));
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border: none;
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
}

.media-seek-slider slider {
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

.media-seek-slider slider:active {
    transform: scale(1.15);
}

.media-time {
    font-size: var(--font-size-xs);
    margin-top: -4px;
}

/* Volume control (used in media window) */
.media-volume {
    padding-top: 8px;
}

.media-volume-slider {
    margin-left: 8px;
}

.media-volume-slider trough {
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
    background-color: var(--color-slider-track);
}

.media-volume-slider highlight {
    background-image: image(var(--color-accent-slider, var(--color-accent-primary)));
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border: none;
    min-height: var(--slider-height);
    border-radius: var(--radius-track);
}

.media-volume-slider slider {
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

.media-volume-slider slider:active {
    transform: scale(1.15);
}

/* Window header buttons (dock/close) */
button.media-window-dock,
button.media-window-close {
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    border-radius: var(--radius-widget);
    background: transparent;
    border: none;
    box-shadow: none;
}

button.media-window-dock:hover,
button.media-window-close:hover {
    background: var(--color-card-overlay-hover);
}

/* Window-specific smaller controls */
.media-window .media-window-control-btn {
    min-width: 24px;
    min-height: 24px;
}

.media-window .media-window-control-btn.media-control-btn-primary {
    min-width: 32px;
    min-height: 32px;
}

/* Window-specific thinner seek slider */
.media-window .media-window-seek-slider trough {
    min-height: 4px;
}

.media-window .media-window-seek-slider highlight {
    min-height: 4px;
}

.media-window .media-window-seek-slider slider {
    min-width: 12px;
    min-height: 12px;
    margin: -4px;
}
"#
}
