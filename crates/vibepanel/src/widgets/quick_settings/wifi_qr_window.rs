use std::cell::Cell;
use std::rc::Rc;

use gtk4::gdk::{self, Monitor};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, Image, Label, Orientation,
};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use qrcode::{Color as QrColor, QrCode};

use crate::services::background_effect::attach_blur_surface_lifecycle;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::network::{NetworkService, WifiAuthentication, WifiCredentials};
use crate::services::surfaces::SurfaceStyleManager;
use crate::styles::{button, color, qs, surface};
use crate::widgets::layer_shell_popover::{
    create_click_catcher, popover_keyboard_mode, setup_esc_handler,
};
use crate::widgets::rounded_picture::RoundedPicture;

const QR_MAX_IMAGE_SIZE: usize = 280;
const QR_QUIET_ZONE_MODULES: usize = 4;

pub struct WifiQrWindow {
    window: ApplicationWindow,
    backdrop: ApplicationWindow,
    _theme_callback_guard: ThemeCallbackGuard,
    password_controls: GtkBox,
    body: GtkBox,
    ssid_label: Label,
    generation: Cell<u64>,
}

impl WifiQrWindow {
    pub fn new(app: &Application) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Share Wi-Fi")
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class(qs::WIFI_QR_WINDOW);
        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-wifi-qr-popover"));
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);

        let card = GtkBox::new(Orientation::Vertical, 16);
        card.add_css_class(surface::POPOVER);
        card.add_css_class(surface::SURFACE_POPOVER);
        card.add_css_class(qs::WIFI_QR_CARD);
        card.set_halign(Align::Center);
        card.set_valign(Align::Center);
        card.set_size_request(400, -1);

        let header = GtkBox::new(Orientation::Horizontal, 8);
        let title = Label::new(Some("Share Wi-Fi"));
        title.add_css_class(surface::POPOVER_TITLE);
        title.add_css_class(color::PRIMARY);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        header.append(&title);

        let close = Button::new();
        close.add_css_class(surface::POPOVER_ICON_BTN);
        close.set_has_frame(false);
        close.set_tooltip_text(Some("Close"));
        close.set_child(Some(&Image::from_icon_name("window-close-symbolic")));
        header.append(&close);
        card.append(&header);

        let ssid_label = Label::new(None);
        ssid_label.add_css_class(color::PRIMARY);
        ssid_label.add_css_class(qs::WIFI_QR_SSID);
        ssid_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        ssid_label.set_max_width_chars(38);
        card.append(&ssid_label);

        let password_controls = GtkBox::new(Orientation::Vertical, 6);
        password_controls.set_halign(Align::Center);
        password_controls.set_visible(false);
        card.append(&password_controls);

        let body = GtkBox::new(Orientation::Vertical, 12);
        body.set_halign(Align::Center);
        card.append(&body);
        window.set_child(Some(&card));

        let theme_callback_guard = attach_blur_surface_lifecycle(
            &window,
            |win: &ApplicationWindow| win.child(),
            || ConfigManager::global().surface_border_radius() as i32,
        );

        let modal = Rc::new_cyclic(|modal_weak: &std::rc::Weak<Self>| {
            let modal_weak = modal_weak.clone();
            let backdrop = create_click_catcher(app, 0, move || {
                if let Some(modal) = modal_weak.upgrade() {
                    modal.hide();
                }
            });
            Self {
                window,
                backdrop,
                _theme_callback_guard: theme_callback_guard,
                password_controls,
                body,
                ssid_label,
                generation: Cell::new(0),
            }
        });

        {
            let modal_weak = Rc::downgrade(&modal);
            close.connect_clicked(move |_| {
                if let Some(modal) = modal_weak.upgrade() {
                    modal.hide();
                }
            });
        }
        {
            let modal_weak = Rc::downgrade(&modal);
            setup_esc_handler(&modal.window, move || {
                if let Some(modal) = modal_weak.upgrade() {
                    modal.hide();
                }
            });
        }

        SurfaceStyleManager::global().apply_pango_attrs_all(&card);

        modal
    }

    pub fn show(self: &Rc<Self>, ssid: &str, monitor: Option<&Monitor>) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.ssid_label.set_label(ssid);
        self.ssid_label.set_tooltip_text(Some(ssid));
        self.show_loading();
        self.window.set_monitor(monitor);
        self.backdrop.set_monitor(monitor);
        self.window.set_keyboard_mode(popover_keyboard_mode());
        self.backdrop.present();
        self.window.present();

        let expected_ssid = ssid.to_string();
        let modal_weak = Rc::downgrade(self);
        NetworkService::global().request_active_wifi_credentials(move |result| {
            if let Some(modal) = modal_weak.upgrade()
                && modal.generation.get() == generation
                && modal.window.is_visible()
            {
                modal.show_result(
                    result
                        .and_then(|credentials| credentials_for_ssid(credentials, &expected_ssid)),
                );
            }
        });
    }

    pub fn hide(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.set_visible(false);
        self.backdrop.set_visible(false);
        self.clear_content();
    }

    fn clear_content(&self) {
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
        while let Some(child) = self.password_controls.first_child() {
            self.password_controls.remove(&child);
        }
        self.password_controls.set_visible(false);
    }

    fn show_loading(&self) {
        self.clear_content();
        let label = Label::new(Some("Loading Wi-Fi credentials..."));
        label.add_css_class(color::MUTED);
        self.body.append(&label);
        SurfaceStyleManager::global().apply_pango_attrs_all(&self.body);
    }

    fn show_result(&self, result: Result<WifiCredentials, String>) {
        self.clear_content();
        match result.and_then(|credentials| {
            wifi_qr_texture(&credentials).map(|(texture, image_size, quiet_zone)| {
                let radius = ConfigManager::global()
                    .surface_border_radius()
                    .min(quiet_zone as u32);
                (
                    credentials.ssid,
                    credentials.password,
                    texture,
                    image_size,
                    radius,
                )
            })
        }) {
            Ok((ssid, password, texture, image_size, radius)) => {
                let picture = RoundedPicture::new();
                picture.set_pixel_size(image_size as i32);
                picture.set_corner_radius(radius as f32);
                picture.set_margin_bottom(52);
                picture.set_paintable(Some(&texture));
                picture.set_tooltip_text(Some(&format!("QR code for Wi-Fi network {ssid}")));
                self.body.append(&picture);

                if let Some(password) = password {
                    self.add_password_toggle(password);
                }
            }
            Err(message) => {
                let label = Label::new(Some(&message));
                label.add_css_class(color::ERROR);
                label.set_max_width_chars(42);
                label.set_wrap(true);
                label.set_justify(gtk4::Justification::Center);
                self.body.append(&label);
            }
        }
        SurfaceStyleManager::global().apply_pango_attrs_all(&self.body);
    }

    fn add_password_toggle(&self, password: String) {
        self.password_controls.set_visible(true);

        let reveal = Button::with_label("Show password");
        reveal.add_css_class(button::LINK);
        self.password_controls.append(&reveal);

        let password_label = Label::new(None);
        password_label.set_justify(gtk4::Justification::Center);
        password_label.set_selectable(true);
        password_label.set_visible(false);
        self.password_controls.append(&password_label);

        reveal.connect_clicked(move |button| {
            if password_label.is_visible() {
                password_label.set_label("");
                password_label.set_visible(false);
                button.set_label("Show password");
            } else {
                password_label.set_label(&password);
                password_label.set_visible(true);
                button.set_label("Hide password");
            }
        });

        SurfaceStyleManager::global().apply_pango_attrs_all(&self.password_controls);
    }
}

impl Drop for WifiQrWindow {
    fn drop(&mut self) {
        self.window.close();
        self.backdrop.close();
    }
}

fn credentials_for_ssid(
    credentials: WifiCredentials,
    expected_ssid: &str,
) -> Result<WifiCredentials, String> {
    if credentials.ssid == expected_ssid {
        Ok(credentials)
    } else {
        Err("Network changed".to_string())
    }
}

fn wifi_qr_payload(credentials: &WifiCredentials) -> String {
    let ssid = escape_wifi_qr_value(&credentials.ssid);
    let hidden = if credentials.hidden { "H:true;" } else { "" };
    let authentication = match credentials.authentication {
        WifiAuthentication::Open => return format!("WIFI:T:nopass;S:{ssid};{hidden};"),
        WifiAuthentication::Wpa => "WPA",
        WifiAuthentication::Sae => "SAE",
    };
    format!(
        "WIFI:T:{authentication};S:{ssid};P:{};{hidden};",
        escape_wifi_qr_value(credentials.password.as_deref().unwrap_or_default())
    )
}

fn escape_wifi_qr_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | ';' | ',' | '"' | ':') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn qr_layout(modules: usize) -> (usize, usize, usize) {
    let total_modules = modules + QR_QUIET_ZONE_MODULES * 2;
    let scale = QR_MAX_IMAGE_SIZE / total_modules;
    let offset = QR_QUIET_ZONE_MODULES * scale;
    (scale, offset, total_modules * scale)
}

fn wifi_qr_texture(credentials: &WifiCredentials) -> Result<(gdk::Texture, usize, usize), String> {
    let code = QrCode::new(wifi_qr_payload(credentials).as_bytes())
        .map_err(|e| format!("Failed to generate QR code: {e}"))?;
    let modules = code.width();
    let (scale, offset, image_size) = qr_layout(modules);
    let mut pixels = vec![255; image_size * image_size * 4];

    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color != QrColor::Dark {
            continue;
        }
        let module_x = index % modules;
        let module_y = index / modules;
        for y in offset + module_y * scale..offset + (module_y + 1) * scale {
            for x in offset + module_x * scale..offset + (module_x + 1) * scale {
                let pixel = (y * image_size + x) * 4;
                pixels[pixel..pixel + 3].fill(0);
            }
        }
    }

    let bytes = glib::Bytes::from_owned(pixels);
    let texture = gdk::MemoryTexture::new(
        image_size as i32,
        image_size as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        image_size * 4,
    )
    .upcast();
    Ok((texture, image_size, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_qr_payload_escapes_credentials_and_marks_hidden_networks() {
        assert_eq!(
            escape_wifi_qr_value(r#"a\b;c,d"e:f"#),
            r#"a\\b\;c\,d\"e\:f"#
        );
        assert_eq!(
            wifi_qr_payload(&WifiCredentials {
                ssid: "guest;wifi".to_string(),
                password: Some("a:b\\c".to_string()),
                hidden: true,
                authentication: WifiAuthentication::Wpa,
            }),
            r#"WIFI:T:WPA;S:guest\;wifi;P:a\:b\\c;H:true;;"#
        );
        assert_eq!(
            wifi_qr_payload(&WifiCredentials {
                ssid: "cafe".to_string(),
                password: None,
                hidden: false,
                authentication: WifiAuthentication::Open,
            }),
            "WIFI:T:nopass;S:cafe;;"
        );
        assert_eq!(
            wifi_qr_payload(&WifiCredentials {
                ssid: "wpa3".to_string(),
                password: Some("secret".to_string()),
                hidden: false,
                authentication: WifiAuthentication::Sae,
            }),
            "WIFI:T:SAE;S:wpa3;P:secret;;"
        );
    }

    #[test]
    fn rejects_credentials_from_changed_network() {
        let credentials = WifiCredentials {
            ssid: "new-network".to_string(),
            password: Some("secret".to_string()),
            hidden: false,
            authentication: WifiAuthentication::Wpa,
        };

        assert_eq!(
            credentials_for_ssid(credentials, "requested-network")
                .err()
                .as_deref(),
            Some("Network changed")
        );
    }

    #[test]
    fn rounded_qr_preserves_quiet_zone() {
        let credentials = WifiCredentials {
            ssid: "s".repeat(32),
            password: Some("p".repeat(63)),
            hidden: false,
            authentication: WifiAuthentication::Wpa,
        };
        let code = QrCode::new(wifi_qr_payload(&credentials).as_bytes()).unwrap();
        let total_modules = code.width() + QR_QUIET_ZONE_MODULES * 2;
        let (scale, offset, image_size) = qr_layout(code.width());

        assert_eq!(offset, QR_QUIET_ZONE_MODULES * scale);
        assert_eq!(image_size, total_modules * scale);
        assert!(image_size <= QR_MAX_IMAGE_SIZE);
        assert!((scale + 1) * total_modules > QR_MAX_IMAGE_SIZE);
        let (texture, texture_size, _) = wifi_qr_texture(&credentials).unwrap();
        assert_eq!(texture.width(), texture_size as i32);
        assert_eq!(texture.height(), texture_size as i32);
    }
}
