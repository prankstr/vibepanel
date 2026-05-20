//! Shared sleep/resume notifications from logind.
//!
//! Services and widgets that need to refresh after suspend should subscribe here
//! instead of each owning a separate `PrepareForSleep` D-Bus subscription.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gio;
use tracing::{debug, warn};

use crate::services::callbacks::{CallbackId, Callbacks};

const LOGIND_SERVICE: &str = "org.freedesktop.login1";
const LOGIND_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const LOGIND_MANAGER_PATH: &str = "/org/freedesktop/login1";
const PREPARE_FOR_SLEEP_SIGNAL: &str = "PrepareForSleep";

/// Process-wide dispatcher for resume-from-sleep events.
pub struct SleepWatcher {
    callbacks: Callbacks<()>,
    subscription: RefCell<Option<gio::SignalSubscription>>,
}

impl SleepWatcher {
    fn new() -> Rc<Self> {
        let watcher = Rc::new(Self {
            callbacks: Callbacks::new(),
            subscription: RefCell::new(None),
        });

        Self::init_logind(&watcher);
        watcher
    }

    /// Get the global sleep watcher singleton.
    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<SleepWatcher> = SleepWatcher::new();
        }

        INSTANCE.with(|s| s.clone())
    }

    /// Register a callback invoked after the system resumes from sleep.
    pub fn on_resume<F>(&self, callback: F) -> CallbackId
    where
        F: Fn() + 'static,
    {
        self.callbacks.register(move |_| callback())
    }

    /// Disconnect a previously registered resume callback.
    pub fn disconnect(&self, id: CallbackId) -> bool {
        self.callbacks.unregister(id)
    }

    fn init_logind(this: &Rc<Self>) {
        let this_weak = Rc::downgrade(this);

        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                let connection = match res {
                    Ok(connection) => connection,
                    Err(e) => {
                        warn!(
                            "SleepWatcher: failed to connect to system bus: {}; resume callbacks disabled",
                            e
                        );
                        return;
                    }
                };

                let this_weak = Rc::downgrade(&this);
                let subscription = connection.subscribe_to_signal(
                    Some(LOGIND_SERVICE),
                    Some(LOGIND_MANAGER_IFACE),
                    Some(PREPARE_FOR_SLEEP_SIGNAL),
                    Some(LOGIND_MANAGER_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        // PrepareForSleep(boolean): true = suspending, false = resumed.
                        if let Some(preparing) = signal.parameters.child_value(0).get::<bool>()
                            && !preparing
                            && let Some(this) = this_weak.upgrade()
                        {
                            debug!("SleepWatcher: system resumed from sleep");
                            this.callbacks.notify(&());
                        }
                    },
                );

                *this.subscription.borrow_mut() = Some(subscription);
            },
        );
    }
}
