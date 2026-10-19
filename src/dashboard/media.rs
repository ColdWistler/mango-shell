//! The dashboard's now-playing card, backed by the same MPRIS poller the bar
//! widget uses.
//!
//! Instead of embedding the bar's widget (its CSS is scoped to the bar
//! window), the card runs the poller with a plain sink: the thread reports
//! track snapshots as [`DashboardInput::Track`], and the transport buttons
//! send [`Command`]s back over the same channel the bar would use. Nothing
//! here talks to D-Bus directly.

use gtk::prelude::*;
use gtk::pango;
use relm4::prelude::*;

use crate::widgets::music::{spawn_track_poller, Command, Track};

use super::{card, DashboardInput, DashboardModel};

/// The dashboard's now-playing card.
pub(super) struct MediaCard {
    root: gtk::Box,
    /// Title + artist, hidden while nothing plays.
    content: gtk::Box,
    title: gtk::Label,
    artist: gtk::Label,
    /// The transport row, hidden alongside the details when the bus is quiet.
    controls: gtk::Box,
    /// Shown in place of the track details when the bus is quiet.
    empty: gtk::Label,
    play: gtk::Button,
    previous: gtk::Button,
    next: gtk::Button,
}

impl MediaCard {
    pub(super) fn new(sender: ComponentSender<DashboardModel>) -> Self {
        let (root, body) = card("Now playing");

        let title = gtk::Label::new(None);
        title.add_css_class("mango-media-title");
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);

        let artist = gtk::Label::new(None);
        artist.add_css_class("mango-card-note");
        artist.set_xalign(0.0);
        artist.set_ellipsize(pango::EllipsizeMode::End);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.append(&title);
        content.append(&artist);
        body.append(&content);

        // --- Transport row. The poller thread owns the D-Bus connection and
        // services these against whatever it last read.
        let commands = spawn_track_poller(move |track| {
            let _ = sender.input(DashboardInput::Track(track));
        });

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.set_halign(gtk::Align::Center);
        controls.add_css_class("mango-transport");

        let previous = transport_button("media-skip-backward-symbolic", "Previous", &commands, Command::Previous);
        let play = transport_button("media-playback-start-symbolic", "Play / pause", &commands, Command::PlayPause);
        let next = transport_button("media-skip-forward-symbolic", "Next", &commands, Command::Next);
        controls.append(&previous);
        controls.append(&play);
        controls.append(&next);
        body.append(&controls);

        let empty = gtk::Label::new(Some("Nothing playing"));
        empty.add_css_class("mango-card-note");
        body.append(&empty);

        // Nothing has reported yet, so start from the empty state.
        content.set_visible(false);
        controls.set_visible(false);

        MediaCard {
            root,
            content,
            title,
            artist,
            controls,
            empty,
            play,
            previous,
            next,
        }
    }

    pub(super) fn root(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Render the latest MPRIS snapshot (`None` = nothing playing, or the bus
    /// could not be read).
    pub(super) fn set(&mut self, track: Option<&Track>) {
        let Some(track) = track else {
            self.content.set_visible(false);
            self.controls.set_visible(false);
            self.empty.set_visible(true);
            return;
        };

        self.content.set_visible(true);
        self.controls.set_visible(true);
        self.empty.set_visible(false);

        self.title
            .set_text(if track.title.is_empty() {
                "Unknown title"
            } else {
                &track.title
            });
        self.artist.set_text(&subtitle(track));

        self.previous.set_sensitive(track.can_prev);
        self.next.set_sensitive(track.can_next);
        self.play.set_icon_name(if track.playing {
            "media-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
    }
}

/// An icon-only transport button wired to the poller's command channel.
fn transport_button(
    icon: &str,
    tooltip: &str,
    commands: &std::sync::mpsc::Sender<Command>,
    command: Command,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-transport-button");
    button.set_tooltip_text(Some(tooltip));

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    button.set_child(Some(&image));

    let commands = commands.clone();
    button.connect_clicked(move |_| {
        let _ = commands.send(command);
    });
    button
}

/// `artist — player`, with fallbacks for players that leave fields blank.
fn subtitle(track: &Track) -> String {
    let artist = if track.artist.is_empty() {
        "Unknown artist"
    } else {
        &track.artist
    };
    format!("{artist} · {}", track.player)
}
