//! Media widget - displays current media playback status via MPRIS.
//!
//! Features:
//! - Compact bar display with album art thumbnail (or play/pause icon fallback)
//! - Hides completely when no MPRIS player is available
//! - Click opens a popover with full playback controls
//! - Pop-out button to open a standalone draggable window

use gtk4::Image;
use gtk4::gio;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, resolve_app_icon_name, set_image_from_app_id};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus};
use crate::services::state;
use crate::services::tooltip::TooltipManager;
use crate::styles::media;
use crate::widgets::base::{BaseWidget, MenuHandle};
use crate::widgets::marquee_label::MarqueeLabel;
use crate::widgets::media_components::{ArtState, load_art_from_url};
use crate::widgets::media_popover::{MediaPopoverController, build_media_popover_with_controller};
use crate::widgets::media_window::{MediaWindowHandle, create_media_window};
use crate::widgets::rounded_picture::RoundedPicture;
use crate::widgets::{WidgetConfig, warn_unknown_options};

// Thread-local global state for the popout window.
// This allows the popout to survive widget recreation during config reloads.
thread_local! {
    static POPOUT_HANDLE: RefCell<Option<MediaWindowHandle>> = const { RefCell::new(None) };
    // Reference to the current widget container, updated when widget is recreated.
    // Used by the popout close callback to restore visibility of the correct widget.
    static POPOUT_WIDGET_CONTAINER: RefCell<Option<gtk4::Box>> = const { RefCell::new(None) };
}

/// Default template: album art, then artist - title, then controls.
const DEFAULT_TEMPLATE: &str = "{art}{artist} - {title}{controls}";
const DEFAULT_MAX_CHARS: usize = 20;

/// Album art size as ratio of bar_size (0.75 = 24px art in 32px bar).
const ART_DISPLAY_SCALE: f64 = 0.75;

/// Configuration for the media widget.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// Template string for rendering.
    /// Widget tokens: {art}, {player_icon}, {icon}, {controls}
    /// Text tokens: {title}, {artist}, {album}
    pub template: String,
    /// Text to show when no player is available (empty = hide widget).
    pub empty_text: String,
    /// Maximum text length (0 = unlimited).
    pub max_chars: usize,
    /// Opacity for the pop-out window (0.0 = fully transparent, 1.0 = fully opaque).
    ///
    /// Note: This field is parsed for config validation but read dynamically from
    /// `ConfigManager::get_widget_option()` at runtime to support live-reload.
    #[allow(dead_code)]
    pub popout_opacity: f64,
}

impl WidgetConfig for MediaConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "media",
            entry,
            &["template", "empty_text", "max_chars", "popout_opacity"],
        );

        let template = entry
            .options
            .get("template")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| DEFAULT_TEMPLATE.to_string());

        let empty_text = entry
            .options
            .get("empty_text")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();

        let max_chars = entry
            .options
            .get("max_chars")
            .and_then(|v| v.as_integer())
            .map(|v| v.max(0) as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let popout_opacity = entry
            .options
            .get("popout_opacity")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(1.0);

        Self {
            template,
            empty_text,
            max_chars,
            popout_opacity,
        }
    }
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            empty_text: String::new(),
            max_chars: DEFAULT_MAX_CHARS,
            popout_opacity: 1.0,
        }
    }
}

// ArtState is imported from media_components

/// Widget tokens that create actual GTK widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidgetToken {
    Art,
    PlayerIcon,
    Icon,
    Controls,
}

impl WidgetToken {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "art" => Some(Self::Art),
            "player_icon" => Some(Self::PlayerIcon),
            "icon" => Some(Self::Icon),
            "controls" => Some(Self::Controls),
            _ => None,
        }
    }
}

/// Text tokens that get replaced with string values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextToken {
    Title,
    Artist,
    Album,
}

impl TextToken {
    fn value(self, snapshot: &MediaSnapshot) -> String {
        match self {
            Self::Title => snapshot.metadata.title.clone().unwrap_or_default(),
            Self::Artist => snapshot.metadata.artist.clone().unwrap_or_default(),
            Self::Album => snapshot.metadata.album.clone().unwrap_or_default(),
        }
    }
}

/// Parsed template element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateElement {
    Widget(WidgetToken),
    TextToken(TextToken),
    Literal(String),
}

/// Parse a template string into elements.
fn parse_template(template: &str) -> Vec<TemplateElement> {
    let mut elements = Vec::new();
    let mut current_literal = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '{' {
            current_literal.push(c);
            continue;
        }

        // Look for closing brace
        let mut token = String::new();
        let mut found_close = false;

        for tc in chars.by_ref() {
            if tc == '}' {
                found_close = true;
                break;
            }
            token.push(tc);
        }

        if !found_close {
            current_literal.push('{');
            current_literal.push_str(&token);
            continue;
        }

        if !current_literal.is_empty() {
            elements.push(TemplateElement::Literal(std::mem::take(
                &mut current_literal,
            )));
        }

        if let Some(widget_token) = WidgetToken::parse(&token) {
            elements.push(TemplateElement::Widget(widget_token));
            continue;
        }

        let text_token = match token.as_str() {
            "title" => Some(TextToken::Title),
            "artist" => Some(TextToken::Artist),
            "album" => Some(TextToken::Album),
            _ => None,
        };

        if let Some(text_token) = text_token {
            elements.push(TemplateElement::TextToken(text_token));
        } else {
            warn!(
                "Unknown template token '{{{}}}' in media widget template. \
                 Known tokens: {{art}}, {{player_icon}}, {{icon}}, {{controls}}, \
                 {{title}}, {{artist}}, {{album}}",
                token
            );
            elements.push(TemplateElement::Literal(format!("{{{}}}", token)));
        }
    }

    if !current_literal.is_empty() {
        elements.push(TemplateElement::Literal(current_literal));
    }

    elements
}

/// Render all non-widget template elements into a single string.
/// Literals (separators) are only included if both adjacent text tokens have values.
fn render_text_from_elements(elements: &[TemplateElement], snapshot: &MediaSnapshot) -> String {
    // First, resolve all token values
    let resolved: Vec<Option<String>> = elements
        .iter()
        .map(|el| match el {
            TemplateElement::Widget(_) => None,
            TemplateElement::TextToken(token) => {
                let val = token.value(snapshot);
                if val.is_empty() { None } else { Some(val) }
            }
            TemplateElement::Literal(s) => Some(s.clone()),
        })
        .collect();

    let mut result = String::new();

    for (idx, element) in elements.iter().enumerate() {
        match element {
            TemplateElement::Widget(_) => {}
            TemplateElement::TextToken(_) => {
                if let Some(ref val) = resolved[idx] {
                    result.push_str(val);
                }
            }
            TemplateElement::Literal(_) => {
                // Only include literal if there's a non-empty text token before AND after
                let has_content_before = resolved[..idx]
                    .iter()
                    .rev()
                    .find(|r| !matches!(r, Some(s) if is_literal_str(s)))
                    .is_some_and(|r| r.is_some());

                let has_content_after = resolved[idx + 1..]
                    .iter()
                    .find(|r| !matches!(r, Some(s) if is_literal_str(s)))
                    .is_some_and(|r| r.is_some());

                if has_content_before
                    && has_content_after
                    && let Some(ref val) = resolved[idx]
                {
                    result.push_str(val);
                }
            }
        }
    }

    result.trim().to_string()
}

/// Check if a string looks like a literal (whitespace and/or punctuation only).
fn is_literal_str(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_whitespace() || c.is_ascii_punctuation() || "–—‒•".contains(c))
}

fn has_text(element: &TemplateElement) -> bool {
    matches!(
        element,
        TemplateElement::TextToken(_) | TemplateElement::Literal(_)
    )
}

fn is_widget(element: &TemplateElement) -> bool {
    matches!(element, TemplateElement::Widget(_))
}

fn compute_text_runs(elements: &[TemplateElement]) -> Vec<std::ops::Range<usize>> {
    let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
    let mut current_start: Option<usize> = None;

    for (idx, element) in elements.iter().enumerate() {
        if has_text(element) {
            if current_start.is_none() {
                current_start = Some(idx);
            }
            continue;
        }

        if is_widget(element)
            && let Some(start) = current_start.take()
        {
            runs.push(start..idx);
        }
    }

    if let Some(start) = current_start {
        runs.push(start..elements.len());
    }

    runs
}

/// Check if the popout window is currently open and visible.
fn is_popout_open() -> bool {
    POPOUT_HANDLE.with(|h| {
        h.borrow()
            .as_ref()
            .is_some_and(|handle| handle.is_visible())
    })
}

/// Media widget that displays playback status and opens a popover on click.
pub struct MediaWidget {
    base: BaseWidget,
    media_callback_id: CallbackId,
    theme_callback_id: Option<CallbackId>,
}

#[derive(Clone)]
struct ControlsHandle {
    container: gtk4::Box,
    play_pause_icon: IconHandle,
}

/// Context holding references to all UI widgets for updates.
struct WidgetUpdateContext<'a> {
    container: &'a gtk4::Box,
    status_icon: &'a Option<IconHandle>,
    player_icon: &'a Option<Image>,
    art_picture: &'a Option<RoundedPicture>,
    text_labels: &'a Vec<Rc<MarqueeLabel>>,
    controls: &'a Option<ControlsHandle>,
    template_elements: &'a [TemplateElement],
    empty_text: &'a str,
    art_state: &'a Rc<RefCell<ArtState>>,
}

/// Owned version of widget references for use in callbacks.
#[derive(Clone)]
struct CallbackWidgetRefs {
    container: gtk4::Box,
    status_icon: Option<IconHandle>,
    player_icon: Option<Image>,
    art_picture: Option<RoundedPicture>,
    text_labels: Vec<Rc<MarqueeLabel>>,
    controls: Option<ControlsHandle>,
    template_elements: Vec<TemplateElement>,
    empty_text: String,
    art_state: Rc<RefCell<ArtState>>,
}

impl CallbackWidgetRefs {
    fn as_context(&self) -> WidgetUpdateContext<'_> {
        WidgetUpdateContext {
            container: &self.container,
            status_icon: &self.status_icon,
            player_icon: &self.player_icon,
            art_picture: &self.art_picture,
            text_labels: &self.text_labels,
            controls: &self.controls,
            template_elements: &self.template_elements,
            empty_text: &self.empty_text,
            art_state: &self.art_state,
        }
    }
}

fn create_controls(parent_widget: &gtk4::Box) -> ControlsHandle {
    use crate::services::icons::IconsService;
    use crate::services::tooltip::TooltipManager;
    use crate::styles::{button, icon};
    use crate::widgets::media_components::create_media_control_button;
    use gtk4::Button;

    let icons = IconsService::global();

    let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    container.add_css_class(media::CONTROLS);
    container.set_visible(false);

    // Add motion controller to manage tooltip behavior when hovering over controls.
    // - On enter: hide parent tooltip so button tooltips can show
    // - On leave: re-trigger parent tooltip
    let motion = gtk4::EventControllerMotion::new();
    motion.connect_enter(|_, _, _| {
        TooltipManager::global().cancel_and_hide();
    });
    let parent_for_leave = parent_widget.clone();
    motion.connect_leave(move |_| {
        TooltipManager::global().trigger_tooltip(&parent_for_leave);
    });
    container.add_controller(motion);

    let prev_btn = create_media_control_button(
        &icons,
        "skip_previous",
        "Previous",
        &[media::CONTROL_BTN, button::COMPACT],
        || MediaService::global().previous(),
    );
    container.append(&prev_btn);

    let play_pause_icon = icons.create_icon("play_arrow", &[icon::ICON]);
    let play_pause_btn = Button::new();
    play_pause_btn.set_has_frame(false);
    play_pause_btn.set_valign(gtk4::Align::Center);
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(media::CONTROL_BTN_PRIMARY);
    play_pause_btn.add_css_class(button::COMPACT);
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.connect_clicked(|_| {
        MediaService::global().play_pause();
    });
    container.append(&play_pause_btn);

    let next_btn = create_media_control_button(
        &icons,
        "skip_next",
        "Next",
        &[media::CONTROL_BTN, button::COMPACT],
        || MediaService::global().next(),
    );
    container.append(&next_btn);

    ControlsHandle {
        container,
        play_pause_icon,
    }
}

impl MediaWidget {
    pub fn new(config: MediaConfig) -> Self {
        let base = BaseWidget::new(&[media::WIDGET]);
        base.set_tooltip("No media playing");

        let template_elements = parse_template(&config.template);

        let mut art_picture: Option<RoundedPicture> = None;
        let mut player_icon: Option<Image> = None;
        let mut status_icon: Option<IconHandle> = None;
        let mut controls: Option<ControlsHandle> = None;
        let mut text_labels: Vec<Rc<MarqueeLabel>> = Vec::new();

        if template_elements
            .iter()
            .any(|e| matches!(e, TemplateElement::Widget(WidgetToken::Art)))
        {
            let config_mgr = ConfigManager::global();
            let art_size = (config_mgr.bar_size() as f64 * ART_DISPLAY_SCALE) as i32;
            let radius_percent = (config_mgr.widget_radius_percent() as f32 / 100.0).min(0.5);
            let corner_radius = art_size as f32 * radius_percent;

            let picture = RoundedPicture::new();
            picture.set_pixel_size(art_size);
            picture.set_corner_radius(corner_radius);
            picture.add_css_class(media::ART_SMALL);
            picture.set_visible(false);
            art_picture = Some(picture);
        }

        if template_elements
            .iter()
            .any(|e| matches!(e, TemplateElement::Widget(WidgetToken::PlayerIcon)))
        {
            let image = Image::from_icon_name(media::ICON_AUDIO_GENERIC);
            image.add_css_class(media::PLAYER_ICON);
            image.set_visible(false);
            player_icon = Some(image);
        }

        if template_elements
            .iter()
            .any(|e| matches!(e, TemplateElement::Widget(WidgetToken::Icon)))
        {
            let handle = base.add_icon(media::ICON_PAUSE, &[media::ICON]);
            handle.widget().set_visible(false);
            status_icon = Some(handle);
        }

        if template_elements
            .iter()
            .any(|e| matches!(e, TemplateElement::Widget(WidgetToken::Controls)))
        {
            controls = Some(create_controls(base.widget()));
        }

        let text_runs = compute_text_runs(&template_elements);

        for _ in &text_runs {
            let marquee = Rc::new(MarqueeLabel::new());
            marquee.label().add_css_class(media::LABEL);
            if config.max_chars > 0 {
                marquee.set_max_width_chars(config.max_chars as i32);
            }
            marquee.set_visible(false);
            text_labels.push(marquee);
        }

        let mut current_text_run_idx: usize = 0;
        let mut pending_text_run = true;

        for element in &template_elements {
            match element {
                TemplateElement::TextToken(_) | TemplateElement::Literal(_) => {
                    if pending_text_run {
                        if let Some(marquee) = text_labels.get(current_text_run_idx) {
                            base.content().append(marquee.widget());
                        }
                        pending_text_run = false;
                    }
                }
                TemplateElement::Widget(token) => {
                    // Any widget token ends the current text run.
                    if !pending_text_run {
                        current_text_run_idx += 1;
                        pending_text_run = true;
                    }

                    match token {
                        WidgetToken::Controls => {
                            if let Some(ctrl) = &controls {
                                base.content().append(&ctrl.container);
                            }
                        }
                        WidgetToken::Art => {
                            if let Some(picture) = &art_picture {
                                base.content().append(picture);
                            }
                        }
                        WidgetToken::PlayerIcon => {
                            if let Some(image) = &player_icon {
                                base.content().append(image);
                            }
                        }
                        WidgetToken::Icon => {
                            if let Some(icon) = &status_icon {
                                base.content().append(&icon.widget());
                            }
                        }
                    }
                }
            }
        }

        // Shared controller storage between the widget and the menu builder.
        let controller_cell: Rc<RefCell<Option<MediaPopoverController>>> =
            Rc::new(RefCell::new(None));
        let controller_for_builder = controller_cell.clone();

        // Check if a popout window is already open (from a previous widget instance).
        // This handles config reloads where the popout should survive.
        let popout_already_open = is_popout_open();

        // Register this widget's container globally so the popout close callback
        // can restore visibility even after widget recreation.
        POPOUT_WIDGET_CONTAINER.with(|c| {
            *c.borrow_mut() = Some(base.widget().clone());
        });

        // If popout is already open, hide the bar widget and update opacity from current config.
        if popout_already_open {
            base.widget().set_visible(false);

            // Update opacity on existing window
            let popout_opacity = ConfigManager::global()
                .get_widget_option("media", "popout_opacity")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|v| v.clamp(0.0, 1.0))
                .unwrap_or(1.0);

            POPOUT_HANDLE.with(|h| {
                if let Some(handle) = h.borrow().as_ref() {
                    handle.set_opacity(popout_opacity);
                }
            });

            debug!(
                "Media widget recreated, updated popout opacity to {}",
                popout_opacity
            );
        }

        // We need access to the menu handle to close the popover when popping out.
        // Use the same pattern as notifications: store it after create_menu returns.
        let menu_handle_cell: Rc<RefCell<Option<Rc<MenuHandle>>>> = Rc::new(RefCell::new(None));
        let menu_handle_for_builder = menu_handle_cell.clone();

        // Create the on_popout callback that will be called when the pop-out button is clicked.
        let on_popout = move || {
            // Hide any visible tooltip to prevent orphaned tooltips
            TooltipManager::global().cancel_and_hide();

            // Close the popover first
            if let Some(menu_handle) = menu_handle_for_builder.borrow().as_ref() {
                menu_handle.hide();
            }

            // If window already exists and is visible, just focus it
            if is_popout_open() {
                POPOUT_HANDLE.with(|h| {
                    if let Some(handle) = h.borrow().as_ref() {
                        handle.show(); // Brings to front
                    }
                });
                return;
            }

            // Hide the bar widget
            POPOUT_WIDGET_CONTAINER.with(|c| {
                if let Some(container) = c.borrow().as_ref() {
                    container.set_visible(false);
                }
            });

            // Create the on_close callback for when the window closes.
            // This uses the global POPOUT_WIDGET_CONTAINER to restore the correct
            // widget even if it was recreated during a config reload.
            let on_close = move || {
                // Clear the global handle
                POPOUT_HANDLE.with(|h| {
                    *h.borrow_mut() = None;
                });

                // Show the current widget container (may be different from original)
                POPOUT_WIDGET_CONTAINER.with(|c| {
                    if let Some(container) = c.borrow().as_ref() {
                        container.set_visible(true);
                    }
                });

                let mut persisted = state::load();
                persisted.media.window_open = false;
                state::save(&persisted);

                debug!("Media window closed, bar widget restored");
            };

            // Read opacity from current config for live-reload support
            let popout_opacity = ConfigManager::global()
                .get_widget_option("media", "popout_opacity")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|v| v.clamp(0.0, 1.0))
                .unwrap_or(1.0);

            let handle = create_media_window(None, popout_opacity, on_close);
            handle.show();

            // Store in global state
            POPOUT_HANDLE.with(|h| {
                *h.borrow_mut() = Some(handle);
            });

            let mut persisted = state::load();
            persisted.media.window_open = true;
            state::save(&persisted);

            debug!("Media window opened, bar widget hidden");
        };

        let menu_handle = base.create_menu("media", move || {
            let on_popout_clone = on_popout.clone();
            let (widget, controller) = build_media_popover_with_controller(move || {
                on_popout_clone();
            });
            *controller_for_builder.borrow_mut() = Some(controller);
            widget
        });

        *menu_handle_cell.borrow_mut() = Some(menu_handle);

        // Reset persisted state on startup (actual popout state is tracked in POPOUT_HANDLE)
        let mut persisted = state::load();
        if persisted.media.window_open && !popout_already_open {
            persisted.media.window_open = false;
            state::save(&persisted);
        }

        let media_service = MediaService::global();
        let template_elements = template_elements.clone();
        let art_state = Rc::new(RefCell::new(ArtState::default()));

        let widget_refs = CallbackWidgetRefs {
            container: base.widget().clone(),
            status_icon: status_icon.clone(),
            player_icon: player_icon.clone(),
            art_picture: art_picture.clone(),
            text_labels,
            controls: controls.clone(),
            template_elements,
            empty_text: config.empty_text.clone(),
            art_state: art_state.clone(),
        };

        update_widgets_from_snapshot_impl(&widget_refs.as_context(), &MediaSnapshot::empty());

        let controller_for_cb = controller_cell.clone();
        let media_callback_id = media_service.connect(move |snapshot: &MediaSnapshot| {
            update_widgets_from_snapshot_impl(&widget_refs.as_context(), snapshot);

            if let Some(controller) = controller_for_cb.borrow().as_ref() {
                controller.update_from_snapshot(snapshot);
            }
        });

        // Subscribe to theme changes to update album art corner radius
        let theme_callback_id = if let Some(picture) = art_picture {
            let picture_for_theme = picture.clone();
            Some(ConfigManager::global().on_theme_change(move || {
                let config_mgr = ConfigManager::global();
                let art_size = (config_mgr.bar_size() as f64 * ART_DISPLAY_SCALE) as i32;
                let radius_percent = (config_mgr.widget_radius_percent() as f32 / 100.0).min(0.5);
                let corner_radius = art_size as f32 * radius_percent;
                picture_for_theme.set_corner_radius(corner_radius);
            }))
        } else {
            None
        };

        Self {
            base,
            media_callback_id,
            theme_callback_id,
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for MediaWidget {
    fn drop(&mut self) {
        MediaService::global().disconnect(self.media_callback_id);
        if let Some(id) = self.theme_callback_id {
            ConfigManager::global().disconnect_theme_callback(id);
        }
    }
}

/// Update widget state from a media snapshot.
fn update_widgets_from_snapshot_impl(ctx: &WidgetUpdateContext<'_>, snapshot: &MediaSnapshot) {
    let has_metadata = snapshot
        .metadata
        .title
        .as_ref()
        .is_some_and(|t| !t.trim().is_empty());
    let should_hide = !snapshot.available
        || (snapshot.playback_status == PlaybackStatus::Stopped && !has_metadata);

    if should_hide {
        debug!(
            "media widget hidden: available={}, status={:?}, has_metadata={}, empty_text='{}'",
            snapshot.available, snapshot.playback_status, has_metadata, ctx.empty_text
        );

        // Don't change visibility if popped out - the pop-out window handles display
        if is_popout_open() {
            return;
        }

        if ctx.empty_text.is_empty() {
            ctx.container.set_visible(false);
        } else {
            ctx.container.set_visible(true);
            for marquee in ctx.text_labels {
                marquee.set_text("");
                marquee.set_visible(false);
            }
            if let Some(first) = ctx.text_labels.first() {
                first.set_text(ctx.empty_text);
                first.set_visible(true);
            }
            if let Some(icon) = ctx.status_icon {
                icon.widget().set_visible(false);
            }
            if let Some(image) = ctx.player_icon {
                image.set_visible(false);
            }
            if let Some(image) = ctx.art_picture {
                image.set_visible(false);
            }
            if let Some(ctrl) = ctx.controls {
                ctrl.container.set_visible(false);
            }
            ctx.container.remove_css_class(media::PLAYING);
            ctx.container.remove_css_class(media::PAUSED);
            ctx.container.add_css_class(media::STOPPED);

            let tooltip_manager = TooltipManager::global();
            tooltip_manager.set_styled_tooltip(ctx.container, "No media playing");
        }
        return;
    }

    debug!(
        "media widget visible: player={:?} status={:?} title={:?} artist={:?}",
        snapshot.player_name,
        snapshot.playback_status,
        snapshot.metadata.title,
        snapshot.metadata.artist,
    );

    if !is_popout_open() {
        ctx.container.set_visible(true);
    }

    ctx.container.remove_css_class(media::PLAYING);
    ctx.container.remove_css_class(media::PAUSED);
    ctx.container.remove_css_class(media::STOPPED);

    match snapshot.playback_status {
        PlaybackStatus::Playing => {
            ctx.container.add_css_class(media::PLAYING);
        }
        PlaybackStatus::Paused => {
            ctx.container.add_css_class(media::PAUSED);
        }
        PlaybackStatus::Stopped => {
            ctx.container.add_css_class(media::STOPPED);
        }
    }

    if let Some(icon) = ctx.status_icon {
        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => media::ICON_PAUSE,
            PlaybackStatus::Paused | PlaybackStatus::Stopped => media::ICON_PLAY,
        };
        icon.set_icon(icon_name);
        icon.widget().set_visible(true);
    }

    if let Some(ctrl) = ctx.controls {
        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => "pause",
            PlaybackStatus::Paused | PlaybackStatus::Stopped => "play_arrow",
        };
        ctrl.play_pause_icon.set_icon(icon_name);
        ctrl.container.set_visible(true);
    }

    if let Some(image) = ctx.player_icon {
        if let Some(player_id) = &snapshot.player_id {
            set_image_from_app_id(image, player_id);
            image.set_visible(true);
        } else {
            image.set_icon_name(Some(media::ICON_AUDIO_GENERIC));
            image.set_visible(true);
        }
    }

    if let Some(picture) = ctx.art_picture {
        let art_url = snapshot.metadata.art_url.as_deref();

        let load_info = {
            let mut state = ctx.art_state.borrow_mut();
            if state.current_url.as_deref() == art_url {
                None
            } else {
                state.cancellable.cancel();
                state.current_url = art_url.map(String::from);
                state.generation += 1;
                state.cancellable = gio::Cancellable::new();
                Some((state.generation, state.cancellable.clone()))
            }
        };

        if let Some((generation, cancellable)) = load_info {
            let art_state = ctx.art_state.clone();
            let player_id = snapshot.player_id.clone();
            let picture_clone = picture.clone();

            // Bar widget doesn't have a placeholder - on_success is a no-op
            let on_success = || {};

            let on_failure = move || {
                show_player_icon_in_art(
                    &picture_clone,
                    player_id.as_deref(),
                    &art_state,
                    generation,
                );
            };

            if let Some(url) = art_url {
                load_art_from_url(
                    url,
                    picture.clone(),
                    ctx.art_state,
                    generation,
                    &cancellable,
                    on_success,
                    on_failure,
                );
            } else {
                on_failure();
            }
        }
    }

    if !ctx.text_labels.is_empty() {
        let runs = compute_text_runs(ctx.template_elements);
        debug!("media widget template runs={}", runs.len());

        for (run_idx, element_range) in runs.iter().cloned().enumerate() {
            if let Some(marquee) = ctx.text_labels.get(run_idx) {
                let text =
                    render_text_from_elements(&ctx.template_elements[element_range], snapshot);
                if text.is_empty() {
                    marquee.set_text("");
                    marquee.set_visible(false);
                } else {
                    marquee.set_text(&text);
                    marquee.set_visible(true);
                }
            }
        }

        for idx in runs.len()..ctx.text_labels.len() {
            if let Some(marquee) = ctx.text_labels.get(idx) {
                marquee.set_visible(false);
            }
        }
    }

    let tooltip = build_tooltip(snapshot);
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(ctx.container, &tooltip);
}

/// Show the player's app icon as fallback for album art.
fn show_player_icon_in_art(
    art_picture: &RoundedPicture,
    player_id: Option<&str>,
    art_state: &Rc<RefCell<ArtState>>,
    generation: u64,
) {
    if art_state.borrow().generation != generation {
        return;
    }

    let icon_name = player_id
        .map(|id| resolve_app_icon_name(id, media::ICON_AUDIO_GENERIC))
        .unwrap_or_else(|| media::ICON_AUDIO_GENERIC.to_string());

    let Some(display) = gtk4::gdk::Display::default() else {
        warn!("No display available for icon lookup");
        art_picture.set_visible(false);
        return;
    };
    let icon_theme = gtk4::IconTheme::for_display(&display);

    let config = ConfigManager::global();
    let art_size = (config.bar_size() as f64 * ART_DISPLAY_SCALE) as i32;

    let paintable = icon_theme.lookup_icon(
        &icon_name,
        &[],
        art_size,
        1,
        gtk4::TextDirection::None,
        gtk4::IconLookupFlags::empty(),
    );

    art_picture.set_paintable(Some(&paintable));
    art_picture.set_visible(true);
}

fn build_tooltip(snapshot: &MediaSnapshot) -> String {
    if !snapshot.available {
        return "No media playing".to_string();
    }

    let mut lines = Vec::new();

    if let Some(name) = &snapshot.player_name {
        lines.push(format!("Player: {}", name));
    }

    if let Some(title) = &snapshot.metadata.title {
        lines.push(format!("Title: {}", title));
    }
    if let Some(artist) = &snapshot.metadata.artist {
        lines.push(format!("Artist: {}", artist));
    }
    if let Some(album) = &snapshot.metadata.album {
        lines.push(format!("Album: {}", album));
    }

    let status = match snapshot.playback_status {
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Stopped => "Stopped",
    };
    lines.push(format!("Status: {}", status));

    if lines.is_empty() {
        "Media".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::media::MediaMetadata;

    #[test]
    fn test_media_config_defaults() {
        let entry = WidgetEntry {
            name: "media".to_string(),
            options: Default::default(),
        };
        let config = MediaConfig::from_entry(&entry);
        assert_eq!(config.template, "{art}{artist} - {title}{controls}");
        assert_eq!(config.empty_text, "");
        assert_eq!(config.max_chars, 20);
    }

    #[test]
    fn test_build_tooltip_empty() {
        let snapshot = MediaSnapshot::empty();
        assert_eq!(build_tooltip(&snapshot), "No media playing");
    }

    #[test]
    fn test_build_tooltip_with_track() {
        let snapshot = MediaSnapshot {
            available: true,
            player_name: Some("Spotify".to_string()),
            metadata: MediaMetadata {
                title: Some("Test Song".to_string()),
                artist: Some("Test Artist".to_string()),
                ..Default::default()
            },
            playback_status: PlaybackStatus::Playing,
            ..Default::default()
        };

        let tooltip = build_tooltip(&snapshot);
        assert!(tooltip.contains("Player: Spotify"));
        assert!(tooltip.contains("Title: Test Song"));
        assert!(tooltip.contains("Artist: Test Artist"));
        assert!(tooltip.contains("Status: Playing"));
    }

    #[test]
    fn test_parse_template_widget_tokens() {
        let elements = parse_template("{art}{icon}{player_icon}");
        assert_eq!(elements.len(), 3);
        assert!(matches!(
            elements[0],
            TemplateElement::Widget(WidgetToken::Art)
        ));
        assert!(matches!(
            elements[1],
            TemplateElement::Widget(WidgetToken::Icon)
        ));
        assert!(matches!(
            elements[2],
            TemplateElement::Widget(WidgetToken::PlayerIcon)
        ));
    }

    #[test]
    fn test_parse_template_text_tokens() {
        let elements = parse_template("{title} - {artist}");
        assert_eq!(elements.len(), 3);
        assert!(matches!(
            elements[0],
            TemplateElement::TextToken(TextToken::Title)
        ));
        assert!(matches!(
            &elements[1],
            TemplateElement::Literal(s) if s == " - "
        ));
        assert!(matches!(
            elements[2],
            TemplateElement::TextToken(TextToken::Artist)
        ));
    }

    #[test]
    fn test_parse_template_mixed() {
        let elements = parse_template("{art}{title} - {artist}");
        assert_eq!(elements.len(), 4);
        assert!(matches!(
            elements[0],
            TemplateElement::Widget(WidgetToken::Art)
        ));
        assert!(matches!(
            elements[1],
            TemplateElement::TextToken(TextToken::Title)
        ));
        assert!(matches!(
            &elements[2],
            TemplateElement::Literal(s) if s == " - "
        ));
        assert!(matches!(
            elements[3],
            TemplateElement::TextToken(TextToken::Artist)
        ));
    }

    #[test]
    fn test_compute_text_runs_controls_between_text() {
        let elements = parse_template("{artist}{controls}{title}");
        let runs = compute_text_runs(&elements);
        assert_eq!(runs.len(), 2);

        assert_eq!(
            elements[runs[0].clone()],
            [TemplateElement::TextToken(TextToken::Artist)]
        );
        assert_eq!(
            elements[runs[1].clone()],
            [TemplateElement::TextToken(TextToken::Title)]
        );
    }

    #[test]
    fn test_compute_text_runs_inline_widget_between_text() {
        let elements = parse_template("{controls}{artist} {art}{title}");
        let runs = compute_text_runs(&elements);
        assert_eq!(runs.len(), 2);

        assert_eq!(
            elements[runs[0].clone()],
            [
                TemplateElement::TextToken(TextToken::Artist),
                TemplateElement::Literal(" ".to_string())
            ]
        );
        assert_eq!(
            elements[runs[1].clone()],
            [TemplateElement::TextToken(TextToken::Title)]
        );
    }

    #[test]
    fn test_render_text_from_elements() {
        let mut snapshot = MediaSnapshot::default();
        snapshot.metadata.title = Some("Test Song".to_string());
        snapshot.metadata.artist = Some("Test Artist".to_string());

        let elements = parse_template("{artist} - {title}");
        let result = render_text_from_elements(&elements, &snapshot);
        assert_eq!(result, "Test Artist - Test Song");

        snapshot.metadata.album = Some("Test Album".to_string());
        let elements = parse_template("{album}: {title}");
        let result = render_text_from_elements(&elements, &snapshot);
        assert_eq!(result, "Test Album: Test Song");
    }

    #[test]
    fn test_render_text_from_elements_missing() {
        let snapshot = MediaSnapshot::default();

        // Both missing - separator should be omitted
        let elements = parse_template("{artist} - {title}");
        let result = render_text_from_elements(&elements, &snapshot);
        assert_eq!(result, "");

        // Only title present - separator should be omitted
        let mut snapshot_title = MediaSnapshot::default();
        snapshot_title.metadata.title = Some("Song".to_string());
        let result = render_text_from_elements(&elements, &snapshot_title);
        assert_eq!(result, "Song");

        // Only artist present - separator should be omitted
        let mut snapshot_artist = MediaSnapshot::default();
        snapshot_artist.metadata.artist = Some("Artist".to_string());
        let result = render_text_from_elements(&elements, &snapshot_artist);
        assert_eq!(result, "Artist");
    }

    #[test]
    fn test_widget_token_parse() {
        assert_eq!(WidgetToken::parse("art"), Some(WidgetToken::Art));
        assert_eq!(WidgetToken::parse("icon"), Some(WidgetToken::Icon));
        assert_eq!(
            WidgetToken::parse("player_icon"),
            Some(WidgetToken::PlayerIcon)
        );
        assert_eq!(WidgetToken::parse("title"), None);
        assert_eq!(WidgetToken::parse("unknown"), None);
    }

    #[test]
    fn test_parse_template_literal_and_tokens() {
        let elements = parse_template("{art}{artist}{icon} - {title}");
        assert!(matches!(
            elements[0],
            TemplateElement::Widget(WidgetToken::Art)
        ));
        assert!(matches!(
            elements[1],
            TemplateElement::TextToken(TextToken::Artist)
        ));
        assert!(matches!(
            elements[2],
            TemplateElement::Widget(WidgetToken::Icon)
        ));
        assert!(matches!(
            &elements[3],
            TemplateElement::Literal(s) if s == " - "
        ));
        assert!(matches!(
            elements[4],
            TemplateElement::TextToken(TextToken::Title)
        ));
    }
}
