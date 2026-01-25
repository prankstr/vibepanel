//! System tray CSS.

/// Return system tray CSS.
pub fn css() -> &'static str {
    r#"
/* ===== SYSTEM TRAY ===== */

/* Tray item hover - subtle scale up */
.tray-item {
    transition: transform 100ms ease-out;
}
.tray-item:hover,
.tray-item.tray-item-menu-open {
    transform: scale(1.15);
}

/* Ensure tray item images have no visual artifacts during updates */
.tray-item image,
.tray-item .icon-root,
.tray-item .icon-root image {
    border: none;
    box-shadow: none;
    outline: none;
    background: transparent;
}

.tray-menu {
    padding: 6px;
    font-family: var(--font-family);
    font-size: var(--font-size);
}

/* Row menu items - extends tray-menu-button pattern */
.qs-row-menu-item,
.tray-menu-button {
    background: transparent;
    border: none;
    box-shadow: none;
    padding: 4px 8px;
    border-radius: var(--radius-widget);
}

.qs-row-menu-item:hover,
.tray-menu-button:hover {
    background-color: var(--color-card-overlay-hover);
}

.tray-menu-button:disabled {
    opacity: 0.5;
}

.tray-menu-button:disabled:hover {
    background: transparent;
}
"#
}
