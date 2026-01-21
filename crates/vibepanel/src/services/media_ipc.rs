//! IPC for CLI ↔ MediaService communication.
//!
//! Uses a Unix datagram socket in `$XDG_RUNTIME_DIR/vibepanel-media.sock`.
//! The CLI sends commands to control player selection; the panel listens
//! and dispatches to the MediaService. The panel also writes state that
//! the CLI can read.
//!
//! Message format (line-based text):
//! - `select:<bus_name>` – select a specific player
//! - `auto` – switch to auto-selection mode
//! - `get_active` – request the active player (response via separate mechanism)
//!
//! State file: `$XDG_RUNTIME_DIR/vibepanel-media-state` contains:
//! - Line 1: active player bus name (or empty for auto)
//! - Line 2: "auto" or "manual"

use std::cell::RefCell;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::rc::Rc;
use tracing::{debug, warn};

/// Get the socket path for media IPC.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("vibepanel-media.sock")
    } else {
        PathBuf::from("/tmp/vibepanel-media.sock")
    }
}

/// Get the state file path.
pub fn state_file_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("vibepanel-media-state")
    } else {
        PathBuf::from("/tmp/vibepanel-media-state")
    }
}

/// Media IPC message types.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaIpcMessage {
    /// Select a specific player by bus name.
    Select { bus_name: String },
    /// Switch to auto-selection mode.
    Auto,
}

impl MediaIpcMessage {
    /// Serialize to wire format.
    pub fn to_wire(&self) -> String {
        match self {
            MediaIpcMessage::Select { bus_name } => format!("select:{}", bus_name),
            MediaIpcMessage::Auto => "auto".to_string(),
        }
    }

    /// Parse from wire format.
    pub fn from_wire(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "auto" {
            return Some(MediaIpcMessage::Auto);
        }
        if let Some(bus_name) = s.strip_prefix("select:") {
            return Some(MediaIpcMessage::Select {
                bus_name: bus_name.to_string(),
            });
        }
        None
    }
}

/// Send a media IPC message to the running panel (best-effort).
pub fn send_message(msg: &MediaIpcMessage) -> io::Result<()> {
    let path = socket_path();
    let socket = UnixDatagram::unbound()?;
    let wire = msg.to_wire();
    socket.send_to(wire.as_bytes(), &path)?;
    Ok(())
}

/// Write current media state to the state file.
///
/// Called by MediaService when the active player changes.
pub fn write_state(active_bus_name: Option<&str>, is_auto: bool) {
    let path = state_file_path();
    let content = format!(
        "{}\n{}",
        active_bus_name.unwrap_or(""),
        if is_auto { "auto" } else { "manual" }
    );
    if let Err(e) = std::fs::write(&path, content) {
        debug!("Media IPC: failed to write state file: {}", e);
    }
}

/// Read current media state from the state file.
///
/// Returns (active_bus_name, is_auto). Returns (None, true) if file doesn't exist.
pub fn read_state() -> (Option<String>, bool) {
    let path = state_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut lines = content.lines();
            let bus_name = lines.next().and_then(|s| {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            });
            let is_auto = lines.next().map(|s| s.trim() == "auto").unwrap_or(true);
            (bus_name, is_auto)
        }
        Err(_) => (None, true),
    }
}

use gtk4::glib;

/// Type alias for media IPC callback.
type MediaCallback = Rc<RefCell<Option<Rc<dyn Fn(MediaIpcMessage)>>>>;

/// Listener for media IPC messages.
pub struct MediaIpcListener {
    _socket: UnixDatagram,
    socket_path: PathBuf,
    source_id: Option<glib::SourceId>,
    callback: MediaCallback,
}

impl MediaIpcListener {
    /// Create and start a new IPC listener.
    pub fn new() -> Option<Rc<RefCell<Self>>> {
        let path = socket_path();

        // Remove stale socket if it exists.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }

        let socket = match UnixDatagram::bind(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!("Media IPC: failed to bind socket at {:?}: {}", path, e);
                return None;
            }
        };

        if let Err(e) = socket.set_nonblocking(true) {
            warn!("Media IPC: failed to set socket non-blocking: {}", e);
            return None;
        }

        debug!("Media IPC: listening on {:?}", path);

        let socket_fd = socket.as_raw_fd();
        let callback: MediaCallback = Rc::new(RefCell::new(None));
        let callback_for_watcher = callback.clone();

        let listener = Rc::new(RefCell::new(Self {
            _socket: socket,
            socket_path: path,
            source_id: None,
            callback,
        }));

        let listener_weak = Rc::downgrade(&listener);
        let source_id =
            glib::unix_fd_add_local(socket_fd, glib::IOCondition::IN, move |fd, _condition| {
                let mut buf = [0u8; 512];
                loop {
                    let n = unsafe {
                        libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
                    };

                    if n <= 0 {
                        break;
                    }

                    let n = n as usize;
                    if let Ok(s) = std::str::from_utf8(&buf[..n])
                        && let Some(msg) = MediaIpcMessage::from_wire(s)
                    {
                        debug!("Media IPC: received message: {:?}", s);
                        if let Some(ref cb) = *callback_for_watcher.borrow() {
                            cb(msg);
                        }
                    }
                }

                if listener_weak.upgrade().is_none() {
                    return glib::ControlFlow::Break;
                }

                glib::ControlFlow::Continue
            });

        listener.borrow_mut().source_id = Some(source_id);

        Some(listener)
    }

    /// Register a callback for incoming messages.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(MediaIpcMessage) + 'static,
    {
        *self.callback.borrow_mut() = Some(Rc::new(callback));
    }
}

impl Drop for MediaIpcListener {
    fn drop(&mut self) {
        if let Some(source_id) = self.source_id.take() {
            source_id.remove();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(state_file_path());
        debug!("Media IPC: listener stopped");
    }
}
