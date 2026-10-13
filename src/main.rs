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
//!   +-- LauncherModel        (launcher.rs)  app launcher, hopped out of the bar
//!   |         + flyout.rs                   with a slide-down panel (SIGUSR1)
//!   +-- WallpaperPickerModel (wallpaper.rs) wallpaper grid, same fly-out panel
//!                                          (SIGUSR2)
//!   +-- ThemePickerModel     (theme.rs)     theme gallery, same fly-out panel,
//!                                          opened from the bar's theme menu
//!   +-- PanelPickerModel     (panel.rs)     panel/layout gallery, same fly-out,
//!                                          opened from the bar's panel button
//!   +-- ShortcutsModel       (shortcuts.rs) keyboard-shortcut reference, same
//!                                          fly-out, opened from the bar
//! ```
//!
//! Why the invisible root? A GTK application needs a window to stay alive, and
//! layer surfaces belong to a single monitor, so the "shell" itself cannot be
//! one surface. OkShell solves this exactly the same way.

mod bar;
mod flyout;
mod launcher;
mod mango;
mod panel;
mod shortcuts;
mod theme;
mod wallpaper;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;

use bar::{BarInit, BarInput, BarModel, BarOutput};
use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use launcher::{LauncherInput, LauncherModel};
use panel::{PanelOutput, PanelPickerInput, PanelPickerModel};
use relm4::prelude::*;
use shortcuts::{ShortcutsInput, ShortcutsModel};
use theme::{ThemeOutput, ThemePickerInput, ThemePickerModel};
use wallpaper::{
    WallpaperInit, WallpaperInput, WallpaperModel, WallpaperOutput, WallpaperPickerInput,
    WallpaperPickerModel,
};

use mango::TagView;

/// The shell owns one bar and one wallpaper surface per monitor, the app
/// launcher, the wallpaper picker, the theme gallery, and coordinates the IPC
/// feed.
struct Shell {
    bars: HashMap<String, Controller<BarModel>>,
    wallpapers: HashMap<String, Controller<WallpaperModel>>,
    launcher: Controller<LauncherModel>,
    picker: Controller<WallpaperPickerModel>,
    /// The gallery you can open from the bar's theme menu.
    theme_picker: Controller<ThemePickerModel>,
    /// The panel gallery, opened from the bar's panel button.
    panel_picker: Controller<PanelPickerModel>,
    /// The keyboard-shortcut reference, opened from the bar.
    shortcuts: Controller<ShortcutsModel>,
    /// The wallpaper currently shown, restored from disk at startup.
    current: Option<PathBuf>,
    /// The active theme, restored from disk at startup.
    theme: &'static theme::Theme,
    /// The active panel (bar layout), restored from disk at startup.
    panel: &'static panel::Panel,
    /// The last tag state per monitor, so rebuilt bars can be repopulated.
    tags: HashMap<String, Vec<TagView>>,
    /// The global stylesheet, kept so the theme can be reloaded in place.
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
    /// Show/hide the theme gallery (sent by the theme menu in the bar).
    ToggleTheme,
    /// Show/hide the panel gallery (sent by the panel button in the bar).
    TogglePanel,
    /// Show/hide the keyboard-shortcut reference (sent by the bar button).
    ToggleShortcuts,
    /// Apply and remember this theme, reloading the stylesheet.
    SetTheme(String),
    /// Apply and remember this panel, rebuilding every bar.
    SetPanel(String),
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
        // The picked theme's token overrides are spliced in after the built-in
        // defaults, and a user stylesheet at ~/.config/mango-shell/style.css
        // is appended on top, so wallpaper-derived `@define-color` overrides
        // (e.g. from matugen) still win over the theme. We keep our own
        // provider (rather than `relm4::set_global_css`) so it can be reloaded
        // in place when the theme — or the wallpaper, and therefore the
        // palette — changes.
        let theme = theme::load_current();
        let panel = panel::load_current();
        let css = gtk::CssProvider::new();
        css.load_from_data(&load_css(panel, theme));
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
        // monitor; they fly out from under the bar and stay hidden until
        // their toggle arrives.
        let launcher = LauncherModel::builder().launch(current.clone()).detach();
        let picker = WallpaperPickerModel::builder()
            .launch(current.clone())
            .forward(sender.input_sender(), |output| match output {
                WallpaperOutput::Apply(path) => ShellInput::SetWallpaper(path),
            });
        // The theme gallery is another shared overlay, opened from the bar's
        // theme menu ("Browse themes…"). It shows the wallpaper on its banner
        // too, so the shell pushes the current one right away.
        let theme_picker = ThemePickerModel::builder()
            .launch(theme.id.to_string())
            .forward(sender.input_sender(), |output| match output {
                ThemeOutput::Apply(id) => ShellInput::SetTheme(id),
            });
        theme_picker.emit(ThemePickerInput::Banner(current.clone()));

        // The panel gallery is yet another shared overlay: a mock of each
        // ported panel layout. Picking one rebuilds every bar.
        let panel_picker = PanelPickerModel::builder()
            .launch(panel.id.to_string())
            .forward(sender.input_sender(), |output| match output {
                PanelOutput::Apply(id) => ShellInput::SetPanel(id),
            });

        // The keyboard-shortcut reference is read-only, but it shares the
        // fly-out recipe (and the wallpaper banner). It re-parses Mango's
        // config each time it opens.
        let shortcuts = ShortcutsModel::builder().launch(current.clone()).detach();

        let model = Shell {
            bars: HashMap::new(),
            wallpapers: HashMap::new(),
            launcher,
            picker,
            theme_picker,
            panel_picker,
            shortcuts,
            current,
            theme,
            panel,
            tags: HashMap::new(),
            css,
        };

        let widgets = view_output!();

        // Make the fly-outs hang just below the panel's bar. They default to
        // `flyout::BAR_GAP`, which assumes the original 40px bar; when a
        // panel floats or is thin, the gap must follow it.
        set_flyout_gap(&model, model.panel);

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
                        theme: self.theme.id.to_string(),
                        panel: self.panel,
                        // Get layout symbol from IPC query
                        layout: "Tiled".to_string(),
                    })
                    // The bar's buttons toggle the shell's overlays, and its
                    // theme menu applies themes.
                    .forward(sender.input_sender(), |output| match output {
                        BarOutput::ToggleLauncher => ShellInput::ToggleLauncher,
                        BarOutput::ToggleWallpaper => ShellInput::ToggleWallpaper,
                        BarOutput::ToggleThemePicker => ShellInput::ToggleTheme,
                        BarOutput::TogglePanelPicker => ShellInput::TogglePanel,
                        BarOutput::ToggleShortcuts => ShellInput::ToggleShortcuts,
                        BarOutput::SetTheme(id) => ShellInput::SetTheme(id),
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
                self.wallpapers.insert(name.clone(), wallpaper);
                // A freshly built bar starts empty; repopulate its tags from
                // the last known state (e.g. right after a panel switch).
                if let Some(tags) = self.tags.get(&name) {
                    self.bars
                        .get(&name)
                        .expect("bar just inserted")
                        .emit(BarInput::SetTags(tags.clone()));
                }
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
                self.tags = update.clone();
                for (name, tags) in update {
                    if let Some(controller) = self.bars.get(&name) {
                        controller.emit(BarInput::SetTags(tags));
                    }
                }
            }

            ShellInput::ToggleLauncher => self.launcher.emit(LauncherInput::Toggle),

            ShellInput::ToggleWallpaper => self.picker.emit(WallpaperPickerInput::Toggle),

            ShellInput::ToggleTheme => self.theme_picker.emit(ThemePickerInput::Toggle),

            ShellInput::TogglePanel => self.panel_picker.emit(PanelPickerInput::Toggle),

            ShellInput::ToggleShortcuts => self.shortcuts.emit(ShortcutsInput::Toggle),

            ShellInput::SetTheme(id) => {
                if !theme::is_known(&id) {
                    return;
                }
                theme::save_current(&id);
                self.theme = theme::get(&id);
                // Reload the whole stylesheet: the theme's token overrides are
                // spliced into the string the provider was built from.
                let theme = self.theme;
                let panel = self.panel;
                self.css.load_from_data(&load_css(panel, theme));
                // Keep the gallery highlight...
                self.theme_picker
                    .emit(ThemePickerInput::SetCurrent(id.clone()));
                // ...and the popover checkmarks in every bar in sync.
                for controller in self.bars.values() {
                    controller.emit(BarInput::SetTheme(id.clone()));
                }
            }

            ShellInput::SetPanel(id) => {
                if !panel::is_known(&id) {
                    return;
                }
                panel::save_current(&id);
                self.panel = panel::get(&id);
                // Reload the stylesheet so the panel's CSS profile (palette +
                // structure) applies.
                let panel = self.panel;
                let theme = self.theme;
                self.css.load_from_data(&load_css(panel, theme));
                // Keep the gallery highlight in sync.
                self.panel_picker
                    .emit(PanelPickerInput::SetCurrent(id.clone()));
                // The bar may float or shrink, so keep the fly-outs hanging
                // just below it.
                set_flyout_gap(self, self.panel);
                // Rebuild every bar from the new layout. The wallpaper
                // surfaces are untouched.
                for (_, controller) in self.bars.drain() {
                    controller.widget().set_visible(false);
                }
                sync_monitors(&self.bars, &sender);
            }

            ShellInput::SetWallpaper(path) => {
                wallpaper::save_current(&path);
                self.current = Some(path.clone());
                // Swap the picture on every monitor...
                for controller in self.wallpapers.values() {
                    controller.emit(WallpaperInput::SetPath(Some(path.clone())));
                }
                // ...and keep the picker's highlight and every flyout banner
                // in sync. The picker's own SetCurrent marks its tile; the
                // launcher and theme gallery just repaint their wallpaper
                // headers.
                self.picker
                    .emit(WallpaperPickerInput::SetCurrent(path.clone()));
                self.launcher
                    .emit(LauncherInput::Banner(Some(path.clone())));
                self.theme_picker
                    .emit(ThemePickerInput::Banner(Some(path.clone())));
                self.shortcuts
                    .emit(ShortcutsInput::Banner(Some(path.clone())));
                // Let an optional hook (matugen, pywal, …) re-derive the
                // palette, then reload the stylesheet once it finishes.
                let sender = sender.clone();
                std::thread::spawn(move || {
                    wallpaper::run_hook(&path);
                    let _ = sender.input_sender().send(ShellInput::ReloadCss);
                });
            }

            ShellInput::ReloadCss => self.css.load_from_data(&load_css(self.panel, self.theme)),
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
/// The theme gallery has no signal of its own (Linux only has two user
/// signals) — open it from the theme menu's "Browse themes…" row instead.
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

/// Keep the fly-out surfaces (launcher, wallpaper picker, theme and panel
/// galleries, shortcut reference) hanging just below the active panel's bar.
/// Each panel has its own height and may float, so the gap must follow it when
/// the panel changes.
fn set_flyout_gap(shell: &Shell, panel: &panel::Panel) {
    let gap = panel.flyout_gap();
    for window in [
        shell.launcher.widget(),
        shell.picker.widget(),
        shell.theme_picker.widget(),
        shell.panel_picker.widget(),
        shell.shortcuts.widget(),
    ] {
        window.set_margin(Edge::Top, gap);
    }
}

/// Build the stylesheet the shell loads: the built-in defaults, then the
/// selected **panel**'s CSS profile (its default palette + structural rules),
/// then the selected theme's token overrides, then an optional user override
/// on top.
///
/// The override lives at `~/.config/mango-shell/style.css` and is meant for
/// `@define-color` tokens, which is how you tint the shell to the wallpaper
/// (the way end4-pC does with matugen). It sits after everything, so a
/// wallpaper-derived palette always wins. Panels define `@define-color`
/// tokens too, but the theme is appended *after* them, so picking e.g.
/// Gruvbox recolours any panel — panels supply the default palette, themes
/// the override.
fn load_css(panel: &panel::Panel, theme: &theme::Theme) -> String {
    let mut css = String::from(include_str!("style.css"));

    css.push('\n');
    css.push_str(panel.css);

    css.push('\n');
    css.push_str(theme.css);

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
