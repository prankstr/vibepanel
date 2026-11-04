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
use tr::tr;
use tracing::{debug, error, warn};

use crate::services::config_manager::ConfigManager;
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
        lines.push(tr!("Update check failed"));
        lines.push(String::new());
        lines.push(tr!("Error: {error}", error = err));
    } else if snapshot.checking {
        lines.push(tr!("Checking for updates..."));
    } else if snapshot.update_count == 0 {
        lines.push(tr!("System is up to date"));
    } else {
        let count = snapshot.update_count as u32;
        lines.push(tr!("{n} update available" | "{n} updates available" % count));
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
        lines.push(tr!(
            "Last check: {last_check}",
            last_check = format_last_check(snapshot.last_check)
        ));
    } else if snapshot.last_check.is_some() {
        lines.push(String::new());
        lines.push(tr!(
            "Last successful check: {last_check}",
            last_check = format_last_check(snapshot.last_check)
        ));
    }

    lines.join("\n")
}

/// Format a repo summary for subtitle text.
///
/// Example: "12 updates" or "1 update"
pub fn format_repo_summary(snapshot: &UpdatesSnapshot) -> String {
    if snapshot.error.is_some() {
        return tr!("Check failed");
    }

    if snapshot.checking {
        return tr!("Checking...");
    }

    if snapshot.update_count == 0 {
        return tr!("Up to date");
    }

    let count = snapshot.update_count as u32;
    tr!("{n} update" | "{n} updates" % count)
}

/// Format a human-readable "last checked" string.
pub fn format_last_check(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return tr!("Never");
    };

    let Ok(elapsed) = time.elapsed() else {
        return tr!("Unknown");
    };

    let secs = elapsed.as_secs();

    if secs < 60 {
        tr!("Just now")
    } else if secs < 3600 {
        let mins = (secs / 60) as u32;
        tr!("{n} minute ago" | "{n} minutes ago" % mins)
    } else if secs < 86400 {
        let hours = (secs / 3600) as u32;
        tr!("{n} hour ago" | "{n} hours ago" % hours)
    } else {
        let days = (secs / 86400) as u32;
        tr!("{n} day ago" | "{n} days ago" % days)
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

/// Wrap a command so the terminal stays open until the user presses Enter.
///
/// Uses a newline separator (not `;`) so user commands ending in `;`, `&`,
/// a newline, or a `# comment` still parse and still reach the prompt.
fn wrap_for_terminal(cmd: &str) -> String {
    format!("{cmd}\necho ''; echo 'Press Enter to close...'; read")
}

/// Spawn a terminal with the upgrade command.
///
/// Reads `terminal` and `update_command` from `[widgets.updates]` at click
/// time so the bar widget and the Quick Settings card behave identically.
/// A custom `update_command` is preferred over the built-in command.
///
/// Returns an error message if spawning fails.
pub fn spawn_upgrade_terminal(package_manager: Option<PackageManager>) -> Result<(), String> {
    let config = ConfigManager::global();
    let option = |name| {
        config
            .get_widget_option("updates", name)
            .and_then(|v| v.as_str().map(String::from))
    };

    let upgrade_cmd = match (option("update_command"), package_manager) {
        (Some(cmd), _) => cmd,
        (None, Some(pm)) => build_upgrade_command(pm),
        (None, None) => return Err("No package manager detected".to_string()),
    };

    let terminal = option("terminal")
        .or_else(detect_terminal)
        .ok_or_else(|| "No terminal emulator found".to_string())?;

    let shell_cmd = wrap_for_terminal(&upgrade_cmd);

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
    use std::time::Duration;

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
            check_status: None,
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
            check_status: None,
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
            check_status: None,
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

        // Ages sit mid-bucket so the time spent running the test can't cross a threshold.
        let now = SystemTime::now();
        for (age_secs, expected) in [
            (5, "Just now"),
            (90, "1 minute ago"),
            (30 * 60, "30 minutes ago"),
            (2 * 3600 + 60, "2 hours ago"),
            (3 * 86400 + 60, "3 days ago"),
        ] {
            let time = now - Duration::from_secs(age_secs);
            assert_eq!(format_last_check(Some(time)), expected, "age {age_secs}s");
        }
        assert_eq!(
            format_last_check(Some(now + Duration::from_secs(3600))),
            "Unknown",
            "a check time in the future"
        );
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

    #[test]
    fn test_wrap_for_terminal_survives_user_command_endings() {
        for cmd in ["true;", "true\n", "true &", "true # comment"] {
            let output = Command::new("sh")
                .args(["-c", &wrap_for_terminal(cmd)])
                .stdin(Stdio::null())
                .output()
                .expect("sh should run");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Press Enter to close..."),
                "prompt missing for {cmd:?}: stdout={stdout:?} stderr={:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
