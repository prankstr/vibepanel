//! Shared base widget abstraction for vibepanel widgets.
//!
//! Provides a thin, reusable wrapper around a root `gtk4::Box` with
//! common CSS classes and helpers for labels, icons, and tooltips.

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, GestureClick, Label, Orientation, Popover, PositionType, gdk};
use std::cell::{Cell, RefCell};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use crate::popover_tracker::{PopoverId, PopoverTracker};
use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, IconsService};
use crate::services::tooltip::TooltipManager;
use crate::styles::{class, state, surface};
use crate::widgets::layer_shell_popover::{Dismissible, LayerShellPopover};
use gtk4::gio;
use gtk4::glib;
use tracing::{debug, info, warn};

/// Timeout for `show_if` commands in seconds.
const SHOW_IF_TIMEOUT_SECS: u64 = 5;

/// Configure a GTK popover with standard settings.
///
/// This is used for internal popovers within Quick Settings cards and tray menus,
/// NOT for the main widget menus (which use LayerShellPopover).
///
/// Applies:
/// - No arrow
/// - Autohide enabled
/// - `widget-menu` CSS class
/// - Bottom position
/// - Center alignment
/// - Configurable vertical offset from config
pub fn configure_popover(popover: &Popover) {
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.add_css_class(surface::WIDGET_MENU);
    popover.add_css_class(surface::NO_FOCUS);
    popover.set_position(PositionType::Bottom);
    popover.set_halign(Align::Center);

    // Get the popover offset from config (defaults to 1 if not set)
    let offset = ConfigManager::global().popover_offset() as i32;
    popover.set_offset(0, offset);
}

/// Handle for managing a widget menu popover.
///
/// This wraps a `LayerShellPopover` and provides the same API as the old
/// GTK Popover-based implementation, but with proper layer-shell keyboard
/// focus, ESC-to-close, and click-outside-to-close behavior.
///
/// Uses lazy initialization: the actual LayerShellPopover is created on first
/// use (when `show()` is called), because at widget construction time the widget
/// isn't yet attached to a window.
pub struct MenuHandle {
    /// The lazily-initialized popover
    popover: RefCell<Option<Rc<LayerShellPopover>>>,
    /// Builder function to create the popover content
    builder: Rc<dyn Fn() -> gtk4::Widget>,
    /// Widget name for CSS styling
    widget_name: String,
    /// Parent widget container
    parent: GtkBox,
    /// ID returned from PopoverTracker when this popover is active.
    /// Used to correctly clear ourselves from the tracker on hide.
    tracker_id: Cell<Option<PopoverId>>,
}

impl MenuHandle {
    fn new<F>(widget_name: String, builder: F, parent: GtkBox) -> Rc<Self>
    where
        F: Fn() -> gtk4::Widget + 'static,
    {
        Rc::new(Self {
            popover: RefCell::new(None),
            builder: Rc::new(builder),
            widget_name,
            parent,
            tracker_id: Cell::new(None),
        })
    }

    /// Ensure the popover is created, creating it lazily if needed.
    ///
    /// Returns `None` if the widget isn't attached to a window yet (shouldn't
    /// happen in practice since this is called on user click, but we handle
    /// it gracefully to avoid panics during teardown/hot-reload).
    fn ensure_popover(&self) -> Option<Rc<LayerShellPopover>> {
        let mut popover_opt = self.popover.borrow_mut();
        if let Some(ref popover) = *popover_opt {
            return Some(popover.clone());
        }

        // Get the application from the widget's window - should work now since
        // we're called when the user clicks, at which point widget is attached
        let app = self
            .parent
            .root()
            .and_then(|r| r.downcast::<gtk4::Window>().ok())
            .and_then(|w| w.application());

        let Some(app) = app else {
            tracing::warn!(
                "MenuHandle::ensure_popover called but widget '{}' has no application",
                self.widget_name
            );
            return None;
        };

        let builder = self.builder.clone();
        let popover = LayerShellPopover::new(&app, &self.widget_name, move || builder());

        *popover_opt = Some(popover.clone());
        Some(popover)
    }

    /// Get the anchor position for the popover.
    ///
    /// Returns the widget's center X coordinate (in monitor-local coordinates)
    /// and the monitor it's on.
    ///
    /// # Coordinate Space
    ///
    /// The returned `anchor_x` is relative to the monitor's origin (0,0 at top-left
    /// of the monitor), NOT global screen coordinates. This is correct because:
    ///
    /// 1. Layer-shell surfaces are per-monitor - the bar is anchored to a specific
    ///    monitor and its native surface coordinates are relative to that monitor.
    /// 2. `compute_bounds(&native)` returns coordinates relative to the native
    ///    surface, which for layer-shell is the monitor-local coordinate space.
    /// 3. The popover is also a layer-shell surface on the same monitor, so it
    ///    uses the same coordinate space for its margin calculations.
    fn get_anchor_info(&self) -> (i32, Option<gtk4::gdk::Monitor>) {
        let Some(native) = self.parent.native() else {
            return (0, None);
        };

        let Some(bounds) = self.parent.compute_bounds(&native) else {
            return (0, None);
        };

        let widget_x = bounds.x() as i32;
        let widget_width = bounds.width() as i32;
        // anchor_x is monitor-relative: the center X of the widget on its monitor
        let anchor_x = widget_x + widget_width / 2;

        // Get monitor - the popover will be placed on the same monitor
        let monitor = self
            .parent
            .root()
            .and_then(|r| r.downcast_ref::<gtk4::Window>().cloned())
            .and_then(|w| w.surface())
            .and_then(|s| gtk4::gdk::Display::default().and_then(|d| d.monitor_at_surface(&s)));

        (anchor_x, monitor)
    }

    pub fn show(&self) {
        let Some(popover) = self.ensure_popover() else {
            return;
        };
        let (anchor_x, monitor) = self.get_anchor_info();

        // Register as active popup and store the ID for later clearing
        let id = PopoverTracker::global().set_active(popover.clone());
        self.tracker_id.set(Some(id));

        popover.show_at(anchor_x, monitor);
    }

    pub fn hide(&self) {
        if let Some(ref popover) = *self.popover.borrow() {
            popover.hide();
        }
        // Clear from tracker using our stored ID (prevents clearing another's registration)
        if let Some(id) = self.tracker_id.take() {
            PopoverTracker::global().clear_if_active(id);
        }
    }

    /// Check if the popover is currently visible.
    pub fn is_visible(&self) -> bool {
        self.popover
            .borrow()
            .as_ref()
            .map(|p| p.is_visible())
            .unwrap_or(false)
    }

    /// Refresh the popover content if it's currently visible.
    ///
    /// For layer-shell popovers, this hides and re-shows the popover
    /// to rebuild its content. This may cause a brief visual flash,
    /// but is necessary because layer-shell windows are recreated fresh
    /// each time (to avoid stale state issues with layer-shell surfaces).
    ///
    /// Used by widgets like Notifications that need to update their
    /// popover content dynamically while the popover is open.
    pub fn refresh_if_visible(&self) {
        if self.is_visible() {
            let Some(popover) = self.ensure_popover() else {
                return;
            };
            let (anchor_x, monitor) = self.get_anchor_info();
            popover.hide();
            popover.show_at(anchor_x, monitor);
        }
    }
}

impl Dismissible for MenuHandle {
    fn dismiss(&self) {
        self.hide();
    }

    fn is_visible(&self) -> bool {
        self.is_visible()
    }
}

/// Describe a process exit status in human-readable form.
///
/// Translates well-known shell exit codes (127 = command not found,
/// 126 = permission denied) into actionable messages.
pub(super) fn describe_exit_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(127) => "command not found".to_string(),
        Some(126) => "permission denied (not executable)".to_string(),
        Some(code) => format!("exit code {code}"),
        None => {
            use std::os::unix::process::ExitStatusExt;
            match status.signal() {
                Some(sig) => format!("killed by signal {sig}"),
                None => "unknown exit status".to_string(),
            }
        }
    }
}

/// Spawn a shell command, reaping the child process in a background thread.
fn spawn_click_command(widget_name: &str, cmd: &str) {
    match Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let widget_name = widget_name.to_string();
            let cmd = cmd.to_string();
            // Reap the child in a background thread to avoid zombie processes
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::warn!(
                        "'{}' click command '{}' failed: {}",
                        widget_name,
                        cmd,
                        describe_exit_status(status)
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "'{}' click command '{}' wait failed: {}",
                        widget_name,
                        cmd,
                        e
                    );
                }
                _ => {}
            });
        }
        Err(e) => {
            tracing::warn!(
                "'{}' failed to spawn click command '{}': {}",
                widget_name,
                cmd,
                e
            );
        }
    }
}

/// Shared base widget container.
///
/// Each widget owns a `BaseWidget` instance and exposes the underlying
/// `gtk4::Box` as its root widget.
///
/// The BaseWidget automatically creates an inner `.content` box for consistent
/// padding and theming across all widgets. Widgets should add their children to
/// `content()` rather than `widget()` directly.
pub struct BaseWidget {
    container: GtkBox,
    content: GtkBox,
    /// The widget's menu popover, if any. Each BaseWidget supports at most one menu.
    menu: Rc<RefCell<Option<Rc<MenuHandle>>>>,
    /// Widget name for CSS class-based styling of popovers (e.g., "clock")
    widget_name: String,
    _gesture_click: GestureClick,
    _show_if_timer: Option<glib::SourceId>,
}

impl BaseWidget {
    /// Create a new base widget container.
    ///
    /// - Uses a horizontal box with zero internal spacing (widget-specific
    ///   spacing should be configured by the widget itself).
    /// - Always adds the `widget` CSS class.
    /// - Creates an inner `.content` box for consistent padding/margins.
    /// - Applies any additional CSS classes passed in `extra_classes`.
    /// - The first class in `extra_classes` is used as the widget name for
    ///   popover styling (e.g., "clock" -> popovers get "clock-popover" class).
    pub fn new(extra_classes: &[&str]) -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 0);
        container.add_css_class(class::WIDGET);
        container.add_css_class(class::WIDGET_ITEM);
        container.set_hexpand(false);
        for cls in extra_classes {
            container.add_css_class(cls);
        }

        // First extra class is the widget name (e.g., "clock", "battery")
        let widget_name = extra_classes
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Create inner content box for consistent padding/margins via CSS
        // Spacing between children is controlled via CSS (see bar.rs .widget > .content)
        let content = GtkBox::new(Orientation::Horizontal, 0);
        content.add_css_class(class::CONTENT);
        // Fill the widget height so children can be properly centered within
        content.set_vexpand(true);
        content.set_valign(Align::Fill);
        // Disable baseline alignment - it can cause vertical offset issues with text
        content.set_baseline_position(gtk4::BaselinePosition::Center);
        container.append(&content);

        let menu: Rc<RefCell<Option<Rc<MenuHandle>>>> = Rc::new(RefCell::new(None));

        let (on_click_right, on_click_middle) =
            ConfigManager::global().get_click_handlers(&widget_name);

        let has_click_handler = on_click_right.is_some() || on_click_middle.is_some();
        if has_click_handler {
            container.add_css_class(state::CLICKABLE);
        }

        let gesture_click = GestureClick::new();
        // Receive all mouse buttons so we can handle right-click and middle-click
        gesture_click.set_button(0);
        {
            let menu_for_cb = menu.clone();
            let widget_name_for_cb = widget_name.clone();
            // Use connect_released for immediate response without double-click detection delay
            gesture_click.connect_released(move |gesture, n_press, x, y| {
                debug!(
                    "BaseWidget click: n_press={}, button={}",
                    n_press,
                    gesture.current_button()
                );

                // Check if the click target is inside an interactive widget that should
                // handle its own clicks. Currently only checks for Button; if bar widgets
                // gain other interactive children (Switch, Entry, etc.), add them here.
                if let Some(widget) = gesture.widget()
                    && let Some(target) = widget.pick(x, y, gtk4::PickFlags::DEFAULT)
                {
                    // Walk up from target to find if it's inside a button
                    let mut current: Option<gtk4::Widget> = Some(target);
                    while let Some(w) = current {
                        if w.downcast_ref::<gtk4::Button>().is_some() {
                            debug!("BaseWidget click: target is a Button, skipping");
                            return;
                        }
                        current = w.parent();
                    }
                }

                let button = gesture.current_button();

                // Process every click regardless of n_press count
                // (we don't use double-click, so treat them all as single clicks)
                match button {
                    gdk::BUTTON_PRIMARY => {
                        let my_menu_was_visible = menu_for_cb
                            .borrow()
                            .as_ref()
                            .map(|m| m.is_visible())
                            .unwrap_or(false);

                        // Cancel any pending or visible tooltips to prevent them from
                        // appearing after the click
                        TooltipManager::global().cancel_and_hide();

                        // Dismiss any active popup (enables seamless transitions)
                        PopoverTracker::global().dismiss_active();

                        if let Some(ref menu) = *menu_for_cb.borrow() {
                            // If our menu was already open, we just closed it - don't re-open
                            // If it wasn't open, open it now
                            if !my_menu_was_visible {
                                debug!("Opening menu from BaseWidget click");
                                menu.show();
                            } else {
                                debug!("Closed own menu from BaseWidget click");
                            }
                        } else {
                            debug!("BaseWidget click: no menu registered");
                        }
                    }
                    gdk::BUTTON_MIDDLE => {
                        if let Some(ref cmd) = on_click_middle {
                            debug!("BaseWidget middle-click: sh -c {}", cmd);
                            spawn_click_command(&widget_name_for_cb, cmd);
                        }
                    }
                    gdk::BUTTON_SECONDARY => {
                        if let Some(ref cmd) = on_click_right {
                            debug!("BaseWidget right-click: sh -c {}", cmd);
                            spawn_click_command(&widget_name_for_cb, cmd);
                        }
                    }
                    _ => {}
                }
            });
        }

        container.add_controller(gesture_click.clone());

        let show_if_timer = Self::setup_show_if_timer(&widget_name, &container);

        Self {
            container,
            content,
            menu,
            widget_name,
            _gesture_click: gesture_click,
            _show_if_timer: show_if_timer,
        }
    }

    /// Set up async `show_if` evaluation if configured.
    ///
    /// Widget starts hidden and the first check runs asynchronously in the
    /// background. When the result arrives, visibility is updated. If
    /// `show_if_interval` is set (and > 0), a periodic timer continues
    /// re-evaluating after the first check. Commands that exceed
    /// `SHOW_IF_TIMEOUT_SECS` are killed and treated as "hide".
    fn setup_show_if_timer(widget_name: &str, container: &GtkBox) -> Option<glib::SourceId> {
        let (show_if, interval) = ConfigManager::global().get_show_if(widget_name);

        let cmd = show_if?;

        // Start hidden; async evaluation will show the widget if appropriate.
        container.set_visible(false);

        let has_interval = interval.filter(|&i| i > 0).is_some();
        let prev_visible = Rc::new(Cell::new(false));
        let container_clone = container.clone();
        let name = widget_name.to_string();

        // Run initial check asynchronously
        {
            let cmd = cmd.clone();
            let container = container_clone.clone();
            let name = name.clone();
            let prev_visible = prev_visible.clone();

            glib::spawn_future_local(async move {
                let cmd_for_retry = cmd.clone();
                let result =
                    gio::spawn_blocking(move || Self::run_show_if_command_with_timeout(&cmd)).await;

                let (visible, stderr) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(widget = %name, error = ?e, "show_if spawn_blocking failed");
                        (false, String::new())
                    }
                };

                container.set_visible(visible);
                debug!(
                    widget = %name,
                    visible,
                    stderr = %stderr.trim(),
                    "show_if initial check"
                );
                prev_visible.set(visible);

                // If hidden and no periodic interval, schedule a single retry
                // after 500ms. This handles race conditions where the data source
                // (e.g., compositor IPC) isn't fully ready when bars are recreated
                // on monitor hotplug. The retry is one-directional: it can only
                // transition hidden → visible.
                if !visible && !has_interval {
                    let container = container.clone();
                    let name = name.clone();
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        glib::spawn_future_local(async move {
                            let result = gio::spawn_blocking(move || {
                                Self::run_show_if_command_with_timeout(&cmd_for_retry)
                            })
                            .await;

                            let (visible, _stderr) = match result {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        widget = %name,
                                        error = ?e,
                                        "show_if retry spawn_blocking failed"
                                    );
                                    (false, String::new())
                                }
                            };

                            if visible {
                                container.set_visible(true);
                                debug!(widget = %name, "show_if retry: now visible");
                            }
                        });
                    });
                }
            });
        }

        // If an interval is configured, start a periodic timer for subsequent checks
        let interval = interval.filter(|&i| i > 0)?;

        let source_id =
            glib::timeout_add_seconds_local(interval.min(u32::MAX as u64) as u32, move || {
                let cmd = cmd.clone();
                let container = container_clone.clone();
                let name = name.clone();
                let prev_visible = prev_visible.clone();

                glib::spawn_future_local(async move {
                    let result =
                        gio::spawn_blocking(move || Self::run_show_if_command_with_timeout(&cmd))
                            .await;

                    let (visible, stderr) = match result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(widget = %name, error = ?e, "show_if spawn_blocking failed");
                            (false, String::new())
                        }
                    };

                    if visible != prev_visible.get() {
                        container.set_visible(visible);
                        info!(
                            widget = %name,
                            visible,
                            stderr = %stderr.trim(),
                            "show_if visibility changed"
                        );
                        prev_visible.set(visible);
                    }
                });

                glib::ControlFlow::Continue
            });

        Some(source_id)
    }

    /// Cancel the periodic `show_if` timer, if any.
    ///
    /// Used by widgets that handle `show_if` themselves (e.g., custom widgets
    /// piggyback on their exec polling cycle instead of running a separate timer).
    pub(super) fn cancel_show_if_timer(&mut self) {
        if let Some(source_id) = self._show_if_timer.take() {
            source_id.remove();
        }
    }

    /// Run a `show_if` shell command synchronously with a timeout.
    /// Returns `(visible, stderr)` where visible = exit 0.
    /// Commands exceeding `SHOW_IF_TIMEOUT_SECS` are killed and treated as hidden.
    pub(super) fn run_show_if_command_with_timeout(cmd: &str) -> (bool, String) {
        let mut child = match Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return (false, String::new()),
        };

        let pid = child.id() as libc::pid_t;
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        std::thread::spawn(move || {
            if rx
                .recv_timeout(Duration::from_secs(SHOW_IF_TIMEOUT_SECS))
                .is_err()
            {
                // Timeout expired — kill the child.
                // SAFETY: Sending SIGKILL to a process. If the process already
                // exited and was reaped, kill() returns ESRCH which is harmless.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        });

        let status = child.wait();

        // Cancel the watchdog (no-op if it already fired).
        let _ = tx.send(());

        match status {
            Ok(s) => {
                use std::os::unix::process::ExitStatusExt;
                if !s.success() && s.signal() == Some(libc::SIGKILL) {
                    warn!(
                        cmd,
                        "show_if command timed out after {SHOW_IF_TIMEOUT_SECS}s"
                    );
                    return (false, String::new());
                }
                (s.success(), String::new())
            }
            Err(_) => (false, String::new()),
        }
    }

    /// Run a `show_if` shell command synchronously (no timeout).
    /// Returns `(visible, stderr)` where visible = exit 0.
    pub(super) fn run_show_if_command(cmd: &str) -> (bool, String) {
        Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .map(|o| {
                let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                (o.status.success(), stderr)
            })
            .unwrap_or((false, String::new()))
    }

    /// Get the root GTK container for this widget.
    ///
    /// This is the outermost box with the `widget` CSS class.
    /// Most widgets should use `content()` to add children instead.
    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Get the inner content box for adding widget children.
    ///
    /// This box has the `content` CSS class and receives consistent
    /// padding/margins via CSS rules like `.widget > .content`.
    /// Widgets should add their labels, icons, etc. to this box.
    pub fn content(&self) -> &GtkBox {
        &self.content
    }

    /// Create an icon using `IconsService`, apply CSS classes, pack it into the
    /// content box, and return the `IconHandle`.
    pub fn add_icon(&self, icon_name: &str, css_classes: &[&str]) -> IconHandle {
        let icons = IconsService::global();
        let handle = icons.create_icon(icon_name, css_classes);
        self.content.append(&handle.widget());
        handle
    }

    /// Create a label and append it to the content box.
    ///
    /// Creates a standard GTK label with CSS classes for styling.
    /// Font rendering is handled centrally by the Pango workaround system
    /// when `pango_font_rendering` is enabled in config.
    ///
    /// # Arguments
    /// * `text` - Initial label text (or None for empty)
    /// * `css_classes` - CSS classes to apply for styling (colors, etc.)
    ///
    /// # Example
    /// ```ignore
    /// use crate::styles::widget;
    /// let label = base.add_label(Some("100%"), &[widget::BATTERY_PERCENTAGE]);
    /// ```
    pub fn add_label(&self, text: Option<&str>, css_classes: &[&str]) -> Label {
        let label = Label::new(text);
        for class in css_classes {
            label.add_css_class(class);
        }
        self.content.append(&label);
        label
    }

    /// Set a styled tooltip on the root container using `TooltipManager`.
    pub fn set_tooltip(&self, text: &str) {
        let tooltip_manager = TooltipManager::global();
        tooltip_manager.set_styled_tooltip(&self.container, text);
    }

    /// Create a menu popover for this widget.
    ///
    /// This creates a layer-shell popover with proper keyboard focus handling,
    /// ESC-to-close, and click-outside-to-close behavior.
    ///
    /// Each BaseWidget supports at most one menu.
    ///
    /// Note: The actual LayerShellPopover is created lazily on first use,
    /// since at widget construction time the widget isn't yet attached to a window.
    ///
    /// Also adds the `clickable` CSS class to enable hover styling for interactive widgets.
    pub fn create_menu<F>(&self, builder: F) -> Rc<MenuHandle>
    where
        F: Fn() -> gtk4::Widget + 'static,
    {
        // Mark as clickable so CSS hover styling applies
        self.container.add_css_class(state::CLICKABLE);

        let handle = MenuHandle::new(self.widget_name.clone(), builder, self.container.clone());
        *self.menu.borrow_mut() = Some(handle.clone());
        handle
    }
}

impl Drop for BaseWidget {
    fn drop(&mut self) {
        if let Some(source_id) = self._show_if_timer.take() {
            source_id.remove();
        }
    }
}
