//! Cava audio visualizer service.
//!
//! Spawns [cava](https://github.com/karlstav/cava) as a subprocess with raw
//! binary output to provide real-time audio frequency spectrum data.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk4::glib;
use tracing::{debug, warn};

use crate::services::callbacks::{CallbackId, Callbacks};

/// Number of frequency bars to request from cava.
pub const NUM_BARS: usize = 24;

/// Cava output framerate.
const FRAMERATE: u32 = 60;

/// Snapshot of the current cava audio spectrum state.
#[derive(Clone, Copy, Debug, Default)]
pub struct CavaSnapshot {
    pub running: bool,
    /// Normalized to 0.0-1.0.
    pub bars: [f32; NUM_BARS],
}

/// Process-wide cava audio visualizer service.
pub struct CavaService {
    snapshot: RefCell<CavaSnapshot>,
    callbacks: Callbacks<CavaSnapshot>,
    stop_flag: RefCell<Option<Arc<AtomicBool>>>,
    child_pid: RefCell<Option<u32>>,
    cava_available: Cell<bool>,
    config_written: Cell<bool>,
    generation: Cell<u64>,
}

impl CavaService {
    fn new() -> Rc<Self> {
        let available = check_cava_installed();
        if available {
            debug!("cava binary found, audio visualizer available");
        } else {
            debug!("cava binary not found, audio visualizer disabled");
        }

        Rc::new(Self {
            snapshot: RefCell::new(CavaSnapshot::default()),
            callbacks: Callbacks::new(),
            stop_flag: RefCell::new(None),
            child_pid: RefCell::new(None),
            cava_available: Cell::new(available),
            config_written: Cell::new(false),
            generation: Cell::new(0),
        })
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<CavaService> = CavaService::new();
        }
        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback for spectrum updates.
    /// Immediately invoked with the current snapshot.
    pub fn connect<F>(&self, callback: F) -> CallbackId
    where
        F: Fn(&CavaSnapshot) + 'static,
    {
        let id = self.callbacks.register(callback);
        self.callbacks.notify_single(id, &self.snapshot.borrow());
        id
    }

    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    pub fn available(&self) -> bool {
        self.cava_available.get()
    }

    /// Start the cava subprocess. No-op if unavailable or already running.
    pub fn start(&self) {
        if !self.cava_available.get() {
            return;
        }
        if self.stop_flag.borrow().is_some() {
            return;
        }

        let run_gen = self.generation.get().wrapping_add(1);
        self.generation.set(run_gen);

        let config_path = cava_config_path();
        if !self.config_written.get() {
            let config_content = format!(
                "[general]\n\
                 bars = {NUM_BARS}\n\
                 framerate = {FRAMERATE}\n\
                 [output]\n\
                 method = raw\n\
                 raw_target = /dev/stdout\n\
                 data_format = binary\n\
                 bit_format = 8bit\n\
                 channels = mono\n"
            );
            if let Err(e) = std::fs::write(&config_path, &config_content) {
                warn!("Failed to write cava config to {}: {}", config_path, e);
                return;
            }
            self.config_written.set(true);
        }

        let child = match Command::new("cava")
            .arg("-p")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                warn!("Failed to spawn cava: {}", e);
                self.cava_available.set(false);
                self.callbacks.notify(&self.snapshot.borrow());
                return;
            }
        };

        let pid = child.id();
        *self.child_pid.borrow_mut() = Some(pid);

        let stop_flag = Arc::new(AtomicBool::new(false));
        *self.stop_flag.borrow_mut() = Some(stop_flag.clone());

        {
            let mut snap = self.snapshot.borrow_mut();
            snap.running = true;
        }
        self.callbacks.notify(&self.snapshot.borrow());

        let stop = stop_flag;
        std::thread::spawn(move || {
            reader_thread(child, stop, run_gen);
        });

        debug!("cava subprocess started (pid {})", pid);
    }

    /// Stop the cava subprocess if running.
    pub fn stop(&self) {
        let stop_flag = self.stop_flag.borrow_mut().take();
        if let Some(flag) = stop_flag {
            flag.store(true, Ordering::Relaxed);
        }

        if let Some(pid) = self.child_pid.borrow_mut().take() {
            // SAFETY: There is a small race window where the reader thread has
            // already called child.wait() (reaping the process) but the
            // on_process_exited idle callback hasn't run yet to clear child_pid.
            // In that case this kill() targets a reaped PID — it returns ESRCH
            // harmlessly. PID reuse within this window is theoretically possible
            // but astronomically unlikely on modern kernels.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            debug!("cava subprocess stopped (pid {})", pid);
        }

        // Bump generation so in-flight data from the old run is ignored
        self.generation.set(self.generation.get().wrapping_add(1));

        {
            let mut snap = self.snapshot.borrow_mut();
            snap.running = false;
            snap.bars.fill(0.0);
        }
        self.callbacks.notify(&self.snapshot.borrow());
    }

    /// Called from the main thread when new bar data arrives.
    fn on_bars_received(&self, bars: [f32; NUM_BARS], run_gen: u64) {
        if run_gen != self.generation.get() {
            return; // Stale data from a previous run
        }

        {
            let mut snap = self.snapshot.borrow_mut();
            snap.bars = bars;
            snap.running = true;
        }
        self.callbacks.notify(&self.snapshot.borrow());
    }

    /// Called from the main thread when the cava process exits.
    fn on_process_exited(&self, run_gen: u64) {
        if run_gen != self.generation.get() {
            return;
        }

        *self.stop_flag.borrow_mut() = None;
        *self.child_pid.borrow_mut() = None;

        {
            let mut snap = self.snapshot.borrow_mut();
            snap.running = false;
            snap.bars.fill(0.0);
        }
        self.callbacks.notify(&self.snapshot.borrow());
        debug!("cava subprocess exited");
    }
}

impl Drop for CavaService {
    fn drop(&mut self) {
        if let Some(flag) = self.stop_flag.borrow().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(pid) = self.child_pid.borrow().as_ref() {
            // See safety comment in stop() — same race window applies here.
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_file(cava_config_path());
    }
}

/// Background thread: reads raw binary frames from cava's stdout and posts them to the main thread.
fn reader_thread(mut child: std::process::Child, stop_flag: Arc<AtomicBool>, run_gen: u64) {
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            warn!("cava: no stdout pipe");
            glib::idle_add_once(move || {
                CavaService::global().on_process_exited(run_gen);
            });
            return;
        }
    };

    let mut buf = vec![0u8; NUM_BARS];

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        match stdout.read_exact(&mut buf) {
            Ok(()) => {
                let mut bars = [0.0f32; NUM_BARS];
                for (b, &raw) in bars.iter_mut().zip(buf.iter()) {
                    *b = raw as f32 / 255.0;
                }
                glib::idle_add_once(move || {
                    CavaService::global().on_bars_received(bars, run_gen);
                });
            }
            Err(_) => {
                // EOF or read error — cava exited
                break;
            }
        }
    }

    let _ = child.wait();

    glib::idle_add_once(move || {
        CavaService::global().on_process_exited(run_gen);
    });
}

fn check_cava_installed() -> bool {
    Command::new("which")
        .arg("cava")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn cava_config_path() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/vibepanel-cava.conf", runtime_dir)
}
