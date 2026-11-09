use super::*;
use crate::theme_vars::{THEME_VAR_EXPECTATIONS, ThemeVarRole, ThemeVarScope};
use crate::ui_regression_test_support::{
    CssProviderGuard, Rgba8, TestDir, assert_pixel_close, center_pixel_of_surface,
    find_descendant_with_class, flush_gtk, init_gtk_or_skip, label_with_text,
    maybe_hold_probe_window, painted_surface_fixture_with_classes, run_ignored_contract_subprocess,
    sample_widget_pixel,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use vibepanel_core::Config;

fn bounds_in_window(
    widget: &impl IsA<gtk4::Widget>,
    window: &gtk4::Window,
) -> (i32, i32, i32, i32) {
    let bounds = widget
        .compute_bounds(window.upcast_ref::<gtk4::Widget>())
        .expect("widget should have bounds after GTK layout");

    (
        bounds.x().round() as i32,
        bounds.y().round() as i32,
        bounds.width().round() as i32,
        bounds.height().round() as i32,
    )
}

fn measured_gap(left: (i32, i32, i32, i32), right: (i32, i32, i32, i32)) -> i32 {
    right.0 - (left.0 + left.2)
}

fn count_descendants_with_class(root: &gtk4::Widget, class_name: &str) -> usize {
    let mut count = usize::from(root.has_css_class(class_name));
    let mut child = root.first_child();
    while let Some(widget) = child {
        count += count_descendants_with_class(&widget, class_name);
        child = widget.next_sibling();
    }
    count
}

fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let ctx = gtk4::glib::MainContext::default();
    while Instant::now() < deadline {
        ctx.iteration(false);
        if condition() {
            return;
        }
    }

    flush_gtk();
    assert!(condition(), "timed out waiting for {description}");
}

fn collect_descendants_with_class(root: &gtk4::Widget, class_name: &str) -> Vec<gtk4::Widget> {
    let mut matches = Vec::new();
    if root.has_css_class(class_name) {
        matches.push(root.clone());
    }

    let mut child = root.first_child();
    while let Some(widget) = child {
        matches.extend(collect_descendants_with_class(&widget, class_name));
        child = widget.next_sibling();
    }

    matches
}

fn has_bar_background_click_guard_ancestor(widget: &gtk4::Widget, stop_at: &gtk4::Widget) -> bool {
    let mut current = Some(widget.clone());
    while let Some(widget) = current {
        if widget.has_css_class(crate::styles::class::WIDGET_WRAPPER)
            || widget.has_css_class(crate::styles::class::WIDGET_ITEM)
            || widget.has_css_class(crate::styles::class::WIDGET)
            || widget.has_css_class(crate::styles::class::WIDGET_MERGE_GROUP)
        {
            return true;
        }
        if widget == *stop_at {
            break;
        }
        current = widget.parent();
    }

    false
}

fn section_widget_class_names(
    bar: &SectionedBar,
    section_name: &str,
    class_names: &[&str],
) -> Vec<String> {
    let section = bar
        .section(section_name)
        .unwrap_or_else(|| panic!("bar should build a {section_name} section"));

    class_names
        .iter()
        .filter(|class_name| find_descendant_with_class(&section, class_name).is_some())
        .map(|class_name| (*class_name).to_string())
        .collect()
}

fn built_bar_fixture(
    config: &Config,
) -> (
    gtk4::Window,
    SectionedBar,
    crate::widgets::BarState,
    PopoverRegistryGuard,
    CssProviderGuard,
) {
    let window_width = 400;
    let popover_registry_guard = PopoverRegistryGuard::new();
    let css_provider = widget_css_provider(config);
    let window = gtk4::Window::builder()
        .title("vibepanel widgets UI regression test")
        .default_width(window_width)
        .default_height(crate::bar::rendered_bar_height(config))
        .build();
    let app = gtk4::Application::builder()
        .application_id("dev.vibepanel.widgets-ui-regression")
        .build();
    let mut state = crate::widgets::BarState::new();
    let built = crate::bar::build_bar_content(&app, config, &mut state, Some("ui-regression-test"));
    built
        .root
        .set_size_request(window_width, crate::bar::rendered_bar_height(config));
    window.set_child(Some(&built.root));
    window.present();
    flush_gtk();

    (
        window,
        built.bar,
        state,
        popover_registry_guard,
        css_provider,
    )
}

fn widget_options_with_disabled(disabled: bool) -> vibepanel_core::config::WidgetOptions {
    vibepanel_core::config::WidgetOptions {
        disabled,
        ..Default::default()
    }
}

fn test_config() -> Config {
    let mut config = Config::default();
    config.bar.size = 32;
    config.bar.spacing = 8;
    config.bar.screen_margin = 0;
    config.bar.inset = 8;
    config.theme.mode = "dark".to_string();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-b".to_string()),
    ];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let mut custom_a = vibepanel_core::config::WidgetOptions::default();
    custom_a
        .options
        .insert("label".to_string(), toml::Value::String("A".to_string()));
    let mut custom_b = vibepanel_core::config::WidgetOptions::default();
    custom_b
        .options
        .insert("label".to_string(), toml::Value::String("B".to_string()));
    config
        .widgets
        .widget_configs
        .insert("custom-a".to_string(), custom_a);
    config
        .widgets
        .widget_configs
        .insert("custom-b".to_string(), custom_b);

    config
}

#[derive(Debug, Clone, Copy)]
struct PaintedBarMetrics {
    shell_bounds: (i32, i32, i32, i32),
    bar_bounds: (i32, i32, i32, i32),
    first_surface_bounds: (i32, i32, i32, i32),
    painted_surface_gap: i32,
}

struct PopoverRegistryGuard;

impl PopoverRegistryGuard {
    fn new() -> Self {
        crate::popover_registry::clear();
        Self
    }
}

impl Drop for PopoverRegistryGuard {
    fn drop(&mut self) {
        crate::popover_registry::clear();
    }
}

struct PaintedBarFixture {
    window: gtk4::Window,
    _popover_registry_guard: PopoverRegistryGuard,
    _css_provider: CssProviderGuard,
    _override_css_provider: Option<CssProviderGuard>,
    _state: crate::widgets::BarState,
    root: gtk4::Box,
    bar: SectionedBar,
    first_surface: gtk4::Widget,
    second_surface: gtk4::Widget,
}

fn set_ui_regression_config(config: &Config) {
    crate::services::config_manager::ConfigManager::replace_global_for_test(config.clone());
}

fn set_ui_regression_config_path(config: &Config, config_path: PathBuf) {
    crate::services::config_manager::ConfigManager::replace_global_with_config_path_for_test(
        config.clone(),
        Some(config_path),
    );
}

fn widget_css_provider(config: &Config) -> CssProviderGuard {
    CssProviderGuard::for_config(config, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION)
}

fn strip_css_comments(css: &str) -> String {
    let mut stripped = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        stripped.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("*/") else {
            break;
        };
        rest = &after_start[end + 2..];
    }
    stripped.push_str(rest);
    stripped
}

fn declared_css_variables(css: &str) -> BTreeSet<String> {
    strip_css_comments(css)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if !line.starts_with("--") {
                return None;
            }
            let name = line.split_once(':')?.0.trim();
            Some(name.to_string())
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CssVarUse {
    name: String,
    fallback_vars: Vec<String>,
    has_fallback: bool,
    in_hover_selector: bool,
}

fn matching_paren(input: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in input.char_indices().skip_while(|(idx, _)| *idx < open_idx) {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

fn split_top_level_once(input: &str, separator: char) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (idx, ch) in input.char_indices() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth = depth.saturating_sub(1);
        } else if ch == separator && depth == 0 {
            return Some((&input[..idx], &input[idx + ch.len_utf8()..]));
        }
    }
    None
}

fn css_vars_in_value(value: &str) -> Vec<String> {
    css_var_uses_in_css(value)
        .into_iter()
        .map(|var| var.name)
        .collect()
}

fn css_var_uses_in_css(css: &str) -> Vec<CssVarUse> {
    let css = strip_css_comments(css);
    let mut uses = Vec::new();
    let mut offset = 0usize;

    while let Some(relative_start) = css[offset..].find("var(") {
        let var_start = offset + relative_start;
        let open_idx = var_start + 3;
        let Some(close_idx) = matching_paren(&css, open_idx) else {
            break;
        };
        let inner = &css[open_idx + 1..close_idx];
        let (name_part, fallback_part) = split_top_level_once(inner, ',').unwrap_or((inner, ""));
        let name = name_part.trim();

        if name.starts_with("--") {
            let selector_start = css[..var_start].rfind('}').map(|idx| idx + 1).unwrap_or(0);
            let selector_end = css[selector_start..var_start]
                .find('{')
                .map(|idx| selector_start + idx)
                .unwrap_or(var_start);
            let selector = &css[selector_start..selector_end];
            let has_fallback = !fallback_part.trim().is_empty();
            let fallback_vars = if has_fallback {
                css_vars_in_value(fallback_part)
            } else {
                Vec::new()
            };

            uses.push(CssVarUse {
                name: name.to_string(),
                fallback_vars,
                has_fallback,
                in_hover_selector: selector.contains(":hover"),
            });
        }

        offset = close_idx + 1;
    }

    uses
}

fn rust_composed_theme_var_css() -> &'static str {
    // Keep this focused on theme vars composed in Rust outside widgets::css::*.
    // User-facing CSS hooks are documented in the wiki, not mirrored here.
    concat!(
        include_str!("services/surfaces.rs"),
        "\n",
        include_str!("widgets/taskbar.rs"),
        "\n",
        include_str!("widgets/osd.rs")
    )
}

fn local_runtime_css_variables() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "--vp-taskbar-button-gap",
        "--vp-taskbar-content-edge",
        "--vp-taskbar-separator-gap",
        "--vp-taskbar-widget-content-gap",
        "--vp-taskbar-widget-content-padding",
        "--vp-workspace-pill-scale",
    ])
}

#[test]
fn theme_vars_have_valid_internal_consumers() {
    let config = test_config();
    let palette = vibepanel_core::ThemePalette::from_config(&config, None, None);
    let popover_palette = vibepanel_core::ThemePalette::popover_palette(&config, None, None);
    let popover_block = popover_palette
        .as_ref()
        .map(|p| p.css_popover_vars_block())
        .unwrap_or_default();
    let emitted_root_vars = declared_css_variables(&palette.css_vars_block());
    let production_css = format!(
        "{}\n{}\n{}",
        popover_block,
        crate::widgets::css::utility_css(&config),
        crate::widgets::css::widget_css(&config)
    );
    let production_declared_vars = declared_css_variables(&production_css);
    let var_uses = css_var_uses_in_css(&production_css);
    let internal_css = format!("{}\n{}", production_css, rust_composed_theme_var_css());
    let internal_var_uses = css_var_uses_in_css(&internal_css);
    let consumed = internal_var_uses
        .iter()
        .fold(BTreeSet::new(), |mut consumed, var| {
            consumed.insert(var.name.clone());
            consumed.extend(var.fallback_vars.iter().cloned());
            consumed
        });
    let expectations_by_name = THEME_VAR_EXPECTATIONS
        .iter()
        .map(|var| (var.name, var))
        .collect::<BTreeMap<_, _>>();
    let local_runtime_vars = local_runtime_css_variables();

    let missing_builtin_consumers = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.role == ThemeVarRole::BuiltinCss)
        // Vars composed in Rust outside `widget_css` (e.g. taskbar) still
        // flow through the consumer check via `rust_composed_theme_var_css`,
        // but are exempt from the production-CSS requirement.
        .filter(|var| !local_runtime_vars.contains(var.name))
        .filter(|var| !consumed.contains(var.name))
        .map(|var| var.name)
        .collect::<Vec<_>>();
    assert!(
        missing_builtin_consumers.is_empty(),
        "built-in CSS variables must be consumed by production CSS; missing={missing_builtin_consumers:?}"
    );

    let missing_rust_composed_consumers = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.role == ThemeVarRole::RustComposedCss)
        .filter(|var| !consumed.contains(var.name))
        .map(|var| var.name)
        .collect::<Vec<_>>();
    assert!(
        missing_rust_composed_consumers.is_empty(),
        "Rust-composed theme vars must be consumed by internal styling paths; missing={missing_rust_composed_consumers:?}"
    );

    let unknown_consumed = consumed
        .iter()
        .filter(|name| {
            !emitted_root_vars.contains(name.as_str())
                && !production_declared_vars.contains(name.as_str())
                && !local_runtime_vars.contains(name.as_str())
                && !expectations_by_name.contains_key(name.as_str())
        })
        .collect::<Vec<_>>();
    assert!(
        unknown_consumed.is_empty(),
        "production CSS consumes theme vars that are neither emitted nor expected user hooks; unknown={unknown_consumed:?}"
    );

    // Generic anti-dead-var guard: every `:root` declaration must be tracked
    // either as an expected theme var or as a Rust-composed runtime var.
    // Prevents reintroducing undocumented dead emissions like the spacing
    // scale, `--widget-opacity`, or alias-only roots.
    let untracked_emitted = emitted_root_vars
        .iter()
        .filter(|name| {
            !expectations_by_name.contains_key(name.as_str())
                && !local_runtime_vars.contains(name.as_str())
        })
        .collect::<Vec<_>>();
    assert!(
        untracked_emitted.is_empty(),
        "root CSS declarations must be tracked in THEME_VAR_EXPECTATIONS; untracked={untracked_emitted:?}"
    );

    let missing_expected_root_vars = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.scope == ThemeVarScope::Root)
        .filter(|var| !emitted_root_vars.contains(var.name))
        .map(|var| var.name)
        .collect::<Vec<_>>();
    assert!(
        missing_expected_root_vars.is_empty(),
        "theme var expectations reference root vars that are not emitted; missing={missing_expected_root_vars:?}"
    );

    let optional_builtin_errors = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.role == ThemeVarRole::OptionalBuiltinCss)
        .flat_map(|expectation| {
            let matching_uses = var_uses
                .iter()
                .filter(|var_use| {
                    var_use.name == expectation.name
                        || var_use
                            .fallback_vars
                            .iter()
                            .any(|fallback| fallback == expectation.name)
                })
                .collect::<Vec<_>>();
            let mut errors = Vec::new();
            if matching_uses.is_empty() {
                errors.push(format!("{} is never consumed", expectation.name));
            }
            for var_use in matching_uses {
                if !var_use.has_fallback {
                    errors.push(format!(
                        "{} is consumed without a fallback",
                        expectation.name
                    ));
                }
                for fallback in &var_use.fallback_vars {
                    let Some(fallback_expectation) = expectations_by_name.get(fallback.as_str())
                    else {
                        errors.push(format!(
                            "{} fallback {} is not in the theme var expectations",
                            expectation.name, fallback
                        ));
                        continue;
                    };
                    if fallback_expectation.scope == ThemeVarScope::UserHook {
                        continue;
                    }
                    if fallback_expectation.scope != ThemeVarScope::Root {
                        errors.push(format!(
                            "{} fallback {} is not a root variable",
                            expectation.name, fallback
                        ));
                    }
                    if !emitted_root_vars.contains(fallback.as_str()) {
                        errors.push(format!(
                            "{} fallback {} is not emitted by the theme palette",
                            expectation.name, fallback
                        ));
                    }
                }
            }
            errors
        })
        .collect::<Vec<_>>();
    assert!(
        optional_builtin_errors.is_empty(),
        "optional built-in CSS variables must be consumed with expected fallbacks; errors={optional_builtin_errors:?}"
    );

    let widget_var_uses = css_var_uses_in_css(&crate::widgets::css::widget_css(&config));
    let missing_hover_bindings = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.hover_binding)
        .map(|var| var.name)
        .filter(|name| {
            !widget_var_uses
                .iter()
                .any(|var_use| var_use.name == *name && var_use.in_hover_selector)
        })
        .collect::<Vec<_>>();
    assert!(
        missing_hover_bindings.is_empty(),
        "hover CSS variables must be consumed inside :hover selectors; missing={missing_hover_bindings:?}"
    );
}

fn outline_color_for_palette(
    palette: &vibepanel_core::ThemePalette,
    color: &str,
    opacity: f64,
) -> Rgba8 {
    let resolved = match color {
        "accent" => palette.accent_primary.as_str(),
        "foreground" => palette.foreground_primary.as_str(),
        "subtle" => palette.border_subtle.as_str(),
        other => other,
    };
    let mut rgba = gtk4::gdk::RGBA::parse(resolved)
        .unwrap_or_else(|_| panic!("resolved outline color should parse: {resolved}"));
    rgba.set_alpha(rgba.alpha() * opacity as f32);
    Rgba8::from_gdk(rgba)
}

fn painted_bar_fixture(config: &Config) -> PaintedBarFixture {
    painted_bar_fixture_with_override_css(config, None)
}

fn painted_surface_fixture(
    config: &Config,
    class_name: &str,
) -> crate::ui_regression_test_support::PaintedSurfaceFixture {
    painted_surface_fixture_with_size(config, class_name, 120, 48)
}

fn painted_surface_fixture_with_size(
    config: &Config,
    class_name: &str,
    width: i32,
    height: i32,
) -> crate::ui_regression_test_support::PaintedSurfaceFixture {
    painted_surface_fixture_with_classes(config, &[class_name], width, height)
}

fn painted_bar_fixture_with_override_css(
    config: &Config,
    override_css: Option<&str>,
) -> PaintedBarFixture {
    set_ui_regression_config(config);

    let window_width = 400;
    let popover_registry_guard = PopoverRegistryGuard::new();
    let css_provider = widget_css_provider(config);
    let override_css_provider = override_css
        .map(|css| CssProviderGuard::new(css, gtk4::STYLE_PROVIDER_PRIORITY_USER + 100));

    let window = gtk4::Window::builder()
        .title("vibepanel UI regression config test")
        .default_width(window_width)
        .default_height(crate::bar::rendered_bar_height(config))
        .build();

    let app = gtk4::Application::builder()
        .application_id("dev.vibepanel.ui-regression")
        .build();
    let mut state = crate::widgets::BarState::new();
    let built = crate::bar::build_bar_content(&app, config, &mut state, Some("ui-regression-test"));
    built
        .root
        .set_size_request(window_width, crate::bar::rendered_bar_height(config));
    built
        .bar
        .set_size_request(window_width, config.bar.size as i32);

    window.set_child(Some(&built.root));
    window.present();
    flush_gtk();

    let left_section = built
        .bar
        .section("left")
        .expect("UI regression config should build a left section");
    let first_wrapper = left_section
        .first_child()
        .expect("UI regression config should build first widget");
    let second_wrapper = first_wrapper
        .next_sibling()
        .expect("UI regression config should build second widget");
    let first_surface = find_descendant_with_class(&first_wrapper, crate::styles::class::WIDGET)
        .expect("first real widget should contain a painted .widget surface");
    let second_surface = find_descendant_with_class(&second_wrapper, crate::styles::class::WIDGET)
        .expect("second real widget should contain a painted .widget surface");

    PaintedBarFixture {
        window,
        _popover_registry_guard: popover_registry_guard,
        _css_provider: css_provider,
        _override_css_provider: override_css_provider,
        _state: state,
        root: built.root,
        bar: built.bar,
        first_surface,
        second_surface,
    }
}

fn painted_bar_metrics(config: &Config) -> PaintedBarMetrics {
    let fixture = painted_bar_fixture(config);
    let shell_bounds = bounds_in_window(&fixture.root, &fixture.window);
    let bar_bounds = bounds_in_window(&fixture.bar, &fixture.window);
    let first_surface_bounds = bounds_in_window(&fixture.first_surface, &fixture.window);
    let second_surface_bounds = bounds_in_window(&fixture.second_surface, &fixture.window);
    let painted_surface_gap = measured_gap(first_surface_bounds, second_surface_bounds);

    maybe_hold_probe_window();

    fixture.window.close();
    flush_gtk();

    PaintedBarMetrics {
        shell_bounds,
        bar_bounds,
        first_surface_bounds,
        painted_surface_gap,
    }
}

fn sample_root_pixel(fixture: &PaintedBarFixture, x: i32, y: i32) -> Rgba8 {
    sample_widget_pixel(&fixture.window, fixture.root.upcast_ref(), x, y)
}

fn center_pixel_of(fixture: &PaintedBarFixture, widget: &gtk4::Widget) -> Rgba8 {
    let bounds = bounds_in_window(widget, &fixture.window);
    sample_root_pixel(fixture, bounds.0 + bounds.2 / 2, bounds.1 + bounds.3 / 2)
}

fn background_pixel_of(fixture: &PaintedBarFixture, widget: &gtk4::Widget) -> Rgba8 {
    let bounds = bounds_in_window(widget, &fixture.window);
    let x = bounds.0 + (bounds.2 - 4).max(0);
    sample_root_pixel(fixture, x, bounds.1 + bounds.3 / 2)
}

fn edge_pixel_of(fixture: &PaintedBarFixture, widget: &gtk4::Widget) -> Rgba8 {
    let bounds = bounds_in_window(widget, &fixture.window);
    sample_root_pixel(fixture, bounds.0 + 1, bounds.1 + bounds.3 / 2)
}

fn assert_luma_delta_at_least(a: Rgba8, b: Rgba8, delta: f64, message: &str) {
    let observed_delta = (a.luma() - b.luma()).abs();
    assert!(
        observed_delta >= delta,
        "{message}; expected luma delta >= {delta}, observed={observed_delta}, a={a:?}, b={b:?}"
    );
}

fn run_ui_regression_config_subprocess(test_case: &str) {
    run_ignored_contract_subprocess(
        "ui_regression_config_runner",
        "VIBEPANEL_UI_REGRESSION_TEST",
        test_case,
        "UI regression config test",
    );
}

/// Live layout of the bar before and after one config change.
struct LayoutChange {
    name: &'static str,
    baseline: Config,
    changed: Config,
    before: PaintedBarMetrics,
    after: PaintedBarMetrics,
}

fn measure_layout_change(
    name: &'static str,
    setup: impl FnOnce(&mut Config),
    change: impl FnOnce(&mut Config),
) -> LayoutChange {
    let mut baseline = test_config();
    setup(&mut baseline);
    let mut changed = baseline.clone();
    change(&mut changed);
    LayoutChange {
        name,
        before: painted_bar_metrics(&baseline),
        after: painted_bar_metrics(&changed),
        baseline,
        changed,
    }
}

fn assert_keeps_bar_height(layout: &LayoutChange) {
    assert_eq!(
        layout.after.bar_bounds.3, layout.before.bar_bounds.3,
        "{}: should not change the measured bar height",
        layout.name
    );
}

fn assert_keeps_surface_gap(layout: &LayoutChange) {
    assert_eq!(
        layout.after.painted_surface_gap, layout.before.painted_surface_gap,
        "{}: should not change the measured gap between painted surfaces",
        layout.name
    );
}

fn check_bar_size_layout() {
    let layout = measure_layout_change("bar.size", |_| {}, |config| config.bar.size = 40);
    assert_keeps_surface_gap(&layout);
    assert_eq!(
        layout.after.first_surface_bounds.3, layout.changed.bar.size as i32,
        "bar.size: should set the painted widget surface height"
    );
    assert_eq!(
        layout.after.bar_bounds.3 - layout.before.bar_bounds.3,
        (layout.changed.bar.size - layout.baseline.bar.size) as i32,
        "bar.size: should grow the bar allocation by the same delta"
    );
}

fn check_bar_spacing_layout() {
    let layout = measure_layout_change("bar.spacing", |_| {}, |config| config.bar.spacing = 16);
    assert_keeps_bar_height(&layout);
    assert_eq!(
        layout.after.painted_surface_gap, layout.changed.bar.spacing as i32,
        "bar.spacing: should set the gap between painted widget surfaces"
    );
}

fn check_bar_inset_layout() {
    let layout = measure_layout_change("bar.inset", |_| {}, |config| config.bar.inset = 20);
    assert_keeps_bar_height(&layout);
    assert_keeps_surface_gap(&layout);
    assert_eq!(
        layout.after.first_surface_bounds.0, layout.changed.bar.inset as i32,
        "bar.inset: should set the x-position of the first painted widget surface"
    );
}

fn check_bar_screen_margin_layout() {
    let layout = measure_layout_change(
        "bar.screen_margin",
        |_| {},
        |config| config.bar.screen_margin = 24,
    );
    assert_keeps_bar_height(&layout);
    assert_keeps_surface_gap(&layout);
    assert_eq!(
        layout.after.first_surface_bounds.0,
        (layout.changed.bar.screen_margin + layout.changed.bar.inset) as i32,
        "bar.screen_margin: should offset the first painted widget surface"
    );
}

fn check_bar_padding_layout() {
    let layout = measure_layout_change(
        "bar.padding",
        |config| {
            config.bar.background_opacity = 1.0;
            config.bar.padding = 4;
        },
        |config| config.bar.padding = 10,
    );
    assert_keeps_surface_gap(&layout);
    assert_eq!(
        layout.after.shell_bounds.3,
        crate::bar::rendered_bar_height(&layout.changed),
        "bar.padding: should count toward the rendered shell height when the background is visible"
    );
    assert_eq!(
        layout.after.first_surface_bounds.3, layout.before.first_surface_bounds.3,
        "bar.padding: should not change the painted widget surface height"
    );
}

fn check_bar_background_opacity_layout() {
    let layout = measure_layout_change(
        "bar.background_opacity",
        |config| {
            config.bar.padding = 10;
            config.bar.background_opacity = 0.0;
        },
        |config| config.bar.background_opacity = 1.0,
    );
    let (baseline, changed) = (&layout.baseline, &layout.changed);
    assert_keeps_surface_gap(&layout);
    assert_eq!(
        crate::bar::rendered_bar_height(baseline),
        baseline.bar.size as i32 + baseline.bar.padding as i32,
        "transparent background: only the screen-edge padding counts toward the layer-shell height"
    );
    assert_eq!(
        crate::bar::rendered_bar_height(changed),
        changed.bar.size as i32 + 2 * changed.bar.padding as i32,
        "visible background: padding on both sides counts toward the layer-shell height"
    );
    assert_eq!(
        layout.after.bar_bounds.3 - layout.before.bar_bounds.3,
        changed.bar.padding as i32,
        "visible background: should expose the center-side padding in the widget tree"
    );
}

fn run_test_config_bar_layout() {
    check_bar_size_layout();
    check_bar_spacing_layout();
    check_bar_inset_layout();
    check_bar_screen_margin_layout();
    check_bar_padding_layout();
    check_bar_background_opacity_layout();
}

fn run_test_widgets_placement() {
    let mut placed = test_config();
    placed.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Single(
        "custom-a".to_string(),
    )];
    placed.widgets.center = vec![vibepanel_core::config::WidgetPlacement::Single(
        "custom-b".to_string(),
    )];
    placed.widgets.right = vec![vibepanel_core::config::WidgetPlacement::Single(
        "clock".to_string(),
    )];

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&placed);

    let left_classes = section_widget_class_names(&bar, "left", &["custom-a"]);
    let center_classes = section_widget_class_names(&bar, "center", &["custom-b"]);
    let right_classes = section_widget_class_names(&bar, "right", &[crate::styles::widget::CLOCK]);

    assert_eq!(left_classes, vec!["custom-a".to_string()], "left section");
    assert_eq!(
        center_classes,
        vec!["custom-b".to_string()],
        "center section"
    );
    assert_eq!(
        right_classes,
        vec![crate::styles::widget::CLOCK.to_string()],
        "right section"
    );

    window.close();
    flush_gtk();

    let mut disabled = test_config();
    disabled
        .widgets
        .widget_configs
        .insert("custom-b".to_string(), widget_options_with_disabled(true));

    let (window, bar, _state, _popover_registry_guard, _css_provider) =
        built_bar_fixture(&disabled);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");

    assert!(
        count_descendants_with_class(&left_section, "custom-a") > 0,
        "enabled sibling should remain present in the real bar tree"
    );
    assert_eq!(
        count_descendants_with_class(&left_section, "custom-b"),
        0,
        "disabled widget should be absent from the real bar tree"
    );

    window.close();
    flush_gtk();
}

fn custom_exec_config(id: &str, exec: &str) -> Config {
    let mut config = test_config();
    let widget_name = format!("custom-{id}");
    config.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Single(
        widget_name.clone(),
    )];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let mut options = vibepanel_core::config::WidgetOptions::default();
    options.options.insert(
        "label".to_string(),
        toml::Value::String("fallback".to_string()),
    );
    options
        .options
        .insert("exec".to_string(), toml::Value::String(exec.to_string()));
    options.options.insert(
        "template".to_string(),
        toml::Value::String("{text} {percentage}%".to_string()),
    );
    options.options.insert(
        "icons".to_string(),
        toml::Value::Table(toml::map::Map::from_iter([(
            "high".to_string(),
            toml::Value::String("cpu-high".to_string()),
        )])),
    );
    config.widgets.widget_configs.insert(widget_name, options);

    config
}

fn run_test_custom_widget_json_exec_output() {
    let config = custom_exec_config(
        "json",
        "printf '%s\\n' '{\"text\":\"cpu\",\"percentage\":42,\"class\":[\"warning\"],\"alt\":\"high\",\"tooltip\":\"details\"}'",
    );
    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let surface = find_descendant_with_class(&left_section, "custom-json")
        .expect("custom-json widget surface should be present");

    wait_until(
        || label_with_text(&surface, "cpu 42%"),
        "custom JSON exec label update",
    );

    assert!(
        surface.has_css_class("warning"),
        "JSON class should be applied to the custom widget surface"
    );
    assert!(
        left_section.is_visible(),
        "non-empty JSON output should keep the custom widget visible"
    );

    maybe_hold_probe_window();
    window.close();
    flush_gtk();
}

struct GroupingCase {
    name: &'static str,
    left: Vec<vibepanel_core::config::WidgetPlacement>,
    clock_show_weather: Option<bool>,
    groups: usize,
    merge_groups: Option<usize>,
    passive: Option<usize>,
    present: &'static [&'static str],
}

fn run_test_widgets_grouping_structure() {
    use vibepanel_core::config::WidgetPlacement::{Group, Single};
    let group = |names: &[&str]| Group {
        group: names.iter().map(|name| name.to_string()).collect(),
    };

    let cases = [
        GroupingCase {
            name: "ungrouped widgets build no group",
            left: vec![
                Single("custom-a".to_string()),
                Single("custom-b".to_string()),
            ],
            clock_show_weather: None,
            groups: 0,
            merge_groups: None,
            passive: None,
            present: &[],
        },
        GroupingCase {
            name: "explicit group of custom widgets",
            left: vec![group(&["custom-a", "custom-b"])],
            clock_show_weather: None,
            groups: 1,
            merge_groups: Some(0),
            passive: None,
            present: &["custom-a", "custom-b"],
        },
        GroupingCase {
            name: "cpu and memory merge",
            left: vec![Single("custom-a".to_string()), group(&["cpu", "memory"])],
            clock_show_weather: None,
            groups: 1,
            merge_groups: Some(1),
            passive: Some(2),
            present: &["cpu", "memory"],
        },
        GroupingCase {
            name: "clock and weather merge",
            left: vec![group(&["clock", "weather"])],
            clock_show_weather: None,
            groups: 1,
            merge_groups: Some(1),
            passive: Some(2),
            present: &["clock", "weather"],
        },
        GroupingCase {
            name: "clock show_weather=false opts out of the weather merge",
            left: vec![group(&["clock", "weather"])],
            clock_show_weather: Some(false),
            groups: 1,
            merge_groups: Some(0),
            passive: Some(0),
            present: &["clock", "weather"],
        },
        GroupingCase {
            name: "clock and spacer merge",
            left: vec![group(&["clock", "spacer"])],
            clock_show_weather: None,
            groups: 1,
            merge_groups: Some(1),
            passive: Some(2),
            present: &["clock", "spacer"],
        },
        GroupingCase {
            name: "clock show_weather=false does not split a clock and spacer merge",
            left: vec![group(&["clock", "spacer"])],
            clock_show_weather: Some(false),
            groups: 1,
            merge_groups: Some(1),
            passive: Some(2),
            present: &["clock", "spacer"],
        },
    ];

    for case in cases {
        let name = case.name;
        let mut config = test_config();
        config.widgets.left = case.left;
        if let Some(show_weather) = case.clock_show_weather {
            config
                .widgets
                .widget_configs
                .entry("clock".to_string())
                .or_default()
                .options
                .insert(
                    "show_weather".to_string(),
                    toml::Value::Boolean(show_weather),
                );
        }

        let (window, bar, _state, _popover_registry_guard, _css_provider) =
            built_bar_fixture(&config);
        let left_section = bar
            .section("left")
            .expect("bar should build a left section");

        assert_eq!(
            count_descendants_with_class(&left_section, crate::styles::class::WIDGET_GROUP),
            case.groups,
            "{name}: widget groups"
        );
        if let Some(merge_groups) = case.merge_groups {
            assert_eq!(
                count_descendants_with_class(
                    &left_section,
                    crate::styles::class::WIDGET_MERGE_GROUP
                ),
                merge_groups,
                "{name}: merge groups"
            );
        }
        if let Some(passive) = case.passive {
            assert_eq!(
                count_descendants_with_class(&left_section, crate::styles::class::PASSIVE),
                passive,
                "{name}: passive widgets"
            );
        }
        for class_name in case.present {
            assert!(
                count_descendants_with_class(&left_section, class_name) > 0,
                "{name}: {class_name} should remain in the bar tree"
            );
        }

        window.close();
        flush_gtk();
    }
}

fn right_section_child_widths(config: &Config) -> Vec<i32> {
    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(config);
    let right_section = bar
        .section("right")
        .expect("bar should build a right section");
    let mut widths = Vec::new();
    let mut child = right_section.first_child();
    while let Some(widget) = child {
        widths.push(bounds_in_window(&widget, &window).2);
        child = widget.next_sibling();
    }

    window.close();
    flush_gtk();
    widths
}

fn config_with_quick_settings_group() -> Config {
    let mut config = test_config();
    config.widgets.left = Vec::new();
    config.widgets.center = Vec::new();
    config.widgets.right = vec![vibepanel_core::config::WidgetPlacement::Group {
        group: vec!["quick_settings".to_string(), "custom-vpn".to_string()],
    }];

    let mut custom_vpn = vibepanel_core::config::WidgetOptions::default();
    custom_vpn
        .options
        .insert("label".to_string(), toml::Value::String("VPN".to_string()));
    config
        .widgets
        .widget_configs
        .insert("custom-vpn".to_string(), custom_vpn);

    config
}

fn run_test_widgets_grouping_spacer_expansion() {
    let compact_widths = right_section_child_widths(&config_with_quick_settings_group());
    assert_eq!(compact_widths.len(), 1);
    let compact_quick_settings_width = compact_widths[0];

    let mut spaced_config = config_with_quick_settings_group();
    spaced_config.widgets.right.insert(
        0,
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["clock".to_string(), "spacer".to_string()],
        },
    );

    let spaced_widths = right_section_child_widths(&spaced_config);
    assert_eq!(spaced_widths.len(), 2);
    assert!(
        spaced_widths[0] > compact_quick_settings_width,
        "the group containing the flexible spacer should absorb spare width"
    );
    assert_eq!(
        spaced_widths[1], compact_quick_settings_width,
        "a sibling group without spacer should stay at its natural width"
    );
    assert!(
        spaced_widths.iter().sum::<i32>() > 300,
        "with no center section, a bare spacer should still expand the side section"
    );
}

fn run_test_widgets_grouping_spacing_contract() {
    let mut config = test_config();
    config.bar.spacing = 18;
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["custom-b".to_string(), "custom-c".to_string()],
        },
    ];
    let mut custom_c = vibepanel_core::config::WidgetOptions::default();
    custom_c
        .options
        .insert("label".to_string(), toml::Value::String("C".to_string()));
    config
        .widgets
        .widget_configs
        .insert("custom-c".to_string(), custom_c);

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let group = find_descendant_with_class(&left_section, crate::styles::class::WIDGET_GROUP)
        .expect("explicit group should render a .widget-group island");
    let grouped_items = collect_descendants_with_class(&group, crate::styles::class::WIDGET_ITEM);
    let mut section_surfaces = Vec::new();
    let mut child = left_section.first_child();
    while let Some(widget) = child {
        section_surfaces.push(
            find_descendant_with_class(&widget, crate::styles::class::WIDGET)
                .expect("each section child should contain a painted widget surface"),
        );
        child = widget.next_sibling();
    }

    assert_eq!(
        section_surfaces.len(),
        2,
        "left section should have one standalone widget surface and one grouped island surface"
    );
    assert_eq!(
        grouped_items.len(),
        2,
        "explicit widget group should keep both custom widgets as grouped items"
    );

    let standalone_bounds = bounds_in_window(&section_surfaces[0], &window);
    let group_bounds = bounds_in_window(&section_surfaces[1], &window);
    let first_grouped_bounds = bounds_in_window(&grouped_items[0], &window);
    let second_grouped_bounds = bounds_in_window(&grouped_items[1], &window);
    let external_gap = measured_gap(standalone_bounds, group_bounds);
    let internal_gap = measured_gap(first_grouped_bounds, second_grouped_bounds);

    assert_eq!(
        external_gap, config.bar.spacing as i32,
        "bar.spacing should still separate standalone widgets from explicit groups"
    );
    assert_eq!(
        internal_gap, 0,
        "explicit group items should not get an extra bar.spacing seam between grouped surfaces"
    );

    window.close();
    flush_gtk();
}

fn run_test_bar_background_click_guard_classes() {
    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["custom-b".to_string(), "custom-c".to_string()],
        },
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["cpu".to_string(), "memory".to_string()],
        },
    ];
    config
        .widgets
        .widget_configs
        .entry("custom-c".to_string())
        .or_default()
        .options
        .insert("label".to_string(), toml::Value::String("C".to_string()));

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let contents = collect_descendants_with_class(&left_section, crate::styles::class::CONTENT);
    assert!(
        contents.len() >= 5,
        "test config should render single, explicit-group, and merge-group content"
    );

    for content in contents {
        assert!(
            has_bar_background_click_guard_ancestor(&content, &left_section),
            "bar widget content should have a guard class ancestor for background clicks"
        );
    }

    window.close();
    flush_gtk();
}

fn merge_group_child_label_gap(override_css: Option<&str>) -> i32 {
    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["cpu".to_string(), "memory".to_string()],
        },
    ];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let fixture = painted_bar_fixture_with_override_css(&config, override_css);
    let left_section = fixture
        .bar
        .section("left")
        .expect("bar should build a left section");
    let merge_group =
        find_descendant_with_class(&left_section, crate::styles::class::WIDGET_MERGE_GROUP)
            .expect("system widgets should render a merge group");
    let contents = collect_descendants_with_class(&merge_group, crate::styles::class::CONTENT);
    assert_eq!(
        contents.len(),
        2,
        "merge group should contain one content box per passive widget"
    );
    let first_last_child = contents[0]
        .last_child()
        .expect("first passive widget content should have a visible trailing child");
    let second_first_child = contents[1]
        .first_child()
        .expect("second passive widget content should have a visible leading child");

    let first_bounds = bounds_in_window(&first_last_child, &fixture.window);
    let second_bounds = bounds_in_window(&second_first_child, &fixture.window);
    let gap = measured_gap(first_bounds, second_bounds);

    fixture.window.close();
    flush_gtk();

    gap
}

fn run_test_widgets_grouping_merge_spacing_contract() {
    let config = test_config();
    let theme_gap = vibepanel_core::ThemePalette::from_config(&config, None, None)
        .sizes
        .widget_content_gap as i32;
    let default_gap = merge_group_child_label_gap(None);
    assert_eq!(
        default_gap, theme_gap,
        "default merge-group child content gap should match the theme gap once"
    );

    let offset = 6;
    let override_css = format!(".widget-merge-group.cpu {{ --widget-gap-adjust: {offset}px; }}");
    let offset_gap = merge_group_child_label_gap(Some(&override_css));
    assert_eq!(
        offset_gap,
        theme_gap + offset,
        "merge-group child content gap should include scoped user gap offset exactly once"
    );
}

fn explicit_group_child_gap(override_css: Option<&str>) -> i32 {
    let mut config = test_config();
    config.bar.size = 40;
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("clock".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["custom-a".to_string(), "custom-b".to_string()],
        },
    ];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let fixture = painted_bar_fixture_with_override_css(&config, override_css);
    let left_section = fixture
        .bar
        .section("left")
        .expect("bar should build a left section");
    let group = find_descendant_with_class(&left_section, crate::styles::class::WIDGET_GROUP)
        .expect("custom widgets should render an explicit group");
    let grouped_items = collect_descendants_with_class(&group, crate::styles::class::WIDGET_ITEM);
    assert_eq!(
        grouped_items.len(),
        2,
        "explicit group should contain two grouped widget items"
    );

    let first_content =
        find_descendant_with_class(&grouped_items[0], crate::styles::class::CONTENT)
            .expect("first explicit group item should contain a content box");
    let second_content =
        find_descendant_with_class(&grouped_items[1], crate::styles::class::CONTENT)
            .expect("second explicit group item should contain a content box");
    let first_last_child = first_content
        .last_child()
        .expect("first explicit group content should have a visible trailing child");
    let second_first_child = second_content
        .first_child()
        .expect("second explicit group content should have a visible leading child");

    let first_bounds = bounds_in_window(&first_last_child, &fixture.window);
    let second_bounds = bounds_in_window(&second_first_child, &fixture.window);
    let gap = measured_gap(first_bounds, second_bounds);

    fixture.window.close();
    flush_gtk();

    gap
}

fn run_test_widgets_grouping_explicit_spacing_contract() {
    let mut config = test_config();
    config.bar.size = 40;
    let theme_gap = vibepanel_core::ThemePalette::from_config(&config, None, None)
        .sizes
        .widget_content_gap as i32;
    let default_gap = explicit_group_child_gap(None);
    assert_eq!(
        default_gap, theme_gap,
        "default explicit-group child content gap should match the theme gap once"
    );

    let offset = 6;
    let override_css = format!(".widget-group {{ --widget-gap-adjust: {offset}px; }}");
    let offset_gap = explicit_group_child_gap(Some(&override_css));
    assert_eq!(
        offset_gap,
        theme_gap + offset,
        "explicit-group child content gap should include scoped user gap offset exactly once"
    );
}

#[derive(Debug, Clone, Copy)]
struct StandaloneContentSpacingMetrics {
    leading_edge: i32,
    child_gap: i32,
}

fn standalone_custom_widget_content_spacing(
    override_css: Option<&str>,
) -> StandaloneContentSpacingMetrics {
    let mut config = test_config();
    config
        .widgets
        .widget_configs
        .entry("custom-a".to_string())
        .or_default()
        .options
        .insert(
            "icon".to_string(),
            toml::Value::String("glyph:*".to_string()),
        );

    let fixture = painted_bar_fixture_with_override_css(&config, override_css);
    let content = find_descendant_with_class(&fixture.first_surface, crate::styles::class::CONTENT)
        .expect("standalone custom widget should contain a content box");
    let first_child = content
        .first_child()
        .expect("custom widget content should have a leading child");
    let second_child = first_child
        .next_sibling()
        .expect("custom widget content should have a second child");

    let content_bounds = bounds_in_window(&content, &fixture.window);
    let first_bounds = bounds_in_window(&first_child, &fixture.window);
    let second_bounds = bounds_in_window(&second_child, &fixture.window);
    let metrics = StandaloneContentSpacingMetrics {
        leading_edge: first_bounds.0 - content_bounds.0,
        child_gap: measured_gap(first_bounds, second_bounds),
    };

    fixture.window.close();
    flush_gtk();

    metrics
}

fn run_test_widgets_scoped_content_spacing_contract() {
    let baseline = standalone_custom_widget_content_spacing(None);
    let padding_offset = 7;
    let gap_offset = 5;
    let override_css = format!(
        ".custom-a {{ --widget-padding-adjust: {padding_offset}px; --widget-gap-adjust: {gap_offset}px; }}"
    );
    let changed = standalone_custom_widget_content_spacing(Some(&override_css));

    assert_eq!(
        changed.leading_edge - baseline.leading_edge,
        padding_offset,
        "widget-scoped content padding offset should move standalone content's leading edge exactly once"
    );
    assert_eq!(
        changed.child_gap - baseline.child_gap,
        gap_offset,
        "widget-scoped content gap offset should change standalone inter-child spacing exactly once"
    );
}

fn custom_widget_surface_widths(override_css: Option<&str>) -> (i32, i32) {
    let mut config = test_config();
    for widget_name in ["custom-a", "custom-b"] {
        config
            .widgets
            .widget_configs
            .entry(widget_name.to_string())
            .or_default()
            .options
            .insert(
                "label".to_string(),
                toml::Value::String("MMMMMMMMMMMM".to_string()),
            );
    }

    let fixture = painted_bar_fixture_with_override_css(&config, override_css);
    let first_bounds = bounds_in_window(&fixture.first_surface, &fixture.window);
    let second_bounds = bounds_in_window(&fixture.second_surface, &fixture.window);

    fixture.window.close();
    flush_gtk();

    (first_bounds.2, second_bounds.2)
}

fn run_test_widgets_scoped_font_scale_contract() {
    let (baseline_first, baseline_second) = custom_widget_surface_widths(None);
    assert_eq!(
        baseline_first, baseline_second,
        "same-label custom widgets should start with matching widths"
    );

    let (scaled_first, scaled_second) =
        custom_widget_surface_widths(Some(".custom-a { --font-scale: 0.1; }"));

    assert!(
        scaled_first < baseline_first / 2,
        "widget-scoped --font-scale should shrink only the targeted widget width"
    );
    assert_eq!(
        scaled_second, baseline_second,
        "widget-scoped --font-scale should not affect adjacent widgets"
    );
}

fn run_test_widgets_background_color_pixel() {
    let global_color = "#112233";
    let widget_color = "#445566";
    let css_color = "#778899";

    let mut global_only = test_config();
    global_only.widgets.background_color = Some(widget_color.to_string());
    global_only.widgets.background_opacity = 1.0;
    if let Some(custom_a) = global_only.widgets.widget_configs.get_mut("custom-a") {
        custom_a
            .options
            .insert("label".to_string(), toml::Value::String(String::new()));
    }

    let mut clock_override = test_config();
    clock_override.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("clock".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
    ];
    clock_override.widgets.background_color = Some(global_color.to_string());
    clock_override.widgets.background_opacity = 1.0;
    clock_override
        .widgets
        .widget_configs
        .entry("clock".to_string())
        .or_default()
        .background_color = Some(widget_color.to_string());

    let user_css = format!(
        ".widget.{clock} {{ --widget-background-color: {css_color}; }}",
        clock = crate::styles::widget::CLOCK
    );

    let cases = [
        (
            "widgets.background_color paints the widget",
            &global_only,
            None,
            widget_color,
        ),
        (
            "clock.background_color beats widgets.background_color",
            &clock_override,
            None,
            widget_color,
        ),
        (
            "user CSS class override beats clock.background_color",
            &clock_override,
            Some(user_css.as_str()),
            css_color,
        ),
    ];

    for (name, config, override_css, expected) in cases {
        let fixture = painted_bar_fixture_with_override_css(config, override_css);
        let rendered = center_pixel_of(&fixture, &fixture.first_surface);
        assert_pixel_close(
            rendered,
            Rgba8::from_hex(expected),
            &format!("{name}: rendered widget pixel"),
        );
        maybe_hold_probe_window();
        fixture.window.close();
        flush_gtk();
    }
}

fn run_test_bar_background_color_css_override_pixel() {
    let toml_color = "#112233";
    let css_color = "#445566";

    let mut config = test_config();
    config.bar.background_color = Some(toml_color.to_string());
    config.bar.background_opacity = 1.0;

    let toml_fixture = painted_bar_fixture(&config);
    let bar_bounds = bounds_in_window(&toml_fixture.bar, &toml_fixture.window);
    let toml_pixel = sample_root_pixel(
        &toml_fixture,
        bar_bounds.0 + 2,
        bar_bounds.1 + bar_bounds.3 / 2,
    );
    assert_pixel_close(
        toml_pixel,
        Rgba8::from_hex(toml_color),
        "bar.background_color should paint the rendered bar background",
    );
    maybe_hold_probe_window();
    toml_fixture.window.close();
    flush_gtk();

    let user_css = format!(
        "sectioned-bar.{bar} {{ --color-background-bar: {css_color}; }}",
        bar = crate::styles::class::BAR
    );
    let css_fixture = painted_bar_fixture_with_override_css(&config, Some(&user_css));
    let css_bar_bounds = bounds_in_window(&css_fixture.bar, &css_fixture.window);
    let css_pixel = sample_root_pixel(
        &css_fixture,
        css_bar_bounds.0 + 2,
        css_bar_bounds.1 + css_bar_bounds.3 / 2,
    );
    assert_pixel_close(
        css_pixel,
        Rgba8::from_hex(css_color),
        "user CSS --color-background-bar override should beat bar.background_color in rendered pixels",
    );
    maybe_hold_probe_window();
    css_fixture.window.close();
    flush_gtk();
}

fn run_test_user_style_css_production_path_pixel() {
    let toml_color = "#112233";
    let css_color = "#445566";

    let mut config = test_config();
    config.widgets.background_color = Some(toml_color.to_string());
    config.widgets.background_opacity = 1.0;
    if let Some(custom_a) = config.widgets.widget_configs.get_mut("custom-a") {
        custom_a
            .options
            .insert("label".to_string(), toml::Value::String(String::new()));
    }

    let dir = TestDir::new("vibepanel-style-css-ui-regression");
    let config_path = dir.path().join("config.toml");
    let style_path = dir.path().join("style.css");
    let imported_dir = dir.path().join("generated");
    let imported_path = imported_dir.join("colors.css");
    std::fs::create_dir_all(&imported_dir).unwrap();
    std::fs::write(&config_path, "# test config placeholder\n").unwrap();
    std::fs::write(
        &imported_path,
        format!(".widget.custom-a {{ --widget-background-color: {css_color}; }}"),
    )
    .unwrap();
    std::fs::write(&style_path, "@import \"generated/colors.css\";\n").unwrap();

    set_ui_regression_config_path(&config, config_path);
    crate::bar::load_css(&config);

    let window_width = 400;
    let window = gtk4::Window::builder()
        .title("vibepanel production style.css UI regression test")
        .default_width(window_width)
        .default_height(crate::bar::rendered_bar_height(&config))
        .build();
    let app = gtk4::Application::builder()
        .application_id("dev.vibepanel.style-css-ui-regression")
        .build();
    let mut state = crate::widgets::BarState::new();
    let built =
        crate::bar::build_bar_content(&app, &config, &mut state, Some("ui-regression-test"));
    built
        .root
        .set_size_request(window_width, crate::bar::rendered_bar_height(&config));
    built
        .bar
        .set_size_request(window_width, config.bar.size as i32);
    window.set_child(Some(&built.root));
    window.present();
    flush_gtk();

    let left_section = built
        .bar
        .section("left")
        .expect("UI regression config should build a left section");
    let first_wrapper = left_section
        .first_child()
        .expect("UI regression config should build first widget");
    let first_surface = find_descendant_with_class(&first_wrapper, crate::styles::class::WIDGET)
        .expect("first real widget should contain a painted .widget surface");
    let bounds = bounds_in_window(&first_surface, &window);
    let rendered = sample_widget_pixel(
        &window,
        built.root.upcast_ref(),
        bounds.0 + bounds.2 / 2,
        bounds.1 + bounds.3 / 2,
    );

    assert_pixel_close(
        rendered,
        Rgba8::from_hex(css_color),
        "relative import from config-adjacent style.css should override rendered widget pixels",
    );

    std::fs::remove_file(style_path).unwrap();
    crate::bar::replace_user_css();
    maybe_hold_probe_window();
    window.close();
    flush_gtk();
}

fn run_test_outline_color_pixels() {
    for (label, color) in [
        ("accent", "accent"),
        ("foreground", "foreground"),
        ("subtle", "subtle"),
        ("hex", "#445566"),
    ] {
        let mut config = test_config();
        config.theme.outline = true;
        config.theme.outline_width = 4;
        config.theme.outline_color = color.to_string();
        config.theme.outline_opacity = 1.0;
        config.widgets.background_color = Some("#101820".to_string());
        config.widgets.background_opacity = 1.0;

        let palette = vibepanel_core::ThemePalette::from_config(&config, None, None);
        let expected = outline_color_for_palette(&palette, color, config.theme.outline_opacity)
            .premultiply_alpha();
        let fixture = painted_bar_fixture(&config);
        let rendered = edge_pixel_of(&fixture, &fixture.first_surface);

        assert_pixel_close(
            rendered,
            expected,
            &format!("theme.outline_color={label} should paint the widget border pixel"),
        );

        maybe_hold_probe_window();
        fixture.window.close();
        flush_gtk();
    }
}

fn run_test_outline_opacity_and_disabled_pixels() {
    let mut transparent_config = test_config();
    transparent_config.theme.outline = true;
    transparent_config.theme.outline_width = 4;
    transparent_config.theme.outline_color = "#80a0c0".to_string();
    transparent_config.theme.outline_opacity = 0.5;
    transparent_config.widgets.background_color = Some("#101820".to_string());
    transparent_config.widgets.background_opacity = 1.0;

    let transparent_fixture = painted_bar_fixture(&transparent_config);
    let transparent_edge = edge_pixel_of(&transparent_fixture, &transparent_fixture.first_surface);
    assert_pixel_close(
        transparent_edge,
        Rgba8::from_hex("#80a0c0")
            .with_alpha(128)
            .premultiply_alpha(),
        "theme.outline_opacity should control the rendered border pixel alpha",
    );
    maybe_hold_probe_window();
    transparent_fixture.window.close();
    flush_gtk();

    let mut disabled_config = transparent_config.clone();
    disabled_config.widgets.outline = Some(false);
    disabled_config.theme.outline_opacity = 1.0;
    let disabled_fixture = painted_bar_fixture(&disabled_config);
    let disabled_edge = edge_pixel_of(&disabled_fixture, &disabled_fixture.first_surface);
    assert_pixel_close(
        disabled_edge,
        Rgba8::from_hex("#101820"),
        "widgets.outline=false should remove the rendered widget border",
    );
    maybe_hold_probe_window();
    disabled_fixture.window.close();
    flush_gtk();
}

fn run_test_widget_outline_color_precedence_pixel() {
    let cases = [
        (
            "custom-a.outline_color beats the global outline color",
            Some("foreground"),
            None,
            "foreground",
        ),
        (
            "user CSS --widget-outline-color paints the targeted widget",
            None,
            Some(".widget.custom-a { --widget-outline-color: #00ff00; }"),
            "#00ff00",
        ),
    ];

    for (name, widget_outline_color, override_css, expected_first) in cases {
        let mut config = test_config();
        config.theme.outline = true;
        config.theme.outline_width = 4;
        config.theme.outline_color = "accent".to_string();
        config.theme.outline_opacity = 1.0;
        config.theme.accent = Some("#224466".to_string());
        config.widgets.background_color = Some("#101820".to_string());
        config.widgets.background_opacity = 1.0;
        if let Some(outline_color) = widget_outline_color {
            config
                .widgets
                .widget_configs
                .entry("custom-a".to_string())
                .or_default()
                .outline_color = Some(outline_color.to_string());
        }

        let palette = vibepanel_core::ThemePalette::from_config(&config, None, None);
        let fixture = painted_bar_fixture_with_override_css(&config, override_css);
        let first_edge = edge_pixel_of(&fixture, &fixture.first_surface);
        let second_edge = edge_pixel_of(&fixture, &fixture.second_surface);

        assert_pixel_close(
            first_edge,
            outline_color_for_palette(&palette, expected_first, 1.0),
            &format!("{name}: first widget outline"),
        );
        assert_pixel_close(
            second_edge,
            outline_color_for_palette(&palette, "accent", 1.0),
            &format!("{name}: sibling widget should keep the global outline color"),
        );

        maybe_hold_probe_window();
        fixture.window.close();
        flush_gtk();
    }
}

fn run_test_theme_mode_dark_light_pixels() {
    let mut dark = test_config();
    dark.theme.mode = "dark".to_string();
    dark.widgets.background_opacity = 1.0;

    let mut light = dark.clone();
    light.theme.mode = "light".to_string();

    let dark_fixture = painted_bar_fixture(&dark);
    let dark_pixel = background_pixel_of(&dark_fixture, &dark_fixture.first_surface);
    maybe_hold_probe_window();
    dark_fixture.window.close();
    flush_gtk();

    let light_fixture = painted_bar_fixture(&light);
    let light_pixel = background_pixel_of(&light_fixture, &light_fixture.first_surface);
    maybe_hold_probe_window();
    light_fixture.window.close();
    flush_gtk();

    assert_luma_delta_at_least(
        dark_pixel,
        light_pixel,
        0.3,
        "theme.mode dark/light should produce visibly different widget pixels",
    );
}

fn run_test_theme_popover_polarity_pixel() {
    let mut config = test_config();
    config.theme.mode = "dark".to_string();
    config.theme.popover = Some("light".to_string());
    config.bar.background_color = Some("#101820".to_string());
    config.widgets.background_opacity = 1.0;
    config.widgets.popover_background_opacity = Some(1.0);

    let bar_fixture = painted_bar_fixture(&config);
    let bar_pixel = background_pixel_of(&bar_fixture, &bar_fixture.first_surface);
    maybe_hold_probe_window();
    bar_fixture.window.close();
    flush_gtk();

    let popover_fixture = painted_surface_fixture(&config, crate::styles::surface::POPOVER);
    let popover_pixel = center_pixel_of_surface(&popover_fixture);
    maybe_hold_probe_window();
    popover_fixture.window.close();
    flush_gtk();

    assert_luma_delta_at_least(
        bar_pixel,
        popover_pixel,
        0.25,
        "theme.popover=light should make a dark bar's popover visibly light",
    );
}

fn run_test_workspaces_urgent_production_pixel() {
    let urgent_color = "#cc3344";
    let mut config = test_config();
    config.theme.states.urgent = urgent_color.to_string();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("workspaces".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-b".to_string()),
    ];
    config.widgets.widget_configs.insert(
        "workspaces".to_string(),
        vibepanel_core::config::WidgetOptions::default(),
    );

    let mut snapshot = crate::services::compositor::WorkspaceSnapshot::default();
    snapshot.active_workspace.insert(1);
    snapshot.occupied_workspaces.insert(2);
    snapshot.urgent_workspaces.insert(2);
    snapshot.window_counts.insert(2, 1);
    crate::services::compositor::CompositorManager::replace_global_for_test(snapshot.clone());
    crate::services::workspace::WorkspaceService::replace_state_for_test(
        vec![
            crate::services::compositor::WorkspaceMeta {
                id: 1,
                idx: 1,
                name: "1".to_string(),
                output: None,
            },
            crate::services::compositor::WorkspaceMeta {
                id: 2,
                idx: 2,
                name: "2".to_string(),
                output: None,
            },
        ],
        snapshot,
    );

    let fixture = painted_bar_fixture(&config);
    let indicators = collect_descendants_with_class(
        fixture.bar.upcast_ref::<gtk4::Widget>(),
        "workspace-indicator",
    );
    let indicator = indicators
        .iter()
        .find(|indicator| indicator.has_css_class(crate::styles::state::URGENT))
        .expect("production workspaces widget should render an urgent indicator");
    assert!(
        indicator.has_css_class(crate::styles::state::URGENT),
        "production workspaces widget should apply the urgent state class"
    );
    let urgent_pixel = center_pixel_of(&fixture, indicator);

    assert_pixel_close(
        urgent_pixel,
        Rgba8::from_hex(urgent_color),
        "production workspaces urgent indicator should render theme.states.urgent",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
}

fn run_test_grouped_seam_pixel() {
    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-c".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["custom-a".to_string(), "custom-b".to_string()],
        },
    ];
    config.widgets.background_color = Some("#223344".to_string());
    config.widgets.background_opacity = 1.0;
    for (name, label) in [("custom-a", "A"), ("custom-b", "B"), ("custom-c", "C")] {
        config
            .widgets
            .widget_configs
            .entry(name.to_string())
            .or_default()
            .options
            .insert("label".to_string(), toml::Value::String(label.to_string()));
        config
            .widgets
            .widget_configs
            .entry(name.to_string())
            .or_default()
            .options
            .insert(
                "on_click".to_string(),
                toml::Value::String("true".to_string()),
            );
    }

    let fixture = painted_bar_fixture(&config);
    let left_section = fixture
        .bar
        .section("left")
        .expect("bar should build a left section");
    let group = find_descendant_with_class(&left_section, crate::styles::class::WIDGET_GROUP)
        .expect("explicit group should render a widget group");
    let grouped_items = collect_descendants_with_class(&group, crate::styles::class::WIDGET_ITEM);
    assert_eq!(grouped_items.len(), 2);

    let second_bounds = bounds_in_window(&grouped_items[1], &fixture.window);
    let seam_pixel = sample_root_pixel(
        &fixture,
        second_bounds.0,
        second_bounds.1 + second_bounds.3 / 2,
    );
    assert_pixel_close(
        seam_pixel,
        Rgba8::from_hex("#223344"),
        "grouped widget seam should paint the widget background, not transparent",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
}

macro_rules! ui_regression_config_tests {
    ($(($test_name:ident, $test_case:literal, $runner:ident)),+ $(,)?) => {
        $(
            #[test]
            #[ignore = "UI regression test: opens GTK windows; run under Xvfb"]
            fn $test_name() {
                run_ui_regression_config_subprocess($test_case);
            }
        )+

        #[test]
        #[ignore = "internal runner for one UI regression config subprocess"]
        fn ui_regression_config_runner() {
            if !init_gtk_or_skip("UI regression config test") {
                return;
            }
            set_ui_regression_config(&test_config());

            match std::env::var("VIBEPANEL_UI_REGRESSION_TEST").as_deref() {
                $(Ok($test_case) => $runner(),)+
                Ok(other) => panic!("unknown UI regression config test: {other}"),
                Err(_) => eprintln!("skipping UI regression config runner: no contract selected"),
            }
        }
    };
}

// Contract categories:
// - UI regression: live GTK layout/rendering measurements or pixel samples.
// - css binding: app-layer selector/class wiring that consumes core CSS tokens.
// Pure config-to-token checks live in vibepanel-core unit tests.
ui_regression_config_tests!(
    (
        test_ui_regression_bar_layout,
        "bar.layout",
        run_test_config_bar_layout
    ),
    (
        test_ui_regression_widgets_placement,
        "widgets.placement",
        run_test_widgets_placement
    ),
    (
        test_ui_regression_custom_widget_json_exec_output,
        "custom_widget.json_exec_output",
        run_test_custom_widget_json_exec_output
    ),
    (
        test_ui_regression_widgets_grouping_structure,
        "widgets.grouping.structure",
        run_test_widgets_grouping_structure
    ),
    (
        test_ui_regression_widgets_grouping_spacer_expansion,
        "widgets.grouping.spacer-expansion",
        run_test_widgets_grouping_spacer_expansion
    ),
    (
        test_ui_regression_widgets_grouping_spacing,
        "widgets.grouping.spacing",
        run_test_widgets_grouping_spacing_contract
    ),
    (
        test_ui_regression_bar_background_click_guard_classes,
        "bar.background_click_guard_classes",
        run_test_bar_background_click_guard_classes
    ),
    (
        test_ui_regression_widgets_grouping_merge_spacing,
        "widgets.grouping.merge-spacing",
        run_test_widgets_grouping_merge_spacing_contract
    ),
    (
        test_ui_regression_widgets_grouping_explicit_spacing,
        "widgets.grouping.explicit-spacing",
        run_test_widgets_grouping_explicit_spacing_contract
    ),
    (
        test_ui_regression_widgets_scoped_content_spacing,
        "widgets.scoped-content-spacing",
        run_test_widgets_scoped_content_spacing_contract
    ),
    (
        test_ui_regression_widgets_scoped_font_scale,
        "widgets.scoped-font-scale",
        run_test_widgets_scoped_font_scale_contract
    ),
    (
        test_ui_regression_widgets_background_color_pixel,
        "widgets.background_color_pixel",
        run_test_widgets_background_color_pixel
    ),
    (
        test_ui_regression_bar_background_color_css_override_pixel,
        "bar.background_color_css_override_pixel",
        run_test_bar_background_color_css_override_pixel
    ),
    (
        test_ui_regression_user_style_css_production_path_pixel,
        "user_style_css.production_path_pixel",
        run_test_user_style_css_production_path_pixel
    ),
    (
        test_ui_regression_outline_color_pixels,
        "theme.outline_color_pixel",
        run_test_outline_color_pixels
    ),
    (
        test_ui_regression_outline_opacity_and_disabled_pixels,
        "theme.outline_opacity_disabled_pixel",
        run_test_outline_opacity_and_disabled_pixels
    ),
    (
        test_ui_regression_widget_outline_color_precedence_pixel,
        "widgets.outline_color_precedence_pixel",
        run_test_widget_outline_color_precedence_pixel
    ),
    (
        test_ui_regression_theme_mode_dark_light_pixel,
        "theme.mode_dark_light_pixel",
        run_test_theme_mode_dark_light_pixels
    ),
    (
        test_ui_regression_theme_popover_polarity_pixel,
        "theme.popover_polarity_pixel",
        run_test_theme_popover_polarity_pixel
    ),
    (
        test_ui_regression_workspaces_urgent_production_pixel,
        "workspaces.urgent_production_pixel",
        run_test_workspaces_urgent_production_pixel
    ),
    (
        test_ui_regression_widgets_grouped_seam_pixel,
        "widgets.grouping.seam_pixel",
        run_test_grouped_seam_pixel
    ),
);
