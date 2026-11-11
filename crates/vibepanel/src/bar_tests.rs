use super::*;
use crate::popover_registry::{self, DispatchAction};
use crate::services::compositor::CompositorManager;
use crate::ui_regression_test_support::{
    TestDir, find_descendant_with_class, first_monitor_or_skip, flush_gtk,
    init_layer_shell_or_skip, registered_test_app, run_ignored_contract_subprocess,
};
use crate::widgets::PopoverKind::{System, Unmergeable};
use crate::widgets::layer_shell_popover::{
    LayerShellPopover, PopoverAnchor, calculate_bar_exclusive_zone, calculate_popover_bar_margin,
    popover_bar_edge,
};
use vibepanel_core::config::{BarPosition, WidgetPlacement};

fn layer_shell_test_config() -> Config {
    let mut config = Config::default();
    config.theme.mode = "dark".to_string();
    config.bar.size = 32;
    config.bar.spacing = 8;
    config.bar.inset = 8;
    config.bar.screen_margin = 0;
    config.theme.animations = false;
    config.advanced.compositor = "mango".to_string();
    config.widgets.left = vec![WidgetPlacement::Single("custom-a".to_string())];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let mut custom_a = vibepanel_core::config::WidgetOptions::default();
    custom_a
        .options
        .insert("label".to_string(), toml::Value::String("A".to_string()));
    config
        .widgets
        .widget_configs
        .insert("custom-a".to_string(), custom_a);

    config
}

struct LayerShellContext {
    app: Application,
    monitor: gtk4::gdk::Monitor,
}

fn layer_shell_context_or_skip() -> Option<LayerShellContext> {
    let display = init_layer_shell_or_skip("layer-shell contract")?;
    let monitor = first_monitor_or_skip(&display, "layer-shell contract")?;

    Some(LayerShellContext {
        app: registered_test_app("dev.vibepanel.layer-shell-contract"),
        monitor,
    })
}

fn apply_layer_shell_config(config: &Config, init_compositor: bool) {
    ConfigManager::replace_global_for_test(config.clone());
    if init_compositor {
        CompositorManager::init_global(&config.advanced);
    }
    load_css(config);
}

struct LayerShellBarFixture {
    window: ApplicationWindow,
    _state: BarState,
    _visibility: Rc<crate::services::bar_visibility::BarVisibilityController>,
}

fn present_layer_shell_bar(
    context: &LayerShellContext,
    config: &Config,
    init_compositor: bool,
) -> LayerShellBarFixture {
    apply_layer_shell_config(config, init_compositor);
    let mut state = BarState::new();
    let (window, visibility) = create_bar_window(
        &context.app,
        config,
        &context.monitor,
        context
            .monitor
            .connector()
            .as_deref()
            .unwrap_or("layer-test"),
        &mut state,
    );
    window.present();
    flush_gtk();

    LayerShellBarFixture {
        window,
        _state: state,
        _visibility: visibility,
    }
}

fn run_layer_shell_contract_subprocess(contract: &str) {
    run_ignored_contract_subprocess(
        "layer_shell_contract_runner",
        "VIBEPANEL_LAYER_SHELL_CONTRACT",
        contract,
        "layer-shell contract",
    );
}

fn test_bar_position(position: &str) -> BarPosition {
    BarPosition::parse(position).unwrap_or_else(|| panic!("invalid bar position: {position}"))
}

fn bar_edge_for_position(position: BarPosition) -> gtk4_layer_shell::Edge {
    match position {
        BarPosition::Top => gtk4_layer_shell::Edge::Top,
        BarPosition::Bottom => gtk4_layer_shell::Edge::Bottom,
        BarPosition::Left => gtk4_layer_shell::Edge::Left,
        BarPosition::Right => gtk4_layer_shell::Edge::Right,
    }
}

fn expect_bar_window_anchor(position: BarPosition, edge: gtk4_layer_shell::Edge) -> bool {
    edge == bar_edge_for_position(position)
        || match edge {
            gtk4_layer_shell::Edge::Top | gtk4_layer_shell::Edge::Bottom => position.is_vertical(),
            gtk4_layer_shell::Edge::Left | gtk4_layer_shell::Edge::Right => {
                position.is_horizontal()
            }
            _ => false,
        }
}

fn expect_popover_anchor(position: BarPosition, edge: gtk4_layer_shell::Edge) -> bool {
    edge == bar_edge_for_position(position)
        || match edge {
            gtk4_layer_shell::Edge::Right => position.is_horizontal(),
            gtk4_layer_shell::Edge::Bottom => position.is_vertical(),
            _ => false,
        }
}

fn assert_bar_window_anchors(case: &str, window: &ApplicationWindow, position: BarPosition) {
    for edge in [
        gtk4_layer_shell::Edge::Top,
        gtk4_layer_shell::Edge::Right,
        gtk4_layer_shell::Edge::Bottom,
        gtk4_layer_shell::Edge::Left,
    ] {
        assert_eq!(
            window.is_anchor(edge),
            expect_bar_window_anchor(position, edge),
            "{case}: bar window {edge:?} anchor should track bar.position={}",
            position.as_str()
        );
    }
}

fn assert_popover_window_anchors(case: &str, window: &ApplicationWindow, position: BarPosition) {
    for edge in [
        gtk4_layer_shell::Edge::Top,
        gtk4_layer_shell::Edge::Right,
        gtk4_layer_shell::Edge::Bottom,
        gtk4_layer_shell::Edge::Left,
    ] {
        assert_eq!(
            window.is_anchor(edge),
            expect_popover_anchor(position, edge),
            "{case}: popover {edge:?} anchor should track bar.position={}",
            position.as_str()
        );
    }
}

fn run_layer_shell_bar_position_contract() {
    struct Case {
        name: &'static str,
        position: &'static str,
        padding: Option<u32>,
        background_opacity: Option<f64>,
    }

    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };
    let monitor_geometry = context.monitor.geometry();
    let cases = [
        Case {
            name: "top",
            position: "top",
            padding: None,
            background_opacity: None,
        },
        Case {
            name: "bottom",
            position: "bottom",
            padding: None,
            background_opacity: None,
        },
        Case {
            name: "left",
            position: "left",
            padding: None,
            background_opacity: None,
        },
        Case {
            name: "right",
            position: "right",
            padding: None,
            background_opacity: None,
        },
        Case {
            name: "height with visible background",
            position: "top",
            padding: Some(10),
            background_opacity: Some(1.0),
        },
        Case {
            name: "height with transparent background",
            position: "top",
            padding: Some(10),
            background_opacity: Some(0.0),
        },
    ];

    for case in cases {
        let name = case.name;
        let position = test_bar_position(case.position);
        let mut config = layer_shell_test_config();
        config.bar.position = position.as_str().to_string();
        if let Some(padding) = case.padding {
            config.bar.padding = padding;
        }
        if let Some(background_opacity) = case.background_opacity {
            config.bar.background_opacity = background_opacity;
        }
        let bar = present_layer_shell_bar(&context, &config, false);
        let window = &bar.window;
        let expected_thickness = rendered_bar_height(&config);
        let (default_width, default_height) = window.default_size();

        assert!(
            window.is_layer_window(),
            "{name}: bar should be a layer window"
        );
        assert_bar_window_anchors(name, window, position);
        if position.is_vertical() {
            assert_eq!(default_width, expected_thickness, "{name}: default width");
            assert_eq!(
                default_height,
                monitor_geometry.height(),
                "{name}: default height"
            );
        } else {
            assert_eq!(
                default_width,
                monitor_geometry.width(),
                "{name}: default width"
            );
            assert_eq!(default_height, expected_thickness, "{name}: default height");
            assert!(
                window.height() >= expected_thickness,
                "{name}: rendered height {} should be at least {expected_thickness}",
                window.height()
            );
        }
        assert_eq!(
            window.layer(),
            gtk4_layer_shell::Layer::Top,
            "{name}: layer"
        );
        assert_eq!(
            window.keyboard_mode(),
            gtk4_layer_shell::KeyboardMode::None,
            "{name}: keyboard mode"
        );
        assert!(
            window.auto_exclusive_zone_is_enabled(),
            "{name}: auto exclusive zone"
        );
        assert_eq!(
            window.namespace().as_deref(),
            Some("vibepanel"),
            "{name}: namespace"
        );
        assert_eq!(
            window.monitor().as_ref(),
            Some(&context.monitor),
            "{name}: monitor"
        );

        window.close();
        flush_gtk();
    }
}

fn run_layer_shell_popover_on_show_reversal_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.theme.animations = true;
    apply_layer_shell_config(&config, true);
    let show_count = Rc::new(Cell::new(0));
    let close_count = Rc::new(Cell::new(0));
    let popover = LayerShellPopover::new(&context.app, "contract", || {
        gtk4::Label::new(Some("contract popover")).upcast::<gtk4::Widget>()
    });
    let show_count_for_callback = show_count.clone();
    popover.set_on_show(move || show_count_for_callback.set(show_count_for_callback.get() + 1));
    let close_count_for_callback = close_count.clone();
    popover.set_on_close(move || close_count_for_callback.set(close_count_for_callback.get() + 1));
    let anchor = PopoverAnchor { x: 160, y: 160 };

    popover.show_at(anchor, Some(context.monitor.clone()));
    assert_eq!(show_count.get(), 1);
    popover.hide();
    popover.show_at(anchor, Some(context.monitor.clone()));
    assert_eq!(show_count.get(), 2);
    assert_eq!(close_count.get(), 0);

    if let Some(window) = popover.test_window() {
        window.close();
    }
    if let Some(catcher) = popover.test_click_catcher() {
        catcher.close();
    }
    flush_gtk();
}

fn run_layer_shell_click_catcher_lifetime_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    apply_layer_shell_config(&layer_shell_test_config(), true);
    let popover = LayerShellPopover::new(&context.app, "contract", || {
        gtk4::Label::new(Some("contract popover")).upcast::<gtk4::Widget>()
    });
    let anchor = PopoverAnchor { x: 160, y: 160 };

    popover.show_at(anchor, Some(context.monitor.clone()));
    flush_gtk();
    let catcher = popover
        .test_click_catcher()
        .expect("an open popover has a click-catcher");

    // The catcher's content is tiny; clicks elsewhere must still reach the
    // window's dismiss gesture.
    let (width, height) = (catcher.width(), catcher.height());
    assert!(width > 1 && height > 1, "catcher should span the output");
    let picked = catcher
        .pick(
            f64::from(width) / 2.0,
            f64::from(height) / 2.0,
            gtk4::PickFlags::DEFAULT,
        )
        .expect("a click inside the catcher hits a widget");
    assert_eq!(picked, catcher.clone().upcast::<gtk4::Widget>());

    // A hidden catcher would keep its fullscreen GPU buffers.
    popover.hide();
    flush_gtk();
    assert!(popover.test_click_catcher().is_none());
    assert!(
        !context
            .app
            .windows()
            .iter()
            .any(|w| w == catcher.upcast_ref::<gtk4::Window>())
    );

    popover.show_at(anchor, Some(context.monitor.clone()));
    flush_gtk();
    let reopened = popover
        .test_click_catcher()
        .expect("reopening creates a new click-catcher");
    assert_ne!(reopened, catcher);

    if let Ok(pointer) = std::env::var("VIBEPANEL_TEST_POINTER") {
        // Far from both the popover and the catcher's 1×1 content.
        std::fs::write(&pointer, "1000 600 click").unwrap();
        wait_for_bar_contract(|| popover.test_click_catcher().is_none());
    }

    popover.hide();
    if let Some(window) = popover.test_window() {
        window.close();
    }
    flush_gtk();
}

fn run_layer_shell_popover_placement_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let cases = [
        ("top, transparent bar", "top", 0.0),
        ("top, visible bar", "top", 1.0),
        ("bottom, transparent bar", "bottom", 0.0),
        ("bottom, visible bar", "bottom", 1.0),
        ("left, transparent bar", "left", 0.0),
        ("left, visible bar", "left", 1.0),
        ("right, transparent bar", "right", 0.0),
        ("right, visible bar", "right", 1.0),
    ];

    for (name, position, background_opacity) in cases {
        let position = test_bar_position(position);
        let mut config = layer_shell_test_config();
        config.bar.position = position.as_str().to_string();
        config.bar.padding = 10;
        config.bar.popover_offset = 23;
        config.bar.background_opacity = background_opacity;
        apply_layer_shell_config(&config, true);

        let popover = LayerShellPopover::new(&context.app, "contract", || {
            gtk4::Label::new(Some("contract popover")).upcast::<gtk4::Widget>()
        });
        popover.show_at(
            PopoverAnchor { x: 160, y: 160 },
            Some(context.monitor.clone()),
        );
        flush_gtk();

        let Some(window) = popover.test_window() else {
            panic!("{name}: popover should create a layer-shell window when shown");
        };
        let Some(catcher) = popover.test_click_catcher() else {
            panic!("{name}: popover should create a click-catcher when shown");
        };

        let bar_edge = popover_bar_edge();
        let expected_bar_margin = if background_opacity > 0.0 {
            config.bar.popover_offset as i32 - config.bar.padding as i32
        } else {
            config.bar.popover_offset as i32
        };

        assert!(window.is_layer_window(), "{name}: popover layer window");
        assert_popover_window_anchors(name, &window, position);
        assert_eq!(
            window.layer(),
            gtk4_layer_shell::Layer::Top,
            "{name}: popover layer"
        );
        assert_ne!(
            window.keyboard_mode(),
            gtk4_layer_shell::KeyboardMode::None,
            "{name}: popover keyboard mode"
        );
        assert_eq!(window.exclusive_zone(), 0, "{name}: popover exclusive zone");
        assert_eq!(
            window.namespace().as_deref(),
            Some("vibepanel-contract-popover"),
            "{name}: popover namespace"
        );
        assert_eq!(
            window.monitor().as_ref(),
            Some(&context.monitor),
            "{name}: popover monitor"
        );
        assert_eq!(
            window.margin(bar_edge),
            expected_bar_margin,
            "{name}: popover bar-edge margin"
        );

        assert!(catcher.is_layer_window(), "{name}: catcher layer window");
        for edge in [
            gtk4_layer_shell::Edge::Top,
            gtk4_layer_shell::Edge::Right,
            gtk4_layer_shell::Edge::Bottom,
            gtk4_layer_shell::Edge::Left,
        ] {
            assert!(catcher.is_anchor(edge), "{name}: catcher {edge:?} anchor");
        }
        assert_eq!(
            catcher.layer(),
            gtk4_layer_shell::Layer::Top,
            "{name}: catcher layer"
        );
        assert_eq!(
            catcher.keyboard_mode(),
            gtk4_layer_shell::KeyboardMode::None,
            "{name}: catcher keyboard mode"
        );
        assert_eq!(
            catcher.exclusive_zone(),
            -1,
            "{name}: catcher exclusive zone"
        );
        assert_eq!(
            catcher.namespace().as_deref(),
            Some("vibepanel-click-catcher"),
            "{name}: catcher namespace"
        );
        assert_eq!(
            catcher.monitor().as_ref(),
            Some(&context.monitor),
            "{name}: catcher monitor"
        );
        assert_eq!(
            catcher.margin(bar_edge),
            calculate_bar_exclusive_zone(),
            "{name}: catcher bar-edge margin"
        );

        popover.hide();
        window.close();
        catcher.close();
        flush_gtk();
    }
}

fn run_layer_shell_widget_popovers_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let cases = [
        ("clock", "clock", crate::styles::calendar::POPOVER),
        ("system", "cpu", crate::styles::system_popover::POPOVER),
    ];

    for (name, widget, content_class) in cases {
        let mut config = layer_shell_test_config();
        config.widgets.left = vec![WidgetPlacement::Single(widget.to_string())];
        let bar = present_layer_shell_bar(&context, &config, true);

        assert!(
            popover_registry::dispatch(widget, DispatchAction::Show),
            "{name}: {widget} widget should register a popover handle"
        );
        flush_gtk();

        let Some(popover_window) = popover_registry::test_layer_shell_window(widget) else {
            panic!("{name}: registry handle should expose its real layer-shell popover window");
        };
        let Some(child) = popover_window.child() else {
            panic!("{name}: popover window should have content after being shown");
        };

        let bar_edge = popover_bar_edge();
        let expected_bar_margin = calculate_popover_bar_margin();

        assert!(popover_window.is_layer_window(), "{name}: layer window");
        for (edge, anchored) in [
            (gtk4_layer_shell::Edge::Top, true),
            (gtk4_layer_shell::Edge::Right, true),
            (gtk4_layer_shell::Edge::Bottom, false),
            (gtk4_layer_shell::Edge::Left, false),
        ] {
            assert_eq!(
                popover_window.is_anchor(edge),
                anchored,
                "{name}: {edge:?} anchor"
            );
        }
        assert_eq!(
            popover_window.layer(),
            gtk4_layer_shell::Layer::Top,
            "{name}: layer"
        );
        assert_eq!(
            popover_window.namespace().as_deref(),
            Some(format!("vibepanel-{widget}-popover").as_str()),
            "{name}: namespace"
        );
        assert_eq!(
            popover_window.monitor().as_ref(),
            Some(&context.monitor),
            "{name}: monitor"
        );
        assert_eq!(
            popover_window.margin(bar_edge),
            expected_bar_margin,
            "{name}: bar-edge margin"
        );
        assert!(
            find_descendant_with_class(&child, content_class).is_some(),
            "{name}: popover should contain {content_class} content"
        );

        popover_registry::dispatch(widget, DispatchAction::Hide);
        popover_window.close();
        bar.window.close();
        flush_gtk();
    }
}

fn label_in(widget: &gtk4::Widget) -> Option<gtk4::Label> {
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        return Some(label.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(label) = label_in(&current) {
            return Some(label);
        }
        child = current.next_sibling();
    }
    None
}

#[track_caller]
fn wait_for_bar_contract(condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        flush_gtk();
        if condition() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "bar visibility condition timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn run_layer_shell_auto_hide_contract(position: &str, opacity: f64, animated: bool) {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };
    let mut config = layer_shell_test_config();
    config.bar.position = position.to_string();
    config.bar.screen_margin = if opacity > 0.0 { 0 } else { 12 };
    config.bar.background_opacity = opacity;
    config.theme.animations = animated;
    config.bar.auto_hide = vibepanel_core::config::AutoHide::Always;
    config.bar.hide_delay_ms = 60;
    config.bar.reveal_delay_ms = 30;
    config.widgets.center = vec![WidgetPlacement::Single("clock".into())];
    config.advanced.compositor = "auto".into();
    let bar = present_layer_shell_bar(&context, &config, true);
    let controller = &bar._visibility;
    let trigger = controller.reveal_trigger().unwrap();
    wait_for_bar_contract(|| !controller.test_footprint().is_empty());
    assert!(!bar._visibility.test_shown());
    assert_eq!(bar.window.exclusive_zone(), -1);
    assert!(
        bar.window.is_mapped(),
        "automatic hide preserves GTK allocations"
    );
    assert!(trigger.is_visible());
    assert_eq!(trigger.exclusive_zone(), -1);

    if config.bar.position().is_horizontal() && opacity == 0.0 {
        let before = controller.test_footprint()[0].width;
        label_in(bar.window.upcast_ref())
            .unwrap()
            .set_label("A much longer hidden widget label");
        wait_for_bar_contract(|| controller.test_footprint()[0].width > before);
        assert!(!bar._visibility.test_shown());
    }

    if let Ok(pointer) = std::env::var("VIBEPANEL_TEST_POINTER") {
        let client = ApplicationWindow::builder()
            .application(&context.app)
            .title("vibepanel-auto-hide-client")
            .default_width(600)
            .default_height(400)
            .build();
        client.set_child(Some(&gtk4::Label::new(Some("Application below the bar"))));
        client.present();
        std::fs::write(&pointer, "640 400").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fn screenshot() -> Vec<u8> {
            let result = std::process::Command::new("grim")
                .args(["-t", "ppm", "-"])
                .output()
                .unwrap();
            assert!(result.status.success());
            result.stdout
        }
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(150);
        wait_for_bar_contract(|| std::time::Instant::now() >= settle);
        let before = screenshot();
        let edge = match position {
            "bottom" => "640 799",
            "left" => "0 400",
            "right" => "1279 400",
            _ => "640 0",
        };
        std::fs::write(&pointer, edge).unwrap();
        wait_for_bar_contract(|| bar._visibility.test_shown());
        if animated {
            wait_for_bar_contract(|| {
                let p = controller.test_progress();
                p > 0.0 && p < 1.0
            });
        }
        // Revealing must not require a second pointer motion to stay open.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(
            bar._visibility.test_shown(),
            "stationary edge hover holds the bar"
        );
        std::fs::write(&pointer, format!("{edge} click")).unwrap();
        wait_for_bar_contract(|| {
            popover_registry::test_layer_shell_window("clock")
                .is_some_and(|window| window.is_visible())
        });
        assert!(controller.test_shown(), "edge click keeps the bar revealed");
        popover_registry::dispatch("clock", DispatchAction::Hide);
        let blank_edge = match position {
            "bottom" => "320 799",
            "left" => "0 200",
            "right" => "1279 200",
            _ => "320 0",
        };
        std::fs::write(&pointer, blank_edge).unwrap();
        for button in ["click", "right-click"] {
            popover_registry::dispatch("clock", DispatchAction::Show);
            wait_for_bar_contract(|| {
                PopoverTracker::global()
                    .holds_output(context.monitor.connector().as_deref().unwrap())
            });
            flush_gtk();
            std::fs::write(&pointer, format!("{blank_edge} {button}")).unwrap();
            wait_for_bar_contract(|| {
                !PopoverTracker::global()
                    .holds_output(context.monitor.connector().as_deref().unwrap())
            });
        }
        let inside = match position {
            "bottom" => "640 780",
            "left" => "20 400",
            "right" => "1260 400",
            _ => "640 20",
        };
        std::fs::write(&pointer, inside).unwrap();
        // Stay over the bar long enough to exercise enter and leave delivery.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(bar._visibility.test_shown(), "hover holds the revealed bar");
        assert!(
            before != screenshot(),
            "revealed bar reaches the compositor"
        );
        std::fs::write(&pointer, "640 400").unwrap();
        wait_for_bar_contract(|| !bar._visibility.test_shown());
        if animated {
            wait_for_bar_contract(|| {
                let p = controller.test_progress();
                p > 0.0 && p < 1.0
            });
        }
        wait_for_bar_contract(|| controller.test_progress() == 0.0);
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(150);
        wait_for_bar_contract(|| std::time::Instant::now() >= settle);
        let after = screenshot();
        // Fractional-scale damage can resample a thin application border by
        // a few RGB levels. Allow only a tiny number of low-contrast changes;
        // retained bar backgrounds or text must still fail this comparison.
        let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        let max_delta = before
            .iter()
            .zip(&after)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        let cleared =
            before.len() == after.len() && max_delta <= 12 && changed <= before.len() / 200;
        if !cleared {
            let directory = std::env::var("XDG_RUNTIME_DIR").unwrap();
            std::fs::write(format!("{directory}/before-{position}.ppm"), &before).unwrap();
            std::fs::write(format!("{directory}/after-{position}.ppm"), &after).unwrap();
        }
        assert!(
            cleared,
            "hidden bar pixels remain on screen: {changed} changed channels, maximum delta {max_delta}"
        );
        client.close();
    }

    controller.test_edge_hover(true);
    wait_for_bar_contract(|| bar._visibility.test_shown());
    assert!(trigger.is_visible(), "keep the edge target while hovered");
    assert_eq!(bar.window.exclusive_zone(), -1);
    controller.test_edge_hover(false);
    assert!(
        !trigger.is_visible(),
        "release the edge target after leaving"
    );
    wait_for_bar_contract(|| !bar._visibility.test_shown());

    if animated {
        wait_for_bar_contract(|| {
            let p = controller.test_progress();
            p > 0.0 && p < 1.0
        });
        let before_reversal = controller.test_progress();
        controller.set_ipc_shown(true);
        assert_eq!(
            controller.test_progress(),
            before_reversal,
            "reversal does not jump"
        );
        controller.test_edge_hover(true);
        wait_for_bar_contract(|| controller.test_progress() == 1.0);
        controller.test_edge_hover(false);
        controller.set_ipc_shown(false);
        wait_for_bar_contract(|| controller.test_progress() == 0.0);
    }

    if !animated {
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(100);
        wait_for_bar_contract(|| std::time::Instant::now() >= settle);
        let evaluations = controller.test_evaluations();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(200);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert_eq!(
            controller.test_evaluations(),
            evaluations,
            "idle auto-hide must not poll"
        );

        let native = gtk4::Popover::new();
        crate::widgets::configure_popover(&native);
        native.set_child(Some(&gtk4::Label::new(Some("Native menu"))));
        native.set_parent(&bar.window.child().unwrap());
        native.popup();
        wait_for_bar_contract(|| controller.test_shown());
        let until = std::time::Instant::now() + std::time::Duration::from_millis(150);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(
            controller.test_shown(),
            "native popup holds without pointer hover"
        );
        native.popdown();
        wait_for_bar_contract(|| !controller.test_shown());
        native.unparent();
    }

    let popover = LayerShellPopover::new(&context.app, "autohide-contract", || {
        gtk4::Label::new(Some("popover")).upcast::<gtk4::Widget>()
    });
    let id = PopoverTracker::global().set_active(
        popover.clone(),
        context.monitor.connector().map(|s| s.to_string()),
    );
    popover.show_at(
        PopoverAnchor { x: 100, y: 100 },
        Some(context.monitor.clone()),
    );
    wait_for_bar_contract(|| bar._visibility.test_shown());
    let popover_window = popover.test_window().unwrap();
    assert_eq!(popover_window.exclusive_zone(), -1);
    assert_eq!(
        popover_window.margin(popover_bar_edge()),
        calculate_popover_bar_margin()
    );
    popover.hide();
    wait_for_bar_contract(|| !controller.test_shown());
    PopoverTracker::global().clear_if_active(id);

    if !animated {
        let qs = crate::widgets::QuickSettingsWindowHandle::new(
            context.app.clone(),
            crate::widgets::QuickSettingsConfig::default(),
        );
        qs.toggle_at(
            PopoverAnchor { x: 100, y: 100 },
            Some(context.monitor.clone()),
        );
        wait_for_bar_contract(|| controller.test_shown());
        qs.test_close_directly();
        wait_for_bar_contract(|| !controller.test_shown());
        qs.destroy();
    }

    controller.set_ipc_shown(true);
    wait_for_bar_contract(|| controller.test_progress() == 1.0);
    let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
    wait_for_bar_contract(|| std::time::Instant::now() >= until);
    assert!(
        controller.test_shown(),
        "IPC show pins beyond the hide delay"
    );
    controller.set_ipc_shown(false);
    wait_for_bar_contract(|| controller.test_progress() == 0.0);
    assert!(
        bar.window.is_visible(),
        "automatic hide keeps the surface mapped"
    );
    assert!(trigger.is_visible(), "IPC hide restores edge reveal");
    controller.test_edge_hover(true);
    wait_for_bar_contract(|| controller.test_shown());
    controller.test_edge_hover(false);
    wait_for_bar_contract(|| !controller.test_shown());
    controller.set_monitor_suppressed(true);
    assert!(!trigger.is_visible());
    controller.set_monitor_suppressed(false);
    assert!(trigger.is_visible());
    bar.window.close();
    drop(bar);
    assert!(
        !trigger.is_visible(),
        "dropping a bar removes its reveal trigger"
    );

    if !animated {
        let manager = crate::services::bar_manager::BarManager::global();
        manager.init(&context.app);
        let key = manager
            .create_bar_for_monitor(&context.monitor, 0, &config)
            .unwrap();
        manager.show_all();
        let visibility = manager.test_visibility(&key);
        wait_for_bar_contract(|| !visibility.is_shown());
        manager.ipc_toggle();
        wait_for_bar_contract(|| visibility.is_shown());
        let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(visibility.is_shown(), "toggle pins an auto-hidden bar");
        manager.ipc_toggle();
        wait_for_bar_contract(|| !visibility.is_shown());
        visibility.test_edge_hover(true);
        wait_for_bar_contract(|| visibility.is_shown());
        visibility.test_edge_hover(false);
        wait_for_bar_contract(|| !visibility.is_shown());
        manager.remove_bar(&key);
    }
}

fn run_layer_shell_intellihide_sway_contract() {
    // This case deliberately creates and moves application windows. Restrict it
    // to the isolated compositor started by run-auto-hide-tests.sh.
    if std::env::var("VIBEPANEL_HEADLESS_SWAY_TEST").as_deref() != Ok("1") {
        return;
    }
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };
    let mut config = layer_shell_test_config();
    config.advanced.compositor = "sway".into();
    config.bar.auto_hide = vibepanel_core::config::AutoHide::Smart;
    config.bar.hide_delay_ms = 30;
    let bar = present_layer_shell_bar(&context, &config, true);
    wait_for_bar_contract(|| bar._visibility.test_shown());
    let client = ApplicationWindow::builder()
        .application(&context.app)
        .title("vibepanel-intellihide-client")
        .default_width(300)
        .default_height(200)
        .build();
    client.set_child(Some(&gtk4::Label::new(Some("Tiled client"))));
    let clicks = Rc::new(Cell::new(0));
    let gesture = GestureClick::new();
    let clicked = clicks.clone();
    gesture.connect_pressed(move |_, _, _, _| clicked.set(clicked.get() + 1));
    client.add_controller(gesture);
    client.present();
    wait_for_bar_contract(|| !bar._visibility.test_shown());
    bar._visibility.set_ipc_shown(true);
    wait_for_bar_contract(|| bar._visibility.test_shown());
    bar._visibility.set_ipc_shown(false);
    wait_for_bar_contract(|| !bar._visibility.test_shown());
    fn command(command: &str) {
        let result = std::process::Command::new("swaymsg")
            .args(["-q", command])
            .status()
            .unwrap();
        assert!(result.success());
    }
    command(
        "[title=\"^vibepanel-intellihide-client$\"] floating enable, border none, resize set 300 200, move position 500 300",
    );
    wait_for_bar_contract(|| bar._visibility.test_shown());
    command("[title=\"^vibepanel-intellihide-client$\"] move position 200 0");
    wait_for_bar_contract(|| bar._visibility.test_shown());
    let pointer = std::env::var("VIBEPANEL_TEST_POINTER").unwrap();
    std::fs::write(&pointer, "250 10 click").unwrap();
    wait_for_bar_contract(|| clicks.get() == 1);
    assert!(
        bar._visibility.test_shown(),
        "island gap passes clicks to the app"
    );
    let popup = LayerShellPopover::new(&context.app, "gap-dismiss-contract", || {
        gtk4::Label::new(Some("Gap dismissal")).upcast::<gtk4::Widget>()
    });
    let tracker_id = PopoverTracker::global().set_active(
        popup.clone(),
        context.monitor.connector().map(|name| name.to_string()),
    );
    popup.show_at(
        PopoverAnchor { x: 50, y: 50 },
        Some(context.monitor.clone()),
    );
    wait_for_bar_contract(|| popup.test_window().is_some_and(|window| window.is_mapped()));
    let settled = std::time::Instant::now() + std::time::Duration::from_millis(100);
    wait_for_bar_contract(|| std::time::Instant::now() >= settled);
    std::fs::write(&pointer, "251 10 click").unwrap();
    wait_for_bar_contract(|| !popup.is_visible());
    assert_eq!(
        clicks.get(),
        1,
        "popup dismissal must not click the application"
    );
    PopoverTracker::global().clear_if_active(tracker_id);
    let settled = std::time::Instant::now() + std::time::Duration::from_millis(100);
    wait_for_bar_contract(|| std::time::Instant::now() >= settled);
    std::fs::write(&pointer, "252 10 click").unwrap();
    wait_for_bar_contract(|| clicks.get() == 2);
    std::fs::write(&pointer, "600 600").unwrap();
    let label = label_in(bar.window.upcast_ref()).unwrap();
    label.set_label("A much longer widget that now overlaps the floating window");
    wait_for_bar_contract(|| !bar._visibility.test_shown());
    label.set_label("A");
    wait_for_bar_contract(|| bar._visibility.test_shown());
    std::fs::write(&pointer, "250 10 click").unwrap();
    wait_for_bar_contract(|| clicks.get() == 3);
    assert!(
        bar._visibility.test_shown(),
        "resized islands update the input region"
    );
    std::fs::write(&pointer, "600 600").unwrap();
    command("[title=\"^vibepanel-intellihide-client$\"] move position 0 0");
    wait_for_bar_contract(|| !bar._visibility.test_shown());
    command("seat seat0 cursor set 10 0");
    wait_for_bar_contract(|| bar._visibility.test_shown());
    command("seat seat0 cursor set 600 600");
    wait_for_bar_contract(|| !bar._visibility.test_shown());
    command("create_output");
    command("output HEADLESS-1 position 0 0");
    command("output HEADLESS-2 mode 1280x800 position 1280 0");
    let display = gtk4::gdk::Display::default().unwrap();
    let monitors = display.monitors();
    wait_for_bar_contract(|| monitors.n_items() == 2);
    let second = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gtk4::gdk::Monitor>())
        .find(|m| m.connector().as_deref() == Some("HEADLESS-2"))
        .unwrap();
    let second_context = LayerShellContext {
        app: context.app.clone(),
        monitor: second,
    };
    let second_bar = present_layer_shell_bar(&second_context, &config, false);
    wait_for_bar_contract(|| second_bar._visibility.test_shown());
    // Center stays on HEADLESS-1, but the rightmost 120px overlap HEADLESS-2.
    command("[title=\"^vibepanel-intellihide-client$\"] move position 1100 0");
    wait_for_bar_contract(|| !second_bar._visibility.test_shown());
    command("[title=\"^vibepanel-intellihide-client$\"] move position 500 300");
    wait_for_bar_contract(|| second_bar._visibility.test_shown());
    second_bar.window.close();
    drop(second_bar);
    bar.window.close();
    drop(bar);

    // Animated island hide: a pointer moving in a gap must not become hover
    // and reverse the slide.
    let mut animated = config.clone();
    animated.theme.animations = true;
    let island = present_layer_shell_bar(&context, &animated, false);
    wait_for_bar_contract(|| island._visibility.test_progress() == 1.0);
    std::fs::write(&pointer, "250 10").unwrap();
    command("[title=\"^vibepanel-intellihide-client$\"] move position 0 0");
    wait_for_bar_contract(|| !island._visibility.test_shown());
    let start = std::time::Instant::now();
    let moves = Cell::new(0);
    wait_for_bar_contract(|| {
        assert!(
            !island._visibility.test_shown(),
            "gap motion during the hide slide must not reveal the bar"
        );
        let elapsed = start.elapsed().as_millis();
        if moves.get() < 5 && elapsed >= moves.get() * 40 {
            moves.set(moves.get() + 1);
            std::fs::write(&pointer, format!("{} 10", 250 + moves.get())).unwrap();
        }
        elapsed >= 600
    });
    assert_eq!(island._visibility.test_progress(), 0.0);
    std::fs::write(&pointer, "600 600").unwrap();
    command("[title=\"^vibepanel-intellihide-client$\"] move position 500 300");
    island.window.close();
    drop(island);
    ConfigManager::replace_global_for_test(config.clone());

    command("[title=\"^vibepanel-intellihide-client$\"] floating disable");

    let manager = crate::services::bar_manager::BarManager::global();
    manager.init(&context.app);
    let first_key = manager
        .create_bar_for_monitor(&context.monitor, 0, &config)
        .unwrap();
    let second_key = manager
        .create_bar_for_monitor(&second_context.monitor, 1, &config)
        .unwrap();
    manager.show_all();
    {
        let tiled = manager.test_visibility(&first_key);
        let empty = manager.test_visibility(&second_key);
        wait_for_bar_contract(|| !tiled.is_shown() && empty.is_shown());
        manager.ipc_toggle();
        wait_for_bar_contract(|| tiled.is_shown() && empty.is_shown());
        let until = std::time::Instant::now() + std::time::Duration::from_millis(150);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(
            tiled.is_shown(),
            "empty output must not block pinning the tiled output"
        );
    }
    manager.reconfigure_all(&display, &config);
    {
        let tiled = manager.test_visibility(&first_key);
        let empty = manager.test_visibility(&second_key);
        wait_for_bar_contract(|| tiled.is_shown() && empty.is_shown());
        let until = std::time::Instant::now() + std::time::Duration::from_millis(150);
        wait_for_bar_contract(|| std::time::Instant::now() >= until);
        assert!(tiled.is_shown(), "rebuild preserves the pin");
        // Hide also retracts the empty output until its scene changes.
        manager.ipc_toggle();
        wait_for_bar_contract(|| !tiled.is_shown() && !empty.is_shown());
    }
    manager.remove_bar(&first_key);
    manager.remove_bar(&second_key);
    command("output HEADLESS-2 disable");
    command("output HEADLESS-1 position 0 0");
    client.close();
}

fn run_layer_shell_always_hotplug_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };
    let mut config = layer_shell_test_config();
    config.bar.background_opacity = 0.6;
    let height = crate::widgets::compute_max_scroll_height;
    config.bar.auto_hide = vibepanel_core::config::AutoHide::Never;
    apply_layer_shell_config(&config, false);
    let never_height = height(Some(context.monitor.clone()));
    for mode in [
        vibepanel_core::config::AutoHide::Always,
        vibepanel_core::config::AutoHide::Smart,
    ] {
        config.bar.auto_hide = mode;
        apply_layer_shell_config(&config, false);
        assert_eq!(
            height(Some(context.monitor.clone())),
            never_height,
            "{mode:?}: notification height must not count bar thickness twice"
        );
    }
    config.bar.auto_hide = vibepanel_core::config::AutoHide::Never;
    let bar = present_layer_shell_bar(&context, &config, false);
    let client = ApplicationWindow::builder()
        .application(&context.app)
        .title("vibepanel-hotplug-client")
        .default_width(600)
        .default_height(400)
        .build();
    client.set_child(Some(&gtk4::Label::new(Some("Tiled client"))));
    client.present();
    wait_for_bar_contract(|| client.height() > 0 && bar.window.exclusive_zone() > 0);
    let settle = std::time::Instant::now() + std::time::Duration::from_millis(150);
    wait_for_bar_contract(|| std::time::Instant::now() >= settle);
    let client_size = (client.width(), client.height());
    let reserved = bar.window.exclusive_zone();
    bar._visibility.set_monitor_suppressed(true);
    let settle = std::time::Instant::now() + std::time::Duration::from_millis(350);
    wait_for_bar_contract(|| std::time::Instant::now() >= settle);
    assert!(bar.window.is_mapped());
    assert_eq!(bar.window.exclusive_zone(), reserved);
    assert_eq!(
        (client.width(), client.height()),
        client_size,
        "hotplug suppression must not reflow tiled clients"
    );
    assert_eq!(bar._visibility.test_progress(), 0.0);
    bar._visibility.set_ipc_shown(false);
    bar._visibility.set_monitor_suppressed(false);
    assert!(
        !bar.window.is_visible(),
        "manual hide survives monitor recovery"
    );
    bar._visibility.set_ipc_shown(true);
    wait_for_bar_contract(|| bar.window.is_mapped() && bar._visibility.test_progress() == 1.0);
    if std::env::var("VIBEPANEL_HEADLESS_SWAY_TEST").as_deref() == Ok("1") {
        let clicks = Rc::new(std::cell::Cell::new(0));
        let gesture = gtk4::GestureClick::new();
        gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let received = clicks.clone();
        gesture.connect_pressed(move |_, _, _, _| received.set(received.get() + 1));
        bar.window.add_controller(gesture);
        let status = std::process::Command::new("swaymsg")
            .args(["-q", "output HEADLESS-1 mode 1600x900"])
            .status()
            .unwrap();
        assert!(status.success());
        wait_for_bar_contract(|| bar.window.width() == 1600);
        let pointer = std::env::var("VIBEPANEL_TEST_POINTER").unwrap();
        // Click beyond the original 1280px input region after resizing the output.
        std::fs::write(pointer, "1200 10 click").unwrap();
        wait_for_bar_contract(|| clicks.get() == 1);
        let status = std::process::Command::new("swaymsg")
            .args(["-q", "output HEADLESS-1 mode 1600x120"])
            .status()
            .unwrap();
        assert!(status.success());
        wait_for_bar_contract(|| context.monitor.geometry().height() == 120);
        assert!(
            height(Some(context.monitor.clone())) < 120,
            "short outputs must not use the 500px missing-monitor fallback"
        );
        let status = std::process::Command::new("swaymsg")
            .args(["-q", "output HEADLESS-1 mode 1280x800"])
            .status()
            .unwrap();
        assert!(status.success());
        wait_for_bar_contract(|| bar.window.width() == 1280);
    }
    client.close();
    bar.window.close();
    drop(bar);

    // Toggle-hidden state survives the output leaving and returning.
    let manager = crate::services::bar_manager::BarManager::global();
    manager.init(&context.app);
    let key = manager
        .create_bar_for_monitor(&context.monitor, 0, &config)
        .unwrap();
    manager.show_all();
    wait_for_bar_contract(|| manager.test_visibility(&key).is_shown());
    manager.ipc_toggle();
    wait_for_bar_contract(|| !manager.test_visibility(&key).is_shown());
    manager.remove_bar(&key);
    manager.create_bar_for_monitor(&context.monitor, 0, &config);
    manager.show_all();
    assert!(
        !manager.test_visibility(&key).is_shown(),
        "toggle-hidden output stays hidden after re-add"
    );
    manager.ipc_show();
    wait_for_bar_contract(|| manager.test_visibility(&key).is_shown());
    manager.remove_bar(&key);

    // IPC state recorded in one mode must not be replayed into another.
    if let Ok(pointer) = std::env::var("VIBEPANEL_TEST_POINTER") {
        std::fs::write(pointer, "600 600").unwrap();
    }
    let mut auto = config.clone();
    auto.bar.auto_hide = vibepanel_core::config::AutoHide::Always;
    auto.bar.hide_delay_ms = 30;
    // Never: toggle twice stores "shown", which would pin an automatic bar.
    manager.create_bar_for_monitor(&context.monitor, 0, &config);
    manager.ipc_toggle();
    wait_for_bar_contract(|| !manager.test_visibility(&key).is_shown());
    manager.ipc_toggle();
    wait_for_bar_contract(|| manager.test_visibility(&key).is_shown());
    manager.remove_bar(&key);
    manager.create_bar_for_monitor(&context.monitor, 0, &auto);
    manager.show_all();
    wait_for_bar_contract(|| !manager.test_visibility(&key).is_shown());
    // Always: toggle twice stores "released", which would hide a never-mode bar.
    manager.ipc_toggle();
    wait_for_bar_contract(|| manager.test_visibility(&key).is_shown());
    manager.ipc_toggle();
    wait_for_bar_contract(|| !manager.test_visibility(&key).is_shown());
    manager.remove_bar(&key);
    manager.create_bar_for_monitor(&context.monitor, 0, &config);
    manager.show_all();
    assert!(
        manager.test_visibility(&key).is_shown(),
        "automatic-mode release must not hide a never-mode bar"
    );
    manager.remove_bar(&key);
}

macro_rules! layer_shell_contract_tests {
    ($(($test_name:ident, $contract:literal, $runner:expr)),+ $(,)?) => {
        $(
            #[test]
            #[ignore = "layer-shell contract: requires a Wayland compositor with layer-shell support"]
            fn $test_name() {
                run_layer_shell_contract_subprocess($contract);
            }
        )+

        #[test]
        #[ignore = "internal runner for one layer-shell contract subprocess"]
        fn layer_shell_contract_runner() {
            match std::env::var("VIBEPANEL_LAYER_SHELL_CONTRACT").as_deref() {
                $(Ok($contract) => $runner,)+
                Ok(other) => panic!("unknown layer-shell contract: {other}"),
                Err(_) => eprintln!("skipping layer-shell contract runner: no contract selected"),
            }
        }
    };
}

layer_shell_contract_tests!(
    (
        test_layer_shell_always_hotplug,
        "bar.always.hotplug",
        run_layer_shell_always_hotplug_contract()
    ),
    (
        test_layer_shell_auto_hide_animated_top,
        "auto-hide.animated.top",
        run_layer_shell_auto_hide_contract("top", 0.6, true)
    ),
    (
        test_layer_shell_auto_hide_animated_bottom,
        "auto-hide.animated.bottom",
        run_layer_shell_auto_hide_contract("bottom", 0.6, true)
    ),
    (
        test_layer_shell_auto_hide_animated_left,
        "auto-hide.animated.left",
        run_layer_shell_auto_hide_contract("left", 0.6, true)
    ),
    (
        test_layer_shell_auto_hide_animated_right,
        "auto-hide.animated.right",
        run_layer_shell_auto_hide_contract("right", 0.6, true)
    ),
    (
        test_layer_shell_auto_hide_top,
        "auto-hide.top",
        run_layer_shell_auto_hide_contract("top", 0.0, false)
    ),
    (
        test_layer_shell_intellihide_sway,
        "intellihide.sway",
        run_layer_shell_intellihide_sway_contract()
    ),
    (
        test_layer_shell_bar_position,
        "bar.position",
        run_layer_shell_bar_position_contract()
    ),
    (
        test_layer_shell_popover_placement,
        "popover.placement",
        run_layer_shell_popover_placement_contract()
    ),
    (
        test_layer_shell_popover_on_show_reversal,
        "popover.on-show-reversal",
        run_layer_shell_popover_on_show_reversal_contract()
    ),
    (
        test_layer_shell_click_catcher_lifetime,
        "popover.click-catcher-lifetime",
        run_layer_shell_click_catcher_lifetime_contract()
    ),
    (
        test_layer_shell_widget_popovers,
        "widget.popovers",
        run_layer_shell_widget_popovers_contract()
    ),
);

#[test]
fn compute_merge_runs_partitions_kinds() {
    use MergeKind::{Popover, Spacer};

    let cases = [
        ("empty", vec![], vec![]),
        (
            "unmergeable never grouped",
            vec![
                Popover(Unmergeable),
                Popover(Unmergeable),
                Popover(Unmergeable),
            ],
            vec![
                (Unmergeable, 0, 1),
                (Unmergeable, 1, 2),
                (Unmergeable, 2, 3),
            ],
        ),
        (
            "consecutive system entries merge",
            vec![Popover(System), Popover(System), Popover(System)],
            vec![(System, 0, 3)],
        ),
        (
            "unmergeable breaks a system run",
            vec![
                Popover(System),
                Popover(System),
                Popover(Unmergeable),
                Popover(System),
            ],
            vec![(System, 0, 2), (Unmergeable, 2, 3), (System, 3, 4)],
        ),
        (
            "single system is its own run",
            vec![Popover(System)],
            vec![(System, 0, 1)],
        ),
        (
            "spacer between same kind is absorbed",
            vec![Popover(System), Spacer, Popover(System)],
            vec![(System, 0, 3)],
        ),
        (
            "spacer before a different kind joins the left run",
            vec![Popover(System), Spacer, Popover(Unmergeable)],
            vec![(System, 0, 2), (Unmergeable, 2, 3)],
        ),
        (
            "leading spacer joins the first run",
            vec![Spacer, Popover(System), Popover(System)],
            vec![(System, 0, 3)],
        ),
        (
            "trailing spacer joins the last run",
            vec![Popover(System), Spacer],
            vec![(System, 0, 2)],
        ),
        ("spacers alone build no runs", vec![Spacer, Spacer], vec![]),
    ];

    for (name, kinds, expected) in cases {
        assert_eq!(compute_merge_runs(&kinds), expected, "{name}");
    }
}

#[test]
fn bar_and_screen_margin_layout_follow_position() {
    let cases = [
        (
            BarPosition::Top,
            gtk4::Orientation::Horizontal,
            gtk4::Orientation::Vertical,
            true,
        ),
        (
            BarPosition::Bottom,
            gtk4::Orientation::Horizontal,
            gtk4::Orientation::Vertical,
            false,
        ),
        (
            BarPosition::Left,
            gtk4::Orientation::Vertical,
            gtk4::Orientation::Horizontal,
            true,
        ),
        (
            BarPosition::Right,
            gtk4::Orientation::Vertical,
            gtk4::Orientation::Horizontal,
            false,
        ),
    ];

    for (position, flow, shell, spacer_precedes_bar) in cases {
        assert_eq!(
            bar_flow_orientation_for(position),
            flow,
            "{position:?}: bar flow orientation"
        );
        assert_eq!(
            shell_orientation_for(position),
            shell,
            "{position:?}: shell orientation"
        );
        assert_eq!(
            screen_margin_spacer_precedes_bar(position),
            spacer_precedes_bar,
            "{position:?}: screen-margin spacer order"
        );
    }

    assert_eq!(screen_margin_spacer_size(BarPosition::Top, 12), (-1, 12));
    assert_eq!(screen_margin_spacer_size(BarPosition::Right, 12), (12, -1));
}

#[test]
fn point_projects_to_edge_target_respects_position_and_bounds() {
    let bounds = (10.0, 20.0, 30.0, 40.0);
    let cases = [
        (BarPosition::Top, 25.0, 19.0, true),
        (BarPosition::Top, 25.0, 20.0, false),
        (BarPosition::Top, 9.0, 19.0, false),
        (BarPosition::Top, 41.0, 19.0, false),
        (BarPosition::Bottom, 25.0, 61.0, true),
        (BarPosition::Bottom, 25.0, 60.0, false),
        (BarPosition::Bottom, 9.0, 61.0, false),
        (BarPosition::Bottom, 41.0, 61.0, false),
        (BarPosition::Left, 9.0, 40.0, true),
        (BarPosition::Left, 10.0, 40.0, false),
        (BarPosition::Left, 9.0, 19.0, false),
        (BarPosition::Left, 9.0, 61.0, false),
        (BarPosition::Right, 41.0, 40.0, true),
        (BarPosition::Right, 40.0, 40.0, false),
        (BarPosition::Right, 41.0, 19.0, false),
        (BarPosition::Right, 41.0, 61.0, false),
    ];

    for (position, x, y, expected) in cases {
        assert_eq!(
            point_projects_to_edge_target(position, bounds.0, bounds.1, bounds.2, bounds.3, x, y,),
            expected,
            "position={position:?}, point=({x}, {y})"
        );
    }
}

#[test]
fn user_css_search_paths_order_and_dedup() {
    let cases = [
        (
            "config dir first",
            Some("/custom/config/dir"),
            Some("/xdg-home"),
            Some("/home/test"),
            vec![
                "/custom/config/dir/style.css",
                "/xdg-home/vibepanel/style.css",
                "/home/test/.config/vibepanel/style.css",
                "style.css",
            ],
        ),
        (
            "config dir, XDG and HOME dedup",
            Some("/home/test/.config/vibepanel"),
            Some("/home/test/.config"),
            Some("/home/test"),
            vec!["/home/test/.config/vibepanel/style.css", "style.css"],
        ),
        (
            "nothing set leaves only the CWD fallback",
            None,
            None,
            None,
            vec!["style.css"],
        ),
        (
            "XDG dedup with HOME",
            None,
            Some("/home/test/.config"),
            Some("/home/test"),
            vec!["/home/test/.config/vibepanel/style.css", "style.css"],
        ),
    ];

    for (name, config_dir, xdg_config_home, home_dir, expected) in cases {
        let paths = user_css_search_paths_from_env(
            config_dir.map(Path::new),
            xdg_config_home.map(Path::new),
            home_dir.map(Path::new),
        );
        let expected: Vec<PathBuf> = expected.into_iter().map(PathBuf::from).collect();
        assert_eq!(paths, expected, "{name}");
    }
}

#[test]
fn find_user_css_prefers_style_next_to_config() {
    let tmp = TestDir::new("vibepanel_test_css");
    let style_path = tmp.path().join("style.css");
    std::fs::write(&style_path, "/* test */").unwrap();

    assert_eq!(find_user_css(Some(tmp.path())), Some(style_path));
}

fn entry_with_width(name: &str, width: Option<toml::Value>) -> WidgetEntry {
    let mut entry = WidgetEntry::new(name);
    if let Some(value) = width {
        entry.options.insert("width".to_string(), value);
    }
    entry
}

#[test]
fn entry_has_fixed_width_option_matches_spacer_config_parsing() {
    let positive = entry_with_width("spacer", Some(toml::Value::Integer(50)));
    assert!(
        entry_has_fixed_width_option(&positive),
        "positive integer width must count as fixed"
    );

    let flexible = entry_with_width("spacer", None);
    assert!(
        !entry_has_fixed_width_option(&flexible),
        "missing width must count as flexible"
    );

    let invalid_string = entry_with_width("spacer", Some(toml::Value::String("oops".to_string())));
    assert!(
        !entry_has_fixed_width_option(&invalid_string),
        "non-integer width must count as flexible, matching SpacerConfig::from_entry"
    );

    let negative = entry_with_width("spacer", Some(toml::Value::Integer(-5)));
    assert!(
        !entry_has_fixed_width_option(&negative),
        "negative width must count as flexible, matching SpacerConfig::from_entry"
    );
}
