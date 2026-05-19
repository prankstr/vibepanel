use super::*;
use crate::services::config_manager::ConfigManager;
use crate::ui_regression_test_support::{
    CssProviderGuard, find_descendant_with_class, flush_gtk, init_gtk_or_skip,
    init_layer_shell_or_skip, label_with_text, registered_test_app,
    run_ignored_contract_subprocess,
};
use std::rc::Rc;
use vibepanel_core::Config;

fn test_app() -> Application {
    registered_test_app("dev.vibepanel.toast-ui-regression")
}

fn test_notification(urgency: u8) -> Notification {
    Notification {
        id: u32::from(urgency) + 1,
        app_name: "UI Regression Test".to_string(),
        app_icon: String::new(),
        summary: "Toast summary".to_string(),
        body: "Toast body".to_string(),
        actions: Vec::new(),
        urgency,
        timestamp: 0.0,
        expire_timeout: 0,
        desktop_entry: None,
        image_path: None,
        image_data: None,
        transient: true,
    }
}

fn run_toast_ui_regression_subprocess(test_case: &str) {
    run_ignored_contract_subprocess(
        "notification_toast_ui_regression_runner",
        "VIBEPANEL_NOTIFICATION_TOAST_UI_REGRESSION_TEST",
        test_case,
        "notification toast UI regression test",
    );
}

#[test]
fn test_notification_toast_critical_class_consumes_critical_tokens() {
    let css = crate::widgets::css::widget_css(&Config::default());

    assert!(
        css.contains(
            ".notification-row.notification-critical,\n.notification-toast-critical {\n    border-left: 3px solid var(--color-state-warning);"
        ),
        "critical toast selector should consume the shared warning border token"
    );
    assert!(
        css.contains(
            ".notification-toast-critical {\n    background-color: var(--color-toast-critical-background);"
        ),
        "critical toast selector should consume the toast critical background token"
    );
}

fn run_test_notification_toast_structure() {
    if !init_gtk_or_skip(
        "notification toast UI regression test",
        Some("VIBEPANEL_UI_REGRESSION_REQUIRED"),
    ) {
        return;
    }

    let mut config = Config::default();
    config.theme.mode = "dark".to_string();
    ConfigManager::replace_global_for_test(config.clone());
    let _css_provider =
        CssProviderGuard::for_config(&config, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let palette = ConfigManager::global().palette();
    SurfaceStyleManager::global().reconfigure(
        palette.surface_styles(),
        config.advanced.pango_font_rendering,
    );

    let window = gtk4::Window::builder()
        .title("vibepanel toast structure UI regression test")
        .default_width(220)
        .default_height(120)
        .build();
    let notification = test_notification(URGENCY_CRITICAL);
    let noop_dismiss: ToastCallback = Rc::new(|_| {});
    let noop_action: ToastActionCallback = Rc::new(|_, _| {});
    let surface = build_toast_content(&notification, noop_dismiss, noop_action, &window);
    surface.set_size_request(180, 80);
    window.set_child(Some(&surface));
    window.present();
    flush_gtk();

    let child = window
        .child()
        .expect("toast test window should contain production toast content");
    let container = find_descendant_with_class(&child, notif::TOAST_CONTAINER)
        .expect("toast should render a styled notification container");

    assert!(container.has_css_class(notif::TOAST));
    assert!(container.has_css_class(notif::TOAST_CRITICAL));
    assert!(!container.has_css_class(notif::TOAST_LOW));
    assert!(
        label_with_text(&child, "UI Regression Test"),
        "toast should render the notification app name"
    );
    assert!(
        label_with_text(&child, "Toast summary"),
        "toast should render the notification summary"
    );
    assert!(
        label_with_text(&child, "Toast body"),
        "toast should render the notification body"
    );

    window.close();
    flush_gtk();
}

#[test]
#[ignore = "notification toast structure contract: requires a Wayland compositor with layer-shell support"]
fn test_notification_toast_structure_contract() {
    if init_layer_shell_or_skip("notification toast UI regression test").is_none() {
        return;
    }

    let app = test_app();
    for position in ["top", "bottom"] {
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
        config.bar.position = position.to_string();
        config.advanced.compositor = "mango".to_string();
        ConfigManager::replace_global_for_test(config.clone());
        let _css_provider =
            CssProviderGuard::for_config(&config, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let palette = ConfigManager::global().palette();
        SurfaceStyleManager::global().reconfigure(
            palette.surface_styles(),
            config.advanced.pango_font_rendering,
        );

        let notification = test_notification(URGENCY_CRITICAL);
        let noop_dismiss: ToastCallback = Rc::new(|_| {});
        let noop_action: ToastActionCallback = Rc::new(|_, _| {});
        let noop_timeout: ToastCallback = Rc::new(|_| {});
        let noop_height: ToastCallback = Rc::new(|_| {});
        let toast = NotificationToast::new(
            &app,
            &notification,
            noop_dismiss,
            noop_action,
            noop_timeout,
            noop_height,
            TOAST_BAR_MARGIN,
        );
        toast.present();
        flush_gtk();

        let child = toast
            .window
            .child()
            .expect("toast window should contain a styled surface");
        let container = find_descendant_with_class(&child, notif::TOAST_CONTAINER)
            .expect("toast should render a styled notification container");
        let bar_edge = if position == "bottom" {
            Edge::Bottom
        } else {
            Edge::Top
        };
        let opposite_edge = if position == "bottom" {
            Edge::Top
        } else {
            Edge::Bottom
        };
        let right_margin = (TOAST_MARGIN_RIGHT
            - SurfaceStyleManager::global().shadow_margin(SURFACE_SHADOW_MARGIN))
        .max(0);

        assert!(toast.window.is_layer_window());
        assert_eq!(toast.window.namespace().as_deref(), Some("vibepanel-toast"));
        assert_eq!(toast.window.layer(), Layer::Overlay);
        assert_eq!(toast.window.keyboard_mode(), KeyboardMode::None);
        assert_eq!(toast.window.exclusive_zone(), 0);
        assert!(toast.window.is_anchor(bar_edge));
        assert!(toast.window.is_anchor(Edge::Right));
        assert!(!toast.window.is_anchor(Edge::Left));
        assert!(!toast.window.is_anchor(opposite_edge));
        assert_eq!(toast.window.margin(bar_edge), TOAST_BAR_MARGIN);
        assert_eq!(toast.window.margin(Edge::Right), right_margin);

        assert!(toast.window.has_css_class(notif::TOAST_WRAPPER));
        assert!(container.has_css_class(notif::TOAST));
        assert!(container.has_css_class(notif::TOAST_CRITICAL));
        assert!(!container.has_css_class(notif::TOAST_LOW));
        assert!(
            label_with_text(&child, "UI Regression Test"),
            "toast should render the notification app name"
        );
        assert!(
            label_with_text(&child, "Toast summary"),
            "toast should render the notification summary"
        );
        assert!(
            label_with_text(&child, "Toast body"),
            "toast should render the notification body"
        );

        toast.window.close();
        flush_gtk();
    }
}

#[test]
#[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
fn test_ui_regression_notification_toast_structure() {
    run_toast_ui_regression_subprocess("toast.structure");
}

#[test]
#[ignore = "internal runner for one notification toast UI regression subprocess"]
fn notification_toast_ui_regression_runner() {
    match std::env::var("VIBEPANEL_NOTIFICATION_TOAST_UI_REGRESSION_TEST").as_deref() {
        Ok("toast.structure") => run_test_notification_toast_structure(),
        Ok(other) => panic!("unknown notification toast UI regression test: {other}"),
        Err(_) => {
            eprintln!("skipping notification toast UI regression test: no test case selected")
        }
    }
}
