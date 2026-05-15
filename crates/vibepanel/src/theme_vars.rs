//! Internal theme variable regression expectations.
//!
//! This is not a public CSS API manifest. User-facing CSS hooks are documented
//! best-effort in the project wiki; this test metadata only tracks variables
//! that production CSS or Rust-composed styling depends on.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeVarScope {
    Root,
    UserHook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeVarRole {
    BuiltinCss,
    OptionalBuiltinCss,
    RustComposedCss,
    Alias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ThemeVarExpectation {
    pub(super) name: &'static str,
    pub(super) scope: ThemeVarScope,
    pub(super) role: ThemeVarRole,
    pub(super) hover_binding: bool,
}

use ThemeVarRole::{Alias, BuiltinCss, OptionalBuiltinCss, RustComposedCss};
use ThemeVarScope::{Root, UserHook};

pub(super) const THEME_VAR_EXPECTATIONS: &[ThemeVarExpectation] = &[
    var("--bar-height", Root, BuiltinCss),
    var("--bar-outline-color", Root, BuiltinCss),
    var("--bar-outline-opacity", Root, BuiltinCss),
    var("--bar-outline-width", Root, BuiltinCss),
    var("--bar-padding-y", Root, BuiltinCss),
    var("--bar-padding-y-bottom", Root, BuiltinCss),
    var("--color-accent-hover-bg", Root, BuiltinCss),
    var("--color-accent-primary", Root, BuiltinCss),
    var("--color-accent-slider", Root, BuiltinCss),
    var("--color-accent-text", Root, BuiltinCss),
    var("--color-background-bar", Root, BuiltinCss),
    var("--color-card-overlay", Root, BuiltinCss),
    var("--color-card-overlay-hover", Root, BuiltinCss),
    var("--color-card-overlay-subtle", Root, Alias),
    var("--color-click-catcher-overlay", Root, BuiltinCss),
    var("--color-foreground-disabled", Root, BuiltinCss),
    var("--color-foreground-faint", Root, BuiltinCss),
    var("--color-foreground-muted", Root, BuiltinCss),
    var("--color-foreground-primary", Root, BuiltinCss),
    var("--color-row-background", Root, Alias),
    var("--color-row-background-hover", Root, Alias),
    var("--color-row-critical-background", Root, BuiltinCss),
    var("--color-slider-track", Root, BuiltinCss),
    var("--color-state-urgent", Root, BuiltinCss),
    var("--color-state-warning", Root, BuiltinCss),
    hover_var("--color-taskbar-button-active-hover-bg", Root, BuiltinCss),
    hover_var("--color-taskbar-button-hover-bg", Root, BuiltinCss),
    hover_var("--color-taskbar-button-urgent-hover-bg", Root, BuiltinCss),
    hover_var("--color-widget-hover-bg", Root, BuiltinCss),
    hover_var(
        "--color-workspace-indicator-active-hover-bg",
        Root,
        BuiltinCss,
    ),
    hover_var(
        "--color-workspace-indicator-hover-bg",
        UserHook,
        OptionalBuiltinCss,
    ),
    var(
        "--color-workspace-indicator-hover-default-bg",
        Root,
        BuiltinCss,
    ),
    hover_var(
        "--color-workspace-indicator-urgent-hover-bg",
        Root,
        BuiltinCss,
    ),
    var("--font-family", Root, BuiltinCss),
    var("--font-scale", Root, Alias),
    var("--font-size", Root, BuiltinCss),
    var("--font-size-base", Root, BuiltinCss),
    var("--font-size-lg", Root, BuiltinCss),
    var("--font-size-md", Root, BuiltinCss),
    var("--font-size-sm", Root, BuiltinCss),
    var("--font-size-xs", Root, BuiltinCss),
    var("--icon-size", Root, BuiltinCss),
    var("--outline-color", Root, Alias),
    var("--outline-opacity", Root, Alias),
    var("--outline-width", Root, Alias),
    var("--popover-background-opacity", Root, BuiltinCss),
    var("--radius-bar", Root, BuiltinCss),
    var("--radius-card", Root, BuiltinCss),
    var("--radius-factor", Root, Alias),
    var("--radius-pill", Root, BuiltinCss),
    var("--radius-round", Root, BuiltinCss),
    var("--radius-surface", Root, BuiltinCss),
    var("--radius-widget", Root, BuiltinCss),
    var("--radius-widget-lg", Root, RustComposedCss),
    var("--shadow-soft", Root, BuiltinCss),
    var("--slider-height", Root, BuiltinCss),
    var("--slider-height-thick", Root, BuiltinCss),
    var("--slider-knob-radius", Root, BuiltinCss),
    var("--slider-knob-size", Root, BuiltinCss),
    var("--slider-radius", Root, BuiltinCss),
    var("--slider-radius-thick", Root, BuiltinCss),
    var("--spacing-widget-gap", Root, BuiltinCss),
    var("--surface-outline-color", Root, BuiltinCss),
    var("--surface-outline-opacity", Root, BuiltinCss),
    var("--surface-outline-width", Root, BuiltinCss),
    var("--widget-background-color", Root, BuiltinCss),
    var("--widget-background-opacity", Root, BuiltinCss),
    var("--widget-height", Root, BuiltinCss),
    var("--widget-hover-tint", Root, BuiltinCss),
    var("--widget-outline-color", Root, BuiltinCss),
    var("--widget-outline-opacity", Root, BuiltinCss),
    var("--widget-outline-width", Root, BuiltinCss),
    var("--widget-padding-y", Root, BuiltinCss),
];

const fn var(name: &'static str, scope: ThemeVarScope, role: ThemeVarRole) -> ThemeVarExpectation {
    ThemeVarExpectation {
        name,
        scope,
        role,
        hover_binding: false,
    }
}

const fn hover_var(
    name: &'static str,
    scope: ThemeVarScope,
    role: ThemeVarRole,
) -> ThemeVarExpectation {
    ThemeVarExpectation {
        name,
        scope,
        role,
        hover_binding: true,
    }
}
