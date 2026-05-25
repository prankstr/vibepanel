//! Custom widget - user-defined icon/label with optional script polling, streaming, and click actions.
//!
//! Supports multiple instances via the `custom-` naming prefix.
//! Each `custom-<name>` entry becomes its own widget with a unique CSS class.
//!
//! `exec` can run once/on interval, or as a line-buffered stream with `continuous = true`.
//! Output is plain text unless it parses as Waybar-style JSON with at least one supported
//! field: `text`, `tooltip`, `class`, `alt`, or `percentage`. `alt` maps through `icons`,
//! and `glyph:<value>` renders a literal side icon.
//!
//! VibePanel supports the JSON output format, not Waybar's config surface: use `template`
//! and `icons` instead of `format`, `format-icons`, `signal`, or `return-type`.
//!
//! For continuous mode, commands must flush stdout per line. Optional `restart_interval`
//! auto-restarts on exit, with a 5-in-60s crash-loop cap.
//!
//! Example:
//! ```toml
//! [widgets.custom-monitor]
//! exec = "stdbuf -oL my-monitor --json"
//! continuous = true
//! restart_interval = 5
//! template = "{text} {percentage}%"
//!
//! [widgets.custom-monitor.icons]
//! warning = "glyph:!"
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use gtk4::gio;
use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, GestureClick, Image, Label, Orientation, gdk};
use serde::Deserialize;
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService, has_material_mapping};
use crate::services::tooltip::TooltipManager;
use crate::styles::{icon, state, widget as wgt};
use crate::widgets::base::{BaseWidget, VisibilityHandle, describe_exit_status};
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Known config keys for the custom widget.
const KNOWN_OPTIONS: &[&str] = &[
    "icon",
    "icons",
    "image",
    "label",
    "exec",
    "template",
    "interval",
    "continuous",
    "restart_interval",
    "on_click",
    "tooltip",
    "max_chars",
];

/// Default exec timeout in seconds.
const EXEC_TIMEOUT_SECS: u64 = 10;

/// Max automatic restarts within [`RESTART_WINDOW_SECS`] before giving up.
const MAX_RESTARTS: u32 = 5;

/// Time window (seconds) over which [`MAX_RESTARTS`] is enforced.
const RESTART_WINDOW_SECS: u64 = 60;

/// Grace period after SIGTERM before continuous custom scripts are SIGKILLed.
const CONTINUOUS_KILL_GRACE_MS: u64 = 250;

/// Side-icon value for a custom widget.
///
/// Unprefixed values are named icons resolved through VibePanel's icon service.
/// `glyph:<value>` renders a literal glyph/emoji/Nerd Font character in the
/// side-icon slot.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CustomIconValue {
    Named(String),
    Glyph(String),
}

fn parse_custom_icon_value(custom_id: &str, value: &str) -> Option<CustomIconValue> {
    if let Some(glyph) = value.strip_prefix("glyph:") {
        if glyph.is_empty() {
            warn!(
                "Custom widget '{}': empty glyph icon value; ignoring",
                custom_id
            );
            return None;
        }
        return Some(CustomIconValue::Glyph(glyph.to_string()));
    }

    Some(CustomIconValue::Named(value.to_string()))
}

/// Configuration for a custom widget instance.
#[derive(Debug, Clone, Default)]
pub struct CustomConfig {
    /// Side icon value. Unprefixed values are named icons; `glyph:<value>` renders
    /// a literal glyph/emoji/Nerd Font character.
    icon: Option<CustomIconValue>,
    /// Exact `alt` -> side-icon mapping. Each key is matched literally against the
    /// JSON `alt` field; unmatched or missing `alt` falls back to the top-level `icon`.
    icons: BTreeMap<String, CustomIconValue>,
    /// Image file path (PNG, SVG, etc.). Supports absolute paths, `file://` URIs,
    /// and `~/`. Takes precedence over `icon` if both are set.
    pub image: Option<String>,
    /// Static/fallback label text. Supports emoji, Nerd Font glyphs, Unicode.
    pub label: String,
    /// Shell command whose first line of stdout replaces the label text.
    pub exec: Option<String>,
    /// Template for formatting exec output. `{output}` is replaced by the
    /// first line of exec stdout. When absent, exec output replaces the
    /// label wholesale.
    pub template: Option<String>,
    /// Polling interval in seconds. 0 = run once at startup. Only used in poll mode.
    pub interval: u64,
    /// When `true`, `exec` is spawned as a long-running process and each stdout
    /// line updates the label in real-time.
    pub continuous: bool,
    /// Auto-restart delay (seconds) for continuous mode. `None` = no restart.
    pub restart_interval: Option<u64>,
    /// Shell command to execute on left click.
    pub on_click: Option<String>,
    /// Static tooltip text.
    pub tooltip: Option<String>,
    /// Truncate label to N characters with ellipsis.
    pub max_chars: Option<i32>,
}

impl WidgetConfig for CustomConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(&entry.name, entry, KNOWN_OPTIONS);

        let icon = entry
            .options
            .get("icon")
            .and_then(|v| v.as_str())
            .and_then(|value| parse_custom_icon_value(&entry.name, value));

        let icons = entry
            .options
            .get("icons")
            .and_then(toml::Value::as_table)
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(alt, icon)| {
                        icon.as_str().and_then(|value| {
                            if value.is_empty() {
                                return None;
                            }
                            parse_custom_icon_value(&entry.name, value)
                                .map(|value| (alt.clone(), value))
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let image = entry
            .options
            .get("image")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let label = entry
            .options
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let exec = entry
            .options
            .get("exec")
            .and_then(|v| v.as_str())
            .map(String::from);

        let template = entry
            .options
            .get("template")
            .and_then(|v| v.as_str())
            .map(String::from);

        let interval = entry
            .options
            .get("interval")
            .and_then(toml::Value::as_integer)
            .map_or(0, |v| u64::try_from(v.max(0)).unwrap_or(0));

        let continuous = entry
            .options
            .get("continuous")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);

        let restart_interval = entry
            .options
            .get("restart_interval")
            .and_then(toml::Value::as_integer)
            .and_then(|v| u64::try_from(v.max(0)).ok())
            .filter(|&v| v > 0);

        let on_click = entry
            .options
            .get("on_click")
            .and_then(|v| v.as_str())
            .map(String::from);

        let tooltip = entry
            .options
            .get("tooltip")
            .and_then(|v| v.as_str())
            .map(String::from);

        let max_chars = entry
            .options
            .get("max_chars")
            .and_then(toml::Value::as_integer)
            .map(|v| i32::try_from(v.max(1)).unwrap_or(i32::MAX));

        Self {
            icon,
            icons,
            image,
            label,
            exec,
            template,
            interval,
            continuous,
            restart_interval,
            on_click,
            tooltip,
            max_chars,
        }
    }
}

/// State shared between timer and click-handler closures for exec polling.
#[derive(Clone)]
struct ExecState {
    cmd: String,
    custom_id: String,
    display: DisplayContext,
    prev_classes: Rc<RefCell<Vec<String>>>,
}

struct ExecWiring {
    state: Option<ExecState>,
    timer_source: Rc<RefCell<Option<SourceId>>>,
    continuous_stop: Option<Arc<AtomicBool>>,
    continuous_pid: Arc<AtomicU32>,
    continuous_restart_source: Rc<RefCell<Option<SourceId>>>,
}

fn record_restart_attempt(times: &mut Vec<std::time::Instant>, now: std::time::Instant) -> bool {
    if let Some(cutoff) = now.checked_sub(std::time::Duration::from_secs(RESTART_WINDOW_SECS)) {
        times.retain(|t| *t > cutoff);
    }
    if times.len() >= MAX_RESTARTS as usize {
        return false;
    }
    times.push(now);
    true
}

#[derive(Clone)]
struct ContinuousContext {
    display: DisplayContext,
    custom_id: String,
    restart_interval: Option<u64>,
    stop_flag: Arc<AtomicBool>,
    pid_slot: Arc<AtomicU32>,
    restart_times: Rc<RefCell<Vec<std::time::Instant>>>,
    restart_source_slot: Rc<RefCell<Option<SourceId>>>,
}

#[derive(Clone)]
struct DisplayContext {
    label: Label,
    icon_view: Option<CustomIconView>,
    icons: BTreeMap<String, CustomIconValue>,
    static_icon: Option<CustomIconValue>,
    fallback: String,
    static_tooltip: Option<String>,
    template: Option<String>,
    widget: gtk4::Box,
    visibility: VisibilityHandle,
    class_target: gtk4::Box,
    has_static_image: bool,
}

struct DisplayWidgets {
    label: Option<Label>,
    icon_view: Option<CustomIconView>,
    static_icon: Option<CustomIconValue>,
    has_static_image: bool,
}

impl DisplayWidgets {
    fn context(&self, label: &Label, config: &CustomConfig, base: &BaseWidget) -> DisplayContext {
        DisplayContext {
            label: label.clone(),
            icon_view: self.icon_view.clone(),
            icons: config.icons.clone(),
            static_icon: self.static_icon.clone(),
            fallback: config.label.clone(),
            static_tooltip: config.tooltip.clone(),
            template: config.template.clone(),
            widget: base.widget().clone(),
            visibility: base.visibility_handle(),
            class_target: base.surface().clone(),
            has_static_image: self.has_static_image,
        }
    }
}

#[derive(Clone)]
struct CustomIconView {
    content: gtk4::Box,
    named: Rc<RefCell<Option<IconHandle>>>,
    glyph: Rc<RefCell<Option<Label>>>,
}

impl CustomIconView {
    fn new(content: &gtk4::Box) -> Self {
        Self {
            content: content.clone(),
            named: Rc::new(RefCell::new(None)),
            glyph: Rc::new(RefCell::new(None)),
        }
    }

    fn set_icon(&self, icon: Option<&CustomIconValue>) {
        match icon {
            Some(CustomIconValue::Named(name)) => self.show_named(name),
            Some(CustomIconValue::Glyph(glyph)) => self.show_glyph(glyph),
            None => self.hide(),
        }
    }

    fn show_named(&self, name: &str) {
        self.hide_glyph();
        if self.named.borrow().is_none() {
            let handle = IconsService::global().create_icon(name, &[]);
            let widget = handle.widget();
            widget.set_halign(gtk4::Align::Center);
            widget.set_hexpand(true);
            widget.set_visible(false);
            self.content.prepend(&widget);
            *self.named.borrow_mut() = Some(handle);
        }

        if let Some(handle) = self.named.borrow().as_ref() {
            handle.set_icon(name);
            handle.widget().set_visible(true);
        }
    }

    fn show_glyph(&self, glyph: &str) {
        self.hide_named();
        if self.glyph.borrow().is_none() {
            let label = Label::new(None);
            label.add_css_class(icon::ROOT);
            label.add_css_class(wgt::CUSTOM_ICON_GLYPH);
            label.set_halign(gtk4::Align::Center);
            label.set_hexpand(true);
            label.set_xalign(0.5);
            label.set_visible(false);
            self.content.prepend(&label);
            *self.glyph.borrow_mut() = Some(label);
        }

        if let Some(label) = self.glyph.borrow().as_ref() {
            label.set_label(glyph);
            label.set_visible(true);
        }
    }

    fn hide(&self) {
        self.hide_named();
        self.hide_glyph();
    }

    fn hide_named(&self) {
        if let Some(handle) = self.named.borrow().as_ref() {
            handle.widget().set_visible(false);
        }
    }

    fn hide_glyph(&self) {
        if let Some(label) = self.glyph.borrow().as_ref() {
            label.set_visible(false);
        }
    }
}

#[derive(Debug, PartialEq)]
struct OutputPlan {
    label: String,
    tooltip: Option<String>,
    icon: Option<CustomIconValue>,
    classes: Vec<String>,
    should_show: bool,
}

// --- Structured JSON output (Waybar-compatible subset) ---

/// A string or list of strings (for CSS `class` deserialization).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(s) => vec![s],
            Self::Multiple(v) => v,
        }
    }
}

/// Structured output from an exec command (auto-detected JSON).
#[derive(Debug, Default, Deserialize)]
struct ExecOutput {
    text: Option<String>,
    tooltip: Option<String>,
    class: Option<StringOrVec>,
    alt: Option<String>,
    percentage: Option<f64>,
}

impl ExecOutput {
    fn has_supported_field(&self) -> bool {
        self.text.is_some()
            || self.tooltip.is_some()
            || self.class.is_some()
            || self.alt.is_some()
            || self.percentage.is_some()
    }
}

/// Try to parse a line as structured JSON. Returns `Some` only if the line
/// is valid JSON **and** contains at least one supported structured field.
fn try_parse_json(line: &str) -> Option<ExecOutput> {
    let out: ExecOutput = serde_json::from_str(line).ok()?;
    out.has_supported_field().then_some(out)
}

/// Expand template placeholders: `{output}`/`{text}`, `{alt}`, `{percentage}`.
fn expand_template(tmpl: &str, text: &str, out: Option<&ExecOutput>) -> String {
    let mut s = tmpl.replace("{output}", text).replace("{text}", text);
    let alt = out.and_then(|o| o.alt.as_deref()).unwrap_or("");
    let percentage = out
        .and_then(|o| o.percentage)
        .map(|pct| format!("{pct:.0}"))
        .unwrap_or_default();
    s = s.replace("{alt}", alt);
    s = s.replace("{percentage}", &percentage);
    s
}

fn plan_output(
    raw: &str,
    fallback: &str,
    tmpl: Option<&str>,
    static_tooltip: Option<&str>,
    icons: &BTreeMap<String, CustomIconValue>,
    static_icon: Option<&CustomIconValue>,
    has_static_image: bool,
) -> OutputPlan {
    let line = raw.trim();
    let parsed = try_parse_json(line);
    let json_present = parsed.is_some();
    let text = match parsed.as_ref() {
        Some(o) => o.text.as_deref().unwrap_or(""),
        None => line,
    };

    let display = if !json_present && line.is_empty() {
        String::new()
    } else {
        match tmpl {
            Some(t) => expand_template(t, text, parsed.as_ref()),
            None => text.to_string(),
        }
    };

    let tooltip = match parsed.as_ref().and_then(|o| o.tooltip.as_deref()) {
        Some("") => None,
        Some(tip) => Some(tip.to_string()),
        None => static_tooltip.map(String::from),
    };

    let icon = resolve_icon(
        parsed.as_ref().and_then(|o| o.alt.as_deref()),
        icons,
        static_icon,
    );

    let classes = parsed
        .as_ref()
        .and_then(|o| o.class.clone())
        .map(StringOrVec::into_vec)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !c.is_empty())
        .collect();

    let should_show = if parsed.is_some() {
        !display.is_empty() || icon.is_some() || has_static_image
    } else {
        !line.is_empty() || !fallback.is_empty() || has_static_image
    };

    OutputPlan {
        label: if display.is_empty() && !json_present {
            fallback.to_string()
        } else {
            display
        },
        tooltip,
        icon,
        classes,
        should_show,
    }
}

fn resolve_icon(
    alt: Option<&str>,
    icons: &BTreeMap<String, CustomIconValue>,
    static_icon: Option<&CustomIconValue>,
) -> Option<CustomIconValue> {
    if let Some(icon) = alt
        .filter(|value| !value.is_empty())
        .and_then(|value| icons.get(value))
    {
        return Some(icon.clone());
    }

    static_icon.cloned()
}

/// Apply a single line of output to the widget. Handles both plain text and
/// auto-detected JSON structured output (tooltip, CSS classes).
///
/// State semantics: each output line fully defines dynamic state.
/// - Plain text or JSON without `class` clears all previously applied CSS classes.
/// - JSON without `tooltip` falls back to the static `tooltip` config key, or clears the tooltip.
fn apply_output(raw: &str, display: &DisplayContext, prev_classes: &mut Vec<String>) {
    let plan = plan_output(
        raw,
        &display.fallback,
        display.template.as_deref(),
        display.static_tooltip.as_deref(),
        &display.icons,
        display.static_icon.as_ref(),
        display.has_static_image,
    );
    display.label.set_label(&plan.label);
    display.label.set_visible(!plan.label.is_empty());

    display.visibility.set_content_visible(plan.should_show);

    // Tooltip: each update fully defines the tooltip state.
    // JSON tooltip overrides, plain text falls back to static config or clears it.
    if let Some(ref tip) = plan.tooltip {
        TooltipManager::global().set_styled_tooltip(&display.widget, tip);
    } else {
        TooltipManager::global().clear_tooltip(&display.widget);
    }

    // Waybar-style dynamic icons: JSON `alt` selects from config `icons`.
    // Plain text or unmatched alt falls back to the static `icon`.
    if let Some(view) = &display.icon_view {
        view.set_icon(plan.icon.as_ref());
    }

    // CSS class rotation: always remove previous classes on every update.
    // Each output line fully defines dynamic state — plain text clears all classes.
    for c in prev_classes.drain(..) {
        display.class_target.remove_css_class(&c);
    }
    for c in plan.classes {
        // Guard against removing static/pre-existing classes: GTK CSS
        // classes are not reference-counted, so blindly tracking and
        // removing a class that was already present would strip it.
        if !display.class_target.has_css_class(&c) {
            display.class_target.add_css_class(&c);
            prev_classes.push(c);
        }
    }
}

/// Resolve `file://` URIs and `~/` paths to absolute paths.
fn resolve_image_path(value: &str) -> String {
    if let Some(path) = value.strip_prefix("file://") {
        path.to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => {
                warn!("$HOME is not set, cannot expand '~/' in image path");
                value.to_string()
            }
        }
    } else {
        value.to_string()
    }
}

fn warn_invalid_custom_icon_value(custom_id: &str, icon_value: &CustomIconValue) {
    let CustomIconValue::Named(icon_name) = icon_value else {
        return;
    };

    if icon_name.starts_with('/') || icon_name.starts_with("~/") || icon_name.starts_with("file://")
    {
        warn!(
            "Custom widget '{}': icon '{}' looks like a file path — \
             use the 'image' field for file-based images instead of 'icon'.",
            custom_id, icon_name
        );
    } else if IconsService::global().uses_material() && !has_material_mapping(icon_name) {
        warn!(
            "Custom widget '{}': icon '{}' has no Material Symbol mapping. \
             Use theme.icons.theme = \"gtk\" or prefix a literal glyph with 'glyph:'.",
            custom_id, icon_name
        );
    }
}

fn build_display_widgets(
    custom_id: &str,
    config: &CustomConfig,
    base: &BaseWidget,
) -> DisplayWidgets {
    let static_icon = config.icon.clone();
    let has_static_image = config.image.is_some();

    let icon_view = if let Some(ref image_path) = config.image {
        if config.icon.is_some() {
            warn!(
                "Custom widget '{}': both 'image' and 'icon' are set; using 'image'",
                custom_id
            );
        }
        if !config.icons.is_empty() {
            warn!(
                "Custom widget '{}': both 'image' and 'icons' are set; dynamic icon mapping \
                 is ignored because 'image' takes precedence",
                custom_id
            );
        }

        let resolved = resolve_image_path(image_path);

        if !resolved.starts_with('/') {
            warn!(
                "Custom widget '{}': image path '{}' is not absolute — \
                 use an absolute path, ~/path, or file:// URI",
                custom_id, image_path
            );
        } else if !std::path::Path::new(&resolved).exists() {
            warn!(
                "Custom widget '{}': image file '{}' does not exist",
                custom_id, resolved
            );
        }

        let image = Image::from_file(&resolved);
        let icon_size = ConfigManager::global().theme_sizes().pixmap_icon_size as i32;
        image.set_pixel_size(icon_size);

        let icon_root = GtkBox::new(Orientation::Horizontal, 0);
        icon_root.add_css_class(icon::ROOT);
        icon_root.set_halign(gtk4::Align::Center);
        icon_root.set_hexpand(true);
        image.set_halign(gtk4::Align::Center);
        icon_root.append(&image);
        base.content().append(&icon_root);

        None
    } else if config.icon.is_some() || !config.icons.is_empty() {
        if let Some(ref icon) = config.icon {
            warn_invalid_custom_icon_value(custom_id, icon);
        }
        for icon in config.icons.values() {
            warn_invalid_custom_icon_value(custom_id, icon);
        }

        let view = CustomIconView::new(base.content());
        view.set_icon(config.icon.as_ref());
        Some(view)
    } else {
        None
    };

    let label = if !config.label.is_empty() || config.exec.is_some() {
        let initial_text = if config.label.is_empty() {
            None
        } else {
            Some(config.label.as_str())
        };
        let lbl = base.add_label(initial_text, &[]);

        if let Some(max_chars) = config.max_chars {
            lbl.set_max_width_chars(max_chars);
            lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        }

        Some(lbl)
    } else {
        None
    };

    if config.exec.is_some()
        && config.label.is_empty()
        && config.icon.is_none()
        && config.image.is_none()
    {
        base.visibility_handle().set_content_visible(false);
    }

    DisplayWidgets {
        label,
        icon_view,
        static_icon,
        has_static_image,
    }
}

fn wire_exec(
    custom_id: &str,
    config: &CustomConfig,
    base: &BaseWidget,
    display_widgets: &DisplayWidgets,
) -> ExecWiring {
    let timer_source = Rc::new(RefCell::new(None));
    let continuous_pid = Arc::new(AtomicU32::new(0));
    let continuous_restart_source = Rc::new(RefCell::new(None));

    if config.continuous {
        let continuous_stop = config
            .exec
            .as_ref()
            .map(|_| Arc::new(AtomicBool::new(false)));

        if let (Some(exec_cmd), Some(lbl), Some(stop)) = (
            config.exec.clone(),
            display_widgets.label.clone(),
            &continuous_stop,
        ) {
            let display = display_widgets.context(&lbl, config, base);
            start_continuous(
                &exec_cmd,
                ContinuousContext {
                    display,
                    custom_id: custom_id.to_string(),
                    restart_interval: config.restart_interval,
                    stop_flag: stop.clone(),
                    pid_slot: continuous_pid.clone(),
                    restart_times: Rc::new(RefCell::new(Vec::<std::time::Instant>::new())),
                    restart_source_slot: continuous_restart_source.clone(),
                },
            );
        }

        return ExecWiring {
            state: None,
            timer_source,
            continuous_stop,
            continuous_pid,
            continuous_restart_source,
        };
    }

    let continuous_stop = None;
    let state = config.exec.clone().map(|exec_cmd| {
        let display = display_widgets.context(
            display_widgets
                .label
                .as_ref()
                .expect("exec widgets have a label"),
            config,
            base,
        );
        let state = ExecState {
            cmd: exec_cmd,
            custom_id: custom_id.to_string(),
            display,
            prev_classes: Rc::new(RefCell::new(Vec::new())),
        };

        // Run the exec command once immediately.
        run_exec(&state);

        if config.interval > 0 {
            let state_for_timer = state.clone();

            let source_id = glib::timeout_add_seconds_local(
                u32::try_from(config.interval).unwrap_or(u32::MAX),
                move || {
                    run_exec(&state_for_timer);
                    glib::ControlFlow::Continue
                },
            );

            *timer_source.borrow_mut() = Some(source_id);
        }

        state
    });

    ExecWiring {
        state,
        timer_source,
        continuous_stop,
        continuous_pid,
        continuous_restart_source,
    }
}

/// Custom widget that displays a user-configured icon/label with optional
/// script polling and click handlers.
pub struct CustomWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Kept alive so custom icon handles remain registered for theme changes
    /// even when no exec closure also owns a clone.
    _icon_view: Option<CustomIconView>,
    /// Active timer source ID for cancellation on drop (poll mode).
    timer_source: Rc<RefCell<Option<SourceId>>>,
    /// Stop flag for the continuous-mode background reader thread.
    continuous_stop: Option<Arc<AtomicBool>>,
    /// PID of the continuous-mode child process (for SIGTERM on drop).
    continuous_pid: Arc<AtomicU32>,
    /// Pending continuous-mode restart timer source ID.
    continuous_restart_source: Rc<RefCell<Option<SourceId>>>,
}

impl CustomWidget {
    /// Create a new custom widget.
    ///
    /// `custom_id` is the part after "custom-" (e.g., "power" from "custom-power").
    /// It's used as the primary CSS class for the widget.
    pub fn new(custom_id: &str, config: CustomConfig) -> Self {
        if config.interval > 0 && config.exec.is_none() {
            warn!(
                "Custom widget '{}': interval is set but no exec command configured",
                custom_id
            );
        }
        if config.continuous && config.exec.is_none() {
            warn!(
                "Custom widget '{}': continuous is set but no exec command configured",
                custom_id
            );
        }
        if config.continuous && config.interval > 0 {
            warn!(
                "Custom widget '{}': interval is ignored in continuous mode",
                custom_id
            );
        }

        let css_class_name = format!("{}{custom_id}", wgt::CUSTOM_PREFIX);

        // Custom output visibility AND-composes with show_if on the outer wrapper;
        // both must be true for the widget to be visible.
        let base = BaseWidget::new(&[&css_class_name]);

        // Enables hover styling for left-click action.
        // BaseWidget handles CLICKABLE for on_click_right / on_click_middle.
        if config.on_click.is_some() {
            base.widget().add_css_class(state::CLICKABLE);
        }

        if let Some(ref tip) = config.tooltip {
            base.set_tooltip(tip);
        }

        let display_widgets = build_display_widgets(custom_id, &config, &base);
        let exec_wiring = wire_exec(custom_id, &config, &base, &display_widgets);

        if let Some(on_click) = config.on_click {
            // Custom widget's own left-click handler. Runs the command, then
            // re-runs exec to refresh the label immediately (poll mode only —
            // continuous mode updates via the stream, not exec re-run).
            let click = GestureClick::new();
            let click_widget_name = css_class_name.clone();
            let is_continuous = config.continuous;
            click.set_button(gdk::BUTTON_PRIMARY);
            click.connect_released(move |_gesture, _n_press, _x, _y| {
                let cmd = on_click.to_string();
                let exec_state = exec_wiring.state.clone();
                let widget_name = click_widget_name.clone();
                debug!("Custom widget executing: sh -c {}", cmd);

                glib::spawn_future_local(async move {
                    let _ = gio::spawn_blocking(move || {
                        use std::process::{Command, Stdio};
                        match Command::new("sh")
                            .args(["-c", &cmd])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                        {
                            Ok(mut child) => match child.wait() {
                                Ok(status) if !status.success() => {
                                    warn!(
                                        "'{}' click command '{}' failed: {}",
                                        widget_name,
                                        cmd,
                                        describe_exit_status(status)
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        "'{}' click command '{}' wait failed: {}",
                                        widget_name, cmd, e
                                    );
                                }
                                _ => {}
                            },
                            Err(e) => {
                                warn!(
                                    "'{}' failed to spawn click command '{}': {}",
                                    widget_name, cmd, e
                                );
                            }
                        }
                    })
                    .await;

                    // Re-run exec after click — poll mode only.
                    // Continuous mode gets updates from the stream.
                    if !is_continuous && let Some(ref state) = exec_state {
                        run_exec(state);
                    }
                });
            });
            base.widget().add_controller(click);
        }

        Self {
            base,
            _icon_view: display_widgets.icon_view,
            timer_source: exec_wiring.timer_source,
            continuous_stop: exec_wiring.continuous_stop,
            continuous_pid: exec_wiring.continuous_pid,
            continuous_restart_source: exec_wiring.continuous_restart_source,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for CustomWidget {
    fn drop(&mut self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
        // Stop continuous-mode process group (shell + descendants).
        if let Some(ref flag) = self.continuous_stop {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(source_id) = self.continuous_restart_source.borrow_mut().take() {
            source_id.remove();
        }
        let pid = self.continuous_pid.load(Ordering::Relaxed);
        if pid != 0 {
            // Negative PID sends SIGTERM to the entire process group,
            // ensuring pipelines and child processes are reaped.
            let pgid = pid as libc::pid_t;
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            // Escalate after a short grace period for scripts that ignore SIGTERM.
            // Guard against PID-reuse: the reader thread zeroes the atomic immediately
            // after child.wait() returns, so a non-zero pid_slot means the process
            // hasn't been reaped yet and the PID is still valid.
            let pid_slot = self.continuous_pid.clone();
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(CONTINUOUS_KILL_GRACE_MS),
                move || {
                    if pid_slot.load(Ordering::Relaxed) == pid {
                        // SAFETY: Killing a process group. PID is still tracked,
                        // process hasn't been reaped by the reader thread yet.
                        unsafe {
                            libc::kill(-pgid, libc::SIGKILL);
                        }
                    }
                },
            );
            debug!(
                "Custom widget continuous process group stopping (pgid {})",
                pid
            );
        }
    }
}

/// Run an exec command asynchronously and update the widget with its output.
///
/// Uses `glib::spawn_future_local` + `gio::spawn_blocking` to avoid blocking
/// the GTK event loop. The command is run via `sh -c` with a 10-second timeout.
///
/// Output is processed through [`apply_output`], so JSON lines with any
/// supported field (`text`, `tooltip`, `class`, `alt`, `percentage`) are
/// auto-detected and applied. Plain text lines work as before.
fn run_exec(state: &ExecState) {
    let display = state.display.clone();
    let custom_id = state.custom_id.clone();
    let exec_cmd = state.cmd.clone();
    let prev_classes = Rc::clone(&state.prev_classes);

    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            use std::os::unix::process::{CommandExt, ExitStatusExt};
            use std::process::{Command, Stdio};
            use std::time::Duration;

            let child = match Command::new("sh")
                .args(["-c", &exec_cmd])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
            {
                Ok(child) => child,
                Err(e) => return Err(format!("Failed to spawn: {e}")),
            };

            // Spawn a watchdog thread that kills the child after the timeout.
            // Uses an mpsc channel so the main thread can cancel the watchdog
            // early when the command finishes, avoiding a leaked sleeping thread.
            // We extract the PID here because wait_with_output() consumes the
            // Child, so we can't call child.kill() from the watchdog thread.
            let pid = child.id() as libc::pid_t;
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                if rx
                    .recv_timeout(Duration::from_secs(EXEC_TIMEOUT_SECS))
                    .is_err()
                {
                    // Timeout expired and sender didn't signal — kill the child
                    // process group so descendants cannot keep stdout open.
                    // SAFETY: Sending SIGKILL to a process group. If it already
                    // exited and was reaped, kill() returns ESRCH which is harmless.
                    // Theoretical PID reuse: if the child exits and its PID is
                    // recycled before the timeout fires, we'd kill an unrelated
                    // process. In practice this can't happen here because
                    // wait_with_output() below is the only call that reaps the
                    // child — if it completes before the timeout, tx.send(())
                    // cancels the watchdog. If it hasn't completed, the child is
                    // still alive and owns the PID.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
            });

            let output = child
                .wait_with_output()
                .map_err(|e| format!("Wait failed: {e}"))?;

            // Cancel the watchdog (no-op if it already fired).
            let _ = tx.send(());

            if !output.status.success() && output.status.signal() == Some(libc::SIGKILL) {
                return Err(format!("Command timed out after {EXEC_TIMEOUT_SECS}s"));
            }

            if !output.status.success() {
                return Err(describe_exit_status(output.status));
            }

            // Extract only the first line from the captured output.
            use std::io::BufRead;
            let mut line = String::new();
            let mut reader = std::io::BufReader::new(&output.stdout[..]);
            let _ = reader.read_line(&mut line);
            Ok(line)
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                apply_output(&output, &display, &mut prev_classes.borrow_mut());
            }
            Ok(Err(err)) => {
                warn!("'custom-{}' exec failed: {}", custom_id, err);
                // Keep previous label text and visibility on error
            }
            Err(err) => {
                warn!("'custom-{}' exec task failed: {:?}", custom_id, err);
                // Keep previous label text and visibility on error
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Continuous (streaming) mode
// ---------------------------------------------------------------------------

/// Schedule a bounded restart after process exit or spawn failure.
/// Enforces crash-loop cap ([`MAX_RESTARTS`] per [`RESTART_WINDOW_SECS`]).
fn schedule_restart(ctx: &ContinuousContext, cmd: &str, reason: &str) {
    if ctx.stop_flag.load(Ordering::Relaxed) {
        return;
    }

    let delay = match ctx.restart_interval {
        Some(d) => d,
        None => return,
    };
    let mut times = ctx.restart_times.borrow_mut();
    if !record_restart_attempt(&mut times, std::time::Instant::now()) {
        warn!(
            "'custom-{}' continuous: restart cap reached ({} in {}s) — {}",
            ctx.custom_id, MAX_RESTARTS, RESTART_WINDOW_SECS, reason
        );
        return;
    }
    drop(times);

    let restart_ctx = ctx.clone();
    let cmd = cmd.to_string();
    let reason = reason.to_string();
    let source_id =
        glib::timeout_add_seconds_local_once(u32::try_from(delay).unwrap_or(u32::MAX), move || {
            *restart_ctx.restart_source_slot.borrow_mut() = None;
            if restart_ctx.stop_flag.load(Ordering::Relaxed) {
                return;
            }
            debug!(
                "'custom-{}' continuous: restarting ({})",
                restart_ctx.custom_id, reason
            );
            start_continuous(&cmd, restart_ctx);
        });
    *ctx.restart_source_slot.borrow_mut() = Some(source_id);
}

/// Spawn a continuous-mode exec process. Background thread reads stdout lines
/// and posts them to the main thread via mpsc + gio::spawn_blocking.
fn start_continuous(cmd: &str, ctx: ContinuousContext) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let child = match Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "'custom-{}' continuous: failed to spawn: {}",
                ctx.custom_id, e
            );
            schedule_restart(&ctx, cmd, "after spawn failure");
            return;
        }
    };

    let pid = child.id();
    ctx.pid_slot.store(pid, Ordering::Relaxed);
    debug!(
        "'custom-{}' continuous started (pid {})",
        ctx.custom_id, pid
    );

    let (tx, rx) = async_channel::bounded::<Option<String>>(64);
    let thread_stop = ctx.stop_flag.clone();
    let thread_id = ctx.custom_id.clone();

    // Background reader thread: stdout via BufRead, stderr logged.
    let reader_pid_slot = ctx.pid_slot.clone();
    std::thread::spawn(move || {
        continuous_reader(child, thread_stop, thread_id, tx, reader_pid_slot);
    });

    let stop = ctx.stop_flag.clone();
    let cmd = cmd.to_string();

    glib::spawn_future_local(async move {
        let mut prev_classes = Vec::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let msg = rx.recv().await.ok().flatten();
            if stop.load(Ordering::Relaxed) {
                break;
            }

            match msg {
                Some(line) => {
                    apply_output(&line, &ctx.display, &mut prev_classes);
                }
                _ => {
                    // Channel closed — process exited.
                    // Clear CSS classes from the old process so the next
                    // incarnation starts with a clean slate.
                    for c in prev_classes.drain(..) {
                        ctx.display.class_target.remove_css_class(&c);
                    }
                    schedule_restart(&ctx, &cmd, "after exit");
                    break;
                }
            }
        }
    });
}

/// Background reader thread: reads stdout lines, logs stderr, sends `Some(line)`
/// per stdout line and `None` on exit through the channel.
fn continuous_reader(
    mut child: std::process::Child,
    stop: Arc<AtomicBool>,
    id: String,
    tx: async_channel::Sender<Option<String>>,
    pid_slot: Arc<AtomicU32>,
) {
    use std::io::BufRead;

    // Stderr in a separate thread to avoid deadlock.
    let stderr = child.stderr.take();
    let tx2 = tx.clone();
    let stop2 = stop.clone();
    let id_for_stderr = id.clone();
    let stderr_handle = stderr.map(|se| {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(se).lines().map_while(Result::ok) {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                if !line.is_empty() {
                    debug!("'custom-{}' stderr: {}", id_for_stderr, line);
                }
            }
            drop(tx2); // ensure channel closes if stderr thread outlives stdout
        })
    });

    if let Some(stdout) = child.stdout.take() {
        for line in std::io::BufReader::new(stdout).lines() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match line {
                Ok(l) => {
                    if tx.send_blocking(Some(l)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    let status = child.wait();
    // Zero the PID slot immediately after wait returns so the Drop-delayed
    // SIGKILL guard never fires against a freed/reused PID.
    pid_slot.store(0, Ordering::Relaxed);

    match status {
        Ok(s) if s.success() => {
            debug!("'custom-{}' continuous process exited cleanly", id);
        }
        Ok(s) => {
            warn!(
                "'custom-{}' continuous process exited: {}",
                id,
                describe_exit_status(s)
            );
        }
        Err(e) => {
            warn!("'custom-{}' continuous process wait failed: {}", id, e);
        }
    }
    let _ = tx.send_blocking(None); // signal exit before joining stderr
    // Drop stderr reader without joining; if a background descendant
    // inherits stderr, the join could block and prevent restart signaling.
    drop(stderr_handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use toml::Value;

    fn make_entry(options: HashMap<String, Value>) -> WidgetEntry {
        WidgetEntry {
            name: "custom-test".to_string(),
            options,
        }
    }

    #[test]
    fn test_record_restart_attempt_enforces_cap_within_window() {
        let now = std::time::Instant::now();
        let mut times = Vec::new();

        for _ in 0..MAX_RESTARTS {
            assert!(record_restart_attempt(&mut times, now));
        }
        assert!(!record_restart_attempt(&mut times, now));
    }

    #[test]
    fn test_record_restart_attempt_prunes_old_attempts() {
        let now = std::time::Instant::now();
        let old = now - std::time::Duration::from_secs(RESTART_WINDOW_SECS + 1);
        let mut times = vec![old; MAX_RESTARTS as usize];

        assert!(record_restart_attempt(&mut times, now));
        assert_eq!(times, vec![now]);
    }

    #[test]
    fn test_record_restart_attempt_handles_early_instant_without_underflow() {
        let now = std::time::Instant::now();
        let early = now
            .checked_sub(std::time::Duration::from_secs(RESTART_WINDOW_SECS + 1))
            .unwrap_or(now);
        let mut times = Vec::new();

        assert!(record_restart_attempt(&mut times, early));
        assert_eq!(times, vec![early]);
    }

    #[test]
    fn test_custom_config_defaults() {
        let entry = make_entry(HashMap::new());
        let config = CustomConfig::from_entry(&entry);

        assert!(config.icon.is_none());
        assert!(config.image.is_none());
        assert!(config.icons.is_empty());
        assert_eq!(config.label, "");
        assert!(config.exec.is_none());
        assert!(config.template.is_none());
        assert_eq!(config.interval, 0);
        assert!(config.on_click.is_none());
        assert!(config.tooltip.is_none());
        assert!(config.max_chars.is_none());
    }

    fn named_icon(name: &str) -> CustomIconValue {
        CustomIconValue::Named(name.to_string())
    }

    fn glyph_icon(glyph: &str) -> CustomIconValue {
        CustomIconValue::Glyph(glyph.to_string())
    }

    #[test]
    fn test_custom_config_full() {
        let mut options = HashMap::new();
        options.insert(
            "icon".to_string(),
            Value::String("system-shutdown-symbolic".to_string()),
        );
        options.insert(
            "icons".to_string(),
            Value::Table(toml::map::Map::from_iter([
                (
                    "dnd".to_string(),
                    Value::String("notifications-disabled".to_string()),
                ),
                (
                    "enabled".to_string(),
                    Value::String("notifications".to_string()),
                ),
            ])),
        );
        options.insert("label".to_string(), Value::String("Power".to_string()));
        options.insert("exec".to_string(), Value::String("echo hello".to_string()));
        options.insert(
            "template".to_string(),
            Value::String(" {output}".to_string()),
        );
        options.insert("interval".to_string(), Value::Integer(600));
        options.insert("on_click".to_string(), Value::String("wlogout".to_string()));
        options.insert(
            "tooltip".to_string(),
            Value::String("Power menu".to_string()),
        );
        options.insert("max_chars".to_string(), Value::Integer(30));

        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);

        assert_eq!(config.icon, Some(named_icon("system-shutdown-symbolic")));
        assert_eq!(
            config.icons.get("dnd"),
            Some(&named_icon("notifications-disabled"))
        );
        assert_eq!(
            config.icons.get("enabled"),
            Some(&named_icon("notifications"))
        );
        assert_eq!(config.label, "Power");
        assert_eq!(config.exec, Some("echo hello".to_string()));
        assert_eq!(config.template, Some(" {output}".to_string()));
        assert_eq!(config.interval, 600);
        assert_eq!(config.on_click, Some("wlogout".to_string()));
        assert_eq!(config.tooltip, Some("Power menu".to_string()));
        assert_eq!(config.max_chars, Some(30));
    }

    #[test]
    fn test_custom_config_icon_glyph_parsed() {
        let mut options = HashMap::new();
        options.insert("icon".to_string(), Value::String("glyph:🔋".to_string()));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert_eq!(config.icon, Some(glyph_icon("🔋")));
    }

    #[test]
    fn test_custom_config_icon_unknown_prefix_stays_named() {
        let mut options = HashMap::new();
        options.insert("icon".to_string(), Value::String("foo:bar".to_string()));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert_eq!(config.icon, Some(named_icon("foo:bar")));
    }

    #[test]
    fn test_custom_config_empty_prefixed_icon_ignored() {
        let mut options = HashMap::new();
        options.insert("icon".to_string(), Value::String("glyph:".to_string()));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert!(config.icon.is_none());
    }

    #[test]
    fn test_custom_config_icons_glyph_value_parsed() {
        let mut icons = toml::map::Map::new();
        icons.insert("unknown".to_string(), Value::String("glyph:?".to_string()));
        icons.insert("dnd".to_string(), Value::String("disabled".to_string()));

        let mut options = HashMap::new();
        options.insert("icons".to_string(), Value::Table(icons));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);

        assert_eq!(config.icons.get("unknown"), Some(&glyph_icon("?")));
        assert_eq!(config.icons.get("dnd"), Some(&named_icon("disabled")));
    }

    #[test]
    fn test_resolve_image_path_absolute() {
        assert_eq!(
            resolve_image_path("/usr/share/icon.png"),
            "/usr/share/icon.png"
        );
    }

    #[test]
    fn test_resolve_image_path_file_uri() {
        assert_eq!(
            resolve_image_path("file:///usr/share/icon.png"),
            "/usr/share/icon.png"
        );
    }

    #[test]
    fn test_resolve_image_path_home_relative() {
        let resolved = resolve_image_path("~/icon.png");
        match std::env::var("HOME") {
            Ok(home) => assert_eq!(resolved, format!("{home}/icon.png")),
            Err(_) => assert_eq!(resolved, "~/icon.png"),
        }
    }

    // --- continuous mode config tests ---

    #[test]
    fn test_custom_config_continuous_true() {
        let mut options = HashMap::new();
        options.insert("continuous".to_string(), Value::Boolean(true));
        options.insert("restart_interval".to_string(), Value::Integer(5));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert!(config.continuous);
        assert_eq!(config.restart_interval, Some(5));
    }

    #[test]
    fn test_custom_config_negative_restart_interval_is_none() {
        let mut options = HashMap::new();
        options.insert("restart_interval".to_string(), Value::Integer(-1));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert!(config.restart_interval.is_none());
    }

    // --- ExecOutput / JSON deserialization tests ---

    #[test]
    fn test_try_parse_json_plain_text() {
        assert!(try_parse_json("hello world").is_none());
    }

    #[test]
    fn test_try_parse_json_unsupported_json_object() {
        assert!(try_parse_json(r#"{"status":"ok"}"#).is_none());
    }

    #[test]
    fn test_try_parse_json_supported_fields_without_text() {
        assert_eq!(
            try_parse_json(r#"{"tooltip":"hi"}"#)
                .unwrap()
                .tooltip
                .as_deref(),
            Some("hi")
        );
        assert_eq!(
            try_parse_json(r#"{"alt":"dark"}"#).unwrap().alt.as_deref(),
            Some("dark")
        );
        assert!(
            try_parse_json(r#"{"class":"active"}"#)
                .unwrap()
                .class
                .is_some()
        );
        assert_eq!(
            try_parse_json(r#"{"percentage":80}"#).unwrap().percentage,
            Some(80.0)
        );
    }

    #[test]
    fn test_try_parse_json_full() {
        let json = r#"{"text":"cpu 42%","tooltip":"details","class":"warning","alt":"high","percentage":42.5}"#;
        let out = try_parse_json(json).unwrap();
        assert_eq!(out.text.as_deref(), Some("cpu 42%"));
        assert_eq!(out.tooltip.as_deref(), Some("details"));
        assert_eq!(out.alt.as_deref(), Some("high"));
        assert!((out.percentage.unwrap() - 42.5).abs() < f64::EPSILON);
    }

    fn icon_map(entries: &[(&str, &str)]) -> BTreeMap<String, CustomIconValue> {
        entries
            .iter()
            .map(|(alt, icon)| ((*alt).to_string(), named_icon(icon)))
            .collect()
    }

    #[test]
    fn test_resolve_icon_exact_match_and_static_fallback() {
        let icons = icon_map(&[("dnd", "notifications-disabled")]);
        let static_icon = named_icon("static");
        assert_eq!(
            resolve_icon(Some("dnd"), &icons, Some(&static_icon)),
            Some(named_icon("notifications-disabled"))
        );
        assert_eq!(
            resolve_icon(Some("unknown"), &icons, Some(&static_icon)),
            Some(named_icon("static"))
        );
        assert_eq!(
            resolve_icon(None, &icons, Some(&static_icon)),
            Some(named_icon("static"))
        );
        assert_eq!(
            resolve_icon(Some(""), &icons, Some(&static_icon)),
            Some(named_icon("static"))
        );
        assert_eq!(
            resolve_icon(Some("unknown"), &BTreeMap::new(), Some(&static_icon)),
            Some(named_icon("static"))
        );
    }

    // --- StringOrVec tests ---

    #[test]
    fn test_string_or_vec_single() {
        let s: StringOrVec = serde_json::from_str(r#""hello""#).unwrap();
        assert_eq!(s, StringOrVec::Single("hello".to_string()));
        assert_eq!(s.into_vec(), vec!["hello"]);
    }

    #[test]
    fn test_string_or_vec_multiple() {
        let s: StringOrVec = serde_json::from_str(r#"["a","b","c"]"#).unwrap();
        assert_eq!(
            s,
            StringOrVec::Multiple(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(s.into_vec(), vec!["a", "b", "c"]);
    }

    // --- expand_template tests ---

    #[test]
    fn test_expand_template_with_exec_output() {
        let out = ExecOutput {
            text: Some("val".into()),
            alt: Some("high".into()),
            percentage: Some(75.0),
            ..Default::default()
        };
        let result = expand_template("{text} ({alt}) {percentage}%", "val", Some(&out));
        assert_eq!(result, "val (high) 75%");
    }

    #[test]
    fn test_expand_template_missing_json_fields_clear_placeholders() {
        let out = ExecOutput {
            text: Some("val".into()),
            ..Default::default()
        };

        let result = expand_template("{text} ({alt}) {percentage}%", "val", Some(&out));
        assert_eq!(result, "val () %");
    }

    #[test]
    fn test_plan_output_plain_text_clears_dynamic_state() {
        let plan = plan_output(
            "hello",
            "fallback",
            Some("[{output}]"),
            None,
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(
            plan,
            OutputPlan {
                label: "[hello]".to_string(),
                tooltip: None,
                icon: None,
                classes: Vec::new(),
                should_show: true,
            }
        );
    }

    #[test]
    fn test_plan_output_json_class_and_tooltip() {
        let icons = icon_map(&[("high", "cpu-high")]);
        let static_icon = named_icon("static-icon");
        let plan = plan_output(
            r#"{"text":"cpu 42%","tooltip":"details","class":["warning",""],"alt":"high","icon":"ignored"}"#,
            "fallback",
            Some("{text}"),
            Some("static"),
            &icons,
            Some(&static_icon),
            false,
        );

        assert_eq!(
            plan,
            OutputPlan {
                label: "cpu 42%".to_string(),
                tooltip: Some("details".to_string()),
                icon: Some(named_icon("cpu-high")),
                classes: vec!["warning".to_string()],
                should_show: true,
            }
        );
    }

    #[test]
    fn test_plan_output_json_alt_without_text_selects_icon() {
        let icons = icon_map(&[("dark", "moon")]);
        let plan = plan_output(
            r#"{"alt":"dark"}"#,
            "fallback",
            None,
            None,
            &icons,
            None,
            false,
        );

        assert_eq!(plan.label, "");
        assert_eq!(plan.icon, Some(named_icon("moon")));
        assert!(plan.should_show);
    }

    #[test]
    fn test_plan_output_json_alt_can_select_glyph_icon() {
        let icons = BTreeMap::from([("unknown".to_string(), glyph_icon("?"))]);
        let plan = plan_output(
            r#"{"alt":"unknown"}"#,
            "fallback",
            None,
            None,
            &icons,
            None,
            false,
        );

        assert_eq!(plan.label, "");
        assert_eq!(plan.icon, Some(glyph_icon("?")));
        assert!(plan.should_show);
    }

    #[test]
    fn test_plan_output_json_percentage_template_shows_without_text() {
        let plan = plan_output(
            r#"{"percentage":80}"#,
            "fallback",
            Some("{percentage}%"),
            None,
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.label, "80%");
        assert!(plan.should_show);
    }

    #[test]
    fn test_plan_output_ignores_json_icon_extension() {
        let static_icon = named_icon("static-icon");
        let plan = plan_output(
            r#"{"text":"cpu 42%","alt":"missing","icon":"json-icon"}"#,
            "fallback",
            None,
            None,
            &BTreeMap::new(),
            Some(&static_icon),
            false,
        );

        assert_eq!(plan.icon, Some(named_icon("static-icon")));
    }

    #[test]
    fn test_plan_output_json_missing_tooltip_falls_back_to_static() {
        let plan = plan_output(
            r#"{"text":"x"}"#,
            "fallback",
            None,
            Some("static"),
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.tooltip, Some("static".to_string()));
        assert_eq!(plan.classes, Vec::<String>::new());
    }

    #[test]
    fn test_plan_output_json_empty_tooltip_clears_static() {
        let plan = plan_output(
            r#"{"text":"x","tooltip":""}"#,
            "fallback",
            None,
            Some("static"),
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.tooltip, None);
    }

    #[test]
    fn test_plan_output_json_tooltip_overrides_static() {
        let plan = plan_output(
            r#"{"text":"x","tooltip":"dynamic"}"#,
            "fallback",
            None,
            Some("static"),
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.tooltip, Some("dynamic".to_string()));
    }

    #[test]
    fn test_output_should_show_plain_text_uses_fallback() {
        let with_fallback = plan_output("", "fallback", None, None, &BTreeMap::new(), None, false);
        assert_eq!(with_fallback.label, "fallback");
        assert!(with_fallback.should_show);
        let without_fallback = plan_output("", "", None, None, &BTreeMap::new(), None, false);
        assert!(!without_fallback.should_show);
    }

    #[test]
    fn test_output_should_show_empty_plain_text_template_uses_fallback() {
        let plan = plan_output(
            "",
            "fallback",
            Some("[{output}]"),
            None,
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.label, "fallback");
        assert!(plan.should_show);
    }

    #[test]
    fn test_output_should_hide_empty_plain_text_template_without_fallback() {
        let plan = plan_output(
            "",
            "",
            Some("[{output}]"),
            None,
            &BTreeMap::new(),
            None,
            false,
        );

        assert_eq!(plan.label, "");
        assert!(!plan.should_show);
    }

    #[test]
    fn test_output_should_show_plain_text_with_static_image() {
        let plan = plan_output("", "", None, None, &BTreeMap::new(), None, true);

        assert!(plan.should_show);
    }

    #[test]
    fn test_output_should_show_json_empty_text_does_not_use_fallback() {
        let plan = plan_output(
            r#"{"text":""}"#,
            "fallback",
            None,
            None,
            &BTreeMap::new(),
            None,
            false,
        );
        assert_eq!(plan.label, "");
        assert!(!plan.should_show);
    }

    #[test]
    fn test_output_should_show_json_empty_text_with_static_image() {
        let plan = plan_output(
            r#"{"text":""}"#,
            "fallback",
            None,
            None,
            &BTreeMap::new(),
            None,
            true,
        );

        assert_eq!(plan.label, "");
        assert!(plan.should_show);
    }

    #[test]
    fn test_output_should_show_json_empty_text_with_static_icon() {
        let static_icon = named_icon("static-icon");
        let plan = plan_output(
            r#"{"text":""}"#,
            "fallback",
            None,
            None,
            &BTreeMap::new(),
            Some(&static_icon),
            false,
        );

        assert_eq!(plan.label, "");
        assert_eq!(plan.icon, Some(named_icon("static-icon")));
        assert!(plan.should_show);
    }
}
