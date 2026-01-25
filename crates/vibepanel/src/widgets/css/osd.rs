//! OSD (On-Screen Display) CSS.

/// Return OSD CSS.
pub fn css() -> &'static str {
    r#"
/* ===== OSD ===== */

/* Window must be transparent so container shows properly */
.osd-window {
    background: transparent;
}

/* Container - tight padding for compact appearance */
/* Note: border-radius set via apply_surface_styles_with_radius() */
.osd-container {
    padding: 12px 16px;
}

/* Slider styling - slightly thicker for better visual weight */
.osd-slider trough {
    background-color: var(--color-slider-track);
    border-radius: var(--radius-track-thick);
    min-height: 10px;
    min-width: 10px;
}

.osd-slider trough highlight {
    background-color: var(--color-accent-slider, var(--color-accent-primary));
    border-radius: var(--radius-track-thick);
    min-height: 10px;
    min-width: 10px;
}

/* Hide the slider knob/thumb */
.osd-slider slider {
    min-width: 0;
    min-height: 0;
    margin: 0;
    padding: 0;
    background: transparent;
    border: none;
    box-shadow: none;
}

/* OSD unavailable state - colors via vp-muted */
.osd-unavailable-icon {
    opacity: 0.6;
}

.osd-unavailable-label {
    font-size: var(--font-size-sm);
}
"#
}
