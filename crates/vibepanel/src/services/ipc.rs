//! Panel IPC for CLI → bar communication.
//!
//! Uses a Unix datagram socket in `$XDG_RUNTIME_DIR/vibepanel.sock`.
//! The CLI sends short text messages; the bar listens and dispatches
//! to the appropriate handler (OSD display, idle inhibitor toggle, etc.).
//!
//! Message format (line-based text):
//! - `volume:<percent>:<muted>` – show volume OSD (e.g., `volume:42:0`)
//! - `volume_unavailable` – show "sink suspended" OSD
//! - `brightness:<percent>` – show brightness OSD
//! - `toggle_inhibitor` – toggle idle inhibitor on/off
//!
//! This is best-effort, fire-and-forget IPC. If the bar isn't running or
//! the socket doesn't exist, the CLI silently continues.
//!
//! The listener uses glib::unix_fd_add_local() to watch the socket fd
//! on the GTK main loop - fully event-driven with zero polling.

use std::cell::RefCell;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::rc::Rc;
use tracing::{debug, warn};

/// Type alias for IPC message callback storage.
type IpcCallback = Rc<RefCell<Option<Rc<dyn Fn(IpcMessage)>>>>;

/// Get the socket path for panel IPC.
///
/// Returns `$XDG_RUNTIME_DIR/vibepanel.sock` or falls back to `/tmp/vibepanel.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("vibepanel.sock")
    } else {
        warn!("XDG_RUNTIME_DIR not set, falling back to /tmp/vibepanel.sock");
        PathBuf::from("/tmp/vibepanel.sock")
    }
}

/// Panel IPC message types.
#[derive(Debug, Clone, PartialEq)]
pub enum IpcMessage {
    /// Show volume OSD with given percentage and mute state.
    Volume { percent: u32, muted: bool },
    /// Show "volume unavailable" OSD (sink suspended).
    VolumeUnavailable,
    /// Show brightness OSD with given percentage.
    Brightness { percent: u32 },
    /// Toggle the idle inhibitor on/off.
    ToggleInhibitor,
}

impl IpcMessage {
    /// Serialize to wire format.
    pub fn to_wire(&self) -> String {
        match self {
            IpcMessage::Volume { percent, muted } => {
                format!("volume:{}:{}", percent, if *muted { 1 } else { 0 })
            }
            IpcMessage::VolumeUnavailable => "volume_unavailable".to_string(),
            IpcMessage::Brightness { percent } => format!("brightness:{}", percent),
            IpcMessage::ToggleInhibitor => "toggle_inhibitor".to_string(),
        }
    }

    /// Parse from wire format.
    pub fn from_wire(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "volume_unavailable" {
            return Some(IpcMessage::VolumeUnavailable);
        }
        if s == "toggle_inhibitor" {
            return Some(IpcMessage::ToggleInhibitor);
        }
        if let Some(rest) = s.strip_prefix("volume:") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() == 2 {
                let percent = parts[0].parse().ok()?;
                let muted = parts[1] == "1";
                return Some(IpcMessage::Volume { percent, muted });
            }
        }
        if let Some(rest) = s.strip_prefix("brightness:") {
            let percent = rest.parse().ok()?;
            return Some(IpcMessage::Brightness { percent });
        }
        None
    }
}

/// Send an IPC message to the running panel (best-effort, fire-and-forget).
///
/// Returns `Ok(())` if the message was sent, or an error if the socket
/// doesn't exist or sending failed. The caller should typically ignore
/// errors since the panel may not be running.
pub fn send_ipc_message(msg: &IpcMessage) -> io::Result<()> {
    let path = socket_path();
    let socket = UnixDatagram::unbound()?;
    let wire = msg.to_wire();
    socket.send_to(wire.as_bytes(), &path)?;
    Ok(())
}

/// Convenience: send a volume OSD message.
pub fn notify_volume(percent: u32, muted: bool) {
    let msg = IpcMessage::Volume { percent, muted };
    if let Err(e) = send_ipc_message(&msg) {
        debug!("IPC: failed to send volume message: {}", e);
    }
}

/// Convenience: send a "volume unavailable" OSD message.
pub fn notify_volume_unavailable() {
    let msg = IpcMessage::VolumeUnavailable;
    if let Err(e) = send_ipc_message(&msg) {
        debug!("IPC: failed to send volume_unavailable message: {}", e);
    }
}

use gtk4::glib;

/// Listener for panel IPC messages.
///
/// Uses glib::unix_fd_add_local() to watch the socket fd on the GTK main loop.
/// Fully event-driven - zero polling, zero background threads.
pub struct IpcListener {
    /// The bound socket (must stay alive while listening).
    _socket: UnixDatagram,
    /// Path to the socket file (for cleanup on drop).
    socket_path: PathBuf,
    /// GLib source ID for the fd watcher.
    source_id: Option<glib::SourceId>,
    /// Registered callback for incoming messages.
    callback: IpcCallback,
}

impl IpcListener {
    /// Create and start a new IPC listener.
    ///
    /// The listener binds to the socket and watches for incoming messages
    /// on the GTK main loop. Call `connect` to register a callback.
    pub fn new() -> Option<Rc<RefCell<Self>>> {
        let path = socket_path();

        // Remove stale socket if it exists.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }

        // Bind the socket.
        let socket = match UnixDatagram::bind(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!("IPC: failed to bind socket at {:?}: {}", path, e);
                return None;
            }
        };

        // Set non-blocking so recv doesn't block the main loop.
        if let Err(e) = socket.set_nonblocking(true) {
            warn!("IPC: failed to set socket non-blocking: {}", e);
            let _ = std::fs::remove_file(&path);
            return None;
        }

        debug!("IPC: listening on {:?}", path);

        let socket_fd = socket.as_raw_fd();
        let callback: IpcCallback = Rc::new(RefCell::new(None));
        let callback_for_watcher = callback.clone();

        let listener = Rc::new(RefCell::new(Self {
            _socket: socket,
            socket_path: path,
            source_id: None,
            callback,
        }));

        // Set up fd watcher on the GTK main loop.
        // This fires whenever data is available on the socket.
        let listener_weak = Rc::downgrade(&listener);
        let source_id =
            glib::unix_fd_add_local(socket_fd, glib::IOCondition::IN, move |fd, _condition| {
                // Read all available messages (socket is non-blocking).
                let mut buf = [0u8; 256];
                loop {
                    // SAFETY: fd is valid as long as the listener exists, and we read into a stack buffer.
                    let n = unsafe {
                        libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
                    };

                    if n <= 0 {
                        // No more data or error (EAGAIN/EWOULDBLOCK for non-blocking).
                        break;
                    }

                    let n = n as usize;
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        debug!("IPC: received message: {:?}", s);
                        if let Some(msg) = IpcMessage::from_wire(s) {
                            // Invoke the callback if registered.
                            if let Some(ref cb) = *callback_for_watcher.borrow() {
                                cb(msg);
                            }
                        } else {
                            debug!("IPC: ignoring unparseable message: {:?}", s);
                        }
                    }
                }

                // Check if the listener was dropped.
                if listener_weak.upgrade().is_none() {
                    return glib::ControlFlow::Break;
                }

                glib::ControlFlow::Continue
            });

        listener.borrow_mut().source_id = Some(source_id);

        Some(listener)
    }

    /// Register a callback for incoming messages.
    ///
    /// The callback is invoked directly on the GTK main loop when messages arrive.
    pub fn connect<F>(&self, callback: F)
    where
        F: Fn(IpcMessage) + 'static,
    {
        *self.callback.borrow_mut() = Some(Rc::new(callback));
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        // Remove the fd watcher from the main loop BEFORE the socket is closed.
        // This is essential: if _socket were dropped first, the glib source
        // would briefly hold a dangling fd. The manual Drop ensures correct
        // teardown order regardless of struct field declaration order.
        if let Some(source_id) = self.source_id.take() {
            source_id.remove();
        }

        // Clean up the socket file.
        let _ = std::fs::remove_file(&self.socket_path);

        debug!("IPC: listener stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        let cases = vec![
            IpcMessage::Volume {
                percent: 42,
                muted: false,
            },
            IpcMessage::Volume {
                percent: 100,
                muted: true,
            },
            IpcMessage::VolumeUnavailable,
            IpcMessage::Brightness { percent: 75 },
            IpcMessage::ToggleInhibitor,
        ];

        for msg in cases {
            let wire = msg.to_wire();
            let parsed = IpcMessage::from_wire(&wire).expect("failed to parse");
            assert_eq!(msg, parsed);
        }
    }

    #[test]
    fn test_from_wire_rejects_empty() {
        assert_eq!(IpcMessage::from_wire(""), None);
        assert_eq!(IpcMessage::from_wire("   "), None);
    }

    #[test]
    fn test_from_wire_rejects_garbage() {
        assert_eq!(IpcMessage::from_wire("hello"), None);
        assert_eq!(IpcMessage::from_wire("not_a_message"), None);
        assert_eq!(IpcMessage::from_wire("volume"), None);
        assert_eq!(IpcMessage::from_wire("brightness"), None);
    }

    #[test]
    fn test_from_wire_rejects_missing_fields() {
        // volume needs percent and muted
        assert_eq!(IpcMessage::from_wire("volume:"), None);
        assert_eq!(IpcMessage::from_wire("volume:42"), None);
        // brightness needs percent
        assert_eq!(IpcMessage::from_wire("brightness:"), None);
    }

    #[test]
    fn test_from_wire_rejects_non_numeric() {
        assert_eq!(IpcMessage::from_wire("volume:abc:0"), None);
        assert_eq!(IpcMessage::from_wire("brightness:xyz"), None);
    }

    #[test]
    fn test_from_wire_muted_non_one_is_unmuted() {
        // Any non-"1" value for the muted field is treated as unmuted.
        assert_eq!(
            IpcMessage::from_wire("volume:42:abc"),
            Some(IpcMessage::Volume {
                percent: 42,
                muted: false
            })
        );
        assert_eq!(
            IpcMessage::from_wire("volume:42:0"),
            Some(IpcMessage::Volume {
                percent: 42,
                muted: false
            })
        );
    }

    #[test]
    fn test_from_wire_rejects_extra_fields() {
        assert_eq!(IpcMessage::from_wire("volume:42:0:extra"), None);
    }

    #[test]
    fn test_from_wire_handles_whitespace() {
        // Leading/trailing whitespace should be trimmed
        assert_eq!(
            IpcMessage::from_wire("  volume:42:1  "),
            Some(IpcMessage::Volume {
                percent: 42,
                muted: true
            })
        );
        assert_eq!(
            IpcMessage::from_wire("\ntoggle_inhibitor\n"),
            Some(IpcMessage::ToggleInhibitor)
        );
    }
}
