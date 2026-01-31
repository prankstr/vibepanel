//! Compositor backend abstraction for workspace and window title tracking.
//!
//! This module provides a pluggable backend system for different Wayland compositors:
//! - MangoWC / DWL (via `mmsg` CLI tool)
//! - Niri (via socket IPC with JSON protocol)
//! - Hyprland (via socket IPC with JSON protocol)
//!
//! The backend trait feeds both:
//! - `WorkspaceService` (workspace/tag state)
//! - `WindowTitleService` (focused window info)
//!
//! # Usage
//!
//! Services should use `CompositorManager::global()` to get a shared backend instance,
//! then register callbacks via `register_workspace_callback` and `register_window_callback`.

pub mod dwl_ipc;
mod factory;
mod hyprland;
mod manager;
mod mango;
mod niri;
pub mod types;

pub use factory::BackendKind;
pub use hyprland::HyprlandBackend;
pub use manager::CompositorManager;
pub use mango::MangoBackend;
pub use niri::NiriBackend;
pub use types::*;
