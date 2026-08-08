//! Backend factory and detection.
//!
//! Provides automatic compositor detection and backend instantiation.

use std::env;
use tracing::{debug, info};

use super::{CompositorBackend, HyprlandBackend, MangoBackend, NiriBackend, SwayBackend};

/// Backend kind enum for configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// MangoWC (uses Mango's JSON IPC socket).
    Mango,
    /// Hyprland compositor.
    Hyprland,
    /// Niri compositor.
    Niri,
    /// Sway and i3-compatible compositors (Miracle WM, Scroll).
    Sway,
    /// Auto-detect from environment.
    Auto,
}

impl BackendKind {
    /// Parse a backend kind from a string (case-insensitive).
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "mango" | "mangowc" => BackendKind::Mango,
            "hyprland" => BackendKind::Hyprland,
            "niri" => BackendKind::Niri,
            "sway" | "miracle" | "miraclewm" | "scroll" => BackendKind::Sway,
            "auto" | "" => BackendKind::Auto,
            _ => BackendKind::Auto, // Unknown defaults to auto-detect
        }
    }
}

/// Detect the compositor backend from environment variables.
///
/// Detection order:
/// 1. HYPRLAND_INSTANCE_SIGNATURE → Hyprland
/// 2. NIRI_SOCKET → Niri
/// 3. SWAYSOCK → Sway
/// 4. MIRACLESOCK → Sway (Miracle WM supports i3 IPC)
/// 5. Default → MangoWC
pub fn detect_backend() -> BackendKind {
    // Check for Hyprland
    if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        debug!("Detected Hyprland via HYPRLAND_INSTANCE_SIGNATURE");
        return BackendKind::Hyprland;
    }

    // Check for Niri
    if env::var("NIRI_SOCKET").is_ok() {
        debug!("Detected Niri via NIRI_SOCKET");
        return BackendKind::Niri;
    }

    // Check for Sway
    if env::var("SWAYSOCK").is_ok() {
        debug!("Detected Sway via SWAYSOCK");
        return BackendKind::Sway;
    }

    // Check for Miracle WM (uses same i3 IPC protocol as Sway)
    if env::var("MIRACLESOCK").is_ok() {
        debug!("Detected Miracle WM via MIRACLESOCK");
        return BackendKind::Sway;
    }

    // Default to MangoWC
    debug!("No compositor-specific socket detected, defaulting to MangoWC");
    BackendKind::Mango
}

/// Create a compositor backend based on kind and config.
///
/// # Arguments
///
/// * `kind` - The backend kind to create (or Auto for detection).
///
/// # Returns
///
/// A boxed backend implementation ready for use.
pub fn create_backend(kind: BackendKind) -> Box<dyn CompositorBackend> {
    let resolved_kind = if kind == BackendKind::Auto {
        detect_backend()
    } else {
        kind
    };

    info!("Creating compositor backend: {:?}", resolved_kind);

    match resolved_kind {
        BackendKind::Mango => Box::new(MangoBackend::new()),
        BackendKind::Hyprland => Box::new(HyprlandBackend::new()),
        BackendKind::Niri => Box::new(NiriBackend::new()),
        BackendKind::Sway => Box::new(SwayBackend::new()),
        BackendKind::Auto => {
            // Should never reach here after resolution, but handle gracefully
            Box::new(MangoBackend::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_kind_from_str() {
        assert_eq!(BackendKind::from_str("mango"), BackendKind::Mango);
        assert_eq!(BackendKind::from_str("MangoWC"), BackendKind::Mango);
        assert_eq!(BackendKind::from_str("hyprland"), BackendKind::Hyprland);
        assert_eq!(BackendKind::from_str("HYPRLAND"), BackendKind::Hyprland);
        assert_eq!(BackendKind::from_str("niri"), BackendKind::Niri);
        assert_eq!(BackendKind::from_str("Niri"), BackendKind::Niri);
        assert_eq!(BackendKind::from_str("sway"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("Sway"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("miracle"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("miraclewm"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("MiracleWM"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("scroll"), BackendKind::Sway);
        assert_eq!(BackendKind::from_str("auto"), BackendKind::Auto);
        assert_eq!(BackendKind::from_str(""), BackendKind::Auto);
        assert_eq!(BackendKind::from_str("unknown"), BackendKind::Auto);
    }
}
