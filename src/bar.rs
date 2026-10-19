//! One layer-shell bar per monitor.
//!
//! The bar itself is a thin shell: it owns the widget components
//! (`TagsModel`, `ClockModel`, the tray and the system indicators) and
//! forwards messages between them and the compositor.
//!
//! Which widgets sit where is decided by two things: the active **panel**
//! ([`crate::panel`]) — a layout profile ported from
//! [adi1090x/polybar-themes](https://github.com/adi1090x/polybar-themes),
//! which brings the palette, height and separator style — and the widget
//! **layout** ([`crate::settings`]) the settings app edits: every widget is
//! built once, keyed by [`WidgetId`], and arranged into the `Start` /
//! `Center` / `End` slots with optional [`Sep`] separators between them.
//! The `panel-<id>` class on the window lets each panel's CSS restyle
//! everything (pill vs flat blocks, height, radius, palette), and the
//! `mango-user-shaped` class marks a bar the user has given explicit shape
//! overrides, so those win ties against the panel's rules.

use std::collections::HashMap;

use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::prelude::*;

use crate::mango::{self, TagView};
use crate::panel::{Panel, Sep, Slot};
use crate::settings::{LayoutEntry, Shape};
use crate::widgets::brightness::BrightnessModel;
use crate::widgets::clock::ClockModel;
use crate::widgets::music::MusicModel;
use crate::widgets::power::PowerModel;
use crate::widgets::resources::ResourcesModel;
use crate::widgets::tags::{TagsInput, TagsModel, TagsOutput};
use crate::widgets::theme::{ThemeButtonInput, ThemeButtonModel, ThemeButtonOutput};
use crate::widgets::tray::TrayModel;
use crate::widgets::visualizer::VisualizerModel;
use crate::widgets::volume::VolumeModel;
use crate::widgets::wifi::WifiModel;

/// Everything a bar needs to exist.
pub struct BarInit {
    /// The GDK monitor this bar is drawn on.
    pub monitor: gdk::Monitor,
    /// The theme id active at launch, so the popover checkmark is right.
    pub theme: String,
    /// The panel (bar layout) this bar is built from.
    pub panel: &'static Panel,
    /// The widget layout the settings app last applied.
    pub layout: Vec<LayoutEntry>,
    /// The user's bar-shape overrides, when they have one.
    pub shape: Option<Shape>,
}

/// One movable widget on the bar.
///
/// The bar builds every widget once and arranges them according to the
/// layout the settings app edits ([`crate::settings`]); the id is the stable
/// name persisted in `~/.config/mango-shell/settings.conf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidgetId {
    Dashboard,
    Theme,
    Panels,
    Overview,
    Shortcuts,
    Music,
    Visualizer,
    Tags,
    Resources,
    Clock,
    Wifi,
    Volume,
    Brightness,
    Power,
    Tray,
}

impl WidgetId {
    /// Every widget, in default order.
    pub const ALL: [WidgetId; 15] = [
        WidgetId::Dashboard,
        WidgetId::Theme,
        WidgetId::Panels,
        WidgetId::Overview,
        WidgetId::Shortcuts,
        WidgetId::Music,
        WidgetId::Visualizer,
        WidgetId::Tags,
        WidgetId::Resources,
        WidgetId::Clock,
        WidgetId::Wifi,
        WidgetId::Volume,
        WidgetId::Brightness,
        WidgetId::Power,
        WidgetId::Tray,
    ];

    /// The name shown in the settings app.
    pub fn label(self) -> &'static str {
        match self {
            WidgetId::Dashboard => "Dashboard",
            WidgetId::Theme => "Themes",
            WidgetId::Panels => "Panel layout",
            WidgetId::Overview => "Overview",
            WidgetId::Shortcuts => "Shortcuts",
            WidgetId::Music => "Now playing",
            WidgetId::Visualizer => "Visualizer",
            WidgetId::Tags => "Workspaces",
            WidgetId::Resources => "Resources",
            WidgetId::Clock => "Clock",
            WidgetId::Wifi => "Wi-Fi",
            WidgetId::Volume => "Volume",
            WidgetId::Brightness => "Brightness",
            WidgetId::Power => "Power",
            WidgetId::Tray => "System tray",
        }
    }

    /// The stable key persisted on disk.
    pub fn key(self) -> &'static str {
        match self {
            WidgetId::Dashboard => "dashboard",
            WidgetId::Theme => "theme",
            WidgetId::Panels => "panels",
            WidgetId::Overview => "overview",
            WidgetId::Shortcuts => "shortcuts",
            WidgetId::Music => "music",
            WidgetId::Visualizer => "visualizer",
            WidgetId::Tags => "tags",
            WidgetId::Resources => "resources",
            WidgetId::Clock => "clock",
            WidgetId::Wifi => "wifi",
            WidgetId::Volume => "volume",
            WidgetId::Brightness => "brightness",
            WidgetId::Power => "power",
            WidgetId::Tray => "tray",
        }
    }

    /// Look a widget up by its persisted key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.key() == key)
    }
}

#[derive(Debug)]
pub enum BarInput {
    /// Replace the tag state for this monitor.
    SetTags(Vec<TagView>),
    /// A tag button was clicked.
    SwitchTag(TagsOutput),
    /// The active theme changed; sync the popover checkmark.
    SetTheme(String),
    /// A new widget layout from the settings app; rebuild the slot boxes.
    SetLayout(Vec<LayoutEntry>),
    /// The bar-shape overrides changed: toggle the `mango-user-shaped` class
    /// the stylesheet's override block keys off.
    SetShape(Option<Shape>),
}

/// Messages the bar sends up to the shell.
#[derive(Debug)]
pub enum BarOutput {
    /// The dashboard button (the Arch mark) was pressed.
    ToggleDashboard,
    /// The theme button's gallery row was pressed.
    ToggleThemePicker,
    /// The panel picker button was pressed.
    TogglePanelPicker,
    /// The shortcuts button was pressed.
    ToggleShortcuts,
    /// The overview button was pressed.
    ToggleOverview,
    /// A theme was picked in the bar's popover.
    SetTheme(String),
}

pub struct BarModel {
    /// The root, so `update` can toggle the user-shape class.
    window: gtk::Window,
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
    #[allow(dead_code)]
    resources: Controller<ResourcesModel>,
    #[allow(dead_code)]
    power: Controller<PowerModel>,
    #[allow(dead_code)]
    visualizer: Controller<VisualizerModel>,
    #[allow(dead_code)]
    music: Controller<MusicModel>,
    theme_button: Controller<ThemeButtonModel>,
    /// Every movable widget, built once and keyed: the layout decides where
    /// (and whether) each one appears.
    map: HashMap<WidgetId, gtk::Widget>,
    /// The layout the settings app last applied.
    entries: Vec<LayoutEntry>,
    /// Separator style between consecutive widgets inside a slot.
    sep: Sep,
    /// The three CentreBox slots, filled from the layout by `rebuild`.
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

        // The dashboard, wallpaper carousel and panel picker all live in the
        // shell, so the bar just reports their clicks. The dashboard button
        // wears the Arch mark with no chip behind it, so the corner of the bar
        // reads as a bare logo instead of an app-grid glyph on a box — and
        // like the mark itself, it now leads somewhere bigger than one app
        // grid: the dashboard panel (see `dashboard.rs`).
        let dashboard_button = gtk::Button::new();
        dashboard_button.set_css_classes(&["mango-dashboard-button"]);
        dashboard_button.set_tooltip_text(Some("Dashboard"));
        dashboard_button.set_child(Some(&arch_logo(ARCH_LOGO_SIZE)));
        let click_sender = sender.clone();
        dashboard_button.connect_clicked(move |_| {
            let _ = click_sender.output(BarOutput::ToggleDashboard);
        });

        let panels_button = gtk::Button::new();
        panels_button.set_css_classes(&["mango-icon-button"]);
        panels_button.set_tooltip_text(Some("Panel layout"));
        panels_button.set_child(Some(&gtk::Image::from_icon_name("view-grid-symbolic")));
        let panels_sender = sender.clone();
        panels_button.connect_clicked(move |_| {
            let _ = panels_sender.output(BarOutput::TogglePanelPicker);
        });

        // The workspace overview (all tags of the focused monitor, with every
        // window drawn to scale). Also reachable from a keybind — see
        // `spawn_signal_watcher` in main.rs.
        let overview_button = gtk::Button::new();
        overview_button.set_css_classes(&["mango-icon-button"]);
        overview_button.set_tooltip_text(Some("Workspace overview"));
        overview_button.set_child(Some(&gtk::Image::from_icon_name("focus-windows-symbolic")));
        let overview_sender = sender.clone();
        overview_button.connect_clicked(move |_| {
            let _ = overview_sender.output(BarOutput::ToggleOverview);
        });

        // The keyboard-shortcut cheat-sheet the shell builds from the keybinds
        // it knows about.
        let shortcuts_button = gtk::Button::new();
        shortcuts_button.set_css_classes(&["mango-icon-button"]);
        shortcuts_button.set_tooltip_text(Some("Keyboard shortcuts"));
        shortcuts_button.set_child(Some(&gtk::Image::from_icon_name(
            "preferences-desktop-keyboard-shortcuts-symbolic",
        )));
        let shortcuts_sender = sender.clone();
        shortcuts_button.connect_clicked(move |_| {
            let _ = shortcuts_sender.output(BarOutput::ToggleShortcuts);
        });

        let tags = TagsModel::builder()
            .launch(())
            .forward(sender.input_sender(), BarInput::SwitchTag);
        let clock = ClockModel::builder().launch(()).detach();
        let tray = TrayModel::builder().launch(()).detach();
        let wifi = WifiModel::builder().launch(()).detach();
        let volume = VolumeModel::builder().launch(()).detach();
        let brightness = BrightnessModel::builder().launch(()).detach();
        // CPU / memory / GPU readouts with a sparkline popover. Reads sysfs and
        // `nvidia-smi` directly, so it needs no daemon.
        let resources = ResourcesModel::builder().launch(()).detach();
        // Battery + the session menu (lock / log out / suspend / reboot /
        // shut down); always visible, so a desktop without a battery still
        // gets the power menu.
        let power = PowerModel::builder().launch(()).detach();
        // The audio visualizer owns its own pill (it hides as a whole when
        // `cava` is unavailable), so it is appended as-is below.
        let visualizer = VisualizerModel::builder().launch(()).detach();
        // "Now playing" over MPRIS: reads whichever player owns the sound, so
        // it works the same for Sonora as for Spotify, VLC, mpv or a browser
        // tab. It also owns its pill and hides itself while nothing plays.
        let music = MusicModel::builder().launch(()).detach();
        // The theme menu lives in the bar but the stylesheet lives in the
        // shell, so its choices go straight to the bar's output.
        let theme_button = ThemeButtonModel::builder().launch(init.theme).forward(
            sender.output_sender(),
            |output| match output {
                ThemeButtonOutput::Apply(id) => BarOutput::SetTheme(id),
                ThemeButtonOutput::OpenGallery => BarOutput::ToggleThemePicker,
            },
        );

        // --- Every movable widget, built once and keyed by id: the layout
        // the settings app edits decides where (and whether) each one shows
        // up (see `crate::settings`). The default arrangement reproduces the
        // old fixed bar: launcher buttons, music and the visualizer on the
        // left, workspaces centred, system widgets on the right.
        let mut map: HashMap<WidgetId, gtk::Widget> = HashMap::new();
        map.insert(WidgetId::Dashboard, dashboard_button.upcast());
        map.insert(
            WidgetId::Theme,
            theme_button.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(WidgetId::Panels, panels_button.upcast());
        map.insert(WidgetId::Overview, overview_button.upcast());
        map.insert(WidgetId::Shortcuts, shortcuts_button.upcast());
        map.insert(
            WidgetId::Music,
            music.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(
            WidgetId::Visualizer,
            visualizer.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(WidgetId::Tags, tags.widget().clone().upcast::<gtk::Widget>());
        map.insert(
            WidgetId::Resources,
            resources.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(WidgetId::Clock, clock.widget().clone().upcast::<gtk::Widget>());
        map.insert(WidgetId::Wifi, wifi.widget().clone().upcast::<gtk::Widget>());
        map.insert(
            WidgetId::Volume,
            volume.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(
            WidgetId::Brightness,
            brightness.widget().clone().upcast::<gtk::Widget>(),
        );
        map.insert(WidgetId::Power, power.widget().clone().upcast::<gtk::Widget>());
        map.insert(WidgetId::Tray, tray.widget().clone().upcast::<gtk::Widget>());

        let start_box = slot_box("mango-slot-start", gtk::Align::Start);
        let center_box = slot_box("mango-slot-center", gtk::Align::Center);
        let end_box = slot_box("mango-slot-end", gtk::Align::End);

        let model = BarModel {
            window: root.clone(),
            tags,
            clock,
            tray,
            wifi,
            volume,
            brightness,
            resources,
            power,
            visualizer,
            music,
            theme_button,
            map,
            entries: init.layout,
            sep: panel.sep,
            start_box,
            center_box,
            end_box,
        };

        let widgets = view_output!();

        // The panel's CSS profile is scoped to `panel-<id>`. It is added
        // after `view_output!` because the view macro replaces the css classes
        // list on the root.
        root.add_css_class(&format!("panel-{}", panel.id));
        // The settings app's shape overrides are scoped to this class, so
        // they can win ties against the panel's own radius / height rules
        // (see `settings::Shape::css`).
        if init.shape.is_some() {
            root.add_css_class("mango-user-shaped");
        }

        // Floating panels inset themselves from the top and the screen edges
        // (full-width panels have `margin_side == 0`).
        root.set_margin(Edge::Top, panel.margin_top);
        root.set_margin(Edge::Left, panel.margin_side);
        root.set_margin(Edge::Right, panel.margin_side);

        // Lay the widgets out for the first time, from the saved layout.
        model.rebuild();

        // Child windows are not presented automatically.
        root.set_visible(true);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            BarInput::SetTags(tags) => self.tags.emit(TagsInput::Set(tags)),
            BarInput::SwitchTag(TagsOutput::Switch(index)) => {
                if crate::mango::has_mango_session() {
                    mango::switch_tag(index);
                }
            }
            BarInput::SetTheme(id) => self.theme_button.emit(ThemeButtonInput::SetTheme(id)),
            BarInput::SetLayout(layout) => {
                self.entries = layout;
                self.rebuild();
            }
            BarInput::SetShape(shape) => {
                if shape.is_some() {
                    self.window.add_css_class("mango-user-shaped");
                } else {
                    self.window.remove_css_class("mango-user-shaped");
                }
            }
        }
    }
}

impl BarModel {
    /// Re-lay the three slot boxes out from `entries`: visible widgets in
    /// order, Start / Center / End, with `panel.sep` separators between them.
    ///
    /// The Center and End slots keep the cluster classes the fixed bar gave
    /// them (`mango-group`, plus `mango-system` on the right), so the panels'
    /// pill CSS still finds its groups even though the widgets inside are
    /// now individually movable.
    fn rebuild(&self) {
        for (slot, classes) in [
            (Slot::Start, &["mango-slot-start"][..]),
            (Slot::Center, &["mango-slot-center", "mango-group"][..]),
            (
                Slot::End,
                &["mango-slot-end", "mango-group", "mango-system"][..],
            ),
        ] {
            let container = match slot {
                Slot::Start => &self.start_box,
                Slot::Center => &self.center_box,
                Slot::End => &self.end_box,
            };
            while let Some(child) = container.first_child() {
                container.remove(&child);
            }
            container.set_css_classes(classes);

            let members: Vec<gtk::Widget> = self
                .entries
                .iter()
                .filter(|entry| entry.slot == slot && entry.visible)
                .filter_map(|entry| self.map.get(&entry.id))
                .cloned()
                .collect();
            append_members(container, &members, self.sep);
        }
    }
}

/// An empty slot box: one horizontal row, centred vertically so the widgets
/// line up in a short bar.
fn slot_box(class: &str, halign: gtk::Align) -> gtk::Box {
    let slot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    slot.set_halign(halign);
    slot.set_valign(gtk::Align::Center);
    slot.set_css_classes(&[class]);
    slot
}

/// Edge of the dashboard button's Arch mark, in pixels.
const ARCH_LOGO_SIZE: i32 = 18;

/// The Arch Linux mark, scaled so its longest edge is `size` pixels.
///
/// The image is embedded under `assets/` rather than read from
/// `/usr/share/pixmaps`, so the shell looks the same on any host and does
/// not depend on the distro's artwork package being installed. The scale is
/// done here rather than with `set_pixel_size` because that only resizes
/// icon-name images — a pixbuf keeps whatever size it was decoded at, which
/// would leave the dashboard's big tile mark at the PNG's natural size.
pub fn arch_logo(size: i32) -> gtk::Image {
    const PNG: &[u8] = include_bytes!("../assets/archlinux-logo.png");
    let image = match gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(PNG)) {
        Ok(pixbuf) => {
            let (width, height) = (pixbuf.width(), pixbuf.height());
            // Ceiling division by hand: `i32::div_ceil` is still unstable.
            let (target_width, target_height) = if width >= height {
                let divisor = width.max(1);
                (size, (size * height + divisor - 1) / divisor)
            } else {
                let divisor = height.max(1);
                ((size * width + divisor - 1) / divisor, size)
            };
            let scaled = if (width, height) == (target_width, target_height) {
                pixbuf
            } else {
                pixbuf
                    .scale_simple(
                        target_width,
                        target_height.max(1),
                        gtk::gdk_pixbuf::InterpType::Bilinear,
                    )
                    .unwrap_or_else(|| pixbuf.clone())
            };
            gtk::Image::from_pixbuf(Some(&scaled))
        }
        // A damaged asset must not take the whole bar down.
        Err(_) => gtk::Image::from_icon_name("view-app-grid-symbolic"),
    };
    image.set_pixel_size(size);
    image
}

/// Chain `members` into `slot` with `sep` separators between consecutive
/// members.
///
/// Each separator tracks the member *following* it, so a widget that hides
/// itself (the now-playing pill while nothing plays, the visualizer without
/// `cava`) takes its leading separator with it instead of leaving a dangling
/// line at the edge of the slot.
fn append_members(slot: &gtk::Box, members: &[gtk::Widget], sep: Sep) {
    for (index, member) in members.iter().enumerate() {
        if index > 0
            && let Some(separator) = separator(sep)
        {
            (*member)
                .bind_property("visible", &separator, "visible")
                .flags(gtk::glib::BindingFlags::SYNC_CREATE)
                .build();
            slot.append(&separator);
        }
        slot.append(member);
    }
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
