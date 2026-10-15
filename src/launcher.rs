//! Application launcher.
//!
//! A single [`Layer::Overlay`] surface shared by every monitor. Instead of
//! covering the whole screen it is a "fly-out": it hangs just below the shell
//! bar (see [`crate::flyout`]) and its panel slides down out of the bar when
//! toggled, over whatever window is open. The card follows the launchers of
//! the HyDE project: a rounded panel with the current wallpaper pinned on top,
//! a pill search field and accent-highlighted rows.
//!
//! It lists every `.desktop` entry GLib knows about, filters them as you type
//! and launches the highlighted one with [`gio::AppInfo::launch`].
//!
//! The two details that matter for a launcher are:
//!
//! 1. **Exclusive keyboard mode.** Without it the compositor keeps sending
//!    keystrokes to whatever window is focused behind the surface.
//! 2. **An `EventControllerKey`.** GTK4 windows have no `key-pressed` signal,
//!    so keys are caught with a controller in the capture phase (before the
//!    entry sees them), which is how Escape/arrows/Enter are handled.

use std::path::PathBuf;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;

/// How many results to show at once. The list is rebuilt on every keystroke,
/// so keeping it bounded keeps typing responsive.
const MAX_RESULTS: usize = 60;

pub struct LauncherModel {
    /// Every launchable application, loaded once at startup.
    apps: Vec<gio::AppInfo>,
    /// Display names parallel to `apps`, cached so the label does not have to
    /// ask GIO for them again while the list is rebuilt.
    names: Vec<String>,
    /// Lowercased `names`, parallel to `apps`, so filtering a keystroke does
    /// not allocate a fresh lowercased string for every application.
    lower_names: Vec<String>,
    /// Indices into `apps` that match the current query, in display order.
    matches: Vec<usize>,
    /// Position within `matches` of the highlighted row.
    selected: usize,
    /// The root, cloned so `update` can show/hide and focus it.
    window: gtk::Window,
    /// Slides + fades the panel out from under the bar.
    flyout: flyout::Flyout,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
}

#[derive(Debug)]
pub enum LauncherInput {
    /// Flip visibility (sent by the shell on `SIGUSR1`).
    Toggle,
    /// Hide without launching (Escape).
    Hide,
    /// The query text changed.
    Search(String),
    /// Move the highlight by `-1`/`+1`.
    Move(isize),
    /// Launch the highlighted entry (Enter).
    Activate,
    /// Launch the entry at this position in `matches` (row click).
    Launch(usize),
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
}

#[relm4::component(pub)]
impl SimpleComponent for LauncherModel {
    type Init = Option<PathBuf>;
    type Input = LauncherInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-launcher"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.flyout.widget() {},
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fly-out surface: hugs the top of the monitor just below the bar,
        // grabs the keyboard while open, hidden until toggled.
        flyout::prepare_layer(&root, "mango-launcher");

        let apps: Vec<gio::AppInfo> = gio::AppInfo::all()
            .into_iter()
            .filter(|app| app.should_show())
            .collect();

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search applications…"));
        search.set_hexpand(true);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_max_content_height(360);
        scrolled.set_propagate_natural_height(true);
        scrolled.set_child(Some(&list));

        // The panel: wallpaper banner, then the search pill and the results.
        let banner = flyout::make_banner();
        flyout::set_banner(&banner, init.as_deref());

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
        panel.add_css_class("mango-launcher-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        panel.append(&search);
        panel.append(&scrolled);

        let flyout_panel = flyout::Flyout::new(&root);
        flyout_panel.set_child(&panel);

        // Re-filter on every text change (immediate, unlike `search-changed`
        // which GtkSearchEntry debounces).
        let search_sender = sender.clone();
        search.connect_changed(move |entry| {
            search_sender.input(LauncherInput::Search(entry.text().to_string()));
        });

        // Clicking a row (or Enter while it is selected) launches it.
        let row_sender = sender.clone();
        list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index >= 0 {
                row_sender.input(LauncherInput::Launch(index as usize));
            }
        });

        // Used by the backdrop click handler below (the key controller takes
        // ownership of `sender` first).
        let backdrop_sender = sender.clone();

        // Capture phase, so these keys are seen before the search entry.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                sender.input(LauncherInput::Hide);
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                sender.input(LauncherInput::Move(1));
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                sender.input(LauncherInput::Move(-1));
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                sender.input(LauncherInput::Activate);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        // Cache the names up front so a keystroke only lowercases the query,
        // not every application.
        let names: Vec<String> = apps
            .iter()
            .map(|app| app.display_name().to_string())
            .collect();
        let lower_names: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();

        let mut model = LauncherModel {
            apps,
            names,
            lower_names,
            matches: Vec::new(),
            selected: 0,
            window: root.clone(),
            flyout: flyout_panel,
            banner,
            search,
            list,
        };

        let widgets = view_output!();

        // Clicking anywhere outside the panel — the transparent fly-out areas
        // over the open window — closes the launcher. Clicks on rows stay
        // inside the panel, so they pass through to the list.
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                backdrop_sender.input(LauncherInput::Hide);
            }
        });
        root.add_controller(click);

        // Populate the list for the empty query so it is ready on first open.
        model.refilter("");

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            LauncherInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            LauncherInput::Hide => self.hide(),
            LauncherInput::Search(query) => self.refilter(&query),
            LauncherInput::Move(delta) => {
                if self.matches.is_empty() {
                    return;
                }
                let last = self.matches.len() as isize - 1;
                self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
                self.update_selection();
            }
            LauncherInput::Activate => self.launch(self.selected),
            LauncherInput::Launch(position) => self.launch(position),
            LauncherInput::Banner(path) => flyout::set_banner(&self.banner, path.as_deref()),
        }
    }
}

impl LauncherModel {
    /// Reset the query and slide the panel out of the bar.
    fn show(&mut self) {
        // `set_text` may or may not emit `changed`, so re-filter explicitly.
        self.search.set_text("");
        self.refilter("");
        self.flyout.show(Some(self.search.upcast_ref()));
    }

    fn hide(&mut self) {
        self.flyout.hide();
    }

    /// Recompute `matches` for `query` and rebuild the rows.
    fn refilter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        self.matches = self
            .lower_names
            .iter()
            .enumerate()
            .filter(|(_, name)| query.is_empty() || name.contains(&query))
            .map(|(index, _)| index)
            .take(MAX_RESULTS)
            .collect();
        self.selected = 0;
        self.render();
    }

    fn render(&mut self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        for &app_index in &self.matches {
            let app = &self.apps[app_index];
            let row = gtk::ListBoxRow::new();

            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            if let Some(icon) = app.icon() {
                let image = gtk::Image::from_gicon(&icon);
                image.set_pixel_size(22);
                line.append(&image);
            }

            let label = gtk::Label::new(Some(&self.names[app_index]));
            label.set_xalign(0.0);
            label.set_halign(gtk::Align::Start);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line.append(&label);

            row.set_child(Some(&line));
            self.list.append(&row);
        }

        self.update_selection();
    }

    fn update_selection(&self) {
        if let Some(row) = self.list.row_at_index(self.selected as i32) {
            self.list.select_row(Some(&row));
        }
    }

    /// Launch the entry at `position` within `matches`.
    fn launch(&mut self, position: usize) {
        let Some(&app_index) = self.matches.get(position) else {
            return;
        };

        // Hide first so the launched application ends up focused.
        self.hide();

        let app = &self.apps[app_index];
        if let Err(error) = app.launch(&[], None::<&gio::AppLaunchContext>) {
            eprintln!(
                "mango-shell: failed to launch {}: {error}",
                app.display_name()
            );
        }
    }
}
