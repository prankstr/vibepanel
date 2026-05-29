//! Shared Open-Meteo weather service.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use gtk4::glib::{self, SourceId};
use gtk4::{gio, prelude::*};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use vibepanel_core::config::{
    DEFAULT_WEATHER_FORECAST_DAYS, DEFAULT_WEATHER_REFRESH_INTERVAL, MAX_WEATHER_FORECAST_DAYS,
    MIN_WEATHER_REFRESH_INTERVAL, WeatherUnits, WeatherWindUnits,
};

use super::callbacks::{CallbackId, Callbacks};
#[cfg(not(test))]
use super::sleep_watcher::SleepWatcher;

const HTTP_TIMEOUT_SECONDS: u64 = 15;
const GEOCLUE_SERVICE: &str = "org.freedesktop.GeoClue2";
const GEOCLUE_MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const GEOCLUE_MANAGER_IFACE: &str = "org.freedesktop.GeoClue2.Manager";
const GEOCLUE_CLIENT_IFACE: &str = "org.freedesktop.GeoClue2.Client";
const GEOCLUE_LOCATION_IFACE: &str = "org.freedesktop.GeoClue2.Location";
const DBUS_PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const DBUS_TIMEOUT_MS: i32 = 5000;
const GEOCLUE_FIX_ATTEMPTS: usize = 10;
const GEOCLUE_FIX_POLL_INTERVAL: Duration = Duration::from_millis(200);
const AUTO_LOCATION_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone)]
struct GeocodeCache {
    query: String,
    location: WeatherLocation,
}

#[derive(Debug, Clone)]
struct AutoLocationCache {
    location: WeatherLocation,
    cached_at: SystemTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWeatherCache {
    location_key: Option<String>,
    units: WeatherUnits,
    wind_units: WeatherWindUnits,
    snapshot: WeatherSnapshot,
}

static GEOCODE_CACHE: OnceLock<Mutex<Option<GeocodeCache>>> = OnceLock::new();
static AUTO_LOCATION_CACHE: OnceLock<Mutex<Option<AutoLocationCache>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedWeatherConfig {
    pub enabled: bool,
    pub auto_locate: bool,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub location: Option<String>,
    pub units: WeatherUnits,
    pub wind_units: Option<WeatherWindUnits>,
    pub forecast_days: u8,
    pub refresh_interval: u64,
}

impl Default for ResolvedWeatherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_locate: false,
            latitude: None,
            longitude: None,
            location: None,
            units: WeatherUnits::Metric,
            wind_units: None,
            forecast_days: DEFAULT_WEATHER_FORECAST_DAYS,
            refresh_interval: DEFAULT_WEATHER_REFRESH_INTERVAL,
        }
    }
}

impl ResolvedWeatherConfig {
    fn normalized(mut self) -> Self {
        self.forecast_days = self.forecast_days.clamp(1, MAX_WEATHER_FORECAST_DAYS);
        self.refresh_interval = self.refresh_interval.max(MIN_WEATHER_REFRESH_INTERVAL);
        self.location = self
            .location
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }

    fn has_location(&self) -> bool {
        matches!((self.latitude, self.longitude), (Some(_), Some(_)))
            || self.location.is_some()
            || self.auto_locate
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    pub available: bool,
    pub is_ready: bool,
    pub loading: bool,
    pub stale: bool,
    pub error: Option<String>,
    pub location: Option<WeatherLocation>,
    pub current: Option<CurrentWeather>,
    pub daily: Vec<DailyForecast>,
    pub last_update: Option<SystemTime>,
    pub units: WeatherUnits,
    pub wind_units: WeatherWindUnits,
}

impl WeatherSnapshot {
    pub fn unknown() -> Self {
        Self {
            available: false,
            is_ready: false,
            loading: false,
            stale: false,
            error: None,
            location: None,
            current: None,
            daily: Vec::new(),
            last_update: None,
            units: WeatherUnits::Metric,
            wind_units: WeatherWindUnits::Kmh,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherLocation {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentWeather {
    pub temperature: f64,
    pub feels_like: Option<f64>,
    pub humidity: Option<u8>,
    pub wind_speed: Option<f64>,
    pub condition: String,
    pub weather_code: Option<i32>,
    pub is_day: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyForecast {
    pub date: String,
    pub condition: String,
    pub weather_code: Option<i32>,
    pub temperature_min: Option<f64>,
    pub temperature_max: Option<f64>,
    pub precipitation_probability: Option<u8>,
}

pub struct WeatherService {
    config: RefCell<ResolvedWeatherConfig>,
    snapshot: RefCell<WeatherSnapshot>,
    callbacks: Callbacks<WeatherSnapshot>,
    timer_source: RefCell<Option<SourceId>>,
    fetch_in_progress: Cell<bool>,
    generation: Cell<u64>,
}

impl WeatherService {
    fn new() -> Rc<Self> {
        let service = Rc::new(Self {
            config: RefCell::new(ResolvedWeatherConfig::default()),
            snapshot: RefCell::new(WeatherSnapshot::unknown()),
            callbacks: Callbacks::new(),
            timer_source: RefCell::new(None),
            fetch_in_progress: Cell::new(false),
            generation: Cell::new(0),
        });

        #[cfg(not(test))]
        {
            // WeatherService is a process-lifetime singleton; refresh after resume
            // so stale laptop wake data is corrected promptly. Unregistration is
            // intentionally not needed for process-lifetime service callbacks.
            let _resume_callback_id = SleepWatcher::global().on_resume(|| {
                WeatherService::global().refresh();
            });
        }

        service
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<WeatherService> = WeatherService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Called by ConfigManager on config load/reload; last call wins.
    /// Consumers should only use `connect`, `snapshot`, and `refresh`.
    pub fn configure(self: &Rc<Self>, config: ResolvedWeatherConfig) {
        let config = config.normalized();
        if *self.config.borrow() == config {
            return;
        }

        self.config.replace(config.clone());
        self.generation.set(self.generation.get().wrapping_add(1));
        self.fetch_in_progress.set(false);
        self.stop_timer();

        if !config.enabled {
            self.set_snapshot(WeatherSnapshot::unknown());
            return;
        }

        if !config.has_location() {
            self.set_snapshot(WeatherSnapshot {
                available: true,
                is_ready: true,
                error: Some("Weather location is not configured".to_string()),
                units: config.units,
                wind_units: resolved_wind_units(&config),
                ..WeatherSnapshot::unknown()
            });
            return;
        }

        // Seed from the on-disk cache so the bar shows the last known weather
        // (marked stale) immediately instead of a placeholder while we refetch.
        let seeded = load_cached_snapshot()
            .filter(|cached| cached_matches_config(cached, &config))
            .map(|cached| {
                let mut snapshot = cached.snapshot;
                snapshot.stale = true;
                snapshot.loading = true;
                snapshot.error = None;
                snapshot
            });

        self.set_snapshot(seeded.unwrap_or(WeatherSnapshot {
            available: true,
            loading: true,
            units: config.units,
            wind_units: resolved_wind_units(&config),
            ..WeatherSnapshot::unknown()
        }));
        self.refresh();
        self.start_timer();
    }

    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&WeatherSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> WeatherSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn refresh(&self) {
        // NOTE: Unlike UpdatesService, weather does NOT gate on NetworkService.
        // HTTP failures already degrade to stale-data retention; a second
        // reachability gate risks false negatives (captive portals, VPN-only).
        let config = self.config.borrow().clone();
        if !config.enabled || !config.has_location() || self.fetch_in_progress.get() {
            return;
        }

        self.fetch_weather_async(config, self.generation.get());
    }

    fn start_timer(self: &Rc<Self>) {
        let interval = self
            .config
            .borrow()
            .refresh_interval
            .try_into()
            .unwrap_or(u32::MAX);
        let this_weak = Rc::downgrade(self);
        let source_id = glib::timeout_add_seconds_local(interval, move || {
            if let Some(this) = this_weak.upgrade() {
                this.refresh();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
        self.timer_source.replace(Some(source_id));
    }

    fn stop_timer(&self) {
        if let Some(source_id) = self.timer_source.borrow_mut().take() {
            source_id.remove();
        }
    }

    fn fetch_weather_async(&self, config: ResolvedWeatherConfig, generation: u64) {
        debug!("WeatherService: starting refresh");
        self.fetch_in_progress.set(true);
        self.update_snapshot(|s| {
            s.loading = true;
            s.available = true;
            s.error = None;
            s.units = config.units;
            s.wind_units = resolved_wind_units(&config);
        });

        std::thread::spawn(move || {
            let result = fetch_weather(&config);
            glib::idle_add_once(move || {
                WeatherService::global().apply_fetch_result(generation, result);
            });
        });
    }

    fn apply_fetch_result(&self, generation: u64, result: Result<WeatherSnapshot, String>) {
        if generation != self.generation.get() {
            return;
        }
        self.fetch_in_progress.set(false);

        match result {
            Ok(snapshot) => {
                save_cached_snapshot(&snapshot, &self.config.borrow());
                self.set_snapshot(snapshot);
            }
            Err(error) => self.apply_error(error),
        }
    }

    fn apply_error(&self, error: String) {
        warn!("WeatherService: {error}");
        self.update_snapshot(|s| {
            s.loading = false;
            s.is_ready = true;
            s.stale = s.current.is_some();
            s.error = Some(error);
        });
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut WeatherSnapshot)) {
        let mut snapshot = self.snapshot.borrow_mut();
        update(&mut snapshot);
        let clone = snapshot.clone();
        drop(snapshot);
        self.callbacks.notify(&clone);
    }

    fn set_snapshot(&self, snapshot: WeatherSnapshot) {
        self.snapshot.replace(snapshot.clone());
        self.callbacks.notify(&snapshot);
    }
}

impl Drop for WeatherService {
    fn drop(&mut self) {
        self.stop_timer();
    }
}

fn fetch_weather(config: &ResolvedWeatherConfig) -> Result<WeatherSnapshot, String> {
    let location = resolve_location(config)?;
    let response = fetch_open_meteo(&location, config)?;

    Ok(WeatherSnapshot {
        available: true,
        is_ready: true,
        loading: false,
        stale: false,
        error: None,
        location: Some(location),
        current: Some(parse_current(&response)?),
        daily: parse_daily(&response),
        last_update: Some(SystemTime::now()),
        units: config.units,
        wind_units: resolved_wind_units(config),
    })
}

fn resolve_location(config: &ResolvedWeatherConfig) -> Result<WeatherLocation, String> {
    if let (Some(latitude), Some(longitude)) = (config.latitude, config.longitude) {
        return Ok(WeatherLocation {
            name: config
                .location
                .clone()
                .unwrap_or_else(|| format!("{latitude:.4}, {longitude:.4}")),
            latitude,
            longitude,
        });
    }

    if let Some(location) = &config.location {
        return geocode_city_cached(location);
    }

    if config.auto_locate {
        return auto_locate();
    }

    Err("Weather location is not configured".to_string())
}

/// Path to the persisted last-known weather snapshot.
///
/// Location: `$XDG_CACHE_HOME/vibepanel/weather.json`
/// Default: `~/.cache/vibepanel/weather.json`
fn weather_cache_path() -> Option<std::path::PathBuf> {
    let cache_home = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    Some(cache_home.join("vibepanel").join("weather.json"))
}

/// Load the last successfully fetched weather snapshot from disk, if any.
fn load_cached_snapshot() -> Option<PersistedWeatherCache> {
    let path = weather_cache_path()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<PersistedWeatherCache>(&contents) {
        Ok(cache) if cache.snapshot.current.is_some() => Some(cache),
        Ok(_) => None,
        Err(err) => {
            debug!(
                "Ignoring unreadable weather cache {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Persist a successful weather snapshot to disk for fast stale display on the
/// next startup. Failures are non-fatal.
fn save_cached_snapshot(snapshot: &WeatherSnapshot, config: &ResolvedWeatherConfig) {
    if snapshot.current.is_none() {
        return;
    }
    let Some(path) = weather_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        debug!(
            "Failed to create weather cache dir {}: {err}",
            parent.display()
        );
        return;
    }
    let cache = PersistedWeatherCache {
        location_key: cache_location_key(config),
        units: config.units,
        wind_units: resolved_wind_units(config),
        snapshot: snapshot.clone(),
    };
    match serde_json::to_string(&cache) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                debug!("Failed to write weather cache {}: {err}", path.display());
            }
        }
        Err(err) => debug!("Failed to serialize weather snapshot: {err}"),
    }
}

/// Whether a cached snapshot matches the configured location closely enough to
/// reuse as stale display. Units must match and the location key — which encodes
/// the location source mode (coordinates, named location, or auto) — must be
/// identical, so caches from different modes never cross-populate.
fn cached_matches_config(cache: &PersistedWeatherCache, config: &ResolvedWeatherConfig) -> bool {
    cache.units == config.units
        && cache.wind_units == resolved_wind_units(config)
        && cache.location_key == cache_location_key(config)
        && cache.snapshot.location.is_some()
}

/// Build a cache key that encodes the configured location source mode so
/// explicit coordinates, named locations, and auto-location never share a key.
fn cache_location_key(config: &ResolvedWeatherConfig) -> Option<String> {
    if let (Some(lat), Some(lon)) = (config.latitude, config.longitude) {
        return Some(format!("coords:{lat:.4},{lon:.4}"));
    }
    if let Some(location) = config
        .location
        .as_deref()
        .map(str::trim)
        .filter(|location| !location.is_empty())
    {
        return Some(format!("location:{}", normalize_location(location)));
    }
    if config.auto_locate {
        return Some("auto".to_string());
    }
    None
}

fn normalize_location(location: &str) -> String {
    location
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn geocode_city_cached(city: &str) -> Result<WeatherLocation, String> {
    let cache = GEOCODE_CACHE.get_or_init(|| Mutex::new(None));
    if let Some(cached) = cache.lock().ok().and_then(|guard| guard.as_ref().cloned())
        && cached.query == city
    {
        return Ok(cached.location);
    }

    let location = geocode_city(city)?;
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(GeocodeCache {
            query: city.to_string(),
            location: location.clone(),
        });
    }
    Ok(location)
}

fn geocode_city(city: &str) -> Result<WeatherLocation, String> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=en&format=json",
        encode_query(city)
    );
    let response: GeocodingResponse = parse_json(&http_get(&url)?, "geocoding")?;
    let result = response
        .results
        .and_then(|r| r.into_iter().next())
        .ok_or_else(|| format!("Weather location not found: {city}"))?;

    Ok(WeatherLocation {
        name: join_location_name(&result.name, result.admin1, result.country),
        latitude: result.latitude,
        longitude: result.longitude,
    })
}

fn auto_locate() -> Result<WeatherLocation, String> {
    if let Some(location) = cached_auto_location(SystemTime::now()) {
        return Ok(location);
    }

    let location = match geoclue_locate() {
        Ok(location) => Ok(location),
        Err(error) => {
            warn!("GeoClue2 auto-location failed: {error}; falling back to IP geolocation");
            ip_auto_locate().map_err(|ip_error| {
                format!("GeoClue2 failed: {error}; IP geolocation failed: {ip_error}")
            })
        }
    }?;
    cache_auto_location(location.clone(), SystemTime::now());
    Ok(location)
}

fn cached_auto_location(now: SystemTime) -> Option<WeatherLocation> {
    let cache = AUTO_LOCATION_CACHE.get_or_init(|| Mutex::new(None));
    cache
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
        .filter(|cached| auto_location_cache_is_fresh(cached, now))
        .map(|cached| cached.location)
}

fn cache_auto_location(location: WeatherLocation, cached_at: SystemTime) {
    let cache = AUTO_LOCATION_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(AutoLocationCache {
            location,
            cached_at,
        });
    }
}

fn auto_location_cache_is_fresh(cached: &AutoLocationCache, now: SystemTime) -> bool {
    now.duration_since(cached.cached_at)
        .map(|age| age < AUTO_LOCATION_CACHE_TTL)
        .unwrap_or(true)
}

fn ip_auto_locate() -> Result<WeatherLocation, String> {
    // ip-api only supports HTTPS on its paid tier.
    let response: AutoLocateResponse = parse_json(
        &http_get("http://ip-api.com/json/?fields=status,message,lat,lon,city,country")?,
        "auto-location",
    )?;
    if response.status.as_deref() != Some("success") {
        return Err(response
            .message
            .unwrap_or_else(|| "Auto-location failed".to_string()));
    }

    let latitude = response.lat.ok_or("Auto-location missing latitude")?;
    let longitude = response.lon.ok_or("Auto-location missing longitude")?;
    Ok(WeatherLocation {
        name: join_location_name_or_coords(response.city, response.country, latitude, longitude),
        latitude,
        longitude,
    })
}

fn geoclue_locate() -> Result<WeatherLocation, String> {
    let manager = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        GEOCLUE_SERVICE,
        GEOCLUE_MANAGER_PATH,
        GEOCLUE_MANAGER_IFACE,
        None::<&gio::Cancellable>,
    )
    .map_err(|err| format!("GeoClue2 manager unavailable: {err}"))?;

    let client_path = manager
        .call_sync(
            "GetClient",
            None::<&glib::Variant>,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .map_err(|err| format!("GeoClue2 GetClient failed: {err}"))?
        .child_value(0)
        .get::<glib::variant::ObjectPath>()
        .map(|path| path.as_str().to_string())
        .ok_or("GeoClue2 GetClient returned no client path")?;

    let client = match gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        GEOCLUE_SERVICE,
        &client_path,
        GEOCLUE_CLIENT_IFACE,
        None::<&gio::Cancellable>,
    ) {
        Ok(client) => client,
        Err(err) => {
            delete_geoclue_client(&manager, &client_path);
            return Err(format!("GeoClue2 client unavailable: {err}"));
        }
    };

    let result = geoclue_locate_with_client(&client);
    cleanup_geoclue_client(&manager, &client_path, &client);

    result
}

fn geoclue_locate_with_client(client: &gio::DBusProxy) -> Result<WeatherLocation, String> {
    set_dbus_property(
        client,
        GEOCLUE_CLIENT_IFACE,
        "DesktopId",
        "io.github.vibepanel".to_variant(),
    )?;
    set_dbus_property(
        client,
        GEOCLUE_CLIENT_IFACE,
        "TimeThreshold",
        10_u32.to_variant(),
    )?;

    client
        .call_sync(
            "Start",
            None::<&glib::Variant>,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .map_err(|err| format!("GeoClue2 Start failed: {err}"))?;

    poll_geoclue_location(client)
}

fn cleanup_geoclue_client(manager: &gio::DBusProxy, client_path: &str, client: &gio::DBusProxy) {
    if let Err(err) = client.call_sync(
        "Stop",
        None::<&glib::Variant>,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
    ) {
        debug!("GeoClue2 Stop failed: {err}");
    }

    delete_geoclue_client(manager, client_path);
}

fn delete_geoclue_client(manager: &gio::DBusProxy, client_path: &str) {
    let Ok(path) = glib::variant::ObjectPath::try_from(client_path) else {
        debug!("GeoClue2 DeleteClient skipped: invalid client path {client_path}");
        return;
    };

    if let Err(err) = manager.call_sync(
        "DeleteClient",
        Some(&(path,).to_variant()),
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
    ) {
        debug!("GeoClue2 DeleteClient failed: {err}");
    }
}

fn poll_geoclue_location(client: &gio::DBusProxy) -> Result<WeatherLocation, String> {
    let mut last_error = None;

    for _ in 0..GEOCLUE_FIX_ATTEMPTS {
        // TODO(weather): replace this worker-thread polling with a GeoClue
        // LocationUpdated signal subscription if auto-location needs tighter behavior.
        match geoclue_location_from_client(client) {
            Ok(location) => return Ok(location),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(GEOCLUE_FIX_POLL_INTERVAL);
    }

    Err(last_error.unwrap_or_else(|| "GeoClue2 did not provide a location".to_string()))
}

fn geoclue_location_from_client(client: &gio::DBusProxy) -> Result<WeatherLocation, String> {
    let path = get_dbus_object_path_property(client, GEOCLUE_CLIENT_IFACE, "Location")?;
    if path == "/" {
        return Err("GeoClue2 has no location fix yet".to_string());
    }

    let location = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        GEOCLUE_SERVICE,
        &path,
        GEOCLUE_LOCATION_IFACE,
        None::<&gio::Cancellable>,
    )
    .map_err(|err| format!("GeoClue2 location unavailable: {err}"))?;

    let latitude = get_dbus_f64_property(&location, GEOCLUE_LOCATION_IFACE, "Latitude")?;
    let longitude = get_dbus_f64_property(&location, GEOCLUE_LOCATION_IFACE, "Longitude")?;
    if latitude == 0.0 && longitude == 0.0 {
        return Err("GeoClue2 returned empty coordinates".to_string());
    }

    Ok(location_from_coords(latitude, longitude))
}

fn location_from_coords(latitude: f64, longitude: f64) -> WeatherLocation {
    WeatherLocation {
        name: format!("{latitude:.4}, {longitude:.4}"),
        latitude,
        longitude,
    }
}

fn set_dbus_property(
    proxy: &gio::DBusProxy,
    interface: &str,
    property: &str,
    value: glib::Variant,
) -> Result<(), String> {
    proxy
        .connection()
        .call_sync(
            Some(GEOCLUE_SERVICE),
            proxy.object_path().as_str(),
            DBUS_PROPERTIES_IFACE,
            "Set",
            Some(&(interface, property, value).to_variant()),
            None::<&glib::VariantTy>,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .map(|_| ())
        .map_err(|err| format!("GeoClue2 failed to set {property}: {err}"))
}

fn get_dbus_object_path_property(
    proxy: &gio::DBusProxy,
    interface: &str,
    property: &str,
) -> Result<String, String> {
    let variant = get_dbus_property(proxy, interface, property)?;
    variant
        .get::<glib::variant::ObjectPath>()
        .map(|path| path.as_str().to_string())
        .ok_or_else(|| format!("GeoClue2 property {property} was not an object path"))
}

fn get_dbus_f64_property(
    proxy: &gio::DBusProxy,
    interface: &str,
    property: &str,
) -> Result<f64, String> {
    let variant = get_dbus_property(proxy, interface, property)?;
    variant
        .get::<f64>()
        .ok_or_else(|| format!("GeoClue2 property {property} was not a float"))
}

fn get_dbus_property(
    proxy: &gio::DBusProxy,
    interface: &str,
    property: &str,
) -> Result<glib::Variant, String> {
    proxy
        .connection()
        .call_sync(
            Some(GEOCLUE_SERVICE),
            proxy.object_path().as_str(),
            DBUS_PROPERTIES_IFACE,
            "Get",
            Some(&(interface, property).to_variant()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .map(|variant| variant.child_value(0).child_value(0))
        .map_err(|err| format!("GeoClue2 failed to read {property}: {err}"))
}

fn fetch_open_meteo(
    location: &WeatherLocation,
    config: &ResolvedWeatherConfig,
) -> Result<OpenMeteoResponse, String> {
    let (temp_unit, precip_unit) = match config.units {
        WeatherUnits::Metric => ("celsius", "mm"),
        WeatherUnits::Imperial => ("fahrenheit", "inch"),
    };
    let wind_unit = open_meteo_wind_unit(resolved_wind_units(config));
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={:.6}&longitude={:.6}&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,is_day&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max&forecast_days={}&timezone=auto&temperature_unit={temp_unit}&wind_speed_unit={wind_unit}&precipitation_unit={precip_unit}",
        location.latitude, location.longitude, config.forecast_days,
    );
    parse_json(&http_get(&url)?, "weather")
}

fn resolved_wind_units(config: &ResolvedWeatherConfig) -> WeatherWindUnits {
    // m/s is the SI standard and the common everyday unit in many metric
    // regions (e.g. the Nordics), so default to it when unset. Imperial still
    // defaults to mph; users can override either via `wind_units`.
    config.wind_units.unwrap_or(match config.units {
        WeatherUnits::Metric => WeatherWindUnits::MetersPerSecond,
        WeatherUnits::Imperial => WeatherWindUnits::Mph,
    })
}

fn open_meteo_wind_unit(units: WeatherWindUnits) -> &'static str {
    match units {
        WeatherWindUnits::Kmh => "kmh",
        WeatherWindUnits::Mph => "mph",
        WeatherWindUnits::MetersPerSecond => "ms",
    }
}

fn http_get(url: &str) -> Result<String, String> {
    let response = minreq::get(url)
        .with_timeout(HTTP_TIMEOUT_SECONDS)
        .send()
        .map_err(|err| format!("Weather request failed: {err}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "Weather request returned HTTP {}",
            response.status_code
        ));
    }
    response
        .as_str()
        .map(str::to_string)
        .map_err(|err| format!("Weather response was not valid UTF-8: {err}"))
}

fn parse_json<'a, T: Deserialize<'a>>(body: &'a str, label: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|err| format!("Failed to parse {label} response: {err}"))
}

fn parse_current(response: &OpenMeteoResponse) -> Result<CurrentWeather, String> {
    let current = response.current.as_ref().ok_or("Missing current weather")?;
    let weather_code = current.weather_code;
    Ok(CurrentWeather {
        temperature: current.temperature_2m.ok_or("Missing temperature")?,
        feels_like: current.apparent_temperature,
        humidity: current.relative_humidity_2m.and_then(to_u8),
        wind_speed: current.wind_speed_10m,
        condition: condition_for_code(weather_code).to_string(),
        weather_code,
        is_day: current.is_day.map(|v| v == 1),
    })
}

fn parse_daily(response: &OpenMeteoResponse) -> Vec<DailyForecast> {
    let Some(daily) = &response.daily else {
        return Vec::new();
    };

    daily
        .time
        .iter()
        .enumerate()
        .map(|(i, date)| {
            let weather_code = at(&daily.weather_code, i);
            DailyForecast {
                date: date.clone(),
                condition: condition_for_code(weather_code).to_string(),
                weather_code,
                temperature_min: at(&daily.temperature_2m_min, i),
                temperature_max: at(&daily.temperature_2m_max, i),
                precipitation_probability: at(&daily.precipitation_probability_max, i)
                    .and_then(to_u8),
            }
        })
        .collect()
}

fn at<T: Copy>(values: &[Option<T>], index: usize) -> Option<T> {
    values.get(index).copied().flatten()
}

fn to_u8(value: i64) -> Option<u8> {
    u8::try_from(value).ok()
}

fn condition_for_code(code: Option<i32>) -> &'static str {
    // Widgets should prefer weather_code for icons/i18n and treat this as fallback text.
    match code {
        Some(0) => "Clear",
        Some(1 | 2) => "Partly cloudy",
        Some(3) => "Cloudy",
        Some(45 | 48) => "Fog",
        Some(51 | 53 | 55 | 56 | 57) => "Drizzle",
        Some(61 | 63 | 65 | 66 | 67 | 80 | 81 | 82) => "Rain",
        Some(71 | 73 | 75 | 77 | 85 | 86) => "Snow",
        Some(95 | 96 | 99) => "Thunderstorm",
        _ => "Unknown",
    }
}

fn join_location_name(name: &str, admin1: Option<String>, country: Option<String>) -> String {
    match (admin1, country) {
        (Some(admin1), Some(country)) => format!("{name}, {admin1}, {country}"),
        (_, Some(country)) => format!("{name}, {country}"),
        _ => name.to_string(),
    }
}

fn join_location_name_or_coords(
    city: Option<String>,
    country: Option<String>,
    latitude: f64,
    longitude: f64,
) -> String {
    match (city, country) {
        (Some(city), Some(country)) => format!("{city}, {country}"),
        (Some(city), None) => city,
        _ => format!("{latitude:.4}, {longitude:.4}"),
    }
}

fn encode_query(input: &str) -> String {
    input
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            // Open-Meteo accepts application/x-www-form-urlencoded spaces.
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct GeocodingResponse {
    results: Option<Vec<GeocodingResult>>,
}

#[derive(Debug, Deserialize)]
struct GeocodingResult {
    name: String,
    latitude: f64,
    longitude: f64,
    country: Option<String>,
    admin1: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AutoLocateResponse {
    status: Option<String>,
    message: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    city: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    current: Option<OpenMeteoCurrent>,
    daily: Option<OpenMeteoDaily>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: Option<f64>,
    relative_humidity_2m: Option<i64>,
    apparent_temperature: Option<f64>,
    weather_code: Option<i32>,
    wind_speed_10m: Option<f64>,
    is_day: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoDaily {
    time: Vec<String>,
    weather_code: Vec<Option<i32>>,
    temperature_2m_max: Vec<Option<f64>>,
    temperature_2m_min: Vec<Option<f64>>,
    precipitation_probability_max: Vec<Option<i64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_config_limits() {
        let config = ResolvedWeatherConfig {
            forecast_days: 99,
            refresh_interval: 1,
            ..ResolvedWeatherConfig::default()
        }
        .normalized();

        assert_eq!(config.forecast_days, MAX_WEATHER_FORECAST_DAYS);
        assert_eq!(config.refresh_interval, MIN_WEATHER_REFRESH_INTERVAL);
    }

    #[test]
    fn has_location_checks_all_sources() {
        assert!(!ResolvedWeatherConfig::default().has_location());
        assert!(
            ResolvedWeatherConfig {
                auto_locate: true,
                ..ResolvedWeatherConfig::default()
            }
            .has_location()
        );
        assert!(
            ResolvedWeatherConfig {
                latitude: Some(1.0),
                longitude: Some(2.0),
                ..ResolvedWeatherConfig::default()
            }
            .has_location()
        );
    }

    #[test]
    fn explicit_coords_win_over_location() {
        let location = resolve_location(&ResolvedWeatherConfig {
            latitude: Some(1.0),
            longitude: Some(2.0),
            location: Some("Test".to_string()),
            auto_locate: true,
            ..ResolvedWeatherConfig::default()
        })
        .unwrap();

        assert_eq!(location.name, "Test");
        assert_eq!(location.latitude, 1.0);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query("New York, NY"), "New+York%2C+NY");
    }

    #[test]
    fn cached_matches_config_respects_explicit_coordinates() {
        let mut snapshot = WeatherSnapshot::unknown();
        snapshot.location = Some(WeatherLocation {
            name: "Cached".to_string(),
            latitude: 52.52,
            longitude: 13.40,
        });
        snapshot.current = Some(CurrentWeather {
            temperature: 20.0,
            feels_like: None,
            humidity: None,
            wind_speed: None,
            condition: "Clear".to_string(),
            weather_code: None,
            is_day: None,
        });
        let cache = PersistedWeatherCache {
            location_key: Some("coords:52.5000,13.4100".to_string()),
            units: WeatherUnits::Metric,
            wind_units: WeatherWindUnits::MetersPerSecond,
            snapshot,
        };

        // Matching explicit coords (same rounded key) are accepted.
        assert!(cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                latitude: Some(52.50),
                longitude: Some(13.41),
                ..ResolvedWeatherConfig::default()
            }
        ));

        // Different explicit coords produce a different key and are rejected.
        assert!(!cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                latitude: Some(40.71),
                longitude: Some(-74.0),
                ..ResolvedWeatherConfig::default()
            }
        ));

        // A coords cache must not satisfy an auto-location config (different mode).
        assert!(!cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                auto_locate: true,
                ..ResolvedWeatherConfig::default()
            }
        ));

        // No cached location is never a match.
        let empty_cache = PersistedWeatherCache {
            location_key: Some("auto".to_string()),
            units: WeatherUnits::Metric,
            wind_units: WeatherWindUnits::MetersPerSecond,
            snapshot: WeatherSnapshot::unknown(),
        };
        assert!(!cached_matches_config(
            &empty_cache,
            &ResolvedWeatherConfig {
                auto_locate: true,
                ..ResolvedWeatherConfig::default()
            }
        ));
    }

    #[test]
    fn cached_matches_config_rejects_different_units_or_location_query() {
        let mut snapshot = WeatherSnapshot::unknown();
        snapshot.location = Some(WeatherLocation {
            name: "Berlin".to_string(),
            latitude: 52.52,
            longitude: 13.40,
        });
        snapshot.current = Some(CurrentWeather {
            temperature: 20.0,
            feels_like: None,
            humidity: None,
            wind_speed: None,
            condition: "Clear".to_string(),
            weather_code: None,
            is_day: None,
        });
        let cache = PersistedWeatherCache {
            location_key: Some("location:berlin".to_string()),
            units: WeatherUnits::Metric,
            wind_units: WeatherWindUnits::MetersPerSecond,
            snapshot,
        };

        assert!(cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                location: Some("  BERLIN  ".to_string()),
                wind_units: Some(WeatherWindUnits::MetersPerSecond),
                ..ResolvedWeatherConfig::default()
            }
        ));
        assert!(!cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                location: Some("Paris".to_string()),
                wind_units: Some(WeatherWindUnits::MetersPerSecond),
                ..ResolvedWeatherConfig::default()
            }
        ));
        assert!(!cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                location: Some("Berlin".to_string()),
                units: WeatherUnits::Imperial,
                wind_units: Some(WeatherWindUnits::MetersPerSecond),
                ..ResolvedWeatherConfig::default()
            }
        ));
        // Same location but a differing wind unit must not match.
        assert!(!cached_matches_config(
            &cache,
            &ResolvedWeatherConfig {
                location: Some("Berlin".to_string()),
                wind_units: Some(WeatherWindUnits::Kmh),
                ..ResolvedWeatherConfig::default()
            }
        ));
    }

    #[test]
    fn resolves_default_wind_units_from_unit_system() {
        let mut config = ResolvedWeatherConfig::default();
        assert_eq!(
            resolved_wind_units(&config),
            WeatherWindUnits::MetersPerSecond
        );

        config.units = WeatherUnits::Imperial;
        assert_eq!(resolved_wind_units(&config), WeatherWindUnits::Mph);

        config.wind_units = Some(WeatherWindUnits::Kmh);
        assert_eq!(resolved_wind_units(&config), WeatherWindUnits::Kmh);
        assert_eq!(
            open_meteo_wind_unit(WeatherWindUnits::MetersPerSecond),
            "ms"
        );
    }

    #[test]
    fn auto_location_cache_expires_after_ttl() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 60 * 60);
        let fresh = AutoLocationCache {
            location: location_from_coords(1.0, 2.0),
            cached_at: now - AUTO_LOCATION_CACHE_TTL + Duration::from_secs(1),
        };
        let expired = AutoLocationCache {
            location: location_from_coords(1.0, 2.0),
            cached_at: now - AUTO_LOCATION_CACHE_TTL,
        };

        assert!(auto_location_cache_is_fresh(&fresh, now));
        assert!(!auto_location_cache_is_fresh(&expired, now));
    }

    #[test]
    fn missing_location_is_available_with_actionable_error() {
        let service = WeatherService::new();
        service.configure(ResolvedWeatherConfig {
            enabled: true,
            ..ResolvedWeatherConfig::default()
        });

        let snapshot = service.snapshot();
        assert!(snapshot.available);
        assert!(snapshot.is_ready);
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Weather location is not configured")
        );
    }

    #[test]
    fn unchanged_configure_does_not_reset_snapshot() {
        let service = WeatherService::new();
        let config = ResolvedWeatherConfig {
            enabled: true,
            ..ResolvedWeatherConfig::default()
        };

        service.configure(config.clone());
        service.update_snapshot(|snapshot| {
            snapshot.error = Some("kept".to_string());
        });
        service.configure(config);

        assert_eq!(service.snapshot().error.as_deref(), Some("kept"));
    }
}
