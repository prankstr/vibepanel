use super::*;
use crate::services::config_manager::ConfigManager;
use crate::ui_regression_test_support::{
    CssProviderGuard, Rgba8, assert_pixel_close, find_descendant_with_class, flush_gtk,
    init_gtk_or_skip, init_layer_shell_or_skip, label_with_text, registered_test_app,
    run_ignored_contract_subprocess, sample_widget_pixel,
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
        close_toast_on_close: false,
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
fn test_toast_position_parse_contract() {
    assert_eq!(
        ToastPosition::parse("top-right"),
        Some(ToastPosition::TopRight)
    );
    assert_eq!(
        ToastPosition::parse("top-center"),
        Some(ToastPosition::TopCenter)
    );
    assert_eq!(
        ToastPosition::parse("top-left"),
        Some(ToastPosition::TopLeft)
    );
    assert_eq!(
        ToastPosition::parse("bottom-right"),
        Some(ToastPosition::BottomRight)
    );
    assert_eq!(
        ToastPosition::parse("bottom-center"),
        Some(ToastPosition::BottomCenter)
    );
    assert_eq!(
        ToastPosition::parse("bottom-left"),
        Some(ToastPosition::BottomLeft)
    );
    assert_eq!(ToastPosition::parse("auto"), None);
    assert_eq!(ToastPosition::default(), ToastPosition::TopRight);
}

#[test]
fn test_calculate_center_margin_contract() {
    assert_eq!(calculate_center_margin(1920, POPOVER_WIDTH), 760);
    assert_eq!(calculate_center_margin(400, POPOVER_WIDTH), 0);
    assert_eq!(calculate_center_margin(320, POPOVER_WIDTH), 0);
}

#[test]
fn test_toast_horizontal_layout_contract() {
    let side_margin = 8;
    let cases = [
        (
            ToastPosition::TopRight,
            None,
            Some(Edge::Right),
            side_margin,
        ),
        (
            ToastPosition::TopRight,
            Some(1920),
            Some(Edge::Right),
            side_margin,
        ),
        (ToastPosition::TopLeft, None, Some(Edge::Left), side_margin),
        (
            ToastPosition::TopLeft,
            Some(1920),
            Some(Edge::Left),
            side_margin,
        ),
        (
            ToastPosition::BottomRight,
            None,
            Some(Edge::Right),
            side_margin,
        ),
        (
            ToastPosition::BottomRight,
            Some(1920),
            Some(Edge::Right),
            side_margin,
        ),
        (
            ToastPosition::BottomLeft,
            None,
            Some(Edge::Left),
            side_margin,
        ),
        (
            ToastPosition::BottomLeft,
            Some(1920),
            Some(Edge::Left),
            side_margin,
        ),
        (ToastPosition::TopCenter, None, None, 0),
        (ToastPosition::TopCenter, Some(1920), Some(Edge::Left), 760),
        (ToastPosition::BottomCenter, None, None, 0),
        (
            ToastPosition::BottomCenter,
            Some(1920),
            Some(Edge::Left),
            760,
        ),
    ];

    for (position, monitor_width, expected_edge, expected_margin) in cases {
        assert_eq!(
            toast_horizontal_layout(position, monitor_width, side_margin),
            (expected_edge, expected_margin),
            "unexpected horizontal layout for {position:?} with monitor_width={monitor_width:?}"
        );
    }
}

#[test]
fn test_toast_surface_margin_independent_of_shadow_setting() {
    let mut config = Config::default();
    config.theme.shadows = false;
    ConfigManager::replace_global_for_test(config.clone());
    let palette = ConfigManager::global().palette();
    SurfaceStyleManager::global().reconfigure(
        palette.surface_styles(),
        config.advanced.pango_font_rendering,
    );

    assert_eq!(
        SurfaceStyleManager::global().shadow_margin(SURFACE_SHADOW_MARGIN),
        0
    );
    assert_eq!(toast_surface_margin(), SURFACE_SHADOW_MARGIN);
}

#[test]
fn test_notification_toast_critical_class_does_not_apply_visual_tokens() {
    let css = crate::widgets::css::widget_css(&Config::default());

    assert!(
        css.contains(
            ".notification-row.notification-critical {\n    border-left: 3px solid var(--color-state-warning);"
        ),
        "critical notification rows should consume the shared warning border token"
    );
    assert!(
        css.contains(
            ".notification-row.notification-critical {\n    background-color: var(--color-row-critical-background);"
        ),
        "critical notification rows should consume the row critical background token"
    );
    assert!(
        !css.contains(".notification-toast-critical {"),
        "critical toasts intentionally keep main's neutral visual styling"
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

fn configure_toast_pixel_test(config: &Config) -> CssProviderGuard {
    ConfigManager::replace_global_for_test(config.clone());
    let css_provider = CssProviderGuard::for_config(config, gtk4::STYLE_PROVIDER_PRIORITY_USER);
    let palette = ConfigManager::global().palette();
    SurfaceStyleManager::global().reconfigure(
        palette.surface_styles(),
        config.advanced.pango_font_rendering,
    );
    css_provider
}

fn toast_pixel_config(background_color: &str) -> Config {
    let mut config = Config::default();
    config.theme.mode = "dark".to_string();
    config.widgets.background_color = Some(background_color.to_string());
    config.widgets.background_opacity = 0.0;
    config.widgets.popover_background_opacity = Some(1.0);
    config
}

fn blank_notification() -> Notification {
    Notification {
        app_name: String::new(),
        summary: String::new(),
        body: String::new(),
        ..test_notification(crate::services::notification::URGENCY_NORMAL)
    }
}

fn toast_surface_fixture(
    config: &Config,
    override_css: Option<&str>,
) -> (
    gtk4::Window,
    CssProviderGuard,
    Option<CssProviderGuard>,
    gtk4::Widget,
) {
    let css_provider = configure_toast_pixel_test(config);
    let override_css_provider = override_css
        .map(|css| CssProviderGuard::new(css, gtk4::STYLE_PROVIDER_PRIORITY_USER + 100));
    let window = gtk4::Window::builder()
        .title("vibepanel toast pixel UI regression test")
        .default_width(220)
        .default_height(120)
        .build();
    let notification = blank_notification();
    let noop_dismiss: ToastCallback = Rc::new(|_| {});
    let noop_action: ToastActionCallback = Rc::new(|_, _| {});
    let surface = build_toast_content(&notification, noop_dismiss, noop_action, &window);
    surface.set_size_request(180, 80);
    window.set_child(Some(&surface));
    window.present();
    flush_gtk();

    (
        window,
        css_provider,
        override_css_provider,
        surface.upcast(),
    )
}

fn center_pixel_of_toast(window: &gtk4::Window, surface: &gtk4::Widget) -> Rgba8 {
    sample_widget_pixel(window, surface, surface.width() / 2, surface.height() / 2)
}

fn edge_pixel_of_toast(window: &gtk4::Window, surface: &gtk4::Widget) -> Rgba8 {
    sample_widget_pixel(window, surface, 1, surface.height() / 2)
}

fn run_test_notification_toast_surface_pixels() {
    if !init_gtk_or_skip(
        "notification toast pixel UI regression test",
        Some("VIBEPANEL_UI_REGRESSION_REQUIRED"),
    ) {
        return;
    }

    let background_color = "#445566";
    let outline_color = "#80a0c0";
    let mut config = toast_pixel_config(background_color);
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = outline_color.to_string();
    config.theme.outline_opacity = 0.5;

    let (window, _css_provider, _override_css_provider, surface) =
        toast_surface_fixture(&config, None);
    // Baseline for replacing SurfaceStyleManager with static surface CSS.
    assert_pixel_close(
        center_pixel_of_toast(&window, &surface),
        Rgba8::from_hex(background_color),
        "production toast surface should use popover opacity, not transparent widget opacity",
    );
    assert_pixel_close(
        edge_pixel_of_toast(&window, &surface),
        Rgba8::from_hex(outline_color)
            .with_alpha(128)
            .premultiply_alpha(),
        "production toast surface should render configured surface outline color and opacity",
    );

    window.close();
    flush_gtk();
}

fn run_test_notification_toast_user_css_outline_pixel() {
    if !init_gtk_or_skip(
        "notification toast pixel UI regression test",
        Some("VIBEPANEL_UI_REGRESSION_REQUIRED"),
    ) {
        return;
    }

    let background_color = "#101820";
    let css_color = "#00ff00";
    let mut config = toast_pixel_config(background_color);
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = "accent".to_string();
    config.theme.outline_opacity = 1.0;
    config.theme.accent = Some("#224466".to_string());

    let user_css = format!(".notification-toast {{ --surface-outline-color: {css_color}; }}");
    let (window, _css_provider, _override_css_provider, surface) =
        toast_surface_fixture(&config, Some(&user_css));

    assert_pixel_close(
        edge_pixel_of_toast(&window, &surface),
        Rgba8::from_hex(css_color),
        "user CSS should override production toast surface outline color",
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
    let cases = [
        (ToastPosition::TopRight, Edge::Top, Some(Edge::Right)),
        (ToastPosition::TopCenter, Edge::Top, None),
        (ToastPosition::TopLeft, Edge::Top, Some(Edge::Left)),
        (ToastPosition::BottomRight, Edge::Bottom, Some(Edge::Right)),
        (ToastPosition::BottomCenter, Edge::Bottom, None),
        (ToastPosition::BottomLeft, Edge::Bottom, Some(Edge::Left)),
    ];

    for (position, vertical_edge, horizontal_edge) in cases {
        let mut config = Config::default();
        config.theme.mode = "dark".to_string();
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
            ToastWindowContext {
                app: &app,
                monitor: None,
                layout: ToastLayout {
                    position,
                    initial_margin: TOAST_EDGE_MARGIN,
                },
            },
            &notification,
            noop_dismiss,
            noop_action,
            noop_timeout,
            noop_height,
        );
        toast.present();
        flush_gtk();

        let child = toast
            .window
            .child()
            .expect("toast window should contain a styled surface");
        let container = find_descendant_with_class(&child, notif::TOAST_CONTAINER)
            .expect("toast should render a styled notification container");
        let side_margin = (TOAST_SIDE_MARGIN - toast_surface_margin()).max(0);

        assert!(toast.window.is_layer_window());
        assert_eq!(toast.window.namespace().as_deref(), Some("vibepanel-toast"));
        assert_eq!(toast.window.layer(), Layer::Overlay);
        assert_eq!(toast.window.keyboard_mode(), KeyboardMode::None);
        assert_eq!(toast.window.exclusive_zone(), 0);
        assert_eq!(
            toast.window.is_anchor(Edge::Top),
            vertical_edge == Edge::Top
        );
        assert_eq!(
            toast.window.is_anchor(Edge::Bottom),
            vertical_edge == Edge::Bottom
        );
        assert_eq!(
            toast.window.is_anchor(Edge::Left),
            horizontal_edge == Some(Edge::Left)
        );
        assert_eq!(
            toast.window.is_anchor(Edge::Right),
            horizontal_edge == Some(Edge::Right)
        );
        assert_eq!(toast.window.margin(vertical_edge), TOAST_EDGE_MARGIN);
        assert_eq!(
            toast.window.margin(Edge::Left),
            if horizontal_edge == Some(Edge::Left) {
                side_margin
            } else {
                0
            }
        );
        assert_eq!(
            toast.window.margin(Edge::Right),
            if horizontal_edge == Some(Edge::Right) {
                side_margin
            } else {
                0
            }
        );

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
#[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
fn test_ui_regression_notification_toast_surface_pixels() {
    run_toast_ui_regression_subprocess("toast.surface_pixels");
}

#[test]
#[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
fn test_ui_regression_notification_toast_user_css_outline_pixel() {
    run_toast_ui_regression_subprocess("toast.user_css_outline_pixel");
}

#[test]
#[ignore = "internal runner for one notification toast UI regression subprocess"]
fn notification_toast_ui_regression_runner() {
    match std::env::var("VIBEPANEL_NOTIFICATION_TOAST_UI_REGRESSION_TEST").as_deref() {
        Ok("toast.structure") => run_test_notification_toast_structure(),
        Ok("toast.surface_pixels") => run_test_notification_toast_surface_pixels(),
        Ok("toast.user_css_outline_pixel") => run_test_notification_toast_user_css_outline_pixel(),
        Ok(other) => panic!("unknown notification toast UI regression test: {other}"),
        Err(_) => {
            eprintln!("skipping notification toast UI regression test: no test case selected")
        }
    }
}
