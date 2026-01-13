//! Core types, configuration, and utilities for vibepanel bar.
//!
//! This crate provides:
//! - Configuration parsing from TOML
//! - Theme palette generation
//! - Logging setup
//! - Shared types used across the bar

pub mod config;
pub mod error;
pub mod logging;
pub mod theme;

pub use config::{Config, ConfigLoadResult, DEFAULT_CONFIG_TOML};
pub use error::{Error, Result};
pub use theme::{AccentSource, SurfaceStyles, ThemePalette, ThemeSizes, parse_hex_color};
