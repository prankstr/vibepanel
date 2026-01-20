//! Media widget - displays current media playback status via the shared
//! `MediaService` (MPRIS D-Bus backed).
//!
//! The MediaService is responsible for D-Bus/MPRIS integration and
//! exposes canonical snapshots; this widget subscribes to those
//! snapshots and renders icon/text/CSS/tooltip accordingly.
//!
//! Features:
//! - Compact bar display with album art thumbnail (or play/pause icon fallback)
//! - Hides completely when no MPRIS player is available
//! - Click opens a popover with full playback controls
//! - Pop-out button to open a standalone draggable window
//!
//! Uses:
//! - `IconsService` (via BaseWidget) for themed media icons
//! - `TooltipManager` for styled tooltips

use gtk4::Image;
use gtk4::gdk::Texture;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use tracing::{debug, warn};
use vibepanel_core::config::WidgetEntry;

use crate::services::callbacks::CallbackId;
use crate::services::config_manager::ConfigManager;
use crate::services::icons::{IconHandle, get_app_icon_name, set_image_from_app_id};
use crate::services::media::{MediaService, MediaSnapshot, PlaybackStatus, format_duration};
use crate::services::tooltip::TooltipManager;
use crate::styles::media;
use crate::widgets::base::BaseWidget;
use crate::widgets::marquee_label::{MarqueeLabel, ScrollMode};
use crate::widgets::media_popover::{MediaPopoverController, build_media_popover_with_controller};
use crate::widgets::rounded_picture::RoundedPicture;
use crate::widgets::{WidgetConfig, warn_unknown_options};

/// Default template: album art, then artist - title.
const DEFAULT_TEMPLATE: &str = "{art}{artist} - {title}";
/// Default maximum text length (0 = unlimited).
const DEFAULT_MAX_CHARS: usize = 30;

// ========== Album Art Rendering Constants ==========

/// Album art display size as a ratio of bar_size.
/// 0.75 ensures art fits within the bar with padding (e.g., 24px art in 32px bar).
/// This leaves room for vertical padding and prevents the art from touching the bar edges,
/// maintaining visual balance with other bar elements.
const ART_DISPLAY_SCALE: f64 = 0.75;

/// Configuration for the media widget.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// Template string for rendering.
    /// Widget tokens: {art}, {player_icon}, {icon}
    /// Text tokens: {title}, {artist}, {album}, {player}, {position}, {duration}
    pub template: String,
    /// Text to show when no player is available (empty = hide widget).
    pub empty_text: String,
    /// Maximum text length (0 = unlimited).
    pub max_chars: usize,
    /// Scroll mode for text that doesn't fit: "pingpong" or "loop".
    pub scroll_mode: ScrollMode,
    /// Custom background color for this widget.
    pub background_color: Option<String>,
}

impl WidgetConfig for MediaConfig {
    fn from_entry(entry: &WidgetEntry) -> Self {
        warn_unknown_options(
            "media",
            entry,
            &[
                "template",
                "empty_text",
                "max_chars",
                "scroll_mode",
                "background_color",
            ],
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

        let scroll_mode = entry
            .options
            .get("scroll_mode")
            .and_then(|v| v.as_str())
            .map(|s| match s.to_lowercase().as_str() {
                "loop" => ScrollMode::Loop,
                _ => ScrollMode::PingPong,
            })
            .unwrap_or_default();

        Self {
            template,
            empty_text,
            max_chars,
            scroll_mode,
            background_color: entry.background_color.clone(),
        }
    }
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            empty_text: String::new(),
            max_chars: DEFAULT_MAX_CHARS,
            scroll_mode: ScrollMode::default(),
            background_color: None,
        }
    }
}

/// State for tracking album art loading to avoid redundant loads.
struct ArtState {
    /// Currently loaded art URL (to detect changes).
    current_url: Option<String>,
    /// Whether we have valid art loaded.
    has_art: bool,
    /// Generation counter to prevent race conditions in async art loading.
    /// Incremented each time a new art load is initiated; callbacks validate
    /// their generation matches before applying the result.
    generation: u64,
    /// Cancellable for in-flight art loading operations.
    /// Cancelled when a new art load begins to abort stale I/O.
    cancellable: gio::Cancellable,
}

impl Default for ArtState {
    fn default() -> Self {
        Self {
            current_url: None,
            has_art: false,
            generation: 0,
            cancellable: gio::Cancellable::new(),
        }
    }
}

/// Widget tokens that create actual GTK widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidgetToken {
    /// Album art (falls back to player icon, then generic music icon).
    Art,
    /// Player app icon (uses get_app_icon_name).
    PlayerIcon,
    /// Play/pause status icon.
    Icon,
    /// Playback controls (previous, play/pause, next buttons).
    Controls,
}

impl WidgetToken {
    /// Parse a token string into a WidgetToken if it matches.
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
    Player,
    Position,
    Duration,
}

impl TextToken {
    /// Get the value for this token from the snapshot.
    ///
    /// Returns an empty string if the value is not available in the snapshot.
    /// This behavior is important for the cleanup logic in `clean_rendered_text`,
    /// which removes orphaned separators when tokens are empty.
    fn value(self, snapshot: &MediaSnapshot) -> String {
        match self {
            Self::Title => snapshot.metadata.title.clone().unwrap_or_default(),
            Self::Artist => snapshot.metadata.artist.clone().unwrap_or_default(),
            Self::Album => snapshot.metadata.album.clone().unwrap_or_default(),
            Self::Player => snapshot.player_name.clone().unwrap_or_default(),
            Self::Position => format_duration(snapshot.position),
            Self::Duration => snapshot
                .metadata
                .length
                .map(format_duration)
                .unwrap_or_default(),
        }
    }
}

/// Parsed template element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateElement {
    /// A widget token (creates a GTK widget).
    Widget(WidgetToken),
    /// Literal text (including text tokens to be replaced).
    Text(String),
}

/// Parse a template string into elements.
///
/// Widget tokens ({art}, {player_icon}, {icon}, {controls}) become TemplateElement::Widget.
/// Everything else (including text tokens) remains as TemplateElement::Text.
///
/// Note: Literal braces are not supported. If you need a literal `{` or `}` character
/// in the template output, this is not currently possible. However, this is unlikely
/// to be needed for typical media display templates.
fn parse_template(template: &str) -> Vec<TemplateElement> {
    let mut elements = Vec::new();
    let mut current_text = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '{' {
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

            if found_close {
                // Check if it's a widget token
                if let Some(widget_token) = WidgetToken::parse(&token) {
                    // Flush any accumulated text
                    if !current_text.is_empty() {
                        elements.push(TemplateElement::Text(current_text.clone()));
                        current_text.clear();
                    }
                    elements.push(TemplateElement::Widget(widget_token));
                } else {
                    // Check if it's a known text token, warn if not
                    const KNOWN_TEXT_TOKENS: &[&str] =
                        &["title", "artist", "album", "player", "position", "duration"];
                    if !KNOWN_TEXT_TOKENS.contains(&token.as_str()) {
                        warn!(
                            "Unknown template token '{{{}}}' in media widget template. \
                             Known tokens: {{art}}, {{player_icon}}, {{icon}}, {{controls}}, \
                             {{title}}, {{artist}}, {{album}}, {{player}}, {{position}}, {{duration}}",
                            token
                        );
                    }
                    // Keep as text (including the braces) - will be replaced later
                    current_text.push('{');
                    current_text.push_str(&token);
                    current_text.push('}');
                }
            } else {
                // Unclosed brace - treat as literal
                current_text.push('{');
                current_text.push_str(&token);
            }
        } else {
            current_text.push(c);
        }
    }

    // Flush remaining text
    if !current_text.is_empty() {
        elements.push(TemplateElement::Text(current_text));
    }

    elements
}

/// Extract the text-only portion of a template (widget tokens removed).
fn extract_text_template(template: &str) -> String {
    let elements = parse_template(template);
    elements
        .iter()
        .filter_map(|e| match e {
            TemplateElement::Text(s) => Some(s.as_str()),
            TemplateElement::Widget(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Render text tokens in a string, replacing {token} with values.
fn render_text_tokens(text: &str, snapshot: &MediaSnapshot) -> String {
    let mut result = text.to_string();

    // Replace all text tokens
    for (token_name, token) in [
        ("title", TextToken::Title),
        ("artist", TextToken::Artist),
        ("album", TextToken::Album),
        ("player", TextToken::Player),
        ("position", TextToken::Position),
        ("duration", TextToken::Duration),
    ] {
        let placeholder = format!("{{{}}}", token_name);
        let value = token.value(snapshot);
        result = result.replace(&placeholder, &value);
    }

    result
}

/// Clean up rendered text by removing artifacts from empty tokens.
///
/// When template tokens like `{artist}` or `{title}` are empty, this function
/// removes orphaned separators that would otherwise appear in the output.
///
/// # Supported Separators
///
/// The following separators are recognized and cleaned up:
/// - `-` (hyphen-minus, U+002D)
/// - `–` (en-dash, U+2013)
/// - `—` (em-dash, U+2014)
/// - `‒` (figure dash, U+2012)
/// - `|` (pipe)
/// - `:` (colon)
///
/// - `/` (slash)
/// - `•` (bullet, U+2022)
///
/// # Examples
///
/// - `" - "` → `""` (when artist or title is empty)
/// - `"Artist - "` → `"Artist"` (trailing separator removed)
/// - `" - Song"` → `"Song"` (leading separator removed)
/// - `"Artist - - Song"` → `"Artist - Song"` (consecutive separators collapsed)
fn clean_rendered_text(text: &str) -> String {
    const SEPARATORS: &[&str] = &["-", "–", "—", "‒", "|", ":", "/", "•"];

    let mut result = String::with_capacity(text.len());
    let mut last_was_separator = true; // Treat start as if preceded by separator (to skip leading)
    let mut pending_separator: Option<&str> = None;

    // Split by whitespace to get tokens
    for token in text.split_whitespace() {
        // Check if this token is a separator
        let is_separator = SEPARATORS.contains(&token);

        if is_separator {
            // If we already have a pending separator or we're at the start, skip this one
            if !last_was_separator && pending_separator.is_none() {
                pending_separator = Some(token);
            }
            // Otherwise skip (consecutive separators or leading separator)
        } else {
            // Regular content token
            if !result.is_empty() {
                // Add space before this token
                if let Some(sep) = pending_separator {
                    // Add the pending separator with spaces
                    result.push(' ');
                    result.push_str(sep);
                    result.push(' ');
                } else {
                    result.push(' ');
                }
            }
            result.push_str(token);
            last_was_separator = false;
            pending_separator = None;
        }
    }

    result
}

/// Media widget that displays playback status and opens a popover on click.
pub struct MediaWidget {
    /// Shared base widget container (provides the root GTK widget).
    base: BaseWidget,
    /// Callback ID for MediaService updates (stored for cleanup on drop).
    media_callback_id: CallbackId,
}

/// Handle for the playback controls buttons.
#[derive(Clone)]
struct ControlsHandle {
    /// Container box for the controls.
    container: gtk4::Box,
    /// Play/pause button icon.
    play_pause_icon: IconHandle,
}

/// Context holding references to all UI widgets for updates.
///
/// This reduces the number of parameters passed to `update_widgets_from_snapshot_impl`
/// by grouping all widget references into a single struct.
struct WidgetUpdateContext<'a> {
    container: &'a gtk4::Box,
    status_icon: &'a Option<IconHandle>,
    player_icon: &'a Option<Image>,
    art_picture: &'a Option<RoundedPicture>,
    text_label: &'a Option<Rc<MarqueeLabel>>,
    controls: &'a Option<ControlsHandle>,
    /// Pre-extracted text template (widget tokens removed, only text tokens remain).
    /// Cached to avoid re-parsing on every update.
    text_template: &'a str,
    empty_text: &'a str,
    art_state: &'a Rc<RefCell<ArtState>>,
}

/// Owned version of widget references for use in callbacks.
///
/// This struct groups all the cloneable widget references together, allowing
/// a single clone operation instead of cloning each field individually.
#[derive(Clone)]
struct CallbackWidgetRefs {
    container: gtk4::Box,
    status_icon: Option<IconHandle>,
    player_icon: Option<Image>,
    art_picture: Option<RoundedPicture>,
    text_label: Option<Rc<MarqueeLabel>>,
    controls: Option<ControlsHandle>,
    /// Pre-extracted text template (widget tokens removed, only text tokens remain).
    /// Cached to avoid re-parsing on every update.
    text_template: String,
    empty_text: String,
    art_state: Rc<RefCell<ArtState>>,
}

impl CallbackWidgetRefs {
    /// Create a borrowed context from this owned struct.
    fn as_context(&self) -> WidgetUpdateContext<'_> {
        WidgetUpdateContext {
            container: &self.container,
            status_icon: &self.status_icon,
            player_icon: &self.player_icon,
            art_picture: &self.art_picture,
            text_label: &self.text_label,
            controls: &self.controls,
            text_template: &self.text_template,
            empty_text: &self.empty_text,
            art_state: &self.art_state,
        }
    }
}

/// Create inline playback controls (previous, play/pause, next buttons).
fn create_controls(base: &BaseWidget) -> ControlsHandle {
    use crate::services::icons::IconsService;
    use crate::styles::{button, icon};
    use crate::widgets::media_utils::create_media_control_button;
    use gtk4::Button;

    let icons = IconsService::global();

    // Container for the control buttons
    let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    container.add_css_class(media::CONTROLS);
    container.set_visible(false); // Hidden until we have a player

    // Previous button
    let prev_btn = create_media_control_button(
        &icons,
        "skip_previous",
        "Previous",
        &[media::CONTROL_BTN, button::COMPACT],
        || MediaService::global().previous(),
    );
    container.append(&prev_btn);

    // Play/pause button
    let play_pause_icon = icons.create_icon("play_arrow", &[icon::ICON]);
    let play_pause_btn = Button::new();
    play_pause_btn.set_child(Some(&play_pause_icon.widget()));
    play_pause_btn.add_css_class(media::CONTROL_BTN);
    play_pause_btn.add_css_class(button::COMPACT);
    play_pause_btn.set_tooltip_text(Some("Play/Pause"));
    play_pause_btn.connect_clicked(|_| {
        MediaService::global().play_pause();
    });
    container.append(&play_pause_btn);

    // Next button
    let next_btn = create_media_control_button(
        &icons,
        "skip_next",
        "Next",
        &[media::CONTROL_BTN, button::COMPACT],
        || MediaService::global().next(),
    );
    container.append(&next_btn);

    // Append to parent widget
    base.content().append(&container);

    ControlsHandle {
        container,
        play_pause_icon,
    }
}

impl MediaWidget {
    /// Create a new media widget with the given configuration.
    pub fn new(config: MediaConfig) -> Self {
        let base = BaseWidget::new(&[media::WIDGET], config.background_color.clone());

        // Initial tooltip until the first snapshot arrives.
        base.set_tooltip("No media playing");

        // Parse the template to identify what widgets we need
        let template_elements = parse_template(&config.template);

        // Create widgets in template order.
        // Each widget is created only once (on first occurrence in template).
        let mut art_picture = None;
        let mut player_icon = None;
        let mut status_icon = None;
        let mut controls = None;
        let mut text_label = None;

        for element in &template_elements {
            match element {
                TemplateElement::Widget(WidgetToken::Art) => {
                    if art_picture.is_none() {
                        // Use RoundedPicture for album art with GPU-accelerated rounded corners.
                        let config_mgr = ConfigManager::global();
                        let art_size = (config_mgr.bar_size() as f64 * ART_DISPLAY_SCALE) as i32;
                        let corner_radius = config_mgr.radius_pill() as f32;

                        let picture = RoundedPicture::new();
                        picture.set_pixel_size(art_size);
                        picture.set_corner_radius(corner_radius);
                        picture.add_css_class(media::ART_SMALL);
                        picture.set_visible(false);

                        base.content().append(&picture);
                        art_picture = Some(picture);
                    }
                }
                TemplateElement::Widget(WidgetToken::PlayerIcon) => {
                    if player_icon.is_none() {
                        // Use GTK Image for app icons (not IconHandle/Material Symbols)
                        // This properly renders themed app icons like "firefox", "spotify"
                        let image = Image::from_icon_name(media::ICON_AUDIO_GENERIC);
                        image.add_css_class(media::PLAYER_ICON);
                        image.set_visible(false);
                        base.content().append(&image);
                        player_icon = Some(image);
                    }
                }
                TemplateElement::Widget(WidgetToken::Icon) => {
                    if status_icon.is_none() {
                        let handle = base.add_icon(media::ICON_PAUSE, &[media::ICON]);
                        handle.widget().set_visible(false);
                        status_icon = Some(handle);
                    }
                }
                TemplateElement::Widget(WidgetToken::Controls) => {
                    if controls.is_none() {
                        controls = Some(create_controls(&base));
                    }
                }
                TemplateElement::Text(_) => {
                    // Text label created once, handles all text content
                    if text_label.is_none() {
                        let marquee = Rc::new(MarqueeLabel::with_scroll_mode(config.scroll_mode));
                        marquee.label().add_css_class(media::LABEL);
                        if config.max_chars > 0 {
                            marquee.set_max_width_chars(config.max_chars as i32);
                        }
                        marquee.set_visible(false);
                        base.content().append(marquee.widget());
                        text_label = Some(marquee);
                    }
                }
            }
        }

        // Shared controller storage between the widget and the menu builder.
        let controller_cell: Rc<RefCell<Option<MediaPopoverController>>> =
            Rc::new(RefCell::new(None));
        let controller_for_builder = controller_cell.clone();

        // Create a popover menu for detailed media controls.
        base.create_menu("media", move || {
            let (widget, controller) = build_media_popover_with_controller();
            *controller_for_builder.borrow_mut() = Some(controller);
            widget
        });

        // Subscribe to the shared MediaService for live updates.
        let media_service = MediaService::global();
        // Pre-extract text template once (strip widget tokens, keep text tokens)
        let text_template = extract_text_template(&config.template);
        // Create shared art state once, used by both initial update and callbacks
        let art_state = Rc::new(RefCell::new(ArtState::default()));

        let widget_refs = CallbackWidgetRefs {
            container: base.widget().clone(),
            status_icon: status_icon.clone(),
            player_icon: player_icon.clone(),
            art_picture: art_picture.clone(),
            text_label: text_label.clone(),
            controls: controls.clone(),
            text_template,
            empty_text: config.empty_text.clone(),
            art_state: art_state.clone(),
        };

        // Initial state - hidden until we have a player
        // Note: We call the impl function directly with the shared art_state
        // to ensure consistent state tracking from the start.
        update_widgets_from_snapshot_impl(&widget_refs.as_context(), &MediaSnapshot::empty());

        // Now register callback (consumes widget_refs via clone in closure)
        let controller_for_cb = controller_cell.clone();
        let media_callback_id = media_service.connect(move |snapshot: &MediaSnapshot| {
            update_widgets_from_snapshot_impl(&widget_refs.as_context(), snapshot);

            // If the popover content has been built, push live updates.
            if let Some(controller) = controller_for_cb.borrow().as_ref() {
                controller.update_from_snapshot(snapshot);
            }
        });

        Self {
            base,
            media_callback_id,
        }
    }

    /// Get the root GTK widget for embedding in the bar.
    pub fn widget(&self) -> &gtk4::Box {
        self.base.widget()
    }
}

impl Drop for MediaWidget {
    fn drop(&mut self) {
        // Unregister callback from MediaService to prevent memory leak
        MediaService::global().disconnect(self.media_callback_id);
    }
}

/// Update the visual widget state given a media snapshot.
///
/// # Icon Naming Conventions
///
/// This module uses two different icon naming conventions depending on the API:
///
/// - **Freedesktop names** (e.g., `"media-playback-pause"`, `"audio-volume-high"`):
///   Used with `IconHandle` and `BaseWidget.add_icon()`. These are mapped internally
///   to Material Symbols font glyphs.
///
/// - **Material Symbols names** (e.g., `"pause"`, `"play_arrow"`, `"skip_next"`):
///   Used directly with `IconsService::create_icon()`. These are the raw icon names
///   from the Material Symbols font.
fn update_widgets_from_snapshot_impl(ctx: &WidgetUpdateContext<'_>, snapshot: &MediaSnapshot) {
    // Handle unavailable state
    if !snapshot.available {
        if ctx.empty_text.is_empty() {
            // Hide widget entirely
            ctx.container.set_visible(false);
        } else {
            // Show empty text
            ctx.container.set_visible(true);
            if let Some(marquee) = ctx.text_label {
                marquee.set_text(ctx.empty_text);
                marquee.set_visible(true);
            }
            // Hide all widget tokens
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
        }
        return;
    }

    // Show widget when player is available
    ctx.container.set_visible(true);

    // Update CSS state classes
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

    // Update status icon (play/pause indicator) - uses freedesktop names
    if let Some(icon) = ctx.status_icon {
        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => media::ICON_PAUSE,
            PlaybackStatus::Paused | PlaybackStatus::Stopped => media::ICON_PLAY,
        };
        icon.set_icon(icon_name);
        icon.widget().set_visible(true);
    }

    // Update playback controls - uses Material Symbols names
    if let Some(ctrl) = ctx.controls {
        let icon_name = match snapshot.playback_status {
            PlaybackStatus::Playing => "pause",
            PlaybackStatus::Paused | PlaybackStatus::Stopped => "play_arrow",
        };
        ctrl.play_pause_icon.set_icon(icon_name);
        ctrl.container.set_visible(true);
    }

    // Update player icon (app icon for the player)
    if let Some(image) = ctx.player_icon {
        if let Some(player_id) = &snapshot.player_id {
            set_image_from_app_id(image, player_id);
            image.set_visible(true);
        } else {
            // No player ID, use generic icon
            image.set_icon_name(Some(media::ICON_AUDIO_GENERIC));
            image.set_visible(true);
        }
    }

    // Album art handling
    if let Some(picture) = ctx.art_picture {
        let art_url = snapshot.metadata.art_url.as_deref();

        // Check if URL changed and prepare for loading in a single borrow.
        // This avoids a race condition window between checking and mutating state.
        let load_info = {
            let mut state = ctx.art_state.borrow_mut();
            if state.current_url.as_deref() == art_url {
                None // No change needed
            } else {
                // Cancel any in-flight art loading operations
                state.cancellable.cancel();
                // Update state atomically
                state.current_url = art_url.map(String::from);
                state.has_art = false;
                state.generation += 1;
                state.cancellable = gio::Cancellable::new();
                Some((state.generation, state.cancellable.clone()))
            }
        };

        if let Some((generation, cancellable)) = load_info {
            if let Some(url) = art_url {
                // Load art asynchronously
                load_album_art_with_fallback(
                    url,
                    picture,
                    snapshot.player_id.as_deref(),
                    ctx.art_state,
                    generation,
                    &cancellable,
                );
            } else {
                // No art URL - show player icon fallback
                show_player_icon_in_art(
                    picture,
                    snapshot.player_id.as_deref(),
                    ctx.art_state,
                    generation,
                );
            }
        }
    }

    // Render text from template
    if let Some(marquee) = ctx.text_label {
        // Use pre-extracted text template (widget tokens already stripped)
        // Render text tokens in the template
        let rendered = render_text_tokens(ctx.text_template, snapshot);
        // Clean up artifacts from empty tokens
        let cleaned = clean_rendered_text(&rendered);

        if cleaned.is_empty() {
            marquee.set_visible(false);
        } else {
            // MarqueeLabel handles overflow via scrolling, no truncation needed
            marquee.set_text(&cleaned);
            marquee.set_visible(true);
        }
    }

    // Build tooltip
    let tooltip = build_tooltip(snapshot);
    let tooltip_manager = TooltipManager::global();
    tooltip_manager.set_styled_tooltip(ctx.container, &tooltip);
}

/// Load album art from a URL (file:// or http(s)://) asynchronously.
/// Falls back to player icon if art loading fails.
///
/// The `generation` parameter is used to prevent race conditions: if the art URL
/// changes while a load is in progress, the callback validates its generation
/// matches the current state before applying the result. The `cancellable` is
/// used to abort in-flight I/O when a new art load begins.
fn load_album_art_with_fallback(
    url: &str,
    art_picture: &RoundedPicture,
    player_id: Option<&str>,
    art_state: &Rc<RefCell<ArtState>>,
    generation: u64,
    cancellable: &gio::Cancellable,
) {
    let url_string = url.to_string();
    let art_picture = art_picture.clone();
    let player_id = player_id.map(String::from);
    let art_state = art_state.clone();
    let cancellable = cancellable.clone();

    // gio::File::for_uri handles file://, http://, and https:// via GVfs
    if url.starts_with("file://") || url.starts_with("http://") || url.starts_with("https://") {
        let file = gio::File::for_uri(url);
        let cancellable_for_read = cancellable.clone();

        file.read_async(
            glib::Priority::DEFAULT,
            Some(&cancellable_for_read),
            move |result| {
                // Validate generation before processing (handles race where cancel arrives late)
                if art_state.borrow().generation != generation {
                    return;
                }

                match result {
                    Ok(stream) => {
                        load_texture_from_stream_with_fallback(
                            stream.upcast(),
                            &art_picture,
                            player_id.as_deref(),
                            &art_state,
                            &url_string,
                            generation,
                            &cancellable,
                        );
                    }
                    Err(e) => {
                        // Don't log cancelled operations as failures
                        if !e.matches(gio::IOErrorEnum::Cancelled) {
                            debug!("Failed to load album art from {}: {}", url_string, e);
                        }
                        show_player_icon_in_art(
                            &art_picture,
                            player_id.as_deref(),
                            &art_state,
                            generation,
                        );
                    }
                }
            },
        );
    } else {
        // Unknown URL scheme - show player icon fallback
        debug!("Unknown album art URL scheme: {}", url);
        show_player_icon_in_art(&art_picture, player_id.as_deref(), &art_state, generation);
    }
}

/// Load a texture from an input stream and apply it to the picture widget.
/// Falls back to player icon if loading fails.
///
/// The `generation` parameter is validated before applying results to prevent
/// race conditions when art URL changes rapidly. The `cancellable` is used to
/// abort the pixbuf decoding if a new art load begins.
fn load_texture_from_stream_with_fallback(
    stream: gio::InputStream,
    art_picture: &RoundedPicture,
    player_id: Option<&str>,
    art_state: &Rc<RefCell<ArtState>>,
    url: &str,
    generation: u64,
    cancellable: &gio::Cancellable,
) {
    let art_picture = art_picture.clone();
    let player_id = player_id.map(String::from);
    let art_state = art_state.clone();
    let url = url.to_string();
    let cancellable = cancellable.clone();

    // Load the texture from the stream using GdkPixbuf
    gtk4::gdk_pixbuf::Pixbuf::from_stream_async(&stream, Some(&cancellable), move |result| {
        // Validate generation before applying result
        if art_state.borrow().generation != generation {
            return; // Stale request, ignore
        }

        match result {
            Ok(pixbuf) => {
                // Create texture directly from pixbuf - RoundedPicture handles corner rounding
                // via GSK's push_rounded_clip (GPU-accelerated).
                let texture = Texture::for_pixbuf(&pixbuf);

                art_picture.set_paintable(Some(&texture));
                art_picture.set_visible(true);

                // Update state
                let mut state = art_state.borrow_mut();
                state.has_art = true;

                debug!("Loaded album art from {}", url);
            }
            Err(e) => {
                // Don't show fallback for cancelled operations (new art load started)
                if e.matches(gio::IOErrorEnum::Cancelled) {
                    return;
                }
                debug!("Failed to decode album art from {}: {}", url, e);
                show_player_icon_in_art(&art_picture, player_id.as_deref(), &art_state, generation);
            }
        }
    });
}

/// Show the player's app icon in the art picture widget as fallback.
///
/// The `generation` parameter is validated before applying to prevent race conditions.
fn show_player_icon_in_art(
    art_picture: &RoundedPicture,
    player_id: Option<&str>,
    art_state: &Rc<RefCell<ArtState>>,
    generation: u64,
) {
    // Validate generation before applying
    if art_state.borrow().generation != generation {
        return; // Stale request, ignore
    }

    // Try to get player icon, fall back to generic music icon
    let icon_name = if let Some(id) = player_id {
        let name = get_app_icon_name(id);
        if name.is_empty() {
            id.to_string()
        } else {
            name
        }
    } else {
        media::ICON_AUDIO_GENERIC.to_string()
    };

    // Load icon as paintable using icon theme
    let Some(display) = gtk4::gdk::Display::default() else {
        warn!("No display available for icon lookup");
        // Can't display without a display - just mark state and return
        let mut state = art_state.borrow_mut();
        state.has_art = false;
        return;
    };
    let icon_theme = gtk4::IconTheme::for_display(&display);

    // Get art size for icon lookup
    let config = ConfigManager::global();
    let art_size = (config.bar_size() as f64 * ART_DISPLAY_SCALE) as i32;

    // Try to load the icon at the art size.
    // Note: lookup_icon() always returns a paintable - if the icon is not found,
    // GTK returns a "missing icon" placeholder. This is acceptable behavior as
    // the user will see a generic icon rather than an empty space.
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

    let mut state = art_state.borrow_mut();
    state.has_art = false; // Mark as fallback, not real art
}

/// Build tooltip text from media snapshot.
fn build_tooltip(snapshot: &MediaSnapshot) -> String {
    if !snapshot.available {
        return "No media playing".to_string();
    }

    let mut lines = Vec::new();

    // Player name
    if let Some(name) = &snapshot.player_name {
        lines.push(format!("Player: {}", name));
    }

    // Track info
    if let Some(title) = &snapshot.metadata.title {
        lines.push(format!("Title: {}", title));
    }
    if let Some(artist) = &snapshot.metadata.artist {
        lines.push(format!("Artist: {}", artist));
    }
    if let Some(album) = &snapshot.metadata.album {
        lines.push(format!("Album: {}", album));
    }

    // Status
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

    #[test]
    fn test_media_config_defaults() {
        let entry = WidgetEntry {
            name: "media".to_string(),
            options: Default::default(),
            background_color: None,
        };
        let config = MediaConfig::from_entry(&entry);
        assert_eq!(config.template, "{art}{artist} - {title}");
        assert_eq!(config.empty_text, "");
        assert_eq!(config.max_chars, 30);
    }

    #[test]
    fn test_build_tooltip_empty() {
        let snapshot = MediaSnapshot::empty();
        assert_eq!(build_tooltip(&snapshot), "No media playing");
    }

    #[test]
    fn test_build_tooltip_with_track() {
        let mut snapshot = MediaSnapshot::default();
        snapshot.available = true;
        snapshot.player_name = Some("Spotify".to_string());
        snapshot.metadata.title = Some("Test Song".to_string());
        snapshot.metadata.artist = Some("Test Artist".to_string());
        snapshot.playback_status = PlaybackStatus::Playing;

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
        assert_eq!(elements.len(), 1);
        assert!(matches!(&elements[0], TemplateElement::Text(s) if s == "{title} - {artist}"));
    }

    #[test]
    fn test_parse_template_mixed() {
        let elements = parse_template("{art}{title} - {artist}");
        assert_eq!(elements.len(), 2);
        assert!(matches!(
            elements[0],
            TemplateElement::Widget(WidgetToken::Art)
        ));
        assert!(matches!(&elements[1], TemplateElement::Text(s) if s == "{title} - {artist}"));
    }

    #[test]
    fn test_render_text_tokens() {
        let mut snapshot = MediaSnapshot::default();
        snapshot.metadata.title = Some("Test Song".to_string());
        snapshot.metadata.artist = Some("Test Artist".to_string());
        snapshot.player_name = Some("Spotify".to_string());

        let result = render_text_tokens("{artist} - {title}", &snapshot);
        assert_eq!(result, "Test Artist - Test Song");

        let result = render_text_tokens("{player}: {title}", &snapshot);
        assert_eq!(result, "Spotify: Test Song");
    }

    #[test]
    fn test_render_text_tokens_missing() {
        let snapshot = MediaSnapshot::default();
        let result = render_text_tokens("{artist} - {title}", &snapshot);
        assert_eq!(result, " - ");
    }

    #[test]
    fn test_clean_rendered_text() {
        // Empty tokens leave separators
        assert_eq!(clean_rendered_text(" - "), "");
        assert_eq!(clean_rendered_text(" - Song"), "Song");
        assert_eq!(clean_rendered_text("Artist - "), "Artist");

        // Multiple separators
        assert_eq!(clean_rendered_text(" - - "), "");

        // Normal text unchanged
        assert_eq!(clean_rendered_text("Artist - Song"), "Artist - Song");
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
    fn test_extract_text_template() {
        // Widget tokens should be stripped
        assert_eq!(
            extract_text_template("{art}{artist} - {title}"),
            "{artist} - {title}"
        );
        assert_eq!(
            extract_text_template("{player_icon}{icon}{title}"),
            "{title}"
        );
        assert_eq!(extract_text_template("{art}"), "");

        // Text-only templates unchanged
        assert_eq!(
            extract_text_template("{artist} - {title}"),
            "{artist} - {title}"
        );

        // Multiple text segments joined
        assert_eq!(
            extract_text_template("{art}{artist}{icon} - {title}"),
            "{artist} - {title}"
        );
    }
}
