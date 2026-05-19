use gtk4::gdk::prelude::{PaintableExt, TextureExtManual};
use gtk4::gsk::prelude::GskRendererExt;
use gtk4::prelude::{
    ApplicationExt, Cast, DisplayExt, GtkWindowExt, ListModelExt, NativeExt, SnapshotExt,
    TextureExt, WidgetExt,
};
use std::sync::{Mutex, MutexGuard};

use vibepanel_core::Config;

static UI_REGRESSION_SUBPROCESS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn ui_regression_subprocess_lock() -> MutexGuard<'static, ()> {
    UI_REGRESSION_SUBPROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn run_ignored_contract_subprocess(
    runner_test: &str,
    env_name: &str,
    contract: &str,
    label: &str,
) {
    let exe = std::env::current_exe().expect("current test binary path should be available");
    let output = {
        let _lock = ui_regression_subprocess_lock();
        std::process::Command::new(exe)
            .arg(runner_test)
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(env_name, contract)
            .env("VIBEPANEL_UI_REGRESSION_REQUIRED", "1")
            .output()
    }
    .unwrap_or_else(|_| panic!("{label} subprocess should run"));

    assert!(
        output.status.success(),
        "{label} subprocess failed for {contract}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub(crate) fn init_gtk_or_skip(context: &str, required_env: Option<&str>) -> bool {
    let init_ok = gtk4::init().is_ok();
    let display_available = gtk4::gdk::Display::default().is_some();

    if !init_ok && !display_available {
        if let Some(env_name) = required_env
            && env_truthy(env_name)
        {
            panic!("{env_name} is set but no GTK display is available");
        }
        eprintln!("skipping {context}: GTK display is unavailable");
        return false;
    }

    if !display_available {
        eprintln!("skipping {context}: no default GTK display");
        return false;
    }

    true
}

pub(crate) fn init_layer_shell_or_skip(context: &str) -> Option<gtk4::gdk::Display> {
    if !init_gtk_or_skip(context, None) {
        return None;
    }

    if !gtk4_layer_shell::is_supported() {
        eprintln!("skipping {context}: compositor does not support layer-shell");
        return None;
    }

    gtk4::gdk::Display::default()
}

pub(crate) fn first_monitor_or_skip(
    display: &gtk4::gdk::Display,
    context: &str,
) -> Option<gtk4::gdk::Monitor> {
    let monitor = display
        .monitors()
        .item(0)?
        .downcast::<gtk4::gdk::Monitor>()
        .ok();
    if monitor.is_none() {
        eprintln!("skipping {context}: no monitor available");
    }

    monitor
}

pub(crate) fn registered_test_app(application_id: &str) -> gtk4::Application {
    let app = gtk4::Application::builder()
        .application_id(application_id)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .expect("test app should register");
    app
}

pub(crate) fn maybe_hold_probe_window() {
    let Ok(ms) = std::env::var("VIBEPANEL_UI_REGRESSION_PROBE_HOLD_MS") else {
        return;
    };
    let Ok(ms) = ms.parse::<u64>() else {
        return;
    };
    if ms == 0 {
        return;
    }

    let ctx = gtk4::glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        ctx.iteration(true);
    }
}

pub(crate) struct CssProviderGuard {
    display: gtk4::gdk::Display,
    provider: gtk4::CssProvider,
}

impl CssProviderGuard {
    pub(crate) fn new(css: &str, priority: u32) -> Self {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(css);

        let display = gtk4::gdk::Display::default().expect("GTK display should exist");
        gtk4::style_context_add_provider_for_display(&display, &provider, priority);

        Self { display, provider }
    }

    pub(crate) fn for_config(config: &Config, priority: u32) -> Self {
        let palette = vibepanel_core::ThemePalette::from_config(config, None, None);
        let popover_palette = vibepanel_core::ThemePalette::popover_palette(config, None, None);
        Self::new(
            &crate::bar::generate_css(config, &palette, popover_palette.as_ref()),
            priority,
        )
    }
}

impl Drop for CssProviderGuard {
    fn drop(&mut self) {
        gtk4::style_context_remove_provider_for_display(&self.display, &self.provider);
    }
}

pub(crate) struct PaintedSurfaceFixture {
    pub(crate) window: gtk4::Window,
    pub(crate) _css_provider: CssProviderGuard,
    pub(crate) surface: gtk4::Widget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgba8 {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) a: u8,
}

impl Rgba8 {
    pub(crate) fn from_hex(hex: &str) -> Self {
        let hex = hex
            .strip_prefix('#')
            .expect("test color should be a #rrggbb literal");
        assert_eq!(hex.len(), 6, "test color should be a #rrggbb literal");
        Self {
            r: u8::from_str_radix(&hex[0..2], 16).expect("valid red channel"),
            g: u8::from_str_radix(&hex[2..4], 16).expect("valid green channel"),
            b: u8::from_str_radix(&hex[4..6], 16).expect("valid blue channel"),
            a: 255,
        }
    }

    pub(crate) fn from_gdk(rgba: gtk4::gdk::RGBA) -> Self {
        Self {
            r: (rgba.red().clamp(0.0, 1.0) * 255.0).round() as u8,
            g: (rgba.green().clamp(0.0, 1.0) * 255.0).round() as u8,
            b: (rgba.blue().clamp(0.0, 1.0) * 255.0).round() as u8,
            a: (rgba.alpha().clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }

    pub(crate) fn over(self, background: Self) -> Self {
        let alpha = f64::from(self.a) / 255.0;
        let blend = |foreground: u8, background: u8| -> u8 {
            (f64::from(foreground) * alpha + f64::from(background) * (1.0 - alpha)).round() as u8
        };
        Self {
            r: blend(self.r, background.r),
            g: blend(self.g, background.g),
            b: blend(self.b, background.b),
            a: 255,
        }
    }

    pub(crate) fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub(crate) fn close_to(self, expected: Self, tolerance: u8) -> bool {
        self.r.abs_diff(expected.r) <= tolerance
            && self.g.abs_diff(expected.g) <= tolerance
            && self.b.abs_diff(expected.b) <= tolerance
            && self.a.abs_diff(expected.a) <= tolerance
    }

    pub(crate) fn luma(self) -> f64 {
        fn channel(c: u8) -> f64 {
            let c = f64::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }
}

pub(crate) fn flush_gtk() {
    let ctx = gtk4::glib::MainContext::default();
    while ctx.pending() {
        ctx.iteration(false);
    }
}

pub(crate) fn find_descendant_with_class(
    root: &gtk4::Widget,
    class_name: &str,
) -> Option<gtk4::Widget> {
    if root.has_css_class(class_name) {
        return Some(root.clone());
    }

    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_descendant_with_class(&widget, class_name) {
            return Some(found);
        }
        child = widget.next_sibling();
    }

    None
}

pub(crate) fn label_with_text(root: &gtk4::Widget, expected: &str) -> bool {
    if let Ok(label) = root.clone().downcast::<gtk4::Label>()
        && label.label() == expected
    {
        return true;
    }

    let mut child = root.first_child();
    while let Some(widget) = child {
        if label_with_text(&widget, expected) {
            return true;
        }
        child = widget.next_sibling();
    }

    false
}

pub(crate) fn painted_surface_fixture_with_classes(
    config: &Config,
    class_names: &[&str],
    width: i32,
    height: i32,
) -> PaintedSurfaceFixture {
    crate::services::config_manager::ConfigManager::replace_global_for_test(config.clone());

    let css_provider =
        CssProviderGuard::for_config(config, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let window = gtk4::Window::builder()
        .title("vibepanel UI regression surface test")
        .default_width(width + 40)
        .default_height(height + 40)
        .build();

    let surface = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    for class_name in class_names {
        surface.add_css_class(class_name);
    }
    surface.set_size_request(width, height);
    window.set_child(Some(&surface));
    window.present();
    flush_gtk();

    PaintedSurfaceFixture {
        window,
        _css_provider: css_provider,
        surface: surface.upcast(),
    }
}

pub(crate) fn sample_widget_pixel(
    window: &gtk4::Window,
    root: &gtk4::Widget,
    x: i32,
    y: i32,
) -> Rgba8 {
    let width = root.width();
    let height = root.height();
    assert!(
        width > 0 && height > 0,
        "widget should be allocated before sampling pixels"
    );
    assert!(
        x >= 0 && x < width,
        "sample x={x} outside rendered widget width={width}"
    );
    assert!(
        y >= 0 && y < height,
        "sample y={y} outside rendered widget height={height}"
    );

    let paintable = gtk4::WidgetPaintable::new(Some(root));
    let snapshot = gtk4::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot
        .to_node()
        .expect("widget paintable snapshot should produce a render node");
    let native = window
        .native()
        .expect("painted fixture should have a native surface");
    let surface = native
        .surface()
        .expect("painted fixture native should have a GDK surface");
    let renderer = gtk4::gsk::Renderer::for_surface(&surface)
        .expect("painted fixture surface should have a GSK renderer");
    let viewport = gtk4::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&viewport));
    let stride = texture.width() as usize * 4;
    let mut data = vec![0; stride * texture.height() as usize];
    texture.download(&mut data, stride);
    drop(texture);
    renderer.unrealize();

    let offset = y as usize * stride + x as usize * 4;
    Rgba8 {
        r: data[offset + 2],
        g: data[offset + 1],
        b: data[offset],
        a: data[offset + 3],
    }
}

pub(crate) fn center_pixel_of_surface(fixture: &PaintedSurfaceFixture) -> Rgba8 {
    sample_widget_pixel(
        &fixture.window,
        &fixture.surface,
        fixture.surface.width() / 2,
        fixture.surface.height() / 2,
    )
}

pub(crate) fn edge_pixel_of_surface(fixture: &PaintedSurfaceFixture) -> Rgba8 {
    sample_widget_pixel(
        &fixture.window,
        &fixture.surface,
        1,
        fixture.surface.height() / 2,
    )
}

pub(crate) fn assert_pixel_close(observed: Rgba8, expected: Rgba8, message: &str) {
    assert!(
        observed.close_to(expected, 2),
        "{message}; expected={expected:?}, observed={observed:?}"
    );
}
