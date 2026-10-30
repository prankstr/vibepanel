//! Event loop and global probe for Wayland clients running on their own
//! connection and thread.

use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

const POLL_TIMEOUT_MS: i32 = 500;

/// Dispatch `queue` until `running` is cleared or the connection fails.
///
/// Returns `false` if it stopped because of a terminal error rather than a
/// requested stop.
pub(super) fn run<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    running: &AtomicBool,
    label: &str,
) -> bool {
    while running.load(Ordering::SeqCst) {
        if let Err(e) = queue.dispatch_pending(state) {
            warn!("{label} dispatch failed: {e}");
            return false;
        }
        if let Err(e) = queue.flush() {
            warn!("{label} flush failed: {e}");
            return false;
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut pfd = libc::pollfd {
            fd: guard.connection_fd().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, length 1.
        let ready = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
        if ready <= 0 {
            continue; // timeout (re-check `running`) or EINTR; guard is dropped
        }
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                warn!("{label} read failed: {e}");
                return false;
            }
        }
    }
    true
}

/// Whether the compositor advertises any of `interfaces` (one roundtrip).
pub(super) fn advertises_any(interfaces: &[&str]) -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<ProbeState>(&conn) else {
        return false;
    };
    globals.contents().with_list(|list| {
        list.iter()
            .any(|g| interfaces.contains(&g.interface.as_str()))
    })
}

struct ProbeState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
