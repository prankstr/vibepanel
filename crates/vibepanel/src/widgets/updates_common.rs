//! Shared UI helpers for the Updates widget and Quick Settings card.
//!
//! This module provides:
//! - Tooltip formatting with repo breakdown
//! - Terminal detection and spawning
//! - Icon name helpers
//! - Time formatting utilities

use std::process::{Command, Stdio};
use std::thread;
use std::time::SystemTime;

use gtk4::glib;
use tracing::{debug, error, warn};

use crate::services::updates::{PackageManager, UpdatesService, UpdatesSnapshot, has_flatpak};

/// Get the appropriate icon name based on snapshot state.
pub fn icon_for_state(snapshot: &UpdatesSnapshot) -> &'static str {
    if snapshot.error.is_some() {
        "software-update-urgent"
    } else {
        "software-update-available"
    }
}

/// Format a complete tooltip from a snapshot.
///
/// Example output:
/// ```text
/// 12 updates available
/// core: 3
/// extra: 5
/// aur: 4
///
/// Last check: 5 minutes ago
/// ```
pub fn format_tooltip(snapshot: &UpdatesSnapshot) -> String {
    let mut lines = Vec::new();

    // Header
    if let Some(ref err) = snapshot.error {
        lines.push("Update check failed".to_string());
        lines.push(String::new());
        lines.push(format!("Error: {}", err));
    } else if snapshot.checking {
        lines.push("Checking for updates...".to_string());
    } else if snapshot.update_count == 0 {
        lines.push("System is up to date".to_string());
    } else {
        let s = if snapshot.update_count == 1 { "" } else { "s" };
        lines.push(format!("{} update{} available", snapshot.update_count, s));
    }

    // Repo breakdown (only if updates are available)
    if snapshot.update_count > 0 && snapshot.error.is_none() {
        // Sort repos for consistent display
        let mut repos: Vec<_> = snapshot.updates_by_repo.iter().collect();
        repos.sort_by_key(|(name, _)| *name);

        for (repo, updates) in repos {
            lines.push(format!("{}: {}", repo, updates.len()));
        }
    }

    // Last check time
    if snapshot.error.is_none() {
        lines.push(String::new());
        lines.push(format!(
            "Last check: {}",
            format_last_check(snapshot.last_check)
        ));
    } else if snapshot.last_check.is_some() {
        lines.push(String::new());
        lines.push(format!(
            "Last successful check: {}",
            format_last_check(snapshot.last_check)
        ));
    }

    lines.join("\n")
}

/// Format a repo summary for subtitle text.
///
/// Example: "12 updates" or "1 update"
pub fn format_repo_summary(snapshot: &UpdatesSnapshot) -> String {
    if snapshot.error.is_some() {
        return "Check failed".to_string();
    }

    if snapshot.checking {
        return "Checking...".to_string();
    }

    if snapshot.update_count == 0 {
        return "Up to date".to_string();
    }

    let s = if snapshot.update_count == 1 { "" } else { "s" };
    format!("{} update{}", snapshot.update_count, s)
}

/// Format a human-readable "last checked" string.
pub fn format_last_check(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "Never".to_string();
    };

    let Ok(elapsed) = time.elapsed() else {
        return "Unknown".to_string();
    };

    let secs = elapsed.as_secs();

    if secs < 60 {
        "Just now".to_string()
    } else if secs < 3600 {
        let mins = secs / 60;
        let s = if mins == 1 { "" } else { "s" };
        format!("{} minute{} ago", mins, s)
    } else if secs < 86400 {
        let hours = secs / 3600;
        let s = if hours == 1 { "" } else { "s" };
        format!("{} hour{} ago", hours, s)
    } else {
        let days = secs / 86400;
        let s = if days == 1 { "" } else { "s" };
        format!("{} day{} ago", days, s)
    }
}

/// List of terminal emulators to try, in priority order.
const TERMINALS: &[&str] = &[
    "ghostty",
    "foot",
    "alacritty",
    "kitty",
    "wezterm",
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "xterm",
];

/// Detect an available terminal emulator.
pub fn detect_terminal() -> Option<String> {
    // First check $TERMINAL environment variable
    if let Ok(term) = std::env::var("TERMINAL") {
        let term = term.trim();
        if !term.is_empty() {
            // Check if it exists
            let name = term.split('/').next_back().unwrap_or(term);
            if which_exists(name) || std::path::Path::new(term).exists() {
                debug!("Using terminal from $TERMINAL: {}", term);
                return Some(term.to_string());
            }
        }
    }

    // Try each known terminal
    for term in TERMINALS {
        if which_exists(term) {
            debug!("Detected terminal: {}", term);
            return Some((*term).to_string());
        }
    }

    warn!("No terminal emulator found");
    None
}

/// Check if a command exists in PATH using `which`.
fn which_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn a terminal with the upgrade command.
///
/// Returns an error message if spawning fails.
pub fn spawn_upgrade_terminal(
    package_manager: PackageManager,
    terminal_override: Option<&str>,
) -> Result<(), String> {
    let terminal = terminal_override
        .map(String::from)
        .or_else(detect_terminal)
        .ok_or_else(|| "No terminal emulator found".to_string())?;

    let upgrade_cmd = build_upgrade_command(package_manager);

    // Build the shell command that runs upgrade and waits for user input
    let shell_cmd = format!(
        "{}; echo ''; echo 'Press Enter to close...'; read",
        upgrade_cmd
    );

    debug!(
        "Spawning terminal '{}' with command: {}",
        terminal, upgrade_cmd
    );

    // Get the terminal name (without path)
    let term_name = terminal.split('/').next_back().unwrap_or(&terminal);

    // Build command based on terminal type
    let result = match term_name {
        // Terminals that don't need -e flag
        "foot" | "kitty" => Command::new(&terminal)
            .args(["sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),

        // gnome-terminal uses -- instead of -e
        "gnome-terminal" => Command::new(&terminal)
            .args(["--", "sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),

        // wezterm has its own syntax
        "wezterm" => Command::new(&terminal)
            .args(["start", "--", "sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),

        // Most terminals use -e
        _ => Command::new(&terminal)
            .args(["-e", "sh", "-c", &shell_cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
    };

    match result {
        Ok(mut child) => {
            debug!("Terminal spawned successfully, watching for exit");

            // Spawn a thread to wait for the terminal to exit, then refresh
            thread::spawn(move || {
                match child.wait() {
                    Ok(status) => {
                        debug!("Terminal exited with status: {}", status);
                        // Schedule refresh on main thread
                        glib::idle_add_once(|| {
                            debug!("Triggering updates refresh after terminal exit");
                            UpdatesService::global().refresh();
                        });
                    }
                    Err(e) => {
                        error!("Failed to wait for terminal process: {}", e);
                    }
                }
            });

            Ok(())
        }
        Err(e) => {
            error!("Failed to spawn terminal '{}': {}", terminal, e);
            Err(format!("Failed to spawn terminal: {}", e))
        }
    }
}

/// Build the shell command used for update execution.
///
/// If Flatpak exists and the primary manager is not Flatpak, append a Flatpak
/// update pass so one click upgrades both system packages and Flatpaks.
fn build_upgrade_command(package_manager: PackageManager) -> String {
    let primary = package_manager.upgrade_command();

    if package_manager != PackageManager::Flatpak && has_flatpak() {
        // Use `;` rather than `&&` so flatpak updates still run even if the
        // primary package manager fails
        format!(
            "{}; echo ''; echo 'Running Flatpak updates...'; flatpak update",
            primary
        )
    } else {
        primary.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_snapshot(updates: Vec<(&str, Vec<&str>)>) -> UpdatesSnapshot {
        let mut by_repo = HashMap::new();
        let mut count = 0;

        for (repo, pkgs) in updates {
            let infos: Vec<_> = pkgs
                .iter()
                .map(|name| crate::services::updates::UpdateInfo {
                    name: name.to_string(),
                })
                .collect();
            count += infos.len();
            by_repo.insert(repo.to_string(), infos);
        }

        UpdatesSnapshot {
            available: true,
            is_ready: true,
            checking: false,
            error: None,
            update_count: count,
            updates_by_repo: by_repo,
            last_check: Some(SystemTime::now()),
            package_manager: Some(PackageManager::Paru),
        }
    }

    #[test]
    fn test_format_tooltip_with_updates() {
        let snapshot = make_snapshot(vec![
            ("official", vec!["linux", "firefox", "systemd"]),
            ("aur", vec!["paru", "vscode"]),
        ]);

        let tooltip = format_tooltip(&snapshot);

        assert!(tooltip.contains("5 updates available"));
        assert!(tooltip.contains("aur: 2"));
        assert!(tooltip.contains("official: 3"));
        assert!(tooltip.contains("Last check:"));
    }

    #[test]
    fn test_format_tooltip_no_updates() {
        let snapshot = UpdatesSnapshot {
            available: true,
            is_ready: true,
            checking: false,
            error: None,
            update_count: 0,
            updates_by_repo: HashMap::new(),
            last_check: Some(SystemTime::now()),
            package_manager: Some(PackageManager::Paru),
        };

        let tooltip = format_tooltip(&snapshot);
        assert!(tooltip.contains("up to date"));
    }

    #[test]
    fn test_format_tooltip_error() {
        let snapshot = UpdatesSnapshot {
            available: true,
            is_ready: true,
            checking: false,
            error: Some("Network error".to_string()),
            update_count: 0,
            updates_by_repo: HashMap::new(),
            last_check: None,
            package_manager: Some(PackageManager::Paru),
        };

        let tooltip = format_tooltip(&snapshot);
        assert!(tooltip.contains("failed"));
        assert!(tooltip.contains("Network error"));
    }

    #[test]
    fn test_format_repo_summary() {
        let snapshot = make_snapshot(vec![
            ("official", vec!["linux", "firefox"]),
            ("aur", vec!["paru"]),
        ]);

        let summary = format_repo_summary(&snapshot);
        assert_eq!(summary, "3 updates");
    }

    #[test]
    fn test_format_repo_summary_single_repo() {
        let snapshot = make_snapshot(vec![("official", vec!["linux", "firefox"])]);

        let summary = format_repo_summary(&snapshot);
        assert_eq!(summary, "2 updates");
    }

    #[test]
    fn test_format_repo_summary_single_update() {
        let snapshot = make_snapshot(vec![("official", vec!["linux"])]);

        let summary = format_repo_summary(&snapshot);
        assert_eq!(summary, "1 update");
    }

    #[test]
    fn test_format_last_check() {
        assert_eq!(format_last_check(None), "Never");

        // Just now
        let now = SystemTime::now();
        let result = format_last_check(Some(now));
        assert!(result.contains("Just now") || result.contains("minute"));
    }

    #[test]
    fn test_icon_for_state() {
        let mut snapshot = make_snapshot(vec![]);
        assert_eq!(icon_for_state(&snapshot), "software-update-available");

        snapshot.error = Some("test".to_string());
        assert_eq!(icon_for_state(&snapshot), "software-update-urgent");
    }

    #[test]
    fn test_build_upgrade_command_flatpak_primary() {
        let cmd = build_upgrade_command(PackageManager::Flatpak);
        assert_eq!(cmd, "flatpak update");
    }

    #[test]
    fn test_build_upgrade_command_non_flatpak_contains_primary() {
        // NOTE: The exact output depends on whether /usr/bin/flatpak exists on
        // the test machine. We only verify the primary command prefix.
        let cmd = build_upgrade_command(PackageManager::Paru);
        assert!(cmd.starts_with("paru -Syu"));
    }
}
