//! Thin wrapper around Mango's IPC.
//!
//! `mango-ipc` talks to the compositor directly over the Unix socket whose
//! path is published in the `MANGO_INSTANCE_SIGNATURE` environment variable,
//! so we never have to shell out to the `mmsg` CLI.
//!
//! Mango's IPC is blocking, so all of it lives on a background thread and
//! communicates with the UI through the shell's message channel
//! (`ComponentSender::input` is safe to call from any thread; the message is
//! processed on the GTK main thread).

use std::collections::HashMap;
use std::thread;

use mango_ipc::{BlockingSocket, IntoArgs as _, dispatch, query, request, response};
use relm4::ComponentSender;

use crate::{Shell, ShellInput};

/// The small, UI-friendly subset of a tag that the bar cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagView {
    pub index: u32,
    pub active: bool,
    pub urgent: bool,
    pub count: u32,
}

/// Stream compositor state into the shell.
pub fn spawn_watcher(sender: ComponentSender<Shell>) {
    thread::spawn(move || {
        let Ok(socket) = BlockingSocket::try_default() else {
            // Not running inside a Mango session: the bar still works,
            // it just won't show tags.
            return;
        };

        // Prime the UI with the current state...
        if let Ok(monitors) = socket.get(query::AllMonitors) {
            sender.input(ShellInput::MangoUpdate(tags_by_monitor(&monitors)));
        }

        // ...then stream every change.
        let Ok(responses) = socket.watch(query::AllMonitors) else {
            return;
        };

        for response in responses {
            match response {
                Ok(monitors) => sender.input(ShellInput::MangoUpdate(tags_by_monitor(&monitors))),
                Err(_) => break,
            }
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

/// Convert a compositor response into tags keyed by monitor connector name.
fn tags_by_monitor(monitors: &response::AllMonitors) -> HashMap<String, Vec<TagView>> {
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
            (monitor.name.clone(), tags)
        })
        .collect()
}
