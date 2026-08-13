//! MediaService - MPRIS D-Bus integration for media player control.
//!
//! This service discovers and controls MPRIS-compatible media players on the session bus.
//! It provides:
//! - Player discovery (org.mpris.MediaPlayer2.*)
//! - Playback state monitoring (Playing/Paused/Stopped)
//! - Metadata access (title, artist, album, art URL, duration)
//! - Playback control (play/pause, next, previous, seek, volume)
//! - Position tracking with periodic polling when playing
//! - Multi-player support with automatic or manual player selection
//!
//! ## Architecture
//!
//! Unlike single-player designs, this service maintains connections to ALL discovered
//! MPRIS players simultaneously. This allows:
//! - Instant player switching (no reconnection delay)
//! - Real-time status for all players (for selector UI)
//! - Simple selection logic (just filter connected players)
//!
//! ## MPRIS D-Bus Interface
//!
//! - Bus: Session
//! - Service names: `org.mpris.MediaPlayer2.*` (e.g., `org.mpris.MediaPlayer2.spotify`)
//! - Object path: `/org/mpris/MediaPlayer2`
//! - Interfaces:
//!   - `org.mpris.MediaPlayer2` - Base interface (Identity, Quit, etc.)
//!   - `org.mpris.MediaPlayer2.Player` - Playback control and state

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::glib::{self, ControlFlow, Variant, clone};
use gtk4::prelude::*;
use tracing::{debug, error, trace, warn};

use super::callbacks::{CallbackId, Callbacks};

// D-Bus constants
const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

/// Position polling interval when playing (in milliseconds).
const POSITION_POLL_INTERVAL_MS: u64 = 1000;
/// Default timeout for D-Bus method calls (in milliseconds).
const DBUS_CALL_TIMEOUT_MS: i32 = 5000;
/// Shorter timeout for position polling queries.
const DBUS_POLL_TIMEOUT_MS: i32 = 1000;
/// Skip at least one poll after local or externally observed seeks.
const SEEK_GRACE_WINDOW: Duration = Duration::from_millis(1500);

// ========== Helper Functions ==========

/// Extract player ID from MPRIS bus name (e.g., "org.mpris.MediaPlayer2.spotify" -> "spotify").
fn player_id_from_bus_name(bus_name: &str) -> String {
    bus_name
        .strip_prefix(MPRIS_PREFIX)
        .map(|s| s.split('.').next().unwrap_or(s))
        .unwrap_or(bus_name)
        .to_string()
}

fn is_youtube_video_id(video_id: &str) -> bool {
    video_id.len() == 11
        && video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn youtube_video_id(source_url: &str) -> Option<&str> {
    let rest = source_url
        .strip_prefix("https://")
        .or_else(|| source_url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;

    let video_id = match host {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            if let Some(query) = path.strip_prefix("watch?") {
                query
                    .split('&')
                    .find_map(|part| part.strip_prefix("v="))?
                    .split(['#', '?'])
                    .next()?
            } else {
                path.strip_prefix("shorts/")?
                    .split(['/', '?', '#', '&'])
                    .next()?
            }
        }
        "youtu.be" | "www.youtu.be" => path.split(['/', '?', '#', '&']).next()?,
        _ => return None,
    };

    is_youtube_video_id(video_id).then_some(video_id)
}

/// Derive a stable YouTube thumbnail when browser MPRIS metadata omits artwork.
fn youtube_thumbnail_url(source_url: &str) -> Option<String> {
    youtube_video_id(source_url)
        .map(|video_id| format!("https://i.ytimg.com/vi/{video_id}/maxresdefault.jpg"))
}

pub(crate) fn youtube_thumbnail_fallback_url(art_url: &str) -> Option<String> {
    art_url
        .strip_prefix("https://i.ytimg.com/vi/")
        .and_then(|path| path.strip_suffix("/maxresdefault.jpg"))
        .filter(|video_id| is_youtube_video_id(video_id))
        // Unlike other lower-resolution variants, mqdefault remains 16:9.
        .map(|video_id| format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg"))
}

fn differs_when_both_present(previous: &Option<String>, current: &Option<String>) -> bool {
    matches!(
        (
            previous.as_deref().filter(|value| !value.is_empty()),
            current.as_deref().filter(|value| !value.is_empty()),
        ),
        (Some(previous), Some(current)) if previous != current
    )
}

fn source_urls_differ(previous: &str, current: &str) -> bool {
    match (youtube_video_id(previous), youtube_video_id(current)) {
        (Some(previous_id), Some(current_id)) => previous_id != current_id,
        _ => previous != current,
    }
}

fn source_urls_differ_when_both_present(
    previous: &Option<String>,
    current: &Option<String>,
) -> bool {
    match (
        previous.as_deref().filter(|url| !url.is_empty()),
        current.as_deref().filter(|url| !url.is_empty()),
    ) {
        (Some(previous), Some(current)) => source_urls_differ(previous, current),
        _ => false,
    }
}

fn has_conflicting_identity(previous: &MediaMetadata, current: &MediaMetadata) -> bool {
    differs_when_both_present(&previous.track_id, &current.track_id)
        || differs_when_both_present(&previous.title, &current.title)
        || source_urls_differ_when_both_present(&previous.url, &current.url)
}

// Missing identity is unknown, so this is not the inverse of `has_track_changed`.
fn is_same_logical_track(previous: &MediaMetadata, current: &MediaMetadata) -> bool {
    if has_conflicting_identity(previous, current) {
        return false;
    }

    if let Some(current_id) = current.track_id.as_deref() {
        return previous.track_id.as_deref() == Some(current_id);
    }

    if let Some(current_url) = current.url.as_deref().filter(|url| !url.is_empty()) {
        return previous
            .url
            .as_deref()
            .is_some_and(|previous_url| !source_urls_differ(previous_url, current_url));
    }

    current
        .title
        .as_ref()
        .is_some_and(|title| !title.is_empty())
        && previous.title == current.title
        && previous.artist == current.artist
}

fn has_track_changed(previous: &MediaMetadata, current: &MediaMetadata) -> bool {
    previous.track_id != current.track_id || has_conflicting_identity(previous, current)
}

fn preserve_same_track_metadata(previous: &MediaMetadata, current: &mut MediaMetadata) {
    if !is_same_logical_track(previous, current) {
        return;
    }

    if current.length.is_none() {
        current.length = previous.length;
    }
    if current.art_url.as_ref().is_none_or(String::is_empty) {
        current.art_url = previous.art_url.clone();
    }
    if current.url.as_ref().is_none_or(String::is_empty) {
        current.url = previous.url.clone();
    }
    if current.track_id.is_none() {
        current.track_id = previous.track_id.clone();
    }
}

/// Browsers can stay active after navigating away from a video while dropping
/// its media session. Keep the last useful art instead of showing the app icon;
/// unlike a clear delay, no later metadata update is expected in this state.
fn preserve_art_when_track_identity_unknown(previous: &MediaMetadata, current: &mut MediaMetadata) {
    let source_missing = current.url.as_ref().is_none_or(String::is_empty);
    let track_changed = current
        .track_id
        .as_deref()
        .is_some_and(|id| previous.track_id.as_deref() != Some(id));

    if source_missing && !track_changed && current.art_url.as_ref().is_none_or(String::is_empty) {
        current.art_url = previous.art_url.clone();
    }
}

fn resolve_metadata(
    previous: &MediaMetadata,
    mut current: MediaMetadata,
    status: PlaybackStatus,
) -> MediaMetadata {
    preserve_incomplete_active_metadata(previous, &mut current, status);
    preserve_art_when_track_identity_unknown(previous, &mut current);
    preserve_same_track_metadata(previous, &mut current);

    if current.art_url.as_ref().is_none_or(String::is_empty) {
        current.art_url = current.url.as_deref().and_then(youtube_thumbnail_url);
    }

    current
}

fn preserve_incomplete_active_metadata(
    previous: &MediaMetadata,
    current: &mut MediaMetadata,
    status: PlaybackStatus,
) {
    let current_is_empty = current
        .title
        .as_ref()
        .is_none_or(|value| value.trim().is_empty())
        && current
            .artist
            .as_ref()
            .is_none_or(|value| value.trim().is_empty())
        && current
            .album
            .as_ref()
            .is_none_or(|value| value.trim().is_empty())
        && current.art_url.as_ref().is_none_or(String::is_empty)
        && current.url.as_ref().is_none_or(String::is_empty)
        && current.length.is_none()
        && current.track_id.is_none();
    if status != PlaybackStatus::Stopped
        && current_is_empty
        && previous.title.is_some()
        && !has_conflicting_identity(previous, current)
    {
        *current = previous.clone();
    }
}

/// Ignore transient zeroes from playing browsers until a non-zero poll arrives.
fn should_accept_position_poll(current: i64, polled: i64, status: PlaybackStatus) -> bool {
    !(polled == 0 && current > 0 && status == PlaybackStatus::Playing)
}

/// Capitalize the first character of a string (e.g., "spotify" -> "Spotify").
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Playback status of the media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl std::str::FromStr for PlaybackStatus {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        })
    }
}

/// Metadata about the currently playing track.
#[derive(Debug, Clone, Default)]
pub struct MediaMetadata {
    /// Track title (xesam:title).
    pub title: Option<String>,
    /// Artist name(s) (xesam:artist).
    pub artist: Option<String>,
    /// Album name (xesam:album).
    pub album: Option<String>,
    /// Resolved artwork URL, normally from mpris:artUrl.
    pub art_url: Option<String>,
    /// Track URL (xesam:url) - useful for identifying web players.
    pub url: Option<String>,
    /// Track duration in microseconds (mpris:length).
    pub length: Option<i64>,
    /// Track ID (mpris:trackid).
    pub track_id: Option<String>,
}

/// Info about a single player, for the player selector UI.
#[derive(Debug, Clone)]
pub struct PlayerInfo {
    /// Bus name (e.g., "org.mpris.MediaPlayer2.spotify").
    pub bus_name: String,
    /// Display name (e.g., "Spotify").
    pub player_name: String,
    /// Current playback status.
    pub playback_status: PlaybackStatus,
    /// Whether this is the currently active player.
    pub is_active: bool,
}

/// Canonical snapshot of media player state.
#[derive(Debug, Clone)]
pub struct MediaSnapshot {
    /// Whether any MPRIS player is available.
    pub available: bool,
    /// Name of the active player (e.g., "Spotify", "Firefox").
    pub player_name: Option<String>,
    /// Raw player ID for icon lookup (e.g., "spotify", "firefox").
    pub player_id: Option<String>,
    /// Current playback status.
    pub playback_status: PlaybackStatus,
    /// Track metadata.
    pub metadata: MediaMetadata,
    /// Current position in microseconds.
    pub position: i64,
    /// Whether the player can play.
    pub can_play: bool,
    /// Whether the player can pause.
    pub can_pause: bool,
    /// Whether the player can go to next track.
    pub can_go_next: bool,
    /// Whether the player can go to previous track.
    pub can_go_previous: bool,
    /// Whether the player can seek.
    pub can_seek: bool,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            available: false,
            player_name: None,
            player_id: None,
            playback_status: PlaybackStatus::Stopped,
            metadata: MediaMetadata::default(),
            position: 0,
            can_play: false,
            can_pause: false,
            can_go_next: false,
            can_go_previous: false,
            can_seek: false,
        }
    }
}

impl MediaSnapshot {
    /// Whether the snapshot contains meaningful track metadata (i.e. a non-empty title).
    pub fn has_metadata(&self) -> bool {
        self.metadata
            .title
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
    }
}

/// State for a single connected MPRIS player.
struct MprisPlayer {
    bus_name: String,
    player_id: String,
    player_name: String,
    proxy: gio::DBusProxy,
    playback_status: PlaybackStatus,
    metadata: MediaMetadata,
    position: i64,
    can_play: bool,
    can_pause: bool,
    can_go_next: bool,
    can_go_previous: bool,
    can_seek: bool,
    can_control: bool,
    /// Signal subscription for PropertiesChanged (set after creation).
    _properties_subscription: Option<gio::SignalSubscription>,
    /// MPRIS Seeked subscription for immediate external seek updates.
    _seeked_subscription: Option<gio::SignalSubscription>,
    /// Track generation for invalidating stale position polls.
    track_generation: u64,
}

impl MprisPlayer {
    fn to_player_info(&self, is_active: bool) -> PlayerInfo {
        PlayerInfo {
            bus_name: self.bus_name.clone(),
            player_name: self.player_name.clone(),
            playback_status: self.playback_status,
            is_active,
        }
    }
}

/// Shared, process-wide media service with multi-player support.
pub struct MediaService {
    /// Connection to the session bus.
    connection: RefCell<Option<gio::DBusConnection>>,
    /// All connected MPRIS players, keyed by bus name.
    players: RefCell<HashMap<String, Rc<RefCell<MprisPlayer>>>>,
    /// Bus name of the currently active player.
    active_player: RefCell<Option<String>>,
    /// User's manual selection (None = auto mode).
    manual_selection: RefCell<Option<String>>,
    /// Last player that started playing (for auto-selection preference).
    last_playing: RefCell<Option<String>>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<MediaSnapshot>,
    /// Signal subscription for NameOwnerChanged (player appear/disappear).
    _name_owner_subscription: RefCell<Option<gio::SignalSubscription>>,
    /// Timer for position polling when playing.
    position_poll_source: RefCell<Option<glib::SourceId>>,
    /// Cancellable for position polling D-Bus calls.
    poll_cancellable: RefCell<gio::Cancellable>,
    /// When the last seek was observed or issued.
    last_seek: Cell<Option<Instant>>,
}

impl MediaService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            connection: RefCell::new(None),
            players: RefCell::new(HashMap::new()),
            active_player: RefCell::new(None),
            manual_selection: RefCell::new(None),
            last_playing: RefCell::new(None),
            callbacks: Callbacks::new(),
            _name_owner_subscription: RefCell::new(None),
            position_poll_source: RefCell::new(None),
            poll_cancellable: RefCell::new(gio::Cancellable::new()),
            last_seek: Cell::new(None),
        });

        Self::init_dbus(&service);
        service
    }

    /// Get the global MediaService singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<MediaService> = MediaService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback for state changes.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&MediaSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        let snapshot = self.build_snapshot();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    /// Unregister a callback by its ID.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Get a clone of the current snapshot.
    pub fn snapshot(&self) -> MediaSnapshot {
        self.build_snapshot()
    }

    /// Get info about all available players (for selector UI).
    pub fn available_players(&self) -> Vec<PlayerInfo> {
        let players = self.players.borrow();
        let active = self.active_player.borrow();

        players
            .values()
            .map(|p| {
                let p = p.borrow();
                let is_active = active.as_ref() == Some(&p.bus_name);
                p.to_player_info(is_active)
            })
            .collect()
    }

    /// Manually select a specific player.
    pub fn set_active_player(self: &Rc<Self>, bus_name: &str) {
        if !self.players.borrow().contains_key(bus_name) {
            warn!("Cannot select unknown player: {}", bus_name);
            return;
        }

        debug!("Manual player selection: {}", bus_name);
        self.manual_selection.replace(Some(bus_name.to_string()));
        self.update_active_player();
        self.notify_callbacks();
    }

    /// Switch to auto-selection mode.
    pub fn set_auto_selection(self: &Rc<Self>) {
        debug!("Switching to auto player selection");
        self.manual_selection.replace(None);
        self.update_active_player();
        self.notify_callbacks();
    }

    /// Check if auto-selection is active.
    pub fn is_auto_selection(&self) -> bool {
        self.manual_selection.borrow().is_none()
    }

    /// Write current active player to state file for CLI commands.
    fn write_ipc_state(&self) {
        let active = self.active_player.borrow();
        super::media_ipc::write_state(active.as_deref());
    }

    // ========== D-Bus Initialization ==========

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        gio::bus_get(
            gio::BusType::Session,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                let connection = match res {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to connect to session bus: {}", e);
                        return;
                    }
                };

                debug!("Connected to session bus for MPRIS");
                this.connection.replace(Some(connection.clone()));

                // Subscribe to NameOwnerChanged to detect player appear/disappear
                let this_weak = Rc::downgrade(&this);
                let subscription = connection.subscribe_to_signal(
                    Some(DBUS_NAME),
                    Some(DBUS_INTERFACE),
                    Some("NameOwnerChanged"),
                    Some(DBUS_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        if let Some(name) = signal.parameters.child_value(0).str()
                            && name.starts_with(MPRIS_PREFIX)
                            && let Some(this) = this_weak.upgrade()
                        {
                            let old_owner_v = signal.parameters.child_value(1);
                            let new_owner_v = signal.parameters.child_value(2);
                            let old_owner = old_owner_v.str().unwrap_or("");
                            let new_owner = new_owner_v.str().unwrap_or("");

                            if old_owner.is_empty() && !new_owner.is_empty() {
                                // Player appeared
                                debug!("MPRIS player appeared: {}", name);
                                this.add_player(name);
                            } else if !old_owner.is_empty() && new_owner.is_empty() {
                                // Player disappeared
                                debug!("MPRIS player disappeared: {}", name);
                                this.remove_player(name);
                            }
                        }
                    },
                );
                this._name_owner_subscription.replace(Some(subscription));

                // Initial player discovery
                this.discover_players();
            },
        );
    }

    /// Discover all available MPRIS players on the bus.
    fn discover_players(self: &Rc<Self>) {
        let Some(connection) = self.connection.borrow().clone() else {
            return;
        };

        let this_weak = Rc::downgrade(self);
        connection.call(
            Some(DBUS_NAME),
            DBUS_PATH,
            DBUS_INTERFACE,
            "ListNames",
            None,
            Some(glib::VariantTy::new("(as)").unwrap()),
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                let reply = match res {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Failed to list D-Bus names: {}", e);
                        return;
                    }
                };

                let names: Vec<String> = reply
                    .child_value(0)
                    .iter()
                    .filter_map(|v| v.get::<String>())
                    .collect();

                let players: Vec<String> = names
                    .into_iter()
                    .filter(|n| n.starts_with(MPRIS_PREFIX))
                    .collect();

                debug!(
                    "Discovered {} MPRIS player(s): {:?}",
                    players.len(),
                    players
                );

                for bus_name in players {
                    this.add_player(&bus_name);
                }
            },
        );
    }

    /// Add a new player (creates proxy and subscribes to signals).
    fn add_player(self: &Rc<Self>, bus_name: &str) {
        if self.players.borrow().contains_key(bus_name) {
            return;
        }

        let Some(connection) = self.connection.borrow().clone() else {
            return;
        };

        let bus_name_owned = bus_name.to_string();
        let this_weak = Rc::downgrade(self);

        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::NONE,
            None::<&gio::DBusInterfaceInfo>,
            &bus_name_owned,
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            None::<&gio::Cancellable>,
            clone!(
                #[strong]
                bus_name_owned,
                move |res| {
                    let Some(this) = this_weak.upgrade() else {
                        return;
                    };

                    let proxy = match res {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Failed to create MPRIS proxy for {}: {}", bus_name_owned, e);
                            return;
                        }
                    };

                    // Extract player ID and name
                    let player_id = player_id_from_bus_name(&bus_name_owned);
                    let player_name = capitalize_first(&player_id);

                    // Create the player with initial state from proxy
                    let player = Rc::new(RefCell::new(MprisPlayer {
                        bus_name: bus_name_owned.clone(),
                        player_id,
                        player_name: player_name.clone(),
                        proxy: proxy.clone(),
                        playback_status: PlaybackStatus::Stopped,
                        metadata: MediaMetadata::default(),
                        position: 0,
                        can_play: false,
                        can_pause: false,
                        can_go_next: false,
                        can_go_previous: false,
                        can_seek: false,
                        can_control: true,
                        _properties_subscription: None,
                        _seeked_subscription: None,
                        track_generation: 0,
                    }));

                    // Update state from cached properties
                    let _ = Self::update_player_from_proxy(&player);

                    // Subscribe to PropertiesChanged for this player
                    let player_weak = Rc::downgrade(&player);
                    let this_weak = Rc::downgrade(&this);
                    let subscription = connection.subscribe_to_signal(
                        Some(&bus_name_owned),
                        Some(PROPERTIES_INTERFACE),
                        Some("PropertiesChanged"),
                        Some(MPRIS_PATH),
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |_signal| {
                            let Some(player) = player_weak.upgrade() else {
                                return;
                            };
                            let Some(this) = this_weak.upgrade() else {
                                return;
                            };

                            let old_status = player.borrow().playback_status;
                            let track_changed = Self::update_player_from_proxy(&player);
                            let new_status = player.borrow().playback_status;
                            let status_changed = old_status != new_status;

                            let bus_name = player.borrow().bus_name.clone();
                            let is_active = this.active_player.borrow().as_ref() == Some(&bus_name);
                            if track_changed && is_active {
                                this.last_seek.set(None);
                            }

                            // Track the most recently playing player
                            if new_status == PlaybackStatus::Playing
                                && old_status != PlaybackStatus::Playing
                            {
                                this.last_playing.replace(Some(bus_name.clone()));
                            }

                            // In auto mode, if this player just started playing, make it active
                            if this.is_auto_selection() && status_changed {
                                if new_status == PlaybackStatus::Playing {
                                    // This player just started playing - make it the active player
                                    let current_active = this.active_player.borrow().clone();
                                    if current_active.as_ref() != Some(&bus_name) {
                                        debug!("Switching to newly playing player: {}", bus_name);
                                        this.active_player.replace(Some(bus_name));
                                        this.on_active_player_changed();
                                    } else {
                                        // Same player resumed - restart position polling
                                        this.start_position_polling();
                                    }
                                } else {
                                    // Player stopped/paused - re-evaluate to find best player
                                    this.update_active_player();
                                }
                            } else if status_changed {
                                // Manual mode: if the active player changed status, handle polling
                                if is_active {
                                    if new_status == PlaybackStatus::Playing {
                                        this.start_position_polling();
                                    } else {
                                        this.stop_position_polling();
                                    }
                                }
                            }

                            this.notify_callbacks();

                            // Some players briefly report stale positions after track
                            // or status changes, so re-poll once they settle.
                            if track_changed || status_changed {
                                let this_weak = Rc::downgrade(&this);
                                glib::timeout_add_local_once(
                                    Duration::from_millis(100),
                                    move || {
                                        if let Some(this) = this_weak.upgrade() {
                                            this.poll_position();
                                        }
                                    },
                                );
                            }
                        },
                    );

                    player.borrow_mut()._properties_subscription = Some(subscription);

                    let player_weak = Rc::downgrade(&player);
                    let this_weak = Rc::downgrade(&this);
                    let seeked_subscription = connection.subscribe_to_signal(
                        Some(&bus_name_owned),
                        Some(MPRIS_PLAYER_INTERFACE),
                        Some("Seeked"),
                        Some(MPRIS_PATH),
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |signal| {
                            let Some(player) = player_weak.upgrade() else {
                                return;
                            };
                            let Some(this) = this_weak.upgrade() else {
                                return;
                            };
                            let Some(position) = signal
                                .parameters
                                .try_child_value(0)
                                .and_then(|value| value.get::<i64>())
                            else {
                                return;
                            };

                            let bus_name = player.borrow().bus_name.clone();
                            player.borrow_mut().position = position.max(0);
                            if this.active_player.borrow().as_ref() == Some(&bus_name) {
                                this.reset_poll_cancellable();
                                this.last_seek.set(Some(Instant::now()));
                                this.notify_callbacks();
                            }
                        },
                    );
                    player.borrow_mut()._seeked_subscription = Some(seeked_subscription);

                    debug!("Added MPRIS player: {} ({})", player_name, bus_name_owned);
                    this.players.borrow_mut().insert(bus_name_owned, player);

                    // Update active player selection
                    this.update_active_player();
                    this.notify_callbacks();
                }
            ),
        );
    }

    /// Remove a player that disappeared.
    fn remove_player(self: &Rc<Self>, bus_name: &str) {
        let removed = self.players.borrow_mut().remove(bus_name);

        if removed.is_some() {
            debug!("Removed MPRIS player: {}", bus_name);

            // Clear manual selection if it was this player
            if self.manual_selection.borrow().as_deref() == Some(bus_name) {
                self.manual_selection.replace(None);
            }

            self.update_active_player();
            self.notify_callbacks();
        }
    }

    /// Update player state from its proxy's cached properties.
    /// Returns `true` if a track change was detected.
    fn update_player_from_proxy(player: &Rc<RefCell<MprisPlayer>>) -> bool {
        // Read all properties first (need to read from proxy without holding borrow_mut)
        let (
            playback_status,
            metadata,
            can_play,
            can_pause,
            can_go_next,
            can_go_previous,
            can_seek,
            can_control,
        ) = {
            let p = player.borrow();
            let proxy = &p.proxy;

            let playback_status = proxy
                .cached_property("PlaybackStatus")
                .and_then(|v| v.get::<String>())
                .map(|s| s.parse().unwrap_or_default())
                .unwrap_or(PlaybackStatus::Stopped);

            let metadata = proxy
                .cached_property("Metadata")
                .map(|m| Self::parse_metadata(&m))
                .unwrap_or_default();

            let can_play = proxy
                .cached_property("CanPlay")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let can_pause = proxy
                .cached_property("CanPause")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let can_go_next = proxy
                .cached_property("CanGoNext")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let can_go_previous = proxy
                .cached_property("CanGoPrevious")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let can_seek = proxy
                .cached_property("CanSeek")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            let can_control = proxy
                .cached_property("CanControl")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(true);

            (
                playback_status,
                metadata,
                can_play,
                can_pause,
                can_go_next,
                can_go_previous,
                can_seek,
                can_control,
            )
        };

        // Preserve artwork and duration when a browser emits a partial update
        // that still identifies the same track.
        let metadata = {
            let p = player.borrow();
            resolve_metadata(&p.metadata, metadata, playback_status)
        };
        let mut p = player.borrow_mut();
        let old_metadata = p.metadata.clone();

        p.playback_status = playback_status;
        p.metadata = metadata;
        p.can_play = can_play;
        p.can_pause = can_pause;
        p.can_go_next = can_go_next;
        p.can_go_previous = can_go_previous;
        p.can_seek = can_seek;
        p.can_control = can_control;

        if has_track_changed(&old_metadata, &p.metadata) {
            p.position = 0;
            p.track_generation += 1;
            true
        } else {
            false
        }
    }

    /// Determine which player should be active.
    fn update_active_player(self: &Rc<Self>) {
        let players = self.players.borrow();
        let old_active = self.active_player.borrow().clone();

        // Honor manual selection if still valid
        if let Some(manual) = self.manual_selection.borrow().as_ref() {
            if players.contains_key(manual) {
                if old_active.as_ref() != Some(manual) {
                    debug!("Active player (manual): {}", manual);
                    self.active_player.replace(Some(manual.clone()));
                    drop(players);
                    self.on_active_player_changed();
                }
                return;
            }
            // Manual selection is no longer valid
            drop(players);
            self.manual_selection.replace(None);
            let players = self.players.borrow();
            self.select_best_player_auto(&players, &old_active);
            return;
        }

        self.select_best_player_auto(&players, &old_active);
    }

    /// Auto-select the best player (last playing > other playing > current paused > other paused > any).
    fn select_best_player_auto(
        self: &Rc<Self>,
        players: &HashMap<String, Rc<RefCell<MprisPlayer>>>,
        old_active: &Option<String>,
    ) {
        // First, check if last_playing is still playing - prefer it
        if let Some(ref last) = *self.last_playing.borrow()
            && let Some(player) = players.get(last)
            && player.borrow().playback_status == PlaybackStatus::Playing
        {
            if old_active.as_ref() != Some(last) {
                debug!("Active player (auto, last playing): {}", last);
                self.active_player.replace(Some(last.clone()));
                self.on_active_player_changed();
            }
            return;
        }

        // Otherwise prefer any playing player
        let playing = players
            .values()
            .find(|p| p.borrow().playback_status == PlaybackStatus::Playing)
            .map(|p| p.borrow().bus_name.clone());

        if let Some(bus_name) = playing {
            if old_active.as_ref() != Some(&bus_name) {
                debug!("Active player (auto, playing): {}", bus_name);
                self.active_player.replace(Some(bus_name));
                self.on_active_player_changed();
            }
            return;
        }

        // If last_playing is paused with metadata, prefer it
        if let Some(ref last) = *self.last_playing.borrow()
            && let Some(player) = players.get(last)
        {
            let p = player.borrow();
            if p.playback_status == PlaybackStatus::Paused && p.metadata.title.is_some() {
                if old_active.as_ref() != Some(last) {
                    debug!("Active player (auto, last playing paused): {}", last);
                    drop(p);
                    self.active_player.replace(Some(last.clone()));
                    self.on_active_player_changed();
                }
                return;
            }
        }

        // If current player is paused with metadata, keep it (don't switch between paused players)
        if let Some(current) = old_active
            && let Some(player) = players.get(current)
        {
            let p = player.borrow();
            if p.playback_status == PlaybackStatus::Paused && p.metadata.title.is_some() {
                return;
            }
        }

        // Find any paused player with metadata
        let paused_with_meta = players
            .values()
            .find(|p| {
                let p = p.borrow();
                p.playback_status == PlaybackStatus::Paused && p.metadata.title.is_some()
            })
            .map(|p| p.borrow().bus_name.clone());

        if let Some(bus_name) = paused_with_meta {
            if old_active.as_ref() != Some(&bus_name) {
                debug!("Active player (auto, paused with metadata): {}", bus_name);
                self.active_player.replace(Some(bus_name));
                self.on_active_player_changed();
            }
            return;
        }

        // Keep current if still valid
        if let Some(current) = old_active
            && players.contains_key(current)
        {
            return;
        }

        // Pick any available player
        let any = players.keys().next().cloned();
        if any != *old_active {
            if let Some(ref bus_name) = any {
                debug!("Active player (auto, fallback): {}", bus_name);
            } else {
                debug!("No active player");
            }
            self.active_player.replace(any);
            self.on_active_player_changed();
        }
    }

    /// Called when the active player changes.
    fn on_active_player_changed(self: &Rc<Self>) {
        self.stop_position_polling();
        self.reset_poll_cancellable();
        self.last_seek.set(None);

        // Write state for CLI to read
        self.write_ipc_state();

        // Fetch position immediately and start polling if playing
        self.poll_position();

        let should_poll = {
            let players = self.players.borrow();
            let active = self.active_player.borrow();
            active
                .as_ref()
                .and_then(|bus| players.get(bus))
                .is_some_and(|p| p.borrow().playback_status == PlaybackStatus::Playing)
        };

        if should_poll {
            self.start_position_polling();
        }
    }

    /// Build the current snapshot from active player state.
    fn build_snapshot(&self) -> MediaSnapshot {
        let players = self.players.borrow();
        let active_bus = self.active_player.borrow();

        let active_player = active_bus
            .as_ref()
            .and_then(|bus| players.get(bus))
            .map(|p| p.borrow());

        match active_player {
            Some(p) => MediaSnapshot {
                available: true,
                player_name: Some(p.player_name.clone()),
                player_id: Some(p.player_id.clone()),
                playback_status: p.playback_status,
                metadata: p.metadata.clone(),
                position: p.position,
                can_play: p.can_play,
                can_pause: p.can_pause,
                can_go_next: p.can_go_next,
                can_go_previous: p.can_go_previous,
                can_seek: p.can_seek,
            },
            None => MediaSnapshot {
                available: !players.is_empty(),
                ..Default::default()
            },
        }
    }

    /// Notify all callbacks with the current snapshot.
    fn notify_callbacks(&self) {
        let snapshot = self.build_snapshot();
        self.callbacks.notify(&snapshot);
    }

    // ========== Metadata Parsing ==========

    fn parse_metadata(variant: &Variant) -> MediaMetadata {
        let mut meta = MediaMetadata::default();

        if let Some(dict) = variant.get::<HashMap<String, Variant>>() {
            if let Some(title) = dict.get("xesam:title") {
                meta.title = title.get::<String>();
            }

            if let Some(artist) = dict.get("xesam:artist") {
                if let Some(artists) = artist.get::<Vec<String>>() {
                    meta.artist = Some(artists.join(", "));
                } else if let Some(artist_str) = artist.get::<String>() {
                    meta.artist = Some(artist_str);
                }
            }

            if let Some(album) = dict.get("xesam:album") {
                meta.album = album.get::<String>();
            }

            if let Some(art_url) = dict.get("mpris:artUrl") {
                meta.art_url = art_url.get::<String>();
            }

            if let Some(url) = dict.get("xesam:url") {
                meta.url = url.get::<String>();
            }

            if let Some(length) = dict.get("mpris:length") {
                meta.length = length
                    .get::<i64>()
                    .or_else(|| length.get::<u64>().map(|v| v as i64));
            }

            if let Some(track_id) = dict.get("mpris:trackid") {
                meta.track_id = track_id
                    .get::<String>()
                    .or_else(|| {
                        track_id
                            .get::<glib::variant::ObjectPath>()
                            .map(|path| path.to_string())
                    })
                    .filter(|id| !id.is_empty() && !id.ends_with("/NoTrack"));
            }
        }

        meta
    }

    // ========== Position Polling ==========

    fn start_position_polling(self: &Rc<Self>) {
        self.stop_position_polling();

        trace!("Starting position polling");
        let this_weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(
            Duration::from_millis(POSITION_POLL_INTERVAL_MS),
            move || {
                let Some(this) = this_weak.upgrade() else {
                    return ControlFlow::Break;
                };

                let should_continue = {
                    let players = this.players.borrow();
                    let active = this.active_player.borrow();
                    active
                        .as_ref()
                        .and_then(|bus| players.get(bus))
                        .is_some_and(|p| p.borrow().playback_status == PlaybackStatus::Playing)
                };

                if !should_continue {
                    this.position_poll_source.replace(None);
                    return ControlFlow::Break;
                }

                this.poll_position();
                ControlFlow::Continue
            },
        );
        self.position_poll_source.replace(Some(source));
    }

    fn stop_position_polling(&self) {
        if let Some(source) = self.position_poll_source.take() {
            trace!("Stopping position polling");
            source.remove();
        }
    }

    fn reset_poll_cancellable(&self) {
        self.poll_cancellable.borrow().cancel();
        self.poll_cancellable.replace(gio::Cancellable::new());
    }

    fn poll_position(self: &Rc<Self>) {
        if self
            .last_seek
            .get()
            .is_some_and(|at| at.elapsed() < SEEK_GRACE_WINDOW)
        {
            return;
        }

        let (bus_name, generation) = {
            let players = self.players.borrow();
            let active = self.active_player.borrow();
            let Some(bus) = active.as_ref() else {
                return;
            };
            let Some(player) = players.get(bus) else {
                return;
            };
            (bus.clone(), player.borrow().track_generation)
        };

        let Some(connection) = self.connection.borrow().clone() else {
            return;
        };

        let cancellable = self.poll_cancellable.borrow().clone();

        connection.call(
            Some(&bus_name),
            MPRIS_PATH,
            PROPERTIES_INTERFACE,
            "Get",
            Some(&(MPRIS_PLAYER_INTERFACE, "Position").to_variant()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NONE,
            DBUS_POLL_TIMEOUT_MS,
            Some(&cancellable),
            clone!(
                #[strong(rename_to = this)]
                self,
                #[strong]
                bus_name,
                move |res| {
                    let players = this.players.borrow();
                    let active = this.active_player.borrow();

                    // Verify we're still polling the same player/track
                    let Some(player) = active
                        .as_ref()
                        .filter(|b| *b == &bus_name)
                        .and_then(|bus| players.get(bus))
                    else {
                        return;
                    };

                    if player.borrow().track_generation != generation {
                        return;
                    }

                    match res {
                        Ok(reply) => {
                            if let Some(inner) = reply.child_value(0).get::<Variant>()
                                && let Some(position) = inner.get::<i64>()
                            {
                                let p = player.borrow();
                                let accept = should_accept_position_poll(
                                    p.position,
                                    position,
                                    p.playback_status,
                                );
                                let changed = accept && p.position != position;
                                drop(p);
                                if changed {
                                    player.borrow_mut().position = position;
                                    drop(players);
                                    drop(active);
                                    this.notify_callbacks();
                                }
                            }
                        }
                        Err(e) => {
                            if !e.matches(gio::IOErrorEnum::Cancelled) {
                                trace!("Position poll failed: {}", e);
                            }
                        }
                    }
                }
            ),
        );
    }

    // ========== Playback Control ==========

    pub fn play_pause(&self) {
        self.call_player_method("PlayPause");
    }

    pub fn next(&self) {
        self.call_player_method("Next");
    }

    pub fn previous(&self) {
        self.call_player_method("Previous");
    }

    /// Set absolute position (in microseconds).
    pub fn set_position(&self, position_us: i64) {
        let track_id = {
            let players = self.players.borrow();
            let active = self.active_player.borrow();
            active
                .as_ref()
                .and_then(|bus| players.get(bus))
                .and_then(|p| p.borrow().metadata.track_id.clone())
        };

        let Some(track_id) = track_id else {
            return;
        };

        let Some((connection, bus_name)) = self.get_active_connection() else {
            return;
        };

        let track_path = match glib::variant::ObjectPath::try_from(track_id.as_str()) {
            Ok(p) => p,
            Err(_) => {
                warn!("Invalid track ID for SetPosition: {}", track_id);
                return;
            }
        };

        self.reset_poll_cancellable();
        self.last_seek.set(Some(Instant::now()));

        // Optimistic update
        {
            let players = self.players.borrow();
            let active = self.active_player.borrow();
            if let Some(player) = active.as_ref().and_then(|bus| players.get(bus)) {
                player.borrow_mut().position = position_us;
            }
        }
        self.notify_callbacks();

        connection.call(
            Some(&bus_name),
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            "SetPosition",
            Some(&(track_path, position_us).to_variant()),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            |res| {
                if let Err(e) = res {
                    warn!("MPRIS SetPosition failed: {}", e);
                }
            },
        );
    }

    fn call_player_method(&self, method: &str) {
        let Some((connection, bus_name)) = self.get_active_connection() else {
            return;
        };

        let method_owned = method.to_string();
        connection.call(
            Some(&bus_name),
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            method,
            None,
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    warn!("MPRIS {} failed: {}", method_owned, e);
                }
            },
        );
    }

    fn get_active_connection(&self) -> Option<(gio::DBusConnection, String)> {
        let connection = self.connection.borrow().clone()?;
        let bus_name = self.active_player.borrow().clone()?;
        Some((connection, bus_name))
    }
}

impl Drop for MediaService {
    fn drop(&mut self) {
        trace!("MediaService dropping, cleaning up resources");
        self.poll_cancellable.borrow().cancel();
        if let Some(source) = self.position_poll_source.take() {
            source.remove();
        }
        self._name_owner_subscription.take();
        self.players.borrow_mut().clear();
    }
}

const MICROSECONDS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3600;

/// Format microseconds as MM:SS or H:MM:SS.
pub fn format_duration(microseconds: i64) -> String {
    if microseconds < 0 {
        return "0:00".to_string();
    }

    let total_seconds = microseconds / MICROSECONDS_PER_SECOND;
    let hours = total_seconds / SECONDS_PER_HOUR;
    let minutes = (total_seconds % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE;
    let seconds = total_seconds % SECONDS_PER_MINUTE;

    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

// ============================================================================
// CLI interface - synchronous, standalone (no GTK main loop required)
// ============================================================================

/// Synchronous media control for CLI usage.
///
/// This is a lightweight, standalone interface that doesn't require GTK or
/// a running main loop. It uses synchronous D-Bus calls to control MPRIS
/// media players.
pub struct MediaCli {
    connection: gio::DBusConnection,
    players: Vec<(String, String)>, // (bus_name, player_name)
    active_player: Option<String>,
}

impl MediaCli {
    /// Create a new CLI media controller.
    ///
    /// Returns `None` if D-Bus connection fails.
    pub fn new() -> Option<Self> {
        let connection =
            gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok()?;

        let mut cli = Self {
            connection,
            players: Vec::new(),
            active_player: None,
        };

        cli.discover_players();
        Some(cli)
    }

    fn discover_players(&mut self) {
        // Call ListNames to find MPRIS players
        let result = self.connection.call_sync(
            Some(DBUS_NAME),
            DBUS_PATH,
            DBUS_INTERFACE,
            "ListNames",
            None,
            Some(glib::VariantTy::new("(as)").unwrap()),
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        );

        let Ok(reply) = result else {
            return;
        };

        let names: Vec<String> = reply
            .child_value(0)
            .iter()
            .filter_map(|v| v.get::<String>())
            .filter(|n| n.starts_with(MPRIS_PREFIX))
            .collect();

        // Build player list with display names
        self.players = names
            .iter()
            .map(|bus_name| {
                let player_id = player_id_from_bus_name(bus_name);
                let player_name = capitalize_first(&player_id);
                (bus_name.clone(), player_name)
            })
            .collect();

        // Check if the panel has a selected player via state file.
        // Use the panel's active player so CLI commands control the same player shown in the UI.
        if let Some(ref bus_name) = super::media_ipc::read_state()
            && self.players.iter().any(|(b, _)| b == bus_name)
        {
            self.active_player = Some(bus_name.clone());
            return;
        }

        // Fallback when panel is not running: first playing player, or first player
        self.active_player = self
            .find_playing_player()
            .or_else(|| self.players.first().map(|(bus, _)| bus.clone()));
    }

    fn find_playing_player(&self) -> Option<String> {
        for (bus_name, _) in &self.players {
            if let Some(status) = self.get_playback_status(bus_name)
                && status == PlaybackStatus::Playing
            {
                return Some(bus_name.clone());
            }
        }
        None
    }

    fn get_playback_status(&self, bus_name: &str) -> Option<PlaybackStatus> {
        let result = self
            .connection
            .call_sync(
                Some(bus_name),
                MPRIS_PATH,
                PROPERTIES_INTERFACE,
                "Get",
                Some(&(MPRIS_PLAYER_INTERFACE, "PlaybackStatus").to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                DBUS_CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .ok()?;

        result
            .child_value(0)
            .get::<Variant>()
            .and_then(|v| v.get::<String>())
            .map(|s| s.parse().unwrap_or(PlaybackStatus::Stopped))
    }

    /// Toggle play/pause on the active player.
    pub fn play_pause(&self) -> Result<(), String> {
        self.call_method("PlayPause")
    }

    /// Skip to next track.
    pub fn next(&self) -> Result<(), String> {
        self.call_method("Next")
    }

    /// Go to previous track.
    pub fn previous(&self) -> Result<(), String> {
        self.call_method("Previous")
    }

    /// Stop playback.
    pub fn stop(&self) -> Result<(), String> {
        self.call_method("Stop")
    }

    /// Get current playback status and metadata.
    pub fn status(&self) -> Result<MediaCliStatus, String> {
        let bus_name = self
            .active_player
            .as_ref()
            .ok_or_else(|| "no media player found".to_string())?;

        // Get all properties at once
        let result = self
            .connection
            .call_sync(
                Some(bus_name),
                MPRIS_PATH,
                PROPERTIES_INTERFACE,
                "GetAll",
                Some(&(MPRIS_PLAYER_INTERFACE,).to_variant()),
                Some(glib::VariantTy::new("(a{sv})").unwrap()),
                gio::DBusCallFlags::NONE,
                DBUS_CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("failed to get player properties: {}", e))?;

        // Parse properties dict
        let props_variant = result.child_value(0);
        let props: std::collections::HashMap<String, Variant> =
            props_variant.get().unwrap_or_default();

        let playback_status = props
            .get("PlaybackStatus")
            .and_then(|v| v.get::<String>())
            .map(|s| s.parse().unwrap_or(PlaybackStatus::Stopped))
            .unwrap_or(PlaybackStatus::Stopped);

        let metadata = props
            .get("Metadata")
            .map(MediaService::parse_metadata)
            .unwrap_or_default();

        let position = props
            .get("Position")
            .and_then(|v| v.get::<i64>())
            .unwrap_or(0);

        // Get player display name
        let player_name = self
            .players
            .iter()
            .find(|(b, _)| b == bus_name)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| bus_name.clone());

        Ok(MediaCliStatus {
            player_name,
            playback_status,
            title: metadata.title,
            artist: metadata.artist,
            position,
            length: metadata.length,
        })
    }

    fn call_method(&self, method: &str) -> Result<(), String> {
        let bus_name = self
            .active_player
            .as_ref()
            .ok_or_else(|| "no media player found".to_string())?;

        self.connection
            .call_sync(
                Some(bus_name),
                MPRIS_PATH,
                MPRIS_PLAYER_INTERFACE,
                method,
                None,
                None,
                gio::DBusCallFlags::NONE,
                DBUS_CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|e| format!("MPRIS {} failed: {}", method, e))?;

        Ok(())
    }
}

/// Status information returned by MediaCli::status().
#[derive(Debug)]
pub struct MediaCliStatus {
    pub player_name: String,
    pub playback_status: PlaybackStatus,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub position: i64,
    pub length: Option<i64>,
}

impl std::fmt::Display for MediaCliStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status_icon = match self.playback_status {
            PlaybackStatus::Playing => "▶",
            PlaybackStatus::Paused => "⏸",
            PlaybackStatus::Stopped => "⏹",
        };

        write!(f, "{} {}", status_icon, self.player_name)?;

        if let Some(ref title) = self.title {
            write!(f, "\n  {}", title)?;
            if let Some(ref artist) = self.artist {
                write!(f, " - {}", artist)?;
            }
        }

        // Show position/duration if available
        if self.position > 0 || self.length.is_some() {
            let pos_str = format_duration(self.position);
            if let Some(length) = self.length {
                let len_str = format_duration(length);
                write!(f, "\n  {} / {}", pos_str, len_str)?;
            } else {
                write!(f, "\n  {}", pos_str)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playback_status_from_str() {
        assert_eq!("Playing".parse(), Ok(PlaybackStatus::Playing));
        assert_eq!("Paused".parse(), Ok(PlaybackStatus::Paused));
        assert_eq!("Stopped".parse(), Ok(PlaybackStatus::Stopped));
        assert_eq!("Unknown".parse(), Ok(PlaybackStatus::Stopped));
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(30_000_000), "0:30");
        assert_eq!(format_duration(90_000_000), "1:30");
        assert_eq!(format_duration(3_661_000_000), "1:01:01");
        assert_eq!(format_duration(-1000), "0:00");
    }

    #[test]
    fn test_media_snapshot_default() {
        let snapshot = MediaSnapshot::default();
        assert!(!snapshot.available);
        assert!(snapshot.player_name.is_none());
        assert_eq!(snapshot.playback_status, PlaybackStatus::Stopped);
    }

    #[test]
    fn test_parse_metadata_normalizes_track_id() {
        let parse_track_id = |track_id: Variant| {
            let metadata = HashMap::from([("mpris:trackid".to_string(), track_id)]).to_variant();
            MediaService::parse_metadata(&metadata).track_id
        };

        let no_track =
            glib::variant::ObjectPath::try_from("/org/mpris/MediaPlayer2/TrackList/NoTrack")
                .unwrap();
        assert_eq!(parse_track_id(no_track.to_variant()), None);
        assert_eq!(parse_track_id("".to_variant()), None);

        let real_track = "/org/chromium/MediaPlayer2/TrackList/Track/1";
        let real_track_path = glib::variant::ObjectPath::try_from(real_track).unwrap();
        assert_eq!(
            parse_track_id(real_track_path.to_variant()).as_deref(),
            Some(real_track)
        );
    }

    #[test]
    fn test_youtube_thumbnail_url() {
        let expected = Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg".to_string());
        assert_eq!(
            youtube_thumbnail_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            expected
        );
        let synthetic_expected =
            Some("https://i.ytimg.com/vi/abcdefghijk/maxresdefault.jpg".to_string());
        assert_eq!(
            youtube_thumbnail_url("https://youtu.be/abcdefghijk?t=30"),
            synthetic_expected
        );
        assert_eq!(
            youtube_thumbnail_url("https://www.youtube.com/shorts/abcdefghijk"),
            synthetic_expected
        );
    }

    #[test]
    fn test_youtube_thumbnail_url_rejects_unrelated_or_invalid_urls() {
        assert_eq!(
            youtube_thumbnail_url("https://example.com/watch?v=abcdefghijk"),
            None
        );
        assert_eq!(
            youtube_thumbnail_url("https://youtube.com.evil.test/watch?v=abcdefghijk"),
            None
        );
        assert_eq!(
            youtube_thumbnail_url("https://www.youtube.com/watch?v=too-short"),
            None
        );
    }

    #[test]
    fn test_youtube_source_identity_uses_video_id() {
        let base = "https://www.youtube.com/watch?v=abcdefghijk";

        assert!(!source_urls_differ(
            base,
            "https://youtu.be/abcdefghijk?t=30#player"
        ));
        assert!(!source_urls_differ(
            base,
            "https://www.youtube.com/watch?v=abcdefghijk&list=PL123&pp=test"
        ));
        assert!(source_urls_differ(
            base,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(source_urls_differ(
            "https://example.com/watch?t=1",
            "https://example.com/watch?t=2"
        ));
        assert!(source_urls_differ(
            base,
            "https://example.com/watch?v=abcdefghijk"
        ));
    }

    #[test]
    fn test_youtube_thumbnail_fallback_url() {
        assert_eq!(
            youtube_thumbnail_fallback_url("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg"),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg".into())
        );
        assert_eq!(
            youtube_thumbnail_fallback_url("https://example.com/vi/abcdefghijk/maxresdefault.jpg"),
            None
        );
        assert_eq!(
            youtube_thumbnail_fallback_url("https://i.ytimg.com/vi/too-short/maxresdefault.jpg"),
            None
        );
    }

    #[test]
    fn test_preserve_same_track_metadata() {
        let previous = MediaMetadata {
            title: Some("Video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/video.png".into()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            length: Some(534_000_000),
            ..Default::default()
        };
        let mut current = MediaMetadata {
            title: previous.title.clone(),
            url: previous.url.clone(),
            ..Default::default()
        };

        preserve_same_track_metadata(&previous, &mut current);

        assert_eq!(current.length, previous.length);
        assert_eq!(current.art_url, previous.art_url);
        assert_eq!(current.url, previous.url);
    }

    #[test]
    fn test_same_youtube_track_retains_previous_native_artwork() {
        let previous = MediaMetadata {
            title: Some("Video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/native.png".into()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            length: Some(534_000_000),
            track_id: Some("/track/1".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            url: previous.url.clone(),
            track_id: previous.track_id.clone(),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, previous.art_url);
        assert_eq!(resolved.length, previous.length);
    }

    #[test]
    fn test_new_same_track_artwork_wins_over_previous_artwork() {
        let previous = MediaMetadata {
            title: Some("Video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/old.png".into()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            art_url: Some("file:///tmp/new.png".into()),
            url: previous.url.clone(),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url.as_deref(), Some("file:///tmp/new.png"));
    }

    #[test]
    fn test_browser_back_state_preserves_previous_artwork() {
        let previous = MediaMetadata {
            title: Some("Video".into()),
            art_url: Some("file:///tmp/video.png".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: Some("Firefox".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Paused);

        assert_eq!(resolved.art_url, previous.art_url);
    }

    #[test]
    fn test_changed_track_id_does_not_preserve_previous_artwork() {
        let previous = MediaMetadata {
            track_id: Some("old".into()),
            art_url: Some("file:///tmp/old.png".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            track_id: Some("new".into()),
            title: Some("New track".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, None);
    }

    #[test]
    fn test_do_not_preserve_metadata_for_new_track() {
        let previous = MediaMetadata {
            title: Some("Old video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/old.png".into()),
            url: Some("https://example.com/old-video".into()),
            length: Some(534_000_000),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: Some("New video".into()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.length, None);
        assert_eq!(
            resolved.art_url.as_deref(),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg")
        );
    }

    #[test]
    fn test_preserve_metadata_without_url_by_title_and_artist() {
        let previous = MediaMetadata {
            title: Some("Track".into()),
            artist: Some("Artist".into()),
            art_url: Some("file:///tmp/track.png".into()),
            length: Some(180_000_000),
            ..Default::default()
        };
        let mut current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            ..Default::default()
        };

        preserve_same_track_metadata(&previous, &mut current);

        assert_eq!(current.length, previous.length);
        assert_eq!(current.art_url, previous.art_url);
    }

    #[test]
    fn test_different_track_ids_veto_title_artist_fallback() {
        let previous = MediaMetadata {
            title: Some("Track".into()),
            artist: Some("Artist".into()),
            art_url: Some("file:///tmp/old.png".into()),
            length: Some(180_000_000),
            track_id: Some("/track/old".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            track_id: Some("/track/new".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, None);
        assert_eq!(resolved.length, None);
    }

    #[test]
    fn test_different_track_ids_veto_equal_url() {
        let previous = MediaMetadata {
            title: Some("Track".into()),
            artist: Some("Artist".into()),
            art_url: Some("file:///tmp/old.png".into()),
            url: Some("https://example.com/track".into()),
            length: Some(180_000_000),
            track_id: Some("/track/old".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            url: previous.url.clone(),
            track_id: Some("/track/new".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, None);
        assert_eq!(resolved.length, None);
    }

    #[test]
    fn test_different_titles_veto_equal_url() {
        let previous = MediaMetadata {
            title: Some("Old track".into()),
            art_url: Some("file:///tmp/old.png".into()),
            url: Some("https://example.com/radio".into()),
            length: Some(180_000_000),
            ..Default::default()
        };
        let mut current = previous.clone();
        current.title = Some("New track".into());
        current.art_url = None;
        current.length = None;

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, None);
        assert_eq!(resolved.length, None);
    }

    #[test]
    fn test_different_non_empty_urls_mark_track_changed() {
        let previous = MediaMetadata {
            url: Some("https://example.com/old".into()),
            ..Default::default()
        };
        let mut current = previous.clone();
        current.url = Some("https://example.com/new".into());

        assert!(has_track_changed(&previous, &current));

        current.url = None;
        assert!(!has_track_changed(&previous, &current));
    }

    #[test]
    fn test_same_track_restores_omitted_track_id() {
        let previous = MediaMetadata {
            title: Some("Track".into()),
            artist: Some("Artist".into()),
            url: Some("https://example.com/track".into()),
            track_id: Some("/track/current".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            artist: previous.artist.clone(),
            url: previous.url.clone(),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.track_id, previous.track_id);
    }

    #[test]
    fn test_same_track_id_preserves_partial_metadata() {
        let previous = MediaMetadata {
            title: Some("Track".into()),
            artist: Some("Artist".into()),
            art_url: Some("file:///tmp/art.png".into()),
            url: Some("https://example.com/track".into()),
            length: Some(180_000_000),
            track_id: Some("/track/current".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            title: previous.title.clone(),
            track_id: previous.track_id.clone(),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.art_url, previous.art_url);
        assert_eq!(resolved.url, previous.url);
        assert_eq!(resolved.length, previous.length);
    }

    #[test]
    fn test_meaningful_preview_artwork_replaces_previous_track() {
        let previous = MediaMetadata {
            title: Some("Playing video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/playing.png".into()),
            url: Some("https://example.com/playing-video".into()),
            ..Default::default()
        };
        let mut preview = MediaMetadata {
            title: Some("Short preview".into()),
            artist: Some("Other creator".into()),
            art_url: youtube_thumbnail_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            ..Default::default()
        };
        let preview_art = preview.art_url.clone();

        preserve_same_track_metadata(&previous, &mut preview);

        assert_eq!(preview.art_url, preview_art);
        assert_ne!(preview.art_url, previous.art_url);
    }

    #[test]
    fn test_incomplete_active_metadata_preserves_previous_presentation() {
        let previous = MediaMetadata {
            title: Some("Playing video".into()),
            artist: Some("Creator".into()),
            art_url: Some("file:///tmp/playing.png".into()),
            url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            length: Some(534_000_000),
            ..Default::default()
        };
        let mut current = MediaMetadata::default();

        preserve_incomplete_active_metadata(&previous, &mut current, PlaybackStatus::Paused);

        assert_eq!(current.title, previous.title);
        assert_eq!(current.art_url, previous.art_url);
        assert_eq!(current.length, previous.length);
    }

    #[test]
    fn test_titleless_metadata_with_artwork_is_not_overwritten() {
        let previous = MediaMetadata {
            title: Some("Old track".into()),
            art_url: Some("file:///tmp/old.png".into()),
            ..Default::default()
        };
        let mut current = MediaMetadata {
            art_url: Some("file:///tmp/new.png".into()),
            ..Default::default()
        };

        preserve_incomplete_active_metadata(&previous, &mut current, PlaybackStatus::Playing);

        assert_eq!(current.title, None);
        assert_eq!(current.art_url.as_deref(), Some("file:///tmp/new.png"));
    }

    #[test]
    fn test_incomplete_new_track_metadata_is_not_overwritten() {
        let previous = MediaMetadata {
            title: Some("Old track".into()),
            art_url: Some("file:///tmp/old.png".into()),
            url: Some("https://example.com/old".into()),
            length: Some(180_000_000),
            track_id: Some("/track/old".into()),
            ..Default::default()
        };
        let current = MediaMetadata {
            url: Some("https://example.com/new".into()),
            track_id: Some("/track/new".into()),
            ..Default::default()
        };

        let resolved = resolve_metadata(&previous, current, PlaybackStatus::Playing);

        assert_eq!(resolved.title, None);
        assert_eq!(resolved.art_url, None);
        assert_eq!(resolved.url.as_deref(), Some("https://example.com/new"));
        assert_eq!(resolved.track_id.as_deref(), Some("/track/new"));
        assert!(has_track_changed(&previous, &resolved));
    }

    #[test]
    fn test_stopped_player_does_not_preserve_incomplete_metadata() {
        let previous = MediaMetadata {
            title: Some("Playing video".into()),
            art_url: Some("file:///tmp/playing.png".into()),
            ..Default::default()
        };
        let mut current = MediaMetadata::default();

        preserve_incomplete_active_metadata(&previous, &mut current, PlaybackStatus::Stopped);

        assert_eq!(current.title, None);
        assert_eq!(current.art_url, None);
    }

    #[test]
    fn test_position_poll_ignores_transient_zero_while_playing() {
        assert!(!should_accept_position_poll(
            120_000_000,
            0,
            PlaybackStatus::Playing
        ));
        assert!(should_accept_position_poll(
            120_000_000,
            0,
            PlaybackStatus::Paused
        ));
        assert!(should_accept_position_poll(
            120_000_000,
            30_000_000,
            PlaybackStatus::Playing
        ));
    }
}
