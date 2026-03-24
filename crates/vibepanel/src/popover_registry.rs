//! Name-based popover registry for IPC control.
//!
//! Maps widget names (e.g., "clock", "quick_settings") to their
//! `PopoverToggleable` handles, enabling IPC commands like
//! `vibepanel popover open clock`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tracing::{debug, warn};

/// Trait for popovers that can be controlled via IPC.
///
/// Object-safe: no generics, no `Self` return types, no `Sized` bound.
pub trait PopoverToggleable {
    /// Show the popover (idempotent — no-op if already visible).
    fn ipc_show(&self);
    /// Hide the popover (idempotent — no-op if already hidden).
    fn ipc_hide(&self);
    /// Toggle the popover visibility.
    ///
    /// Default calls `ipc_show()`/`ipc_hide()` based on `ipc_is_visible()`.
    /// Implementors may override if their toggle has different semantics.
    fn ipc_toggle(&self) {
        if self.ipc_is_visible() {
            self.ipc_hide();
        } else {
            self.ipc_show();
        }
    }
    /// Check if the popover is currently visible.
    fn ipc_is_visible(&self) -> bool;

    /// Return the monitor connector name (e.g., "eDP-1") this handle belongs to.
    /// Used on multi-monitor setups to dispatch to the focused monitor's handle.
    /// Default: `None` (treat as unknown — will match any monitor as fallback).
    fn monitor_connector(&self) -> Option<String> {
        None
    }
}

/// Actions that can be dispatched to a registered popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAction {
    Show,
    Hide,
    Toggle,
}

thread_local! {
    static REGISTRY: RefCell<HashMap<String, Vec<Rc<dyn PopoverToggleable>>>> =
        RefCell::new(HashMap::new());
}

/// Register a popover handle under the given widget name.
///
/// Names use underscores internally (e.g., "quick_settings"). The CLI uses
/// hyphens ("quick-settings") and normalization happens at dispatch time.
///
/// On multi-monitor setups, multiple handles may be registered under the same
/// name (one per bar). Dispatch resolves to the focused monitor's handle.
pub fn register(name: &str, handle: Rc<dyn PopoverToggleable>) {
    REGISTRY.with(|r| {
        debug!("PopoverRegistry: registered '{}'", name);
        r.borrow_mut()
            .entry(name.to_string())
            .or_default()
            .push(handle);
    });
}

/// Dispatch an action to a named popover.
///
/// The name is normalized (hyphens -> underscores) before lookup.
/// On multi-monitor setups, dispatches to the handle on the focused monitor.
/// Falls back to the first registered handle if the focused monitor can't be determined.
/// Returns `true` if a handle was found and the action was dispatched.
pub fn dispatch(name: &str, action: DispatchAction) -> bool {
    let normalized = name.replace('-', "_");
    let handle = REGISTRY.with(|r| {
        let reg = r.borrow();
        let handles = reg.get(&normalized)?;
        if handles.len() == 1 {
            return Some(handles[0].clone());
        }
        // Multi-monitor: resolve to the focused monitor's handle.
        resolve_focused_handle(handles)
    });

    if let Some(handle) = handle {
        match action {
            DispatchAction::Show => handle.ipc_show(),
            DispatchAction::Hide => handle.ipc_hide(),
            DispatchAction::Toggle => handle.ipc_toggle(),
        }
        debug!(
            "PopoverRegistry: dispatched {:?} to '{}'",
            action, normalized
        );
        true
    } else {
        warn!(
            "PopoverRegistry: no handle registered for '{}' (normalized from '{}')",
            normalized, name
        );
        false
    }
}

/// Resolve which handle to dispatch to based on the focused monitor.
///
/// Queries the compositor for the focused window's output, then finds the
/// handle whose `monitor_connector()` matches. Falls back to the first handle.
fn resolve_focused_handle(
    handles: &[Rc<dyn PopoverToggleable>],
) -> Option<Rc<dyn PopoverToggleable>> {
    if handles.is_empty() {
        return None;
    }
    let focused_output = crate::services::compositor::CompositorManager::global()
        .get_focused_window()
        .and_then(|w| w.output);

    if let Some(ref output) = focused_output {
        for handle in handles {
            if handle.monitor_connector().as_deref() == Some(output) {
                return Some(handle.clone());
            }
        }
        debug!(
            "PopoverRegistry: no handle matches focused output '{}', using first",
            output
        );
    }
    Some(handles[0].clone())
}

/// Clear all registered handles.
///
/// Called from `BarManager::reconfigure_all()` before bars are destroyed.
/// New registrations happen during bar rebuild.
pub fn clear() {
    REGISTRY.with(|r| {
        let count: usize = r.borrow().values().map(|v| v.len()).sum();
        r.borrow_mut().clear();
        debug!("PopoverRegistry: cleared {} handle(s)", count);
    });
}
