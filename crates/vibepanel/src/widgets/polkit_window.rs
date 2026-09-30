//! Centered polkit authentication prompt.
//!
//! Renders [`AuthView`] snapshots from [`PolkitAgent`]. No backdrop and no
//! click-outside dismissal: a stray click must not cancel a password prompt.

use std::cell::RefCell;
use std::ffi::CStr;
use std::rc::Rc;

use gtk4::glib::translate::ToGlibPtr;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Entry, Label, Orientation, PasswordEntry,
    PasswordEntryBuffer,
};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};

use crate::services::background_effect::attach_blur_surface_lifecycle;
use crate::services::config_manager::{ConfigManager, ThemeCallbackGuard};
use crate::services::icons::{IconHandle, IconsService};
use crate::services::polkit_agent::{AuthView, PolkitAgent};
use crate::services::surfaces::{SHADOW_MARGIN, SurfaceStyleManager};
use crate::styles::{button, color, polkit, surface};
use crate::widgets::base::vp_button_with_label;
use crate::widgets::layer_shell_popover::setup_esc_handler;

pub fn install(app: &Application) {
    let app = app.clone();
    let window: RefCell<Option<Rc<PolkitWindow>>> = RefCell::new(None);
    PolkitAgent::global().set_view_handler(move |view| {
        if view.is_none() && window.borrow().is_none() {
            return;
        }
        let window = window
            .borrow_mut()
            .get_or_insert_with(|| PolkitWindow::new(&app))
            .clone();
        window.render(view);
    });
}

struct PolkitWindow {
    window: ApplicationWindow,
    _theme_callback_guard: ThemeCallbackGuard,
    _icon: IconHandle,
    card: GtkBox,
    message: Label,
    user: Label,
    entry: PasswordEntry,
    echo_entry: Entry,
    status: Label,
    info: Label,
    authenticate: gtk4::Button,
}

impl PolkitWindow {
    fn new(app: &Application) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Authentication required")
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class(surface::LAYER_SHELL_POPOVER);
        window.init_layer_shell();
        window.set_namespace(Some("vibepanel-polkit"));
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);

        let card = GtkBox::new(Orientation::Vertical, 20);
        card.add_css_class(surface::POPOVER);
        card.add_css_class(surface::SURFACE_POPOVER);
        card.add_css_class(polkit::POPOVER);
        card.set_size_request(440, -1);

        let header = GtkBox::new(Orientation::Horizontal, 16);
        let badge = GtkBox::new(Orientation::Vertical, 0);
        badge.add_css_class(polkit::BADGE);
        badge.set_valign(Align::Center);
        let icon = IconsService::global().create_icon("system-lock-screen-symbolic", &[]);
        let icon_widget = icon.widget();
        icon_widget.set_halign(Align::Center);
        icon_widget.set_valign(Align::Center);
        icon_widget.set_vexpand(true);
        badge.append(&icon_widget);
        header.append(&badge);

        let text = GtkBox::new(Orientation::Vertical, 4);
        text.set_hexpand(true);
        text.set_valign(Align::Center);
        let title = Label::new(Some("Authentication required"));
        title.add_css_class(polkit::TITLE);
        title.add_css_class(color::PRIMARY);
        title.set_xalign(0.0);
        text.append(&title);

        let message = Label::new(None);
        message.add_css_class(color::MUTED);
        message.add_css_class(polkit::MESSAGE);
        message.set_xalign(0.0);
        message.set_wrap(true);
        message.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        message.set_max_width_chars(1);
        text.append(&message);

        let user = Label::new(None);
        user.add_css_class(color::MUTED);
        user.add_css_class(polkit::USER);
        user.set_xalign(0.0);
        text.append(&user);
        header.append(&text);
        card.append(&header);

        let fields = GtkBox::new(Orientation::Vertical, 8);
        // PasswordEntry keeps secrets in non-pageable memory, wiped on free.
        let entry = PasswordEntry::new();
        entry.set_show_peek_icon(true);
        // Undo history would copy a revealed password into unwiped memory.
        entry.set_enable_undo(false);
        fields.append(&entry);
        let echo_entry = Entry::new();
        fields.append(&echo_entry);

        let status = Label::new(None);
        status.add_css_class(color::ERROR);
        status.add_css_class(polkit::ERROR);
        let info = Label::new(None);
        info.add_css_class(color::MUTED);
        info.add_css_class(polkit::INFO);
        for label in [&status, &info] {
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
            label.set_max_width_chars(1);
            fields.append(label);
        }
        card.append(&fields);

        let buttons = GtkBox::new(Orientation::Horizontal, 8);
        buttons.add_css_class(polkit::ACTIONS);
        buttons.set_halign(Align::End);
        let cancel = vp_button_with_label("Cancel");
        cancel.add_css_class(button::CARD);
        let authenticate = vp_button_with_label("Authenticate");
        authenticate.add_css_class(button::ACCENT);
        buttons.append(&cancel);
        buttons.append(&authenticate);
        card.append(&buttons);

        let wrapper = GtkBox::new(Orientation::Vertical, 0);
        wrapper.add_css_class(surface::POPOVER_WRAPPER);
        wrapper.set_margin_top(SHADOW_MARGIN);
        wrapper.set_margin_bottom(SHADOW_MARGIN);
        wrapper.set_margin_start(SHADOW_MARGIN);
        wrapper.set_margin_end(SHADOW_MARGIN);
        wrapper.append(&card);
        window.set_child(Some(&wrapper));

        let card_for_blur = card.clone();
        let theme_callback_guard = attach_blur_surface_lifecycle(
            &window,
            move |_: &ApplicationWindow| Some(card_for_blur.clone().upcast()),
            || ConfigManager::global().surface_border_radius() as i32,
        );

        let this = Rc::new(Self {
            window,
            _theme_callback_guard: theme_callback_guard,
            _icon: icon,
            card,
            message,
            user,
            entry,
            echo_entry,
            status,
            info,
            authenticate,
        });

        cancel.connect_clicked(|_| PolkitAgent::global().cancel());
        // Older gtk4-layer-shell closes the window when its output goes away;
        // keep it for reuse and answer the request.
        this.window.connect_close_request(|_| {
            PolkitAgent::global().cancel();
            gtk4::glib::Propagation::Stop
        });
        setup_esc_handler(&this.window, || PolkitAgent::global().cancel());
        let weak = Rc::downgrade(&this);
        this.authenticate.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.submit();
            }
        });
        let weak = Rc::downgrade(&this);
        this.entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.submit();
            }
        });
        let weak = Rc::downgrade(&this);
        this.echo_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.submit();
            }
        });

        this
    }

    fn active_entry(&self) -> &gtk4::Editable {
        if WidgetExt::is_visible(&self.echo_entry) {
            self.echo_entry.upcast_ref()
        } else {
            self.entry.upcast_ref()
        }
    }

    fn submit(&self) {
        let editable = self.active_entry();
        // SAFETY: the string is owned by the entry buffer and valid until the
        // text or buffer changes; respond() hands it to PAM before any UI code
        // runs, and the buffer is only replaced after it returns.
        let text =
            unsafe { CStr::from_ptr(gtk4::ffi::gtk_editable_get_text(editable.to_glib_none().0)) };
        PolkitAgent::global().respond(text);
        self.reset_entries();
    }

    /// Swaps the buffer: `set_text("")` leaves the old bytes, only freeing wipes them.
    fn reset_entries(&self) {
        if let Some(text) = self.entry.delegate().and_downcast::<gtk4::Text>() {
            text.set_buffer(&PasswordEntryBuffer::new());
            text.set_visibility(false);
        }
        self.echo_entry.set_text("");
    }

    fn render(&self, view: Option<&AuthView>) {
        let Some(view) = view else {
            self.reset_entries();
            self.window.set_keyboard_mode(KeyboardMode::None);
            self.window.set_visible(false);
            return;
        };

        self.message.set_label(&view.message);
        self.user
            .set_label(&format!("Authenticating as {}", view.user));
        self.user.set_visible(view.other_user);

        let prompt = view.prompt.as_ref();
        if prompt.is_none() {
            self.reset_entries();
        }
        let echo = prompt.is_some_and(|p| p.echo);
        self.entry.set_visible(!echo);
        self.echo_entry.set_visible(echo);
        if let Some(prompt) = prompt {
            self.entry.set_placeholder_text(Some(&prompt.label));
            self.echo_entry.set_placeholder_text(Some(&prompt.label));
        }
        let ready = prompt.is_some() && !view.busy;
        let was_ready = self.authenticate.is_sensitive();
        self.entry.set_sensitive(ready);
        self.echo_entry.set_sensitive(ready);
        self.authenticate.set_sensitive(ready);

        for (label, text) in [(&self.status, &view.error), (&self.info, &view.info)] {
            label.set_label(text.as_deref().unwrap_or(""));
            // While busy, keep an already-shown label (now blank) so the card
            // doesn't shrink on submit and regrow when the retry error arrives.
            label.set_visible(text.is_some() || (view.busy && label.is_visible()));
        }

        let styles = SurfaceStyleManager::global();
        styles.apply_pango_attrs_all(&self.card);
        if !self.window.is_visible() {
            // Exclusive even on Hyprland (unlike popover_keyboard_mode()):
            // blocking other surfaces is fine for a modal prompt.
            crate::popover_tracker::PopoverTracker::global().dismiss_active();
            self.window.set_keyboard_mode(KeyboardMode::Exclusive);
            self.window.present();
        }
        // Only on becoming ready: re-grabbing selects all typed text.
        if ready && !was_ready {
            self.active_entry().grab_focus();
        }
    }
}
