//! vibepanel - A modern Wayland status bar
//!
//! This is the main entry point for the vibepanel bar application.

mod bar;
pub mod layout_math;
pub mod popover_tracker;
mod sectioned_bar;
mod services;
pub mod styles;
mod widgets;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gtk4::Application;
use gtk4::prelude::*;
use tracing::{debug, error, info, warn};

use services::bar_manager;
use vibepanel_core::{Config, ThemePalette, logging};

use crate::services::bar_manager::BarManager;
use crate::services::compositor::CompositorManager;
use crate::services::config_manager::ConfigManager;

/// vibepanel - A modern Wayland status bar
#[derive(Parser, Debug)]
#[command(name = "vibepanel", version, about, long_about = None)]
struct Args {
    /// Path to the configuration file (uses XDG lookup if not specified)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Increase verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Print example configuration and exit
    #[arg(long)]
    print_example_config: bool,

    /// Validate configuration and exit (returns non-zero on errors)
    #[arg(long)]
    check_config: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Control screen brightness
    Brightness {
        #[command(subcommand)]
        action: BrightnessAction,
    },
    /// Control audio volume
    Volume {
        #[command(subcommand)]
        action: VolumeAction,
    },
    /// Run a command with idle/sleep inhibited
    Inhibit {
        /// Reason for inhibiting (shown in system monitors)
        #[arg(short, long, default_value = "User requested")]
        reason: String,
        /// Command to run (idle inhibited while running)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Control media playback (MPRIS)
    Media {
        #[command(subcommand)]
        action: MediaAction,
    },
}

#[derive(Subcommand, Debug)]
enum BrightnessAction {
    /// Get current brightness percentage
    Get,
    /// Set brightness to a specific percentage (0-100)
    Set {
        /// Brightness percentage (0-100)
        #[arg(value_parser = clap::value_parser!(u32).range(0..=100))]
        percent: u32,
    },
    /// Increase brightness by a percentage (default: 5)
    Inc {
        /// Amount to increase (default: 5)
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Decrease brightness by a percentage (default: 5)
    Dec {
        /// Amount to decrease (default: 5)
        #[arg(default_value = "5")]
        amount: u32,
    },
}

#[derive(Subcommand, Debug)]
enum VolumeAction {
    /// Get current volume percentage
    Get,
    /// Set volume to a specific percentage (0-150)
    Set {
        /// Volume percentage (0-150, values above 100 are overdrive)
        #[arg(value_parser = clap::value_parser!(u32).range(0..=150))]
        percent: u32,
    },
    /// Increase volume by a percentage (default: 5)
    Inc {
        /// Amount to increase (default: 5)
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Decrease volume by a percentage (default: 5)
    Dec {
        /// Amount to decrease (default: 5)
        #[arg(default_value = "5")]
        amount: u32,
    },
    /// Mute audio
    Mute,
    /// Unmute audio
    Unmute,
    /// Toggle mute state
    ToggleMute,
}

#[derive(Subcommand, Debug)]
enum MediaAction {
    /// Toggle play/pause
    PlayPause,
    /// Skip to next track
    Next,
    /// Go to previous track
    Previous,
    /// Stop playback
    Stop,
    /// Show current playback status
    Status,
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Initialize logging
    logging::init(args.verbose);

    // Handle subcommands (these don't need config or GTK)
    if let Some(command) = args.command {
        return handle_command(command);
    }

    // Load configuration using XDG lookup chain
    // If --config is specified, it must exist and be valid (no fallback)
    let load_result = match Config::find_and_load(args.config.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Error: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if let Some(ref source) = load_result.source {
        info!("Loaded configuration from {:?}", source);
    } else if load_result.used_defaults {
        warn!("Using default configuration (no config file found)");
    }

    let config = load_result.config;

    // Validate configuration (strict - fail on invalid values)
    if let Err(e) = config.validate() {
        eprintln!("Error: {}", e);
        return ExitCode::FAILURE;
    }

    debug!("Configuration validated successfully");

    // --check-config: just validate and exit
    if args.check_config {
        if let Some(ref source) = load_result.source {
            println!("Configuration valid: {}", source.display());
        } else {
            println!("Configuration valid (using defaults)");
        }
        return ExitCode::SUCCESS;
    }

    // --print-example-config: print the example config with comments
    if args.print_example_config {
        print!("{}", vibepanel_core::config::DEFAULT_CONFIG_TOML);
        return ExitCode::SUCCESS;
    }

    info!("Configuration loaded successfully");
    info!("Bar size: {}px", config.bar.size);
    info!(
        "Widgets: {} left, {} center, {} right",
        config.widgets.left.len(),
        config.widgets.center.len(),
        config.widgets.right.len()
    );

    // Run the GTK application
    run_gtk_app(config, load_result.source)
}

/// Handle CLI subcommands (brightness, volume, etc.)
fn handle_command(command: Command) -> ExitCode {
    match command {
        Command::Brightness { action } => handle_brightness_command(action),
        Command::Volume { action } => handle_volume_command(action),
        Command::Inhibit { reason, command } => handle_inhibit_command(&reason, &command),
        Command::Media { action } => handle_media_command(action),
    }
}

/// Handle brightness subcommands using direct sysfs/logind access.
fn handle_brightness_command(action: BrightnessAction) -> ExitCode {
    use crate::services::brightness::BrightnessCli;

    let cli = match BrightnessCli::new() {
        Some(c) => c,
        None => {
            eprintln!(
                "Error: no backlight device found (is this a laptop with a supported backlight?)"
            );
            return ExitCode::FAILURE;
        }
    };

    match action {
        BrightnessAction::Get => {
            println!("{}", cli.get_percent());
            ExitCode::SUCCESS
        }
        BrightnessAction::Set { percent } => {
            if let Err(e) = cli.set_percent(percent) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        BrightnessAction::Inc { amount } => {
            let current = cli.get_percent();
            let new_value = (current + amount).min(100);
            if let Err(e) = cli.set_percent(new_value) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                println!("{}", new_value);
                ExitCode::SUCCESS
            }
        }
        BrightnessAction::Dec { amount } => {
            let current = cli.get_percent();
            let new_value = current.saturating_sub(amount).max(1);
            if let Err(e) = cli.set_percent(new_value) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                println!("{}", new_value);
                ExitCode::SUCCESS
            }
        }
    }
}

/// Handle volume subcommands using PulseAudio.
fn handle_volume_command(action: VolumeAction) -> ExitCode {
    use crate::services::audio::AudioCli;
    use crate::services::osd_ipc::{notify_volume, notify_volume_unavailable};

    /// Check if an error indicates the audio sink is unavailable for control.
    /// This covers sinks that aren't ready (0 channels, invalid specs, etc.)
    fn is_sink_unavailable_error(error: &str) -> bool {
        error.contains("not ready") || error.contains("no channels")
    }

    let mut cli = match AudioCli::new() {
        Some(c) => c,
        None => {
            eprintln!(
                "Error: could not connect to PulseAudio (is PulseAudio/pipewire-pulse running?)"
            );
            return ExitCode::FAILURE;
        }
    };

    match action {
        VolumeAction::Get => {
            println!("{}", cli.get_volume());
            ExitCode::SUCCESS
        }
        VolumeAction::Set { percent } => {
            match cli.set_volume(percent) {
                Ok(()) => {
                    notify_volume(percent, cli.is_muted());
                    ExitCode::SUCCESS
                }
                Err(e) if is_sink_unavailable_error(&e) => {
                    // Sink is suspended/unavailable
                    notify_volume_unavailable();
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        VolumeAction::Inc { amount } => {
            let current = cli.get_volume();
            let new_value = (current + amount).min(150);
            match cli.set_volume(new_value) {
                Ok(()) => {
                    notify_volume(new_value, cli.is_muted());
                    println!("{}", new_value);
                    ExitCode::SUCCESS
                }
                Err(e) if is_sink_unavailable_error(&e) => {
                    notify_volume_unavailable();
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        VolumeAction::Dec { amount } => {
            let current = cli.get_volume();
            let new_value = current.saturating_sub(amount);
            match cli.set_volume(new_value) {
                Ok(()) => {
                    notify_volume(new_value, cli.is_muted());
                    println!("{}", new_value);
                    ExitCode::SUCCESS
                }
                Err(e) if is_sink_unavailable_error(&e) => {
                    notify_volume_unavailable();
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        VolumeAction::Mute => {
            if let Err(e) = cli.set_muted(true) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                notify_volume(cli.get_volume(), true);
                ExitCode::SUCCESS
            }
        }
        VolumeAction::Unmute => {
            if let Err(e) = cli.set_muted(false) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                notify_volume(cli.get_volume(), false);
                ExitCode::SUCCESS
            }
        }
        VolumeAction::ToggleMute => {
            let muted = cli.is_muted();
            if let Err(e) = cli.set_muted(!muted) {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                notify_volume(cli.get_volume(), !muted);
                println!("{}", if !muted { "muted" } else { "unmuted" });
                ExitCode::SUCCESS
            }
        }
    }
}

/// Handle inhibit subcommand - run a command with idle/sleep inhibited.
fn handle_inhibit_command(reason: &str, command: &[String]) -> ExitCode {
    use crate::services::idle_inhibitor::IdleInhibitorCli;
    use std::process::Command as ProcessCommand;

    if command.is_empty() {
        eprintln!("Error: no command specified");
        return ExitCode::FAILURE;
    }

    // Acquire the inhibit lock
    let _inhibitor = match IdleInhibitorCli::new(reason) {
        Some(i) => i,
        None => {
            eprintln!("Error: could not acquire idle inhibitor (is systemd-logind running?)");
            return ExitCode::FAILURE;
        }
    };

    // Run the command while holding the inhibit lock
    let program = &command[0];
    let args = &command[1..];

    let status = ProcessCommand::new(program).args(args).status();

    match status {
        Ok(exit_status) => {
            if exit_status.success() {
                ExitCode::SUCCESS
            } else {
                // Return the same exit code as the child process
                ExitCode::from(exit_status.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            eprintln!("Error: failed to run command '{}': {}", program, e);
            ExitCode::FAILURE
        }
    }
    // _inhibitor is dropped here, releasing the lock
}

/// Handle media subcommands using MPRIS D-Bus.
fn handle_media_command(action: MediaAction) -> ExitCode {
    use crate::services::media::MediaCli;

    let cli = match MediaCli::new() {
        Some(c) => c,
        None => {
            eprintln!("Error: could not connect to D-Bus session bus");
            return ExitCode::FAILURE;
        }
    };

    match action {
        MediaAction::PlayPause => {
            if let Err(e) = cli.play_pause() {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        MediaAction::Next => {
            if let Err(e) = cli.next() {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        MediaAction::Previous => {
            if let Err(e) = cli.previous() {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        MediaAction::Stop => {
            if let Err(e) = cli.stop() {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        MediaAction::Status => match cli.status() {
            Ok(status) => {
                println!("{}", status);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                ExitCode::FAILURE
            }
        },
    }
}

/// Initialize and run the GTK4 application.
fn run_gtk_app(config: Config, config_source: Option<PathBuf>) -> ExitCode {
    // Log the config source for diagnostics
    if let Some(ref source) = config_source {
        info!("Running with configuration file: {}", source.display());
    } else {
        info!("Running with default configuration (no file found)");
    }

    // Initialize the config manager singleton (before GTK, so it's ready for hot-reload)
    ConfigManager::init_global(config.clone(), config_source.clone());

    // Initialize the compositor manager singleton with advanced config
    // This must happen after ConfigManager but before GTK widgets are created
    CompositorManager::init_global(&config.advanced);

    // Default to Wayland backend
    // SAFETY: This is called before GTK initialization, and we're setting a
    // well-known environment variable. No other threads are accessing env vars yet.
    if std::env::var("GDK_BACKEND").is_err() {
        unsafe {
            std::env::set_var("GDK_BACKEND", "wayland");
        }
    }

    let app = Application::builder()
        .application_id("io.github.vibepanel")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    // Clone config for the activate closure
    let config_for_activate = config.clone();

    app.connect_activate(move |app| {
        info!("GTK application activated");

        // Load CSS styling
        bar::load_css(&config_for_activate);

        // Initialize theming services with config values
        // IconsService must be initialized before widgets are created
        services::icons::IconsService::init_global(
            &config_for_activate.theme.icons.theme,
            config_for_activate.theme.icons.weight,
        );
        debug!(
            "Icons service initialized with theme: {}, weight: {}",
            config_for_activate.theme.icons.theme, config_for_activate.theme.icons.weight
        );

        // Initialize theming-related services with theme-derived styles
        let palette = ThemePalette::from_config(&config_for_activate);
        let surface_styles = palette.surface_styles();
        services::surfaces::SurfaceStyleManager::init_global_with_config(
            surface_styles.clone(),
            config_for_activate.advanced.pango_font_rendering,
        );
        debug!(
            "Surface style manager initialized with theme styles (pango_font_rendering={})",
            config_for_activate.advanced.pango_font_rendering
        );
        services::tooltip::TooltipManager::init_global(surface_styles);
        debug!("Tooltip manager initialized with theme styles");

        // Initialize idle inhibitor service (uses D-Bus ScreenSaver API)
        let _ = services::idle_inhibitor::IdleInhibitorService::global();
        debug!("Idle inhibitor service initialized");

        // Get the display for monitor enumeration
        let display = match gtk4::gdk::Display::default() {
            Some(d) => d,
            None => {
                error!("Could not get default display - is a display server running?");
                return;
            }
        };

        // Initialize bar manager and sync bars to current monitors
        let bar_manager = BarManager::global();
        bar_manager.init(app);
        bar_manager.sync_monitors(&display, &config_for_activate);

        info!(
            "Bar(s) created: {} bar(s) with {} widget handle(s)",
            bar_manager.bar_count(),
            bar_manager.handle_count()
        );

        // Connect monitor change signals for hot-plug support.
        // We capture the display directly so sync_monitors is called unconditionally,
        // even when monitors.n_items() == 0 (all monitors disconnected). This ensures
        // bars for removed monitors are properly cleaned up.
        //
        // We connect to both `items_changed` and `notify::n-items` because some
        // Wayland compositors/GTK4 versions don't reliably emit `items_changed`.
        {
            let config_for_hotplug = config_for_activate.clone();
            let display_for_hotplug = display.clone();
            display
                .monitors()
                .connect_items_changed(move |_monitors, _pos, _removed, _added| {
                    info!("Monitor configuration changed (items_changed), syncing...");
                    // Hide all bars immediately to prevent them from appearing
                    // on the wrong monitor during compositor surface reassignment.
                    BarManager::global().hide_all();
                    bar_manager::sync_monitors_when_ready(
                        &display_for_hotplug,
                        &config_for_hotplug,
                    );
                });
        }
        {
            let config_for_hotplug = config_for_activate.clone();
            let display_for_hotplug = display.clone();
            display
                .monitors()
                .connect_notify_local(Some("n-items"), move |_monitors, _| {
                    info!("Monitor count changed (notify::n-items), syncing...");
                    // Hide all bars immediately to prevent them from appearing
                    // on the wrong monitor during compositor surface reassignment.
                    BarManager::global().hide_all();
                    bar_manager::sync_monitors_when_ready(
                        &display_for_hotplug,
                        &config_for_hotplug,
                    );
                });
        }

        // Create OSD overlay if enabled and keep it alive on the application
        if config_for_activate.osd.enabled {
            let overlay = crate::widgets::OsdOverlay::new(app, &config_for_activate.osd);
            // Attach to the application so the Rc stays alive for the
            // lifetime of the app.
            unsafe {
                app.set_data("vibepanel-osd-overlay", overlay);
            }
            debug!("OSD overlay initialized and attached to application");
        } else {
            debug!("OSD overlay disabled via configuration");
        }

        // Start config file watcher for live reload
        ConfigManager::global().start_watching();
    });

    app.connect_startup(|_| {
        info!("GTK application starting up");
    });

    app.connect_shutdown(|_| {
        info!("GTK application shutting down");
        // Stop config watcher
        ConfigManager::global().stop_watching();
    });

    // Run the application with empty args (we already parsed with clap)
    let empty_args: Vec<String> = vec![];
    let status = app.run_with_args(&empty_args);

    if status == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        error!("GTK application exited with error");
        ExitCode::FAILURE
    }
}
