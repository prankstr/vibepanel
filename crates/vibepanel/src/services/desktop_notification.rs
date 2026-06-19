//! Freedesktop desktop notification sender.

use std::collections::HashMap;

use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use tracing::warn;

const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_IFACE: &str = "org.freedesktop.Notifications";
const DBUS_CALL_TIMEOUT_MS: i32 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    fn as_hint(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

pub fn send_with_id<F>(
    summary: &str,
    body: &str,
    icon: &str,
    urgency: Urgency,
    transient: bool,
    close_toast_on_close: bool,
    on_sent: F,
) where
    F: Fn(Option<u32>) + 'static,
{
    let summary = summary.to_string();
    let body = body.to_string();
    let icon = icon.to_string();
    let on_sent = std::rc::Rc::new(on_sent);

    gio::bus_get(
        gio::BusType::Session,
        None::<&gio::Cancellable>,
        move |result| {
            let connection = match result {
                Ok(connection) => connection,
                Err(err) => {
                    warn!("Failed to get session bus for notification: {err}");
                    on_sent(None);
                    return;
                }
            };

            let actions: Vec<String> = Vec::new();
            let mut hints: HashMap<String, glib::Variant> = HashMap::new();
            hints.insert("urgency".to_string(), urgency.as_hint().to_variant());
            hints.insert("transient".to_string(), transient.to_variant());
            hints.insert(
                "x-vibepanel-close-toast-on-close".to_string(),
                close_toast_on_close.to_variant(),
            );
            let params = (
                "vibepanel",
                0_u32,
                icon.as_str(),
                summary.as_str(),
                body.as_str(),
                actions,
                hints,
                -1_i32,
            )
                .to_variant();

            connection.call(
                Some(NOTIFICATIONS_NAME),
                NOTIFICATIONS_PATH,
                NOTIFICATIONS_IFACE,
                "Notify",
                Some(&params),
                Some(glib::VariantTy::new("(u)").unwrap()),
                gio::DBusCallFlags::NONE,
                DBUS_CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                {
                    let on_sent = on_sent.clone();
                    move |result| match result {
                        Ok(reply) => {
                            if let Some((id,)) = reply.get::<(u32,)>() {
                                on_sent(Some(id));
                            } else {
                                warn!("Desktop notification returned invalid Notify reply");
                                on_sent(None);
                            }
                        }
                        Err(err) => {
                            warn!("Failed to send desktop notification: {err}");
                            on_sent(None);
                        }
                    }
                },
            );
        },
    );
}

pub fn close(id: u32) {
    gio::bus_get(
        gio::BusType::Session,
        None::<&gio::Cancellable>,
        move |result| {
            let connection = match result {
                Ok(connection) => connection,
                Err(err) => {
                    warn!("Failed to get session bus to close notification {id}: {err}");
                    return;
                }
            };

            let params = (id,).to_variant();
            connection.call(
                Some(NOTIFICATIONS_NAME),
                NOTIFICATIONS_PATH,
                NOTIFICATIONS_IFACE,
                "CloseNotification",
                Some(&params),
                None,
                gio::DBusCallFlags::NONE,
                DBUS_CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                move |result| {
                    if let Err(err) = result {
                        warn!("Failed to close desktop notification {id}: {err}");
                    }
                },
            );
        },
    );
}
