//! Window state used by intellihide, before taskbar filtering.
//!
//! All tiled layouts use the same policy. Only floating windows need geometry.
//! Existing compositor event subscriptions wake one shared worker off the GTK thread.
//! Geometry polling is limited to floating windows on backends without layout events.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::services::callbacks::{CallbackId, Callbacks};
use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Option<Self> {
        (x.is_finite()
            && y.is_finite()
            && width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0)
            .then_some(Self {
                x,
                y,
                width,
                height,
            })
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    pub fn translated(self, x: f64, y: f64) -> Self {
        Self {
            x: self.x + x,
            y: self.y + y,
            ..self
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputWindows {
    pub tiled: bool,
    /// Output-local floating window rectangles, including borders when reported.
    pub floating: Vec<Rect>,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideDecision {
    Hide,
    Show,
    Unknown,
}

impl OutputWindows {
    pub fn decision(&self, footprint: &[Rect]) -> HideDecision {
        if self.tiled
            || self
                .floating
                .iter()
                .any(|window| footprint.iter().any(|bar| window.intersects(*bar)))
        {
            HideDecision::Hide
        } else if self.incomplete || (!self.floating.is_empty() && footprint.is_empty()) {
            HideDecision::Unknown
        } else {
            HideDecision::Show
        }
    }

    fn add(&mut self, floating: Option<bool>, rect: Option<Rect>) {
        match floating {
            Some(false) => self.tiled = true,
            Some(true) => match rect {
                Some(rect) => self.floating.push(rect),
                None => self.incomplete = true,
            },
            None => self.incomplete = true,
        }
    }
}

pub type Snapshot = HashMap<String, OutputWindows>;

pub enum VisibilityReader {
    Mango(String),
    Niri(String),
    Hyprland(String),
    Sway(String),
}

impl VisibilityReader {
    fn read(&self) -> Option<Snapshot> {
        match self {
            Self::Mango(path) => mango_scene(
                &query(path, b"get all-monitors\n", None, false)?,
                &query(path, b"get all-clients\n", None, false)?,
            ),
            Self::Niri(path) => {
                let workspaces = query(path, b"\"Workspaces\"\n", None, false)?;
                let windows = query(path, b"\"Windows\"\n", None, false)?;
                niri_scene(&workspaces["Ok"]["Workspaces"], &windows["Ok"]["Windows"])
            }
            Self::Hyprland(path) => hyprland_scene(
                &query(path, b"j/monitors", None, true)?,
                &query(path, b"j/clients", None, true)?,
            ),
            Self::Sway(path) => sway_scene(
                &query(path, b"", Some(1), false)?,
                &query(path, b"", Some(4), false)?,
            ),
        }
    }
}

// Quiet requests: failures turn the scene unknown; the bar logs transitions
// rather than flooding the journal on every failed refresh.
fn query(path: &str, request: &[u8], i3_type: Option<u32>, read_to_end: bool) -> Option<Value> {
    const MAX_REPLY: u64 = 16 * 1024 * 1024;
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    if let Some(kind) = i3_type {
        // Use Sway's shared framing and 64 MiB tree-reply limit.
        super::sway::ipc_send(&mut stream, kind, request).ok()?;
        let (reply_type, response) = super::sway::ipc_recv(&mut stream).ok()?;
        return (reply_type == kind)
            .then(|| serde_json::from_slice(&response).ok())
            .flatten();
    }
    stream.write_all(request).ok()?;
    let mut response = Vec::new();
    if read_to_end {
        stream.take(MAX_REPLY + 1).read_to_end(&mut response).ok()?;
    } else {
        BufReader::new(stream.take(MAX_REPLY + 1))
            .read_until(b'\n', &mut response)
            .ok()?;
    }
    if response.len() as u64 > MAX_REPLY {
        return None;
    }
    serde_json::from_slice(&response).ok()
}

thread_local! {
    static WORKER: RefCell<Weak<VisibilitySubscription>> = const { RefCell::new(Weak::new()) };
}

static REFRESH: LazyLock<Mutex<std::sync::Weak<RefreshSignal>>> =
    LazyLock::new(|| Mutex::new(std::sync::Weak::new()));

/// Called by backend event streams before taskbar filtering or deduplication.
pub(super) fn notify_changed() {
    if let Some(signal) = REFRESH.lock().upgrade() {
        signal.request();
    }
}

#[derive(Default)]
struct RefreshState {
    pending: bool,
    stopped: bool,
}

#[derive(Default)]
struct RefreshSignal {
    state: Mutex<RefreshState>,
    wake: Condvar,
}

impl RefreshSignal {
    fn request(&self) {
        self.state.lock().pending = true;
        self.wake.notify_one();
    }

    fn wait(&self, fallback: Option<Duration>, not_before: Instant) -> bool {
        let deadline = fallback.map(|delay| Instant::now() + delay);
        let mut state = self.state.lock();
        while !state.stopped && !state.pending {
            if let Some(deadline) = deadline {
                if self.wake.wait_until(&mut state, deadline).timed_out() {
                    break;
                }
            } else {
                self.wake.wait(&mut state);
            }
        }
        // Coalesce bursts without postponing a refresh indefinitely during dragging.
        while !state.stopped && Instant::now() < not_before {
            self.wake.wait_until(&mut state, not_before);
        }
        state.pending = false;
        !state.stopped
    }

    fn stop(&self) {
        self.state.lock().stopped = true;
        self.wake.notify_one();
    }
}

impl VisibilityReader {
    fn run(
        &self,
        signal: &RefreshSignal,
        snapshot: &RwLock<Snapshot>,
        sender: async_channel::Sender<()>,
    ) {
        let mut fallback = None;
        let mut not_before = Instant::now();
        while signal.wait(fallback, not_before) {
            let result = self.read();
            if signal.state.lock().stopped {
                break;
            }
            fallback = self.fallback_interval(result.as_ref());
            not_before = Instant::now() + Duration::from_millis(100);
            let next = result.unwrap_or_default();
            let changed = {
                let mut current = snapshot.write();
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            };
            if changed {
                let _ = sender.try_send(());
            }
        }
    }

    fn fallback_interval(&self, snapshot: Option<&Snapshot>) -> Option<Duration> {
        let Some(snapshot) = snapshot else {
            return Some(Duration::from_secs(1));
        };
        if !matches!(self, Self::Niri(_))
            && snapshot.values().any(|output| !output.floating.is_empty())
        {
            // Sway/Hyprland lack continuous resize events; Mango watch delivery
            // varies by version. One shared worker polls at most 10 times/second
            // while floaters are visible; tiled-only and empty scenes do not poll.
            // Each poll reads the full scene for every output; narrow to
            // outputs hosting an intellihide bar if polling cost shows up.
            Some(Duration::from_millis(100))
        } else if snapshot.values().any(|output| output.incomplete) {
            Some(Duration::from_secs(1))
        } else {
            None
        }
    }
}

/// The final subscription cancels both worker waits and GTK delivery. Each
/// generation owns its snapshot, so late replies cannot update a replacement.
pub struct VisibilitySubscription {
    snapshot: Arc<RwLock<Snapshot>>,
    signal: Arc<RefreshSignal>,
    changes: Callbacks<()>,
    delivery: RefCell<Option<gtk4::glib::JoinHandle<()>>>,
}

impl VisibilitySubscription {
    pub fn acquire(reader: Option<VisibilityReader>) -> Rc<Self> {
        WORKER.with(|slot| {
            if let Some(worker) = slot.borrow().upgrade() {
                return worker;
            }
            if reader.is_none() {
                tracing::warn!("Intellihide is unavailable for this compositor; using auto-hide");
            }
            let snapshot = Arc::new(RwLock::new(Snapshot::default()));
            let signal = Arc::new(RefreshSignal::default());
            signal.state.lock().pending = true;
            *REFRESH.lock() = Arc::downgrade(&signal);
            let (sender, receiver) = async_channel::bounded(1);
            if let Some(reader) = reader {
                let snapshot = Arc::clone(&snapshot);
                let signal = Arc::clone(&signal);
                let result =
                    thread::Builder::new()
                        .name("bar-visibility".into())
                        .spawn(move || {
                            reader.run(&signal, &snapshot, sender);
                        });
                if let Err(error) = result {
                    tracing::warn!(%error, "Could not start intellihide worker");
                }
            }
            let worker = Rc::new(Self {
                snapshot,
                signal,
                changes: Callbacks::new(),
                delivery: RefCell::new(None),
            });
            let weak = Rc::downgrade(&worker);
            *worker.delivery.borrow_mut() = Some(gtk4::glib::spawn_future_local(async move {
                while receiver.recv().await.is_ok() {
                    let Some(worker) = weak.upgrade() else {
                        break;
                    };
                    worker.changes.notify(&());
                }
            }));
            *slot.borrow_mut() = Rc::downgrade(&worker);
            worker
        })
    }

    pub fn connect_changed(&self, callback: impl Fn() + 'static) -> CallbackId {
        self.changes.register(move |_| callback())
    }

    pub fn disconnect_changed(&self, id: CallbackId) {
        self.changes.unregister(id);
    }

    pub fn decision(&self, output: &str, footprint: &[Rect]) -> HideDecision {
        self.snapshot
            .read()
            .get(output)
            .map_or(HideDecision::Unknown, |scene| scene.decision(footprint))
    }
}

impl Drop for VisibilitySubscription {
    fn drop(&mut self) {
        self.signal.stop();
        if let Some(delivery) = self.delivery.borrow_mut().take() {
            delivery.abort();
        }
    }
}

fn rect(value: &Value) -> Option<Rect> {
    Rect::new(
        value["x"].as_f64()?,
        value["y"].as_f64()?,
        value["width"].as_f64()?,
        value["height"].as_f64()?,
    )
}

fn pair(value: &Value) -> Option<(f64, f64)> {
    Some((value.get(0)?.as_f64()?, value.get(1)?.as_f64()?))
}

fn flag(value: &Value, name: &str) -> bool {
    value[name].as_bool() == Some(true)
}

// Floating windows are rendered in the global output layout on these compositors.
// Project only windows already filtered by their owning workspace's visibility.
fn project_floating(scene: &mut Snapshot, outputs: &HashMap<String, Rect>) {
    let floating: Vec<_> = scene
        .iter()
        .flat_map(|(owner, state)| {
            state.floating.iter().filter_map(|window| {
                outputs
                    .get(owner)
                    .map(|output| (owner.clone(), window.translated(output.x, output.y)))
            })
        })
        .collect();
    for (name, state) in scene.iter_mut() {
        let Some(output) = outputs.get(name) else {
            state.incomplete = true;
            continue;
        };
        for (owner, window) in &floating {
            if owner != name && window.intersects(*output) {
                state.floating.push(window.translated(-output.x, -output.y));
            }
        }
    }
}

fn hyprland_output_rect(monitor: &Value) -> Option<Rect> {
    let scale = monitor["scale"].as_f64()?;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let (mut width, mut height) = (monitor["width"].as_f64()?, monitor["height"].as_f64()?);
    if monitor["transform"].as_u64()? % 2 == 1 {
        std::mem::swap(&mut width, &mut height);
    }
    Rect::new(
        monitor["x"].as_f64()?,
        monitor["y"].as_f64()?,
        width / scale,
        height / scale,
    )
}

fn mango_scene(monitors: &Value, clients: &Value) -> Option<Snapshot> {
    let monitors = monitors["monitors"].as_array()?;
    let clients = clients["clients"].as_array()?;
    let mut scene = Snapshot::new();
    for monitor in monitors {
        let name = monitor["name"].as_str()?;
        let origin = monitor["x"].as_f64().zip(monitor["y"].as_f64());
        let mut output = OutputWindows::default();
        // Overview coordinates do not describe the normal workspace.
        if monitor["active_tags"]
            .as_array()
            .is_some_and(|tags| tags.contains(&Value::from(super::mango::OVERVIEW_WORKSPACE_ID)))
        {
            output.incomplete = true;
        }
        for client in clients
            .iter()
            .filter(|client| client["monitor"].as_str() == Some(name))
        {
            if flag(client, "is_minimized") || flag(client, "is_swallowedby") {
                continue;
            }
            match client["is_visible"].as_bool() {
                Some(false) => continue,
                None => {
                    output.incomplete = true;
                    continue;
                }
                Some(true) => {}
            }
            let geometry = rect(client)
                .zip(origin)
                .map(|(r, (x, y))| r.translated(-x, -y));
            let floating = if flag(client, "is_fullscreen") || flag(client, "is_maximized") {
                Some(false)
            } else {
                client["is_floating"].as_bool()
            };
            output.add(floating, geometry);
        }
        scene.insert(name.to_string(), output);
    }
    let outputs = monitors
        .iter()
        .filter_map(|monitor| Some((monitor["name"].as_str()?.to_string(), rect(monitor)?)))
        .collect();
    project_floating(&mut scene, &outputs);
    Some(scene)
}

fn niri_scene(workspaces: &Value, windows: &Value) -> Option<Snapshot> {
    let workspaces = workspaces.as_array()?;
    let windows = windows.as_array()?;
    let mut scene = Snapshot::new();
    for workspace in workspaces.iter().filter(|ws| flag(ws, "is_active")) {
        let Some(name) = workspace["output"].as_str() else {
            continue;
        };
        let id = workspace["id"].as_u64()?;
        let mut output = OutputWindows::default();
        for window in windows
            .iter()
            .filter(|w| w["workspace_id"].as_u64() == Some(id))
        {
            let layout = &window["layout"];
            let geometry = pair(&layout["tile_pos_in_workspace_view"])
                .zip(pair(&layout["tile_size"]))
                .and_then(|((x, y), (w, h))| Rect::new(x, y, w, h));
            output.add(window["is_floating"].as_bool(), geometry);
        }
        scene.insert(name.to_string(), output);
    }
    Some(scene)
}

fn hyprland_scene(monitors: &Value, clients: &Value) -> Option<Snapshot> {
    let monitors = monitors.as_array()?;
    let clients = clients.as_array()?;
    let mut scene = Snapshot::new();
    for monitor in monitors {
        let name = monitor["name"].as_str()?;
        let id = monitor["id"].as_i64()?;
        let active = monitor["activeWorkspace"]["id"].as_i64()?;
        let special = monitor["specialWorkspace"]["id"]
            .as_i64()
            .filter(|id| *id != 0);
        let origin = monitor["x"].as_f64().zip(monitor["y"].as_f64());
        let mut output = OutputWindows::default();
        for client in clients.iter().filter(|c| c["monitor"].as_i64() == Some(id)) {
            if client["mapped"].as_bool() == Some(false) || flag(client, "hidden") {
                continue;
            }
            let ws = client["workspace"]["id"].as_i64();
            if !flag(client, "pinned")
                && ws != Some(active)
                && !(special.is_some() && ws == special)
            {
                if ws.is_none() {
                    output.incomplete = true;
                }
                continue;
            }
            let geometry = pair(&client["at"])
                .zip(pair(&client["size"]))
                .and_then(|((x, y), (w, h))| Rect::new(x, y, w, h))
                .zip(origin)
                .map(|(r, (x, y))| r.translated(-x, -y));
            let floating = if client["fullscreen"].as_u64().is_some_and(|mode| mode > 0)
                || client["fullscreen"].as_bool() == Some(true)
            {
                Some(false)
            } else {
                client["floating"].as_bool()
            };
            output.add(floating, geometry);
        }
        scene.insert(name.to_string(), output);
    }
    let outputs = monitors
        .iter()
        .filter_map(|monitor| {
            Some((
                monitor["name"].as_str()?.to_string(),
                hyprland_output_rect(monitor)?,
            ))
        })
        .collect();
    project_floating(&mut scene, &outputs);
    Some(scene)
}

fn sway_scene(workspaces: &Value, tree: &Value) -> Option<Snapshot> {
    let visible: HashSet<&str> = workspaces
        .as_array()?
        .iter()
        .filter(|ws| flag(ws, "visible"))
        .filter_map(|ws| ws["name"].as_str())
        .collect();
    let mut scene = Snapshot::new();
    for output in tree["nodes"].as_array()? {
        let name = output["name"].as_str()?;
        if name == "__i3" {
            continue;
        }
        let origin = output["rect"]["x"]
            .as_f64()
            .zip(output["rect"]["y"].as_f64());
        let mut state = OutputWindows::default();
        for workspace in output["nodes"].as_array()? {
            if workspace["name"]
                .as_str()
                .is_some_and(|name| visible.contains(name))
            {
                sway_node(workspace, false, origin, &mut state);
            }
        }
        scene.insert(name.to_string(), state);
    }
    let outputs = tree["nodes"]
        .as_array()?
        .iter()
        .filter_map(|output| Some((output["name"].as_str()?.to_string(), rect(&output["rect"])?)))
        .collect();
    project_floating(&mut scene, &outputs);
    Some(scene)
}

fn sway_node(node: &Value, floating: bool, origin: Option<(f64, f64)>, state: &mut OutputWindows) {
    if node["visible"].as_bool() == Some(false) {
        return;
    }
    let floating = floating
        || node["type"] == "floating_con"
        || matches!(node["floating"].as_str(), Some("user_on" | "auto_on"));
    let is_window =
        node["pid"].as_u64().is_some_and(|pid| pid > 0) || node["window"].as_u64().is_some();
    if is_window {
        let geometry = rect(&node["rect"])
            .zip(origin)
            .map(|(r, (x, y))| r.translated(-x, -y));
        // Fullscreen also hides when entered from floating mode.
        state.add(
            Some(floating && node["fullscreen_mode"].as_u64().unwrap_or(0) == 0),
            geometry,
        );
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                sway_node(child, floating || key == "floating_nodes", origin, state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bar() -> Rect {
        Rect::new(400.0, 10.0, 200.0, 32.0).unwrap()
    }

    #[test]
    fn worker_sleeps_until_event_and_notifies_only_for_changed_scenes() {
        use std::os::unix::net::UnixListener;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let dir = crate::ui_regression_test_support::TestDir::new("visibility-worker");
        let path = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let tiled = Arc::new(AtomicBool::new(false));
        let server_tiled = tiled.clone();
        let (requests, received) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut count = 0;
            while count < 6 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut command = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut command)
                    .unwrap();
                let reply = if command.contains("Workspaces") {
                    json!({"Ok":{"Workspaces":[{"id":1,"output":"A","is_active":true}]}})
                } else {
                    let windows = if server_tiled.load(Ordering::Relaxed) {
                        json!([{"workspace_id":1,"is_floating":false}])
                    } else {
                        json!([])
                    };
                    json!({"Ok":{"Windows":windows}})
                };
                writeln!(stream, "{reply}").unwrap();
                count += 1;
                requests.send(()).unwrap();
            }
            count
        });
        let signal = Arc::new(RefreshSignal::default());
        signal.request();
        let snapshot = Arc::new(RwLock::new(Snapshot::new()));
        let (sender, notifications) = async_channel::bounded(1);
        let worker_signal = signal.clone();
        let worker_snapshot = snapshot.clone();
        let worker = thread::spawn(move || {
            VisibilityReader::Niri(path.to_string_lossy().into_owned()).run(
                &worker_signal,
                &worker_snapshot,
                sender,
            )
        });
        let wait_reply = || {
            for _ in 0..2 {
                received.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        };
        let wait_notification = || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while notifications.try_recv().is_err() {
                assert!(Instant::now() < deadline, "missing scene notification");
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait_reply();
        wait_notification();
        assert!(
            received.recv_timeout(Duration::from_millis(250)).is_err(),
            "idle worker polled IPC"
        );
        signal.request();
        wait_reply();
        assert!(received.recv_timeout(Duration::from_millis(150)).is_err());
        assert!(
            notifications.try_recv().is_err(),
            "unchanged scene notified GTK"
        );
        tiled.store(true, Ordering::Relaxed);
        signal.request();
        wait_reply();
        wait_notification();
        assert!(snapshot.read()["A"].tiled);
        signal.stop();
        worker.join().unwrap();
        assert_eq!(server.join().unwrap(), 6);
    }

    #[test]
    fn stop_interrupts_idle_and_throttled_waits() {
        for throttle in [Duration::ZERO, Duration::from_secs(60)] {
            let signal = Arc::new(RefreshSignal::default());
            let waiting = signal.clone();
            let (done, result) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                done.send(waiting.wait(None, Instant::now() + throttle))
                    .unwrap();
            });
            assert!(result.recv_timeout(Duration::from_millis(20)).is_err());
            if !throttle.is_zero() {
                signal.request();
            }
            signal.stop();
            assert!(!result.recv_timeout(Duration::from_secs(1)).unwrap());
            worker.join().unwrap();
        }
    }

    #[test]
    fn geometry_polling_is_limited_to_floaters_without_layout_events() {
        let mut scene = Snapshot::from([("A".into(), OutputWindows::default())]);
        let reader = VisibilityReader::Sway(String::new());
        assert_eq!(reader.fallback_interval(Some(&scene)), None);
        scene.get_mut("A").unwrap().tiled = true;
        assert_eq!(reader.fallback_interval(Some(&scene)), None);
        scene.get_mut("A").unwrap().floating.push(bar());
        assert_eq!(
            reader.fallback_interval(Some(&scene)),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            VisibilityReader::Niri(String::new()).fallback_interval(Some(&scene)),
            None
        );
        assert_eq!(reader.fallback_interval(None), Some(Duration::from_secs(1)));
    }

    #[test]
    fn islands_ignore_empty_edge_space_and_edge_contact() {
        let mut state = OutputWindows::default();
        state.add(Some(true), Rect::new(0.0, 0.0, 200.0, 200.0));
        state.add(Some(true), Rect::new(400.0, 42.0, 200.0, 200.0));
        assert_eq!(state.decision(&[bar()]), HideDecision::Show);
        state.add(Some(true), Rect::new(400.0, 41.5, 200.0, 200.0));
        assert_eq!(state.decision(&[bar()]), HideDecision::Hide);
    }

    #[test]
    fn tiled_windows_need_no_geometry_and_unknown_is_not_empty() {
        let mut state = OutputWindows::default();
        assert_eq!(state.decision(&[]), HideDecision::Show);
        state.add(Some(true), None);
        assert_eq!(state.decision(&[bar()]), HideDecision::Unknown);
        state.add(Some(false), None);
        assert_eq!(state.decision(&[]), HideDecision::Hide);
    }

    #[test]
    fn niri_uses_each_outputs_active_workspace_and_full_width_ids() {
        let workspaces = json!([
            {"id":4294967297u64,"output":"A","is_active":true},
            {"id":2,"output":"B","is_active":true},
            {"id":3,"output":"A","is_active":false}
        ]);
        let windows = json!([
            {"workspace_id":4294967297u64,"is_floating":false,"layout":{}},
            {"workspace_id":2,"is_floating":true,"layout":{
                "tile_pos_in_workspace_view":[450,0],"tile_size":[100,100]}},
            {"workspace_id":3,"is_floating":false}
        ]);
        let scene = niri_scene(&workspaces, &windows).unwrap();
        assert!(scene["A"].tiled);
        assert!(!scene["B"].tiled);
        assert_eq!(scene["B"].decision(&[bar()]), HideDecision::Hide);
        assert!(niri_scene(&Value::Null, &windows).is_none());
    }

    #[test]
    fn mango_includes_special_clients_but_not_minimized_or_hidden() {
        let monitors = json!({"monitors":[{"name":"A","x":-1000,"y":0,"active_tags":[1,3]}]});
        let clients = json!({"clients":[
            {"monitor":"A","is_visible":true,"is_floating":true,"tags":[0],
             "x":-550,"y":0,"width":100,"height":100},
            {"monitor":"A","is_visible":true,"is_floating":false,"is_minimized":true},
            {"monitor":"A","is_visible":false,"is_floating":false}
        ]});
        let scene = mango_scene(&monitors, &clients).unwrap();
        assert!(!scene["A"].tiled);
        assert_eq!(scene["A"].decision(&[bar()]), HideDecision::Hide);
    }

    #[test]
    fn hyprland_accounts_for_special_workspaces_and_pinned_windows() {
        let monitors = json!([{"name":"A","id":0,"x":0,"y":0,
            "activeWorkspace":{"id":1},"specialWorkspace":{"id":-99}}]);
        let clients = json!([
            {"monitor":0,"workspace":{"id":2},"floating":false},
            {"monitor":0,"workspace":{"id":-99},"floating":true,"at":[450,0],"size":[100,100]},
            {"monitor":0,"workspace":{"id":1},"floating":false,"hidden":true}
        ]);
        let scene = hyprland_scene(&monitors, &clients).unwrap();
        assert!(!scene["A"].tiled);
        assert_eq!(scene["A"].decision(&[bar()]), HideDecision::Hide);
        let pinned = json!([{"monitor":0,"workspace":{"id":2},"floating":true,
            "pinned":true,"at":[450,0],"size":[100,100]}]);
        assert_eq!(
            hyprland_scene(&monitors, &pinned).unwrap()["A"].decision(&[bar()]),
            HideDecision::Hide
        );
    }

    #[test]
    fn mango_projects_visible_floaters_but_not_tiled_or_hidden_clients() {
        let monitors = json!({"monitors":[
            {"name":"A","x":-1000,"y":0,"width":1000,"height":800},
            {"name":"B","x":0,"y":0,"width":1000,"height":800}
        ]});
        let clients = json!({"clients":[
            {"monitor":"A","is_visible":true,"is_floating":true,
             "x":-100,"y":0,"width":200,"height":200}
        ]});
        let edge = Rect::new(0.0, 0.0, 1000.0, 32.0).unwrap();
        let scene = mango_scene(&monitors, &clients).unwrap();
        assert_eq!(scene["A"].decision(&[edge]), HideDecision::Hide);
        assert_eq!(scene["B"].decision(&[edge]), HideDecision::Hide);
        for field in ["is_floating", "is_visible"] {
            let mut clients = clients.clone();
            clients["clients"][0][field] = json!(false);
            let scene = mango_scene(&monitors, &clients).unwrap();
            assert_eq!(scene["B"].decision(&[edge]), HideDecision::Show);
        }
    }

    #[test]
    fn hyprland_projects_from_owner_workspace_using_logical_output_bounds() {
        let monitors = json!([
            {"name":"A","id":0,"x":0,"y":0,"width":1920,"height":1080,
             "scale":1.5,"transform":0,"activeWorkspace":{"id":1}},
            {"name":"B","id":1,"x":1280,"y":0,"width":1920,"height":1080,
             "scale":1.5,"transform":1,"activeWorkspace":{"id":2}}
        ]);
        let mut clients = json!([
            {"monitor":0,"workspace":{"id":1},"floating":true,
             "at":[1200,0],"size":[200,200]}
        ]);
        let bar = Rect::new(0.0, 0.0, 720.0, 32.0).unwrap();
        assert_eq!(
            hyprland_output_rect(&monitors[1]).unwrap(),
            Rect::new(1280.0, 0.0, 720.0, 1280.0).unwrap()
        );
        assert_eq!(
            hyprland_scene(&monitors, &clients).unwrap()["B"].decision(&[bar]),
            HideDecision::Hide
        );
        clients[0]["workspace"]["id"] = json!(3);
        assert_eq!(
            hyprland_scene(&monitors, &clients).unwrap()["B"].decision(&[bar]),
            HideDecision::Show
        );
        clients[0]["pinned"] = json!(true);
        assert_eq!(
            hyprland_scene(&monitors, &clients).unwrap()["B"].decision(&[bar]),
            HideDecision::Hide
        );
        for fullscreen in [json!(2), json!(true)] {
            clients[0]["fullscreen"] = fullscreen;
            let scene = hyprland_scene(&monitors, &clients).unwrap();
            assert_eq!(scene["A"].decision(&[bar]), HideDecision::Hide);
            assert_eq!(scene["B"].decision(&[bar]), HideDecision::Show);
        }
    }

    #[test]
    fn niri_floating_geometry_stays_on_its_own_output() {
        let workspaces = json!([
            {"id":1,"output":"A","is_active":true},
            {"id":2,"output":"B","is_active":true}
        ]);
        let windows = json!([{"workspace_id":1,"is_floating":true,"layout":{
            "tile_pos_in_workspace_view":[-100,0],"tile_size":[2000,200]}}]);
        let scene = niri_scene(&workspaces, &windows).unwrap();
        assert_eq!(scene["A"].decision(&[bar()]), HideDecision::Hide);
        assert_eq!(scene["B"].decision(&[bar()]), HideDecision::Show);
    }

    #[test]
    fn sway_tree_preserves_floating_ancestry_and_workspace_visibility() {
        let workspaces = json!([{"name":"web","visible":true}]);
        let tree = json!({"nodes":[{"name":"A","rect":{"x":1000,"y":0},"nodes":[
            {"name":"hidden","nodes":[{"pid":1}]},
            {"name":"web","nodes":[],"floating_nodes":[{"type":"floating_con","nodes":[
                {"pid":2,"visible":true,"rect":{"x":1450,"y":0,"width":100,"height":100}}
            ]}]}
        ]}]});
        let scene = sway_scene(&workspaces, &tree).unwrap();
        assert!(!scene["A"].tiled);
        assert_eq!(scene["A"].decision(&[bar()]), HideDecision::Hide);
    }
}
