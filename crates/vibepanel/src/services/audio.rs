//! AudioService - PulseAudio/PipeWire audio monitoring and control.
//!
//! Provides a GTK/GLib-friendly, callback-based API for:
//! - Monitoring default sink volume and mute state
//! - Monitoring default source (mic) mute state
//! - Enumerating available sinks for quick settings
//! - Setting volume/mute with efficient handling of rapid changes
//!
//! Uses `libpulse-binding` for native PulseAudio protocol access, which
//! works seamlessly with PipeWire's `pipewire-pulse` compatibility layer
//! on most modern Wayland desktops.
//!
//! Architecture:
//! - A background thread runs the PulseAudio threaded mainloop
//! - State updates are sent to the GTK main loop via `glib::idle_add_once()`
//!   which wakes the main loop immediately (no polling required)
//! - Volume/mute commands are sent to the background thread via `std::sync::mpsc`

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use parking_lot::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use gtk4::glib;
use tracing::{debug, error, info, warn};

use libpulse_binding as pulse;

use super::callbacks::Callbacks;

/// Duration (in ms) after connecting to PulseAudio during which the OSD
/// should stay quiet. PulseAudio/PipeWire emits a flurry of updates as
/// devices are discovered and defaults are resolved.
const INITIAL_SETTLE_MS: u64 = 200;
use pulse::callbacks::ListResult;
use pulse::context::introspect::SinkInfo;
use pulse::context::subscribe::{Facility, InterestMaskSet, Operation as SubscribeOp};
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::def::PortAvailable;
use pulse::mainloop::threaded::Mainloop;
use pulse::proplist::Proplist;
use pulse::volume::Volume;

/// Information about an audio sink (output device).
#[derive(Debug, Clone)]
pub struct SinkInfoSnapshot {
    /// Internal PulseAudio name (used for set-default-sink).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this is the current default sink.
    pub is_default: bool,
    /// Whether the sink's active port is available (e.g., headphones plugged in).
    /// `None` if the sink doesn't support jack detection or has no ports.
    /// `Some(false)` means the port is not available (e.g., headphones unplugged).
    /// `Some(true)` means the port is available.
    pub port_available: Option<bool>,
}

/// Information about an audio source (input device).
#[derive(Debug, Clone)]
pub struct SourceInfoSnapshot {
    /// Internal PulseAudio name (used for set-default-source).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this is the current default source.
    pub is_default: bool,
    /// Whether the source's active port is available (e.g., mic plugged in).
    /// `None` if the source doesn't support jack detection or has no ports.
    /// `Some(false)` means the port is not available.
    /// `Some(true)` means the port is available.
    pub port_available: Option<bool>,
}

/// Snapshot of audio service state for callbacks.
#[derive(Debug, Clone)]
pub struct AudioSnapshot {
    /// Current volume as a percentage (0–150, allowing overdrive).
    pub volume: u32,
    /// Whether the default sink is muted.
    pub muted: bool,
    /// Whether the default source (mic) is muted, if available.
    pub mic_muted: Option<bool>,
    /// Current mic volume as a percentage (0–150), if available.
    pub mic_volume: Option<u32>,
    /// List of available sinks.
    pub sinks: Vec<SinkInfoSnapshot>,
    /// Name of the current default sink.
    pub default_sink_name: Option<String>,
    /// List of available sources (input devices).
    pub sources: Vec<SourceInfoSnapshot>,
    /// Name of the current default source.
    pub default_source_name: Option<String>,
    /// Whether the audio backend is available and connected.
    pub available: bool,
    /// Whether volume/mute controls are currently functional.
    /// False when the sink has invalid channels (e.g., Asahi Linux after reboot
    /// before any audio producer has connected to the sink).
    pub control_available: bool,
    /// Whether mic volume/mute controls are currently functional.
    pub mic_control_available: bool,
}

impl Default for AudioSnapshot {
    fn default() -> Self {
        Self {
            volume: 0,
            muted: false,
            mic_muted: None,
            mic_volume: None,
            sinks: Vec::new(),
            default_sink_name: None,
            sources: Vec::new(),
            default_source_name: None,
            available: false,
            control_available: true, // Optimistic default; updated when sink info arrives
            mic_control_available: true,
        }
    }
}

impl AudioSnapshot {
    /// Convenience accessor for the current volume percentage.
    #[allow(dead_code)]
    pub fn volume(&self) -> u32 {
        self.volume
    }

    /// Whether the default sink is muted.
    #[allow(dead_code)]
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Whether the audio backend is available.
    #[allow(dead_code)]
    pub fn is_available(&self) -> bool {
        self.available
    }
}

/// Commands sent from the main thread to the Pulse worker thread.
///
/// These commands are processed asynchronously by the PulseAudio worker thread.
/// Volume operations are clamped to [0, 150] before being sent.
#[derive(Debug)]
enum AudioCommand {
    /// Set volume to an absolute percentage (0–150).
    ///
    /// Values are clamped to [0, 150] before sending. Values above 100 represent
    /// overdrive/amplification. The command is silently ignored if no default
    /// sink is available or if the sink's control is unavailable (e.g., 0 channels).
    SetVolume(u32),

    /// Adjust volume relative to the current level.
    ///
    /// The delta is added to the current volume percentage. The result is clamped
    /// to [0, 150]. Positive values increase volume, negative values decrease it.
    /// Example: `SetVolumeRelative(-5)` decreases volume by 5 percentage points.
    SetVolumeRelative(i32),

    /// Set the mute state for the default audio output sink.
    ///
    /// Pass `true` to mute, `false` to unmute. The UI is notified immediately
    /// for responsiveness, before the PulseAudio operation completes.
    SetMuted(bool),

    /// Toggle the mute state for the default audio output sink.
    ///
    /// If currently muted, unmutes; if currently unmuted, mutes.
    /// The UI is notified immediately for responsiveness.
    ToggleMute,

    /// Set microphone volume to an absolute percentage (0–150).
    ///
    /// Values are clamped to [0, 150] before sending. Silently ignored if
    /// no default source is available or if mic control is unavailable.
    SetMicVolume(u32),

    /// Set the mute state for the default audio input source (microphone).
    ///
    /// Pass `true` to mute, `false` to unmute. The UI is notified immediately
    /// for responsiveness, before the PulseAudio operation completes.
    SetMicMuted(bool),

    /// Toggle the mute state for the default audio input source (microphone).
    ///
    /// If currently muted, unmutes; if currently unmuted, mutes.
    ToggleMicMute,

    /// Set the default audio output sink by its PulseAudio name.
    ///
    /// The name should match the `name` field from `SinkInfoSnapshot`.
    /// A server event will trigger a full state refresh after the change.
    SetDefaultSink(String),

    /// Set the default audio input source by its PulseAudio name.
    ///
    /// The name should match the `name` field from `SourceInfoSnapshot`.
    /// A server event will trigger a full state refresh after the change.
    SetDefaultSource(String),

    /// Request a full state refresh from PulseAudio.
    ///
    /// Fetches server info, sink list, source list, and default device details.
    /// Useful for recovering from missed events or forcing a sync.
    Refresh,

    /// Record that an external tool requested a volume change.
    ///
    /// This is used for behavioral detection: when volume changes are made
    /// via external tools (e.g., `pactl`, `wpctl`, WM keybinds) that bypass
    /// the AudioService, we track the "expected" value so the behavioral
    /// heuristic can detect if the backend is ignoring changes.
    ///
    /// The OSD IPC handler calls this when receiving volume messages from
    /// external sources. Does not actually change volume—the external tool
    /// already did that.
    NoteExternalVolumeRequest(u32),

    /// Shut down the worker thread gracefully.
    ///
    /// Disconnects from PulseAudio and exits the worker loop. Called when
    /// `AudioService` is dropped.
    Shutdown,
}

/// Internal state update sent from the Pulse thread to the main thread.
#[derive(Debug, Clone)]
struct AudioStateUpdate {
    volume: u32,
    muted: bool,
    mic_muted: Option<bool>,
    mic_volume: Option<u32>,
    sinks: Vec<SinkInfoSnapshot>,
    default_sink_name: Option<String>,
    sources: Vec<SourceInfoSnapshot>,
    default_source_name: Option<String>,
    available: bool,
    control_available: bool,
    mic_control_available: bool,
}

/// Shared, process-wide audio service.
pub struct AudioService {
    /// Latest snapshot of audio state.
    current: RefCell<AudioSnapshot>,
    /// Registered callbacks.
    callbacks: Callbacks<AudioSnapshot>,
    /// Whether the service has completed initialization.
    ready: Cell<bool>,
    /// Timestamp when the service first became ready.
    ready_at: Cell<Option<Instant>>,
    /// Sender for commands to the Pulse worker thread.
    command_tx: Sender<AudioCommand>,
}

impl AudioService {
    fn new() -> Rc<Self> {
        // Channel for commands to the Pulse thread (the thread blocks on recv()).
        let (command_tx, command_rx) = mpsc::channel::<AudioCommand>();

        let service = Rc::new(Self {
            current: RefCell::new(AudioSnapshot::default()),
            callbacks: Callbacks::new(),
            ready: Cell::new(false),
            ready_at: Cell::new(None),
            command_tx,
        });

        // State updates come back via glib::idle_add_once() - no polling needed.
        thread::spawn(move || {
            pulse_worker_thread(command_rx);
        });

        service
    }

    /// Get the global AudioService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<AudioService> = AudioService::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback to be invoked when audio state changes.
    ///
    /// The callback is executed on the GLib main loop and is called
    /// immediately with the current snapshot if the service is ready.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(&AudioSnapshot) + 'static,
    {
        self.callbacks.register(callback);

        if self.ready.get() {
            let snapshot = self.current.borrow().clone();
            self.callbacks.notify(&snapshot);
        }
    }

    /// Get the current audio snapshot.
    pub fn current(&self) -> AudioSnapshot {
        self.current.borrow().clone()
    }

    /// Get the current volume percentage.
    #[allow(dead_code)]
    pub fn volume(&self) -> u32 {
        self.current.borrow().volume
    }

    /// Whether the default sink is muted.
    #[allow(dead_code)]
    pub fn is_muted(&self) -> bool {
        self.current.borrow().muted
    }

    /// Whether the service has completed initialization.
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.ready.get()
    }

    /// Whether the service is still in the initial
    /// post-connection settle period. During this time,
    /// User-facing UI like the OSD should typically stay quiet during this period.
    pub fn in_initial_settle(&self) -> bool {
        match self.ready_at.get() {
            None => true,
            Some(t) => t.elapsed() < Duration::from_millis(INITIAL_SETTLE_MS),
        }
    }

    /// Whether the audio backend is available.
    #[allow(dead_code)]
    pub fn is_available(&self) -> bool {
        self.current.borrow().available
    }

    /// Set volume as a percentage (0–150).
    ///
    /// Values are clamped to [0, 150]. This method is efficient for rapid
    /// calls (e.g., holding volume keys).
    pub fn set_volume(&self, percent: u32) {
        let percent = percent.clamp(0, 150);
        let _ = self.command_tx.send(AudioCommand::SetVolume(percent));
    }

    /// Adjust volume by a relative amount (e.g., +5 or -5 percentage points).
    pub fn set_volume_relative(&self, delta: i32) {
        let _ = self.command_tx.send(AudioCommand::SetVolumeRelative(delta));
    }

    /// Set the mute state for the default sink.
    #[allow(dead_code)]
    pub fn set_muted(&self, muted: bool) {
        let _ = self.command_tx.send(AudioCommand::SetMuted(muted));
    }

    /// Toggle the mute state for the default sink.
    pub fn toggle_mute(&self) {
        let _ = self.command_tx.send(AudioCommand::ToggleMute);
    }

    /// Set the mute state for the default source (mic).
    #[allow(dead_code)]
    pub fn set_mic_muted(&self, muted: bool) {
        let _ = self.command_tx.send(AudioCommand::SetMicMuted(muted));
    }

    /// Toggle the mute state for the default source (mic).
    #[allow(dead_code)]
    pub fn toggle_mic_mute(&self) {
        let _ = self.command_tx.send(AudioCommand::ToggleMicMute);
    }

    /// Set the default sink by name.
    pub fn set_default_sink(&self, name: &str) {
        let _ = self
            .command_tx
            .send(AudioCommand::SetDefaultSink(name.to_string()));
    }

    /// Set mic volume as a percentage (0–150).
    ///
    /// Values are clamped to [0, 150]. This method is efficient for rapid
    /// calls (e.g., dragging slider).
    pub fn set_mic_volume(&self, percent: u32) {
        let percent = percent.clamp(0, 150);
        let _ = self.command_tx.send(AudioCommand::SetMicVolume(percent));
    }

    /// Set the default source (microphone) by name.
    pub fn set_default_source(&self, name: &str) {
        let _ = self
            .command_tx
            .send(AudioCommand::SetDefaultSource(name.to_string()));
    }

    /// Request a full state refresh.
    #[allow(dead_code)]
    pub fn refresh(&self) {
        let _ = self.command_tx.send(AudioCommand::Refresh);
    }

    /// Record that an external tool requested a volume change.
    ///
    /// This is used for behavioral detection: when volume changes are made
    /// via external tools (e.g., `pactl`, `wpctl`, WM keybinds) that bypass
    /// the AudioService, we still need to track the "expected" value so the
    /// behavioral heuristic can detect if the backend is ignoring changes.
    ///
    /// The OSD IPC handler calls this when it receives volume messages from
    /// external sources.
    pub fn note_external_volume_request(&self, percent: u32) {
        let _ = self
            .command_tx
            .send(AudioCommand::NoteExternalVolumeRequest(percent));
    }

    /// Get a list of available sinks.
    #[allow(dead_code)]
    pub fn sinks(&self) -> Vec<SinkInfoSnapshot> {
        self.current.borrow().sinks.clone()
    }

    fn apply_state_update(&self, update: AudioStateUpdate) {
        let new_snapshot = AudioSnapshot {
            volume: update.volume,
            muted: update.muted,
            mic_muted: update.mic_muted,
            mic_volume: update.mic_volume,
            sinks: update.sinks,
            default_sink_name: update.default_sink_name,
            sources: update.sources,
            default_source_name: update.default_source_name,
            available: update.available,
            control_available: update.control_available,
            mic_control_available: update.mic_control_available,
        };

        // Check if anything actually changed.
        {
            let current = self.current.borrow();
            if current.volume == new_snapshot.volume
                && current.muted == new_snapshot.muted
                && current.mic_muted == new_snapshot.mic_muted
                && current.mic_volume == new_snapshot.mic_volume
                && current.default_sink_name == new_snapshot.default_sink_name
                && current.default_source_name == new_snapshot.default_source_name
                && current.available == new_snapshot.available
                && current.control_available == new_snapshot.control_available
                && current.mic_control_available == new_snapshot.mic_control_available
                && current.sinks.len() == new_snapshot.sinks.len()
                && current.sources.len() == new_snapshot.sources.len()
            {
                // Sinks list length is the same; check if contents differ.
                let sinks_equal =
                    current
                        .sinks
                        .iter()
                        .zip(new_snapshot.sinks.iter())
                        .all(|(a, b)| {
                            a.name == b.name
                                && a.is_default == b.is_default
                                && a.port_available == b.port_available
                        });
                let sources_equal =
                    current
                        .sources
                        .iter()
                        .zip(new_snapshot.sources.iter())
                        .all(|(a, b)| {
                            a.name == b.name
                                && a.is_default == b.is_default
                                && a.port_available == b.port_available
                        });
                if sinks_equal && sources_equal {
                    return;
                }
            }
        }

        // Update state and mark as ready.
        *self.current.borrow_mut() = new_snapshot.clone();
        if !self.ready.get() {
            self.ready.set(true);
            self.ready_at.set(Some(Instant::now()));
            debug!("AudioService: ready (connected to PulseAudio)");
        }

        self.callbacks.notify(&new_snapshot);
    }
}

impl Drop for AudioService {
    fn drop(&mut self) {
        debug!("AudioService: shutting down");

        // Signal the worker thread to stop.
        let _ = self.command_tx.send(AudioCommand::Shutdown);
    }
}

/// Internal state for the PulseAudio worker thread.
///
/// This state is shared between the worker thread and PulseAudio callbacks
/// via `Arc<Mutex<PulseWorkerState>>`. All fields are updated asynchronously
/// from PulseAudio events and polled by the main thread when building
/// `AudioStateUpdate` snapshots.
///
/// # Field Groups
///
/// The fields are organized into logical groups:
/// - **Sink (output) state**: volume, muted, sinks, default_sink_*, channel_count, control_available
/// - **Source (input) state**: mic_*, sources, default_source_*, mic_channel_count, mic_control_available  
/// - **Connection state**: available
/// - **Behavioral detection**: last_volume_request, stuck_attempts
#[derive(Default)]
struct PulseWorkerState {
    // ===== Sink (Output Device) State =====
    /// Current volume of the default sink as a percentage (0–150).
    ///
    /// Values above 100 represent overdrive/amplification. Updated whenever
    /// we receive sink info from PulseAudio. Not updated if the sink reports
    /// invalid volume data (e.g., 0 channels during startup).
    volume: u32,

    /// Whether the default audio output sink is currently muted.
    ///
    /// This is independent of volume—a sink can be muted at any volume level.
    muted: bool,

    /// All available audio output sinks discovered from PulseAudio.
    ///
    /// Updated whenever sinks are added, removed, or their properties change.
    /// Each entry contains the sink name, description, and default status.
    sinks: Vec<SinkInfoSnapshot>,

    /// The PulseAudio name of the current default audio output sink.
    ///
    /// This is the internal identifier used by PulseAudio (e.g., "alsa_output.pci-0000_00_1f.3.analog-stereo").
    /// Used to match sink info updates and to set the default sink.
    default_sink_name: Option<String>,

    /// The PulseAudio index of the current default audio output sink.
    ///
    /// Used for efficient lookups when setting volume/mute by index rather than name.
    /// May be `None` briefly during startup before sink info is received.
    default_sink_index: Option<u32>,

    /// Number of audio channels in the default sink's volume structure.
    ///
    /// Typically 2 for stereo, but varies by device. A value of 0 indicates
    /// the sink is not yet active or has invalid audio specs (common on Asahi
    /// Linux before audio is first played). Volume operations are blocked when
    /// this is 0 to prevent PulseAudio assertion failures.
    channel_count: u8,

    /// Whether volume control operations are safe to perform on the default sink.
    ///
    /// This is `false` when:
    /// - `channel_count` is 0
    /// - Volume/channel_map/sample_spec validation fails
    /// - Behavioral detection indicates the backend is ignoring volume changes
    ///
    /// When `false`, volume operations are silently skipped to avoid crashes
    /// or user-visible errors.
    control_available: bool,

    // ===== Source (Input Device / Microphone) State =====
    /// Whether the default audio input source (microphone) is muted.
    ///
    /// `None` if we haven't yet received source info from PulseAudio.
    /// Once set, remains `Some` for the session lifetime.
    mic_muted: Option<bool>,

    /// Current volume of the default source (microphone) as a percentage (0–150).
    ///
    /// `None` if we haven't yet received source info. Values above 100
    /// represent gain/amplification.
    mic_volume: Option<u32>,

    /// All available audio input sources (microphones) discovered from PulseAudio.
    ///
    /// Monitor sources (which mirror output audio) are filtered out since
    /// they're not useful as microphone inputs for most users.
    sources: Vec<SourceInfoSnapshot>,

    /// The PulseAudio name of the current default audio input source.
    ///
    /// Used to identify the active microphone device.
    default_source_name: Option<String>,

    /// The PulseAudio index of the current default audio input source.
    ///
    /// Used for efficient lookups when setting mic volume/mute.
    default_source_index: Option<u32>,

    /// Number of audio channels in the default source's volume structure.
    ///
    /// Typically 1 for mono microphones. A value of 0 indicates the source
    /// is not yet active. Mic volume operations are blocked when this is 0.
    mic_channel_count: u8,

    /// Whether mic volume control operations are safe to perform.
    ///
    /// Similar to `control_available` but for the default source.
    mic_control_available: bool,

    // ===== Connection State =====
    /// Whether we have an active connection to the PulseAudio server.
    ///
    /// Set to `true` once we receive initial server info, `false` if the
    /// connection is lost. Used by the UI to show connection status.
    available: bool,

    // ===== Behavioral Detection State =====
    // These fields detect when PulseAudio/PipeWire silently ignores volume
    // changes, which happens on some systems (e.g., Asahi Linux with DSP
    // filter chains) until audio is actively playing.
    /// The last volume percentage we attempted to set.
    ///
    /// `None` if no volume change has been requested since startup or since
    /// the last successful volume change was confirmed. Used to detect when
    /// the backend ignores our volume requests.
    last_volume_request: Option<u32>,

    /// Counter for consecutive failed volume change attempts.
    ///
    /// Incremented when the reported volume doesn't change after we requested
    /// a change. Reset to 0 when any volume change is observed (from us or
    /// externally). When this reaches 2, `control_available` is set to `false`
    /// indicating the backend is unresponsive to volume commands.
    stuck_attempts: u8,
}

/// Main function for the PulseAudio worker thread.
fn pulse_worker_thread(command_rx: Receiver<AudioCommand>) {
    let mainloop = match Mainloop::new() {
        Some(ml) => ml,
        None => {
            error!("AudioService: failed to create PulseAudio mainloop");
            return;
        }
    };

    let mut proplist = Proplist::new().unwrap();
    proplist
        .set_str(pulse::proplist::properties::APPLICATION_NAME, "vibepanel")
        .ok();
    proplist
        .set_str(
            pulse::proplist::properties::APPLICATION_ID,
            "dev.vibepanel.bar",
        )
        .ok();

    let context = match Context::new_with_proplist(&mainloop, "vibepanel-audio", &proplist) {
        Some(ctx) => ctx,
        None => {
            error!("AudioService: failed to create PulseAudio context");
            return;
        }
    };

    // Wrap in Arc<Mutex<>> for sharing with callbacks.
    // Note: libpulse's Mainloop is !Send+!Sync, but we only use it within
    // this single thread. The Arc<Mutex<>> is used for the callback closures.
    #[allow(clippy::arc_with_non_send_sync)]
    let context = Arc::new(Mutex::new(context));
    #[allow(clippy::arc_with_non_send_sync)]
    let mainloop = Arc::new(Mutex::new(mainloop));
    let state = Arc::new(Mutex::new(PulseWorkerState::default()));

    {
        let mut ml = mainloop.lock();
        if ml.start().is_err() {
            error!("AudioService: failed to start PulseAudio mainloop");
            return;
        }
    }

    {
        let mut ml = mainloop.lock();
        ml.lock();

        let mut ctx = context.lock();
        if ctx.connect(None, ContextFlagSet::NOFLAGS, None).is_err() {
            error!("AudioService: failed to connect to PulseAudio server");
            ml.unlock();
            return;
        }

        ml.unlock();
    }

    // Wait for the context to be ready.
    loop {
        let ctx_state = {
            let mut ml = mainloop.lock();
            ml.lock();
            let ctx = context.lock();
            let s = ctx.get_state();
            ml.unlock();
            s
        };

        match ctx_state {
            ContextState::Ready => {
                info!("AudioService: connected to PulseAudio");
                break;
            }
            ContextState::Failed | ContextState::Terminated => {
                error!("AudioService: PulseAudio connection failed");
                return;
            }
            _ => {
                // Still connecting; wait a bit.
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    // Set up subscriptions.
    setup_subscriptions(
        Arc::clone(&mainloop),
        Arc::clone(&context),
        Arc::clone(&state),
    );

    // Do an initial state fetch.
    fetch_full_state(
        Arc::clone(&mainloop),
        Arc::clone(&context),
        Arc::clone(&state),
    );

    // Main command loop.
    loop {
        // Block on commands - no polling needed.
        // PulseAudio disconnection will be detected when user tries to interact,
        // or via the subscription callback if the server sends a disconnect event.
        match command_rx.recv() {
            Ok(AudioCommand::Shutdown) => {
                debug!("AudioService: worker thread shutting down");
                break;
            }
            Ok(cmd) => {
                handle_command(
                    cmd,
                    Arc::clone(&mainloop),
                    Arc::clone(&context),
                    Arc::clone(&state),
                );
            }
            Err(mpsc::RecvError) => {
                debug!("AudioService: command channel disconnected");
                break;
            }
        }
    }

    {
        let mut ml = mainloop.lock();
        ml.lock();
        let mut ctx = context.lock();
        ctx.disconnect();
        ml.unlock();
        ml.stop();
    }

    debug!("AudioService: worker thread exited");
}

fn setup_subscriptions(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    let mut ml = mainloop.lock();
    ml.lock();

    let mut ctx = context.lock();

    // Set up the subscription callback.
    let state_for_cb = Arc::clone(&state);
    let context_for_cb = Arc::clone(&context);

    ctx.set_subscribe_callback(Some(Box::new(move |facility, op, index| {
        let Some(facility) = facility else { return };
        let Some(op) = op else { return };

        // We care about sink, source, and server changes.
        // Note: We're inside a callback, so the mainloop is already locked.
        // We must NOT call mainloop.lock() or ml.lock() here.
        match facility {
            Facility::Sink => {
                if matches!(
                    op,
                    SubscribeOp::Changed | SubscribeOp::New | SubscribeOp::Removed
                ) {
                    // Fetch updated sink info.
                    fetch_sink_by_index_from_callback(
                        Arc::clone(&context_for_cb),
                        Arc::clone(&state_for_cb),
                        index,
                    );
                }
            }
            Facility::Source => {
                if matches!(
                    op,
                    SubscribeOp::Changed | SubscribeOp::New | SubscribeOp::Removed
                ) {
                    // Fetch updated source info for mic volume/mute.
                    fetch_source_by_index_from_callback(
                        Arc::clone(&context_for_cb),
                        Arc::clone(&state_for_cb),
                        index,
                    );
                }
            }
            Facility::Server => {
                // Server info changed (e.g., default sink changed).
                fetch_full_state_from_callback(
                    Arc::clone(&context_for_cb),
                    Arc::clone(&state_for_cb),
                );
            }
            _ => {}
        }
    })));

    // Subscribe to sink, source, and server events.
    let mask = InterestMaskSet::SINK | InterestMaskSet::SOURCE | InterestMaskSet::SERVER;
    ctx.subscribe(mask, |_success| {});

    ml.unlock();
}

fn handle_command(
    cmd: AudioCommand,
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    match cmd {
        AudioCommand::SetVolume(percent) => {
            set_sink_volume(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                percent,
            );
        }
        AudioCommand::SetVolumeRelative(delta) => {
            let current = state.lock().volume;
            let new_volume = (current as i32 + delta).clamp(0, 150) as u32;
            set_sink_volume(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                new_volume,
            );
        }
        AudioCommand::SetMuted(muted) => {
            set_sink_mute(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                muted,
            );
        }
        AudioCommand::ToggleMute => {
            let current_muted = state.lock().muted;
            set_sink_mute(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                !current_muted,
            );
        }
        AudioCommand::SetMicMuted(muted) => {
            set_source_mute(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                muted,
            );
        }
        AudioCommand::ToggleMicMute => {
            let current_muted = state.lock().mic_muted.unwrap_or(false);
            set_source_mute(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                !current_muted,
            );
        }
        AudioCommand::SetMicVolume(percent) => {
            set_source_volume(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
                percent,
            );
        }
        AudioCommand::SetDefaultSink(name) => {
            set_default_sink(Arc::clone(&mainloop), Arc::clone(&context), &name);
            // The server event will trigger a full state refresh.
        }
        AudioCommand::SetDefaultSource(name) => {
            set_default_source(Arc::clone(&mainloop), Arc::clone(&context), &name);
            // The server event will trigger a full state refresh.
        }
        AudioCommand::Refresh => {
            fetch_full_state(mainloop, context, state);
        }
        AudioCommand::NoteExternalVolumeRequest(percent) => {
            // Record the externally-requested volume for behavioral detection.
            // Don't actually send a PA command - the external tool already did that.
            {
                let mut st = state.lock();
                st.last_volume_request = Some(percent);
                debug!(
                    "AudioService: noted external volume request {}% (for behavioral detection)",
                    percent
                );
            }
            // Query the sink to trigger update_sink_state and run the behavioral check.
            // PA won't send us an event if it ignored the volume change, so we have to ask.
            fetch_default_sink(
                Arc::clone(&mainloop),
                Arc::clone(&context),
                Arc::clone(&state),
            );
        }
        AudioCommand::Shutdown => {
            // Handled in the main loop.
        }
    }
}

fn fetch_full_state(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    // First, get server info to find the default sink/source names.
    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);
    let context_for_cb = Arc::clone(&context);

    // Use a Mutex to track whether we've already processed the callback
    // (get_server_info uses FnMut but is effectively called once).
    let called = Arc::new(Mutex::new(false));

    introspect.get_server_info(move |info| {
        // Ensure we only process once.
        {
            let mut c = called.lock();
            if *c {
                return;
            }
            *c = true;
        }

        let default_sink_name = info.default_sink_name.as_ref().map(|s| s.to_string());
        let default_source_name = info.default_source_name.as_ref().map(|s| s.to_string());

        {
            let mut st = state_for_cb.lock();
            st.default_sink_name = default_sink_name.clone();
            st.available = true;
        }

        // We're inside a callback, so the mainloop is already locked.
        // Use the context directly without locking the mainloop.

        // Fetch sinks
        fetch_sinks_inner(Arc::clone(&context_for_cb), Arc::clone(&state_for_cb));

        // Fetch default sink details
        if let Some(sink_name) = default_sink_name {
            fetch_sink_by_name_inner(
                Arc::clone(&context_for_cb),
                Arc::clone(&state_for_cb),
                &sink_name,
            );
        }

        // Fetch default source for mic mute status
        if default_source_name.is_some() {
            fetch_default_source_from_callback(
                Arc::clone(&context_for_cb),
                Arc::clone(&state_for_cb),
            );
        }
    });

    ml.unlock();
}

/// Version called from within a callback (mainloop already locked).
fn fetch_full_state_from_callback(
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);
    let context_for_cb = Arc::clone(&context);

    // Use a Mutex to track whether we've already processed the callback.
    let called = Arc::new(Mutex::new(false));

    introspect.get_server_info(move |info| {
        // Ensure we only process once.
        {
            let mut c = called.lock();
            if *c {
                return;
            }
            *c = true;
        }

        let default_sink_name = info.default_sink_name.as_ref().map(|s| s.to_string());
        let default_source_name = info.default_source_name.as_ref().map(|s| s.to_string());

        {
            let mut st = state_for_cb.lock();
            st.default_sink_name = default_sink_name.clone();
            st.available = true;
        }

        // We're inside a callback, so the mainloop is already locked.
        // Use the context directly without locking the mainloop.

        // Fetch sinks
        fetch_sinks_inner(Arc::clone(&context_for_cb), Arc::clone(&state_for_cb));

        // Fetch default sink details
        if let Some(sink_name) = default_sink_name {
            fetch_sink_by_name_inner(
                Arc::clone(&context_for_cb),
                Arc::clone(&state_for_cb),
                &sink_name,
            );
        }

        // Fetch default source for mic mute status
        if default_source_name.is_some() {
            fetch_default_source_from_callback(
                Arc::clone(&context_for_cb),
                Arc::clone(&state_for_cb),
            );
        }
    });
}

/// Inner version called from within a callback (mainloop already locked).
fn fetch_sinks_inner(context: Arc<Mutex<Context>>, state: Arc<Mutex<PulseWorkerState>>) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    // Collect sinks in a temporary Vec.
    let collected_sinks = Arc::new(Mutex::new(Vec::new()));
    let collected_for_cb = Arc::clone(&collected_sinks);
    let state_for_cb = Arc::clone(&state);

    introspect.get_sink_info_list(move |result| {
        match result {
            ListResult::Item(info) => {
                let name = info
                    .name
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let description = info
                    .description
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.clone());

                let default_name = state_for_cb.lock().default_sink_name.clone();
                let is_default = default_name.as_ref().map(|n| n == &name).unwrap_or(false);

                // Check active port availability (for jack detection, e.g., headphones)
                // PortAvailable::Unknown means no jack detection support - treat as available
                // PortAvailable::No means port not available (e.g., headphones unplugged)
                // PortAvailable::Yes means port is available (e.g., headphones plugged in)
                let port_available = info.active_port.as_ref().map(|port| match port.available {
                    PortAvailable::No => false,
                    PortAvailable::Yes | PortAvailable::Unknown => true,
                });

                collected_for_cb.lock().push(SinkInfoSnapshot {
                    name,
                    description,
                    is_default,
                    port_available,
                });
            }
            ListResult::End => {
                // All sinks collected; update state.
                let sinks = std::mem::take(&mut *collected_for_cb.lock());
                {
                    let mut st = state_for_cb.lock();
                    st.sinks = sinks;
                }
                send_state_update(&state_for_cb.lock());
            }
            ListResult::Error => {
                warn!("AudioService: error fetching sink list");
            }
        }
    });
}

/// Inner version called from within a callback (mainloop already locked).
fn fetch_sink_by_name_inner(
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    name: &str,
) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);

    introspect.get_sink_info_by_name(name, move |result| {
        if let ListResult::Item(info) = result {
            update_sink_state(&state_for_cb, info);
            send_state_update(&state_for_cb.lock());
        }
    });
}

/// Fetch the default sink state (called from command handler, locks mainloop).
fn fetch_default_sink(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    let sink_index = state.lock().default_sink_index;

    let Some(index) = sink_index else {
        debug!("AudioService: no default sink to query");
        return;
    };

    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);

    introspect.get_sink_info_by_index(index, move |result| {
        if let ListResult::Item(info) = result {
            update_sink_state(&state_for_cb, info);
            send_state_update(&state_for_cb.lock());
        }
    });

    ml.unlock();
}

/// Inner version called from within a callback (mainloop already locked).
fn fetch_sink_by_index_from_callback(
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    index: u32,
) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);

    introspect.get_sink_info_by_index(index, move |result| {
        if let ListResult::Item(info) = result {
            // Only update if this is the default sink.
            let is_default = {
                let st = state_for_cb.lock();
                st.default_sink_index == Some(info.index)
                    || st.default_sink_name.as_deref() == info.name.as_ref().map(|s| s.as_ref())
            };

            if is_default {
                update_sink_state(&state_for_cb, info);
                send_state_update(&state_for_cb.lock());
            }
        }
    });
}

/// Version called from within a callback (mainloop already locked, no need for lock/unlock).
fn fetch_default_source_from_callback(
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    // First get server info to find the default source name.
    let state_for_cb = Arc::clone(&state);
    let context_for_source = Arc::clone(&context);

    // Use a Mutex to track whether we've already processed the callback.
    let called = Arc::new(Mutex::new(false));

    introspect.get_server_info(move |info| {
        // Ensure we only process once.
        {
            let mut c = called.lock();
            if *c {
                return;
            }
            *c = true;
        }

        let default_source_name = info.default_source_name.as_ref().map(|s| s.to_string());

        {
            let mut st = state_for_cb.lock();
            st.default_source_name = default_source_name.clone();
        }

        if let Some(source_name) = default_source_name {
            // We're inside a callback, so the mainloop is already locked.
            // Just get the context and call introspect directly.
            let ctx2 = context_for_source.lock();
            let introspect2 = ctx2.introspect();

            let state_for_source = Arc::clone(&state_for_cb);

            introspect2.get_source_info_by_name(&source_name, move |result| {
                if let ListResult::Item(info) = result {
                    update_source_state(&state_for_source, info);
                    send_state_update(&state_for_source.lock());
                }
            });
        }

        // Fetch all sources for the source list
        fetch_sources_inner(Arc::clone(&context_for_source), Arc::clone(&state_for_cb));
    });
}

/// Inner version called from within a callback (mainloop already locked).
fn fetch_sources_inner(context: Arc<Mutex<Context>>, state: Arc<Mutex<PulseWorkerState>>) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    // Collect sources in a temporary Vec.
    let collected_sources = Arc::new(Mutex::new(Vec::new()));
    let collected_for_cb = Arc::clone(&collected_sources);
    let state_for_cb = Arc::clone(&state);

    introspect.get_source_info_list(move |result| {
        match result {
            ListResult::Item(info) => {
                // Skip monitor sources (they mirror sinks, not useful as mic inputs)
                if info.monitor_of_sink.is_some() {
                    return;
                }

                let name = info
                    .name
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let description = info
                    .description
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.clone());

                let default_name = state_for_cb.lock().default_source_name.clone();
                let is_default = default_name.as_ref().map(|n| n == &name).unwrap_or(false);

                // Check active port availability (for jack detection)
                let port_available = info.active_port.as_ref().map(|port| match port.available {
                    PortAvailable::No => false,
                    PortAvailable::Yes | PortAvailable::Unknown => true,
                });

                collected_for_cb.lock().push(SourceInfoSnapshot {
                    name,
                    description,
                    is_default,
                    port_available,
                });
            }
            ListResult::End => {
                // All sources collected; update state.
                let sources = std::mem::take(&mut *collected_for_cb.lock());
                {
                    let mut st = state_for_cb.lock();
                    st.sources = sources;
                }
                send_state_update(&state_for_cb.lock());
            }
            ListResult::Error => {
                warn!("AudioService: error fetching source list");
            }
        }
    });
}

/// Update source (microphone) state from SourceInfo.
fn update_source_state(
    state: &Arc<Mutex<PulseWorkerState>>,
    info: &libpulse_binding::context::introspect::SourceInfo,
) {
    let mut st = state.lock();

    // Get channel count from the source's volume structure
    let channel_count = info.volume.len();

    // Check various validation indicators from libpulse
    let volume_valid = info.volume.is_valid();
    let channel_map_valid = info.channel_map.is_valid();
    let sample_spec_valid = info.sample_spec.is_valid();

    // Calculate volume as percentage (only if volume structure is valid)
    let volume_percent = if volume_valid && channel_count > 0 {
        let avg_volume = info.volume.avg();
        ((avg_volume.0 as f64 / Volume::NORMAL.0 as f64) * 100.0).round() as u32
    } else {
        st.mic_volume.unwrap_or(0) // Keep previous value if invalid
    };

    // Static checks for structural validity
    let static_ok = channel_count > 0 && volume_valid && channel_map_valid && sample_spec_valid;

    st.mic_volume = Some(volume_percent);
    st.mic_muted = Some(info.mute);
    st.default_source_index = Some(info.index);
    st.mic_channel_count = channel_count;
    st.mic_control_available = static_ok;
    if let Some(name) = info.name.as_ref() {
        st.default_source_name = Some(name.to_string());
    }
}

/// Inner version called from within a callback (mainloop already locked).
fn fetch_source_by_index_from_callback(
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    index: u32,
) {
    let ctx = context.lock();
    let introspect = ctx.introspect();

    let state_for_cb = Arc::clone(&state);

    introspect.get_source_info_by_index(index, move |result| {
        if let ListResult::Item(info) = result {
            // Only update if this is the default source.
            let is_default = {
                let st = state_for_cb.lock();
                st.default_source_index == Some(info.index)
                    || st.default_source_name.as_deref() == info.name.as_ref().map(|s| s.as_ref())
            };

            if is_default {
                update_source_state(&state_for_cb, info);
                send_state_update(&state_for_cb.lock());
            }
        }
    });
}

fn update_sink_state(state: &Arc<Mutex<PulseWorkerState>>, info: &SinkInfo) {
    let mut st = state.lock();

    // Get channel count from the sink's volume structure
    let channel_count = info.volume.len();

    // Check various validation indicators from libpulse
    let volume_valid = info.volume.is_valid();
    let channel_map_valid = info.channel_map.is_valid();
    let sample_spec_valid = info.sample_spec.is_valid();
    let sample_spec_channels = info.sample_spec.channels;
    let sink_state = info.state;

    // Debug: log all indicators to understand the Asahi Linux situation
    debug!(
        "AudioService: sink '{}' volume.len()={} volume.is_valid()={} \
         channel_map.is_valid()={} sample_spec.is_valid()={} sample_spec.channels={} \
         state={:?} flags={:?}",
        info.name.as_ref().map(|s| s.as_ref()).unwrap_or("?"),
        channel_count,
        volume_valid,
        channel_map_valid,
        sample_spec_valid,
        sample_spec_channels,
        sink_state,
        info.flags
    );

    // Reset behavioral detection state if sink changed
    let sink_changed = st.default_sink_name.as_deref() != info.name.as_ref().map(|s| s.as_ref());
    if sink_changed {
        st.last_volume_request = None;
        st.stuck_attempts = 0;
    }

    // Calculate volume as percentage (only if volume structure is valid)
    let volume_percent = if volume_valid && channel_count > 0 {
        let avg_volume = info.volume.avg();
        ((avg_volume.0 as f64 / Volume::NORMAL.0 as f64) * 100.0).round() as u32
    } else {
        st.volume // Keep previous value if invalid
    };

    // Behavioral detection: did the backend respond to our volume request?
    //
    // Some audio stacks (e.g. Asahi Linux with PipeWire DSP filter chains)
    // report valid sink properties but silently ignore volume changes until
    // audio is actually playing. We detect this by tracking whether our
    // requested volume changes are reflected in the reported volume.
    //
    // Simple heuristic: if we requested a change and the volume didn't move
    // at all, the backend is ignoring us. We don't care *where* it ended up,
    // just whether it moved.
    let prev_volume = st.volume;

    if let Some(requested) = st.last_volume_request {
        // We requested a change. Did the volume move at all?
        if volume_percent != prev_volume {
            // Volume changed - backend is responsive
            st.stuck_attempts = 0;
            st.last_volume_request = None;
        } else if requested != prev_volume {
            // We asked for a different value but volume didn't budge
            st.stuck_attempts = st.stuck_attempts.saturating_add(1);
        } else {
            // We requested the same value we already had - not a real test
            st.last_volume_request = None;
        }
    } else {
        // No pending request: if volume changed externally, backend is responsive
        if volume_percent != prev_volume {
            st.stuck_attempts = 0;
        }
    }

    let behavioral_ok = st.stuck_attempts < 2;

    // Static checks for structural validity
    let static_ok = channel_count > 0 && volume_valid && channel_map_valid && sample_spec_valid;

    // Combine static and behavioral checks
    let control_available = static_ok && behavioral_ok;

    // Log transitions in control_available state
    if !control_available && st.control_available {
        let mut reasons = Vec::new();
        if channel_count == 0 {
            reasons.push("0 channels");
        }
        if !volume_valid {
            reasons.push("volume invalid");
        }
        if !channel_map_valid {
            reasons.push("channel_map invalid");
        }
        if !sample_spec_valid {
            reasons.push("sample_spec invalid");
        }
        if !behavioral_ok {
            reasons.push("backend not responding to volume changes");
        }
        warn!(
            "AudioService: volume control unavailable ({})",
            reasons.join(", ")
        );
    } else if control_available && !st.control_available {
        info!(
            "AudioService: volume control restored (channels={}, state={:?})",
            channel_count, sink_state
        );
    }

    st.volume = volume_percent;
    st.muted = info.mute;
    st.default_sink_index = Some(info.index);
    st.channel_count = channel_count;
    st.control_available = control_available;
    if let Some(name) = info.name.as_ref() {
        st.default_sink_name = Some(name.to_string());
    }
}

fn set_sink_volume(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    percent: u32,
) {
    let (sink_index, channel_count, control_available) = {
        let st = state.lock();
        (
            st.default_sink_index,
            st.channel_count,
            st.control_available,
        )
    };

    let sink_index = match sink_index {
        Some(idx) => idx,
        None => {
            warn!("AudioService: no default sink to set volume on");
            return;
        }
    };

    // Guard against invalid channel count (would crash PA with assertion failure)
    if !control_available || channel_count == 0 {
        debug!(
            "AudioService: skipping volume change - control unavailable (channels={})",
            channel_count
        );
        return;
    }

    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let mut introspect = ctx.introspect();

    // Calculate the volume value.
    let volume_value = Volume((Volume::NORMAL.0 as f64 * percent as f64 / 100.0) as u32);

    // Use the actual channel count from the sink
    let mut cv = pulse::volume::ChannelVolumes::default();
    cv.set(channel_count, volume_value);

    introspect.set_sink_volume_by_index(sink_index, &cv, None);

    // Update cached state immediately for responsiveness.
    {
        let mut st = state.lock();
        st.volume = percent;
        st.last_volume_request = Some(percent); // Track for behavioral detection
    }

    ml.unlock();
}

fn set_sink_mute(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    muted: bool,
) {
    let sink_index = match state.lock().default_sink_index {
        Some(idx) => idx,
        None => {
            warn!("AudioService: no default sink to set mute on");
            return;
        }
    };

    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let mut introspect = ctx.introspect();

    introspect.set_sink_mute_by_index(sink_index, muted, None);

    // Update cached state immediately for responsiveness.
    {
        let mut st = state.lock();
        st.muted = muted;
    }

    ml.unlock();

    // Notify UI of the change immediately (don't wait for PA event)
    {
        let st = state.lock();
        send_state_update(&st);
    }
}

fn set_source_mute(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    muted: bool,
) {
    let source_index = match state.lock().default_source_index {
        Some(idx) => idx,
        None => {
            warn!("AudioService: no default source to set mute on");
            return;
        }
    };

    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let mut introspect = ctx.introspect();

    introspect.set_source_mute_by_index(source_index, muted, None);

    // Update cached state immediately for responsiveness.
    {
        let mut st = state.lock();
        st.mic_muted = Some(muted);
    }

    ml.unlock();

    // Notify UI of the change immediately (don't wait for PA event)
    {
        let st = state.lock();
        send_state_update(&st);
    }
}

fn set_source_volume(
    mainloop: Arc<Mutex<Mainloop>>,
    context: Arc<Mutex<Context>>,
    state: Arc<Mutex<PulseWorkerState>>,
    percent: u32,
) {
    let (source_index, channel_count, control_available) = {
        let st = state.lock();
        (
            st.default_source_index,
            st.mic_channel_count,
            st.mic_control_available,
        )
    };

    let source_index = match source_index {
        Some(idx) => idx,
        None => {
            warn!("AudioService: no default source to set volume on");
            return;
        }
    };

    // Guard against invalid channel count
    if !control_available || channel_count == 0 {
        debug!(
            "AudioService: skipping mic volume change - control unavailable (channels={})",
            channel_count
        );
        return;
    }

    let mut ml = mainloop.lock();
    ml.lock();

    let ctx = context.lock();
    let mut introspect = ctx.introspect();

    // Calculate the volume value.
    let volume_value = Volume((Volume::NORMAL.0 as f64 * percent as f64 / 100.0) as u32);

    // Use the actual channel count from the source
    let mut cv = pulse::volume::ChannelVolumes::default();
    cv.set(channel_count, volume_value);

    introspect.set_source_volume_by_index(source_index, &cv, None);

    // Update cached state immediately for responsiveness.
    {
        let mut st = state.lock();
        st.mic_volume = Some(percent);
    }

    ml.unlock();
}

fn set_default_sink(mainloop: Arc<Mutex<Mainloop>>, context: Arc<Mutex<Context>>, name: &str) {
    let mut ml = mainloop.lock();
    ml.lock();

    let mut ctx = context.lock();
    ctx.set_default_sink(name, |_success| {});

    ml.unlock();
}

fn set_default_source(mainloop: Arc<Mutex<Mainloop>>, context: Arc<Mutex<Context>>, name: &str) {
    let mut ml = mainloop.lock();
    ml.lock();

    let mut ctx = context.lock();
    ctx.set_default_source(name, |_success| {});

    ml.unlock();
}

fn build_state_update(state: &PulseWorkerState) -> AudioStateUpdate {
    AudioStateUpdate {
        volume: state.volume,
        muted: state.muted,
        mic_muted: state.mic_muted,
        mic_volume: state.mic_volume,
        sinks: state.sinks.clone(),
        default_sink_name: state.default_sink_name.clone(),
        sources: state.sources.clone(),
        default_source_name: state.default_source_name.clone(),
        available: state.available,
        control_available: state.control_available,
        mic_control_available: state.mic_control_available,
    }
}

/// Send a state update to the main thread via glib::idle_add_once().
/// This wakes the GLib main loop immediately (no polling).
fn send_state_update(state: &PulseWorkerState) {
    let update = build_state_update(state);
    glib::idle_add_once(move || {
        AudioService::global().apply_state_update(update);
    });
}

// CLI interface - synchronous, standalone (no GTK main loop required)

use pulse::mainloop::standard::IterateResult;
use pulse::mainloop::standard::Mainloop as StandardMainloop;

/// Synchronous audio control for CLI usage.
///
/// This is a lightweight, standalone interface that doesn't require GTK or
/// a running main loop. It uses blocking PulseAudio calls with a standard
/// (non-threaded) mainloop.
pub struct AudioCli {
    /// PulseAudio mainloop (standard, non-threaded).
    mainloop: StandardMainloop,
    /// PulseAudio context.
    context: Context,
    /// Cached volume percentage.
    volume: u32,
    /// Cached mute state.
    muted: bool,
    /// Index of the default sink.
    sink_index: Option<u32>,
    /// Number of channels in the default sink.
    channel_count: u8,
    /// Whether volume control is currently available (sink not suspended).
    control_available: bool,
}

impl AudioCli {
    /// Create a new CLI audio controller.
    ///
    /// Returns `None` if PulseAudio connection fails.
    pub fn new() -> Option<Self> {
        let mut mainloop = StandardMainloop::new()?;

        let mut proplist = Proplist::new()?;
        proplist
            .set_str(
                pulse::proplist::properties::APPLICATION_NAME,
                "vibepanel-cli",
            )
            .ok();

        let mut context = Context::new_with_proplist(&mainloop, "vibepanel-cli", &proplist)?;

        // Connect to the server.
        if context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .is_err()
        {
            return None;
        }

        // Wait for the context to be ready (with timeout).
        let start = Instant::now();
        let timeout = Duration::from_secs(5);

        loop {
            match mainloop.iterate(false) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => return None,
            }

            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => return None,
                _ => {
                    if start.elapsed() > timeout {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }

        let mut cli = Self {
            mainloop,
            context,
            volume: 0,
            muted: false,
            sink_index: None,
            channel_count: 2,         // Default to stereo, updated by refresh_state
            control_available: false, // Conservative default, updated by refresh_state
        };

        // Fetch initial state.
        cli.refresh_state();

        Some(cli)
    }

    /// Get the current volume percentage.
    pub fn get_volume(&self) -> u32 {
        self.volume
    }

    /// Check if audio is muted.
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Set volume to a specific percentage (0-150).
    pub fn set_volume(&mut self, percent: u32) -> Result<(), String> {
        let sink_index = self.sink_index.ok_or_else(|| {
            "no default sink found (is PulseAudio/pipewire-pulse running?)".to_string()
        })?;

        // Guard against invalid channel count (would crash PA with assertion failure)
        if self.channel_count == 0 {
            return Err(
                "audio sink has no channels (not yet active - try playing audio first)".to_string(),
            );
        }

        // Guard against unavailable sink (0 channels, invalid specs, etc.)
        if !self.control_available {
            return Err("audio device not ready (try playing audio first)".to_string());
        }

        let percent = percent.clamp(0, 150);

        let mut introspect = self.context.introspect();

        // Calculate the volume value.
        let volume_value = Volume((Volume::NORMAL.0 as f64 * percent as f64 / 100.0) as u32);

        // Use the actual channel count from the sink
        let mut cv = pulse::volume::ChannelVolumes::default();
        cv.set(self.channel_count, volume_value);

        let op = introspect.set_sink_volume_by_index(sink_index, &cv, None);

        // Wait for operation to complete.
        self.wait_for_operation(op)?;

        // Update cached state.
        self.volume = percent;

        Ok(())
    }

    /// Set the mute state.
    pub fn set_muted(&mut self, muted: bool) -> Result<(), String> {
        let sink_index = self.sink_index.ok_or_else(|| {
            "no default sink found (is PulseAudio/pipewire-pulse running?)".to_string()
        })?;

        let mut introspect = self.context.introspect();
        let op = introspect.set_sink_mute_by_index(sink_index, muted, None);

        // Wait for operation to complete.
        self.wait_for_operation(op)?;

        // Update cached state.
        self.muted = muted;

        Ok(())
    }

    /// Wait for an operation to complete.
    fn wait_for_operation(
        &mut self,
        op: pulse::operation::Operation<dyn FnMut(bool)>,
    ) -> Result<(), String> {
        let start = Instant::now();
        let timeout = Duration::from_secs(5);

        loop {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    return Err("audio backend stopped while applying change".to_string());
                }
            }

            match op.get_state() {
                pulse::operation::State::Running => {
                    if start.elapsed() > timeout {
                        return Err("audio backend did not respond in time".to_string());
                    }
                    continue;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Refresh state from PulseAudio.
    fn refresh_state(&mut self) {
        let default_sink_name = self.get_default_sink_name();

        if let Some(name) = default_sink_name {
            self.fetch_sink_info(&name);
        }
    }

    /// Get the default sink name from server info.
    fn get_default_sink_name(&mut self) -> Option<String> {
        use std::sync::Arc;

        let result: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let result_clone = Arc::clone(&result);
        let done = Arc::new(Mutex::new(false));
        let done_clone = Arc::clone(&done);

        let introspect = self.context.introspect();
        introspect.get_server_info(move |info| {
            if let Some(name) = info.default_sink_name.as_ref() {
                *result_clone.lock() = Some(name.to_string());
            }
            *done_clone.lock() = true;
        });

        // Iterate until done.
        while !*done.lock() {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => return None,
            }
        }

        // Read result from shared state (don't use Arc::try_unwrap as callback may still be held).
        result.lock().clone()
    }

    /// Fetch sink info by name.
    fn fetch_sink_info(&mut self, name: &str) {
        use std::sync::Arc;

        // Use Arc<Mutex<>> for all result values so they can be updated by the callback
        // and read back by the main thread.
        let result = Arc::new(Mutex::new((
            None::<u32>,  // volume
            None::<bool>, // muted
            None::<u32>,  // index
            None::<u8>,   // channels
            None::<bool>, // control_available
        )));
        let done = Arc::new(Mutex::new(false));

        let result_clone = Arc::clone(&result);
        let done_clone = Arc::clone(&done);

        let introspect = self.context.introspect();
        introspect.get_sink_info_by_name(name, move |list_result| {
            if let ListResult::Item(info) = list_result {
                let avg_volume = info.volume.avg();
                let volume_percent =
                    ((avg_volume.0 as f64 / Volume::NORMAL.0 as f64) * 100.0).round() as u32;

                // Compute control_available using same logic as AudioService.
                // We do NOT check sink state - suspended sinks still accept volume control.
                let channel_count = info.volume.len();
                let volume_valid = info.volume.is_valid();
                let channel_map_valid = info.channel_map.is_valid();
                let sample_spec_valid = info.sample_spec.is_valid();

                let available =
                    channel_count > 0 && volume_valid && channel_map_valid && sample_spec_valid;

                let mut r = result_clone.lock();
                r.0 = Some(volume_percent);
                r.1 = Some(info.mute);
                r.2 = Some(info.index);
                r.3 = Some(channel_count);
                r.4 = Some(available);
            }
            if matches!(
                list_result,
                ListResult::End | ListResult::Error | ListResult::Item(_)
            ) {
                *done_clone.lock() = true;
            }
        });

        // Iterate until done.
        while !*done.lock() {
            match self.mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => return,
            }
        }

        // Read results from the shared state.
        let r = result.lock();
        if let Some(v) = r.0 {
            self.volume = v;
        }
        if let Some(m) = r.1 {
            self.muted = m;
        }
        if let Some(i) = r.2 {
            self.sink_index = Some(i);
        }
        if let Some(c) = r.3 {
            self.channel_count = c;
        }
        if let Some(ca) = r.4 {
            self.control_available = ca;
        }
    }
}

impl Drop for AudioCli {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}
