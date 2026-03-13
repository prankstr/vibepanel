//! Notification widget CSS.

use super::DISMISS_ANIMATION_MS;

/// Return notifications CSS.
pub fn css(animations: bool) -> String {
    let row_transition = if animations {
        format!(
            "transition: opacity {ms}ms ease;",
            ms = DISMISS_ANIMATION_MS,
        )
    } else {
        "transition: none;".to_string()
    };

    format!(
        r#"
/* ===== NOTIFICATIONS ===== */
/* Shared styles for both popover rows and toasts */

/* Bell icon states */
.notification-icon.has-critical {{
    color: var(--color-state-warning);
}}

.notification-icon.backend-unavailable {{
    color: var(--color-foreground-disabled);
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
    border-radius: var(--radius-round);
    background-color: var(--color-accent-primary);
}}

/* Shared icon styling (row + toast) */
.notification-row-icon,
.notification-toast-icon {{
    margin-top: 2px;
    min-width: 48px;
    min-height: 48px;
    border-radius: var(--radius-round);
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
    min-width: 20px;
    min-height: 20px;
    padding: 0;
    opacity: 0.7;
    border-radius: var(--radius-round);
}}

.notification-dismiss-btn:hover,
.notification-toast-dismiss:hover {{
    opacity: 1;
    background: var(--color-card-overlay-hover);
}}

.notification-dismiss-btn {{
    margin-left: 4px;
    margin-top: -3px;
    margin-right: -3px;
}}

.notification-toast-dismiss {{
    margin-top: -3px;
    margin-right: -3px;
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

/* Header icon sizing */
.notification-header-icon {{
    font-size: calc(var(--icon-size) * 1.15);
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

/* Notification row (spacing between rows handled by GtkBox) */
.notification-row {{
    padding: 6px;
    border-radius: var(--radius-pill);
    {row_transition}
}}

/* Dismiss animation: fade out (height collapse handled by Revealer) */
.notification-row.notification-row-dismissing {{
    opacity: 0;
}}

.notification-timestamp {{
    font-size: var(--font-size-xs);
}}

/* Action buttons */
.notification-actions {{
    margin-top: 6px;
}}

button.notification-action-btn {{
    padding: 0;
    min-height: 0;
    min-width: 0;
    border-radius: var(--radius-widget);
    color: var(--color-accent-primary);
}}

button.notification-action-btn label {{
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

button.notification-toast-action {{
    min-height: 0;
    border-radius: var(--radius-widget);
    color: var(--color-accent-primary);
}}

button.notification-toast-action label {{
    font-size: var(--font-size-sm);
    padding: 4px 8px;
}}
"#
    )
}
