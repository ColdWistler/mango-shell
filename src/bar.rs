//! One layer-shell bar per monitor.
//!
//! The bar itself is a thin shell: it owns the widget components
//! (`TagsModel`, `ClockModel`, the tray and the system indicators) and
//! forwards messages between them and the compositor.
//!
//! Visually it follows the "hug" layout from pctrade/end4-pC: a full-width
//! translucent surface with rounded bottom corners, and each cluster of
//! widgets (launcher, workspaces, clock, status) sitting in its own rounded
//! pill (`layer 1` on top of `layer 0`). All of that lives in `style.css`.

use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::prelude::*;

use crate::mango::{self, TagView};
use crate::widgets::brightness::BrightnessModel;
use crate::widgets::clock::ClockModel;
use crate::widgets::tags::{TagsInput, TagsModel, TagsOutput};
use crate::widgets::tray::TrayModel;
use crate::widgets::volume::VolumeModel;
use crate::widgets::wifi::WifiModel;

/// Everything a bar needs to exist.
pub struct BarInit {
    /// Monitor connector name, e.g. `DP-1`.
    pub name: String,
    /// The GDK monitor this bar is drawn on.
    pub monitor: gdk::Monitor,
}

#[derive(Debug)]
pub enum BarInput {
    /// Replace the tag state for this monitor.
    SetTags(Vec<TagView>),
    /// A tag button was clicked.
    SwitchTag(TagsOutput),
}

/// Messages the bar sends up to the shell.
#[derive(Debug)]
pub enum BarOutput {
    /// The launcher button was pressed.
    ToggleLauncher,
    /// The wallpaper button was pressed.
    ToggleWallpaper,
}

pub struct BarModel {
    name: String,
    launcher_button: gtk::Button,
    wallpaper_button: gtk::Button,
    tags: Controller<TagsModel>,
    clock: Controller<ClockModel>,
    tray: Controller<TrayModel>,
    wifi: Controller<WifiModel>,
    volume: Controller<VolumeModel>,
    brightness: Controller<BrightnessModel>,
}

#[relm4::component(pub)]
impl SimpleComponent for BarModel {
    type Init = BarInit;
    type Input = BarInput;
    type Output = BarOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-bar"],

            gtk::CenterBox {
                set_css_classes: &["mango-bar-surface"],

                // Left: launcher button + the tag (workspace) pills.
                #[wrap(Some)]
                set_start_widget = &gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 6,
                    set_halign: gtk::Align::Start,
                    set_valign: gtk::Align::Center,
                    set_margin_start: 8,

                    append = &model.launcher_button.clone() {},
                    append = &model.wallpaper_button.clone() {},

                    append = &gtk::Box {
                        set_css_classes: &["mango-group"],
                        set_orientation: gtk::Orientation::Horizontal,
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Center,
                        append = &model.tags.widget().clone() {},
                    },
                },

                // Center: the clock, in its own pill.
                #[wrap(Some)]
                set_center_widget = &gtk::Box {
                    set_css_classes: &["mango-group"],
                    set_orientation: gtk::Orientation::Horizontal,
                    set_halign: gtk::Align::Center,
                    set_valign: gtk::Align::Center,
                    append = &model.clock.widget().clone() {},
                },

                // Right: tray, system indicators and the monitor name.
                #[wrap(Some)]
                set_end_widget = &gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 6,
                    set_halign: gtk::Align::End,
                    set_valign: gtk::Align::Center,
                    set_margin_end: 8,

                    // Tray icons, in their own pill (it hides itself when empty).
                    append = &model.tray.widget().clone() {},

                    // Wi-Fi, volume and brightness.
                    append = &gtk::Box {
                        set_css_classes: &["mango-group", "mango-system"],
                        set_orientation: gtk::Orientation::Horizontal,
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Center,
                        append = &model.wifi.widget().clone() {},
                        append = &model.volume.widget().clone() {},
                        append = &model.brightness.widget().clone() {},
                    },

                    // Which monitor this bar belongs to.
                    append = &gtk::Box {
                        set_css_classes: &["mango-group"],
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Center,
                        append = &gtk::Label {
                            set_css_classes: &["mango-status"],
                            #[watch]
                            set_label: &model.name,
                        },
                    },
                },
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Turn the window into a top-of-screen bar on `init.monitor` before it
        // is realized.
        root.init_layer_shell();
        root.set_monitor(Some(&init.monitor));
        root.set_namespace(Some("mango-bar"));
        root.set_layer(Layer::Top);
        // Reserve exactly the bar's height so tiled windows never overlap it.
        root.auto_exclusive_zone_enable();
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);
        root.set_decorated(false);

        // The launcher lives in the shell, so the bar just reports the click.
        let launcher_button = gtk::Button::new();
        launcher_button.set_css_classes(&["mango-icon-button"]);
        launcher_button.set_tooltip_text(Some("Applications"));
        launcher_button.set_child(Some(&gtk::Image::from_icon_name("view-app-grid-symbolic")));
        let click_sender = sender.clone();
        launcher_button.connect_clicked(move |_| {
            let _ = click_sender.output(BarOutput::ToggleLauncher);
        });

        // The wallpaper picker also lives in the shell.
        let wallpaper_button = gtk::Button::new();
        wallpaper_button.set_css_classes(&["mango-icon-button"]);
        wallpaper_button.set_tooltip_text(Some("Wallpaper"));
        wallpaper_button.set_child(Some(&gtk::Image::from_icon_name(
            "preferences-desktop-wallpaper-symbolic",
        )));
        let wallpaper_sender = sender.clone();
        wallpaper_button.connect_clicked(move |_| {
            let _ = wallpaper_sender.output(BarOutput::ToggleWallpaper);
        });

        let tags = TagsModel::builder()
            .launch(())
            .forward(sender.input_sender(), BarInput::SwitchTag);
        let clock = ClockModel::builder().launch(()).detach();
        let tray = TrayModel::builder().launch(()).detach();
        let wifi = WifiModel::builder().launch(()).detach();
        let volume = VolumeModel::builder().launch(()).detach();
        let brightness = BrightnessModel::builder().launch(()).detach();

        let model = BarModel {
            name: init.name,
            launcher_button,
            wallpaper_button,
            tags,
            clock,
            tray,
            wifi,
            volume,
            brightness,
        };

        let widgets = view_output!();

        // Child windows are not presented automatically.
        root.set_visible(true);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            BarInput::SetTags(tags) => self.tags.emit(TagsInput::Set(tags)),
            BarInput::SwitchTag(TagsOutput::Switch(index)) => mango::switch_tag(index),
        }
    }
}
