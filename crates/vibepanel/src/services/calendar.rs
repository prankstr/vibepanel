//! DankCalendar-backed event service.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use gtk4::glib;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::callbacks::{CallbackId, Callbacks};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_LIMIT: usize = 5000;
const GTK_CALENDAR_GRID_PADDING_DAYS: i64 = 14;
const FETCH_ERROR_MESSAGE: &str = "Error fetching calendar events. Check logs for details.";

#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEvent {
    pub calendar_name: String,
    pub color: Option<String>,
    pub title: String,
    pub location: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub all_day: bool,
}

impl CalendarEvent {
    /// Last local day occupied by this event; calendar end times are exclusive.
    pub(crate) fn final_day(&self) -> NaiveDate {
        if self.end > self.start {
            (self.end - chrono::Duration::nanoseconds(1)).date_naive()
        } else {
            self.start.date_naive()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CalendarSnapshot {
    pub focus_month: Option<NaiveDate>,
    pub loading: bool,
    pub error: Option<String>,
    pub backend_available: bool,
    pub events_by_date: BTreeMap<NaiveDate, Vec<Arc<CalendarEvent>>>,
}

impl CalendarSnapshot {
    pub fn matches_focus_month(&self, date: NaiveDate) -> bool {
        self.focus_month == Some(month_start(date))
    }
}

pub struct CalendarService {
    snapshot: RefCell<CalendarSnapshot>,
    callbacks: Callbacks<CalendarSnapshot>,
    generation: Cell<u64>,
    worker_active: Cell<bool>,
    pending_refresh: RefCell<Option<RefreshRequest>>,
}

#[derive(Clone, Copy)]
struct RefreshRequest {
    generation: u64,
    focus_date: NaiveDate,
}

impl CalendarService {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            snapshot: RefCell::new(CalendarSnapshot::default()),
            callbacks: Callbacks::new(),
            generation: Cell::new(0),
            worker_active: Cell::new(false),
            pending_refresh: RefCell::new(None),
        })
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<CalendarService> = CalendarService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&CalendarSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        let snapshot = self.snapshot();
        self.callbacks.notify_single(id, &snapshot);
        id
    }

    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    pub fn snapshot(&self) -> CalendarSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn ensure_discovery(&self, focus_date: NaiveDate) {
        if self.worker_active.get() || self.snapshot.borrow().backend_available {
            return;
        }
        self.refresh(focus_date);
    }

    pub fn refresh(&self, focus_date: NaiveDate) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);

        let snapshot = {
            let mut snapshot = self.snapshot.borrow_mut();
            prepare_refresh_snapshot(&mut snapshot, focus_date);
            snapshot.clone()
        };

        // Keep one blocking socket worker active and replace queued work with the
        // latest navigation request. Queue before notifying because callbacks may
        // re-enter refresh() with newer work.
        self.pending_refresh.replace(Some(RefreshRequest {
            generation,
            focus_date,
        }));
        self.start_pending_refresh();
        self.callbacks.notify(&snapshot);
    }

    fn start_pending_refresh(&self) {
        if self.worker_active.get() {
            return;
        }
        let Some(request) = self.pending_refresh.borrow_mut().take() else {
            return;
        };
        self.worker_active.set(true);
        std::thread::spawn(move || {
            let result = fetch_dankcalendar_events(request.focus_date);
            glib::idle_add_once(move || {
                CalendarService::global().complete_refresh(request.generation, result);
            });
        });
    }

    fn complete_refresh(&self, generation: u64, result: Result<CalendarSnapshot, CalendarError>) {
        self.apply_refresh_result(generation, result);
        self.worker_active.set(false);
        self.start_pending_refresh();
    }

    fn apply_refresh_result(
        &self,
        generation: u64,
        result: Result<CalendarSnapshot, CalendarError>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        match result {
            Ok(mut snapshot) => {
                snapshot.loading = false;
                self.set_snapshot(snapshot);
            }
            Err(CalendarError::Unavailable(error)) => {
                debug!("CalendarService: {error}");
                let mut snapshot = self.snapshot.borrow().clone();
                snapshot.loading = false;
                snapshot.error = None;
                snapshot.backend_available = false;
                snapshot.events_by_date.clear();
                self.set_snapshot(snapshot);
            }
            Err(CalendarError::Fetch(error)) => {
                warn!("CalendarService: {error}");
                let mut snapshot = self.snapshot.borrow().clone();
                snapshot.loading = false;
                snapshot.backend_available = true;
                snapshot.error = Some(FETCH_ERROR_MESSAGE.to_string());
                self.set_snapshot(snapshot);
            }
        }
    }

    fn set_snapshot(&self, snapshot: CalendarSnapshot) {
        self.snapshot.replace(snapshot.clone());
        self.callbacks.notify(&snapshot);
    }
}

#[derive(Debug)]
enum CalendarError {
    Unavailable(String),
    Fetch(String),
}

fn fetch_dankcalendar_events(focus_date: NaiveDate) -> Result<CalendarSnapshot, CalendarError> {
    let mut client = connect_dankcalendar().map_err(CalendarError::Unavailable)?;
    let calendars = client.calendars_list().map_err(CalendarError::Fetch)?;
    let (window_start, window_end) =
        event_fetch_window(focus_date).map_err(CalendarError::Fetch)?;
    let events = client
        .events_list(window_start, window_end)
        .map_err(CalendarError::Fetch)?;
    Ok(CalendarSnapshot {
        focus_month: Some(month_start(focus_date)),
        loading: false,
        error: None,
        backend_available: true,
        events_by_date: bucket_events(events, &calendars, window_start.date(), window_end.date()),
    })
}

fn prepare_refresh_snapshot(snapshot: &mut CalendarSnapshot, focus_date: NaiveDate) {
    let focus_month = month_start(focus_date);
    if snapshot.focus_month != Some(focus_month) {
        snapshot.events_by_date.clear();
    }
    snapshot.focus_month = Some(focus_month);
    snapshot.loading = true;
    snapshot.error = None;
}

fn month_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

struct DankCalendarClient {
    reader: BufReader<UnixStream>,
    next_id: i64,
}

impl DankCalendarClient {
    fn connect(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .map_err(|e| format!("connect DankCalendar socket {}: {e}", path.display()))?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|e| format!("set write timeout: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut capabilities = String::new();
        let read = reader
            .read_line(&mut capabilities)
            .map_err(|e| format!("read DankCalendar capabilities: {e}"))?;
        if read == 0 {
            return Err("DankCalendar socket closed before capabilities".to_string());
        }
        debug!("DankCalendar capabilities: {}", capabilities.trim());
        Ok(Self { reader, next_id: 1 })
    }

    fn calendars_list(&mut self) -> Result<HashMap<String, DankCalendar>, String> {
        let value = self.request("calendars.list", None)?;
        let calendars: Vec<DankCalendar> = serde_json::from_value(value)
            .map_err(|e| format!("parse DankCalendar calendars: {e}"))?;
        Ok(calendars
            .into_iter()
            .map(|calendar| (calendar.id.clone(), calendar))
            .collect())
    }

    fn events_list(
        &mut self,
        window_start: chrono::NaiveDateTime,
        window_end: chrono::NaiveDateTime,
    ) -> Result<Vec<DankEvent>, String> {
        let from = window_start
            .and_local_timezone(Local)
            .single()
            .unwrap_or_else(|| {
                DateTime::<Utc>::from_naive_utc_and_offset(window_start, Utc).with_timezone(&Local)
            });
        let to = window_end
            .and_local_timezone(Local)
            .single()
            .unwrap_or_else(|| {
                DateTime::<Utc>::from_naive_utc_and_offset(window_end, Utc).with_timezone(&Local)
            });

        let value = self.request(
            "events.list",
            // DankCalendar uses this range to expand recurring events server-side.
            Some(json!({
                "from": from.to_rfc3339(),
                "to": to.to_rfc3339(),
                "limit": EVENT_LIMIT,
            })),
        )?;
        let response: DankEventsResponse =
            serde_json::from_value(value).map_err(|e| format!("parse DankCalendar events: {e}"))?;
        Ok(response.events)
    }

    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = match params {
            Some(params) => json!({"id": id, "method": method, "params": params}),
            None => json!({"id": id, "method": method}),
        };
        let stream = self.reader.get_mut();
        writeln!(stream, "{request}").map_err(|e| format!("write DankCalendar request: {e}"))?;
        stream
            .flush()
            .map_err(|e| format!("flush DankCalendar request: {e}"))?;

        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .map_err(|e| format!("read DankCalendar response: {e}"))?;
            if read == 0 {
                return Err("DankCalendar socket closed".to_string());
            }
            let response: DankResponse = serde_json::from_str(line.trim())
                .map_err(|e| format!("parse DankCalendar response: {e}"))?;
            if response.id != Some(id) {
                continue;
            }
            if let Some(error) = response.error {
                return Err(error);
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

fn event_fetch_window(
    focus_date: NaiveDate,
) -> Result<(chrono::NaiveDateTime, chrono::NaiveDateTime), String> {
    let month_start = month_start(focus_date);
    let next_month = if month_start.month() == 12 {
        NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1)
    }
    .ok_or_else(|| "invalid focus date".to_string())?;

    // GtkCalendar always renders a 6x7 grid; this covers adjacent-month spillover.
    let start = month_start - chrono::Duration::days(GTK_CALENDAR_GRID_PADDING_DAYS);
    let end = next_month + chrono::Duration::days(GTK_CALENDAR_GRID_PADDING_DAYS);
    Ok((
        start
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| "invalid fetch window".to_string())?,
        end.and_hms_opt(0, 0, 0)
            .ok_or_else(|| "invalid fetch window".to_string())?,
    ))
}

#[derive(Debug, Deserialize)]
struct DankResponse {
    id: Option<i64>,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DankCalendar {
    id: String,
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    hidden: bool,
}

#[derive(Debug, Deserialize)]
struct DankEventsResponse {
    events: Vec<DankEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DankEvent {
    #[serde(default)]
    calendar_id: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    location: String,
    start: String,
    end: String,
    #[serde(default)]
    all_day: bool,
    #[serde(default)]
    status: String,
}

fn bucket_events(
    events: Vec<DankEvent>,
    calendars: &HashMap<String, DankCalendar>,
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> BTreeMap<NaiveDate, Vec<Arc<CalendarEvent>>> {
    let mut by_date: BTreeMap<NaiveDate, Vec<Arc<CalendarEvent>>> = BTreeMap::new();
    let Some(last_window_day) = window_end.pred_opt() else {
        return by_date;
    };
    for event in events {
        if event.status == "cancelled" {
            continue;
        }
        let calendar = calendars.get(&event.calendar_id);
        if calendar.is_some_and(|calendar| calendar.hidden) {
            continue;
        }
        let Some(normalized) = normalize_event(&event, calendar).map(Arc::new) else {
            continue;
        };
        let Some((event_start_day, event_end_day)) = event_day_range(&event, &normalized) else {
            continue;
        };
        let start_day = event_start_day.max(window_start);
        let end_day = event_end_day.min(last_window_day);
        if start_day > end_day {
            continue;
        }
        let mut day = start_day;
        while day <= end_day {
            by_date
                .entry(day)
                .or_default()
                .push(Arc::clone(&normalized));
            if day == end_day {
                break;
            }
            let Some(next_day) = day.succ_opt() else {
                break;
            };
            day = next_day;
        }
    }
    for events in by_date.values_mut() {
        events.sort_by_key(|event| (!event.all_day, event.start));
    }
    by_date
}

fn normalize_event(event: &DankEvent, calendar: Option<&DankCalendar>) -> Option<CalendarEvent> {
    let start = parse_event_time(&event.start)?;
    let end = parse_event_time(&event.end)?;
    Some(CalendarEvent {
        calendar_name: calendar.map(|c| c.name.clone()).unwrap_or_default(),
        color: calendar.and_then(|c| c.color.clone()),
        title: if event.summary.is_empty() {
            "(untitled)".to_string()
        } else {
            event.summary.clone()
        },
        location: event.location.clone(),
        start,
        end,
        all_day: event.all_day,
    })
}

fn parse_event_time(value: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Local))
}

fn event_day_range(
    event: &DankEvent,
    normalized: &CalendarEvent,
) -> Option<(NaiveDate, NaiveDate)> {
    if !event.all_day {
        let start_day = normalized.start.date_naive();
        let end_day = normalized.final_day().max(start_day);
        return Some((start_day, end_day));
    }

    // All-day timestamps are civil dates from the daemon; local timezone conversion
    // can shift their date, so bucket them from the raw RFC3339 values.
    let start_day = DateTime::parse_from_rfc3339(&event.start)
        .ok()?
        .date_naive();
    let mut end_day = DateTime::parse_from_rfc3339(&event.end).ok()?.date_naive();
    if end_day > start_day {
        end_day = end_day.pred_opt().unwrap_or(start_day);
    }
    Some((start_day, end_day.max(start_day)))
}

fn connect_dankcalendar() -> Result<DankCalendarClient, String> {
    if cfg!(test) {
        return Err("DankCalendar I/O disabled in tests".to_string());
    }

    let explicit_path = env::var_os("DANKCAL_SOCKET")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    connect_dankcalendar_with_paths(
        explicit_path.as_deref(),
        runtime_dir.as_deref(),
        Path::new("/tmp"),
    )
}

fn connect_dankcalendar_with_paths(
    explicit_path: Option<&Path>,
    runtime_dir: Option<&Path>,
    fallback_dir: &Path,
) -> Result<DankCalendarClient, String> {
    if let Some(path) = explicit_path {
        match DankCalendarClient::connect(path) {
            Ok(client) => return Ok(client),
            Err(error) => {
                warn!("DANKCAL_SOCKET connect failed, falling back to discovery: {error}");
            }
        }
    }

    if let Some(runtime_dir) = &runtime_dir {
        let flatpak_dir = runtime_dir.join("app/com.danklinux.dankcalendar");
        if let Some(client) = connect_to_socket_in_dir(&flatpak_dir) {
            return Ok(client);
        }
    }
    for dir in runtime_dir.into_iter().chain([fallback_dir]) {
        if let Some(client) = connect_to_socket_in_dir(dir) {
            return Ok(client);
        }
    }
    Err("DankCalendar socket not found".to_string())
}

fn connect_to_socket_in_dir(dir: &Path) -> Option<DankCalendarClient> {
    let entries = fs::read_dir(dir).ok()?;
    let mut candidates: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_dankcalendar_socket_name(path) && is_current_user_socket(path))
        .collect();
    candidates.sort();

    for path in candidates {
        if let Ok(client) = DankCalendarClient::connect(&path) {
            return Some(client);
        }
    }
    None
}

fn is_current_user_socket(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| {
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() }
        })
        .unwrap_or(false)
}

fn is_dankcalendar_socket_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(id) = name
        .strip_prefix("dankcal-")
        .and_then(|name| name.strip_suffix(".sock"))
    else {
        return false;
    };
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

    struct TestSocketDir {
        path: PathBuf,
    }

    impl TestSocketDir {
        fn new() -> Self {
            let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "vibepanel-calendar-dir-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test socket directory");
            Self { path }
        }
    }

    impl Drop for TestSocketDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestSocket {
        path: PathBuf,
    }

    impl TestSocket {
        fn bind() -> (Self, UnixListener) {
            let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "vibepanel-calendar-test-{}-{id}.sock",
                std::process::id()
            ));
            let listener = UnixListener::bind(&path).expect("bind test calendar socket");
            (Self { path }, listener)
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn read_request(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read client request");
        assert!(line.ends_with('\n'), "request must be newline-delimited");
        serde_json::from_str(line.trim()).expect("parse client request")
    }

    #[test]
    fn bucket_cases() {
        struct Case {
            name: &'static str,
            start: String,
            end: String,
            all_day: bool,
            hidden_calendar: bool,
            expected_days: Vec<NaiveDate>,
        }

        let day = |d| NaiveDate::from_ymd_opt(2026, 6, d).unwrap();
        let window_start = day(1);
        let window_end = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let cases = [
            Case {
                name: "all-day end is exclusive",
                start: "2026-06-22T00:00:00Z".to_string(),
                end: "2026-06-24T00:00:00Z".to_string(),
                all_day: true,
                hidden_calendar: false,
                expected_days: vec![day(22), day(23)],
            },
            Case {
                name: "hidden calendar is filtered",
                start: "2026-06-22T12:00:00Z".to_string(),
                end: "2026-06-22T13:00:00Z".to_string(),
                all_day: false,
                hidden_calendar: true,
                expected_days: vec![],
            },
            Case {
                name: "timed event ending at midnight stays on start day",
                start: Local
                    .with_ymd_and_hms(2026, 6, 22, 23, 0, 0)
                    .unwrap()
                    .to_rfc3339(),
                end: Local
                    .with_ymd_and_hms(2026, 6, 23, 0, 0, 0)
                    .unwrap()
                    .to_rfc3339(),
                all_day: false,
                hidden_calendar: false,
                expected_days: vec![day(22)],
            },
            Case {
                name: "long event only inside fetch window",
                start: "2020-01-01T00:00:00Z".to_string(),
                end: "2030-01-01T00:00:00Z".to_string(),
                all_day: true,
                hidden_calendar: false,
                expected_days: window_start.iter_days().take(30).collect(),
            },
        ];

        for case in cases {
            let event = DankEvent {
                calendar_id: "cal".to_string(),
                summary: "event".to_string(),
                location: String::new(),
                start: case.start,
                end: case.end,
                all_day: case.all_day,
                status: String::new(),
            };
            let calendars = if case.hidden_calendar {
                HashMap::from([(
                    "cal".to_string(),
                    DankCalendar {
                        id: "cal".to_string(),
                        name: "Hidden".to_string(),
                        color: None,
                        hidden: true,
                    },
                )])
            } else {
                HashMap::new()
            };

            let buckets = bucket_events(vec![event], &calendars, window_start, window_end);

            assert_eq!(
                buckets.keys().copied().collect::<Vec<_>>(),
                case.expected_days,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn fetch_window_covers_visible_month_grid_spillover() {
        let (start, end) = event_fetch_window(NaiveDate::from_ymd_opt(2026, 2, 15).unwrap())
            .expect("valid fetch window");

        assert_eq!(start.date(), NaiveDate::from_ymd_opt(2026, 1, 18).unwrap());
        assert_eq!(end.date(), NaiveDate::from_ymd_opt(2026, 3, 15).unwrap());
    }

    #[test]
    fn refresh_snapshot_clears_events_only_when_focus_month_changes() {
        let june = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let july = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let mut snapshot = CalendarSnapshot {
            focus_month: Some(june),
            loading: false,
            error: Some("old error".to_string()),
            backend_available: true,
            events_by_date: BTreeMap::from([(june, Vec::new())]),
        };

        prepare_refresh_snapshot(&mut snapshot, NaiveDate::from_ymd_opt(2026, 6, 20).unwrap());
        assert!(snapshot.events_by_date.contains_key(&june));
        assert!(snapshot.matches_focus_month(june));
        assert!(snapshot.loading);
        assert!(snapshot.error.is_none());

        prepare_refresh_snapshot(&mut snapshot, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());
        assert!(snapshot.events_by_date.is_empty());
        assert!(snapshot.matches_focus_month(july));
        assert!(!snapshot.matches_focus_month(june));
    }

    #[test]
    fn refresh_results_distinguish_absent_and_fetch_errors() {
        let service = CalendarService::new();

        service.apply_refresh_result(
            0,
            Err(CalendarError::Fetch(
                "socket /tmp/dankcal-123.sock closed".to_string(),
            )),
        );

        assert_eq!(
            service.snapshot().error.as_deref(),
            Some("Error fetching calendar events. Check logs for details.")
        );
        assert!(service.snapshot().backend_available);
        service.snapshot.borrow_mut().events_by_date =
            BTreeMap::from([(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(), Vec::new())]);

        service.apply_refresh_result(
            0,
            Err(CalendarError::Unavailable(
                "DankCalendar socket not found".to_string(),
            )),
        );

        assert!(service.snapshot().error.is_none());
        assert!(!service.snapshot().backend_available);
        assert!(service.snapshot().events_by_date.is_empty());
    }

    #[test]
    fn refresh_coalesces_pending_work_to_latest_request() {
        let service = CalendarService::new();
        service.worker_active.set(true);
        let june = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let july = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let august = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();

        service.refresh(june);
        service.refresh(july);
        service.refresh(august);

        let pending = service
            .pending_refresh
            .borrow()
            .expect("latest request queued");
        assert_eq!(pending.generation, service.generation.get());
        assert_eq!(pending.focus_date, august);
        assert!(service.snapshot().matches_focus_month(august));
    }

    #[test]
    fn reentrant_refresh_keeps_newest_pending_request() {
        let service = CalendarService::new();
        service.worker_active.set(true);
        let june = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let july = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let reentered = Rc::new(Cell::new(false));
        let service_for_callback = service.clone();
        let reentered_for_callback = reentered.clone();
        let callback_id = service.connect(move |snapshot| {
            if snapshot.loading && !reentered_for_callback.replace(true) {
                service_for_callback.refresh(july);
            }
        });

        service.refresh(june);

        let pending = service
            .pending_refresh
            .borrow()
            .expect("reentrant request queued");
        assert_eq!(pending.generation, service.generation.get());
        assert_eq!(pending.focus_date, july);
        assert!(service.snapshot().matches_focus_month(july));
        assert!(service.disconnect(callback_id));
    }

    #[test]
    fn discovery_skips_stale_socket_before_live_socket() {
        let dir = TestSocketDir::new();
        let mut live_pids = [unsafe { libc::getppid() } as u32, std::process::id()];
        live_pids.sort_by_key(u32::to_string);

        let stale_path = dir.path.join(format!("dankcal-{}.sock", live_pids[0]));
        drop(UnixListener::bind(stale_path).expect("bind stale socket"));

        let live_path = dir.path.join(format!("dankcal-{}.sock", live_pids[1]));
        let listener = UnixListener::bind(live_path).expect("bind live socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept discovered client");
            writeln!(stream, "capabilities").unwrap();
        });

        let client = connect_to_socket_in_dir(&dir.path).expect("discover live socket");
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn invalid_explicit_socket_falls_back_to_discovery() {
        let dir = TestSocketDir::new();
        let live_path = dir
            .path
            .join(format!("dankcal-{}.sock", std::process::id()));
        let listener = UnixListener::bind(live_path).expect("bind discovered socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept discovered client");
            writeln!(stream, "capabilities").unwrap();
        });
        let missing_path = dir.path.join("missing.sock");

        let client = connect_dankcalendar_with_paths(
            Some(&missing_path),
            Some(&dir.path),
            Path::new("/nonexistent-vibepanel-calendar-test-dir"),
        )
        .expect("fall back to discovered socket");

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn client_frames_requests_and_discards_nonmatching_response_ids() {
        let (socket, listener) = TestSocket::bind();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            writeln!(stream, r#"{{"protocol":"dankcalendar"}}"#).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let calendars_request = read_request(&mut reader);
            assert_eq!(calendars_request["id"], 1);
            assert_eq!(calendars_request["method"], "calendars.list");
            assert!(calendars_request.get("params").is_none());
            // A future response arriving before its request is not this call's result.
            writeln!(stream, r#"{{"id":2,"result":{{"events":["premature"]}}}}"#).unwrap();
            writeln!(stream, r#"{{"id":1,"result":[]}}"#).unwrap();

            let events_request = read_request(&mut reader);
            assert_eq!(events_request["id"], 2);
            assert_eq!(events_request["method"], "events.list");
            assert_eq!(events_request["params"]["limit"], EVENT_LIMIT);
            assert!(events_request["params"]["from"].as_str().is_some());
            assert!(events_request["params"]["to"].as_str().is_some());
            writeln!(stream, r#"{{"id":2,"result":{{"events":[]}}}}"#).unwrap();
        });

        let mut client = DankCalendarClient::connect(&socket.path).unwrap();
        assert!(client.calendars_list().unwrap().is_empty());
        let from = NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 7, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert!(client.events_list(from, to).unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn connect_fails_when_socket_closes_before_capabilities() {
        let (socket, listener) = TestSocket::bind();
        let server = std::thread::spawn(move || drop(listener.accept().expect("accept client")));

        let error = DankCalendarClient::connect(&socket.path)
            .err()
            .expect("connect must fail");
        assert!(error.contains("closed before capabilities"), "{error}");
        server.join().unwrap();
    }

    /// Connect to a server that sends capabilities, then return the error from
    /// `calendars_list` after `respond` handles the request.
    fn calendars_list_error(respond: fn(&mut UnixStream, Value)) -> String {
        let (socket, listener) = TestSocket::bind();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            writeln!(stream, "capabilities").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let request = read_request(&mut reader);
            respond(&mut stream, request);
        });

        let mut client = DankCalendarClient::connect(&socket.path).expect("connect");
        let error = client.calendars_list().unwrap_err();
        server.join().unwrap();
        error
    }

    #[test]
    fn request_fails_when_socket_closes_before_response() {
        let error = calendars_list_error(|_, _| {});
        assert_eq!(error, "DankCalendar socket closed");
    }

    #[test]
    fn request_propagates_response_error() {
        let error = calendars_list_error(|stream, request| {
            writeln!(
                stream,
                r#"{{"id":{},"error":"calendar unavailable"}}"#,
                request["id"]
            )
            .unwrap();
        });
        assert_eq!(error, "calendar unavailable");
    }
}
