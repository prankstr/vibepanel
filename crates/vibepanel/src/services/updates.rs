//! UpdatesService - shared, event-driven package update state.
//!
//! This service provides:
//! - Auto-detection of package managers (dnf, pacman, paru, flatpak)
//! - Periodic checking for available updates
//! - Background thread execution to avoid blocking the UI
//! - Grouped updates by repository
//!
//! Supports:
//! - Fedora: dnf
//! - Arch Linux: pacman (official repos), paru (official + AUR)
//! - Universal: flatpak

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::SystemTime;
use tr::tr;

use gtk4::glib::{self, SourceId};
use tracing::{debug, info, warn};

use super::callbacks::{CallbackId, Callbacks};
use super::network::{NetworkService, NetworkSnapshot};

/// Default check interval in seconds (1 hour).
const DEFAULT_CHECK_INTERVAL: u64 = 3600;

/// Minimum check interval to prevent abuse (5 minutes).
const MIN_CHECK_INTERVAL: u64 = 300;

/// Supported package managers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// Fedora's DNF package manager.
    Dnf,
    /// Arch Linux's pacman (official repos only).
    Pacman,
    /// Arch Linux's paru (official repos + AUR).
    Paru,
    /// Flatpak package manager.
    Flatpak,
}

impl PackageManager {
    /// Get the upgrade command for this package manager.
    pub fn upgrade_command(&self) -> &'static str {
        match self {
            Self::Dnf => "sudo dnf upgrade --refresh",
            Self::Pacman => "sudo pacman -Syu",
            Self::Paru => "paru -Syu",
            Self::Flatpak => "flatpak update",
        }
    }
}

/// Information about a single package update.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Package name.
    pub name: String,
}

/// Canonical snapshot of update state.
#[derive(Debug, Clone)]
pub struct UpdatesSnapshot {
    /// Whether a package manager was detected.
    pub available: bool,
    /// Whether the initial check has completed.
    pub is_ready: bool,
    /// Whether a check is currently in progress.
    pub checking: bool,
    /// Human-readable status during an active check (e.g. "Checking dnf...").
    pub check_status: Option<String>,
    /// Last error message, if any.
    pub error: Option<String>,
    /// Total number of available updates.
    pub update_count: usize,
    /// Updates grouped by repository name.
    pub updates_by_repo: HashMap<String, Vec<UpdateInfo>>,
    /// Time of the last successful check.
    pub last_check: Option<SystemTime>,
    /// Detected package manager.
    pub package_manager: Option<PackageManager>,
}

impl UpdatesSnapshot {
    /// Create an initial "unknown" snapshot.
    pub fn unknown() -> Self {
        Self {
            available: false,
            is_ready: false,
            checking: false,
            check_status: None,
            error: None,
            update_count: 0,
            updates_by_repo: HashMap::new(),
            last_check: None,
            package_manager: None,
        }
    }
}

/// Result of a background update check.
#[derive(Debug)]
struct CheckResult {
    updates_by_repo: HashMap<String, Vec<UpdateInfo>>,
    error: Option<String>,
}

/// Shared, process-wide updates service.
pub struct UpdatesService {
    snapshot: RefCell<UpdatesSnapshot>,
    callbacks: Callbacks<UpdatesSnapshot>,
    check_interval: Cell<u64>,
    timer_source: RefCell<Option<SourceId>>,
    /// Network subscription for a scheduled check that was skipped offline.
    network_wait: RefCell<Option<CallbackId>>,
    /// Prevent concurrent checks.
    check_in_progress: Cell<bool>,
}

impl UpdatesService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            snapshot: RefCell::new(UpdatesSnapshot::unknown()),
            callbacks: Callbacks::new(),
            check_interval: Cell::new(DEFAULT_CHECK_INTERVAL),
            timer_source: RefCell::new(None),
            network_wait: RefCell::new(None),
            check_in_progress: Cell::new(false),
        });

        // Detect package manager
        let pm = detect_package_manager();
        {
            let mut snapshot = service.snapshot.borrow_mut();
            snapshot.package_manager = pm;
            snapshot.available = pm.is_some();
        }

        if pm.is_some() {
            info!("UpdatesService: detected package manager {:?}", pm);
            // Start initial check and periodic timer
            Self::start_periodic_checks(&service);
        } else {
            info!("UpdatesService: no supported package manager detected");
            let mut snapshot = service.snapshot.borrow_mut();
            snapshot.is_ready = true;
        }

        service
    }

    /// Get the global UpdatesService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<UpdatesService> = UpdatesService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked whenever the snapshot changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&UpdatesSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        // Immediately notify with current snapshot
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Return the current snapshot.
    pub fn snapshot(&self) -> UpdatesSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Trigger an immediate update check.
    pub fn refresh(&self) {
        if !self.snapshot.borrow().available {
            return;
        }
        if !is_online(&NetworkService::global().snapshot()) {
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.error = Some("Offline — check skipped".to_string());
            let clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&clone);
            return;
        }
        self.check_updates_async();
    }

    /// Update the human-readable check status and notify listeners.
    ///
    /// Called from the main thread (via `glib::idle_add_once`) while a
    /// background check is in progress.
    fn set_check_status(&self, status: &str) {
        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.check_status = Some(status.to_string());
        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }

    /// Set the check interval in seconds, restarting the periodic timer if
    /// one is running.
    pub fn set_check_interval(self: &Rc<Self>, seconds: u64) {
        let seconds = seconds.max(MIN_CHECK_INTERVAL);
        if self.check_interval.replace(seconds) == seconds {
            return;
        }
        debug!("UpdatesService: check interval set to {}s", seconds);

        let old_timer = self.timer_source.borrow_mut().take();
        if let Some(source_id) = old_timer {
            source_id.remove();
            Self::schedule_periodic_checks(self);
        }
    }

    /// Start periodic update checks.
    fn start_periodic_checks(this: &Rc<Self>) {
        Self::run_scheduled_check(this);
        Self::schedule_periodic_checks(this);
    }

    fn schedule_periodic_checks(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);
        let interval = this.check_interval.get();

        let source_id = glib::timeout_add_seconds_local(interval as u32, move || {
            if let Some(this) = this_weak.upgrade() {
                Self::run_scheduled_check(&this);
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });

        *this.timer_source.borrow_mut() = Some(source_id);
    }

    /// Run a startup or timer check, or defer it until the network is up.
    ///
    /// The network state is loaded asynchronously, so at startup it reads as
    /// offline even when it isn't.
    fn run_scheduled_check(this: &Rc<Self>) {
        if is_online(&NetworkService::global().snapshot()) {
            this.check_updates_async();
            return;
        }
        if this.network_wait.borrow().is_some() {
            return;
        }
        debug!("UpdatesService: offline, checking once the network is up");

        let this_weak = Rc::downgrade(this);
        let id = NetworkService::global().connect(move |net| {
            if is_online(net)
                && let Some(this) = this_weak.upgrade()
                && let Some(id) = this.network_wait.take()
            {
                NetworkService::global().unsubscribe(id);
                this.check_updates_async();
            }
        });
        *this.network_wait.borrow_mut() = Some(id);
    }

    /// Perform an async update check in a background thread.
    fn check_updates_async(&self) {
        // Prevent concurrent checks
        if self.check_in_progress.get() {
            debug!("UpdatesService: check already in progress, skipping");
            return;
        }

        if !is_online(&NetworkService::global().snapshot()) {
            debug!("UpdatesService: offline, skipping update check");
            return;
        }

        let pm = match self.snapshot.borrow().package_manager {
            Some(pm) => pm,
            None => return,
        };

        self.check_in_progress.set(true);

        // Mark as checking
        {
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.checking = true;
            let snapshot_clone = snapshot.clone();
            drop(snapshot);
            self.callbacks.notify(&snapshot_clone);
        }

        debug!("UpdatesService: starting update check with {:?}", pm);

        // Spawn background thread
        std::thread::spawn(move || {
            let report_status = |status: String| {
                glib::idle_add_once(move || {
                    UpdatesService::global().set_check_status(&status);
                });
            };

            let result = run_update_check(pm, &report_status);

            // Send result back to main thread
            glib::idle_add_once(move || {
                UpdatesService::global().apply_check_result(result);
            });
        });
    }

    /// Apply the result of a background check.
    fn apply_check_result(&self, result: CheckResult) {
        self.check_in_progress.set(false);

        let mut snapshot = self.snapshot.borrow_mut();
        snapshot.checking = false;
        snapshot.check_status = None;
        snapshot.is_ready = true;

        if let Some(err) = result.error {
            warn!("UpdatesService: check failed: {}", err);
            snapshot.error = Some(err);
            // Keep previous update data on error
        } else {
            snapshot.error = None;
            snapshot.updates_by_repo = result.updates_by_repo;
            snapshot.update_count = snapshot.updates_by_repo.values().map(|v| v.len()).sum();
            snapshot.last_check = Some(SystemTime::now());

            debug!(
                "UpdatesService: found {} updates across {} repos",
                snapshot.update_count,
                snapshot.updates_by_repo.len()
            );
        }

        let snapshot_clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&snapshot_clone);
    }
}

fn is_online(net: &NetworkSnapshot) -> bool {
    net.connected() || net.wired_connected() || net.mobile_active()
}

impl Drop for UpdatesService {
    fn drop(&mut self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

/// Detect the available package manager.
///
/// Detection order:
/// 1. paru (Arch + AUR)
/// 2. dnf (Fedora)
/// 3. pacman (Arch official only)
/// 4. flatpak (cross-distro)
fn detect_package_manager() -> Option<PackageManager> {
    // Check for paru first (implies Arch + AUR support)
    if Path::new("/usr/bin/paru").exists() {
        return Some(PackageManager::Paru);
    }

    // Check for dnf (Fedora)
    if Path::new("/usr/bin/dnf").exists() {
        return Some(PackageManager::Dnf);
    }

    // Check for pacman (Arch without AUR helper)
    if Path::new("/usr/bin/pacman").exists() {
        return Some(PackageManager::Pacman);
    }

    // Check for flatpak (cross-distro sandboxed apps)
    if has_flatpak() {
        return Some(PackageManager::Flatpak);
    }

    None
}

/// Whether Flatpak is available on the system.
pub(crate) fn has_flatpak() -> bool {
    Path::new("/usr/bin/flatpak").exists()
}

/// Run the update check for the given package manager.
///
/// This runs in a background thread and should not touch any GTK state.
/// `report_status` is called before each step to update the UI with progress.
fn run_update_check(pm: PackageManager, report_status: &dyn Fn(String)) -> CheckResult {
    let mut result = match pm {
        PackageManager::Dnf => check_dnf_updates(report_status),
        PackageManager::Pacman => check_pacman_updates(report_status),
        PackageManager::Paru => check_paru_updates(report_status),
        PackageManager::Flatpak => check_flatpak_updates(report_status),
    };

    // Append flatpak updates when a different primary manager is detected
    if pm != PackageManager::Flatpak && has_flatpak() {
        report_status(tr!("Checking {source}...", source = "flatpak"));
        if let Err(e) = append_flatpak_updates(&mut result.updates_by_repo) {
            debug!("Flatpak update check failed (ignored): {}", e);
        }
    }

    result
}

/// Check for updates using DNF (Fedora).
///
/// Split into two phases so the UI can show meaningful progress:
/// 1. `dnf makecache --refresh` — refreshes repo metadata (slow, network-bound)
/// 2. `dnf upgrade --assumeno` — checks for available upgrades against fresh cache
///
/// Uses `dnf upgrade --assumeno` which performs full dependency resolution,
/// giving accurate results that match what `dnf upgrade` will actually do.
/// This avoids false positives from `check-update` when packages from
/// higher-priority repos (like COPRs) are already installed.
fn check_dnf_updates(report_status: &dyn Fn(String)) -> CheckResult {
    // Phase 1: Refresh repo metadata (this is the slow, network-bound part)
    report_status(tr!("Refreshing dnf cache..."));
    match dnf_command().args(["makecache", "--refresh"]).output() {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return CheckResult {
                updates_by_repo: HashMap::new(),
                error: Some(format!("Failed to refresh repos: {}", stderr.trim())),
            };
        }
        Err(e) => {
            return CheckResult {
                updates_by_repo: HashMap::new(),
                error: Some(format!("Failed to run dnf makecache: {}", e)),
            };
        }
        _ => {}
    }

    // Phase 2: Check for available upgrades against fresh cache
    report_status(tr!("Checking for upgrades..."));
    let output = dnf_command().args(["upgrade", "--assumeno"]).output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);

            // --assumeno exits 1 after declining a transaction, so that alone isn't a failure.
            if !output.status.success() && !stdout.contains("Transaction Summary:") {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let reason = stderr.lines().rev().find(|l| !l.trim().is_empty());
                return CheckResult {
                    updates_by_repo: HashMap::new(),
                    error: Some(format!(
                        "dnf upgrade failed: {}",
                        reason.unwrap_or("unknown error").trim()
                    )),
                };
            }

            CheckResult {
                updates_by_repo: parse_dnf_upgrade_output(&stdout),
                error: None,
            }
        }
        Err(e) => CheckResult {
            updates_by_repo: HashMap::new(),
            error: Some(format!("Failed to run dnf: {}", e)),
        },
    }
}

/// `dnf` with English output, since the parser matches dnf's section headings.
fn dnf_command() -> Command {
    let mut cmd = Command::new("dnf");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd
}

/// Parse DNF upgrade --assumeno output.
///
/// Format:
/// ```text
/// Package                   Arch    Version           Repository      Size
/// Upgrading:
///  package-name             arch    version           repo            size
///    replacing package-name arch    version           @System         size
///  another-package          arch    version           repo            size
///
/// Transaction Summary:
///  Upgrading:         2 packages
/// ```
///
/// We parse the "Upgrading:" section to extract package names and repos.
/// Sections can follow each other without a blank line, so any unindented
/// heading ends it.
fn parse_dnf_upgrade_output(output: &str) -> HashMap<String, Vec<UpdateInfo>> {
    let mut by_repo: HashMap<String, Vec<UpdateInfo>> = HashMap::new();
    let mut in_upgrading_section = false;

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            in_upgrading_section = false;
            continue;
        }

        if !line.starts_with(char::is_whitespace) && trimmed.ends_with(':') {
            in_upgrading_section = trimmed == "Upgrading:";
            continue;
        }

        if !in_upgrading_section || trimmed.starts_with("replacing") {
            continue;
        }

        // Parse package line: "package-name  arch  version  repo  size"
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 4 {
            let name = parts[0].to_string();
            let repo = parts[3].to_string();

            let update = UpdateInfo { name };
            by_repo.entry(repo).or_default().push(update);
        }
    }

    by_repo
}

/// Check for updates using pacman (Arch official repos).
fn check_pacman_updates(report_status: &dyn Fn(String)) -> CheckResult {
    report_status(tr!("Checking {source}...", source = "pacman"));

    // Try checkupdates first (from pacman-contrib), fall back to pacman -Qu
    let output = Command::new("checkupdates").output();

    let output = match output {
        Ok(o) if o.status.success() || o.status.code() == Some(2) => o,
        _ => {
            // Fallback to pacman -Qu
            match Command::new("pacman").args(["-Qu"]).output() {
                Ok(o) => o,
                Err(e) => {
                    return CheckResult {
                        updates_by_repo: HashMap::new(),
                        error: Some(format!("Failed to run pacman: {}", e)),
                    };
                }
            }
        }
    };

    // checkupdates returns exit code 2 if no updates (not an error)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let updates = parse_checkupdates_output(&stdout);

    // For pacman, we don't have repo info, so group under "official"
    let mut by_repo = HashMap::new();
    if !updates.is_empty() {
        by_repo.insert("official".to_string(), updates);
    }

    CheckResult {
        updates_by_repo: by_repo,
        error: None,
    }
}

/// Check for updates using paru (Arch + AUR).
fn check_paru_updates(report_status: &dyn Fn(String)) -> CheckResult {
    let mut by_repo: HashMap<String, Vec<UpdateInfo>> = HashMap::new();

    // Check official repos with checkupdates
    report_status(tr!("Checking official repos..."));
    let official_output = Command::new("checkupdates").output();

    match official_output {
        Ok(output) if output.status.success() || output.status.code() == Some(2) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let updates = parse_checkupdates_output(&stdout);
            if !updates.is_empty() {
                by_repo.insert("official".to_string(), updates);
            }
        }
        Ok(output) => {
            // checkupdates failed, try pacman -Qu as fallback
            debug!(
                "checkupdates failed with code {:?}, trying pacman -Qu",
                output.status.code()
            );
            if let Ok(fallback) = Command::new("pacman").args(["-Qu"]).output() {
                let stdout = String::from_utf8_lossy(&fallback.stdout);
                let updates = parse_checkupdates_output(&stdout);
                if !updates.is_empty() {
                    by_repo.insert("official".to_string(), updates);
                }
            }
        }
        Err(e) => {
            debug!("checkupdates not available: {}, trying pacman -Qu", e);
            // checkupdates not installed, try pacman -Qu
            if let Ok(fallback) = Command::new("pacman").args(["-Qu"]).output() {
                let stdout = String::from_utf8_lossy(&fallback.stdout);
                let updates = parse_checkupdates_output(&stdout);
                if !updates.is_empty() {
                    by_repo.insert("official".to_string(), updates);
                }
            }
        }
    }

    // Check AUR with paru -Qua
    report_status(tr!("Checking {source}...", source = "AUR"));
    let aur_output = Command::new("paru").args(["-Qua"]).output();

    match aur_output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let updates = parse_checkupdates_output(&stdout);
            if !updates.is_empty() {
                by_repo.insert("aur".to_string(), updates);
            }
        }
        Ok(output) => {
            // Exit code 1 usually means no AUR updates, which is fine
            if output.status.code() != Some(1) {
                debug!("paru -Qua returned code {:?}", output.status.code());
            }
        }
        Err(e) => {
            return CheckResult {
                updates_by_repo: by_repo,
                error: Some(format!("Failed to run paru: {}", e)),
            };
        }
    }

    CheckResult {
        updates_by_repo: by_repo,
        error: None,
    }
}

/// Check for updates using Flatpak.
fn check_flatpak_updates(report_status: &dyn Fn(String)) -> CheckResult {
    report_status(tr!("Checking {source}...", source = "flatpak"));

    let mut by_repo = HashMap::new();
    match append_flatpak_updates(&mut by_repo) {
        Ok(()) => CheckResult {
            updates_by_repo: by_repo,
            error: None,
        },
        Err(e) => CheckResult {
            updates_by_repo: HashMap::new(),
            error: Some(e),
        },
    }
}

/// Append Flatpak updates under the `flatpak` repo key.
fn append_flatpak_updates(by_repo: &mut HashMap<String, Vec<UpdateInfo>>) -> Result<(), String> {
    let output = Command::new("flatpak")
        .args(["remote-ls", "--updates", "--columns=application"])
        .output()
        .map_err(|e| format!("Failed to run flatpak: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "flatpak remote-ls failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let updates = parse_flatpak_updates_output(&stdout);
    if !updates.is_empty() {
        by_repo.insert("flatpak".to_string(), updates);
    }

    Ok(())
}

/// Parse checkupdates/pacman -Qu output.
///
/// Format: `package-name oldversion -> newversion`
fn parse_checkupdates_output(output: &str) -> Vec<UpdateInfo> {
    let mut updates = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Try to parse "name oldver -> newver" format
        if let Some((name_old, _new_ver)) = line.split_once(" -> ") {
            let parts: Vec<&str> = name_old.split_whitespace().collect();
            if !parts.is_empty() {
                updates.push(UpdateInfo {
                    name: parts[0].to_string(),
                });
                continue;
            }
        }

        // Fallback: just take the first word as the package name
        if let Some(name) = line.split_whitespace().next() {
            updates.push(UpdateInfo {
                name: name.to_string(),
            });
        }
    }

    updates
}

/// Parse `flatpak remote-ls --updates --columns=application` output.
///
/// Format: one Flatpak application/runtime ID per line.
/// Duplicates are removed because the same ID can appear for both system and
/// user installations.
fn parse_flatpak_updates_output(output: &str) -> Vec<UpdateInfo> {
    let mut seen = HashSet::new();
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|name| seen.insert(*name))
        .map(|name| UpdateInfo {
            name: name.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dnf_upgrade_output() {
        let output = r#"
Updating and loading repositories:
Repositories loaded.
Package                   Arch    Version           Repository      Size
Upgrading:
 kernel                   x86_64  6.5.0-1.fc39      updates         100 MiB
   replacing kernel       x86_64  6.4.0-1.fc39      @System         100 MiB
 firefox                  x86_64  119.0-1.fc39      updates          90 MiB
 mesa-libGL               x86_64  23.2.1-2.fc39     fedora           10 MiB

Transaction Summary:
 Upgrading:         3 packages

Total size of inbound packages is 200 MiB.
Operation aborted by the user.
"#;

        let result = parse_dnf_upgrade_output(output);

        assert!(result.contains_key("updates"));
        assert!(result.contains_key("fedora"));

        let updates = &result["updates"];
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].name, "kernel");
        assert_eq!(updates[1].name, "firefox");

        let fedora = &result["fedora"];
        assert_eq!(fedora.len(), 1);
        assert_eq!(fedora[0].name, "mesa-libGL");
    }

    #[test]
    fn test_parse_dnf_upgrade_output_ignores_adjacent_sections() {
        let output = r#"
Upgrading:
 fzf                      aarch64 0:0.74.5-1.fc44   updates         4.9 MiB
   replacing fzf          aarch64 0:0.74.4-1.fc44   updates         4.9 MiB
Skipping packages with conflicts:
 chezmoi                  aarch64 2.73.0-1.fc44     updates        38.0 MiB
"#;

        let result = parse_dnf_upgrade_output(output);

        let names: Vec<&str> = result.values().flatten().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["fzf"]);
    }

    #[test]
    fn test_parse_dnf_upgrade_output_nothing_to_do() {
        let output = r#"
Updating and loading repositories:
Repositories loaded.
Nothing to do.
"#;

        let result = parse_dnf_upgrade_output(output);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_checkupdates_output() {
        let output = r#"
linux 6.5.9.arch2-1 -> 6.6.1.arch1-1
linux-headers 6.5.9.arch2-1 -> 6.6.1.arch1-1
firefox 119.0-1 -> 120.0-1
"#;

        let result = parse_checkupdates_output(output);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "linux");
        assert_eq!(result[2].name, "firefox");
    }

    #[test]
    fn test_parse_empty_output() {
        let dnf_result = parse_dnf_upgrade_output("");
        assert!(dnf_result.is_empty());

        let pacman_result = parse_checkupdates_output("");
        assert!(pacman_result.is_empty());

        let flatpak_result = parse_flatpak_updates_output("");
        assert!(flatpak_result.is_empty());
    }

    #[test]
    fn test_parse_flatpak_updates_output() {
        let output = r#"
org.mozilla.firefox
org.gnome.Calculator
org.freedesktop.Platform
"#;

        let result = parse_flatpak_updates_output(output);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "org.mozilla.firefox");
        assert_eq!(result[2].name, "org.freedesktop.Platform");
    }

    #[test]
    fn test_parse_flatpak_updates_output_dedup() {
        // Same ID can appear for both system and user installations
        let output = "org.mozilla.firefox\norg.gnome.Calculator\norg.mozilla.firefox\n";
        let result = parse_flatpak_updates_output(output);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "org.mozilla.firefox");
        assert_eq!(result[1].name, "org.gnome.Calculator");
    }

    #[test]
    fn test_package_manager_upgrade_command() {
        assert_eq!(
            PackageManager::Dnf.upgrade_command(),
            "sudo dnf upgrade --refresh"
        );
        assert_eq!(PackageManager::Pacman.upgrade_command(), "sudo pacman -Syu");
        assert_eq!(PackageManager::Paru.upgrade_command(), "paru -Syu");
        assert_eq!(PackageManager::Flatpak.upgrade_command(), "flatpak update");
    }
}
