//! The dashboard: the shell's front door, and its at-a-glance read-out.
//!
//! The bar's Arch mark opens this panel instead of the launcher directly, so
//! the corner of the bar leads somewhere rather than to a single app grid. It
//! is one fly-out built with the shared recipe (see [`crate::flyout`]): an
//! `Overlay`-layer surface hanging under the bar whose card slides + fades out
//! of it, with the current wallpaper pinned on top like every other fly-out.
//!
//! The card has two halves:
//!
//! * **The doors.** *Applications* hands off to the real [`crate::launcher`];
//!   *Settings* is the seam the settings app grows into. The shell closes this
//!   panel as it opens the other surface, so only one fly-out ever hangs under
//!   the bar.
//! * **The read-outs.** A live clock heads a grid of cards — weather, battery
//!   with the session actions, now playing, CPU / RAM / GPU / disk, the Mango
//!   workspaces, and a row of recently launched apps.
//!
//! Everything slow (sysfs sampling, MPRIS, the weather request) runs on worker
//! threads exactly like the bar's applets: the cards own a poller each and the
//! results arrive as [`DashboardInput`] messages. The resource card only
//! samples while the panel is open.
//!
//! Escape or a click anywhere outside the card closes it.

mod media;
mod power;
mod resources;
mod weather;

use std::path::PathBuf;
use std::time::Duration;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;
use crate::launcher;
use crate::mango::{self, TagView};
use crate::widgets::music::Track;
use crate::widgets::power::BatteryState;
use crate::widgets::resources::ResourceSample;
use crate::widgets::tags::tag_label;

use media::MediaCard;
use power::PowerCard;
use resources::ResourcesCard;
use weather::{WeatherCard, WeatherReport};

/// Edge of the Arch mark on the Applications tile, in pixels.
const LOGO_SIZE: i32 = 44;

/// How many recent launches the quick-launch row shows.
const RECENT_LIMIT: usize = 7;

pub struct DashboardModel {
    /// The root, cloned so `update` can show/hide it.
    window: gtk::Window,
    /// Upstream, for the recent-apps tiles (see [`DashboardInput::LaunchRecent`]).
    sender: ComponentSender<DashboardModel>,
    /// Slides + fades the panel out from under the bar.
    flyout: flyout::Flyout,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
    /// Header labels, refreshed on every [`DashboardInput::Tick`].
    greeting: gtk::Label,
    clock: gtk::Label,
    date: gtk::Label,
    /// The recently-launched row, rebuilt each time the panel opens.
    recent: gtk::Box,
    /// The info cards.
    resources: ResourcesCard,
    power: PowerCard,
    media: MediaCard,
    weather: WeatherCard,
    workspaces: WorkspacesCard,
}

#[derive(Debug)]
pub enum DashboardInput {
    /// Flip visibility (sent by the bar's Arch-mark button).
    Toggle,
    /// Hide without opening anything (Escape or a backdrop click).
    Hide,
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
    /// A fresh resource sample for the stats card.
    Resources(ResourceSample),
    /// A fresh battery snapshot.
    Battery(BatteryState),
    /// The MPRIS snapshot changed (`None` = nothing playing).
    Track(Option<Track>),
    /// The weather request finished (`None` = unavailable).
    Weather(Option<WeatherReport>),
    /// Tag and window counts for the workspaces card (Mango sessions only).
    SetState { tags: Vec<TagView>, windows: usize },
    /// A recent-apps tile was pressed: launch this desktop id.
    LaunchRecent(String),
    /// The wall clock ticked; refresh the header.
    Tick,
}

/// Messages the dashboard sends up to the shell.
#[derive(Debug)]
pub enum DashboardOutput {
    /// The Applications tile was pressed: open the app launcher.
    OpenLauncher,
    /// The Settings tile was pressed: open the settings app.
    OpenSettings,
}

#[relm4::component(pub)]
impl SimpleComponent for DashboardModel {
    type Init = Option<PathBuf>;
    type Input = DashboardInput;
    type Output = DashboardOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-dashboard"],
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
        flyout::prepare_layer(&root, "mango-dashboard");

        let banner = flyout::make_banner();
        flyout::set_banner(&banner, init.as_deref());

        // --- Header: title and greeting on the left, live clock on the right.
        let title = gtk::Label::new(Some("Dashboard"));
        title.add_css_class("mango-dashboard-title");

        let greeting = gtk::Label::new(None);
        greeting.add_css_class("mango-dashboard-greeting");
        greeting.set_xalign(0.0);

        let heading = gtk::Box::new(gtk::Orientation::Vertical, 2);
        heading.set_halign(gtk::Align::Start);
        heading.append(&title);
        heading.append(&greeting);

        let clock = gtk::Label::new(None);
        clock.add_css_class("mango-dashboard-clock");

        let date = gtk::Label::new(None);
        date.add_css_class("mango-dashboard-date");

        let clock_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        clock_box.set_halign(gtk::Align::End);
        clock_box.set_valign(gtk::Align::Center);
        clock_box.append(&clock);
        clock_box.append(&date);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.set_hexpand(true);
        header.append(&heading);
        let header_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header_spacer.set_hexpand(true);
        header.append(&header_spacer);
        header.append(&clock_box);

        // --- The two doors. Applications reuses the bar's Arch mark so the
        // button the user pressed and the tile it leads to read as the same
        // object. The tiles only report the click: the shell decides what each
        // one opens (and closes this panel).
        let apps_button = tile(crate::bar::arch_logo(LOGO_SIZE), "Applications");
        let settings_button = tile(
            {
                let image = gtk::Image::from_icon_name("preferences-system-symbolic");
                image.set_pixel_size(LOGO_SIZE);
                image
            },
            "Settings",
        );
        let apps_sender = sender.clone();
        apps_button.connect_clicked(move |_| {
            let _ = apps_sender.output(DashboardOutput::OpenLauncher);
        });
        let settings_sender = sender.clone();
        settings_button.connect_clicked(move |_| {
            let _ = settings_sender.output(DashboardOutput::OpenSettings);
        });

        let tiles = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        tiles.add_css_class("mango-dashboard-tiles");
        tiles.append(&apps_button);
        tiles.append(&settings_button);

        // --- The info grid: three read-outs on top, the wide resource block
        // and the workspaces below, then the recent row across the bottom.
        let resources = ResourcesCard::new(sender.clone());
        let power = PowerCard::new(sender.clone());
        let media = MediaCard::new(sender.clone());
        let weather = WeatherCard::new(sender.clone());
        let workspaces = WorkspacesCard::new();
        // The workspaces card is Mango-only state; on a dwl session there is
        // nothing to fill it with, so it does not show a dead card.
        if !mango::has_mango_session() {
            workspaces.root().set_visible(false);
        }

        let recent = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        recent.add_css_class("mango-recent-row");
        let recent_card = card("Recent apps");
        recent_card.0.append(&recent);

        let grid = gtk::Grid::new();
        grid.add_css_class("mango-dashboard-grid");
        grid.set_column_homogeneous(true);
        grid.set_row_spacing(10);
        grid.set_column_spacing(10);
        grid.attach(&weather.root(), 0, 0, 1, 1);
        grid.attach(&power.root(), 1, 0, 1, 1);
        grid.attach(&media.root(), 2, 0, 1, 1);
        grid.attach(&resources.root(), 0, 1, 2, 1);
        grid.attach(&workspaces.root(), 2, 1, 1, 1);
        grid.attach(&recent_card.0, 0, 2, 3, 1);

        // --- The panel itself.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 12);
        panel.add_css_class("mango-dashboard-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        panel.append(&header);
        panel.append(&tiles);
        panel.append(&grid);

        let flyout_panel = flyout::Flyout::new(&root);
        flyout_panel.set_child(&panel);

        // The header runs off a one-second timer: cheap enough to keep the
        // clock, date and greeting honest even while the panel is closed, so
        // it is always right the moment it opens.
        {
            let sender = sender.clone();
            glib::timeout_add_local(Duration::from_secs(1), move || {
                sender.input(DashboardInput::Tick);
                glib::ControlFlow::Continue
            });
        }

        // Used by the backdrop click handler below.
        let backdrop_sender = sender.clone();

        // Capture phase, so Escape is seen before the buttons.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let keys_sender = sender.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                keys_sender.input(DashboardInput::Hide);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        let model = DashboardModel {
            window: root.clone(),
            sender,
            flyout: flyout_panel,
            banner,
            greeting,
            clock,
            date,
            recent,
            resources,
            power,
            media,
            weather,
            workspaces,
        };

        let widgets = view_output!();

        // Clicking anywhere outside the panel — the transparent fly-out areas
        // over the open window — closes the dashboard. Clicks on the cards
        // stay inside the panel, so they pass through.
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                backdrop_sender.input(DashboardInput::Hide);
            }
        });
        root.add_controller(click);

        model.tick();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            DashboardInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            DashboardInput::Hide => self.hide(),
            DashboardInput::Banner(path) => flyout::set_banner(&self.banner, path.as_deref()),
            DashboardInput::Resources(sample) => self.resources.set(&sample),
            DashboardInput::Battery(state) => self.power.set(&state),
            DashboardInput::Track(track) => self.media.set(track.as_ref()),
            DashboardInput::Weather(report) => self.weather.set(report.as_ref()),
            DashboardInput::SetState { tags, windows } => {
                self.workspaces.set(&tags, windows);
            }
            DashboardInput::LaunchRecent(id) => self.launch_recent(&id),
            DashboardInput::Tick => self.tick(),
        }
    }
}

impl DashboardModel {
    /// Slide the panel out of the bar, waking the cards that only sample while
    /// it is open and refreshing the ones that go stale.
    ///
    /// There is nothing to type into, so no widget is focused: the surface
    /// still takes exclusive keyboard mode, which is what makes Escape land
    /// here instead of on the window underneath.
    fn show(&mut self) {
        self.resources.set_active(true);
        self.render_recent();
        self.weather.refresh_if_stale();
        self.tick();
        self.flyout.show(None);
    }

    fn hide(&mut self) {
        self.resources.set_active(false);
        self.flyout.hide();
    }

    /// Refresh the header from the local wall clock.
    ///
    /// The shell has no time crate: `localtime_r` through `libc` (already a
    /// dependency) is the whole of the conversion, and the weekday / month
    /// names are spelled out here rather than pulled from the locale so the
    /// header reads the same everywhere.
    fn tick(&self) {
        let Some(tm) = local_time() else {
            return;
        };

        self.clock
            .set_text(&format!("{:02}:{:02}", tm.tm_hour, tm.tm_min));

        const WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let wday = WDAYS
            .get(tm.tm_wday.clamp(0, 6) as usize)
            .copied()
            .unwrap_or("");
        let month = MONTHS
            .get(tm.tm_mon.clamp(0, 11) as usize)
            .copied()
            .unwrap_or("");
        self.date
            .set_text(&format!("{wday} {} {month} {}", tm.tm_mday, tm.tm_year + 1900));

        self.greeting.set_text(greeting(tm.tm_hour));
    }

    /// Rebuild the recent-apps row from what [`launcher`] has recorded.
    fn render_recent(&mut self) {
        while let Some(child) = self.recent.first_child() {
            self.recent.remove(&child);
        }

        let entries = recent_entries(RECENT_LIMIT);
        if entries.is_empty() {
            let hint = gtk::Label::new(Some("Launch something to fill this row"));
            hint.add_css_class("mango-card-note");
            self.recent.append(&hint);
            return;
        }

        for (id, name) in entries {
            let Some(app) = find_app(&id) else {
                continue;
            };
            let button = gtk::Button::new();
            button.add_css_class("mango-recent-button");
            button.set_tooltip_text(Some(&name));
            let icon = match app.icon() {
                Some(gicon) => gtk::Image::from_gicon(&gicon),
                None => gtk::Image::from_icon_name("application-x-executable-symbolic"),
            };
            icon.set_pixel_size(28);
            button.set_child(Some(&icon));

            button.connect_clicked({
                let sender = self.sender.clone();
                let id = id.clone();
                move |_| {
                    let _ = sender.input(DashboardInput::LaunchRecent(id.clone()));
                }
            });
            self.recent.append(&button);
        }
    }

    /// Launch a recent app by desktop id, hiding the panel first so the
    /// launched application ends up focused.
    fn launch_recent(&mut self, id: &str) {
        self.hide();
        let Some(app) = find_app(id) else {
            return;
        };
        match app.launch(&[], None::<&gio::AppLaunchContext>) {
            // A fresh launch jumps back to the front of the row.
            Ok(()) => launcher::record_launch(&app),
            Err(error) => eprintln!(
                "mango-shell: failed to launch {}: {error}",
                app.display_name()
            ),
        }
    }
}

/// One launcher tile: a big icon over its caption.
fn tile(icon: gtk::Image, caption: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_css_classes(&["mango-dashboard-tile"]);
    button.set_tooltip_text(Some(caption));

    let stack = gtk::Box::new(gtk::Orientation::Vertical, 8);
    stack.set_halign(gtk::Align::Center);
    stack.append(&icon);

    let label = gtk::Label::new(Some(caption));
    label.add_css_class("mango-dashboard-tile-label");
    stack.append(&label);

    button.set_child(Some(&stack));
    button
}

/// Build one info card: a titled block for the grid to drop into place.
/// Returns `(root, body)`.
pub(super) fn card(title: &str) -> (gtk::Box, gtk::Box) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("mango-card");
    root.set_hexpand(true);

    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("mango-card-title");
    heading.set_xalign(0.0);
    root.append(&heading);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_hexpand(true);
    root.append(&body);

    (root, body)
}

/// The workspaces card: one chip per tag of the focused monitor, drawn with
/// the same kanji glyphs as the bar, plus a one-line summary. Clicking a chip
/// switches to that tag, exactly like the bar's strip.
struct WorkspacesCard {
    root: gtk::Box,
    chips: gtk::Box,
    summary: gtk::Label,
}

impl WorkspacesCard {
    fn new() -> Self {
        let (root, body) = card("Workspaces");

        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chips.add_css_class("mango-workspace-chips");
        body.append(&chips);

        let summary = gtk::Label::new(Some("Waiting for Mango…"));
        summary.add_css_class("mango-card-note");
        summary.set_xalign(0.0);
        body.append(&summary);

        WorkspacesCard { root, chips, summary }
    }

    fn root(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn set(&mut self, tags: &[TagView], windows: usize) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }

        if tags.is_empty() {
            self.summary.set_text("No monitor state yet");
            return;
        }

        let mut occupied = 0usize;
        for tag in tags {
            if tag.count > 0 {
                occupied += 1;
            }

            let chip = gtk::Button::new();
            let mut classes = vec!["mango-chip"];
            if tag.count > 0 {
                classes.push("occupied");
            }
            if tag.urgent {
                classes.push("urgent");
            }
            if tag.active {
                classes.push("active");
            }
            chip.set_css_classes(&classes);
            chip.set_tooltip_text(Some(&format!(
                "Tag {} ({} window(s))",
                tag.index, tag.count
            )));
            chip.set_child(Some(&gtk::Label::new(Some(&tag_label(tag.index)))));

            let index = tag.index;
            chip.connect_clicked(move |_| {
                if mango::has_mango_session() {
                    mango::switch_tag(index);
                }
            });
            self.chips.append(&chip);
        }

        self.summary.set_text(&format!(
            "{windows} window{} · {} tag{} in use",
            if windows == 1 { "" } else { "s" },
            occupied,
            if occupied == 1 { "" } else { "s" },
        ));
    }
}

/// Read the recent-launches file (written by [`launcher::record_launch`]),
/// newest first, without duplicates and up to `limit`.
fn recent_entries(limit: usize) -> Vec<(String, String)> {
    let Some(path) = launcher::recent_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut seen = Vec::new();
    let mut entries = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split('\t');
        let (Some(_time), Some(id), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if seen.iter().any(|known| known == &id) {
            continue;
        }
        seen.push(id.to_string());
        entries.push((id.to_string(), name.to_string()));
        if entries.len() >= limit {
            break;
        }
    }
    entries
}

/// Find a launchable application by its desktop id.
fn find_app(id: &str) -> Option<gio::AppInfo> {
    gio::AppInfo::all()
        .into_iter()
        .find(|app| app.id().as_deref() == Some(id))
}

/// The local wall clock as a `struct tm`, or `None` if the conversion failed.
fn local_time() -> Option<libc::tm> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as libc::time_t;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&secs, &mut tm) };
    if result.is_null() {
        None
    } else {
        Some(tm)
    }
}

/// A day-appropriate sign-off for the header.
fn greeting(hour: i32) -> &'static str {
    match hour {
        0..=11 => "Good morning",
        12..=16 => "Good afternoon",
        17..=21 => "Good evening",
        _ => "Good night",
    }
}
