//! One layer-shell bar per monitor.
//!
//! The bar itself is a thin shell: it owns the widget components
//! (`TagsModel`, `ClockModel`, the tray and the system indicators) and
//! forwards messages between them and the compositor.
//!
//! Which widgets sit where is decided by the active **panel**
//! ([`crate::panel`]), a layout profile ported from
//! [adi1090x/polybar-themes](https://github.com/adi1090x/polybar-themes). The
//! clusters (launcher buttons, tags, clock, tray, system indicators, monitor
//! name) are built here imperatively, each wrapped the same way as the old
//! fixed bar, then distributed into the CentreBox slots the panel asked for,
//! with optional [`Sep`] separators between them. The `panel-<id>` class on
//! the window lets each panel's CSS restyle everything (pill vs flat blocks,
//! height, radius, palette).

use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::prelude::*;

use crate::mango::{self, TagView};
use crate::panel::{Panel, Sep, Slot};
use crate::widgets::brightness::BrightnessModel;
use crate::widgets::clock::ClockModel;
use crate::widgets::tags::{TagsInput, TagsModel, TagsOutput};
use crate::widgets::theme::{ThemeButtonInput, ThemeButtonModel, ThemeButtonOutput};
use crate::widgets::tray::TrayModel;
use crate::widgets::volume::VolumeModel;
use crate::widgets::wifi::WifiModel;

/// Everything a bar needs to exist.
pub struct BarInit {
    /// Monitor connector name, e.g. `DP-1`.
    pub name: String,
    /// The GDK monitor this bar is drawn on.
    pub monitor: gdk::Monitor,
    /// The theme id active at launch, so the popover checkmark is right.
    pub theme: String,
    /// The panel (bar layout) this bar is built from.
    pub panel: &'static Panel,
    /// The layout symbol for this monitor (e.g. "Tiled", "Scroller", "Monocle").
    pub layout: String,
}

#[derive(Debug)]
pub enum BarInput {
    /// Replace the tag state for this monitor.
    SetTags(Vec<TagView>),
    /// A tag button was clicked.
    SwitchTag(TagsOutput),
    /// The active theme changed; sync the popover checkmark.
    SetTheme(String),
}

/// Messages the bar sends up to the shell.
#[derive(Debug)]
pub enum BarOutput {
    /// The launcher button was pressed.
    ToggleLauncher,
    /// The wallpaper button was pressed.
    ToggleWallpaper,
    /// The theme button's gallery row was pressed.
    ToggleThemePicker,
    /// The panel picker button was pressed.
    TogglePanelPicker,
    /// The shortcuts button was pressed.
    ToggleShortcuts,
    /// A theme was picked in the bar's popover.
    SetTheme(String),
}

pub struct BarModel {
    tags: Controller<TagsModel>,
    // The following components are wired into the slot boxes during `init`;
    // their Controllers are kept here (and in the view it is enough to hold
    // the slot boxes) purely so the child components stay alive. Relm4 shuts
    // a component down when its Controller is dropped.
    #[allow(dead_code)]
    clock: Controller<ClockModel>,
    #[allow(dead_code)]
    tray: Controller<TrayModel>,
    #[allow(dead_code)]
    wifi: Controller<WifiModel>,
    #[allow(dead_code)]
    volume: Controller<VolumeModel>,
    #[allow(dead_code)]
    brightness: Controller<BrightnessModel>,
    theme_button: Controller<ThemeButtonModel>,
    /// The three CentreBox slots, already populated from the panel layout.
    start_box: gtk::Box,
    center_box: gtk::Box,
    end_box: gtk::Box,
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

                // The slot boxes are fully built in `init` from the panel
                // layout, so the declarative part stays this small.
                #[wrap(Some)]
                set_start_widget = &model.start_box.clone() {},

                #[wrap(Some)]
                set_center_widget = &model.center_box.clone() {},

                #[wrap(Some)]
                set_end_widget = &model.end_box.clone() {},
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let panel = init.panel;

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

        // The launcher, wallpaper picker and panel picker all live in the
        // shell, so the bar just reports their clicks.
        let launcher_button = gtk::Button::new();
        launcher_button.set_css_classes(&["mango-icon-button"]);
        launcher_button.set_tooltip_text(Some("Applications"));
        launcher_button.set_child(Some(&gtk::Image::from_icon_name("view-app-grid-symbolic")));
        let click_sender = sender.clone();
        launcher_button.connect_clicked(move |_| {
            let _ = click_sender.output(BarOutput::ToggleLauncher);
        });

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

        let panels_button = gtk::Button::new();
        panels_button.set_css_classes(&["mango-icon-button"]);
        panels_button.set_tooltip_text(Some("Panel layout"));
        panels_button.set_child(Some(&gtk::Image::from_icon_name("view-grid-symbolic")));
        let panels_sender = sender.clone();
        panels_button.connect_clicked(move |_| {
            let _ = panels_sender.output(BarOutput::TogglePanelPicker);
        });

        let tags = TagsModel::builder()
            .launch(())
            .forward(sender.input_sender(), BarInput::SwitchTag);
        let clock = ClockModel::builder().launch(()).detach();
        let tray = TrayModel::builder().launch(()).detach();
        let wifi = WifiModel::builder().launch(()).detach();
        let volume = VolumeModel::builder().launch(()).detach();
        let brightness = BrightnessModel::builder().launch(()).detach();
        // The theme menu lives in the bar but the stylesheet lives in the
        // shell, so its choices go straight to the bar's output.
        let theme_button = ThemeButtonModel::builder().launch(init.theme).forward(
            sender.output_sender(),
            |output| match output {
                ThemeButtonOutput::Apply(id) => BarOutput::SetTheme(id),
                ThemeButtonOutput::OpenGallery => BarOutput::ToggleThemePicker,
            },
        );

        // --- Build the clusters exactly like the old fixed bar did, so every
        // panel starts from the same building blocks and CSS handles the
        // rest. ----------------------------------------------------------

        // The launcher/wallpaper/theme/panel buttons sit together, bare.
        let apps = horizontal_cluster(&[], 6, gtk::Align::Center);
        apps.append(&launcher_button);
        apps.append(&wallpaper_button);
        apps.append(theme_button.widget());
        apps.append(&panels_button);

        // Workspace pills.
        let tags_cluster = horizontal_cluster(&["mango-group"], 0, gtk::Align::Center);
        tags_cluster.append(tags.widget());

        // The clock in its own pill.
        let clock_cluster = horizontal_cluster(&["mango-group"], 0, gtk::Align::Center);
        clock_cluster.append(clock.widget());

        // The tray brings its own `.mango-group` (and hides itself when
        // empty), so it is appended as-is.
        let tray_widget = tray.widget().clone().upcast::<gtk::Widget>();

        // Wi-Fi, volume and brightness in one pill.
        let system_cluster =
            horizontal_cluster(&["mango-group", "mango-system"], 0, gtk::Align::Center);
        system_cluster.append(wifi.widget());
        system_cluster.append(volume.widget());
        system_cluster.append(brightness.widget());

        // Which monitor this bar belongs to.
        let desk_cluster = horizontal_cluster(&["mango-group"], 0, gtk::Align::Center);
        let name_label = gtk::Label::new(Some(&init.layout));
        name_label.add_css_class("mango-status");
        desk_cluster.append(&name_label);

        // --- Distribute them into the slots the panel asked for. ----------
        let members: Vec<(Slot, gtk::Widget)> = vec![
            (panel.apps, apps.upcast()),
            (panel.tags, tags_cluster.upcast()),
            (panel.clock, clock_cluster.upcast()),
            (panel.tray, tray_widget),
            (panel.system, system_cluster.upcast()),
            (panel.desk, desk_cluster.upcast()),
        ];

        let mut start = Vec::new();
        let mut center = Vec::new();
        let mut end = Vec::new();
        for (slot, widget) in members {
            match slot {
                Slot::Start => start.push(widget),
                Slot::Center => center.push(widget),
                Slot::End => end.push(widget),
            }
        }

        let start_box = assemble_slot("mango-slot-start", start, panel.sep, gtk::Align::Start);
        let center_box = assemble_slot("mango-slot-center", center, panel.sep, gtk::Align::Center);
        let end_box = assemble_slot("mango-slot-end", end, panel.sep, gtk::Align::End);

        let model = BarModel {
            tags,
            clock,
            tray,
            wifi,
            volume,
            brightness,
            theme_button,
            start_box,
            center_box,
            end_box,
        };

        let widgets = view_output!();

        // The panel's CSS profile is scoped to `panel-<id>`. It is added
        // after `view_output!` because the view macro replaces the css classes
        // list on the root.
        root.add_css_class(&format!("panel-{}", panel.id));

        // Floating panels inset themselves from the top and the screen edges
        // (full-width panels have `margin_side == 0`).
        root.set_margin(Edge::Top, panel.margin_top);
        root.set_margin(Edge::Left, panel.margin_side);
        root.set_margin(Edge::Right, panel.margin_side);

        // Child windows are not presented automatically.
        root.set_visible(true);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            BarInput::SetTags(tags) => self.tags.emit(TagsInput::Set(tags)),
            BarInput::SwitchTag(TagsOutput::Switch(index)) => mango::switch_tag(index),
            BarInput::SetTheme(id) => self.theme_button.emit(ThemeButtonInput::SetTheme(id)),
        }
    }
}

/// A horizontal box with the given CSS classes, centred vertically so the
/// clusters line up in a short bar.
fn horizontal_cluster(classes: &[&str], spacing: i32, halign: gtk::Align) -> gtk::Box {
    let cluster = gtk::Box::new(gtk::Orientation::Horizontal, spacing);
    cluster.set_css_classes(classes);
    cluster.set_halign(halign);
    cluster.set_valign(gtk::Align::Center);
    cluster
}

/// Chain `members` into one slot box with 6px spacing and `panel.sep`
/// separators between consecutive members.
fn assemble_slot(class: &str, members: Vec<gtk::Widget>, sep: Sep, halign: gtk::Align) -> gtk::Box {
    let slot = horizontal_cluster(&[class], 6, halign);
    for (index, member) in members.iter().enumerate() {
        if index > 0
            && let Some(separator) = separator(sep)
        {
            slot.append(&separator);
        }
        slot.append(member);
    }
    slot
}

/// The separator widget for a [`Sep`] style, if it has one.
fn separator(sep: Sep) -> Option<gtk::Widget> {
    match sep {
        Sep::None => None,
        Sep::Line => {
            // A plain box: `GtkSeparator` is drawn by the theme and ignores
            // `background-color`, so a 1px box is the reliable hairline.
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.add_css_class("mango-sep");
            line.set_valign(gtk::Align::Center);
            Some(line.upcast())
        }
        Sep::Block => {
            let block = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            block.add_css_class("mango-sep-block");
            block.set_valign(gtk::Align::Center);
            Some(block.upcast())
        }
        Sep::Glyph(glyph) => {
            let label = gtk::Label::new(Some(glyph));
            label.add_css_class("mango-sep-glyph");
            Some(label.upcast())
        }
    }
}
