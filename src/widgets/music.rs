//! "Now playing" widget, backed by MPRIS.
//!
//! Most Linux music players — Spotify, Sonora, Amberol, Rhythmbox, VLC, mpv,
//! and any browser tab that is playing media — advertise what they are playing
//! on the session bus through the [MPRIS] spec (`org.mpris.MediaPlayer2.*`).
//! Rather than talk to one player, this widget reads that standard interface,
//! so it follows whichever player currently owns the sound: the track's title,
//! artist and album, its playback status and position, and the cover art
//! (`mpris:artUrl`, either a local `file://` path or an `http(s)` URL).
//!
//! The bar shows a small cover thumbnail next to the artist — title; clicking
//! it opens a popover with the full artwork, the track details, transport
//! buttons (previous / play-pause / next) and a seek slider. Scrolling the bar
//! widget skips to the previous or next track. The whole pill hides itself
//! while nothing is playing, so an idle shell has no empty gap in the bar.
//!
//! Like the volume and visualizer widgets, everything slow happens off the GTK
//! main loop: a poller thread owns the D-Bus connection and re-reads the bus's
//! players on a short tick, and a second thread downloads the cover art, so a
//! slow network never stalls the bar. (Artwork is fetched with `curl`, the same
//! way the volume widget shells out to `pactl`, because GIO's HTTP support
//! depends on an optional `gvfs` backend.)
//!
//! [MPRIS]: https://specifications.freedesktop.org/mpris-spec/latest/

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use gtk::cairo;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use zbus::blocking::Connection;
use zbus::blocking::Proxy;
use zbus::blocking::fdo::DBusProxy;
use zbus::zvariant::{OwnedValue, Value};

/// Every MPRIS player's bus name starts with this.
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
/// The one object path every player exposes.
const PLAYER_PATH: &str = "/org/mpris/MediaPlayer2";
/// The playback interface: metadata, status, position, transport.
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
/// The root interface, used only for the player's human name (`Identity`).
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";

/// While a track is playing, re-read often so the progress slider moves.
const PLAYING_POLL: Duration = Duration::from_millis(500);
/// A paused/stopped player still needs the occasional check (track changed
/// from another app, player closed, …).
const IDLE_POLL: Duration = Duration::from_millis(1500);
/// With nothing on the bus, check rarely so an idle shell stays quiet.
const EMPTY_POLL: Duration = Duration::from_secs(5);
/// How long to wait before retrying when the session bus is unreachable.
const RETRY: Duration = Duration::from_secs(5);
/// Cap on a cover download, so a hostile URL cannot exhaust memory.
const MAX_ART_BYTES: usize = 16 * 1024 * 1024;

/// The seek slider runs on this many steps (`fraction = value / SEEK_RANGE`).
const SEEK_RANGE: f64 = 1000.0;

/// How long after the last slider movement a seek is committed, so a drag
/// sends one call instead of one per pixel.
const SEEK_DEBOUNCE: Duration = Duration::from_millis(180);

/// How close the player's reported position must be to a just-issued seek
/// target (as a fraction of the track) for the seek to count as confirmed.
const SEEK_TOLERANCE: f64 = 0.02;
/// While playing, stop waiting for a seek to be confirmed after this long and
/// trust the player's position again.
const SEEK_CONFIRM: Duration = Duration::from_millis(2500);

/// Cover thumbnail in the bar.
const BAR_COVER: i32 = 22;
/// Cover art in the popover.
const POP_COVER: i32 = 96;
/// Upper bound on the stored cover size; cairo cover-fits it down anyway.
const MAX_COVER_PX: i32 = 256;

/// A snapshot of what a player is playing right now.
///
/// This doubles as the poller's change detector: it derives `PartialEq`, and
/// the UI is only told when the snapshot actually differs (a new track, a
/// status change, or the position moving on while playing).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    /// The player's bus name, e.g. `org.mpris.MediaPlayer2.spotify`.
    pub bus: String,
    /// The player's own name (`Identity`), e.g. `Spotify`.
    pub player: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// `mpris:artUrl` — a `file://` path or an `http(s)` URL.
    pub art: Option<String>,
    pub playing: bool,
    /// Track length in microseconds, when the player reports one.
    pub length: Option<i64>,
    /// Playhead position in microseconds.
    pub position: i64,
    pub can_next: bool,
    pub can_prev: bool,
    pub can_seek: bool,
}

impl Track {
    /// The one-line label shown in the bar: `artist — title`, with graceful
    /// fallbacks when a player leaves a field empty.
    pub(crate) fn bar_text(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => self.player.clone(),
            ("", title) => title.to_string(),
            (artist, "") => artist.to_string(),
            (artist, title) => format!("{artist} — {title}"),
        }
    }

    /// The hover tooltip: the full title and artist, whichever are known.
    fn tooltip(&self) -> String {
        match (self.title.trim(), self.artist.trim()) {
            ("", "") => self.player.clone(),
            ("", artist) => artist.to_string(),
            (title, "") => title.to_string(),
            (title, artist) => format!("{title}\n{artist}"),
        }
    }
}

/// Messages sent from the poller and downloader threads into the UI.
#[derive(Debug)]
pub enum MusicInput {
    /// A fresh snapshot, or `None` when nothing is playing.
    Update(Option<Track>),
    /// A cover finished downloading; `token` guards against a stale one
    /// overwriting a newer track's art.
    Cover { token: u64, bytes: Vec<u8> },
    /// A cover could not be fetched (the fallback glyph stays).
    CoverFailed { token: u64 },
}

/// Transport commands sent from the UI to the poller thread. The dashboard's
/// now-playing card shares these, so `pub(crate)` on the enum.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    PlayPause,
    Next,
    Previous,
    /// Seek to a fraction (`0.0..=1.0`) of the track.
    Seek(f64),
}

/// A cover-art download request handed to the fetcher thread.
#[derive(Debug)]
struct ArtRequest {
    token: u64,
    url: String,
}

/// A fixed-size, cover-fitted album-art thumbnail.
///
/// The artwork is painted by cairo rather than shown with a `GtkPicture`,
/// because a `GtkPicture` reports the *image's* natural size as its own and
/// would stretch the bar to the artwork's height. A drawing area requests
/// exactly the size we ask for and crops the image to it.
struct Cover {
    area: gtk::DrawingArea,
    pixbuf: Rc<RefCell<Option<Pixbuf>>>,
}

impl Cover {
    fn new(width: i32, height: i32, radius: f64) -> Self {
        let area = gtk::DrawingArea::new();
        area.set_content_width(width);
        area.set_content_height(height);
        area.set_valign(gtk::Align::Center);

        let pixbuf: Rc<RefCell<Option<Pixbuf>>> = Rc::new(RefCell::new(None));
        let painted = pixbuf.clone();
        area.set_draw_func(move |_, cr, w, h| {
            if let Some(pixbuf) = painted.borrow().as_ref() {
                draw_cover(cr, pixbuf, w, h, radius);
            }
        });

        Cover { area, pixbuf }
    }

    /// Show `pixbuf`, or clear back to the fallback glyph.
    fn set(&self, pixbuf: Option<Pixbuf>) {
        *self.pixbuf.borrow_mut() = pixbuf;
        self.area.queue_draw();
    }
}

/// Build a cover with a music-note fallback behind it. Returns the container
/// to put in a layout and the [`Cover`] handle to update it.
fn build_cover(width: i32, height: i32, radius: f64) -> (gtk::Overlay, Cover) {
    let cover = Cover::new(width, height, radius);

    let fallback = gtk::Image::from_icon_name("audio-x-generic-symbolic");
    fallback.set_pixel_size((width / 2).max(12));
    fallback.set_halign(gtk::Align::Center);
    fallback.set_valign(gtk::Align::Center);
    fallback.add_css_class("mango-music-fallback");

    // The icon is the base; the (usually opaque) artwork is painted over it.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&fallback));
    overlay.add_overlay(&cover.area);
    // `GtkOverlay` only measures its main child, so without this the overlay
    // would be only as big as the small fallback icon while the (larger)
    // drawing area hangs out of it — painting the artwork straight over the
    // neighbouring title. Reserving the cover's own size keeps the label clear.
    overlay.set_size_request(width, height);
    overlay.add_css_class("mango-music-cover");

    (overlay, cover)
}

pub struct MusicModel {
    /// The current snapshot, or `None` while nothing is playing.
    track: Option<Track>,
    /// The channel to the poller thread. Only written (never read) here: it is
    /// held so the thread outlives `init`, like the bar's controller fields.
    #[allow(dead_code)]
    commands: Option<Sender<Command>>,
    /// Channel driving the cover-download thread.
    art: Option<Sender<ArtRequest>>,
    /// The `art` URL we have requested (or already loaded), so a poll that
    /// changes nothing does not re-download the same cover.
    cover_url: Option<String>,
    /// Increments on every new cover request and tags the reply.
    cover_token: u64,
    /// True while the seek slider is being set from the model, so the
    /// resulting `value-changed` is ignored.
    syncing: Rc<Cell<bool>>,
    /// True while the user is scrubbing the seek slider; poll updates then
    /// leave the handle alone instead of fighting the drag.
    scrub: Rc<Cell<bool>>,
    /// The fraction the user last seeked to, and when, while the player has
    /// not confirmed it. Some players (Firefox while paused) report a stale or
    /// zero position for a moment after a seek; this keeps the handle where the
    /// user put it until the player agrees.
    pending_seek: Rc<Cell<Option<(f64, Instant)>>>,

    popover: gtk::Popover,

    // Bar widgets.
    bar_cover: Cover,
    bar_label: gtk::Label,

    // Popover widgets.
    pop_cover: Cover,
    pop_title: gtk::Label,
    pop_artist: gtk::Label,
    pop_album: gtk::Label,
    pop_player: gtk::Label,
    play_button: gtk::Button,
    play_icon: gtk::Image,
    prev_button: gtk::Button,
    next_button: gtk::Button,
    seek: gtk::Scale,
    pos_label: gtk::Label,
    len_label: gtk::Label,
}

#[relm4::component(pub)]
impl SimpleComponent for MusicModel {
    type Init = ();
    type Input = MusicInput;
    type Output = ();

    view! {
        #[root]
        // The pill is the component's own root, like the tray and the
        // visualizer, so hiding it while nothing plays also hides the pill.
        gtk::Box {
            set_css_classes: &["mango-group", "mango-music"],
            set_orientation: gtk::Orientation::Horizontal,
            set_valign: gtk::Align::Center,
            #[watch]
            set_visible: model.track.is_some(),

            #[name = "button"]
            gtk::MenuButton {
                set_css_classes: &["mango-music-button"],
                set_always_show_arrow: false,
                #[watch]
                set_tooltip_text: Some(&model.tooltip()),
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let commands = spawn_poller(sender.clone());
        let art = spawn_art(sender.clone());

        // --- The bar content: cover + artist — title. -------------------
        let (bar_overlay, bar_cover) = build_cover(BAR_COVER, BAR_COVER, 0.0);
        let bar_label = gtk::Label::new(None);
        bar_label.add_css_class("mango-music-label");
        bar_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        bar_label.set_max_width_chars(22);
        bar_label.set_xalign(0.0);

        let bar_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar_content.set_valign(gtk::Align::Center);
        bar_content.append(&bar_overlay);
        bar_content.append(&bar_label);

        // --- The popover: artwork, details, transport, progress. --------
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("mango-applet");
        content.add_css_class("mango-music-menu");

        let (pop_overlay, pop_cover) = build_cover(POP_COVER, POP_COVER, 0.0);

        let pop_title = gtk::Label::new(None);
        pop_title.add_css_class("mango-music-title");
        pop_title.set_xalign(0.0);
        pop_title.set_halign(gtk::Align::Start);
        pop_title.set_wrap(true);
        pop_title.set_max_width_chars(22);

        let pop_artist = gtk::Label::new(None);
        pop_artist.add_css_class("mango-music-artist");
        pop_artist.set_xalign(0.0);
        pop_artist.set_halign(gtk::Align::Start);
        pop_artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
        pop_artist.set_max_width_chars(24);

        let pop_album = gtk::Label::new(None);
        pop_album.add_css_class("mango-music-album");
        pop_album.set_xalign(0.0);
        pop_album.set_halign(gtk::Align::Start);
        pop_album.set_ellipsize(gtk::pango::EllipsizeMode::End);
        pop_album.set_max_width_chars(24);

        let pop_player = gtk::Label::new(None);
        pop_player.add_css_class("mango-music-player");
        pop_player.set_xalign(0.0);
        pop_player.set_halign(gtk::Align::Start);

        let info = gtk::Box::new(gtk::Orientation::Vertical, 3);
        info.set_valign(gtk::Align::Center);
        info.set_hexpand(true);
        info.append(&pop_title);
        info.append(&pop_artist);
        info.append(&pop_album);
        info.append(&pop_player);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.append(&pop_overlay);
        header.append(&info);
        content.append(&header);

        // Transport buttons.
        let prev_button = icon_button("media-skip-backward-symbolic", "Previous");
        let next_button = icon_button("media-skip-forward-symbolic", "Next");

        let play_button = gtk::Button::new();
        play_button.add_css_class("mango-music-play");
        let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
        play_button.set_child(Some(&play_icon));

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        controls.set_halign(gtk::Align::Center);
        controls.append(&prev_button);
        controls.append(&play_button);
        controls.append(&next_button);
        content.append(&controls);

        // Progress: position, seek slider, length.
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, SEEK_RANGE, 1.0);
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        seek.set_size_request(200, -1);
        seek.add_css_class("mango-slider");
        seek.set_sensitive(false);

        let pos_label = gtk::Label::new(Some("--:--"));
        pos_label.add_css_class("mango-music-time");
        pos_label.set_width_chars(5);
        pos_label.set_xalign(1.0);

        let len_label = gtk::Label::new(Some("--:--"));
        len_label.add_css_class("mango-music-time");
        len_label.set_width_chars(5);
        len_label.set_xalign(0.0);

        let progress = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        progress.set_valign(gtk::Align::Center);
        progress.append(&pos_label);
        progress.append(&seek);
        progress.append(&len_label);
        content.append(&progress);

        popover.set_child(Some(&content));
        crate::anim::pop_in(&popover);

        let syncing = Rc::new(Cell::new(false));
        let scrub = Rc::new(Cell::new(false));
        let pending_seek: Rc<Cell<Option<(f64, Instant)>>> = Rc::new(Cell::new(None));

        let model = MusicModel {
            track: None,
            commands: Some(commands.clone()),
            art: Some(art),
            cover_url: None,
            cover_token: 0,
            syncing: syncing.clone(),
            scrub: scrub.clone(),
            pending_seek: pending_seek.clone(),
            popover: popover.clone(),
            bar_cover,
            bar_label,
            pop_cover,
            pop_title,
            pop_artist,
            pop_album,
            pop_player,
            play_button: play_button.clone(),
            play_icon,
            prev_button: prev_button.clone(),
            next_button: next_button.clone(),
            seek: seek.clone(),
            pos_label,
            len_label,
        };

        let widgets = view_output!();
        widgets.button.set_child(Some(&bar_content));
        widgets.button.set_popover(Some(&popover));

        // --- Wire the transport controls. ------------------------------
        play_button.connect_clicked({
            let commands = commands.clone();
            move |_| {
                let _ = commands.send(Command::PlayPause);
            }
        });
        prev_button.connect_clicked({
            let commands = commands.clone();
            move |_| {
                let _ = commands.send(Command::Previous);
            }
        });
        next_button.connect_clicked({
            let commands = commands.clone();
            move |_| {
                let _ = commands.send(Command::Next);
            }
        });

        // Scrubbing the slider seeks, debounced so a drag sends one call.
        let scrub_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        seek.connect_value_changed({
            let syncing = syncing.clone();
            let scrub = scrub.clone();
            let scrub_timer = scrub_timer.clone();
            let pending_seek = pending_seek.clone();
            let commands = commands.clone();
            move |scale| {
                if syncing.get() {
                    return;
                }
                scrub.set(true);
                let fraction = scale.value() / SEEK_RANGE;
                if let Some(timer) = scrub_timer.borrow_mut().take() {
                    timer.remove();
                }
                let commands = commands.clone();
                let scrub = scrub.clone();
                let timer_slot = scrub_timer.clone();
                let pending_seek = pending_seek.clone();
                let timer = glib::timeout_add_local_once(SEEK_DEBOUNCE, move || {
                    // The one-shot has fired, so glib has destroyed its source:
                    // drop the id here, or the next change calls `remove` on a
                    // dead source and glib panics.
                    timer_slot.borrow_mut().take();
                    scrub.set(false);
                    // Remember where the user dropped the handle so a player
                    // that reports a stale/zero position right after the seek
                    // does not yank it back before it catches up.
                    pending_seek.set(Some((fraction, Instant::now())));
                    let _ = commands.send(Command::Seek(fraction));
                });
                *scrub_timer.borrow_mut() = Some(timer);
            }
        });

        // Scrolling the bar widget skips tracks: up = previous, down = next.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let commands = commands.clone();
            move |_, _dx, dy| {
                if dy < 0.0 {
                    let _ = commands.send(Command::Previous);
                } else if dy > 0.0 {
                    let _ = commands.send(Command::Next);
                }
                glib::Propagation::Stop
            }
        });
        root.add_controller(scroll);

        model.sync();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            MusicInput::Update(track) => {
                // A different track (or none) invalidates any pending seek.
                let same = match (&self.track, &track) {
                    (Some(old), Some(new)) => old.bus == new.bus && old.title == new.title,
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    self.pending_seek.set(None);
                }
                self.track = track;
                self.sync_cover();
                // A player that went away should not leave its popover open.
                if self.track.is_none() {
                    self.popover.popdown();
                }
                self.sync();
            }
            MusicInput::Cover { token, bytes } => {
                if token != self.cover_token {
                    return;
                }
                let Some(pixbuf) = decode_pixbuf(&bytes) else {
                    return;
                };
                self.bar_cover.set(Some(pixbuf.clone()));
                self.pop_cover.set(Some(pixbuf));
            }
            MusicInput::CoverFailed { token } => {
                // Keep the fallback glyph; nothing to do unless a newer track
                // has already replaced this request.
                let _ = token;
            }
        }
    }
}

impl MusicModel {
    /// Tooltip for the bar widget.
    fn tooltip(&self) -> String {
        match &self.track {
            Some(track) => track.tooltip(),
            None => "Now playing".to_string(),
        }
    }

    /// Request the current track's artwork when the URL changed, clearing the
    /// old covers meanwhile.
    fn sync_cover(&mut self) {
        let url = self.track.as_ref().and_then(|track| track.art.clone());
        if url == self.cover_url {
            return;
        }
        self.cover_url = url.clone();
        self.bar_cover.set(None);
        self.pop_cover.set(None);

        let Some(url) = url else { return };
        self.cover_token += 1;
        if let Some(art) = &self.art {
            let _ = art.send(ArtRequest {
                token: self.cover_token,
                url,
            });
        }
    }

    /// Push the current track into every widget.
    fn sync(&self) {
        let Some(track) = &self.track else {
            self.bar_label.set_text("");
            self.seek.set_sensitive(false);
            return;
        };

        self.bar_label.set_text(&track.bar_text());

        self.pop_title.set_text(if track.title.is_empty() {
            "Unknown track"
        } else {
            &track.title
        });
        self.pop_artist.set_text(&track.artist);
        self.pop_album.set_text(&track.album);
        self.pop_player.set_text(&track.player);

        let (play_icon, play_tip) = if track.playing {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        self.play_icon.set_icon_name(Some(play_icon));
        self.play_button.set_tooltip_text(Some(play_tip));
        self.prev_button.set_sensitive(track.can_prev);
        self.next_button.set_sensitive(track.can_next);

        // Progress. The slider is only moved when the user is not scrubbing it;
        // a just-issued seek also overrides the player until it catches up.
        let length = track.length.filter(|length| *length > 0);
        let polled = length
            .map(|length| (track.position as f64 / length as f64).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let target = self.pending_fraction(polled, track.playing);
        let shown = target.unwrap_or(polled);
        if !self.scrub.get() {
            self.syncing.set(true);
            self.seek.set_value(shown * SEEK_RANGE);
            self.syncing.set(false);
        }
        self.seek.set_sensitive(track.can_seek && length.is_some());

        let shown_position = match (target, length) {
            (Some(fraction), Some(length)) => (fraction * length as f64).round() as i64,
            _ => track.position,
        };
        self.pos_label.set_text(&format_duration(shown_position));
        self.len_label.set_text(
            &length
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string()),
        );
    }

    /// The fraction the seek bar should show, given the player's reported
    /// `polled` fraction. Returns the user's seek target as long as the player
    /// has not caught up to it: some players (Firefox while paused) report a
    /// stale or zero position for a while after a seek. While playing, that
    /// patience is capped so a failed seek still falls back to the player.
    fn pending_fraction(&self, polled: f64, playing: bool) -> Option<f64> {
        let (target, at) = self.pending_seek.get()?;
        if playing && ((polled - target).abs() <= SEEK_TOLERANCE || at.elapsed() >= SEEK_CONFIRM) {
            self.pending_seek.set(None);
            return None;
        }
        Some(target)
    }
}

/// A bare, round icon button with a tooltip (the applet look).
fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-applet-icon-button");
    button.set_tooltip_text(Some(tooltip));
    button.set_child(Some(&gtk::Image::from_icon_name(icon)));
    button
}

/// `mm:ss`, or `h:mm:ss` for a track past the hour.
fn format_duration(micros: i64) -> String {
    let total = (micros.max(0) / 1_000_000) as u64;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Drive the widget from a single background thread.
///
/// The thread owns the session-bus connection and, on every tick, lists the
/// MPRIS players, picks the most interesting one (playing beats having a
/// track) and reads its properties. It only wakes the UI when the snapshot
/// actually changed. Transport commands from the UI are serviced between
/// ticks; the thread exits as soon as the widget drops its sender.
fn spawn_poller(sender: ComponentSender<MusicModel>) -> Sender<Command> {
    spawn_track_poller(move |track| sender.input(MusicInput::Update(track)))
}

/// The poller behind [`spawn_poller`], with the UI calls handed to `sink` so
/// other views can run it against their own input enum — the dashboard's
/// now-playing card sends its snapshots this way. The returned channel carries
/// transport commands; the thread services them against whatever it last read,
/// then re-reads the bus. The thread exits as soon as the caller drops `tx`.
pub(crate) fn spawn_track_poller(
    sink: impl Fn(Option<Track>) + Send + 'static,
) -> Sender<Command> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let Some(mut conn) = connect(&rx) else {
            return;
        };
        let mut last: Option<Track> = None;

        loop {
            // Service every queued command first, then re-read, so a button
            // press is reflected as soon as possible.
            let wait = poll_interval(last.as_ref());
            let mut commands = Vec::new();
            match rx.recv_timeout(wait) {
                Ok(command) => commands.push(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(command) = rx.try_recv() {
                commands.push(command);
            }
            for command in &commands {
                if let Some(track) = last.as_ref() {
                    run_command(&conn, track, command);
                }
            }

            // `None` here means the snapshot could not be read at all (the bus
            // or the connection is gone), as opposed to `Some(None)` — nothing
            // playing. Reconnect on failure and hide the widget meanwhile.
            let current = match snapshot(&conn) {
                Ok(current) => current,
                Err(_) => {
                    if last.is_some() {
                        sink(None);
                        last = None;
                    }
                    std::thread::sleep(RETRY);
                    match connect(&rx) {
                        Some(fresh) => conn = fresh,
                        None => return,
                    }
                    continue;
                }
            };

            if current != last {
                sink(current.clone());
                last = current;
            }
        }
    });

    tx
}

/// How long to wait before the next poll, based on what is playing.
fn poll_interval(last: Option<&Track>) -> Duration {
    match last {
        Some(track) if track.playing => PLAYING_POLL,
        Some(_) => IDLE_POLL,
        None => EMPTY_POLL,
    }
}

/// Open the session bus, retrying until it is available or the channel dies.
fn connect(rx: &mpsc::Receiver<Command>) -> Option<Connection> {
    loop {
        if let Ok(conn) = Connection::session() {
            return Some(conn);
        }
        if let Err(RecvTimeoutError::Disconnected) = rx.recv_timeout(RETRY) {
            return None;
        }
    }
}

/// Read the best current player, or `Err` if the bus could not be reached.
fn snapshot(conn: &Connection) -> zbus::Result<Option<Track>> {
    let bus = DBusProxy::new(conn)?;
    let mut best: Option<(u32, Track)> = None;

    for name in bus.list_names()? {
        let name = name.as_str();
        let Some(suffix) = name.strip_prefix(MPRIS_PREFIX) else {
            continue;
        };
        if suffix.is_empty() {
            continue;
        }
        let Some(track) = read_player(conn, name) else {
            continue;
        };
        // Prefer a player that is actually playing, then one with a title.
        let score = u32::from(track.playing) * 2 + u32::from(!track.title.is_empty());
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, track));
        }
    }

    Ok(best.map(|(_, track)| track))
}

/// Read one player's properties into a [`Track`], or `None` if it has nothing
/// to show (no metadata, or stopped with no track).
fn read_player(conn: &Connection, name: &str) -> Option<Track> {
    let player = Proxy::new(conn, name, PLAYER_PATH, PLAYER_INTERFACE).ok()?;

    let metadata: HashMap<String, OwnedValue> = player.get_property("Metadata").ok()?;
    let status: String = player.get_property("PlaybackStatus").unwrap_or_default();
    let playing = status == "Playing";

    let title = meta(&metadata, "xesam:title").unwrap_or_default();
    if title.is_empty() && !playing {
        return None;
    }
    let artist = meta(&metadata, "xesam:artist")
        .or_else(|| meta(&metadata, "xesam:albumArtist"))
        .unwrap_or_default();
    let album = meta(&metadata, "xesam:album").unwrap_or_default();
    let art = meta(&metadata, "mpris:artUrl");
    let length = metadata.get("mpris:length").and_then(value_to_i64);

    let position = player.get_property::<i64>("Position").unwrap_or(0);
    let can_next = player.get_property::<bool>("CanGoNext").unwrap_or(false);
    let can_prev = player
        .get_property::<bool>("CanGoPrevious")
        .unwrap_or(false);
    let can_seek = player.get_property::<bool>("CanSeek").unwrap_or(false);

    let player_name = Proxy::new(conn, name, PLAYER_PATH, ROOT_INTERFACE)
        .ok()
        .and_then(|root| root.get_property::<String>("Identity").ok())
        .filter(|identity| !identity.is_empty())
        .unwrap_or_else(|| name.strip_prefix(MPRIS_PREFIX).unwrap_or(name).to_string());

    Some(Track {
        bus: name.to_string(),
        player: player_name,
        title,
        artist,
        album,
        art,
        playing,
        length,
        position,
        can_next,
        can_prev,
        can_seek,
    })
}

/// Run one transport command against the player in `track`.
fn run_command(conn: &Connection, track: &Track, command: &Command) {
    let Ok(player) = Proxy::new(conn, track.bus.as_str(), PLAYER_PATH, PLAYER_INTERFACE) else {
        return;
    };
    match command {
        Command::PlayPause => {
            let _ = player.call::<_, _, ()>("PlayPause", &());
        }
        Command::Next => {
            let _ = player.call::<_, _, ()>("Next", &());
        }
        Command::Previous => {
            let _ = player.call::<_, _, ()>("Previous", &());
        }
        Command::Seek(fraction) => {
            // MPRIS only offers a relative `Seek`, so turn the requested
            // fraction of the track into the offset from the current playhead.
            let Some(length) = track.length.filter(|length| *length > 0) else {
                return;
            };
            // `track.position` is up to a poll interval stale, and the offset
            // is measured from where the player is *now*, so read the live
            // playhead; fall back to the snapshot if the player won't say.
            let position = player
                .get_property::<i64>("Position")
                .unwrap_or(track.position);
            let target = (fraction.clamp(0.0, 1.0) * length as f64).round() as i64;
            let offset = target - position;
            let _ = player.call::<_, _, ()>("Seek", &(offset,));
        }
    }
}

/// Fetch cover art on a background thread.
fn spawn_art(sender: ComponentSender<MusicModel>) -> Sender<ArtRequest> {
    let (tx, rx) = mpsc::channel::<ArtRequest>();

    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let bytes = fetch_art(&request.url);
            let message = match bytes {
                Some(bytes) => MusicInput::Cover {
                    token: request.token,
                    bytes,
                },
                None => MusicInput::CoverFailed {
                    token: request.token,
                },
            };
            sender.input(message);
        }
    });

    tx
}

/// Fetch a cover: a local `file://` file, or an `http(s)` URL via `curl`.
fn fetch_art(url: &str) -> Option<Vec<u8>> {
    // Players normally give a `file://` URL, but a bare absolute path turns up
    // too. Either way it is a local read.
    let local = file_url_path(url).or_else(|| url.starts_with('/').then(|| url.to_string()));
    if let Some(path) = local {
        return std::fs::read(path).ok().filter(|bytes| !bytes.is_empty());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let output = std::process::Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "10",
                "--max-filesize",
                &MAX_ART_BYTES.to_string(),
                "--",
                url,
            ])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        return output
            .status
            .success()
            .then_some(output.stdout)
            .filter(|bytes| !bytes.is_empty());
    }
    None
}

/// Turn a `file://` URL into a local path, or `None` for other schemes.
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // `file:///path` has an empty host; `file://localhost/path` names one.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(index) => &rest[index..],
        None => return None,
    };
    Some(percent_decode(path))
}

/// Decode `%XX` escapes in a URL path (no `+`-as-space: that is form encoding,
/// not file URLs).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode image bytes into a pixbuf, bounded so a huge poster does not cost
/// more than the widget needs (the drawing code cover-fits it down anyway).
fn decode_pixbuf(bytes: &[u8]) -> Option<Pixbuf> {
    let pixbuf = Pixbuf::from_read(Cursor::new(bytes.to_vec())).ok()?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let longest = width.max(height);
    if longest <= MAX_COVER_PX {
        return Some(pixbuf);
    }
    let scale = f64::from(MAX_COVER_PX) / f64::from(longest);
    let scaled_width = (f64::from(width) * scale).round().max(1.0) as i32;
    let scaled_height = (f64::from(height) * scale).round().max(1.0) as i32;
    pixbuf.scale_simple(scaled_width, scaled_height, InterpType::Bilinear)
}

/// Paint `pixbuf` into `width` x `height`, scaled to cover and centre-cropped,
/// clipped to rounded corners.
fn draw_cover(cr: &cairo::Context, pixbuf: &Pixbuf, width: i32, height: i32, radius: f64) {
    let image_width = f64::from(pixbuf.width());
    let image_height = f64::from(pixbuf.height());
    let width = f64::from(width);
    let height = f64::from(height);
    if image_width <= 0.0 || image_height <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }

    let scale = (width / image_width).max(height / image_height);
    let draw_width = image_width * scale;
    let draw_height = image_height * scale;
    let x = (width - draw_width) / 2.0;
    let y = (height - draw_height) / 2.0;

    let _ = cr.save();
    rounded_rect(cr, 0.0, 0.0, width, height, radius);
    cr.clip();
    cr.translate(x, y);
    cr.scale(scale, scale);
    cr.set_source_pixbuf(pixbuf, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -pi / 2.0, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        pi / 2.0,
    );
    cr.arc(x + radius, y + height - radius, radius, pi / 2.0, pi);
    cr.arc(x + radius, y + radius, radius, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

/// Pull a metadata key out as a string, joining string arrays (the spec uses
/// `as` for `xesam:artist` and `s` for `xesam:title`).
fn meta(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|value| value_to_string(value))
        .filter(|text| !text.is_empty())
}

/// Best-effort string view of a D-Bus value.
fn value_to_string(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(text) => Some(text.to_string()),
        Value::Array(array) => {
            let parts: Vec<String> = array
                .iter()
                .filter_map(|value| value_to_string(value))
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

/// Best-effort integer view of a D-Bus value (`mpris:length` is usually `x`).
fn value_to_i64(value: &OwnedValue) -> Option<i64> {
    match &**value {
        Value::U8(number) => Some(i64::from(*number)),
        Value::I16(number) => Some(i64::from(*number)),
        Value::U16(number) => Some(i64::from(*number)),
        Value::I32(number) => Some(i64::from(*number)),
        Value::U32(number) => Some(i64::from(*number)),
        Value::I64(number) => Some(*number),
        Value::U64(number) => i64::try_from(*number).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `OwnedValue` from a plain `Value`, the shape a D-Bus reply
    /// hands back (there is no `From<String>` for `OwnedValue`).
    fn owned(value: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(value).expect("a Value should be owned")
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(42_000_000), "0:42");
        assert_eq!(format_duration(195_000_000), "3:15");
        assert_eq!(format_duration(3_723_000_000), "1:02:03");
        // A negative position (a glitch) reads as zero rather than underflow.
        assert_eq!(format_duration(-5), "0:00");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2Ftmp%2Fx"), "/tmp/x");
        // A malformed escape is left alone.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn extracts_local_file_paths() {
        assert_eq!(
            file_url_path("file:///home/me/a%20b/cover.jpg").as_deref(),
            Some("/home/me/a b/cover.jpg")
        );
        assert_eq!(
            file_url_path("file://localhost/tmp/x.png").as_deref(),
            Some("/tmp/x.png")
        );
        assert_eq!(file_url_path("https://example.com/x.png"), None);
    }

    #[test]
    fn reads_metadata_strings_and_arrays() {
        let mut map = HashMap::new();
        map.insert("xesam:title".to_string(), owned(Value::from("Song")));
        map.insert(
            "xesam:artist".to_string(),
            owned(Value::from(vec!["A", "B"])),
        );
        assert_eq!(meta(&map, "xesam:title").as_deref(), Some("Song"));
        assert_eq!(meta(&map, "xesam:artist").as_deref(), Some("A, B"));
        assert_eq!(meta(&map, "xesam:album"), None);
    }

    #[test]
    fn reads_microsecond_lengths() {
        assert_eq!(
            value_to_i64(&OwnedValue::from(195_000_000_i64)),
            Some(195_000_000)
        );
        assert_eq!(value_to_i64(&OwnedValue::from(7_u32)), Some(7));
        assert_eq!(value_to_i64(&owned(Value::from("nope"))), None);
    }

    #[test]
    fn bar_text_falls_back_to_the_player() {
        let track = Track {
            player: "Spotify".to_string(),
            ..Track::default()
        };
        assert_eq!(track.bar_text(), "Spotify");
    }
}
