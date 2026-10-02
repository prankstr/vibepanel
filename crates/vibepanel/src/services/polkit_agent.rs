//! Polkit authentication agent.
//!
//! Registers `org.freedesktop.PolicyKit1.AuthenticationAgent` on the system bus
//! so `pkexec`, `systemctl` and other polkit-gated actions prompt in VibePanel
//! instead of needing a separate agent (polkit-gnome, hyprpolkitagent, ...).
//!
//! The D-Bus side is ours (same gio pattern as the VPN secret agent). Password
//! verification goes through libpolkit-agent-1's `PolkitAgentSession`, loaded
//! at runtime, which spawns the setuid helper or uses polkit's socket-activated
//! helper, whichever the installed polkit provides.
//!
//! Only one agent can be registered per session, so this is opt-in via
//! `[polkit] enabled`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::rc::Rc;

use gtk4::gio::{self, prelude::*};
use gtk4::glib::{self, Variant};
use tracing::{debug, info, warn};

use crate::services::desktop_notification::{self, Urgency};

const POLKIT_NAME: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const AGENT_IFACE: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";
const AGENT_PATH: &str = "/org/vibepanel/PolicyKit1/AuthenticationAgent";

const ERROR_CANCELLED: &str = "org.freedesktop.PolicyKit1.Error.Cancelled";
const ERROR_FAILED: &str = "org.freedesktop.PolicyKit1.Error.Failed";

const DBUS_TIMEOUT_MS: i32 = 10_000;

const AGENT_INTROSPECTION: &str = r#"
<node>
    <interface name="org.freedesktop.PolicyKit1.AuthenticationAgent">
        <method name="BeginAuthentication">
            <arg type="s" name="action_id" direction="in"/>
            <arg type="s" name="message" direction="in"/>
            <arg type="s" name="icon_name" direction="in"/>
            <arg type="a{ss}" name="details" direction="in"/>
            <arg type="s" name="cookie" direction="in"/>
            <arg type="a(sa{sv})" name="identities" direction="in"/>
        </method>
        <method name="CancelAuthentication">
            <arg type="s" name="cookie" direction="in"/>
        </method>
    </interface>
</node>
"#;

#[derive(Debug, Clone)]
pub struct AuthView {
    pub message: String,
    pub user: String,
    /// Authenticating as someone other than the logged-in user.
    pub other_user: bool,
    /// `None` while waiting, or when PAM only sent info (fingerprint).
    pub prompt: Option<Prompt>,
    pub info: Option<String>,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub label: String,
    /// PAM's `echo_on`: input may be shown (username, OTP).
    pub echo: bool,
}

type ViewHandler = Rc<dyn Fn(Option<&AuthView>)>;

struct Request {
    invocation: gio::DBusMethodInvocation,
    cookie: String,
    message: String,
    uid: u32,
    other_user: bool,
}

struct Active {
    request: Request,
    session: Option<ffi::Session>,
    /// A failure without a response is PAM failing on its own; retrying would loop.
    responded: bool,
    view: AuthView,
}

pub struct PolkitAgent {
    enabled: Cell<bool>,
    connection: RefCell<Option<gio::DBusConnection>>,
    object_id: RefCell<Option<gio::RegistrationId>>,
    /// Closure because gio 0.21's `gio::WatcherId` shadows the type
    /// `bus_watch_name_on_connection` returns.
    unwatch: RefCell<Option<Box<dyn FnOnce()>>>,
    /// Unique name of the running polkitd; only it may call us.
    polkit_owner: RefCell<Option<String>>,
    /// polkitd instance and subject we're registered with.
    registered: RefCell<Option<(String, Variant)>>,
    active: RefCell<Option<Active>>,
    queue: RefCell<VecDeque<Request>>,
    /// Bumped per session; signals from older sessions are ignored.
    generation: Cell<u64>,
    conflict_notified: Cell<bool>,
    on_view: RefCell<Option<ViewHandler>>,
}

impl PolkitAgent {
    fn new() -> Self {
        Self {
            enabled: Cell::new(false),
            connection: RefCell::new(None),
            object_id: RefCell::new(None),
            unwatch: RefCell::new(None),
            polkit_owner: RefCell::new(None),
            registered: RefCell::new(None),
            active: RefCell::new(None),
            queue: RefCell::new(VecDeque::new()),
            generation: Cell::new(0),
            conflict_notified: Cell::new(false),
            on_view: RefCell::new(None),
        }
    }

    pub fn global() -> Rc<Self> {
        thread_local! {
            static INSTANCE: Rc<PolkitAgent> = Rc::new(PolkitAgent::new());
        }
        INSTANCE.with(|s| s.clone())
    }

    pub fn set_view_handler(&self, handler: impl Fn(Option<&AuthView>) + 'static) {
        *self.on_view.borrow_mut() = Some(Rc::new(handler));
    }

    pub fn set_enabled(&self, enabled: bool) {
        if enabled == self.enabled.get() {
            return;
        }
        self.enabled.set(enabled);
        if enabled {
            self.enable();
        } else {
            self.disable();
        }
    }

    fn enable(&self) {
        if let Err(err) = ffi::lib() {
            warn!("Polkit agent disabled: {err}");
            self.enabled.set(false);
            return;
        }
        gio::bus_get(gio::BusType::System, None::<&gio::Cancellable>, |result| {
            let agent = Self::global();
            if !agent.enabled.get() || agent.connection.borrow().is_some() {
                return;
            }
            match result {
                Ok(connection) => agent.setup(connection),
                Err(err) => warn!("Polkit agent: no system bus: {err}"),
            }
        });
    }

    fn setup(&self, connection: gio::DBusConnection) {
        let interface = gio::DBusNodeInfo::for_xml(AGENT_INTROSPECTION)
            .ok()
            .and_then(|node| node.lookup_interface(AGENT_IFACE))
            .expect("valid polkit agent introspection XML");

        let registration = connection
            .register_object(AGENT_PATH, &interface)
            .method_call(|_conn, sender, _path, _iface, method, params, invocation| {
                Self::global().handle_method(sender, method, params, invocation);
            })
            .build();
        match registration {
            Ok(id) => *self.object_id.borrow_mut() = Some(id),
            Err(err) => {
                warn!("Polkit agent: failed to export {AGENT_PATH}: {err}");
                return;
            }
        }

        // AUTO_START: polkitd is often D-Bus activated; if the first auth request
        // started it instead, no agent would be registered yet.
        let watch_id = gio::bus_watch_name_on_connection(
            &connection,
            POLKIT_NAME,
            gio::BusNameWatcherFlags::AUTO_START,
            |connection, _name, owner| {
                let agent = Self::global();
                *agent.polkit_owner.borrow_mut() = Some(owner.to_string());
                agent.register_with_authority(connection, owner.to_string());
            },
            |_connection, _name| {
                let agent = Self::global();
                *agent.polkit_owner.borrow_mut() = None;
                *agent.registered.borrow_mut() = None;
                agent.cancel_all();
            },
        );
        *self.unwatch.borrow_mut() = Some(Box::new(move || gio::bus_unwatch_name(watch_id)));
        *self.connection.borrow_mut() = Some(connection);
    }

    fn disable(&self) {
        self.cancel_all();
        if let Some(unwatch) = self.unwatch.take() {
            unwatch();
        }
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Some((polkitd, subject)) = self.registered.take() {
            unregister(&connection, &polkitd, subject);
        }
        if let Some(id) = self.object_id.take() {
            let _ = connection.unregister_object(id);
        }
        *self.polkit_owner.borrow_mut() = None;
        self.conflict_notified.set(false);
    }

    fn serves(&self, polkitd: &str) -> bool {
        self.enabled.get()
            && self.object_id.borrow().is_some()
            && self.polkit_owner.borrow().as_deref() == Some(polkitd)
    }

    fn register_with_authority(&self, connection: gio::DBusConnection, polkitd: String) {
        let lookup = connection.clone();
        resolve_session_id(&lookup, move |session_id| {
            let agent = Self::global();
            if !agent.serves(&polkitd) {
                return;
            }
            let Some(session_id) = session_id else {
                warn!(
                    "Polkit agent: no login session found (needs systemd-logind, elogind or \
                     XDG_SESSION_ID); not registering"
                );
                return;
            };
            let subject = session_subject(&session_id);
            let locale = std::env::var("LANG").unwrap_or_else(|_| "C".to_string());
            let params = Variant::tuple_from_iter([
                subject.clone(),
                locale.to_variant(),
                AGENT_PATH.to_variant(),
            ]);
            let reply_connection = connection.clone();
            // The recorded instance, not the well-known name: a restarted polkitd
            // must not get a registration we'd then ignore.
            let destination = polkitd.clone();
            connection.call(
                Some(&destination),
                AUTHORITY_PATH,
                AUTHORITY_IFACE,
                "RegisterAuthenticationAgent",
                Some(&params),
                None,
                gio::DBusCallFlags::NONE,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                move |result| {
                    // Judge by current state: after a quick disable/enable this
                    // registration is still valid for the re-exported object.
                    let agent = Self::global();
                    let current = agent.polkit_owner.borrow().clone();
                    match result {
                        Ok(_) if agent.serves(&polkitd) => {
                            info!("Polkit agent registered for session {session_id}");
                            *agent.registered.borrow_mut() = Some((polkitd, subject));
                            agent.conflict_notified.set(false);
                        }
                        Ok(_) if current.is_some_and(|owner| owner != polkitd) => {}
                        Ok(_) => unregister(&reply_connection, &polkitd, subject),
                        // "already exists" from our own kept registration.
                        Err(_) if agent.registered.borrow().is_some() => {}
                        // Stale reply: disabled since, or polkitd changed.
                        Err(err) if !agent.serves(&polkitd) => {
                            debug!("Polkit agent: ignoring stale registration error: {err}");
                        }
                        Err(err) if err.message().contains("already exists") => {
                            warn!("Polkit agent: another agent is already registered: {err}");
                            agent.notify_conflict();
                        }
                        Err(err) => warn!("Polkit agent: registration failed: {err}"),
                    }
                },
            );
        });
    }

    fn notify_conflict(&self) {
        if self.conflict_notified.replace(true) {
            return;
        }
        desktop_notification::send_with_id(
            "Polkit agent not started",
            "Another polkit agent is already running",
            "dialog-warning",
            Urgency::Normal,
            false,
            false,
            |_| {},
        );
    }

    fn handle_method(
        &self,
        sender: Option<&str>,
        method: &str,
        params: Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        if sender.is_none() || sender != self.polkit_owner.borrow().as_deref() {
            invocation.return_dbus_error(ERROR_FAILED, "Only polkitd may call this agent");
            return;
        }
        match method {
            "BeginAuthentication" => self.begin(params, invocation),
            "CancelAuthentication" => {
                if let Some((cookie,)) = params.get::<(String,)>() {
                    self.cancel_cookie(&cookie);
                }
                invocation.return_value(None);
            }
            _ => invocation.return_dbus_error(ERROR_FAILED, "Unknown method"),
        }
    }

    fn begin(&self, params: Variant, invocation: gio::DBusMethodInvocation) {
        type Args = (
            String,
            String,
            String,
            HashMap<String, String>,
            String,
            Vec<(String, HashMap<String, Variant>)>,
        );
        let Some((action_id, message, _icon, _details, cookie, identities)) = params.get::<Args>()
        else {
            invocation.return_dbus_error(ERROR_FAILED, "Invalid arguments");
            return;
        };
        let uids = unix_user_ids(&identities);
        // SAFETY: getuid() cannot fail.
        let own_uid = unsafe { libc::getuid() };
        // Authenticates as a single identity; there is no identity picker.
        let Some(uid) = choose_uid(&uids, own_uid) else {
            invocation.return_dbus_error(ERROR_FAILED, "No unix-user identity to authenticate");
            return;
        };
        debug!("Polkit agent: request for {action_id} as uid {uid} (offered {uids:?})");
        self.queue.borrow_mut().push_back(Request {
            invocation,
            cookie,
            message,
            uid,
            other_user: uid != own_uid,
        });
        self.start_next();
    }

    fn start_next(&self) {
        if self.active.borrow().is_some() {
            return;
        }
        let Some(request) = self.queue.borrow_mut().pop_front() else {
            self.emit();
            return;
        };
        let view = AuthView {
            message: request.message.clone(),
            user: user_name(request.uid),
            other_user: request.other_user,
            prompt: None,
            info: None,
            error: None,
            busy: false,
        };
        *self.active.borrow_mut() = Some(Active {
            request,
            session: None,
            responded: false,
            view,
        });
        self.start_session();
    }

    fn start_session(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let session = {
            let mut guard = self.active.borrow_mut();
            let Some(active) = guard.as_mut() else {
                return;
            };
            let session = ffi::Session::new(active.request.uid, &active.request.cookie);
            active.responded = false;
            active.session = session.clone();
            session
        };
        let Some(session) = session else {
            self.finish(End::Failed("Could not start polkit session"));
            return;
        };

        session.connect_request(move |label, echo| {
            Self::global().update(generation, |view| {
                view.prompt = Some(Prompt {
                    label: label.trim().trim_end_matches(':').to_string(),
                    echo,
                });
                // Keep info: pam_faillock sends the lockout notice before the prompt.
                view.busy = false;
            });
        });
        session.connect_show_info(move |text| {
            Self::global().update(generation, |view| append_line(&mut view.info, text));
        });
        session.connect_show_error(move |text| {
            Self::global().update(generation, |view| view.error = Some(text.to_string()));
        });
        // Deferred: `completed` can fire synchronously inside initiate()/cancel().
        session.connect_completed(move |gained| {
            glib::idle_add_local_once(move || Self::global().on_completed(generation, gained));
        });
        self.emit();
        session.initiate();
    }

    fn update(&self, generation: u64, apply: impl FnOnce(&mut AuthView)) {
        if generation != self.generation.get() {
            return;
        }
        if let Some(active) = self.active.borrow_mut().as_mut() {
            apply(&mut active.view);
        }
        self.emit();
    }

    fn on_completed(&self, generation: u64, gained: bool) {
        if generation != self.generation.get() {
            return;
        }
        let outcome = {
            let mut active = self.active.borrow_mut();
            let Some(active) = active.as_mut() else {
                return;
            };
            let outcome = outcome(gained, active.responded);
            if outcome == Outcome::Retry {
                prepare_retry(&mut active.view);
            }
            outcome
        };
        match outcome {
            Outcome::Success => self.finish(End::Granted),
            Outcome::Fail(message) => self.finish(End::Failed(message)),
            Outcome::Retry => self.start_session(),
        }
    }

    pub fn respond(&self, response: &CStr) {
        let session = {
            let mut active = self.active.borrow_mut();
            let Some(active) = active.as_mut() else {
                return;
            };
            if active.view.prompt.is_none() || active.view.busy {
                return;
            }
            active.responded = true;
            active.view.busy = true;
            active.view.error = None;
            active.view.info = None;
            active.session.clone()
        };
        // Before any UI code runs: `response` borrows the entry buffer (see submit()).
        if let Some(session) = session {
            session.response(response);
        }
        self.emit();
    }

    pub fn cancel(&self) {
        self.finish(End::Dismissed);
    }

    fn cancel_cookie(&self, cookie: &str) {
        let is_active = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.request.cookie == cookie);
        if is_active {
            self.finish(End::Dismissed);
            return;
        }
        let mut queue = self.queue.borrow_mut();
        if let Some(index) = queue.iter().position(|r| r.cookie == cookie) {
            let request = queue.remove(index).expect("index from position");
            request
                .invocation
                .return_dbus_error(ERROR_CANCELLED, "Cancelled by polkit");
        }
    }

    fn cancel_all(&self) {
        for request in self.queue.take() {
            request
                .invocation
                .return_dbus_error(ERROR_CANCELLED, "Agent stopped");
        }
        self.finish(End::Dismissed);
    }

    fn finish(&self, end: End) {
        self.generation.set(self.generation.get() + 1);
        let Some(active) = self.active.take() else {
            return;
        };
        if let Some(session) = &active.session {
            session.cancel();
        }
        let invocation = active.request.invocation;
        match end {
            End::Granted => invocation.return_value(None),
            End::Dismissed => invocation.return_dbus_error(ERROR_CANCELLED, "Dismissed"),
            End::Failed(message) => invocation.return_dbus_error(ERROR_FAILED, message),
        }
        self.start_next();
    }

    fn emit(&self) {
        let Some(handler) = self.on_view.borrow().clone() else {
            return;
        };
        let view = self.active.borrow().as_ref().map(|a| a.view.clone());
        handler(view.as_ref());
    }
}

enum End {
    Granted,
    Dismissed,
    Failed(&'static str),
}

fn unregister(connection: &gio::DBusConnection, polkitd: &str, subject: Variant) {
    let params = Variant::tuple_from_iter([subject, AGENT_PATH.to_variant()]);
    connection.call(
        Some(polkitd),
        AUTHORITY_PATH,
        AUTHORITY_IFACE,
        "UnregisterAuthenticationAgent",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        |result| match result {
            Ok(_) => info!("Polkit agent unregistered"),
            Err(err) => debug!("Polkit agent: unregister failed: {err}"),
        },
    );
}

fn append_line(slot: &mut Option<String>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    match slot {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(text);
        }
        None => *slot = Some(text.to_string()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Success,
    Retry,
    Fail(&'static str),
}

/// Wrong passwords always retry: PAM (pam_faillock, fail delays) owns the
/// attempt policy and reports lockouts in the next session's preauth.
fn outcome(gained: bool, responded: bool) -> Outcome {
    if gained {
        Outcome::Success
    } else if !responded {
        Outcome::Fail("Authentication failed")
    } else {
        Outcome::Retry
    }
}

/// `respond()` clears `error`, so any error left came from PAM this attempt
/// (expired account, access denied); keep it over the generic message.
fn prepare_retry(view: &mut AuthView) {
    view.prompt = None;
    view.info = None;
    view.busy = false;
    view.error
        .get_or_insert_with(|| "Authentication failed, try again".to_string());
}

/// `XDG_SESSION_ID`, else logind's `session/auto` (which falls back to the
/// user's display session when running outside one, e.g. a user service).
fn resolve_session_id(
    connection: &gio::DBusConnection,
    done: impl FnOnce(Option<String>) + 'static,
) {
    if let Some(id) = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty())
    {
        done(Some(id));
        return;
    }
    connection.call(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&("org.freedesktop.login1.Session", "Id").to_variant()),
        Some(glib::VariantTy::new("(v)").expect("valid type")),
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            let id = result
                .ok()
                .and_then(|reply| reply.child_value(0).as_variant())
                .and_then(|id| id.str().map(str::to_string));
            done(id);
        },
    );
}

fn session_subject(session_id: &str) -> Variant {
    let details = HashMap::from([("session-id".to_string(), session_id.to_variant())]);
    ("unix-session", details).to_variant()
}

/// polkitd already expands admin groups into users, so groups can be ignored.
fn unix_user_ids(identities: &[(String, HashMap<String, Variant>)]) -> Vec<u32> {
    identities
        .iter()
        .filter(|(kind, _)| kind == "unix-user")
        .filter_map(|(_, details)| details.get("uid")?.get::<u32>())
        .collect()
}

/// Ourselves, then root, then the first offered.
fn choose_uid(uids: &[u32], own_uid: u32) -> Option<u32> {
    [own_uid, 0]
        .into_iter()
        .find(|uid| uids.contains(uid))
        .or_else(|| uids.first().copied())
}

fn user_name(uid: u32) -> String {
    // SAFETY: getpwuid returns NULL or a pointer to static storage; only
    // called from the main thread, and the name is copied out immediately.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() {
            return uid.to_string();
        }
        CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    }
}

/// libpolkit-agent-1, dlopened so builds don't need polkit headers; the agent
/// stays off when the library is missing.
mod ffi {
    use std::ffi::{CStr, CString, c_char, c_int};
    use std::sync::OnceLock;

    use gtk4::glib::{self, gobject_ffi::GObject, prelude::*, translate::*};

    type ObjectPtr = *mut GObject;

    struct Lib {
        unix_user_new: unsafe extern "C" fn(c_int) -> ObjectPtr,
        session_new: unsafe extern "C" fn(ObjectPtr, *const c_char) -> ObjectPtr,
        initiate: unsafe extern "C" fn(ObjectPtr),
        response: unsafe extern "C" fn(ObjectPtr, *const c_char),
        cancel: unsafe extern "C" fn(ObjectPtr),
    }

    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

    fn load() -> Result<Lib, String> {
        // SAFETY: loading polkit's libraries runs no unusual initializers, and
        // the signatures below match polkit's public headers.
        unsafe {
            let agent = libloading::Library::new("libpolkit-agent-1.so.0")
                .map_err(|e| format!("libpolkit-agent-1 not found: {e}"))?;
            let missing = |e: libloading::Error| format!("libpolkit-agent-1 symbol missing: {e}");
            let lib = Lib {
                // From libpolkit-gobject-1; dlsym also searches the handle's dependencies.
                unix_user_new: *agent.get(b"polkit_unix_user_new\0").map_err(missing)?,
                session_new: *agent.get(b"polkit_agent_session_new\0").map_err(missing)?,
                initiate: *agent
                    .get(b"polkit_agent_session_initiate\0")
                    .map_err(missing)?,
                response: *agent
                    .get(b"polkit_agent_session_response\0")
                    .map_err(missing)?,
                cancel: *agent
                    .get(b"polkit_agent_session_cancel\0")
                    .map_err(missing)?,
            };
            // Never unload: polkit registers GTypes, which can't be unregistered.
            std::mem::forget(agent);
            Ok(lib)
        }
    }

    pub(super) fn lib() -> Result<(), &'static str> {
        LIB.get_or_init(load)
            .as_ref()
            .map(|_| ())
            .map_err(|e| e.as_str())
    }

    fn loaded() -> &'static Lib {
        LIB.get()
            .and_then(|lib| lib.as_ref().ok())
            .expect("polkit library checked before use")
    }

    #[derive(Clone)]
    pub(super) struct Session(glib::Object);

    impl Session {
        pub(super) fn new(uid: u32, cookie: &str) -> Option<Self> {
            let cookie = CString::new(cookie).ok()?;
            let lib = loaded();
            // SAFETY: both constructors return a new (full) reference or NULL;
            // the session takes its own reference to the identity.
            unsafe {
                let identity = (lib.unix_user_new)(uid as c_int);
                if identity.is_null() {
                    return None;
                }
                let identity: glib::Object = from_glib_full(identity);
                let session = (lib.session_new)(identity.as_ptr(), cookie.as_ptr());
                (!session.is_null()).then(|| Self(from_glib_full(session)))
            }
        }

        pub(super) fn initiate(&self) {
            // SAFETY: self.0 is a live PolkitAgentSession.
            unsafe { (loaded().initiate)(self.0.as_ptr()) }
        }

        pub(super) fn response(&self, response: &CStr) {
            // SAFETY: self.0 is a live PolkitAgentSession; the helper copies the string.
            unsafe { (loaded().response)(self.0.as_ptr(), response.as_ptr()) }
        }

        pub(super) fn cancel(&self) {
            // SAFETY: self.0 is a live PolkitAgentSession; cancelling twice is a no-op.
            unsafe { (loaded().cancel)(self.0.as_ptr()) }
        }

        pub(super) fn connect_request(&self, f: impl Fn(&str, bool) + 'static) {
            self.0.connect_local("request", false, move |args| {
                let text = args.get(1)?.get::<String>().ok()?;
                let echo = args.get(2)?.get::<bool>().ok()?;
                f(&text, echo);
                None
            });
        }

        pub(super) fn connect_show_info(&self, f: impl Fn(&str) + 'static) {
            self.connect_text("show-info", f);
        }

        pub(super) fn connect_show_error(&self, f: impl Fn(&str) + 'static) {
            self.connect_text("show-error", f);
        }

        fn connect_text(&self, signal: &str, f: impl Fn(&str) + 'static) {
            self.0.connect_local(signal, false, move |args| {
                f(&args.get(1)?.get::<String>().ok()?);
                None
            });
        }

        pub(super) fn connect_completed(&self, f: impl Fn(bool) + 'static) {
            self.0.connect_local("completed", false, move |args| {
                f(args.get(1)?.get::<bool>().ok()?);
                None
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uid: u32) -> (String, HashMap<String, Variant>) {
        (
            "unix-user".to_string(),
            HashMap::from([("uid".to_string(), uid.to_variant())]),
        )
    }

    #[test]
    fn parses_unix_users_from_wire_format() {
        let group = (
            "unix-group".to_string(),
            HashMap::from([("gid".to_string(), 10_u32.to_variant())]),
        );
        let wire = vec![user(1001), group, user(0)].to_variant();
        let identities: Vec<(String, HashMap<String, Variant>)> = wire.get().unwrap();
        assert_eq!(unix_user_ids(&identities), vec![1001, 0]);
    }

    #[test]
    fn chooses_own_uid_then_root_then_first() {
        assert_eq!(choose_uid(&[1001, 1000, 0], 1000), Some(1000));
        assert_eq!(choose_uid(&[1001, 0], 1000), Some(0));
        assert_eq!(choose_uid(&[1001, 1002], 1000), Some(1001));
        assert_eq!(choose_uid(&[], 1000), None);
    }

    #[test]
    fn retries_wrong_passwords_but_not_pam_failures() {
        assert_eq!(outcome(true, true), Outcome::Success);
        assert_eq!(outcome(false, true), Outcome::Retry);
        // PAM failed without a response (requisite preauth etc.): no retry loop.
        assert!(matches!(outcome(false, false), Outcome::Fail(_)));
    }

    #[test]
    fn retry_keeps_pam_error_over_generic_message() {
        let mut view = AuthView {
            message: String::new(),
            user: String::new(),
            other_user: false,
            prompt: Some(Prompt {
                label: "Password".into(),
                echo: false,
            }),
            info: Some("info".into()),
            error: Some("Your account has expired".into()),
            busy: true,
        };
        prepare_retry(&mut view);
        assert!(view.prompt.is_none() && view.info.is_none() && !view.busy);
        assert_eq!(view.error.as_deref(), Some("Your account has expired"));

        view.error = None;
        prepare_retry(&mut view);
        assert_eq!(
            view.error.as_deref(),
            Some("Authentication failed, try again")
        );
    }

    #[test]
    fn keeps_every_pam_info_message() {
        let mut info = None;
        append_line(&mut info, "The account is locked due to 3 failed logins.");
        append_line(&mut info, "  ");
        append_line(&mut info, "(10 minutes left to unlock)\n");
        assert_eq!(
            info.as_deref(),
            Some("The account is locked due to 3 failed logins.\n(10 minutes left to unlock)")
        );
    }

    #[test]
    fn session_subject_matches_polkit_signature() {
        let subject = session_subject("3");
        assert_eq!(subject.type_().as_str(), "(sa{sv})");
        let (kind, details): (String, HashMap<String, Variant>) = subject.get().unwrap();
        assert_eq!(kind, "unix-session");
        assert_eq!(details["session-id"].str(), Some("3"));
    }
}
