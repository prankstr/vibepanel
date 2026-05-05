//! vibepanel - A modern Wayland status bar
//!
//! This is the main entry point for the vibepanel bar application.

mod bar;
pub mod layout_math;
pub mod popover_registry;
pub mod popover_tracker;
mod sectioned_bar;
mod services;
pub mod styles;
mod widgets;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gtk4::Application;
use gtk4::prelude::*;
use tracing::{debug, error, info, warn};

use services::audio::AudioService;
use services::bar_manager;
use vibepanel_core::{Config, logging};

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
    /// Control idle/sleep inhibitor
    Inhibit {
        #[command(subcommand)]
        action: InhibitAction,
    },
    /// Control media playback (MPRIS)
    Media {
        #[command(subcommand)]
        action: MediaAction,
    },
    /// Control bar visibility
    Bar {
        #[command(subcommand)]
        action: BarAction,
    },
    /// Control widget popovers
    Popover {
        #[command(subcommand)]
        action: PopoverAction,
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
    /// Set volume to a specific percentage
    Set {
        /// Volume percentage, clamped to 100% unless audio overdrive is enabled in readable config
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

#[derive(Subcommand, Debug)]
enum InhibitAction {
    /// Toggle idle/sleep inhibitor on the running panel
    Toggle,
}

#[derive(Subcommand, Debug)]
enum BarAction {
    /// Show the bar
    Show,
    /// Hide the bar (releases exclusive zone)
    Hide,
    /// Toggle bar visibility
    Toggle,
}

#[derive(Subcommand, Debug)]
enum PopoverAction {
    /// Show a widget's popover
    Show {
        /// Widget name (e.g., clock, battery, quick-settings)
        widget: String,
    },
    /// Hide a popover (dismiss active if no widget specified)
    Hide {
        /// Widget name (optional — hides active popover if omitted)
        widget: Option<String>,
    },
    /// Toggle a widget's popover
    Toggle {
        /// Widget name (e.g., clock, battery, quick-settings)
        widget: String,
    },
}

fn main() -> ExitCode {
    let mut args = Args::parse();

    // Initialize logging
    logging::init(args.verbose);

    // CLI subcommands don't need GTK. Volume only needs the audio policy and can
    // fall back safely if unrelated config is broken.
    if let Some(command) = args.command.take() {
        return handle_command(command, args.config.as_deref());
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
fn handle_command(command: Command, config_path: Option<&Path>) -> ExitCode {
    match command {
        Command::Brightness { action } => handle_brightness_command(action),
        Command::Volume { action } => {
            handle_volume_command(action, Config::read_audio_allow_overdrive(config_path))
        }
        Command::Inhibit { action } => handle_inhibit_command(action),
        Command::Media { action } => handle_media_command(action),
        Command::Bar { action } => handle_bar_command(action),
        Command::Popover { action } => handle_popover_command(action),
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
///
/// Volume commands are standalone media-key friendly operations: config errors
/// must not fail the command, so unreadable policy safely caps controls at 100%.
fn handle_volume_command(action: VolumeAction, allow_overdrive: bool) -> ExitCode {
    use crate::services::audio::{AudioCli, volume_user_max_percent};
    use crate::services::ipc::{notify_volume, notify_volume_unavailable};

    /// Check if an error indicates the audio sink is unavailable for control.
    /// This covers sinks that aren't ready (0 channels, invalid specs, etc.)
    fn is_sink_unavailable_error(error: &str) -> bool {
        error.contains("not ready") || error.contains("no channels")
    }

    let user_max_percent = volume_user_max_percent(allow_overdrive);
    let mut cli = match AudioCli::new(user_max_percent) {
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
                    notify_volume(cli.get_volume(), cli.is_muted());
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
            let delta = amount.min(i32::MAX as u32) as i32;
            match cli.set_volume_relative(delta) {
                Ok(()) => {
                    let actual = cli.get_volume();
                    notify_volume(actual, cli.is_muted());
                    println!("{}", actual);
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
            let delta = -(amount.min(i32::MAX as u32) as i32);
            match cli.set_volume_relative(delta) {
                Ok(()) => {
                    let actual = cli.get_volume();
                    notify_volume(actual, cli.is_muted());
                    println!("{}", actual);
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

/// Handle inhibit subcommand.
fn handle_inhibit_command(action: InhibitAction) -> ExitCode {
    use crate::services::ipc::{IpcMessage, send_ipc_message};

    match action {
        InhibitAction::Toggle => {
            let msg = IpcMessage::ToggleInhibitor;
            match send_ipc_message(&msg) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!(
                        "Error: could not reach vibepanel IPC socket (is the panel running?): {}",
                        e
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
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

/// Handle bar subcommands (show/hide/toggle) via IPC.
fn handle_bar_command(action: BarAction) -> ExitCode {
    use crate::services::ipc::{BarIpcAction, IpcMessage, send_ipc_message};

    let ipc_action = match action {
        BarAction::Show => BarIpcAction::Show,
        BarAction::Hide => BarIpcAction::Hide,
        BarAction::Toggle => BarIpcAction::Toggle,
    };
    let msg = IpcMessage::Bar { action: ipc_action };
    match send_ipc_message(&msg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "Error: could not reach vibepanel IPC socket (is the panel running?): {}",
                e
            );
            ExitCode::FAILURE
        }
    }
}

/// Handle popover subcommands (show/hide/toggle) via IPC.
fn handle_popover_command(action: PopoverAction) -> ExitCode {
    use crate::services::ipc::{IpcMessage, PopoverIpcAction, send_ipc_message};

    let ipc_action = match action {
        PopoverAction::Show { widget } => PopoverIpcAction::Show(widget),
        PopoverAction::Hide { widget } => PopoverIpcAction::Hide(widget),
        PopoverAction::Toggle { widget } => PopoverIpcAction::Toggle(widget),
    };
    let msg = IpcMessage::Popover { action: ipc_action };
    match send_ipc_message(&msg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "Error: could not reach vibepanel IPC socket (is the panel running?): {}",
                e
            );
            ExitCode::FAILURE
        }
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

    // Default to Wayland backend
    // SAFETY: This is called before GTK initialization and before any service
    // initialization that may spawn worker threads (for example AudioService).
    // No other threads are accessing env vars yet.
    if std::env::var("GDK_BACKEND").is_err() {
        unsafe {
            std::env::set_var("GDK_BACKEND", "wayland");
        }
    }

    // Initialize the config manager singleton (before GTK, so it's ready for hot-reload)
    ConfigManager::init_global(config.clone(), config_source.clone());
    AudioService::global().set_allow_overdrive(config.audio.allow_overdrive);

    // Initialize the compositor manager singleton with advanced config
    // This must happen after ConfigManager but before GTK widgets are created
    CompositorManager::init_global(&config.advanced);

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
        let palette = ConfigManager::global().palette();
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

        // Initialize idle inhibitor service (uses logind D-Bus API)
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

        // Initialize background effect (blur) service unconditionally so that
        // hot-reloading `theme.blur = true` at runtime works.  The service is
        // cheap when the compositor lacks ext-background-effect support, and
        // call-sites already gate on `blur_enabled()`.
        services::background_effect::BackgroundEffectManager::init_global();
        debug!("Background effect (blur) service initialized");

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
        //
        // Both handlers share a debounce timer: on each signal we hide bars
        // immediately (to avoid wrong-monitor rendering) and schedule
        // reconfiguration after 300ms. This gives the compositor time to fully
        // register new outputs in its IPC before show_if commands query it.
        let debounce_source: std::rc::Rc<std::cell::Cell<Option<gtk4::glib::SourceId>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        {
            let config_for_hotplug = config_for_activate.clone();
            let display_for_hotplug = display.clone();
            let debounce = debounce_source.clone();
            display
                .monitors()
                .connect_items_changed(move |_monitors, _pos, _removed, _added| {
                    info!("Monitor configuration changed (items_changed), syncing...");
                    BarManager::global().hide_all();
                    // Cancel any pending debounce from a previous signal
                    if let Some(source) = debounce.take() {
                        source.remove();
                    }
                    let display = display_for_hotplug.clone();
                    let config = config_for_hotplug.clone();
                    let debounce_clear = debounce.clone();
                    debounce.set(Some(gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(300),
                        move || {
                            // Clear stale SourceId — one-shot timers auto-remove
                            // from the main loop, so the id is invalid after firing.
                            debounce_clear.take();
                            bar_manager::sync_monitors_when_ready(&display, &config);
                        },
                    )));
                });
        }
        {
            let config_for_hotplug = config_for_activate.clone();
            let display_for_hotplug = display.clone();
            let debounce = debounce_source;
            display
                .monitors()
                .connect_notify_local(Some("n-items"), move |_monitors, _| {
                    info!("Monitor count changed (notify::n-items), syncing...");
                    BarManager::global().hide_all();
                    // Cancel any pending debounce from a previous signal
                    if let Some(source) = debounce.take() {
                        source.remove();
                    }
                    let display = display_for_hotplug.clone();
                    let config = config_for_hotplug.clone();
                    let debounce_clear = debounce.clone();
                    debounce.set(Some(gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(300),
                        move || {
                            // Clear stale SourceId — one-shot timers auto-remove
                            // from the main loop, so the id is invalid after firing.
                            debounce_clear.take();
                            bar_manager::sync_monitors_when_ready(&display, &config);
                        },
                    )));
                });
        }

        // Initialize panel IPC listener unconditionally (handles CLI → panel messages).
        // This must be created regardless of OSD enabled state so that
        // `vibepanel inhibit` and other CLI commands always work.
        {
            use crate::services::ipc::{IpcListener, IpcMessage};

            let osd_enabled = config_for_activate.osd.enabled;
            let osd_overlay = if osd_enabled {
                let overlay = crate::widgets::OsdOverlay::new(app, &config_for_activate.osd);
                debug!("OSD overlay initialized");
                Some(overlay)
            } else {
                debug!("OSD overlay disabled via configuration");
                None
            };

            if let Some(listener) = IpcListener::new() {
                let osd_for_ipc = osd_overlay.clone();

                listener.borrow().connect(move |msg| {
                    match &msg {
                        IpcMessage::ToggleInhibitor => {
                            debug!("IPC: toggling idle inhibitor");
                            services::idle_inhibitor::IdleInhibitorService::global().toggle();
                        }
                        // OSD-visual messages: forward to OSD overlay if enabled.
                        IpcMessage::Volume { .. }
                        | IpcMessage::VolumeUnavailable
                        | IpcMessage::Brightness { .. } => {
                            if let Some(ref overlay) = osd_for_ipc {
                                overlay.handle_ipc_message(&msg);
                            }
                        }
                        IpcMessage::Bar { action } => {
                            use crate::services::ipc::BarIpcAction;
                            let manager = BarManager::global();
                            match action {
                                BarIpcAction::Show => manager.ipc_show(),
                                BarIpcAction::Hide => manager.ipc_hide(),
                                BarIpcAction::Toggle => manager.ipc_toggle(),
                            }
                        }
                        IpcMessage::Popover { action } => {
                            use crate::popover_registry::{self as registry, DispatchAction};
                            use crate::services::ipc::PopoverIpcAction;
                            match action {
                                PopoverIpcAction::Show(name) => {
                                    registry::dispatch(name, DispatchAction::Show);
                                }
                                PopoverIpcAction::Hide(None) => {
                                    crate::popover_tracker::PopoverTracker::global()
                                        .dismiss_active();
                                }
                                PopoverIpcAction::Hide(Some(name)) => {
                                    registry::dispatch(name, DispatchAction::Hide);
                                }
                                PopoverIpcAction::Toggle(name) => {
                                    registry::dispatch(name, DispatchAction::Toggle);
                                }
                            }
                        }
                    }
                });

                // Attach listener to the application so it stays alive.
                // SAFETY: Key is unique to vibepanel; stored type matches what
                // would be retrieved. The data keeps the Rc alive for the
                // application's lifetime.
                unsafe {
                    app.set_data("vibepanel-ipc-listener", listener);
                }
                debug!("IPC listener initialized and attached to application");
            }

            // Attach OSD overlay to the application so the Rc stays alive.
            if let Some(overlay) = osd_overlay {
                // SAFETY: Key is unique to vibepanel; stored type matches what
                // would be retrieved. The data keeps the Rc alive for the
                // application's lifetime.
                unsafe {
                    app.set_data("vibepanel-osd-overlay", overlay);
                }
                debug!("OSD overlay attached to application");
            }
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
