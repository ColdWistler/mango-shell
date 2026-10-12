//! Application launcher.
//!
//! A single centred [`Layer::Overlay`] surface that stays hidden until the
//! shell toggles it (currently via `SIGUSR1`, see `main.rs`). It lists every
//! `.desktop` entry GLib knows about, filters them as you type and launches the
//! highlighted one with [`gio::AppInfo::launch`].
//!
//! The two details that matter for a launcher are:
//!
//! 1. **Exclusive keyboard mode.** Without it the compositor keeps sending
//!    keystrokes to whatever window is focused behind the surface.
//! 2. **An `EventControllerKey`.** GTK4 windows have no `key-pressed` signal,
//!    so keys are caught with a controller in the capture phase (before the
//!    entry sees them), which is how Escape/arrows/Enter are handled.

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::prelude::*;

/// How many results to show at once. The list is rebuilt on every keystroke,
/// so keeping it bounded keeps typing responsive.
const MAX_RESULTS: usize = 60;

pub struct LauncherModel {
    /// Every launchable application, loaded once at startup.
    apps: Vec<gio::AppInfo>,
    /// Indices into `apps` that match the current query, in display order.
    matches: Vec<usize>,
    /// Position within `matches` of the highlighted row.
    selected: usize,
    /// The root, cloned so `update` can show/hide and focus it.
    window: gtk::Window,
    search: gtk::SearchEntry,
    scrolled: gtk::ScrolledWindow,
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
}

#[relm4::component(pub)]
impl SimpleComponent for LauncherModel {
    type Init = ();
    type Input = LauncherInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-launcher"],
            set_decorated: false,

            gtk::Box {
                set_css_classes: &["mango-launcher-panel"],
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 10,
                set_halign: gtk::Align::Center,
                set_valign: gtk::Align::Center,

                append = &model.search.clone() {},
                append = &model.scrolled.clone() {},
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Layer-shell config must run before the window is realized, and `init`
        // happens during GTK startup, so this ordering is correct (same as the
        // bar in `bar.rs`).
        root.init_layer_shell();
        root.set_namespace(Some("mango-launcher"));
        root.set_layer(Layer::Overlay);
        // Grab keyboard focus while the launcher is up; otherwise keystrokes go
        // to the window behind it.
        root.set_keyboard_mode(KeyboardMode::Exclusive);
        // Fill the screen so the panel is centred on a dimmed backdrop. Child
        // windows are not presented automatically, so it starts hidden.
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Bottom, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);
        root.set_visible(false);

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

        let mut model = LauncherModel {
            apps,
            matches: Vec::new(),
            selected: 0,
            window: root.clone(),
            search,
            scrolled,
            list,
        };

        let widgets = view_output!();

        // Clicking the dimmed backdrop — anything outside the panel — closes
        // the launcher. Clicks on rows stay inside the panel, so they pass
        // through to the list.
        if let Some(panel) = root.child() {
            let hide_sender = backdrop_sender;
            let click = gtk::GestureClick::new();
            click.connect_pressed(move |_, _, x, y| {
                let area = panel.allocation();
                let inside = x >= f64::from(area.x())
                    && x < f64::from(area.x() + area.width())
                    && y >= f64::from(area.y())
                    && y < f64::from(area.y() + area.height());
                if !inside {
                    hide_sender.input(LauncherInput::Hide);
                }
            });
            root.add_controller(click);
        }

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
        }
    }
}

impl LauncherModel {
    /// Reset the query and present the surface.
    fn show(&mut self) {
        // `set_text` may or may not emit `changed`, so re-filter explicitly.
        self.search.set_text("");
        self.refilter("");
        self.window.present();
        self.search.grab_focus();
    }

    fn hide(&mut self) {
        self.window.set_visible(false);
    }

    /// Recompute `matches` for `query` and rebuild the rows.
    fn refilter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        self.matches = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, app)| {
                query.is_empty() || app.display_name().to_lowercase().contains(&query)
            })
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

            let label = gtk::Label::new(Some(app.display_name().as_str()));
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
