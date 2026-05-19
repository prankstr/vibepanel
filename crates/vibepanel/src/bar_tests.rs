use super::*;
use crate::popover_registry::{self, DispatchAction};
use crate::services::compositor::CompositorManager;
use crate::ui_regression_test_support::{
    find_descendant_with_class, first_monitor_or_skip, flush_gtk, init_layer_shell_or_skip,
    registered_test_app, run_ignored_contract_subprocess,
};
use crate::widgets::PopoverKind::{System, Unmergeable};
use crate::widgets::layer_shell_popover::{
    LayerShellPopover, calculate_bar_exclusive_zone, calculate_popover_bar_margin, popover_bar_edge,
};
use std::time::{SystemTime, UNIX_EPOCH};
use vibepanel_core::config::WidgetPlacement;

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let unique = format!(
            "{}_{}_{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    ConfigManager::init_global(config.clone(), None);
    if init_compositor {
        CompositorManager::init_global(&config.advanced);
    }
    load_css(config);
}

struct LayerShellBarFixture {
    window: ApplicationWindow,
    _state: BarState,
}

fn present_layer_shell_bar(
    context: &LayerShellContext,
    config: &Config,
    init_compositor: bool,
) -> LayerShellBarFixture {
    apply_layer_shell_config(config, init_compositor);
    let mut state = BarState::new();
    let window = create_bar_window(
        &context.app,
        config,
        &context.monitor,
        "layer-test",
        &mut state,
    );
    window.present();
    flush_gtk();

    LayerShellBarFixture {
        window,
        _state: state,
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

fn run_layer_shell_bar_position_contract(position: &str) {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut top_config = layer_shell_test_config();
    top_config.bar.position = position.to_string();
    let bar = present_layer_shell_bar(&context, &top_config, false);
    let top_window = &bar.window;

    let expect_bottom = position == "bottom";

    assert!(top_window.is_layer_window());
    assert_eq!(
        top_window.is_anchor(gtk4_layer_shell::Edge::Top),
        !expect_bottom,
        "top anchor should track bar.position"
    );
    assert!(top_window.is_anchor(gtk4_layer_shell::Edge::Left));
    assert!(top_window.is_anchor(gtk4_layer_shell::Edge::Right));
    assert_eq!(
        top_window.is_anchor(gtk4_layer_shell::Edge::Bottom),
        expect_bottom,
        "bottom anchor should track bar.position"
    );
    assert_eq!(top_window.layer(), gtk4_layer_shell::Layer::Top);
    assert_eq!(
        top_window.keyboard_mode(),
        gtk4_layer_shell::KeyboardMode::None
    );
    assert!(top_window.auto_exclusive_zone_is_enabled());
    assert_eq!(top_window.namespace().as_deref(), Some("vibepanel"));
    assert_eq!(top_window.monitor().as_ref(), Some(&context.monitor));

    top_window.close();
    flush_gtk();
}

fn run_layer_shell_bar_height_contract(background_opacity: f64) {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.bar.padding = 10;
    config.bar.background_opacity = background_opacity;
    let bar = present_layer_shell_bar(&context, &config, false);
    let window = &bar.window;

    let expected_height = rendered_bar_height(&config);
    let (_default_width, default_height) = window.default_size();

    assert_eq!(default_height, expected_height);
    assert!(window.auto_exclusive_zone_is_enabled());
    assert!(window.height() >= expected_height);

    window.close();
    flush_gtk();
}

fn run_layer_shell_popover_position_contract(position: &str) {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.bar.position = position.to_string();
    config.bar.padding = 10;
    config.bar.popover_offset = 18;
    config.bar.background_opacity = 1.0;
    apply_layer_shell_config(&config, true);

    let popover = LayerShellPopover::new(&context.app, "contract", || {
        gtk4::Label::new(Some("contract popover")).upcast::<gtk4::Widget>()
    });
    popover.show_at(160, Some(context.monitor.clone()));
    flush_gtk();

    let Some(window) = popover.test_window() else {
        panic!("popover should create a layer-shell window when shown");
    };
    let Some(catcher) = popover.test_click_catcher() else {
        panic!("popover should create a click-catcher when shown");
    };

    let expect_bottom = position == "bottom";
    let bar_edge = popover_bar_edge();
    let expected_bar_margin = calculate_popover_bar_margin();
    let expected_catcher_margin = calculate_bar_exclusive_zone();

    assert!(window.is_layer_window());
    assert_eq!(
        window.is_anchor(gtk4_layer_shell::Edge::Top),
        !expect_bottom,
        "popover top anchor should track bar.position"
    );
    assert!(window.is_anchor(gtk4_layer_shell::Edge::Right));
    assert_eq!(
        window.is_anchor(gtk4_layer_shell::Edge::Bottom),
        expect_bottom,
        "popover bottom anchor should track bar.position"
    );
    assert!(!window.is_anchor(gtk4_layer_shell::Edge::Left));
    assert_eq!(window.layer(), gtk4_layer_shell::Layer::Top);
    assert_ne!(window.keyboard_mode(), gtk4_layer_shell::KeyboardMode::None);
    assert_eq!(window.exclusive_zone(), 0);
    assert_eq!(
        window.namespace().as_deref(),
        Some("vibepanel-contract-popover")
    );
    assert_eq!(window.monitor().as_ref(), Some(&context.monitor));
    assert_eq!(window.margin(bar_edge), expected_bar_margin);

    assert!(catcher.is_layer_window());
    assert!(catcher.is_anchor(gtk4_layer_shell::Edge::Top));
    assert!(catcher.is_anchor(gtk4_layer_shell::Edge::Right));
    assert!(catcher.is_anchor(gtk4_layer_shell::Edge::Bottom));
    assert!(catcher.is_anchor(gtk4_layer_shell::Edge::Left));
    assert_eq!(catcher.layer(), gtk4_layer_shell::Layer::Top);
    assert_eq!(
        catcher.keyboard_mode(),
        gtk4_layer_shell::KeyboardMode::None
    );
    assert_eq!(catcher.exclusive_zone(), -1);
    assert_eq!(
        catcher.namespace().as_deref(),
        Some("vibepanel-click-catcher")
    );
    assert_eq!(catcher.monitor().as_ref(), Some(&context.monitor));
    assert_eq!(catcher.margin(bar_edge), expected_catcher_margin);

    popover.hide();
    window.close();
    catcher.close();
    flush_gtk();
}

fn run_layer_shell_popover_offset_contract(position: &str, background_opacity: f64) {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.bar.position = position.to_string();
    config.bar.popover_offset = 23;
    config.bar.background_opacity = background_opacity;
    apply_layer_shell_config(&config, true);

    let popover = LayerShellPopover::new(&context.app, "contract", || {
        gtk4::Label::new(Some("contract popover")).upcast::<gtk4::Widget>()
    });
    popover.show_at(160, Some(context.monitor.clone()));
    flush_gtk();

    let Some(window) = popover.test_window() else {
        panic!("popover should create a layer-shell window when shown");
    };
    let Some(catcher) = popover.test_click_catcher() else {
        panic!("popover should create a click-catcher when shown");
    };

    let bar_edge = popover_bar_edge();
    let expect_bottom = position == "bottom";
    let expected_bar_margin = calculate_popover_bar_margin();
    let expected_catcher_margin = calculate_bar_exclusive_zone();

    assert!(window.is_layer_window());
    assert_eq!(
        window.is_anchor(gtk4_layer_shell::Edge::Top),
        !expect_bottom,
        "popover top anchor should track bar.position"
    );
    assert!(window.is_anchor(gtk4_layer_shell::Edge::Right));
    assert_eq!(
        window.is_anchor(gtk4_layer_shell::Edge::Bottom),
        expect_bottom,
        "popover bottom anchor should track bar.position"
    );
    assert!(!window.is_anchor(gtk4_layer_shell::Edge::Left));
    assert_eq!(window.layer(), gtk4_layer_shell::Layer::Top);
    assert_eq!(
        window.namespace().as_deref(),
        Some("vibepanel-contract-popover")
    );
    assert_eq!(window.monitor().as_ref(), Some(&context.monitor));
    assert_eq!(window.margin(bar_edge), expected_bar_margin);
    if config.bar.background_opacity > 0.0 {
        assert_eq!(
            window.margin(bar_edge),
            config.bar.popover_offset as i32 - config.bar.padding as i32
        );
    } else {
        assert_eq!(window.margin(bar_edge), config.bar.popover_offset as i32);
    }

    assert!(catcher.is_layer_window());
    assert_eq!(catcher.margin(bar_edge), expected_catcher_margin);

    popover.hide();
    window.close();
    catcher.close();
    flush_gtk();
}

fn run_layer_shell_clock_popover_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.widgets.left = vec![WidgetPlacement::Single("clock".to_string())];
    let bar = present_layer_shell_bar(&context, &config, true);

    assert!(
        popover_registry::dispatch("clock", DispatchAction::Show),
        "clock widget should register a popover handle"
    );
    flush_gtk();

    let Some(popover_window) = popover_registry::test_layer_shell_window("clock") else {
        panic!("clock registry handle should expose its real layer-shell popover window");
    };
    let Some(child) = popover_window.child() else {
        panic!("clock popover window should have content after being shown");
    };

    let bar_edge = popover_bar_edge();
    let expected_bar_margin = calculate_popover_bar_margin();

    assert!(popover_window.is_layer_window());
    assert!(popover_window.is_anchor(gtk4_layer_shell::Edge::Top));
    assert!(popover_window.is_anchor(gtk4_layer_shell::Edge::Right));
    assert!(!popover_window.is_anchor(gtk4_layer_shell::Edge::Bottom));
    assert!(!popover_window.is_anchor(gtk4_layer_shell::Edge::Left));
    assert_eq!(popover_window.layer(), gtk4_layer_shell::Layer::Top);
    assert_eq!(
        popover_window.namespace().as_deref(),
        Some("vibepanel-clock-popover")
    );
    assert_eq!(popover_window.monitor().as_ref(), Some(&context.monitor));
    assert_eq!(popover_window.margin(bar_edge), expected_bar_margin);
    assert!(
        find_descendant_with_class(&child, crate::styles::calendar::POPOVER).is_some(),
        "clock popover should contain calendar popover content"
    );

    popover_registry::dispatch("clock", DispatchAction::Hide);
    popover_window.close();
    bar.window.close();
    flush_gtk();
}

fn run_layer_shell_system_widget_popover_contract() {
    let Some(context) = layer_shell_context_or_skip() else {
        return;
    };

    let mut config = layer_shell_test_config();
    config.widgets.left = vec![WidgetPlacement::Single("cpu".to_string())];
    let bar = present_layer_shell_bar(&context, &config, true);

    assert!(
        popover_registry::dispatch("cpu", DispatchAction::Show),
        "cpu widget should register a system popover handle"
    );
    flush_gtk();

    let Some(popover_window) = popover_registry::test_layer_shell_window("cpu") else {
        panic!("cpu registry handle should expose its real layer-shell popover window");
    };
    let Some(child) = popover_window.child() else {
        panic!("cpu popover window should have content after being shown");
    };

    let bar_edge = popover_bar_edge();
    let expected_bar_margin = calculate_popover_bar_margin();

    assert!(popover_window.is_layer_window());
    assert!(popover_window.is_anchor(gtk4_layer_shell::Edge::Top));
    assert!(popover_window.is_anchor(gtk4_layer_shell::Edge::Right));
    assert!(!popover_window.is_anchor(gtk4_layer_shell::Edge::Bottom));
    assert!(!popover_window.is_anchor(gtk4_layer_shell::Edge::Left));
    assert_eq!(popover_window.layer(), gtk4_layer_shell::Layer::Top);
    assert_eq!(
        popover_window.namespace().as_deref(),
        Some("vibepanel-cpu-popover")
    );
    assert_eq!(popover_window.monitor().as_ref(), Some(&context.monitor));
    assert_eq!(popover_window.margin(bar_edge), expected_bar_margin);
    assert!(
        find_descendant_with_class(&child, crate::styles::system_popover::POPOVER).is_some(),
        "cpu popover should contain shared system popover content"
    );

    popover_registry::dispatch("cpu", DispatchAction::Hide);
    popover_window.close();
    bar.window.close();
    flush_gtk();
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
        test_layer_shell_bar_position_top,
        "bar.position.top",
        run_layer_shell_bar_position_contract("top")
    ),
    (
        test_layer_shell_bar_position_bottom,
        "bar.position.bottom",
        run_layer_shell_bar_position_contract("bottom")
    ),
    (
        test_layer_shell_bar_height_visible,
        "bar.height.visible",
        run_layer_shell_bar_height_contract(1.0)
    ),
    (
        test_layer_shell_bar_height_transparent,
        "bar.height.transparent",
        run_layer_shell_bar_height_contract(0.0)
    ),
    (
        test_layer_shell_popover_position_top,
        "popover.position.top",
        run_layer_shell_popover_position_contract("top")
    ),
    (
        test_layer_shell_popover_position_bottom,
        "popover.position.bottom",
        run_layer_shell_popover_position_contract("bottom")
    ),
    (
        test_layer_shell_popover_offset_top,
        "popover.offset.top",
        run_layer_shell_popover_offset_contract("top", 0.0)
    ),
    (
        test_layer_shell_popover_offset_bottom,
        "popover.offset.bottom",
        run_layer_shell_popover_offset_contract("bottom", 0.0)
    ),
    (
        test_layer_shell_popover_offset_visible_top,
        "popover.offset.visible.top",
        run_layer_shell_popover_offset_contract("top", 1.0)
    ),
    (
        test_layer_shell_popover_offset_visible_bottom,
        "popover.offset.visible.bottom",
        run_layer_shell_popover_offset_contract("bottom", 1.0)
    ),
    (
        test_layer_shell_clock_widget_popover,
        "widget.clock.popover",
        run_layer_shell_clock_popover_contract()
    ),
    (
        test_layer_shell_system_widget_popover,
        "widget.system.popover",
        run_layer_shell_system_widget_popover_contract()
    ),
);

#[test]
fn merge_runs_empty() {
    assert_eq!(compute_merge_runs(&[]), vec![]);
}

#[test]
fn merge_runs_unmergeable_never_grouped() {
    let runs = compute_merge_runs(&[
        MergeKind::Popover(Unmergeable),
        MergeKind::Popover(Unmergeable),
        MergeKind::Popover(Unmergeable),
    ]);
    assert_eq!(
        runs,
        vec![
            (Unmergeable, 0, 1),
            (Unmergeable, 1, 2),
            (Unmergeable, 2, 3),
        ]
    );
}

#[test]
fn merge_runs_system_grouping() {
    // Consecutive System entries merge into one run
    assert_eq!(
        compute_merge_runs(&[
            MergeKind::Popover(System),
            MergeKind::Popover(System),
            MergeKind::Popover(System),
        ]),
        vec![(System, 0, 3)]
    );
    // Unmergeable breaks a System run; singleton System stays singleton
    assert_eq!(
        compute_merge_runs(&[
            MergeKind::Popover(System),
            MergeKind::Popover(System),
            MergeKind::Popover(Unmergeable),
            MergeKind::Popover(System),
        ]),
        vec![(System, 0, 2), (Unmergeable, 2, 3), (System, 3, 4)],
    );
    // Single System is its own run
    assert_eq!(
        compute_merge_runs(&[MergeKind::Popover(System)]),
        vec![(System, 0, 1)]
    );
}

#[test]
fn merge_runs_spacer_absorbed_between_same_kind() {
    // cpu, spacer, memory → still merges into one System run
    assert_eq!(
        compute_merge_runs(&[
            MergeKind::Popover(System),
            MergeKind::Spacer,
            MergeKind::Popover(System),
        ]),
        vec![(System, 0, 3)]
    );
}

#[test]
fn merge_runs_spacer_absorbed_into_left_run() {
    // System, spacer, Unmergeable → spacer attaches to System run
    assert_eq!(
        compute_merge_runs(&[
            MergeKind::Popover(System),
            MergeKind::Spacer,
            MergeKind::Popover(Unmergeable),
        ]),
        vec![(System, 0, 2), (Unmergeable, 2, 3)]
    );
}

#[test]
fn merge_runs_leading_spacer_absorbed() {
    // spacer, System, System → leading spacer joins first run
    assert_eq!(
        compute_merge_runs(&[
            MergeKind::Spacer,
            MergeKind::Popover(System),
            MergeKind::Popover(System),
        ]),
        vec![(System, 0, 3)]
    );
}

#[test]
fn merge_runs_trailing_spacer_absorbed() {
    // System, spacer → trailing spacer joins the System run
    assert_eq!(
        compute_merge_runs(&[MergeKind::Popover(System), MergeKind::Spacer]),
        vec![(System, 0, 2)]
    );
}

#[test]
fn merge_runs_all_spacers_get_no_runs() {
    // Spacers only exist to join painted groups; alone they build nothing.
    assert_eq!(
        compute_merge_runs(&[MergeKind::Spacer, MergeKind::Spacer]),
        Vec::new()
    );
}

#[test]
fn user_css_search_paths_config_dir_first() {
    let dir = std::path::Path::new("/custom/config/dir");
    let paths = user_css_search_paths_from_env(
        Some(dir),
        Some(Path::new("/xdg-home")),
        Some(Path::new("/home/test")),
    );
    assert_eq!(
        paths,
        vec![
            dir.join("style.css"),
            PathBuf::from("/xdg-home/vibepanel/style.css"),
            PathBuf::from("/home/test/.config/vibepanel/style.css"),
            PathBuf::from("style.css"),
        ]
    );
}

#[test]
fn user_css_search_paths_deduplicates() {
    let paths = user_css_search_paths_from_env(
        Some(Path::new("/home/test/.config/vibepanel")),
        Some(Path::new("/home/test/.config")),
        Some(Path::new("/home/test")),
    );
    assert_eq!(
        paths,
        vec![
            PathBuf::from("/home/test/.config/vibepanel/style.css"),
            PathBuf::from("style.css"),
        ]
    );
}

#[test]
fn user_css_search_paths_none_config_dir_still_has_entries() {
    let paths = user_css_search_paths_from_env(None, None, None);
    assert_eq!(
        paths,
        vec![PathBuf::from("style.css")],
        "without config/XDG/HOME, only the CWD fallback remains"
    );
}

#[test]
fn user_css_search_paths_xdg_dedup_with_home() {
    let paths = user_css_search_paths_from_env(
        None,
        Some(Path::new("/home/test/.config")),
        Some(Path::new("/home/test")),
    );
    assert_eq!(
        paths,
        vec![
            PathBuf::from("/home/test/.config/vibepanel/style.css"),
            PathBuf::from("style.css"),
        ]
    );
}

#[test]
fn find_user_css_returns_none_when_no_file_exists() {
    // Pass a config_dir that does not contain style.css; without HOME/XDG
    // overrides the CWD fallback is unlikely to exist either, but we use an
    // empty tmp dir as config_dir so that slot is definitely absent.
    let tmp = TestDir::new("vibepanel_test_missing_css");
    // An empty directory: none of the candidate paths will exist.
    let result =
        user_css_search_paths_from_env(Some(tmp.path()), Some(tmp.path()), Some(tmp.path()))
            .into_iter()
            .find(|p| p.exists());
    assert_eq!(result, None);
}

#[test]
fn find_user_css_finds_existing_file() {
    // Create a real style.css in a temp directory and point config_dir at
    // it so find_user_css returns the config-adjacent path (highest priority).
    let tmp = TestDir::new("vibepanel_test_css");
    let style_path = tmp.path().join("style.css");
    std::fs::write(&style_path, "/* test */").unwrap();

    // Use the injected-env helper so we don't depend on real HOME/XDG.
    let found =
        user_css_search_paths_from_env(Some(tmp.path()), Some(tmp.path()), Some(tmp.path()))
            .into_iter()
            .find(|p| p.exists());

    assert_eq!(found, Some(style_path));
}
