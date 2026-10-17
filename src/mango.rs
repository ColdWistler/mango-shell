//! Thin wrapper around Mango's IPC.
//!
//! `mango-ipc` talks to the compositor directly over the Unix socket whose
//! path is published in the `MANGO_INSTANCE_SIGNATURE` environment variable,
//! so we never have to shell out to the `mmsg` CLI.
//!
//! Mango's IPC is blocking, so all of it lives on background threads and
//! communicates with the UI through the shell's message channel
//! (`ComponentSender::input` is safe to call from any thread; the message is
//! processed on the GTK main thread).
//!
//! Two state feeds run side by side: [`spawn_watcher`] streams monitors (the
//! bar's tag strips), [`spawn_client_watcher`] streams windows (the workspace
//! overview's tiles). They use their own sockets so a slow consumer in one
//! never stalls the other.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use mango_ipc::{BlockingSocket, IntoArgs as _, dispatch, query, request, response};
use mango_ipc::socket::BlockingSocketError;
use relm4::ComponentSender;

use crate::{Shell, ShellInput};

/// How long to wait before re-opening a watch that failed or was closed.
///
/// Long enough not to hammer a compositor that is restarting, short enough
/// that the shell catches up before anyone notices.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// The small, UI-friendly subset of a tag that the bar cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagView {
    pub index: u32,
    pub active: bool,
    pub urgent: bool,
    pub count: u32,
}

/// What the shell knows about one monitor: the tag strip the bar renders,
/// plus the rect the workspace overview scales window tiles into.
#[derive(Debug, Clone)]
pub struct MonitorView {
    /// Whether Mango considers this the focused monitor.
    pub active: bool,
    /// The monitor rect, in compositor layout coordinates.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The monitor's tags, in order (index 1..=len).
    pub tags: Vec<TagView>,
}

/// Everything the overview needs to draw (and act on) one window tile.
#[derive(Debug, Clone)]
pub struct ClientView {
    /// Mango's client id: the handle every client dispatch targets.
    pub id: u32,
    pub title: String,
    /// The Wayland app id, used to look the icon up.
    pub appid: String,
    /// Connector name of the monitor the window is on.
    pub monitor: String,
    /// 1-based tag indices the window sits on.
    pub tags: Vec<u32>,
    /// The window rect, in compositor layout coordinates.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub is_focused: bool,
    pub is_floating: bool,
    /// Pinned to every tag.
    pub is_global: bool,
}

/// Stream monitor state (tags, focus) into the shell.
///
/// `watch` is a single connection, and it ends: the compositor can close it,
/// a line can fail to parse, the socket can hiccup. A thread that gave up
/// there would freeze the bar (and the overview) on the last state it saw
/// for the rest of the session, so every attempt is followed by another one,
/// each re-priming the UI before it listens — a gap in the stream corrects
/// itself on the next connect.
pub fn spawn_watcher(sender: ComponentSender<Shell>) {
    thread::spawn(move || {
        // No `MANGO_INSTANCE_SIGNATURE` means this is not a Mango session:
        // the bar still works, it just won't show tags, and there is nothing
        // to reconnect to.
        if BlockingSocket::try_default().is_err() {
            return;
        }
        loop {
            stream_monitors(&sender);
            thread::sleep(RECONNECT_DELAY);
        }
    });
}

/// Prime once, then stream until the connection ends — however it ends.
fn stream_monitors(sender: &ComponentSender<Shell>) {
    let Ok(socket) = BlockingSocket::try_default() else {
        return;
    };

    // Prime the UI with the current state...
    if let Ok(monitors) = socket.get(query::AllMonitors) {
        sender.input(ShellInput::MangoUpdate(monitors_by_name(&monitors)));
    }

    // ...then stream every change.
    let Ok(responses) = socket.watch(query::AllMonitors) else {
        return;
    };

    for response in responses {
        match response {
            Ok(monitors) => sender.input(ShellInput::MangoUpdate(monitors_by_name(&monitors))),
            Err(error) => {
                eprintln!("mango-shell: monitor watch: {error}");
                // Only an I/O error means the stream itself is gone; the
                // caller re-opens it. A line we cannot parse, or one the
                // compositor flagged as an error, says nothing about the
                // connection — skip it and keep listening.
                if matches!(error, BlockingSocketError::ReceiveResponse(_)) {
                    return;
                }
            }
        }
    }
}

/// Stream window state into the shell — the workspace overview's feed.
///
/// Like [`spawn_watcher`], the feed reconnects instead of dying: a silent
/// end here is what leaves closed windows drawn in the workspace preview.
///
/// Scratchpad / minimized / overlay windows are filtered out here: they are
/// not part of any tag's layout, so the overview never shows them.
pub fn spawn_client_watcher(sender: ComponentSender<Shell>) {
    thread::spawn(move || {
        // Not running inside a Mango session: nothing to stream.
        if BlockingSocket::try_default().is_err() {
            return;
        }
        loop {
            stream_clients(&sender);
            thread::sleep(RECONNECT_DELAY);
        }
    });
}

/// Prime once with the current window list, then stream until the
/// connection ends — however it ends.
fn stream_clients(sender: &ComponentSender<Shell>) {
    let Ok(socket) = BlockingSocket::try_default() else {
        return;
    };

    if let Ok(clients) = socket.get(query::AllClients) {
        sender.input(ShellInput::ClientsUpdate(clients_by_view(&clients)));
    }

    let Ok(responses) = socket.watch(query::AllClients) else {
        return;
    };

    for response in responses {
        match response {
            Ok(clients) => sender.input(ShellInput::ClientsUpdate(clients_by_view(&clients))),
            Err(error) => {
                eprintln!("mango-shell: client watch: {error}");
                if matches!(error, BlockingSocketError::ReceiveResponse(_)) {
                    return;
                }
            }
        }
    }
}

/// Re-read the window list once, out of band.
///
/// The overview draws whatever the shell last received from
/// [`spawn_client_watcher`], so a close that slipped through would leave a
/// preview of a dead window on its card until the next update arrived. This
/// is a fresh `get`, so opening the workspaces view always starts from the
/// compositor's own answer.
pub fn refresh_clients(sender: ComponentSender<Shell>) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            return;
        };
        if let Ok(clients) = socket.get(query::AllClients) {
            sender.input(ShellInput::ClientsUpdate(clients_by_view(&clients)));
        }
    });
}

/// Focus `index` (1-based) on the current monitor.
pub fn switch_tag(index: u32) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            return;
        };
        let Some(tag) = request::Tag::new(index) else {
            return;
        };
        let mask = request::TagMaskOrAllOrPrev::TagMask(request::TagMask::from(tag));
        // `view` takes the tag mask plus a `synctag` flag.
        let _ = socket.dispatch(dispatch::View::new(mask).with_arg(false));
    });
}

/// View `tag` (1-based) and then focus `client`.
///
/// Both requests run on one connection, in order, so the focus cannot land
/// before the tag is visible. This is what clicking a window tile in the
/// workspace overview does.
pub fn jump_to(tag: u32, client: u32) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            return;
        };
        if let Some(tag) = request::Tag::new(tag) {
            let mask = request::TagMaskOrAllOrPrev::TagMask(request::TagMask::from(tag));
            let _ = socket.dispatch(dispatch::View::new(mask).with_arg(false));
        }
        let _ = socket.dispatch_with_client(dispatch::Focusid, Some(client));
    });
}

/// Close `client` (middle-click in the overview).
pub fn kill_client(client: u32) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            return;
        };
        let _ = socket.dispatch_with_client(dispatch::Killclient, Some(client));
    });
}

/// Move `client` to `tag` (1-based), leaving the view alone — the overview's
/// drag-a-window-onto-another-card action.
pub fn move_client_to_tag(client: u32, tag: u32) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            return;
        };
        let Some(tag) = request::Tag::new(tag) else {
            return;
        };
        let mask = request::TagMaskOrAll::TagMask(request::TagMask::from(tag));
        let _ = socket.dispatch_with_client(dispatch::Tag::new(mask).with_arg(false), Some(client));
    });
}

/// Convert a compositor response into monitors keyed by connector name.
fn monitors_by_name(monitors: &response::AllMonitors) -> HashMap<String, MonitorView> {
    monitors
        .monitors
        .iter()
        .map(|monitor| {
            let tags = monitor
                .tags
                .iter()
                .map(|tag| TagView {
                    index: tag.index.get(),
                    active: tag.is_active,
                    urgent: tag.is_urgent,
                    count: tag.client_count,
                })
                .collect();
            let view = MonitorView {
                active: monitor.active,
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
                tags,
            };
            (monitor.name.clone(), view)
        })
        .collect()
}

/// Convert a compositor response into the overview's window list.
fn clients_by_view(all: &response::AllClients) -> Vec<ClientView> {
    all.clients
        .iter()
        .filter(|client| !client.is_scratchpad && !client.is_minimized && !client.is_overlay)
        .filter(|client| client.width > 0 && client.height > 0)
        .map(|client| ClientView {
            id: client.id,
            title: client.title.clone(),
            appid: client.appid.clone(),
            monitor: client.monitor.clone(),
            tags: client.tags.clone(),
            x: client.x,
            y: client.y,
            width: client.width,
            height: client.height,
            is_focused: client.is_focused,
            is_floating: client.is_floating,
            is_global: client.is_global,
        })
        .collect()
}
