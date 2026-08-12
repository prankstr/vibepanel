//! Mango layout widget and chooser CSS.

pub fn css() -> &'static str {
    r#"
.mango-layout-popover {
    min-width: 292px;
}

.mango-layout-grid {
    margin-top: 2px;
}

button.mango-layout-tile {
    min-width: 88px;
    min-height: 62px;
    padding: 4px 6px;
    border-radius: var(--radius-widget-lg);
}

.mango-layout-tile-icon {
    font-size: 32px;
}

.mango-layout-mirror-horizontal {
    transform: scaleX(-1);
}

.mango-layout-rotate-positive-90 {
    transform: rotate(90deg);
}

.mango-layout-rotate-negative-90 {
    transform: rotate(-90deg);
}

.mango-layout-tile-label {
    font-size: 0.9em;
}

button.mango-layout-selected {
    font-weight: 600;
}
"#
}
