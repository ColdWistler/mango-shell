//! The dashboard: the shell's front door.
//!
//! The bar's Arch mark opens this panel instead of the launcher directly, so
//! the corner of the bar leads somewhere rather than to a single app grid. It
//! is one fly-out built with the shared recipe (see [`crate::flyout`]): an
//! `Overlay`-layer surface hanging under the bar whose card slides + fades out
//! of it, with the current wallpaper pinned on top like every other fly-out.
//!
//! The card is deliberately small — a title over two tiles:
//!
//! * **Applications** (the Arch mark again) hands off to the real
//!   [`crate::launcher`]. The shell closes this panel as it opens the launcher,
//!   so only one fly-out ever hangs under the bar.
//! * **Settings** is the seam the settings app will grow into: pressing it
//!   already routes to the shell ([`DashboardOutput::OpenSettings`]), which
//!   today just closes the dashboard again.
//!
//! Escape or a click anywhere outside the card closes it, exactly like the
//! launcher.

use std::path::PathBuf;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;

/// Edge of the Arch mark on the Applications tile, in pixels.
const LOGO_SIZE: i32 = 44;

pub struct DashboardModel {
    /// The root, cloned so `update` can show/hide it.
    window: gtk::Window,
    /// Slides + fades the panel out from under the bar.
    flyout: flyout::Flyout,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
}

#[derive(Debug)]
pub enum DashboardInput {
    /// Flip visibility (sent by the bar's Arch-mark button).
    Toggle,
    /// Hide without opening anything (Escape or a backdrop click).
    Hide,
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
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

        // The two tiles. The Applications one reuses the bar's Arch mark so
        // the button the user pressed and the tile it leads to read as the
        // same object.
        let apps_button = tile(crate::bar::arch_logo(LOGO_SIZE), "Applications");
        let settings_button = tile(
            {
                let image = gtk::Image::from_icon_name("preferences-system-symbolic");
                image.set_pixel_size(LOGO_SIZE);
                image
            },
            "Settings",
        );

        // The shell decides what each tile opens (and closes this panel), so
        // the tiles only report the click.
        let apps_sender = sender.clone();
        apps_button.connect_clicked(move |_| {
            let _ = apps_sender.output(DashboardOutput::OpenLauncher);
        });
        let settings_sender = sender.clone();
        settings_button.connect_clicked(move |_| {
            let _ = settings_sender.output(DashboardOutput::OpenSettings);
        });

        let title = gtk::Label::new(Some("Dashboard"));
        title.add_css_class("mango-dashboard-title");

        let tiles = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        tiles.add_css_class("mango-dashboard-tiles");
        tiles.append(&apps_button);
        tiles.append(&settings_button);

        // The panel: wallpaper banner, the title, then the tiles.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 12);
        panel.add_css_class("mango-dashboard-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        panel.append(&title);
        panel.append(&tiles);

        let flyout_panel = flyout::Flyout::new(&root);
        flyout_panel.set_child(&panel);

        // Used by the backdrop click handler below (the key controller takes
        // ownership of `sender` first).
        let backdrop_sender = sender.clone();

        // Capture phase, so Escape is seen before the buttons.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                sender.input(DashboardInput::Hide);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        let model = DashboardModel {
            window: root.clone(),
            flyout: flyout_panel,
            banner,
        };

        let widgets = view_output!();

        // Clicking anywhere outside the panel — the transparent fly-out areas
        // over the open window — closes the dashboard. Clicks on the tiles
        // stay inside the panel, so they pass through.
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                backdrop_sender.input(DashboardInput::Hide);
            }
        });
        root.add_controller(click);

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
        }
    }
}

impl DashboardModel {
    /// Slide the panel out of the bar.
    ///
    /// There is nothing to type into, so no widget is focused: the surface
    /// still takes exclusive keyboard mode, which is what makes Escape land
    /// here instead of on the window underneath.
    fn show(&mut self) {
        self.flyout.show(None);
    }

    fn hide(&mut self) {
        self.flyout.hide();
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
