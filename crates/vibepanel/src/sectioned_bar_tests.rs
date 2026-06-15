use super::*;
use crate::theme_vars::{THEME_VAR_EXPECTATIONS, ThemeVarRole, ThemeVarScope};
use crate::ui_regression_test_support::{
    CssProviderGuard, Rgba8, assert_pixel_close, center_pixel_of_surface, edge_pixel_of_surface,
    find_descendant_with_class, flush_gtk, init_gtk_or_skip, label_with_text,
    maybe_hold_probe_window, painted_surface_fixture_with_classes, run_ignored_contract_subprocess,
    sample_widget_pixel,
};
use crate::widgets::css::{POPOVER_BG_WITH_OPACITY, WIDGET_BG_WITH_OPACITY};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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

struct UiRegressionTestDir(PathBuf);

impl UiRegressionTestDir {
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

impl Drop for UiRegressionTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn ui_regression_config_css(config: &Config) -> String {
    let palette = vibepanel_core::ThemePalette::from_config(config, None, None);
    let popover_palette = vibepanel_core::ThemePalette::popover_palette(config, None, None);
    crate::bar::generate_css(config, &palette, popover_palette.as_ref())
}

fn production_bar_declarations(config: &Config) -> String {
    let css = crate::widgets::css::widget_css(config);
    let selector = format!(
        "{}.{class}",
        crate::styles::class::SECTIONED_BAR,
        class = crate::styles::class::BAR
    );
    let selector_pos = css
        .find(&selector)
        .unwrap_or_else(|| panic!("production widget CSS should contain the {selector} selector"));
    let selector_block = &css[selector_pos..];
    let block_end = selector_block
        .find('}')
        .expect("production bar CSS selector should have a declaration block");

    selector_block[..block_end].to_string()
}
fn assert_bar_style_bindings(config: &Config, bar: &SectionedBar) {
    assert!(
        bar.has_css_class(crate::styles::class::BAR),
        "production-built SectionedBar should carry the .bar CSS class"
    );

    let declarations = production_bar_declarations(config);

    assert!(
        declarations.contains("background: var(--color-background-bar);"),
        "production bar CSS should apply --color-background-bar to sectioned-bar.bar"
    );
    assert!(
        declarations.contains("background-clip: padding-box;"),
        "production bar CSS should keep the background inside the outline"
    );
    assert!(
        declarations.contains("border-radius: var(--radius-bar);"),
        "production bar CSS should apply --radius-bar to sectioned-bar.bar"
    );
    assert!(
        declarations.contains("border: var(--bar-outline-width) solid"),
        "production bar CSS should apply --bar-outline-width to sectioned-bar.bar"
    );
}

fn production_widget_declarations(config: &Config) -> String {
    let css = crate::widgets::css::widget_css(config);
    let selector = ".widget {";
    let selector_pos = css
        .find(selector)
        .expect("production widget CSS should contain the .widget selector");
    let selector_block = &css[selector_pos..];
    let block_end = selector_block
        .find('}')
        .expect("production .widget selector should have a declaration block");

    selector_block[..block_end].to_string()
}

fn assert_widget_style_bindings(config: &Config) {
    let declarations = production_widget_declarations(config);

    assert!(
        declarations.contains(&format!("background-color: {WIDGET_BG_WITH_OPACITY};")),
        "production widget CSS should apply --widget-background-color and --widget-background-opacity to .widget"
    );
    assert!(
        declarations.contains("background-clip: padding-box;"),
        "production widget CSS should keep the background inside the outline"
    );
    assert!(
        declarations.contains("border-radius: var(--radius-widget);"),
        "production widget CSS should apply --radius-widget to .widget"
    );
    assert!(
        declarations.contains("border: var(--widget-outline-width) solid"),
        "production widget CSS should apply --widget-outline-width to .widget"
    );
}

fn production_popover_declarations(config: &Config) -> String {
    let css = crate::widgets::css::utility_css(config);
    let selector = ".popover {";
    let selector_pos = css
        .find(selector)
        .expect("production utility CSS should contain the .popover selector");
    let selector_block = &css[selector_pos..];
    let block_end = selector_block
        .find('}')
        .expect("production .popover selector should have a declaration block");

    selector_block[..block_end].to_string()
}

fn assert_popover_style_bindings(config: &Config) {
    let declarations = production_popover_declarations(config);

    assert!(
        declarations.contains(&format!("background-color: {POPOVER_BG_WITH_OPACITY};")),
        "production popover CSS should apply --widget-background-color and --popover-background-opacity to .popover"
    );
    assert!(
        declarations.contains("background-clip: padding-box;"),
        "production popover CSS should keep the background inside the outline"
    );
    assert!(
        declarations.contains("border: var(--surface-outline-width) solid"),
        "production popover CSS should apply --surface-outline-width to .popover"
    );
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
}

#[test]
fn bar_padding_reads_physically_in_horizontal_and_vertical() {
    // Regression: .bar--vertical must not re-swap padding — the generator
    // already emits correct physical sides and the base rule reads them
    // physically (see theme.rs css_vars_block).
    let config = test_config();
    let css = strip_css_comments(&crate::widgets::css::widget_css(&config));

    let horizontal_block =
        block_after_selector(&css, "sectioned-bar.bar {").expect("horizontal bar block must exist");
    let vertical_block = block_after_selector(&css, "sectioned-bar.bar.bar--vertical {")
        .expect("vertical bar block must exist");

    fn pick<'a>(css: &'a str, property: &str) -> &'a str {
        for line in css.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix(property) {
                return rest.trim_start().trim_end_matches(';').trim();
            }
        }
        ""
    }

    let h_top = pick(horizontal_block, "padding-top:");
    let h_right = pick(horizontal_block, "padding-right:");
    assert!(
        h_top.contains("--vp-internal-bar-padding-top"),
        "horizontal bar padding-top should read --vp-internal-bar-padding-top; got {h_top:?}"
    );
    assert!(
        h_right.contains("--vp-internal-bar-padding-right"),
        "horizontal bar padding-right should read --vp-internal-bar-padding-right; got {h_right:?}"
    );

    // .bar--vertical must not rotate physical padding onto the flow axis.
    for property in [
        "padding-top:",
        "padding-right:",
        "padding-bottom:",
        "padding-left:",
    ] {
        assert!(
            !vertical_block.contains(property),
            ".bar--vertical must not override {property} — bar padding reads \
             physically from --vp-internal-bar-padding-* in the base rule. \
             Reintroducing this swap rotates the cross-axis padding onto the \
             flow axis and is a user-visible regression.\n\
             Vertical block:\n{vertical_block}"
        );
    }
}

fn block_after_selector<'a>(css: &'a str, selector: &str) -> Option<&'a str> {
    let start = css.find(selector)?;
    let brace_open = css[start..].find('{')? + start;
    let depth_open = brace_open + 1;
    let mut depth = 1usize;
    let mut idx = depth_open;
    while idx < css.len() && depth > 0 {
        match css.as_bytes()[idx] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        idx += 1;
    }
    Some(&css[depth_open..idx - 1])
}

#[test]
fn theme_vars_cover_hover_bindings() {
    let config = test_config();
    let production_css = crate::widgets::css::widget_css(&config);
    let var_uses = css_var_uses_in_css(&production_css);
    let hover_vars = THEME_VAR_EXPECTATIONS
        .iter()
        .filter(|var| var.hover_binding)
        .map(|var| var.name)
        .collect::<Vec<_>>();
    let missing_hover_bindings = hover_vars
        .iter()
        .filter(|name| {
            !var_uses
                .iter()
                .any(|var_use| var_use.name == **name && var_use.in_hover_selector)
        })
        .copied()
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
    assert_bar_style_bindings(config, &built.bar);
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

fn run_test_config_bar_size() {
    let baseline = test_config();
    let mut changed = baseline.clone();
    changed.bar.size = 40;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);

    assert_eq!(
        changed_metrics.first_surface_bounds.3, changed.bar.size as i32,
        "bar.size should control the live measured painted widget surface height"
    );
    assert_eq!(
        changed_metrics.bar_bounds.3 - baseline_metrics.bar_bounds.3,
        (changed.bar.size - baseline.bar.size) as i32,
        "changing bar.size should grow the live bar allocation by the same delta"
    );
    assert_eq!(
        changed_metrics.painted_surface_gap, baseline_metrics.painted_surface_gap,
        "changing bar.size should not change bar.spacing's measured painted surface gap"
    );
}

fn run_test_config_bar_spacing() {
    let baseline = test_config();
    let mut changed = baseline.clone();
    changed.bar.spacing = 16;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);

    assert_eq!(
        changed_metrics.painted_surface_gap, changed.bar.spacing as i32,
        "bar.spacing should control the live measured gap between painted sibling widget surfaces"
    );
    assert_eq!(
        changed_metrics.bar_bounds.3, baseline_metrics.bar_bounds.3,
        "changing bar.spacing should not change bar.size's measured bar height"
    );
}

fn run_test_config_bar_inset() {
    let baseline = test_config();
    let mut changed = baseline.clone();
    changed.bar.inset = 20;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);

    assert_eq!(
        changed_metrics.first_surface_bounds.0, changed.bar.inset as i32,
        "bar.inset should control the live measured x-position of the first painted widget surface"
    );
    assert_eq!(
        changed_metrics.bar_bounds.3, baseline_metrics.bar_bounds.3,
        "changing bar.inset should not change bar.size's measured bar height"
    );
    assert_eq!(
        changed_metrics.painted_surface_gap, baseline_metrics.painted_surface_gap,
        "changing bar.inset should not change bar.spacing's measured painted surface gap"
    );
}

fn run_test_config_bar_screen_margin() {
    let baseline = test_config();
    let mut changed = baseline.clone();
    changed.bar.screen_margin = 24;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);

    let expected_first_x = changed.bar.screen_margin + changed.bar.inset;

    assert_eq!(
        changed_metrics.first_surface_bounds.0, expected_first_x as i32,
        "bar.screen_margin should offset the live measured x-position of the first painted widget surface"
    );
    assert_eq!(
        changed_metrics.bar_bounds.3, baseline_metrics.bar_bounds.3,
        "changing bar.screen_margin should not change bar.size's measured bar height"
    );
    assert_eq!(
        changed_metrics.painted_surface_gap, baseline_metrics.painted_surface_gap,
        "changing bar.screen_margin should not change bar.spacing's measured painted surface gap"
    );
}

fn run_test_config_bar_padding() {
    let mut baseline = test_config();
    baseline.bar.background_opacity = 1.0;
    baseline.bar.padding = 4;

    let mut changed = baseline.clone();
    changed.bar.padding = 10;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);
    let expected_shell_height = crate::bar::rendered_bar_height(&changed);

    assert_eq!(
        changed_metrics.shell_bounds.3, expected_shell_height,
        "bar.padding should contribute to live measured rendered shell height when bar background is visible"
    );
    assert_eq!(
        changed_metrics.first_surface_bounds.3, baseline_metrics.first_surface_bounds.3,
        "changing bar.padding should not change the painted widget surface height"
    );
    assert_eq!(
        changed_metrics.painted_surface_gap, baseline_metrics.painted_surface_gap,
        "changing bar.padding should not change bar.spacing's measured painted surface gap"
    );
}

fn run_test_config_bar_background_opacity() {
    let mut baseline = test_config();
    baseline.bar.padding = 10;
    baseline.bar.background_opacity = 0.0;

    let mut changed = baseline.clone();
    changed.bar.background_opacity = 1.0;

    let baseline_metrics = painted_bar_metrics(&baseline);
    let changed_metrics = painted_bar_metrics(&changed);

    assert_eq!(
        crate::bar::rendered_bar_height(&baseline),
        baseline.bar.size as i32 + baseline.bar.padding as i32,
        "transparent bar.background_opacity should include only screen-edge bar.padding in the layer-shell height"
    );
    assert_eq!(
        crate::bar::rendered_bar_height(&changed),
        changed.bar.size as i32 + 2 * changed.bar.padding as i32,
        "visible bar.background_opacity should include both sides of bar.padding in the layer-shell height"
    );
    assert_eq!(
        changed_metrics.bar_bounds.3 - baseline_metrics.bar_bounds.3,
        changed.bar.padding as i32,
        "changing bar.background_opacity from transparent to visible should expose the center-side bar padding in the live widget tree"
    );
    assert_eq!(
        changed_metrics.painted_surface_gap, baseline_metrics.painted_surface_gap,
        "changing bar.background_opacity should not change bar.spacing's measured painted surface gap"
    );
}

fn run_test_widgets_placement_sections() {
    let mut config = test_config();
    config.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Single(
        "custom-a".to_string(),
    )];
    config.widgets.center = vec![vibepanel_core::config::WidgetPlacement::Single(
        "custom-b".to_string(),
    )];
    config.widgets.right = vec![vibepanel_core::config::WidgetPlacement::Single(
        "clock".to_string(),
    )];

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);

    let left_classes = section_widget_class_names(&bar, "left", &["custom-a"]);
    let center_classes = section_widget_class_names(&bar, "center", &["custom-b"]);
    let right_classes = section_widget_class_names(&bar, "right", &[crate::styles::widget::CLOCK]);

    assert_eq!(left_classes, vec!["custom-a".to_string()]);
    assert_eq!(center_classes, vec!["custom-b".to_string()]);
    assert_eq!(
        right_classes,
        vec![crate::styles::widget::CLOCK.to_string()]
    );

    window.close();
    flush_gtk();
}

fn run_test_widgets_disabled() {
    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-b".to_string()),
    ];
    config
        .widgets
        .widget_configs
        .insert("custom-b".to_string(), widget_options_with_disabled(true));

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let custom_a_count = count_descendants_with_class(&left_section, "custom-a");
    let custom_b_count = count_descendants_with_class(&left_section, "custom-b");

    assert!(
        custom_a_count > 0,
        "enabled sibling should remain present in the real bar tree"
    );
    assert_eq!(
        custom_b_count, 0,
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

fn run_test_widgets_grouping_explicit_group() {
    let mut baseline = test_config();
    baseline.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-b".to_string()),
    ];

    let mut changed = baseline.clone();
    changed.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Group {
        group: vec!["custom-a".to_string(), "custom-b".to_string()],
    }];

    let (
        baseline_window,
        baseline_bar,
        _baseline_state,
        _baseline_popover_registry_guard,
        _baseline_css_provider,
    ) = built_bar_fixture(&baseline);
    let (
        changed_window,
        changed_bar,
        _changed_state,
        _changed_popover_registry_guard,
        _changed_css_provider,
    ) = built_bar_fixture(&changed);
    let baseline_left = baseline_bar
        .section("left")
        .expect("baseline bar should build a left section");
    let changed_left = changed_bar
        .section("left")
        .expect("changed bar should build a left section");
    let baseline_group_count =
        count_descendants_with_class(&baseline_left, crate::styles::class::WIDGET_GROUP);
    let changed_group_count =
        count_descendants_with_class(&changed_left, crate::styles::class::WIDGET_GROUP);
    let changed_merge_count =
        count_descendants_with_class(&changed_left, crate::styles::class::WIDGET_MERGE_GROUP);
    let changed_custom_a_count = count_descendants_with_class(&changed_left, "custom-a");
    let changed_custom_b_count = count_descendants_with_class(&changed_left, "custom-b");

    assert_eq!(baseline_group_count, 0);
    assert_eq!(changed_group_count, 1);
    assert_eq!(changed_merge_count, 0);
    assert!(changed_custom_a_count > 0);
    assert!(changed_custom_b_count > 0);

    baseline_window.close();
    changed_window.close();
    flush_gtk();
}

fn run_test_widgets_grouping_system_merge() {
    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["cpu".to_string(), "memory".to_string()],
        },
    ];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let group_count =
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_GROUP);
    let merge_group_count =
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_MERGE_GROUP);
    let passive_count = count_descendants_with_class(&left_section, crate::styles::class::PASSIVE);
    let cpu_count = count_descendants_with_class(&left_section, "cpu");
    let memory_count = count_descendants_with_class(&left_section, "memory");

    assert_eq!(group_count, 1);
    assert_eq!(merge_group_count, 1);
    assert_eq!(passive_count, 2);
    assert!(
        cpu_count > 0,
        "cpu should remain visible in the production merge-group subtree"
    );
    assert!(
        memory_count > 0,
        "memory should remain visible in the production merge-group subtree"
    );

    window.close();
    flush_gtk();
}

fn clock_weather_group_counts(show_weather: Option<bool>) -> (usize, usize, usize, usize, usize) {
    let mut config = test_config();
    config.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Group {
        group: vec!["clock".to_string(), "weather".to_string()],
    }];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();
    if let Some(show_weather) = show_weather {
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

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let counts = (
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_GROUP),
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_MERGE_GROUP),
        count_descendants_with_class(&left_section, crate::styles::class::PASSIVE),
        count_descendants_with_class(&left_section, "clock"),
        count_descendants_with_class(&left_section, "weather"),
    );

    window.close();
    flush_gtk();
    counts
}

fn run_test_widgets_grouping_clock_weather_merge() {
    let (groups, merge_groups, passive, clock, weather) = clock_weather_group_counts(None);
    assert_eq!((groups, merge_groups, passive), (1, 1, 2));
    assert!(clock > 0);
    assert!(weather > 0);
}

fn run_test_widgets_grouping_clock_weather_explicit_opt_out() {
    let (groups, merge_groups, passive, clock, weather) = clock_weather_group_counts(Some(false));
    assert_eq!((groups, merge_groups, passive), (1, 0, 0));
    assert!(clock > 0);
    assert!(weather > 0);
}

fn clock_spacer_group_counts(show_weather: Option<bool>) -> (usize, usize, usize, usize, usize) {
    let mut config = test_config();
    config.widgets.left = vec![vibepanel_core::config::WidgetPlacement::Group {
        group: vec!["clock".to_string(), "spacer".to_string()],
    }];
    config.widgets.center = Vec::new();
    config.widgets.right = Vec::new();
    if let Some(show_weather) = show_weather {
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

    let (window, bar, _state, _popover_registry_guard, _css_provider) = built_bar_fixture(&config);
    let left_section = bar
        .section("left")
        .expect("bar should build a left section");
    let counts = (
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_GROUP),
        count_descendants_with_class(&left_section, crate::styles::class::WIDGET_MERGE_GROUP),
        count_descendants_with_class(&left_section, crate::styles::class::PASSIVE),
        count_descendants_with_class(&left_section, "clock"),
        count_descendants_with_class(&left_section, "spacer"),
    );

    window.close();
    flush_gtk();
    counts
}

fn run_test_widgets_grouping_clock_spacer_merge() {
    let (groups, merge_groups, passive, clock, spacer) = clock_spacer_group_counts(None);
    assert_eq!((groups, merge_groups, passive), (1, 1, 2));
    assert!(clock > 0);
    assert!(spacer > 0);
}

fn run_test_widgets_grouping_clock_spacer_merge_opt_out_ignored() {
    let (groups, merge_groups, passive, clock, spacer) = clock_spacer_group_counts(Some(false));
    assert_eq!((groups, merge_groups, passive), (1, 1, 2));
    assert!(clock > 0);
    assert!(spacer > 0);
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

fn run_test_widgets_grouping_spacer_does_not_expand_sibling_group() {
    let compact_config = config_with_quick_settings_group();
    let compact_widths = right_section_child_widths(&compact_config);
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
}

fn run_test_widgets_empty_center_spacer_expands_side_section() {
    let mut config = config_with_quick_settings_group();
    config.widgets.right.insert(
        0,
        vibepanel_core::config::WidgetPlacement::Group {
            group: vec!["clock".to_string(), "spacer".to_string()],
        },
    );

    let widths = right_section_child_widths(&config);
    assert_eq!(widths.len(), 2);
    assert!(
        widths.iter().sum::<i32>() > 300,
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
    let color = "#445566";
    let mut config = test_config();
    config.widgets.background_color = Some(color.to_string());
    config.widgets.background_opacity = 1.0;
    if let Some(custom_a) = config.widgets.widget_configs.get_mut("custom-a") {
        custom_a
            .options
            .insert("label".to_string(), toml::Value::String(String::new()));
    }

    assert_widget_style_bindings(&config);

    let fixture = painted_bar_fixture(&config);
    let rendered = center_pixel_of(&fixture, &fixture.first_surface);
    assert_pixel_close(
        rendered,
        Rgba8::from_hex(color),
        "opaque widgets.background_color should paint the sampled widget surface pixel",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
}

fn run_test_widgets_background_color_precedence_pixel() {
    let global_color = "#112233";
    let widget_color = "#445566";
    let css_color = "#778899";

    let mut config = test_config();
    config.widgets.left = vec![
        vibepanel_core::config::WidgetPlacement::Single("clock".to_string()),
        vibepanel_core::config::WidgetPlacement::Single("custom-a".to_string()),
    ];
    config.widgets.background_color = Some(global_color.to_string());
    config.widgets.background_opacity = 1.0;
    config
        .widgets
        .widget_configs
        .entry("clock".to_string())
        .or_default()
        .background_color = Some(widget_color.to_string());

    let css = ui_regression_config_css(&config);
    assert!(
        css.contains(&format!(
            ".widget.{clock},",
            clock = crate::styles::widget::CLOCK
        )),
        "per-widget CSS should scope clock overrides to the clock widget class"
    );
    assert!(
        css.contains(&format!("--widget-background-color: {widget_color};")),
        "clock widget config should emit a per-widget background token override"
    );

    let widget_fixture = painted_bar_fixture(&config);
    let widget_rendered = center_pixel_of(&widget_fixture, &widget_fixture.first_surface);
    assert_pixel_close(
        widget_rendered,
        Rgba8::from_hex(widget_color),
        "clock.background_color should override global widgets.background_color in the rendered widget pixel",
    );
    maybe_hold_probe_window();
    widget_fixture.window.close();
    flush_gtk();

    let user_css = format!(
        ".widget.{clock} {{ --widget-background-color: {css_color}; }}",
        clock = crate::styles::widget::CLOCK
    );
    let css_fixture = painted_bar_fixture_with_override_css(&config, Some(&user_css));
    let css_rendered = center_pixel_of(&css_fixture, &css_fixture.first_surface);
    assert_pixel_close(
        css_rendered,
        Rgba8::from_hex(css_color),
        "user CSS class override should beat clock.background_color in the rendered widget pixel",
    );
    maybe_hold_probe_window();
    css_fixture.window.close();
    flush_gtk();
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

    let dir = UiRegressionTestDir::new("vibepanel-style-css-ui-regression");
    let config_path = dir.path().join("config.toml");
    let style_path = dir.path().join("style.css");
    std::fs::write(&config_path, "# test config placeholder\n").unwrap();
    std::fs::write(
        &style_path,
        format!(".widget.custom-a {{ --widget-background-color: {css_color}; }}"),
    )
    .unwrap();

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
        "config-adjacent style.css loaded by production load_css should override rendered widget pixels",
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

fn run_test_per_widget_outline_color_pixel() {
    let mut config = test_config();
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = "accent".to_string();
    config.theme.outline_opacity = 1.0;
    config.theme.accent = Some("#224466".to_string());
    config.widgets.background_color = Some("#101820".to_string());
    config.widgets.background_opacity = 1.0;
    config
        .widgets
        .widget_configs
        .entry("custom-a".to_string())
        .or_default()
        .outline_color = Some("foreground".to_string());

    let css = ui_regression_config_css(&config);
    assert!(
        css.contains(".widget.custom-a,")
            && css.contains("--widget-outline-color: var(--color-foreground-primary);"),
        "per-widget outline_color should emit a scoped widget outline token"
    );

    let palette = vibepanel_core::ThemePalette::from_config(&config, None, None);
    let fixture = painted_bar_fixture(&config);
    let first_edge = edge_pixel_of(&fixture, &fixture.first_surface);
    let second_edge = edge_pixel_of(&fixture, &fixture.second_surface);

    assert_pixel_close(
        first_edge,
        outline_color_for_palette(&palette, "foreground", 1.0),
        "custom-a.outline_color should override the global outline color on the first widget",
    );
    assert_pixel_close(
        second_edge,
        outline_color_for_palette(&palette, "accent", 1.0),
        "sibling widget should keep the global outline color",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
}

fn run_test_widget_outline_color_css_override_pixel() {
    let css_color = "#00ff00";
    let bg_color = "#101820";
    let mut config = test_config();
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = "accent".to_string();
    config.theme.outline_opacity = 1.0;
    config.theme.accent = Some("#224466".to_string());
    config.widgets.background_color = Some(bg_color.to_string());
    config.widgets.background_opacity = 1.0;

    let user_css = format!(".widget.custom-a {{ --widget-outline-color: {css_color}; }}");
    let fixture = painted_bar_fixture_with_override_css(&config, Some(&user_css));
    let first_edge = edge_pixel_of(&fixture, &fixture.first_surface);
    let second_edge = edge_pixel_of(&fixture, &fixture.second_surface);

    assert_pixel_close(
        first_edge,
        Rgba8::from_hex(css_color),
        "user CSS --widget-outline-color override should paint the targeted widget outline",
    );
    assert_pixel_close(
        second_edge,
        outline_color_for_palette(
            &vibepanel_core::ThemePalette::from_config(&config, None, None),
            "accent",
            1.0,
        ),
        "sibling widget should keep the TOML/theme outline color",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
}

fn run_test_surface_outline_css_gsk_parity() {
    let mut config = test_config();
    config.theme.outline = true;
    config.theme.outline_width = 4;
    config.theme.outline_color = "accent".to_string();
    config.theme.outline_opacity = 0.8;
    config.theme.accent = Some("#446688".to_string());
    config.widgets.background_color = Some("#101820".to_string());
    config.widgets.popover_background_opacity = Some(1.0);

    set_ui_regression_config(&config);
    let surface_fixture = painted_surface_fixture(&config, crate::styles::surface::POPOVER);
    let css_edge = edge_pixel_of_surface(&surface_fixture);
    let gsk_rgba = crate::services::config_manager::ConfigManager::global()
        .surface_outline_rgba_for_widget("custom-a", &surface_fixture.surface);
    let gsk_edge = Rgba8::from_gdk(gsk_rgba).premultiply_alpha();

    assert_pixel_close(
        css_edge,
        gsk_edge,
        "CSS-rendered surface outline and GSK animated outline resolver should agree",
    );

    maybe_hold_probe_window();
    surface_fixture.window.close();
    flush_gtk();
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

    let css = ui_regression_config_css(&config);
    assert!(
        css.contains("/* ===== Popover polarity override ===== */"),
        "theme.popover should emit scoped popover palette overrides"
    );
    assert_popover_style_bindings(&config);

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

fn run_test_theme_states_urgent_pixel() {
    let urgent_color = "#cc3344";
    let mut config = test_config();
    config.theme.states.urgent = urgent_color.to_string();
    if let Some(custom_a) = config.widgets.widget_configs.get_mut("custom-a") {
        custom_a
            .options
            .insert("label".to_string(), toml::Value::String(String::new()));
    }

    let fixture = painted_surface_fixture_with_classes(
        &config,
        &["workspace-indicator", crate::styles::state::URGENT],
        120,
        32,
    );
    let urgent_pixel = center_pixel_of_surface(&fixture);

    assert_pixel_close(
        urgent_pixel,
        Rgba8::from_hex(urgent_color),
        "theme.states.urgent should paint urgent workspace/taskbar surfaces",
    );

    maybe_hold_probe_window();
    fixture.window.close();
    flush_gtk();
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
            if !init_gtk_or_skip("UI regression config test", Some("VIBEPANEL_UI_REGRESSION_REQUIRED")) {
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
        test_ui_regression_bar_size,
        "bar.size",
        run_test_config_bar_size
    ),
    (
        test_ui_regression_bar_spacing,
        "bar.spacing",
        run_test_config_bar_spacing
    ),
    (
        test_ui_regression_bar_inset,
        "bar.inset",
        run_test_config_bar_inset
    ),
    (
        test_ui_regression_bar_screen_margin,
        "bar.screen_margin",
        run_test_config_bar_screen_margin
    ),
    (
        test_ui_regression_bar_padding,
        "bar.padding",
        run_test_config_bar_padding
    ),
    (
        test_ui_regression_bar_background_opacity,
        "bar.background_opacity",
        run_test_config_bar_background_opacity
    ),
    (
        test_ui_regression_widgets_placement_sections,
        "widgets.placement.sections",
        run_test_widgets_placement_sections
    ),
    (
        test_ui_regression_widgets_disabled,
        "widgets.disabled",
        run_test_widgets_disabled
    ),
    (
        test_ui_regression_custom_widget_json_exec_output,
        "custom_widget.json_exec_output",
        run_test_custom_widget_json_exec_output
    ),
    (
        test_ui_regression_widgets_grouping_explicit_group,
        "widgets.grouping.explicit-group",
        run_test_widgets_grouping_explicit_group
    ),
    (
        test_ui_regression_widgets_grouping_system_merge,
        "widgets.grouping.system-merge",
        run_test_widgets_grouping_system_merge
    ),
    (
        test_ui_regression_widgets_grouping_clock_weather_merge,
        "widgets.grouping.clock-weather-merge",
        run_test_widgets_grouping_clock_weather_merge
    ),
    (
        test_ui_regression_widgets_grouping_clock_weather_opt_out,
        "widgets.grouping.clock-weather-opt-out",
        run_test_widgets_grouping_clock_weather_explicit_opt_out
    ),
    (
        test_ui_regression_widgets_grouping_clock_spacer_merge,
        "widgets.grouping.clock-spacer-merge",
        run_test_widgets_grouping_clock_spacer_merge
    ),
    (
        test_ui_regression_widgets_grouping_clock_spacer_merge_opt_out_ignored,
        "widgets.grouping.clock-spacer-merge-opt-out-ignored",
        run_test_widgets_grouping_clock_spacer_merge_opt_out_ignored
    ),
    (
        test_ui_regression_widgets_grouping_spacer_does_not_expand_sibling_group,
        "widgets.grouping.spacer-does-not-expand-sibling-group",
        run_test_widgets_grouping_spacer_does_not_expand_sibling_group
    ),
    (
        test_ui_regression_widgets_empty_center_spacer_expands_side_section,
        "widgets.empty-center-spacer-expands-side-section",
        run_test_widgets_empty_center_spacer_expands_side_section
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
        test_ui_regression_widgets_background_color_precedence_pixel,
        "widgets.background_color_precedence_pixel",
        run_test_widgets_background_color_precedence_pixel
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
        test_ui_regression_per_widget_outline_color_pixel,
        "widgets.outline_color_precedence_pixel",
        run_test_per_widget_outline_color_pixel
    ),
    (
        test_ui_regression_widget_outline_color_css_override_pixel,
        "widgets.outline_color_css_override_pixel",
        run_test_widget_outline_color_css_override_pixel
    ),
    (
        test_ui_regression_surface_outline_css_gsk_parity,
        "theme.surface_outline_css_gsk_parity",
        run_test_surface_outline_css_gsk_parity
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
        test_ui_regression_theme_states_urgent_pixel,
        "theme.states_urgent_pixel",
        run_test_theme_states_urgent_pixel
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
