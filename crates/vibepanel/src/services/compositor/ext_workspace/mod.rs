//! `ext-workspace-v1` support.
//!
//! - [`client`]: protocol client on its own Wayland connection and thread,
//!   also used by the generic backend.
//! - [`overlay`]: merging ext-workspace state (several active / hidden
//!   workspaces) over the Hyprland backend; owned by `CompositorManager`.

pub(super) mod client;
pub mod overlay;

pub(crate) use client::MANAGER_INTERFACE;
pub use client::{ExtWorkspaceClient, ModelCallback};
