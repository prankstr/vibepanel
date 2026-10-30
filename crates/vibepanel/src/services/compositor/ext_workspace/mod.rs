//! `ext-workspace-v1` support.
//!
//! - [`client`]: protocol client on its own Wayland connection and thread.
//! - [`backend`]: [`ExtWorkspaceBackend`], a workspace backend built purely on
//!   the protocol, for compositors without a native IPC backend. This is the
//!   primary use.
//! - [`overlay`]: merging ext-workspace state (several active / hidden
//!   workspaces) over a native backend; owned by `CompositorManager`.

mod backend;
mod client;
pub mod overlay;

pub use backend::ExtWorkspaceBackend;
pub use client::{ExtWorkspaceClient, ModelCallback};

/// Whether the compositor offers ext-workspace-v1.
pub fn is_available() -> bool {
    ExtWorkspaceClient::is_available()
}
