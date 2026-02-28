//! Custom widget - user-defined icon/label with optional script polling and click actions.
//!
//! Supports multiple instances via the `custom-` naming prefix.
//! Each `custom-<name>` entry becomes its own widget with a unique CSS class.
//!
//! `on_click` is custom-widget-specific (runs the command, then refreshes `exec`
//! output). `on_click_right` and `on_click_middle` are handled by BaseWidget and
//! available on all widgets.
//!
//! Configuration example:
//! ```toml
//! [widgets]
//! right = ["custom-power", "custom-weather", "clock"]
//!
//! [widgets.custom-power]
//! icon = "system-shutdown-symbolic"
//! label = "Power"
//! tooltip = "Power menu"
//! on_click = "wlogout"
//! # on_click_right and on_click_middle are available on all widgets
//! on_click_right = "systemctl suspend"
//!
//! [widgets.custom-weather]
//! exec = "curl -s 'wttr.in/?format=1'"
//! template = " {output}"
//! interval = 600
//! on_click = "xdg-open https://wttr.in"
//! tooltip = "Weather"
//! max_chars = 30
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib::{self, SourceId};
use gtk4::prelude::*;
use gtk4::{GestureClick, Label, gdk};
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, has_material_mapping};
use crate::styles::{state, widget as wgt};
use crate::widgets::base::{BaseWidget, describe_exit_status};
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Known config keys for the custom widget.
const KNOWN_OPTIONS: &[&str] = &[
    "icon",
    "label",
    "exec",
    "template",
    "interval",
    "on_click",
    "tooltip",
    "max_chars",
];

/// Default exec timeout in seconds.
const EXEC_TIMEOUT_SECS: u64 = 10;

/// Configuration for a custom widget instance.
#[derive(Debug, Clone, Default)]
pub struct CustomConfig {
    /// Logical icon name via `IconsService` (e.g., "system-shutdown-symbolic").
    pub icon: Option<String>,
    /// Static/fallback label text. Supports emoji, Nerd Font glyphs, Unicode.
    pub label: String,
    /// Shell command whose first line of stdout replaces the label text.
    pub exec: Option<String>,
    /// Template for formatting exec output. `{output}` is replaced by the
    /// first line of exec stdout. When absent, exec output replaces the
    /// label wholesale.
    pub template: Option<String>,
    /// Polling interval in seconds. 0 = run once at startup.
    pub interval: u64,
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
            label,
            exec,
            template,
            interval,
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
    label: Option<Label>,
    fallback_text: String,
    template: Option<String>,
    custom_id: String,
    widget: gtk4::Box,
}

/// Pre-exec show_if gate: when present, the show_if command is evaluated
/// before each exec cycle. Non-zero exit hides the widget and skips exec.
#[derive(Clone)]
struct ShowIfGate {
    cmd: String,
}

/// Custom widget that displays a user-configured icon/label with optional
/// script polling and click handlers.
pub struct CustomWidget {
    /// Shared base widget container.
    base: BaseWidget,
    /// Keeps the label GTK widget alive (needed when exec updates it).
    #[allow(dead_code)]
    label: Option<Label>,
    /// Keeps the icon GTK widget alive for the lifetime of this widget.
    #[allow(dead_code)]
    icon_handle: Option<IconHandle>,
    /// Active timer source ID for cancellation on drop.
    timer_source: Rc<RefCell<Option<SourceId>>>,
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

        let css_class_name = format!("{}{custom_id}", wgt::CUSTOM_PREFIX);
        let mut base = BaseWidget::new(&[&css_class_name]);

        // Retrieve show_if config from WidgetOptions (not CustomConfig — these
        // are cross-cutting fields parsed at the WidgetOptions level).
        let (show_if_cmd, show_if_interval) = ConfigManager::global().get_show_if(&css_class_name);

        // When exec + interval is active, show_if piggybacks on the exec cycle
        // instead of running a separate timer. Cancel BaseWidget's show_if timer.
        let piggyback = config.exec.is_some() && config.interval > 0 && show_if_cmd.is_some();
        if piggyback {
            base.cancel_show_if_timer();

            if show_if_interval.is_some() {
                warn!(
                    "Custom widget '{}': show_if_interval is ignored when exec + interval \
                     is set (show_if piggybacks on the exec cycle)",
                    custom_id
                );
            }
        }

        // Enables hover styling for left-click action.
        // BaseWidget handles CLICKABLE for on_click_right / on_click_middle.
        if config.on_click.is_some() {
            base.widget().add_css_class(state::CLICKABLE);
        }

        if let Some(ref tip) = config.tooltip {
            base.set_tooltip(tip);
        }

        let icon_handle = config.icon.as_ref().map(|icon_name| {
            if !has_material_mapping(icon_name) {
                warn!(
                    "Custom widget '{}': icon '{}' has no Material Symbol mapping. \
                     Use theme.icons.theme = \"gtk\" or a Nerd Font glyph in the label field.",
                    custom_id, icon_name
                );
            }
            base.add_icon(icon_name, &[])
        });

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

        let timer_source = Rc::new(RefCell::new(None));

        let exec_show_if = if piggyback {
            show_if_cmd.map(|cmd| ShowIfGate { cmd })
        } else {
            None
        };

        // Must be set up before click handlers to share exec_state
        let exec_state = if let Some(exec_cmd) = config.exec {
            let state = ExecState {
                cmd: exec_cmd,
                label: label.clone(),
                fallback_text: config.label,
                template: config.template,
                custom_id: custom_id.to_string(),
                widget: base.widget().clone(),
            };

            // Run the exec command once immediately (with show_if gate if piggybacking)
            run_exec(
                &state.cmd,
                state.label.as_ref(),
                &state.fallback_text,
                state.template.as_deref(),
                &state.custom_id,
                &state.widget,
                exec_show_if.as_ref(),
            );

            if config.interval > 0 {
                let state_for_timer = state.clone();
                let show_if_for_timer = exec_show_if.clone();

                let source_id = glib::timeout_add_seconds_local(
                    u32::try_from(config.interval).unwrap_or(u32::MAX),
                    move || {
                        run_exec(
                            &state_for_timer.cmd,
                            state_for_timer.label.as_ref(),
                            &state_for_timer.fallback_text,
                            state_for_timer.template.as_deref(),
                            &state_for_timer.custom_id,
                            &state_for_timer.widget,
                            show_if_for_timer.as_ref(),
                        );
                        glib::ControlFlow::Continue
                    },
                );

                *timer_source.borrow_mut() = Some(source_id);
            }

            Some(state)
        } else {
            None
        };

        if let Some(on_click) = config.on_click {
            // Custom widget's own left-click handler. Runs the command, then
            // re-runs exec to refresh the label immediately.
            // Right-click and middle-click are handled by BaseWidget via
            // on_click_right / on_click_middle in the widget config.
            let click = GestureClick::new();
            let click_widget_name = css_class_name.clone();
            click.set_button(gdk::BUTTON_PRIMARY);
            click.connect_released(move |_gesture, _n_press, _x, _y| {
                let cmd = on_click.to_string();
                let exec_state = exec_state.clone();
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

                    // Re-run exec after the click command finishes so the
                    // label reflects the new state immediately.
                    // No show_if gate — the user explicitly clicked.
                    if let Some(ref state) = exec_state {
                        run_exec(
                            &state.cmd,
                            state.label.as_ref(),
                            &state.fallback_text,
                            state.template.as_deref(),
                            &state.custom_id,
                            &state.widget,
                            None,
                        );
                    }
                });
            });
            base.widget().add_controller(click);
        }

        Self {
            base,
            label,
            icon_handle,
            timer_source,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for CustomWidget {
    fn drop(&mut self) {
        // In-flight run_exec futures are safe: cloned GTK refs keep widgets alive,
        // and set_label/set_visible on a detached widget is a harmless no-op.
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
            debug!("Custom widget timer cancelled on drop");
        }
    }
}

/// Run an exec command asynchronously and update the label with its output.
///
/// Uses `glib::spawn_future_local` + `gio::spawn_blocking` to avoid blocking
/// the GTK event loop. The command is run via `sh -c` with a 10-second timeout.
///
/// Auto-hides the widget when exec returns empty output and no fallback label
/// is configured. Shows the widget again when exec returns non-empty output.
///
/// When `show_if` is set, the show_if command is evaluated first as a gate:
/// non-zero exit hides the widget and skips exec entirely.
fn run_exec(
    exec_cmd: &str,
    label: Option<&Label>,
    fallback_text: &str,
    template: Option<&str>,
    custom_id: &str,
    widget: &gtk4::Box,
    show_if: Option<&ShowIfGate>,
) {
    let Some(label) = label else { return };

    let label = label.clone();
    let exec_cmd = exec_cmd.to_string();
    let fallback_text = fallback_text.to_string();
    let template = template.map(String::from);
    let custom_id = custom_id.to_string();
    let widget = widget.clone();
    let show_if_cmd = show_if.map(|g| g.cmd.clone());

    glib::spawn_future_local(async move {
        // Pre-exec show_if gate: if the command exits non-zero, hide and skip exec.
        if let Some(ref cmd) = show_if_cmd {
            let cmd = cmd.clone();
            let custom_id_for_check = custom_id.clone();
            let result = gio::spawn_blocking(move || BaseWidget::run_show_if_command(&cmd)).await;

            let (visible, stderr) = match result {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        widget = %custom_id_for_check,
                        error = ?e,
                        "show_if spawn_blocking failed"
                    );
                    (false, String::new())
                }
            };

            widget.set_visible(visible);
            if !stderr.is_empty() {
                debug!(
                    widget = %custom_id,
                    visible,
                    stderr = %stderr.trim(),
                    "show_if check"
                );
            }

            if !visible {
                return;
            }
        }

        let result = gio::spawn_blocking(move || {
            use std::os::unix::process::ExitStatusExt;
            use std::process::{Command, Stdio};
            use std::time::Duration;

            let child = match Command::new("sh")
                .args(["-c", &exec_cmd])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
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
                    // Timeout expired and sender didn't signal — kill the child.
                    // SAFETY: Sending SIGKILL to a process. If the process already
                    // exited and was reaped, kill() returns ESRCH which is harmless.
                    // Theoretical PID reuse: if the child exits and its PID is
                    // recycled before the timeout fires, we'd kill an unrelated
                    // process. In practice this can't happen here because
                    // wait_with_output() below is the only call that reaps the
                    // child — if it completes before the timeout, tx.send(())
                    // cancels the watchdog. If it hasn't completed, the child is
                    // still alive and owns the PID.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
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
                // Trim whitespace (including the trailing newline from read_line)
                let text = output.trim();
                if text.is_empty() {
                    label.set_label(&fallback_text);
                    // Auto-hide when exec returns empty and no fallback is configured
                    if fallback_text.is_empty() {
                        widget.set_visible(false);
                    }
                } else {
                    let display = if let Some(ref tmpl) = template {
                        tmpl.replace("{output}", text)
                    } else {
                        text.to_string()
                    };
                    label.set_label(&display);
                    widget.set_visible(true);
                }
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
    fn test_custom_config_defaults() {
        let entry = make_entry(HashMap::new());
        let config = CustomConfig::from_entry(&entry);

        assert!(config.icon.is_none());
        assert_eq!(config.label, "");
        assert!(config.exec.is_none());
        assert!(config.template.is_none());
        assert_eq!(config.interval, 0);
        assert!(config.on_click.is_none());
        assert!(config.tooltip.is_none());
        assert!(config.max_chars.is_none());
    }

    #[test]
    fn test_custom_config_full() {
        let mut options = HashMap::new();
        options.insert(
            "icon".to_string(),
            Value::String("system-shutdown-symbolic".to_string()),
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

        assert_eq!(config.icon, Some("system-shutdown-symbolic".to_string()));
        assert_eq!(config.label, "Power");
        assert_eq!(config.exec, Some("echo hello".to_string()));
        assert_eq!(config.template, Some(" {output}".to_string()));
        assert_eq!(config.interval, 600);
        assert_eq!(config.on_click, Some("wlogout".to_string()));
        assert_eq!(config.tooltip, Some("Power menu".to_string()));
        assert_eq!(config.max_chars, Some(30));
    }

    #[test]
    fn test_custom_config_negative_interval_clamped() {
        let mut options = HashMap::new();
        options.insert("interval".to_string(), Value::Integer(-5));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert_eq!(config.interval, 0);
    }

    #[test]
    fn test_custom_config_max_chars_min_one() {
        let mut options = HashMap::new();
        options.insert("max_chars".to_string(), Value::Integer(0));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert_eq!(config.max_chars, Some(1));
    }

    #[test]
    fn test_custom_config_ignores_non_string_icon() {
        let mut options = HashMap::new();
        options.insert("icon".to_string(), Value::Integer(42));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert!(config.icon.is_none());
    }

    #[test]
    fn test_custom_config_template_parsed() {
        let mut options = HashMap::new();
        options.insert(
            "template".to_string(),
            Value::String(" {output}".to_string()),
        );
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert_eq!(config.template, Some(" {output}".to_string()));
    }

    #[test]
    fn test_custom_config_template_ignores_non_string() {
        let mut options = HashMap::new();
        options.insert("template".to_string(), Value::Integer(42));
        let entry = make_entry(options);
        let config = CustomConfig::from_entry(&entry);
        assert!(config.template.is_none());
    }

    // --- show_if piggyback tests ---

    #[test]
    fn test_run_show_if_command_true() {
        let (visible, stderr) = BaseWidget::run_show_if_command("true");
        assert!(visible);
        assert!(stderr.is_empty());
    }

    #[test]
    fn test_run_show_if_command_false() {
        let (visible, stderr) = BaseWidget::run_show_if_command("false");
        assert!(!visible);
        assert!(stderr.is_empty());
    }

    #[test]
    fn test_run_show_if_command_captures_stderr() {
        let (visible, stderr) = BaseWidget::run_show_if_command("echo err >&2");
        assert!(visible);
        assert!(stderr.trim() == "err");
    }

    #[test]
    fn test_run_show_if_command_nonexistent_binary() {
        let (visible, _stderr) =
            BaseWidget::run_show_if_command("/nonexistent/binary/that/does/not/exist");
        assert!(!visible);
    }
}
