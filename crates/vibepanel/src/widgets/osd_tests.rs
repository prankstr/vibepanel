use super::*;
use crate::ui_regression_test_support::{
    CssProviderGuard, Rgba8, assert_pixel_close, flush_gtk, init_gtk_or_skip,
    run_ignored_contract_subprocess, sample_widget_pixel,
};
use vibepanel_core::Config;

fn run_osd_ui_regression_subprocess(test_case: &str) {
    run_ignored_contract_subprocess(
        "osd_ui_regression_runner",
        "VIBEPANEL_OSD_UI_REGRESSION_TEST",
        test_case,
        "OSD UI regression test",
    );
}

fn configure_osd_pixel_test(config: &Config) -> CssProviderGuard {
    ConfigManager::replace_global_for_test(config.clone());
    let css_provider = CssProviderGuard::for_config(config, gtk4::STYLE_PROVIDER_PRIORITY_USER);
    let palette = ConfigManager::global().palette();
    SurfaceStyleManager::global().reconfigure(
        palette.surface_styles(),
        config.advanced.pango_font_rendering,
    );
    css_provider
}

fn osd_pixel_config(background_color: &str) -> Config {
    let mut config = Config::default();
    config.theme.mode = "dark".to_string();
    config.widgets.border_radius = 50;
    config.widgets.background_color = Some(background_color.to_string());
    config.widgets.background_opacity = 0.0;
    config.widgets.popover_background_opacity = Some(1.0);
    config
}

fn osd_surface_fixture(
    config: &Config,
    override_css: Option<&str>,
) -> (
    gtk4::Window,
    CssProviderGuard,
    Option<CssProviderGuard>,
    gtk4::Widget,
) {
    let css_provider = configure_osd_pixel_test(config);
    let override_css_provider = override_css
        .map(|css| CssProviderGuard::new(css, gtk4::STYLE_PROVIDER_PRIORITY_USER + 100));
    let window = gtk4::Window::builder()
        .title("vibepanel OSD pixel UI regression test")
        .default_width(260)
        .default_height(120)
        .build();
    let content = build_osd_content(Orientation::Horizontal, false);
    content.container.set_size_request(220, 72);
    window.set_child(Some(&content.container));
    window.present();
    flush_gtk();

    (
        window,
        css_provider,
        override_css_provider,
        content.container.upcast(),
    )
}

fn run_test_osd_surface_pixels() {
    if !init_gtk_or_skip("OSD pixel UI regression test") {
        return;
    }

    let background_color = "#445566";
    let outline_color = "#80a0c0";
    let mut config = osd_pixel_config(background_color);
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = outline_color.to_string();
    config.theme.outline_opacity = 1.0;

    let (window, _css_provider, _override_css_provider, surface) =
        osd_surface_fixture(&config, None);
    assert_pixel_close(
        sample_widget_pixel(&window, &surface, 8, surface.height() / 2),
        Rgba8::from_hex(background_color),
        "production OSD surface should use popover opacity, not transparent widget opacity",
    );
    assert_pixel_close(
        sample_widget_pixel(&window, &surface, 1, surface.height() / 2),
        Rgba8::from_hex(outline_color),
        "production OSD surface should render configured surface outline color",
    );
    let corner = sample_widget_pixel(&window, &surface, 1, 1);
    assert!(
        corner.a <= 2,
        "production OSD surface should render transparent rounded corners; corner={corner:?}"
    );

    window.close();
    flush_gtk();
}

fn run_test_osd_user_css_outline_pixel() {
    if !init_gtk_or_skip("OSD pixel UI regression test") {
        return;
    }

    let background_color = "#101820";
    let css_color = "#00ff00";
    let mut config = osd_pixel_config(background_color);
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = "accent".to_string();
    config.theme.outline_opacity = 1.0;
    config.theme.accent = Some("#224466".to_string());

    let user_css = format!(".osd {{ --surface-outline-color: {css_color}; }}");
    let (window, _css_provider, _override_css_provider, surface) =
        osd_surface_fixture(&config, Some(&user_css));

    assert_pixel_close(
        sample_widget_pixel(&window, &surface, 1, surface.height() / 2),
        Rgba8::from_hex(css_color),
        "user CSS should override production OSD surface outline color",
    );

    window.close();
    flush_gtk();
}

#[test]
#[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
fn test_ui_regression_osd_surface_pixels() {
    run_osd_ui_regression_subprocess("osd.surface_pixels");
}

#[test]
#[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
fn test_ui_regression_osd_user_css_outline_pixel() {
    run_osd_ui_regression_subprocess("osd.user_css_outline_pixel");
}

#[test]
#[ignore = "internal runner for one OSD UI regression subprocess"]
fn osd_ui_regression_runner() {
    match std::env::var("VIBEPANEL_OSD_UI_REGRESSION_TEST").as_deref() {
        Ok("osd.surface_pixels") => run_test_osd_surface_pixels(),
        Ok("osd.user_css_outline_pixel") => run_test_osd_user_css_outline_pixel(),
        Ok(other) => panic!("unknown OSD UI regression test: {other}"),
        Err(_) => eprintln!("skipping OSD UI regression test: no test case selected"),
    }
}
