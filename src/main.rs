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
//!   +-- DashboardModel       (dashboard.rs) the front door behind the bar's
//!   |         + flyout.rs                   Arch mark: launcher + settings tiles
//!   +-- CarouselModel        (carousel.rs)  fullscreen skewed wallpaper
//!   |                                      carousel (SIGUSR2), ported from
//!   |                                      DankMaterialShell's wallpaperCarousel
//!   +-- ThemePickerModel     (theme.rs)     theme gallery, same fly-out panel,
//!                                          opened from the bar's theme menu
//!   +-- PanelPickerModel     (panel.rs)     panel/layout gallery, same fly-out,
//!                                          opened from the bar's panel button
//!   +-- ShortcutsModel       (shortcuts.rs) keyboard-shortcut reference, same
//!                                          fly-out, opened from the bar
//!   +-- OverviewModel        (overview.rs)  workspace preview grid — a card
//!                                          per tag with every window drawn to
//!                                          scale (bar button or SIGRTMIN+1)
//!   +-- SettingsModel        (settings.rs)  bar customisation: reorder the
//!   |         + phone.rs                   bar's widgets, override its height
//!   |                                      and corner rounding — the app is a
//!   |                                      phone that rises from the bottom
//!   |                                      of the screen (dashboard tile)
//! ```
//!
//! Why the invisible root? A GTK application needs a window to stay alive, and
//! layer surfaces belong to a single monitor, so the "shell" itself cannot be
//! one surface. OkShell solves this exactly the same way.

mod anim;
mod bar;
mod carousel;
mod dashboard;
mod flyout;
mod launcher;
mod mango;
mod overview;
mod panel;
mod phone;
mod settings;
mod shortcuts;
mod theme;
mod wallpaper;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;

use bar::{BarInit, BarInput, BarModel, BarOutput};
use carousel::{CarouselInput, CarouselModel, CarouselOutput};
use dashboard::{DashboardInput, DashboardModel, DashboardOutput};
use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use launcher::{LauncherInput, LauncherModel};
use overview::{OverviewInput, OverviewModel};
use panel::{PanelOutput, PanelPickerInput, PanelPickerModel};
use relm4::prelude::*;
use settings::{LayoutEntry, SettingsInit, SettingsInput, SettingsModel, SettingsOutput, Shape};
use shortcuts::{ShortcutsInput, ShortcutsModel};
use theme::{ThemeOutput, ThemePickerInput, ThemePickerModel};
use wallpaper::{WallpaperInit, WallpaperInput, WallpaperModel};

use mango::{ClientView, MonitorView};

/// The shell owns one bar and one wallpaper surface per monitor, the app
/// launcher, the dashboard in front of it, the wallpaper carousel, the theme
/// gallery, the workspace overview, and coordinates the IPC feed.
struct Shell {
    bars: HashMap<String, Controller<BarModel>>,
    wallpapers: HashMap<String, Controller<WallpaperModel>>,
    launcher: Controller<LauncherModel>,
    /// The front-door panel behind the bar's Arch mark: the launcher's and
    /// the settings app's entries point.
    dashboard: Controller<DashboardModel>,
    /// The fullscreen skewed wallpaper carousel.
    carousel: Controller<CarouselModel>,
    /// The gallery you can open from the bar's theme menu.
    theme_picker: Controller<ThemePickerModel>,
    /// The panel gallery, opened from the bar's panel button.
    panel_picker: Controller<PanelPickerModel>,
    /// The keyboard-shortcut reference, opened from the bar.
    shortcuts: Controller<ShortcutsModel>,
    /// The workspace preview grid, opened from the bar or a keybind.
    overview: Controller<OverviewModel>,
    /// The settings app: bar-widget layout and bar-shape overrides, opened
    /// from the dashboard's Settings tile.
    settings: Controller<SettingsModel>,
    /// The wallpaper currently shown, restored from disk at startup.
    current: Option<PathBuf>,
    /// The active theme, restored from disk at startup.
    theme: &'static theme::Theme,
    /// The active panel (bar layout), restored from disk at startup.
    panel: &'static panel::Panel,
    /// The settings app's widget layout for the bar, restored at startup.
    /// Bars are built from it; every change is broadcast and persisted.
    layout: Vec<LayoutEntry>,
    /// The settings app's bar-shape overrides (height / corner radius), or
    /// `None` while the panel's own shape is in force.
    shape: Option<Shape>,
    /// The last monitor state per connector, so rebuilt bars can be
    /// repopulated and the overview always has geometry to scale against.
    monitors: HashMap<String, MonitorView>,
    /// The last window list, so the overview can be rebuilt while open.
    clients: Vec<ClientView>,
    /// The global stylesheet, kept so the theme can be reloaded in place.
    css: gtk::CssProvider,
    has_mango: bool,
}

#[derive(Debug)]
enum ShellInput {
    /// Re-read the monitor list from GDK.
    SyncMonitors,
    /// Spawn a bar and wallpaper surface for the monitor with this connector.
    AddMonitor(String),
    /// Tear down the surfaces for this connector.
    RemoveMonitor(String),
    /// Fresh monitor state (tags, geometry), keyed by connector name.
    MangoUpdate(HashMap<String, MonitorView>),
    /// Fresh window list, for the overview's tiles.
    ClientsUpdate(Vec<ClientView>),
    /// Show/hide the application launcher (sent on `SIGUSR1`).
    ToggleLauncher,
    /// Show/hide the dashboard, the panel behind the bar's Arch mark.
    ToggleDashboard,
    /// The dashboard's Settings tile was pressed.
    OpenSettings,
    /// Show/hide the wallpaper carousel (sent on `SIGUSR2` or the bar button).
    ToggleWallpaper,
    /// Show/hide the theme gallery (sent by the theme menu in the bar).
    ToggleTheme,
    /// Show/hide the panel gallery (sent by the panel button in the bar).
    TogglePanel,
    /// Show/hide the keyboard-shortcut reference (sent by the bar button).
    ToggleShortcuts,
    /// Show/hide the workspace overview (sent by the bar button or `SIGRTMIN+1`).
    ToggleOverview,
    /// Apply and remember this theme, reloading the stylesheet.
    SetTheme(String),
    /// Apply and remember this panel, rebuilding every bar.
    SetPanel(String),
    /// Remember and show this wallpaper everywhere.
    SetWallpaper(PathBuf),
    /// Re-read `style.css` after a wallpaper change regenerated it.
    ReloadCss,
    /// The settings app reordered / hid bar widgets: persist and broadcast.
    LayoutChanged(Vec<LayoutEntry>),
    /// The settings app changed the bar-shape overrides: persist, reload the
    /// stylesheet, and re-gap the fly-outs (the bar may have grown).
    ShapeChanged(Shape),
    /// The settings app's Reset was pressed: defaults everywhere, UI included.
    ResetSettings,
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
        // defaults. For the Matugen (Wallpaper) theme, the matugen-generated
        // stylesheet at ~/.config/mango-shell/style.css is appended on top.
        // We keep our own provider (rather than `relm4::set_global_css`) so
        // it can be reloaded in place when the theme or wallpaper changes.
        let theme = theme::load_current();
        let panel = panel::load_current();
        // The settings app's persisted state: which widgets sit where on the
        // bar, and any explicit height / radius overrides.
        let (layout, shape) = settings::load();
        let css = gtk::CssProvider::new();
        css.load_from_data(&load_css(panel, theme, shape));
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

        // The launcher and the carousel are single surfaces shared by every
        // monitor; the launcher flies out from under the bar and both stay
        // hidden until their toggle arrives.
        let launcher = LauncherModel::builder().launch(current.clone()).detach();
        // The dashboard is the front door the bar's Arch mark opens: a small
        // fly-out with the tiles that lead to the launcher and (once it is
        // built) the settings app.
        let dashboard = DashboardModel::builder()
            .launch(current.clone())
            .forward(sender.input_sender(), |output| match output {
                DashboardOutput::OpenLauncher => ShellInput::ToggleLauncher,
                DashboardOutput::OpenSettings => ShellInput::OpenSettings,
            });
        let carousel = CarouselModel::builder()
            .launch(current.clone())
            .forward(sender.input_sender(), |output| match output {
                CarouselOutput::Apply(path) => ShellInput::SetWallpaper(path),
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

        // The workspace overview watches Mango's IPC itself (like the bar's
        // tag dispatches), so it only needs the state pushes and its toggle.
        let overview = OverviewModel::builder().launch(()).detach();

        // The settings app shares the fly-out recipe. It edits two things and
        // pushes every change up live: the widget layout (the shell saves it
        // and broadcasts it to every bar) and the bar shape (the shell saves
        // it and reloads the stylesheet).
        let settings = SettingsModel::builder()
            .launch(SettingsInit {
                wallpaper: current.clone(),
                layout: layout.clone(),
                shape,
                default_height: panel.height as u32,
            })
            .forward(sender.input_sender(), |output| match output {
                SettingsOutput::LayoutChanged(layout) => ShellInput::LayoutChanged(layout),
                SettingsOutput::ShapeChanged(shape) => ShellInput::ShapeChanged(shape),
                SettingsOutput::Reset => ShellInput::ResetSettings,
            });
        let has_mango = mango::has_mango_session();
        
        let model = Shell {
            bars: HashMap::new(),
            wallpapers: HashMap::new(),
            launcher,
            dashboard,
            carousel,
            theme_picker,
            panel_picker,
            shortcuts,
            overview,
            settings,
            current,
            theme,
            panel,
            layout,
            shape,
            monitors: HashMap::new(),
            clients: Vec::new(),
            css,
            has_mango,
        };

        let widgets = view_output!();

        // Make the fly-outs hang just below the panel's bar. They default to
        // `flyout::BAR_GAP`, which assumes the original 40px bar; when a
        // panel floats or is thin, the gap must follow it.
        set_flyout_gap(&model, model.panel);

        setup_monitor_watcher(&sender);
        sync_monitors(&model.bars, &sender);
        if has_mango {
            mango::spawn_watcher(sender.clone());
            mango::spawn_client_watcher(sender.clone());
        }
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
                        monitor: monitor.clone(),
                        theme: self.theme.id.to_string(),
                        panel: self.panel,
                        layout: self.layout.clone(),
                        shape: self.shape,
                    })
                    // The bar's buttons toggle the shell's overlays, and its
                    // theme menu applies themes.
                    .forward(sender.input_sender(), |output| match output {
                        BarOutput::ToggleDashboard => ShellInput::ToggleDashboard,
                        BarOutput::ToggleThemePicker => ShellInput::ToggleTheme,
                        BarOutput::TogglePanelPicker => ShellInput::TogglePanel,
                        BarOutput::ToggleShortcuts => ShellInput::ToggleShortcuts,
                        BarOutput::ToggleOverview => ShellInput::ToggleOverview,
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
                if self.has_mango {
                    if let Some(monitor) = self.monitors.get(&name) {
                        self.bars
                            .get(&name)
                            .expect("bar just inserted")
                            .emit(BarInput::SetTags(monitor.tags.clone()));
                    }
                } else {
                    self.bars
                        .get(&name)
                        .expect("bar just inserted")
                        .emit(BarInput::SetTags(Vec::new()));
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
                if self.has_mango {
                    for (name, monitor) in &update {
                        if let Some(controller) = self.bars.get(name) {
                            controller.emit(BarInput::SetTags(monitor.tags.clone()));
                        }
                    }
                    self.monitors = update;
                    self.push_overview_state();
                    self.push_dashboard_state();
                }
            }

            ShellInput::ClientsUpdate(clients) => {
                if self.has_mango {
                    self.clients = clients;
                    self.push_overview_state();
                    self.push_dashboard_state();
                }
            }

            ShellInput::ToggleLauncher => {
                // The launcher and the dashboard are fly-outs from the same
                // corner of the bar, so they swap rather than stack: opening
                // one closes the other. (`SIGUSR1` reaches the launcher the
                // same way the dashboard's Applications tile does.)
                self.dashboard.emit(DashboardInput::Hide);
                self.launcher.emit(LauncherInput::Toggle);
            }

            ShellInput::ToggleDashboard => {
                self.launcher.emit(LauncherInput::Hide);
                // Same reasoning as the overview below: the dashboard draws
                // the shell's last pushed state, so re-read the client list
                // before it opens rather than letting it come up stale.
                if self.has_mango {
                    mango::refresh_clients(sender.clone());
                    self.push_dashboard_state();
                }
                self.dashboard.emit(DashboardInput::Toggle);
            }

            ShellInput::OpenSettings => {
                // The dashboard's Settings tile leads here. Fly-outs from the
                // same corner swap rather than stack (same rule as the
                // launcher / dashboard pair), so close the dashboard first.
                self.dashboard.emit(DashboardInput::Hide);
                self.settings.emit(SettingsInput::Toggle);
            }

            ShellInput::ToggleWallpaper => self.carousel.emit(CarouselInput::Toggle),

            ShellInput::ToggleTheme => self.theme_picker.emit(ThemePickerInput::Toggle),

            ShellInput::TogglePanel => self.panel_picker.emit(PanelPickerInput::Toggle),

            ShellInput::ToggleShortcuts => self.shortcuts.emit(ShortcutsInput::Toggle),

            ShellInput::ToggleOverview => {
                // A toggle is also an opening, and the grid draws the shell's
                // last pushed state: re-read it so the overview can never come
                // up still showing a window that has since been closed. The
                // live feed covers whatever happens while it stays open.
                if self.has_mango {
                    mango::refresh_clients(sender.clone());
                }
                self.overview.emit(OverviewInput::Toggle);
            }

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
                self.css.load_from_data(&load_css(panel, theme, self.shape));
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
                self.css.load_from_data(&load_css(panel, theme, self.shape));
                // Keep the gallery highlight in sync.
                self.panel_picker
                    .emit(PanelPickerInput::SetCurrent(id.clone()));
                // The Height slider's "off" position follows the active
                // panel's own height, so tell the settings app about it too.
                self.settings
                    .emit(SettingsInput::PanelHeight(panel.height as u32));
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
                // ...and keep the carousel centered on the new wallpaper and
                // every flyout banner in sync. The launcher and theme gallery
                // just repaint their wallpaper headers.
                self.carousel
                    .emit(CarouselInput::SetCurrent(path.clone()));
                self.launcher
                    .emit(LauncherInput::Banner(Some(path.clone())));
                self.dashboard
                    .emit(DashboardInput::Banner(Some(path.clone())));
                self.theme_picker
                    .emit(ThemePickerInput::Banner(Some(path.clone())));
                self.shortcuts
                    .emit(ShortcutsInput::Banner(Some(path.clone())));
                self.settings
                    .emit(SettingsInput::Banner(Some(path.clone())));
                // Let an optional hook (matugen, pywal, …) re-derive the
                // palette, then reload the stylesheet once it finishes.
                let sender = sender.clone();
                std::thread::spawn(move || {
                    wallpaper::run_hook(&path);
                    let _ = sender.input_sender().send(ShellInput::ReloadCss);
                });
            }

            ShellInput::ReloadCss => {
                self.css
                    .load_from_data(&load_css(self.panel, self.theme, self.shape))
            }

            ShellInput::LayoutChanged(layout) => {
                // The settings app is the only writer: keep our copy for new
                // bars, persist, then rearrange every bar that is up.
                self.layout = layout.clone();
                settings::save(&self.layout, self.shape);
                for controller in self.bars.values() {
                    controller.emit(BarInput::SetLayout(layout.clone()));
                }
            }

            ShellInput::ShapeChanged(shape) => {
                // A shape with nothing overridden in it is "no overrides".
                self.shape = (!shape.is_default()).then_some(shape);
                settings::save(&self.layout, self.shape);
                let panel = self.panel;
                let theme = self.theme;
                self.css.load_from_data(&load_css(panel, theme, self.shape));
                for controller in self.bars.values() {
                    controller.emit(BarInput::SetShape(self.shape));
                }
                // The bar may have grown or shrunk: the fly-outs hang below
                // it, so their gap must follow.
                set_flyout_gap(self, self.panel);
            }

            ShellInput::ResetSettings => {
                // Defaults in both halves: the stock widget order and no
                // shape overrides (the panel profile rules again).
                self.layout = settings::default_layout();
                self.shape = None;
                settings::save(&self.layout, None);
                let panel = self.panel;
                let theme = self.theme;
                self.css.load_from_data(&load_css(panel, theme, None));
                for controller in self.bars.values() {
                    controller.emit(BarInput::SetLayout(self.layout.clone()));
                    controller.emit(BarInput::SetShape(None));
                }
                set_flyout_gap(self, self.panel);
                self.settings.emit(SettingsInput::SetState {
                    layout: self.layout.clone(),
                    shape: None,
                });
            }
        }
    }
}

impl Shell {
    /// Hand the overview the whole of what it draws. It keeps its own copy
    /// (and only repaints while open), so both maps go along every update.
    fn push_overview_state(&self) {
        if self.has_mango {
            self.overview.emit(OverviewInput::SetState {
                monitors: self.monitors.clone(),
                clients: self.clients.clone(),
            });
        }
    }

    /// Hand the dashboard's workspaces card the focused monitor's tag strip
    /// and how many windows sit on its active tags.
    fn push_dashboard_state(&self) {
        if !self.has_mango {
            return;
        }
        let Some((name, monitor)) = self.monitors.iter().find(|(_, monitor)| monitor.active) else {
            return;
        };

        let active: Vec<u32> = monitor
            .tags
            .iter()
            .filter(|tag| tag.active)
            .map(|tag| tag.index)
            .collect();
        let windows = self
            .clients
            .iter()
            .filter(|client| {
                client.monitor == *name
                    && (client.is_global || client.tags.iter().any(|tag| active.contains(tag)))
            })
            .count();

        self.dashboard.emit(DashboardInput::SetState {
            tags: monitor.tags.clone(),
            windows,
        });
    }
}

/// Toggle the launcher on `SIGUSR1`, the wallpaper carousel on `SIGUSR2` and
/// the workspace overview on `SIGRTMIN+1`.
///
/// Mango's keybind spawns a command that signals this process, for example:
///
/// ```ini
/// bind=SUPER,d,spawn,pkill -SIGUSR1 mango-shell
/// bind=SUPER,w,spawn,pkill -SIGUSR2 mango-shell
/// bind=SUPER,t,spawn,pkill -RTMIN+1 mango-shell
/// ```
///
/// The theme gallery has no signal of its own (Linux only has two user
/// signals, both spoken for) — open it from the theme menu's "Browse themes…"
/// row instead. The overview uses a realtime signal, the first one available.
///
/// A signal keeps the shell a single instance, unlike spawning a second copy.
fn spawn_signal_watcher(sender: &ComponentSender<Shell>) {
    use signal_hook::consts::{SIGUSR1, SIGUSR2};
    use signal_hook::iterator::Signals;

    // The first realtime signal; `pkill -RTMIN+1` addresses the same one.
    // glibc numbers realtime signals from 34, so this is 35.
    let overview = libc::SIGRTMIN() + 1;

    let sender = sender.clone();
    std::thread::spawn(move || {
        let Ok(mut signals) = Signals::new([SIGUSR1, SIGUSR2, overview]) else {
            return;
        };
        for signal in signals.forever() {
            match signal {
                SIGUSR2 => sender.input(ShellInput::ToggleWallpaper),
                signal if signal == overview => sender.input(ShellInput::ToggleOverview),
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

/// Keep the fly-out surfaces (launcher, dashboard, theme and panel galleries,
/// shortcut reference) hanging just below the active panel's bar. Each panel
/// has its own height and may float — and the settings app's Height override
/// can pin the bar to something else entirely — so the gap is computed from
/// the *effective* height and re-applied whenever either changes. The
/// wallpaper carousel is fullscreen and needs no gap; the settings phone rises
/// from the bottom of the screen and owns its top margin (see `phone.rs`).
fn set_flyout_gap(shell: &Shell, panel: &panel::Panel) {
    let gap = match shell.shape.and_then(|shape| shape.height) {
        Some(height) => panel.margin_top + height as i32 + 6,
        None => panel.flyout_gap(),
    };
    for window in [
        shell.launcher.widget(),
        shell.dashboard.widget(),
        shell.theme_picker.widget(),
        shell.panel_picker.widget(),
        shell.shortcuts.widget(),
    ] {
        window.set_margin(Edge::Top, gap);
    }
}

/// Build the stylesheet the shell loads: the built-in defaults, then the
/// selected **panel**'s CSS profile (its default palette + structural rules),
/// then the selected theme's token overrides.
///
/// For the Matugen (Wallpaper) theme, also append the matugen-generated
/// palette from `~/.config/mango-shell/style.css` so the wallpaper-derived
/// colours apply. For other themes, do not apply that universal override
/// (so switching themes actually changes the appearance).
///
/// The settings app's **shape overrides** go in last of all, so their
/// `window.mango-bar.mango-user-shaped …` selectors win ties against the
/// panel profile and the theme (equal specificity, later rule).
fn load_css(panel: &panel::Panel, theme: &theme::Theme, shape: Option<Shape>) -> String {
    let mut css = String::from(include_str!("style.css"));

    css.push('\n');
    css.push_str(panel.css);

    css.push('\n');
    css.push_str(theme.css);

    if theme.id == "matugen" {
        if let Some(home) = std::env::var_os("HOME") {
            let path = std::path::Path::new(&home).join(".config/mango-shell/style.css");
            if let Ok(extra) = std::fs::read_to_string(&path) {
                css.push('\n');
                css.push_str(&extra);
            }
        }
    }

    if let Some(shape) = shape {
        css.push('\n');
        css.push_str(&shape.css());
    }

    css
}

fn main() {
    let app = RelmApp::new("com.example.MangoShell");
    app.run::<Shell>(());
}
