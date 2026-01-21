//! MediaService - MPRIS D-Bus integration for media player control.
//!
//! This service discovers and controls MPRIS-compatible media players on the session bus.
//! It provides:
//! - Player discovery (org.mpris.MediaPlayer2.*)
//! - Playback state monitoring (Playing/Paused/Stopped)
//! - Metadata access (title, artist, album, art URL, duration)
//! - Playback control (play/pause, next, previous, seek, volume)
//! - Position tracking with periodic polling when playing
//!
//! ## MPRIS D-Bus Interface
//!
//! - Bus: Session
//! - Service names: `org.mpris.MediaPlayer2.*` (e.g., `org.mpris.MediaPlayer2.spotify`)
//! - Object path: `/org/mpris/MediaPlayer2`
//! - Interfaces:
//!   - `org.mpris.MediaPlayer2` - Base interface (Identity, Quit, etc.)
//!   - `org.mpris.MediaPlayer2.Player` - Playback control and state

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

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
    /// Album art URL (mpris:artUrl) - can be file:// or http(s)://.
    pub art_url: Option<String>,
    /// Track URL (xesam:url) - useful for identifying web players.
    pub url: Option<String>,
    /// Track duration in microseconds (mpris:length).
    pub length: Option<i64>,
    /// Track ID (mpris:trackid).
    pub track_id: Option<String>,
}

/// Canonical snapshot of media player state.
#[derive(Debug, Clone)]
pub struct MediaSnapshot {
    /// Whether any MPRIS player is available.
    pub available: bool,
    /// Name of the active player (e.g., "Spotify", "Firefox").
    pub player_name: Option<String>,
    /// Raw player ID for icon lookup (e.g., "spotify", "firefox").
    /// Derived from bus name, suitable for desktop app icon detection.
    pub player_id: Option<String>,
    /// Bus name of the active player (e.g., "org.mpris.MediaPlayer2.spotify").
    pub player_bus_name: Option<String>,
    /// Current playback status.
    pub playback_status: PlaybackStatus,
    /// Track metadata.
    pub metadata: MediaMetadata,
    /// Current position in microseconds.
    pub position: i64,
    /// Player volume (0.0 - 1.0+).
    pub volume: f64,
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
    /// Whether the player can be controlled at all.
    pub can_control: bool,
    /// List of available player bus names.
    pub available_players: Vec<String>,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            available: false,
            player_name: None,
            player_id: None,
            player_bus_name: None,
            playback_status: PlaybackStatus::Stopped,
            metadata: MediaMetadata::default(),
            position: 0,
            volume: 1.0,
            can_play: false,
            can_pause: false,
            can_go_next: false,
            can_go_previous: false,
            can_seek: false,
            can_control: false,
            available_players: Vec::new(),
        }
    }
}

impl MediaSnapshot {
    /// Create an empty snapshot (no player available).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Check if there's an active player with metadata.
    #[allow(dead_code)] // Part of public API for potential future use
    pub fn has_track(&self) -> bool {
        self.available && self.metadata.title.is_some()
    }
}

/// Shared, process-wide media service.
pub struct MediaService {
    /// Connection to the session bus.
    connection: RefCell<Option<gio::DBusConnection>>,
    /// Proxy to the active player's Player interface.
    player_proxy: RefCell<Option<gio::DBusProxy>>,
    /// Current snapshot of media state.
    snapshot: RefCell<MediaSnapshot>,
    /// Registered callbacks for state changes.
    callbacks: Callbacks<MediaSnapshot>,
    /// Signal subscription for NameOwnerChanged (player appear/disappear).
    _name_owner_subscription: RefCell<Option<gio::SignalSubscription>>,
    /// Signal subscription for PropertiesChanged on the active player.
    _properties_subscription: RefCell<Option<gio::SignalSubscription>>,
    /// Timer for position polling when playing.
    position_poll_source: RefCell<Option<glib::SourceId>>,
    /// Cancellable for in-flight D-Bus operations on the current player.
    /// Cancelled when switching players to abort stale requests.
    player_cancellable: RefCell<gio::Cancellable>,
    /// Generation counter for track changes. Incremented on every track change
    /// to invalidate in-flight position poll requests for the old track.
    track_generation: RefCell<u64>,
}

impl MediaService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            connection: RefCell::new(None),
            player_proxy: RefCell::new(None),
            snapshot: RefCell::new(MediaSnapshot::empty()),
            callbacks: Callbacks::new(),
            _name_owner_subscription: RefCell::new(None),
            _properties_subscription: RefCell::new(None),
            position_poll_source: RefCell::new(None),
            player_cancellable: RefCell::new(gio::Cancellable::new()),
            track_generation: RefCell::new(0),
        });

        Self::init_dbus(&service);
        service
    }

    /// Get the global MediaService singleton.
    ///
    /// # Thread Safety
    ///
    /// This method must only be called from the GTK main thread. The singleton
    /// is thread-local to align with GTK's single-threaded event loop model.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<MediaService> = MediaService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Select the best player from available players.
    ///
    /// Priority order:
    /// 1. Keep current player if it's still in the list and is playing/paused with metadata
    /// 2. Otherwise, pick a different player (prefer any player over a stopped one with no metadata)
    ///
    /// Returns `None` if no players available, or the best candidate's bus name.
    fn pick_best_player(&self, players: &[String]) -> Option<String> {
        if players.is_empty() {
            return None;
        }

        let current_snapshot = self.snapshot.borrow();

        // Keep current player if still valid and has meaningful state
        if let Some(current_bus) = &current_snapshot.player_bus_name
            && players.contains(current_bus)
        {
            // Keep it if playing, or paused with metadata
            let has_metadata = current_snapshot
                .metadata
                .title
                .as_ref()
                .is_some_and(|t| !t.trim().is_empty());
            let dominated = current_snapshot.playback_status == PlaybackStatus::Playing
                || (current_snapshot.playback_status == PlaybackStatus::Paused && has_metadata);

            if dominated {
                return Some(current_bus.clone());
            }

            // Current player is stopped/empty - try to find a different one
            // (The new player might be better, we won't know until we connect to it)
            for player in players {
                if player != current_bus {
                    debug!(
                        "Current player {} is stopped/empty, switching to {}",
                        current_bus, player
                    );
                    return Some(player.clone());
                }
            }

            // No other players, stick with current (widget will hide it)
            return Some(current_bus.clone());
        }

        // No current player or it disappeared, pick first available
        players.first().cloned()
    }

    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&MediaSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        // Immediately invoke with current snapshot (only the new callback)
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    /// Unregister a callback by its ID.
    ///
    /// Returns `true` if the callback was found and removed, `false` otherwise.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    /// Get a clone of the current snapshot.
    pub fn snapshot(&self) -> MediaSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Update the snapshot using a closure and notify all callbacks.
    ///
    /// This helper reduces boilerplate for the common pattern of:
    /// 1. Borrow snapshot mutably
    /// 2. Apply changes
    /// 3. Clone for notification
    /// 4. Drop borrow
    /// 5. Notify callbacks
    fn update_and_notify<F>(&self, f: F)
    where
        F: FnOnce(&mut MediaSnapshot),
    {
        let snapshot = {
            let mut s = self.snapshot.borrow_mut();
            f(&mut s);
            s.clone()
        };
        self.callbacks.notify(&snapshot);
    }

    // ========== D-Bus Initialization ==========

    fn init_dbus(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        // Connect to session bus asynchronously
        gio::bus_get(
            gio::BusType::Session,
            None::<&gio::Cancellable>,
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
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
                        // NameOwnerChanged(name: s, old_owner: s, new_owner: s)
                        // Only trigger discovery if the name is an MPRIS player
                        if let Some(name) = signal.parameters.child_value(0).str()
                            && name.starts_with(MPRIS_PREFIX)
                            && let Some(this) = this_weak.upgrade()
                        {
                            this.discover_players();
                        }
                    },
                );
                this._name_owner_subscription.replace(Some(subscription));

                // Initial player discovery
                this.discover_players();
            },
        );
    }

    /// Discover available MPRIS players on the bus.
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
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let reply = match res {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Failed to list D-Bus names: {}", e);
                        return;
                    }
                };

                // Parse the (as) response
                let names: Vec<String> = reply
                    .child_value(0)
                    .iter()
                    .filter_map(|v| v.get::<String>())
                    .collect();

                // Filter for MPRIS players
                let players: Vec<String> = names
                    .into_iter()
                    .filter(|n| n.starts_with(MPRIS_PREFIX))
                    .collect();

                debug!(
                    "Discovered {} MPRIS player(s): {:?}",
                    players.len(),
                    players
                );

                // Update available players list
                {
                    let mut snapshot = this.snapshot.borrow_mut();
                    snapshot.available_players = players.clone();
                }

                // Determine best player and switch if needed
                let current_player = this.snapshot.borrow().player_bus_name.clone();
                let best_player = this.pick_best_player(&players);

                match (&current_player, &best_player) {
                    (_, None) => {
                        // No players available
                        this.clear_player();
                    }
                    (None, Some(player)) => {
                        // No current player, select new one
                        this.select_player(player);
                    }
                    (Some(current), Some(best)) if current != best => {
                        // Switch to better player
                        debug!("Switching from {} to {}", current, best);
                        this.select_player(best);
                    }
                    (Some(current), Some(_)) if !players.contains(current) => {
                        // Current player disappeared, select new one
                        this.select_player(best_player.as_ref().unwrap());
                    }
                    _ => {
                        // Just notify about updated player list
                        let snapshot = this.snapshot.borrow().clone();
                        this.callbacks.notify(&snapshot);
                    }
                }
            },
        );
    }

    /// Select and connect to a specific player.
    fn select_player(self: &Rc<Self>, bus_name: &str) {
        debug!("Selecting MPRIS player: {}", bus_name);

        // Cancel any in-flight operations for the previous player
        self.player_cancellable.borrow().cancel();
        // Create a fresh cancellable for the new player
        self.player_cancellable.replace(gio::Cancellable::new());

        // Clear old subscription
        self._properties_subscription.replace(None);
        self.stop_position_polling();

        let this_weak = Rc::downgrade(self);
        let bus_name_owned = bus_name.to_string();
        let bus_name_for_proxy = bus_name_owned.clone();
        let cancellable = self.player_cancellable.borrow().clone();

        // Create proxy for the Player interface
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None::<&gio::DBusInterfaceInfo>,
            &bus_name_for_proxy,
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            Some(&cancellable),
            move |res| {
                let this = match this_weak.upgrade() {
                    Some(t) => t,
                    None => return,
                };

                let proxy = match res {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to create MPRIS proxy for {}: {}", bus_name_owned, e);
                        this.clear_player();
                        return;
                    }
                };

                debug!("Created MPRIS proxy for {}", bus_name_owned);
                this.player_proxy.replace(Some(proxy.clone()));

                // Extract player ID from bus name for icon lookup
                // e.g., "org.mpris.MediaPlayer2.spotify" -> "spotify"
                // e.g., "org.mpris.MediaPlayer2.firefox.instance_1234" -> "firefox"
                let player_id = bus_name_owned.strip_prefix(MPRIS_PREFIX).map(|s| {
                    // Take only the first segment (before any dots)
                    // This handles cases like "firefox.instance_1234" -> "firefox"
                    s.split('.').next().unwrap_or(s).to_string()
                });

                // Extract player name (capitalized version for display)
                let player_name = player_id
                    .as_ref()
                    .map(|id| {
                        // Capitalize first letter
                        let mut chars = id.chars();
                        match chars.next() {
                            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                            None => id.to_string(),
                        }
                    })
                    .unwrap_or_else(|| bus_name_owned.clone());

                // Update snapshot with player info
                {
                    let mut snapshot = this.snapshot.borrow_mut();
                    snapshot.available = true;
                    snapshot.player_name = Some(player_name);
                    snapshot.player_id = player_id;
                    snapshot.player_bus_name = Some(bus_name_owned.clone());
                }

                // Read initial properties
                this.update_from_proxy(&proxy);

                // Subscribe to PropertiesChanged
                let connection = proxy.connection();
                let this_weak = Rc::downgrade(&this);
                let proxy_for_cb = proxy.clone();
                let subscription = connection.subscribe_to_signal(
                    Some(&bus_name_owned),
                    Some(PROPERTIES_INTERFACE),
                    Some("PropertiesChanged"),
                    Some(MPRIS_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_signal| {
                        if let Some(this) = this_weak.upgrade() {
                            this.update_from_proxy(&proxy_for_cb);
                        }
                    },
                );
                this._properties_subscription.replace(Some(subscription));

                // Notify listeners
                let snapshot = this.snapshot.borrow().clone();
                this.callbacks.notify(&snapshot);

                // Start position polling if playing
                if snapshot.playback_status == PlaybackStatus::Playing {
                    this.start_position_polling();
                }
            },
        );
    }

    /// Clear the active player (no player available).
    fn clear_player(&self) {
        debug!("Clearing MPRIS player");

        // Cancel any in-flight operations
        self.player_cancellable.borrow().cancel();
        // Create a fresh cancellable for future operations
        self.player_cancellable.replace(gio::Cancellable::new());

        self.player_proxy.replace(None);
        self._properties_subscription.replace(None);
        self.stop_position_polling();

        self.update_and_notify(|snapshot| {
            let available_players = snapshot.available_players.clone();
            *snapshot = MediaSnapshot::empty();
            snapshot.available_players = available_players;
        });
    }

    /// Update snapshot from proxy properties.
    fn update_from_proxy(self: &Rc<Self>, proxy: &gio::DBusProxy) {
        // Update snapshot and get status change info for polling management
        let (old_status, new_status, track_changed) = {
            let mut snapshot = self.snapshot.borrow_mut();
            let old_status = snapshot.playback_status;
            let old_track_id = snapshot.metadata.track_id.clone();
            let old_title = snapshot.metadata.title.clone();

            // PlaybackStatus
            if let Some(status) = proxy.cached_property("PlaybackStatus")
                && let Some(s) = status.get::<String>()
            {
                snapshot.playback_status = s.parse().unwrap_or_default();
            }

            // Metadata
            if let Some(metadata) = proxy.cached_property("Metadata") {
                snapshot.metadata = Self::parse_metadata(&metadata);
            }

            // Volume
            if let Some(volume) = proxy.cached_property("Volume")
                && let Some(v) = volume.get::<f64>()
            {
                snapshot.volume = v;
            }

            // Detect track change by comparing track IDs OR titles.
            // Some players (e.g., Firefox) use a constant track ID for all tracks,
            // so we also check if the title changed as a fallback.
            let track_id_changed = old_track_id != snapshot.metadata.track_id;
            let title_changed = old_title.is_some()
                && snapshot.metadata.title.is_some()
                && old_title != snapshot.metadata.title;
            let track_changed = track_id_changed || title_changed;

            // Reset position to 0 when track changes to avoid showing stale position
            // from the previous track. The next poll will update to the correct value.
            if track_changed {
                snapshot.position = 0;
            }
            // Note: Position doesn't reliably emit PropertiesChanged and the cached
            // value is often stale. We poll it separately via poll_position().
            // Don't update position here (except on track change) to avoid
            // overwriting good values with stale ones.

            // Capabilities
            snapshot.can_play = proxy
                .cached_property("CanPlay")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            snapshot.can_pause = proxy
                .cached_property("CanPause")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            snapshot.can_go_next = proxy
                .cached_property("CanGoNext")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            snapshot.can_go_previous = proxy
                .cached_property("CanGoPrevious")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            snapshot.can_seek = proxy
                .cached_property("CanSeek")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            snapshot.can_control = proxy
                .cached_property("CanControl")
                .and_then(|v| v.get::<bool>())
                .unwrap_or(true);

            (old_status, snapshot.playback_status, track_changed)
        };

        // Increment track generation on track change to invalidate in-flight position polls
        if track_changed {
            *self.track_generation.borrow_mut() += 1;
        }

        // Manage position polling based on playback status change
        if old_status != new_status {
            if new_status == PlaybackStatus::Playing {
                self.start_position_polling();
            } else {
                // Increment generation when stopping to invalidate any in-flight polls
                *self.track_generation.borrow_mut() += 1;
                self.stop_position_polling();
            }
        }

        // If track changed while playing, do an immediate position poll
        // to get the new track's position quickly
        if track_changed && new_status == PlaybackStatus::Playing {
            self.poll_position();
        }

        // If current player became stopped with no metadata, try to find a better one
        // This handles the case where e.g. a Slack call ends while music is playing elsewhere
        let should_try_switch = {
            let snapshot = self.snapshot.borrow();
            let has_metadata = snapshot
                .metadata
                .title
                .as_ref()
                .is_some_and(|t| !t.trim().is_empty());
            snapshot.playback_status == PlaybackStatus::Stopped
                && !has_metadata
                && snapshot.available_players.len() > 1
        };

        if should_try_switch {
            debug!("Current player stopped with no metadata, re-evaluating players");
            self.discover_players();
            return; // discover_players will notify
        }

        // Notify after releasing the borrow
        let snapshot = self.snapshot.borrow().clone();
        self.callbacks.notify(&snapshot);
    }

    /// Parse MPRIS metadata dict into MediaMetadata.
    fn parse_metadata(variant: &Variant) -> MediaMetadata {
        let mut meta = MediaMetadata::default();

        // Metadata is a{sv}
        if let Some(dict) = variant.get::<HashMap<String, Variant>>() {
            // Title
            if let Some(title) = dict.get("xesam:title") {
                meta.title = title.get::<String>();
            }

            // Artist (can be array of strings)
            if let Some(artist) = dict.get("xesam:artist") {
                if let Some(artists) = artist.get::<Vec<String>>() {
                    meta.artist = Some(artists.join(", "));
                } else if let Some(artist_str) = artist.get::<String>() {
                    meta.artist = Some(artist_str);
                }
            }

            // Album
            if let Some(album) = dict.get("xesam:album") {
                meta.album = album.get::<String>();
            }

            // Art URL
            if let Some(art_url) = dict.get("mpris:artUrl") {
                meta.art_url = art_url.get::<String>();
            }

            // Track URL (useful for identifying web players)
            if let Some(url) = dict.get("xesam:url") {
                meta.url = url.get::<String>();
            }

            // Length (duration in microseconds)
            // Note: Some players (like Spotify) report length as u64, others as i64
            if let Some(length) = dict.get("mpris:length") {
                meta.length = length
                    .get::<i64>()
                    .or_else(|| length.get::<u64>().map(|v| v as i64));
            }

            // Track ID
            if let Some(track_id) = dict.get("mpris:trackid") {
                // Track ID is an object path (o), but we store as string
                if let Some(id) = track_id.get::<String>() {
                    meta.track_id = Some(id);
                } else if let Some(path) = track_id.get::<glib::variant::ObjectPath>() {
                    meta.track_id = Some(path.to_string());
                }
            }
        }

        meta
    }

    // ========== Position Polling ==========

    /// Start periodic position polling while playback is active.
    fn start_position_polling(self: &Rc<Self>) {
        // Stop any existing polling first to prevent duplicate timers
        self.stop_position_polling();

        trace!("Starting position polling");
        let this_weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(
            Duration::from_millis(POSITION_POLL_INTERVAL_MS),
            move || {
                let Some(this) = this_weak.upgrade() else {
                    return ControlFlow::Break;
                };

                // Only continue polling if still playing
                if this.snapshot.borrow().playback_status != PlaybackStatus::Playing {
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

    fn poll_position(self: &Rc<Self>) {
        let Some(proxy) = self.player_proxy.borrow().clone() else {
            return;
        };

        // Position property needs to be fetched directly (not cached reliably)
        let connection = proxy.connection();
        let bus_name = proxy.name().map(|n| n.to_string());
        let Some(bus_name) = bus_name else {
            return;
        };

        // Capture current generation to detect stale responses after track/status change
        let generation_at_request = *self.track_generation.borrow();

        let cancellable = self.player_cancellable.borrow().clone();

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
                move |res| {
                    let current_gen = *this.track_generation.borrow();

                    // Ignore stale results if playback stopped while request was in flight.
                    // This prevents the "jump back on pause" issue.
                    if this.snapshot.borrow().playback_status != PlaybackStatus::Playing {
                        return;
                    }

                    // Ignore stale results if track changed while request was in flight.
                    // This prevents showing old track's position on the new track.
                    if current_gen != generation_at_request {
                        return;
                    }

                    match res {
                        Ok(reply) => {
                            // Response is (v) where v contains the actual value
                            if let Some(inner) = reply.child_value(0).get::<Variant>()
                                && let Some(position) = inner.get::<i64>()
                            {
                                // Only notify if position actually changed
                                if this.snapshot.borrow().position != position {
                                    this.update_and_notify(|snapshot| {
                                        snapshot.position = position;
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            // Don't log cancelled operations (expected when switching players)
                            if !e.matches(gio::IOErrorEnum::Cancelled) {
                                trace!("Position poll failed (transient): {}", e);
                            }
                        }
                    }
                }
            ),
        );
    }

    // ========== Playback Control ==========

    /// Toggle play/pause.
    pub fn play_pause(&self) {
        self.call_player_method("PlayPause");
    }

    /// Start playback.
    #[allow(dead_code)] // Part of public API, use play_pause() for toggle behavior
    pub fn play(&self) {
        self.call_player_method("Play");
    }

    /// Pause playback.
    #[allow(dead_code)] // Part of public API, use play_pause() for toggle behavior
    pub fn pause(&self) {
        self.call_player_method("Pause");
    }

    /// Stop playback.
    #[allow(dead_code)] // Part of public API for potential future use
    pub fn stop(&self) {
        self.call_player_method("Stop");
    }

    /// Skip to next track.
    pub fn next(&self) {
        self.call_player_method("Next");
    }

    /// Skip to previous track.
    pub fn previous(&self) {
        self.call_player_method("Previous");
    }

    /// Seek by offset (in microseconds). Positive = forward, negative = backward.
    #[allow(dead_code)]
    pub fn seek(&self, offset_us: i64) {
        let Some((connection, bus_name)) = self.get_player_connection() else {
            return;
        };

        connection.call(
            Some(&bus_name),
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            "Seek",
            Some(&(offset_us,).to_variant()),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            |res| {
                if let Err(e) = res {
                    warn!("MPRIS Seek failed: {}", e);
                }
            },
        );
    }

    /// Set absolute position (in microseconds).
    ///
    /// This optimistically updates the local position immediately for responsive UI,
    /// then sends the seek command to the player. If the command fails, the next
    /// position poll will correct the displayed value.
    pub fn set_position(&self, position_us: i64) {
        let track_id = {
            let snapshot = self.snapshot.borrow();
            match &snapshot.metadata.track_id {
                Some(id) => id.clone(),
                None => return,
            }
        };

        let Some((connection, bus_name)) = self.get_player_connection() else {
            return;
        };

        // SetPosition takes (TrackId: o, Position: x)
        let track_path = glib::variant::ObjectPath::try_from(track_id.as_str()).ok();
        let Some(track_path) = track_path else {
            warn!("Invalid track ID for SetPosition: {}", track_id);
            return;
        };

        // Optimistically update local position for responsive UI
        self.update_and_notify(|snapshot| {
            snapshot.position = position_us;
        });

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
                    // Note: We don't revert the optimistic update on failure.
                    // The next position poll will correct the displayed value.
                }
            },
        );
    }

    /// Set player volume (0.0 - 1.0+).
    ///
    /// MPRIS allows volumes > 1.0 for amplification, but negative values are invalid.
    /// Invalid values (negative, NaN, infinity) are rejected with a warning.
    #[allow(dead_code)]
    pub fn set_volume(self: &Rc<Self>, volume: f64) {
        // Validate volume - MPRIS allows > 1.0 for amplification, but not negative or non-finite
        if !volume.is_finite() || volume < 0.0 {
            warn!("Invalid volume value: {}", volume);
            return;
        }

        let Some((connection, bus_name)) = self.get_player_connection() else {
            return;
        };

        // Store previous volume for rollback on error
        let previous_volume = self.snapshot.borrow().volume;

        // Optimistically update local snapshot for responsive UI
        self.update_and_notify(|snapshot| {
            snapshot.volume = volume;
        });

        // Volume is set via org.freedesktop.DBus.Properties.Set
        let volume_variant = volume.to_variant();
        let params = (MPRIS_PLAYER_INTERFACE, "Volume", volume_variant).to_variant();

        let this = Rc::downgrade(self);
        connection.call(
            Some(&bus_name),
            MPRIS_PATH,
            PROPERTIES_INTERFACE,
            "Set",
            Some(&params),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            DBUS_CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |res| {
                if let Err(e) = res {
                    warn!("MPRIS set volume failed: {}", e);
                    // Revert optimistic update on error
                    if let Some(this) = this.upgrade() {
                        this.update_and_notify(|snapshot| {
                            snapshot.volume = previous_volume;
                        });
                    }
                }
            },
        );
    }

    /// Switch to a different player by bus name.
    #[allow(dead_code)] // Part of public API for future player selector feature
    pub fn switch_player(self: &Rc<Self>, bus_name: &str) {
        if self.snapshot.borrow().player_bus_name.as_deref() == Some(bus_name) {
            return; // Already on this player
        }

        if self
            .snapshot
            .borrow()
            .available_players
            .contains(&bus_name.to_string())
        {
            self.select_player(bus_name);
        }
    }

    /// Call a simple method on the Player interface (no arguments, no return).
    fn call_player_method(&self, method: &str) {
        let Some((connection, bus_name)) = self.get_player_connection() else {
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

    /// Get the D-Bus connection and bus name for the current player.
    ///
    /// Returns `None` if no player is connected.
    fn get_player_connection(&self) -> Option<(gio::DBusConnection, String)> {
        let proxy = self.player_proxy.borrow().clone()?;
        let connection = proxy.connection();
        let bus_name = proxy.name()?.to_string();
        Some((connection, bus_name))
    }
}

impl Drop for MediaService {
    fn drop(&mut self) {
        trace!("MediaService dropping, cleaning up resources");

        // Cancel any in-flight D-Bus operations
        self.player_cancellable.borrow().cancel();

        // Stop position polling timer (SourceId doesn't auto-remove on drop)
        if let Some(source) = self.position_poll_source.take() {
            source.remove();
        }

        // Signal subscriptions (gio::SignalSubscription) automatically unsubscribe
        // when dropped, so we just need to clear them to trigger the drop.
        self._name_owner_subscription.take();
        self._properties_subscription.take();
    }
}

// ========== Helper Functions ==========

/// Microseconds per second (for duration formatting).
const MICROSECONDS_PER_SECOND: i64 = 1_000_000;
/// Seconds per minute (for duration formatting).
const SECONDS_PER_MINUTE: i64 = 60;
/// Seconds per hour (for duration formatting).
const SECONDS_PER_HOUR: i64 = 3600;

/// Format duration in microseconds to human-readable string (MM:SS or H:MM:SS).
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
        assert_eq!(format_duration(30_000_000), "0:30"); // 30 seconds
        assert_eq!(format_duration(90_000_000), "1:30"); // 1:30
        assert_eq!(format_duration(3_661_000_000), "1:01:01"); // 1h 1m 1s
        assert_eq!(format_duration(-1000), "0:00"); // negative
    }

    #[test]
    fn test_media_snapshot_default() {
        let snapshot = MediaSnapshot::default();
        assert!(!snapshot.available);
        assert!(snapshot.player_name.is_none());
        assert_eq!(snapshot.playback_status, PlaybackStatus::Stopped);
        assert!(!snapshot.has_track());
    }

    #[test]
    fn test_media_snapshot_has_track() {
        let mut snapshot = MediaSnapshot::default();
        assert!(!snapshot.has_track());

        snapshot.available = true;
        assert!(!snapshot.has_track());

        snapshot.metadata.title = Some("Test Track".to_string());
        assert!(snapshot.has_track());
    }
}
