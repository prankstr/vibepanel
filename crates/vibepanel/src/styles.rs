//! Shared CSS class constants for vibepanel.
//!
//! This module centralizes all CSS class names used across the codebase,
//! making them discoverable, avoiding typos, and enabling IDE autocompletion.
//!
//! # Usage
//!
//! ```ignore
//! use crate::styles::{class, button, card, color, state};
//!
//! widget.add_css_class(class::WIDGET);
//! button.add_css_class(button::RESET);
//! icon.add_css_class(color::PRIMARY);
//! ```

/// Core structural/layout CSS classes.
pub mod class {
    /// Base widget container class (`.widget`).
    pub const WIDGET: &str = "widget";

    /// Widget item class (`.widget-item`).
    /// Applied to individual widget containers. Remains on widgets even when grouped,
    /// enabling per-widget hover effects within groups.
    pub const WIDGET_ITEM: &str = "widget-item";

    /// Widget group container class (`.widget-group`).
    /// Applied to shared island containers that hold multiple grouped widgets.
    pub const WIDGET_GROUP: &str = "widget-group";

    /// Widget content inner box (`.content`).
    pub const CONTENT: &str = "content";

    /// Vertical center with caps alignment (`.vcenter-caps`).
    pub const VCENTER_CAPS: &str = "vcenter-caps";

    /// Bar window class (`.bar-window`).
    pub const BAR_WINDOW: &str = "bar-window";

    /// Bar shell class (`.bar-shell`).
    pub const BAR_SHELL: &str = "bar-shell";

    /// Bar shell inner class (`.bar-shell-inner`).
    pub const BAR_SHELL_INNER: &str = "bar-shell-inner";

    /// Bar margin spacer (`.bar-margin-spacer`).
    pub const BAR_MARGIN_SPACER: &str = "bar-margin-spacer";

    /// Main bar class (`.bar`).
    pub const BAR: &str = "bar";

    /// Sectioned bar widget CSS name (`sectioned-bar`).
    pub const SECTIONED_BAR: &str = "sectioned-bar";

    /// Click catcher overlay (`.vp-click-catcher`).
    pub const CLICK_CATCHER: &str = "vp-click-catcher";

    // Bar sections
    /// Bar section left (`.bar-section--left`).
    pub const BAR_SECTION_LEFT: &str = "bar-section--left";

    /// Bar section right (`.bar-section--right`).
    pub const BAR_SECTION_RIGHT: &str = "bar-section--right";

    /// Bar section center (`.bar-section--center`).
    pub const BAR_SECTION_CENTER: &str = "bar-section--center";
}

/// Foreground/text color classes.
///
/// These apply `color: var(--color-foreground-*)` to text and icons.
pub mod color {
    /// Primary foreground color (`.vp-primary`).
    pub const PRIMARY: &str = "vp-primary";

    /// Muted/secondary foreground color (`.vp-muted`).
    pub const MUTED: &str = "vp-muted";

    /// Disabled/tertiary foreground color (`.vp-disabled`).
    pub const DISABLED: &str = "vp-disabled";

    /// Faint foreground color for very subtle decorative elements (`.vp-faint`).
    pub const FAINT: &str = "vp-faint";

    /// Accent color (`.vp-accent`).
    pub const ACCENT: &str = "vp-accent";

    /// Error/urgent color (`.vp-error`).
    pub const ERROR: &str = "vp-error";

    /// Generic text class (`.text`).
    pub const TEXT: &str = "text";
}

/// Button style classes.
pub mod button {
    /// Reset button - strips all GTK chrome (`.vp-btn-reset`).
    ///
    /// Use for buttons that need custom styling without default backgrounds,
    /// borders, shadows, or padding.
    pub const RESET: &str = "vp-btn-reset";

    /// Compact button - minimal chrome for icon-only buttons (`.vp-btn-compact`).
    ///
    /// Similar to RESET but specifically for icon buttons that need
    /// zero padding and minimum size.
    pub const COMPACT: &str = "vp-btn-compact";

    /// Accent-styled button with filled background (`.vp-btn-accent`).
    pub const ACCENT: &str = "vp-btn-accent";

    /// Card-styled button with subtle background (`.vp-btn-card`).
    pub const CARD: &str = "vp-btn-card";

    /// Link-styled button - text only, no background (`.vp-btn-link`).
    pub const LINK: &str = "vp-btn-link";

    /// Ghost button - transparent with hover effect (`.vp-btn-ghost`).
    pub const GHOST: &str = "vp-btn-ghost";
}

/// Card and container classes.
pub mod card {
    /// Base card container (`.vp-card`).
    ///
    /// Provides card overlay background and border-radius.
    pub const BASE: &str = "vp-card";

    /// Quick Settings card (`.qs-card`).
    ///
    /// Used on QS toggle cards (Wi-Fi, Bluetooth, VPN, etc.).
    pub const QS: &str = "qs-card";
}

/// List row classes.
pub mod row {
    /// Base row class (`.vp-row`).
    pub const BASE: &str = "vp-row";

    /// Quick Settings row (`.qs-row`).
    pub const QS: &str = "qs-row";

    /// Row content container (`.qs-row-content`).
    pub const QS_CONTENT: &str = "qs-row-content";

    /// Row title label (`.qs-row-title`).
    pub const QS_TITLE: &str = "qs-row-title";

    /// Row subtitle label (`.qs-row-subtitle`).
    pub const QS_SUBTITLE: &str = "qs-row-subtitle";

    /// Row icon class (`.qs-row-icon`).
    pub const QS_ICON: &str = "qs-row-icon";

    /// Row action label (`.qs-row-action-label`).
    pub const QS_ACTION_LABEL: &str = "qs-row-action-label";

    /// Row menu button (`.qs-row-menu-button`).
    pub const QS_MENU_BUTTON: &str = "qs-row-menu-button";

    /// Row menu icon (`.qs-row-menu-icon`).
    pub const QS_MENU_ICON: &str = "qs-row-menu-icon";

    /// Row indicator icon (`.qs-row-indicator`).
    pub const QS_INDICATOR: &str = "qs-row-indicator";

    /// Row indicator background box (`.qs-row-indicator-bg`).
    ///
    /// Background box behind the checkmark icon for selected state.
    pub const QS_INDICATOR_BG: &str = "qs-row-indicator-bg";

    /// Radio indicator box for unselected state (`.qs-radio-indicator`).
    ///
    /// A CSS-styled box that respects `--radius-pill` for configurable corner radius.
    /// Used instead of the `radio-symbolic` icon so the shape scales with the user's
    /// border radius setting (square at 0%, circular at 50%).
    pub const QS_RADIO_INDICATOR: &str = "qs-radio-indicator";
}

/// State/toggle classes for dynamic styling.
pub mod state {
    /// Active icon state (`.qs-icon-active`).
    ///
    /// Applied to icons when their associated feature is enabled/active.
    /// Changes color to accent.
    pub const ICON_ACTIVE: &str = "qs-icon-active";

    /// Active subtitle state (`.qs-subtitle-active`).
    ///
    /// Applied to subtitles when showing active connection info.
    /// Changes color to accent.
    pub const SUBTITLE_ACTIVE: &str = "qs-subtitle-active";

    /// Expanded state for accordions/chevrons (`.expanded`).
    pub const EXPANDED: &str = "expanded";

    /// Muted/disabled audio state (`.muted`).
    pub const MUTED: &str = "muted";

    /// Service unavailable state (`.service-unavailable`).
    pub const SERVICE_UNAVAILABLE: &str = "service-unavailable";

    /// Clickable element (`.clickable`).
    pub const CLICKABLE: &str = "clickable";

    /// Occupied workspace state (`.occupied`).
    pub const OCCUPIED: &str = "occupied";

    /// Urgent workspace state (`.urgent`).
    pub const URGENT: &str = "urgent";

    /// Spinning/loading animation state (`.spinning`).
    pub const SPINNING: &str = "spinning";
}

/// Quick Settings specific component classes.
pub mod qs {
    // Card identifiers (for per-card CSS targeting)
    /// Wi-Fi toggle card (`.qs-wifi`).
    pub const WIFI: &str = "qs-wifi";

    /// Bluetooth toggle card (`.qs-bluetooth`).
    pub const BLUETOOTH: &str = "qs-bluetooth";

    /// VPN toggle card (`.qs-vpn`).
    pub const VPN: &str = "qs-vpn";

    /// Updates toggle card (`.qs-updates`).
    pub const UPDATES: &str = "qs-updates";

    /// Idle inhibitor toggle card (`.qs-idle-inhibitor`).
    pub const IDLE_INHIBITOR: &str = "qs-idle-inhibitor";

    // Slider row identifiers (for per-row CSS targeting)
    /// Audio output slider row (`.qs-audio-output`).
    pub const AUDIO_OUTPUT: &str = "qs-audio-output";

    /// Microphone input slider row (`.qs-audio-mic`).
    pub const AUDIO_MIC: &str = "qs-audio-mic";

    /// Brightness slider row (`.qs-brightness`).
    pub const BRIGHTNESS: &str = "qs-brightness";

    // Window
    /// Quick Settings window (`.quick-settings-window`).
    pub const WINDOW: &str = "quick-settings-window";

    /// Window container (`.qs-window-container`).
    pub const WINDOW_CONTAINER: &str = "qs-window-container";

    /// Control center content (`.qs-control-center`).
    pub const CONTROL_CENTER: &str = "qs-control-center";

    /// Cards row (`.qs-cards-row`).
    pub const CARDS_ROW: &str = "qs-cards-row";

    /// Click catcher (`.qs-click-catcher`).
    pub const CLICK_CATCHER: &str = "qs-click-catcher";

    // Toggle components
    /// Toggle icon (`.qs-toggle-icon`).
    pub const TOGGLE_ICON: &str = "qs-toggle-icon";

    /// Toggle label (`.qs-toggle-label`).
    pub const TOGGLE_LABEL: &str = "qs-toggle-label";

    /// Toggle subtitle (`.qs-toggle-subtitle`).
    pub const TOGGLE_SUBTITLE: &str = "qs-toggle-subtitle";

    /// Expander button (`.qs-toggle-more`).
    pub const TOGGLE_MORE: &str = "qs-toggle-more";

    /// Expander chevron icon (`.qs-toggle-more-icon`).
    pub const TOGGLE_MORE_ICON: &str = "qs-toggle-more-icon";

    /// List container (`.qs-list`).
    pub const LIST: &str = "qs-list";

    /// Scan/refresh button (`.qs-scan-button`).
    pub const SCAN_BUTTON: &str = "qs-scan-button";

    /// Scan label (`.qs-scan-label`).
    pub const SCAN_LABEL: &str = "qs-scan-label";

    /// Scan spinner (`.qs-scan-spinner`).
    pub const SCAN_SPINNER: &str = "qs-scan-spinner";

    /// Wi-Fi switch row container (`.qs-wifi-switch-row`).
    pub const WIFI_SWITCH_ROW: &str = "qs-wifi-switch-row";

    /// Wi-Fi switch label (`.qs-wifi-switch-label`).
    pub const WIFI_SWITCH_LABEL: &str = "qs-wifi-switch-label";

    /// Ethernet section container in expanded details (`.qs-ethernet-section`).
    pub const ETHERNET_ROW_CONTAINER: &str = "qs-ethernet-section";

    /// Ethernet connection row with background (`.qs-ethernet-connection-row`).
    pub const ETHERNET_CONNECTION_ROW: &str = "qs-ethernet-connection-row";

    /// Network empty state container (`.qs-no-connections-state`).
    pub const NO_CONNECTIONS_STATE: &str = "qs-no-connections-state";

    /// Network empty state icon (`.qs-no-connections-icon`).
    pub const NO_CONNECTIONS_ICON: &str = "qs-no-connections-icon";

    /// Network empty state label (`.qs-no-connections-label`).
    pub const NO_CONNECTIONS_LABEL: &str = "qs-no-connections-label";

    /// Wi-Fi disabled icon state (`.qs-wifi-disabled-icon`).
    pub const WIFI_DISABLED_ICON: &str = "qs-wifi-disabled-icon";

    /// Bluetooth disabled icon state (`.qs-bt-disabled-icon`).
    pub const BT_DISABLED_ICON: &str = "qs-bt-disabled-icon";

    /// Wi-Fi disabled state container (`.qs-wifi-disabled-state`).
    pub const WIFI_DISABLED_STATE: &str = "qs-wifi-disabled-state";

    /// Wi-Fi disabled state icon (`.qs-wifi-disabled-state-icon`).
    pub const WIFI_DISABLED_STATE_ICON: &str = "qs-wifi-disabled-state-icon";

    /// Wi-Fi disabled state label (`.qs-wifi-disabled-label`).
    pub const WIFI_DISABLED_LABEL: &str = "qs-wifi-disabled-label";

    /// Generic disabled state container (`.qs-disabled-state`).
    pub const DISABLED_STATE: &str = "qs-disabled-state";

    /// Generic disabled state icon (`.qs-disabled-state-icon`).
    pub const DISABLED_STATE_ICON: &str = "qs-disabled-state-icon";

    /// Generic disabled state label (`.qs-disabled-state-label`).
    pub const DISABLED_STATE_LABEL: &str = "qs-disabled-state-label";

    /// Muted placeholder label (`.qs-muted`).
    pub const MUTED_LABEL: &str = "qs-muted";

    /// Audio row disabled state (`.qs-audio-row-disabled`).
    pub const AUDIO_ROW_DISABLED: &str = "qs-audio-row-disabled";

    /// Card disabled state - suppresses hover effect (`.qs-card-disabled`).
    pub const CARD_DISABLED: &str = "qs-card-disabled";

    /// Audio hint text (`.qs-audio-hint`).
    pub const AUDIO_HINT: &str = "qs-audio-hint";

    /// Audio details container (`.qs-audio-details`).
    pub const AUDIO_DETAILS: &str = "qs-audio-details";

    /// Section header (`.qs-section-header`).
    pub const SECTION_HEADER: &str = "qs-section-header";

    // Updates card
    /// Updates details container (`.qs-updates-details`).
    pub const UPDATES_DETAILS: &str = "qs-updates-details";

    /// Updates last check label (`.qs-updates-last-check`).
    pub const UPDATES_LAST_CHECK: &str = "qs-updates-last-check";

    /// Updates scroll container (`.qs-updates-scroll`).
    pub const UPDATES_SCROLL: &str = "qs-updates-scroll";

    /// Updates list container (`.qs-updates-list`).
    pub const UPDATES_LIST: &str = "qs-updates-list";

    /// Updates error row (`.qs-updates-error`).
    pub const UPDATES_ERROR: &str = "qs-updates-error";

    // Wi-Fi card
    /// Wi-Fi network row (`.qs-wifi-row`).
    pub const WIFI_ROW: &str = "qs-wifi-row";

    /// Wi-Fi base signal icon (dimmed, full bars) (`.qs-wifi-base`).
    pub const WIFI_BASE: &str = "qs-wifi-base";

    /// Wi-Fi overlay signal icon (highlighted, actual bars) (`.qs-wifi-overlay`).
    pub const WIFI_OVERLAY: &str = "qs-wifi-overlay";

    /// Row menu content container (`.qs-row-menu-content`).
    pub const ROW_MENU_CONTENT: &str = "qs-row-menu-content";

    /// Row menu item button (`.qs-row-menu-item`).
    pub const ROW_MENU_ITEM: &str = "qs-row-menu-item";

    /// VPN row (`.qs-vpn-row`).
    pub const VPN_ROW: &str = "qs-vpn-row";

    /// Bluetooth row (`.qs-bt-row`).
    pub const BT_ROW: &str = "qs-bt-row";

    /// Bluetooth controls row (`.qs-bt-controls-row`).
    pub const BT_CONTROLS_ROW: &str = "qs-bt-controls-row";

    /// Bluetooth auth prompt container (`.qs-bt-auth-prompt`).
    pub const BT_AUTH_PROMPT: &str = "qs-bt-auth-prompt";

    /// Bluetooth auth character box container (`.qs-bt-char-container`).
    pub const BT_CHAR_CONTAINER: &str = "qs-bt-char-container";

    /// Individual character entry box (`.qs-bt-char-box`).
    pub const BT_CHAR_BOX: &str = "qs-bt-char-box";

    /// Bluetooth auth button row (`.qs-bt-auth-buttons`).
    pub const BT_AUTH_BUTTONS: &str = "qs-bt-auth-buttons";

    // Power card
    /// Power card container (`.qs-power-card`).
    pub const POWER_CARD: &str = "qs-power-card";

    /// Power progress overlay (`.qs-power-progress`).
    pub const POWER_PROGRESS: &str = "qs-power-progress";

    /// Power progress confirming state (`.qs-power-confirming`).
    pub const POWER_CONFIRMING: &str = "qs-power-confirming";

    /// Power action row (`.qs-power-row`).
    pub const POWER_ROW: &str = "qs-power-row";

    /// Power action row content (`.qs-power-row-content`).
    pub const POWER_ROW_CONTENT: &str = "qs-power-row-content";

    /// Power details container (`.qs-power-details`).
    pub const POWER_DETAILS: &str = "qs-power-details";
}

/// Widget-specific CSS classes.
pub mod widget {
    // Spacer
    /// Spacer widget (`.spacer`).
    pub const SPACER: &str = "spacer";

    // Clock
    /// Clock widget (`.clock`).
    pub const CLOCK: &str = "clock";

    /// Clock label (`.clock-label`).
    pub const CLOCK_LABEL: &str = "clock-label";

    // Battery
    /// Battery widget (`.battery`).
    pub const BATTERY: &str = "battery";

    /// Battery percentage label (`.battery-percentage`).
    pub const BATTERY_PERCENTAGE: &str = "battery-percentage";

    // Workspaces
    /// Workspaces widget (`.workspaces`).
    pub const WORKSPACES: &str = "workspaces";

    /// Workspace indicator (`.workspace-indicator`).
    pub const WORKSPACE_INDICATOR: &str = "workspace-indicator";

    /// Workspace indicator minimal style (`.workspace-indicator-minimal`).
    pub const WORKSPACE_INDICATOR_MINIMAL: &str = "workspace-indicator-minimal";

    /// Workspace separator (`.workspace-separator`).
    pub const WORKSPACE_SEPARATOR: &str = "workspace-separator";

    /// Active workspace (`.active`).
    pub const ACTIVE: &str = "active";

    // System tray
    /// System tray widget (`.tray`).
    pub const TRAY: &str = "tray";

    /// Tray item button (`.tray-item`).
    pub const TRAY_ITEM: &str = "tray-item";

    /// Tray item with menu open - keeps icon enlarged (`.tray-item-menu-open`).
    pub const TRAY_ITEM_MENU_OPEN: &str = "tray-item-menu-open";

    /// Tray menu container (`.tray-menu`).
    pub const TRAY_MENU: &str = "tray-menu";

    /// Tray menu button (`.tray-menu-button`).
    pub const TRAY_MENU_BUTTON: &str = "tray-menu-button";

    /// Tray menu back button (`.tray-menu-back`).
    pub const TRAY_MENU_BACK: &str = "tray-menu-back";

    /// Tray menu submenu indicator (`.tray-menu-submenu`).
    pub const TRAY_MENU_SUBMENU: &str = "tray-menu-submenu";

    // Battery
    /// Battery icon (`.battery-icon`).
    pub const BATTERY_ICON: &str = "battery-icon";

    /// Battery charging state (`.battery-charging`).
    pub const BATTERY_CHARGING: &str = "battery-charging";

    /// Battery low state (`.battery-low`).
    pub const BATTERY_LOW: &str = "battery-low";

    // Notifications
    /// Notifications widget (`.notifications`).
    pub const NOTIFICATIONS: &str = "notifications";

    /// Notification icon (`.notification-icon`).
    pub const NOTIFICATION_ICON: &str = "notification-icon";

    /// Has critical notifications (`.has-critical`).
    pub const HAS_CRITICAL: &str = "has-critical";

    /// Backend unavailable state (`.backend-unavailable`).
    pub const BACKEND_UNAVAILABLE: &str = "backend-unavailable";

    /// Notification badge container (`.notification-badge`).
    pub const NOTIFICATION_BADGE: &str = "notification-badge";

    /// Notification badge dot (`.notification-badge-dot`).
    pub const NOTIFICATION_BADGE_DOT: &str = "notification-badge-dot";

    // Window title
    /// Window title widget (`.window-title`).
    pub const WINDOW_TITLE: &str = "window-title";

    /// Window title label (`.window-title-label`).
    pub const WINDOW_TITLE_LABEL: &str = "window-title-label";

    /// Window title app icon (`.window-title-app-icon`).
    pub const WINDOW_TITLE_APP_ICON: &str = "window-title-app-icon";

    // Updates
    /// Updates widget (`.updates`).
    pub const UPDATES: &str = "updates";

    /// Updates icon (`.updates-icon`).
    pub const UPDATES_ICON: &str = "updates-icon";

    /// Updates count label (`.updates-count`).
    pub const UPDATES_COUNT: &str = "updates-count";

    /// Updates error state (`.updates-error`).
    pub const UPDATES_ERROR: &str = "updates-error";

    /// Updates checking state (`.updates-checking`).
    pub const UPDATES_CHECKING: &str = "updates-checking";

    // Quick Settings bar widget
    /// Quick Settings bar widget (`.quick-settings`).
    pub const QUICK_SETTINGS: &str = "quick-settings";

    // CPU
    /// CPU widget (`.cpu`).
    pub const CPU: &str = "cpu";

    /// CPU icon (`.cpu-icon`).
    pub const CPU_ICON: &str = "cpu-icon";

    /// CPU label (`.cpu-label`).
    pub const CPU_LABEL: &str = "cpu-label";

    /// CPU high usage state (`.cpu-high`).
    pub const CPU_HIGH: &str = "cpu-high";

    // Memory
    /// Memory widget (`.memory`).
    pub const MEMORY: &str = "memory";

    /// Memory icon (`.memory-icon`).
    pub const MEMORY_ICON: &str = "memory-icon";

    /// Memory label (`.memory-label`).
    pub const MEMORY_LABEL: &str = "memory-label";

    /// Memory high usage state (`.memory-high`).
    pub const MEMORY_HIGH: &str = "memory-high";
}

/// Surface and popover classes.
pub mod surface {
    /// Popover surface style (`.vp-surface-popover`).
    pub const POPOVER: &str = "vp-surface-popover";

    /// Widget menu popover (`.widget-menu`).
    pub const WIDGET_MENU: &str = "widget-menu";

    /// Widget menu content (`.widget-menu-content`).
    pub const WIDGET_MENU_CONTENT: &str = "widget-menu-content";

    /// No focus outline container (`.vp-no-focus`).
    pub const NO_FOCUS: &str = "vp-no-focus";

    /// Popover header icon button (`.vp-popover-icon-btn`).
    pub const POPOVER_ICON_BTN: &str = "vp-popover-icon-btn";

    /// Popover title (`.vp-popover-title`).
    pub const POPOVER_TITLE: &str = "vp-popover-title";

    /// Layer shell popover window (`.layer-shell-popover`).
    ///
    /// Applied to layer-shell based popover windows for proper focus handling.
    pub const LAYER_SHELL_POPOVER: &str = "layer-shell-popover";

    /// Layer shell click catcher (`.layer-shell-click-catcher`).
    ///
    /// Transparent fullscreen overlay for click-outside-to-close behavior.
    pub const LAYER_SHELL_CLICK_CATCHER: &str = "layer-shell-click-catcher";
}

/// Icon-related classes.
pub mod icon {
    /// Icon root container (`.icon-root`).
    pub const ROOT: &str = "icon-root";

    /// Text-based icon (Material Symbols) (`.text-icon`).
    pub const TEXT: &str = "text-icon";

    /// Material symbol (`.material-symbol`).
    pub const MATERIAL_SYMBOL: &str = "material-symbol";

    /// Generic icon class (`.icon`).
    pub const ICON: &str = "icon";
}

/// Notification popover and toast classes.
pub mod notification {
    // Popover structure
    /// Popover root (`.notification-popover`).
    pub const POPOVER: &str = "notification-popover";

    /// Notification list container (`.notification-list`).
    pub const LIST: &str = "notification-list";

    /// Scrollable area (`.notification-scroll`).
    pub const SCROLL: &str = "notification-scroll";

    // Header
    /// Header container (`.notification-header`).
    pub const HEADER: &str = "notification-header";

    /// Header title (`.notification-header-title`).
    pub const HEADER_TITLE: &str = "notification-header-title";

    /// Header icon button (`.notification-header-icon-btn`).
    pub const HEADER_ICON_BTN: &str = "notification-header-icon-btn";

    /// Header icon (`.notification-header-icon`) - for icon sizing.
    pub const HEADER_ICON: &str = "notification-header-icon";

    /// Clear all button (`.notification-clear-btn`).
    pub const CLEAR_BTN: &str = "notification-clear-btn";

    /// Clear label (`.notification-clear-label`).
    pub const CLEAR_LABEL: &str = "notification-clear-label";

    // Empty state
    /// Empty state container (`.notification-empty`).
    pub const EMPTY: &str = "notification-empty";

    /// Empty state icon (`.notification-empty-icon`).
    pub const EMPTY_ICON: &str = "notification-empty-icon";

    /// Empty state label (`.notification-empty-label`).
    pub const EMPTY_LABEL: &str = "notification-empty-label";

    // Row/card
    /// Notification row/card (`.notification-row`).
    pub const ROW: &str = "notification-row";

    /// Critical urgency (`.notification-critical`).
    pub const CRITICAL: &str = "notification-critical";

    /// Low urgency (`.notification-low`).
    pub const LOW: &str = "notification-low";

    /// Row icon (`.notification-row-icon`).
    pub const ROW_ICON: &str = "notification-row-icon";

    /// Row content (`.notification-row-content`).
    pub const ROW_CONTENT: &str = "notification-row-content";

    /// App name label (`.notification-app-name`).
    pub const APP_NAME: &str = "notification-app-name";

    /// Timestamp label (`.notification-timestamp`).
    pub const TIMESTAMP: &str = "notification-timestamp";

    /// Summary label (`.notification-summary`).
    pub const SUMMARY: &str = "notification-summary";

    /// Body text (`.notification-body`).
    pub const BODY: &str = "notification-body";

    /// Body container (`.notification-body-container`).
    pub const BODY_CONTAINER: &str = "notification-body-container";

    /// Truncated body (`.notification-body-truncated`).
    pub const BODY_TRUNCATED: &str = "notification-body-truncated";

    // Actions
    /// Actions container (`.notification-actions`).
    pub const ACTIONS: &str = "notification-actions";

    /// Action button (`.notification-action-btn`).
    pub const ACTION_BTN: &str = "notification-action-btn";

    /// Dismiss button (`.notification-dismiss-btn`).
    pub const DISMISS_BTN: &str = "notification-dismiss-btn";

    /// Dismiss icon (`.notification-dismiss-icon`).
    pub const DISMISS_ICON: &str = "notification-dismiss-icon";

    // Toast
    /// Toast window (`.notification-toast`).
    pub const TOAST: &str = "notification-toast";

    /// Toast container (`.notification-toast-container`).
    pub const TOAST_CONTAINER: &str = "notification-toast-container";

    /// Toast critical state (`.notification-toast-critical`).
    pub const TOAST_CRITICAL: &str = "notification-toast-critical";

    /// Toast low urgency (`.notification-toast-low`).
    pub const TOAST_LOW: &str = "notification-toast-low";

    /// Toast icon (`.notification-toast-icon`).
    pub const TOAST_ICON: &str = "notification-toast-icon";

    /// Toast content (`.notification-toast-content`).
    pub const TOAST_CONTENT: &str = "notification-toast-content";

    /// Toast app name (`.notification-toast-app`).
    pub const TOAST_APP: &str = "notification-toast-app";

    /// Toast summary (`.notification-toast-summary`).
    pub const TOAST_SUMMARY: &str = "notification-toast-summary";

    /// Toast body (`.notification-toast-body`).
    pub const TOAST_BODY: &str = "notification-toast-body";

    /// Toast dismiss button (`.notification-toast-dismiss`).
    pub const TOAST_DISMISS: &str = "notification-toast-dismiss";

    /// Toast actions container (`.notification-toast-actions`).
    pub const TOAST_ACTIONS: &str = "notification-toast-actions";

    /// Toast action button (`.notification-toast-action`).
    pub const TOAST_ACTION: &str = "notification-toast-action";

    /// Toast clickable content (`.notification-toast-clickable`).
    pub const TOAST_CLICKABLE: &str = "notification-toast-clickable";
}

/// On-Screen Display (OSD) classes.
pub mod osd {
    /// OSD window (`.osd-window`).
    pub const WINDOW: &str = "osd-window";

    /// OSD widget container (`.osd-widget`).
    pub const WIDGET: &str = "osd-widget";

    /// OSD container (`.osd-container`).
    pub const CONTAINER: &str = "osd-container";

    /// Normal content (`.osd-normal`).
    pub const NORMAL: &str = "osd-normal";

    /// OSD icon (`.osd-icon`).
    pub const ICON: &str = "osd-icon";

    /// OSD slider (`.osd-slider`).
    pub const SLIDER: &str = "osd-slider";

    /// Unavailable state content (`.osd-unavailable`).
    pub const UNAVAILABLE: &str = "osd-unavailable";

    /// Unavailable icon (`.osd-unavailable-icon`).
    pub const UNAVAILABLE_ICON: &str = "osd-unavailable-icon";

    /// Unavailable label (`.osd-unavailable-label`).
    pub const UNAVAILABLE_LABEL: &str = "osd-unavailable-label";

    /// Vertical orientation (`.osd-vertical`).
    pub const VERTICAL: &str = "osd-vertical";

    /// Horizontal orientation (`.osd-horizontal`).
    pub const HORIZONTAL: &str = "osd-horizontal";
}

/// Battery popover classes.
pub mod battery {
    /// Section title (`.vp-section-title`).
    pub const SECTION_TITLE: &str = "vp-section-title";

    /// Battery popover container (`.battery-popover`).
    pub const POPOVER: &str = "battery-popover";

    /// Battery popover section title (`.battery-popover-section-title`).
    pub const POPOVER_SECTION_TITLE: &str = "battery-popover-section-title";

    /// Battery percentage (`.battery-popover-percent`).
    pub const POPOVER_PERCENT: &str = "battery-popover-percent";

    /// Battery state (`.battery-popover-state`).
    pub const POPOVER_STATE: &str = "battery-popover-state";

    /// Battery time (`.battery-popover-time`).
    pub const POPOVER_TIME: &str = "battery-popover-time";

    /// Battery power (`.battery-popover-power`).
    pub const POPOVER_POWER: &str = "battery-popover-power";

    /// Profile button (`.battery-popover-profile-button`).
    pub const POPOVER_PROFILE_BUTTON: &str = "battery-popover-profile-button";

    /// No profiles available label (`.battery-popover-no-profiles`).
    pub const POPOVER_NO_PROFILES: &str = "battery-popover-no-profiles";

    /// Popover separator (`.battery-popover-separator`).
    pub const POPOVER_SEPARATOR: &str = "battery-popover-separator";
}

/// Calendar popover classes.
pub mod calendar {
    /// Calendar popover (`.calendar-popover`).
    pub const POPOVER: &str = "calendar-popover";

    /// Calendar header (`.calendar-header`).
    pub const HEADER: &str = "calendar-header";

    /// Navigation button (`.calendar-nav-button`).
    pub const NAV_BUTTON: &str = "calendar-nav-button";

    /// Calendar widget (`.calendar-widget`).
    pub const WIDGET: &str = "calendar-widget";

    /// Calendar popover grid (`.calendar-popover-grid`).
    pub const GRID: &str = "calendar-popover-grid";

    /// Show today state (`.show-today`).
    pub const SHOW_TODAY: &str = "show-today";
}

/// Tooltip classes.
pub mod tooltip {
    /// Tooltip window (`.vibepanel-tooltip`).
    pub const WINDOW: &str = "vibepanel-tooltip";

    /// Tooltip label (`.vibepanel-tooltip-label`).
    pub const LABEL: &str = "vibepanel-tooltip-label";
}

/// Media widget classes (MPRIS media player control).
pub mod media {
    // Bar widget
    /// Media widget container (`.media`).
    pub const WIDGET: &str = "media";

    /// Media icon (play/pause state indicator) (`.media-icon`).
    pub const ICON: &str = "media-icon";

    /// Player icon (app icon like Spotify, Firefox) (`.media-player-icon`).
    pub const PLAYER_ICON: &str = "media-player-icon";

    /// Media text label in bar (`.media-label`).
    pub const LABEL: &str = "media-label";

    /// Small album art thumbnail in bar (`.media-art-small`).
    pub const ART_SMALL: &str = "media-art-small";

    /// Controls container (`.media-controls`).
    pub const CONTROLS: &str = "media-controls";

    /// Control button (`.media-control-btn`).
    pub const CONTROL_BTN: &str = "media-control-btn";

    /// Playing state (`.media-playing`).
    pub const PLAYING: &str = "media-playing";

    /// Paused state (`.media-paused`).
    pub const PAUSED: &str = "media-paused";

    /// Stopped state (`.media-stopped`).
    pub const STOPPED: &str = "media-stopped";

    // Shared popover/window content
    /// Media content container (`.media-content`).
    pub const CONTENT: &str = "media-content";

    /// Large album art (`.media-art`).
    pub const ART: &str = "media-art";

    /// Album art placeholder when no art available (`.media-art-placeholder`).
    pub const ART_PLACEHOLDER: &str = "media-art-placeholder";

    /// Track title label (`.media-track-title`).
    pub const TRACK_TITLE: &str = "media-track-title";

    /// Artist label (`.media-artist`).
    pub const ARTIST: &str = "media-artist";

    /// Album label (`.media-album`).
    pub const ALBUM: &str = "media-album";

    /// Primary control button (play/pause) (`.media-control-btn-primary`).
    pub const CONTROL_BTN_PRIMARY: &str = "media-control-btn-primary";

    /// Primary control button icon - larger size (`.media-primary-icon`).
    pub const PRIMARY_ICON: &str = "media-primary-icon";

    /// Seek bar container (`.media-seek`).
    pub const SEEK: &str = "media-seek";

    /// Seek slider (`.media-seek-slider`).
    pub const SEEK_SLIDER: &str = "media-seek-slider";

    /// Position/duration labels container (`.media-time`).
    pub const TIME: &str = "media-time";

    /// Position label (`.media-position`).
    pub const POSITION: &str = "media-position";

    /// Duration label (`.media-duration`).
    pub const DURATION: &str = "media-duration";

    /// Volume slider container (`.media-volume`).
    pub const VOLUME: &str = "media-volume";

    /// Volume slider (`.media-volume-slider`).
    pub const VOLUME_SLIDER: &str = "media-volume-slider";

    /// Volume icon (`.media-volume-icon`).
    pub const VOLUME_ICON: &str = "media-volume-icon";

    /// Player selector dropdown (`.media-player-selector`).
    pub const PLAYER_SELECTOR: &str = "media-player-selector";

    /// Player name label (`.media-player-name`).
    pub const PLAYER_NAME: &str = "media-player-name";

    // Pop-out window
    /// Pop-out window (`.media-window`).
    pub const WINDOW: &str = "media-window";

    /// Window header/drag area (`.media-window-header`).
    pub const WINDOW_HEADER: &str = "media-window-header";

    /// Window drag handle (`.media-window-drag`).
    pub const WINDOW_DRAG: &str = "media-window-drag";

    /// Window close button (`.media-window-close`).
    pub const WINDOW_CLOSE: &str = "media-window-close";

    /// Window dock button (return to popover) (`.media-window-dock`).
    pub const WINDOW_DOCK: &str = "media-window-dock";

    /// Window control button - smaller than popover (`.media-window-control-btn`).
    pub const WINDOW_CONTROL_BTN: &str = "media-window-control-btn";

    /// Window seek slider - thinner than popover (`.media-window-seek-slider`).
    pub const WINDOW_SEEK_SLIDER: &str = "media-window-seek-slider";

    // Popover
    /// Media popover (`.media-popover`).
    pub const POPOVER: &str = "media-popover";

    /// Media popover header row (`.media-popover-header`).
    pub const HEADER: &str = "media-popover-header";

    /// Pop-out button in popover (`.media-popout-btn`).
    pub const POPOUT_BTN: &str = "media-popout-btn";

    /// Pop-out button icon (`.media-popout-icon`).
    pub const POPOUT_ICON: &str = "media-popout-icon";

    /// Player selector button in popover (`.media-player-selector-btn`).
    pub const PLAYER_SELECTOR_BTN: &str = "media-player-selector-btn";

    /// Player selector menu container (`.media-player-menu`).
    pub const PLAYER_MENU: &str = "media-player-menu";

    /// Player selector menu item (`.media-player-menu-item`).
    pub const PLAYER_MENU_ITEM: &str = "media-player-menu-item";

    /// Player menu item title label (`.media-player-menu-title`).
    pub const PLAYER_MENU_TITLE: &str = "media-player-menu-title";

    /// Player menu item subtitle/status label (`.media-player-menu-subtitle`).
    pub const PLAYER_MENU_SUBTITLE: &str = "media-player-menu-subtitle";

    /// Player menu check icon (`.media-player-menu-check`).
    pub const PLAYER_MENU_CHECK: &str = "media-player-menu-check";

    // Empty state
    /// No player available state (`.media-empty`).
    pub const EMPTY: &str = "media-empty";

    /// Empty state icon (`.media-empty-icon`).
    pub const EMPTY_ICON: &str = "media-empty-icon";

    /// Empty state label (`.media-empty-label`).
    pub const EMPTY_LABEL: &str = "media-empty-label";

    // Icon names (freedesktop naming convention)
    // These are used with IconHandle and BaseWidget.add_icon(), which map
    // freedesktop names to Material Symbols font glyphs internally.

    /// Icon for paused/stopped state (shows play button).
    pub const ICON_PLAY: &str = "media-playback-start";

    /// Icon for playing state (shows pause button).
    pub const ICON_PAUSE: &str = "media-playback-pause";

    /// Icon for skipping to next track.
    pub const ICON_NEXT: &str = "media-skip-forward";

    /// Icon for skipping to previous track.
    pub const ICON_PREVIOUS: &str = "media-skip-backward";

    /// Generic audio/music icon (fallback when player icon unavailable).
    pub const ICON_AUDIO_GENERIC: &str = "audio-x-generic";
}

/// System resource popover classes (shared by CPU and Memory widgets).
pub mod system_popover {
    /// System popover container (`.system-popover`).
    pub const POPOVER: &str = "system-popover";

    /// Section card wrapper (`.system-section-card`).
    pub const SECTION_CARD: &str = "system-section-card";

    /// Section title container (`.system-section-title`).
    pub const SECTION_TITLE: &str = "system-section-title";

    /// Section title icon (`.system-section-icon`).
    pub const SECTION_ICON: &str = "system-section-icon";

    /// Progress bar (`.system-progress-bar`).
    pub const PROGRESS_BAR: &str = "system-progress-bar";

    /// Core row for per-CPU display (`.system-core-row`).
    pub const CORE_ROW: &str = "system-core-row";

    /// Core bar (`.system-core-bar`).
    pub const CORE_BAR: &str = "system-core-bar";

    /// Expander header row (`.system-expander-header`).
    pub const EXPANDER_HEADER: &str = "system-expander-header";

    /// Expander content container (`.system-expander-content`).
    pub const EXPANDER_CONTENT: &str = "system-expander-content";

    /// Network speed icon (`.system-network-icon`).
    pub const NETWORK_ICON: &str = "system-network-icon";
}
