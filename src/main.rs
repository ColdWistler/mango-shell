//! A Mango (mangowm) status bar written with Relm4 + GTK4.
//!
//! The structure deliberately mirrors the one used by
//! [OkShell](https://github.com/JohnOberhauser/OkShell):
//!
//! ```text
//! Shell                      (main.rs)   one invisible 1x1 layer surface,
//!   |                                    watches monitors + owns the IPC feed
//!   +-- BarModel             (bar.rs)    one real layer surface per monitor
//!         +-- TagsModel      (widgets/)  clickable tag buttons
//!         +-- ClockModel     (widgets/)  clock + date
//! ```
//!
//! Why the invisible root? A GTK application needs a window to stay alive, and
//! layer surfaces belong to a single monitor, so the "shell" itself cannot be
//! one surface. OkShell solves this exactly the same way.

mod bar;
mod mango;
mod widgets;

use std::collections::HashMap;

use bar::{BarInit, BarInput, BarModel};
use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::prelude::*;

use mango::TagView;

/// The shell owns one bar per monitor and coordinates the IPC feed.
struct Shell {
    bars: HashMap<String, Controller<BarModel>>,
}

#[derive(Debug)]
enum ShellInput {
    /// Re-read the monitor list from GDK.
    SyncMonitors,
    /// Spawn a bar for the monitor with this connector name.
    AddBar(String),
    /// Tear down the bar for this connector.
    RemoveBar(String),
    /// Fresh tag state, keyed by monitor connector name.
    MangoUpdate(HashMap<String, Vec<TagView>>),
}

#[relm4::component(pub)]
impl SimpleComponent for Shell {
    type Init = ();
    type Input = ShellInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_decorated: false,
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The stylesheet needs a live GDK display, so it is loaded here rather
        // than in `main`, where the display does not exist yet.
        relm4::set_global_css(include_str!("style.css"));

        // The root is never meant to be seen: a 1x1 background surface that
        // only exists to keep the application alive.
        root.init_layer_shell();
        root.set_layer(Layer::Background);
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Left, true);
        root.set_default_size(1, 1);
        root.set_namespace(Some("mango-shell-root"));
        root.set_visible(false);

        let model = Shell {
            bars: HashMap::new(),
        };

        let widgets = view_output!();

        setup_monitor_watcher(&sender);
        sync_monitors(&model.bars, &sender);
        mango::spawn_watcher(sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            ShellInput::SyncMonitors => sync_monitors(&self.bars, &sender),

            ShellInput::AddBar(name) => {
                if self.bars.contains_key(&name) {
                    return;
                }
                let Some(monitor) = find_monitor(&name) else {
                    return;
                };
                let controller = BarModel::builder()
                    .launch(BarInit {
                        name: name.clone(),
                        monitor,
                    })
                    .detach();
                self.bars.insert(name, controller);
            }

            ShellInput::RemoveBar(name) => {
                // Dropping the controller shuts the component down and closes
                // its layer surface.
                if let Some(controller) = self.bars.remove(&name) {
                    controller.widget().set_visible(false);
                }
            }

            ShellInput::MangoUpdate(update) => {
                for (name, tags) in update {
                    if let Some(controller) = self.bars.get(&name) {
                        controller.emit(BarInput::SetTags(tags));
                    }
                }
            }
        }
    }
}

/// Watch GDK for monitor hotplug events.
///
/// The display owns its monitor list, so the signal handler keeps working even
/// though we drop our local reference here.
fn setup_monitor_watcher(sender: &ComponentSender<Shell>) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let monitors = display.monitors();
    let sender = sender.clone();
    monitors.connect_items_changed(move |_, _, _, _| {
        sender.input(ShellInput::SyncMonitors);
    });
}

/// Diff the connected monitors against the bars we currently have.
fn sync_monitors(bars: &HashMap<String, Controller<BarModel>>, sender: &ComponentSender<Shell>) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let monitors = display.monitors();

    let mut present = Vec::new();
    for index in 0..monitors.n_items() {
        let Some(object) = monitors.item(index) else {
            continue;
        };
        let Ok(monitor) = object.downcast::<gdk::Monitor>() else {
            continue;
        };
        // The connector name (e.g. "DP-1") is what Mango uses in its IPC too.
        let Some(name) = monitor.connector().map(|connector| connector.to_string()) else {
            continue;
        };

        present.push(name.clone());
        if !bars.contains_key(&name) {
            sender.input(ShellInput::AddBar(name));
        }
    }

    for name in bars.keys() {
        if !present.contains(name) {
            sender.input(ShellInput::RemoveBar(name.clone()));
        }
    }
}

fn find_monitor(name: &str) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    for index in 0..monitors.n_items() {
        let Some(object) = monitors.item(index) else {
            continue;
        };
        let Ok(monitor) = object.downcast::<gdk::Monitor>() else {
            continue;
        };
        if monitor.connector().as_deref() == Some(name) {
            return Some(monitor);
        }
    }
    None
}

fn main() {
    let app = RelmApp::new("com.example.MangoShell");
    app.run::<Shell>(());
}
