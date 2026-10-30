//! `ext-workspace-v1` support.
//!
//! - [`client`]: protocol client on its own Wayland connection and thread.
//! - [`backend`]: [`ExtWorkspaceBackend`], the generic backend for compositors
//!   without a native IPC backend. It also runs, for window titles only,
//!   without ext-workspace.
//! - [`overlay`]: merging ext-workspace state (several active / hidden
//!   workspaces) over the Hyprland backend; owned by `CompositorManager`.

mod backend;
mod client;
pub mod overlay;

pub use backend::ExtWorkspaceBackend;
pub(crate) use client::MANAGER_INTERFACE;
pub use client::{ExtWorkspaceClient, ModelCallback};
