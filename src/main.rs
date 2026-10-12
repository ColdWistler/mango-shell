//! A Mango (mangowm) status bar written with Relm4 + GTK4.
//!
//! The structure deliberately mirrors the one used by
//! [OkShell](https://github.com/JohnOberhauser/OkShell):
//!
//! ```text
//! Shell                      (main.rs)     one invisible 1x1 layer surface,
//!   |                                      watches monitors + owns the IPC feed
//!   +-- BarModel             (bar.rs)      one real layer surface per monitor
//!   |     +-- TagsModel      (widgets/)    clickable tag buttons
//!   |     +-- ClockModel     (widgets/)    clock + date
//!   +-- WallpaperModel       (wallpaper.rs) one background surface per monitor
//!   +-- LauncherModel        (launcher.rs) centred overlay app launcher,
//!   |                                      toggled with SIGUSR1
//!   +-- WallpaperPickerModel (wallpaper.rs) centred overlay wallpaper grid,
//!                                          toggled with SIGUSR2
//! ```
//!
//! Why the invisible root? A GTK application needs a window to stay alive, and
//! layer surfaces belong to a single monitor, so the "shell" itself cannot be
//! one surface. OkShell solves this exactly the same way.

mod bar;
mod launcher;
mod mango;
mod wallpaper;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;

use bar::{BarInit, BarInput, BarModel, BarOutput};
use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use launcher::{LauncherInput, LauncherModel};
use relm4::prelude::*;
use wallpaper::{
    WallpaperInit, WallpaperInput, WallpaperModel, WallpaperOutput, WallpaperPickerInput,
    WallpaperPickerModel,
};

use mango::TagView;

/// The shell owns one bar and one wallpaper surface per monitor, the app
/// launcher, the wallpaper picker, and coordinates the IPC feed.
struct Shell {
    bars: HashMap<String, Controller<BarModel>>,
    wallpapers: HashMap<String, Controller<WallpaperModel>>,
    launcher: Controller<LauncherModel>,
    picker: Controller<WallpaperPickerModel>,
    /// The wallpaper currently shown, restored from disk at startup.
    current: Option<PathBuf>,
    /// The global stylesheet, kept so a wallpaper change can reload it.
    css: gtk::CssProvider,
}

#[derive(Debug)]
enum ShellInput {
    /// Re-read the monitor list from GDK.
    SyncMonitors,
    /// Spawn a bar and wallpaper surface for the monitor with this connector.
    AddMonitor(String),
    /// Tear down the surfaces for this connector.
    RemoveMonitor(String),
    /// Fresh tag state, keyed by monitor connector name.
    MangoUpdate(HashMap<String, Vec<TagView>>),
    /// Show/hide the application launcher (sent on `SIGUSR1`).
    ToggleLauncher,
    /// Show/hide the wallpaper picker (sent on `SIGUSR2` or the bar button).
    ToggleWallpaper,
    /// Remember and show this wallpaper everywhere.
    SetWallpaper(PathBuf),
    /// Re-read `style.css` after a wallpaper change regenerated it.
    ReloadCss,
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
        //
        // A user stylesheet at ~/.config/mango-shell/style.css is appended on
        // top, so wallpaper-derived `@define-color` overrides (e.g. from
        // matugen) win over the built-in Material 3 defaults. We keep our own
        // provider (rather than `relm4::set_global_css`) so it can be reloaded
        // in place when the wallpaper — and therefore the palette — changes.
        let css = gtk::CssProvider::new();
        css.load_from_data(&load_css());
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        // The root is never meant to be seen: a 1x1 background surface that
        // only exists to keep the application alive.
        root.init_layer_shell();
        root.set_layer(Layer::Background);
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Left, true);
        root.set_default_size(1, 1);
        root.set_namespace(Some("mango-shell-root"));
        root.set_visible(false);

        // The wallpaper is remembered across restarts.
        let current = wallpaper::load_current();

        // The launcher and the picker are single surfaces shared by every
        // monitor. They stay hidden until their toggle arrives.
        let launcher = LauncherModel::builder().launch(()).detach();
        let picker = WallpaperPickerModel::builder()
            .launch(current.clone())
            .forward(sender.input_sender(), |output| match output {
                WallpaperOutput::Apply(path) => ShellInput::SetWallpaper(path),
            });

        let model = Shell {
            bars: HashMap::new(),
            wallpapers: HashMap::new(),
            launcher,
            picker,
            current,
            css,
        };

        let widgets = view_output!();

        setup_monitor_watcher(&sender);
        sync_monitors(&model.bars, &sender);
        mango::spawn_watcher(sender.clone());
        spawn_signal_watcher(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            ShellInput::SyncMonitors => sync_monitors(&self.bars, &sender),

            ShellInput::AddMonitor(name) => {
                if self.bars.contains_key(&name) {
                    return;
                }
                let Some(monitor) = find_monitor(&name) else {
                    return;
                };
                let bar = BarModel::builder()
                    .launch(BarInit {
                        name: name.clone(),
                        monitor: monitor.clone(),
                    })
                    // The bar's buttons toggle the shell's overlays.
                    .forward(sender.input_sender(), |output| match output {
                        BarOutput::ToggleLauncher => ShellInput::ToggleLauncher,
                        BarOutput::ToggleWallpaper => ShellInput::ToggleWallpaper,
                    });
                // One background surface per monitor, showing the current
                // wallpaper (if any).
                let wallpaper = WallpaperModel::builder()
                    .launch(WallpaperInit {
                        monitor,
                        path: self.current.clone(),
                    })
                    .detach();
                self.bars.insert(name.clone(), bar);
                self.wallpapers.insert(name, wallpaper);
            }

            ShellInput::RemoveMonitor(name) => {
                // Dropping the controllers shuts the components down and closes
                // their layer surfaces.
                if let Some(controller) = self.bars.remove(&name) {
                    controller.widget().set_visible(false);
                }
                if let Some(controller) = self.wallpapers.remove(&name) {
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

            ShellInput::ToggleLauncher => self.launcher.emit(LauncherInput::Toggle),

            ShellInput::ToggleWallpaper => self.picker.emit(WallpaperPickerInput::Toggle),

            ShellInput::SetWallpaper(path) => {
                wallpaper::save_current(&path);
                self.current = Some(path.clone());
                // Swap the picture on every monitor...
                for controller in self.wallpapers.values() {
                    controller.emit(WallpaperInput::SetPath(Some(path.clone())));
                }
                // ...and keep the picker's highlight in sync.
                self.picker
                    .emit(WallpaperPickerInput::SetCurrent(path.clone()));
                // Let an optional hook (matugen, pywal, …) re-derive the
                // palette, then reload the stylesheet once it finishes.
                let sender = sender.clone();
                std::thread::spawn(move || {
                    wallpaper::run_hook(&path);
                    let _ = sender.input_sender().send(ShellInput::ReloadCss);
                });
            }

            ShellInput::ReloadCss => self.css.load_from_data(&load_css()),
        }
    }
}

/// Toggle the launcher on `SIGUSR1` and the wallpaper picker on `SIGUSR2`.
///
/// Mango's keybind spawns a command that signals this process, for example:
///
/// ```ini
/// bind=SUPER,d,spawn,pkill -SIGUSR1 mango-shell
/// bind=SUPER,w,spawn,pkill -SIGUSR2 mango-shell
/// ```
///
/// A signal keeps the shell a single instance, unlike spawning a second copy.
fn spawn_signal_watcher(sender: &ComponentSender<Shell>) {
    use signal_hook::consts::{SIGUSR1, SIGUSR2};
    use signal_hook::iterator::Signals;

    let sender = sender.clone();
    std::thread::spawn(move || {
        let Ok(mut signals) = Signals::new([SIGUSR1, SIGUSR2]) else {
            return;
        };
        for signal in signals.forever() {
            match signal {
                SIGUSR2 => sender.input(ShellInput::ToggleWallpaper),
                _ => sender.input(ShellInput::ToggleLauncher),
            }
        }
    });
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
            sender.input(ShellInput::AddMonitor(name));
        }
    }

    for name in bars.keys() {
        if !present.contains(name) {
            sender.input(ShellInput::RemoveMonitor(name.clone()));
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

/// The built-in stylesheet, plus an optional user override appended after it.
///
/// The override lives at `~/.config/mango-shell/style.css` and is meant for
/// `@define-color` tokens, which is how you tint the shell to the wallpaper
/// (the way end4-pC does with matugen). Keeping it as a plain append means no
/// extra provider-priority plumbing.
fn load_css() -> String {
    let mut css = String::from(include_str!("style.css"));

    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config/mango-shell/style.css");
        if let Ok(extra) = std::fs::read_to_string(&path) {
            css.push('\n');
            css.push_str(&extra);
        }
    }

    css
}

fn main() {
    let app = RelmApp::new("com.example.MangoShell");
    app.run::<Shell>(());
}
