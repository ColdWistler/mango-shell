//! The settings app — the seam behind the dashboard's Settings tile, now a
//! phone.
//!
//! The app is a landscape phone that rises from the bottom of the screen
//! (see [`crate::phone`]): a slab with a bezel, a speaker, a camera and a
//! status bar, with the settings themselves laid out on its screen in two
//! cards, closed with the home indicator along the bottom.
//!
//! Two things are customisable, both applied **live** (every change is pushed
//! to the shell as it happens, saved, and every bar repaints in place):
//!
//! * **Bar widgets** — the order of the widgets across the bar's `Start` /
//!   `Center` / `End` slots, which slot each sits in, and whether it shows at
//!   all. The bar builds each widget once (see [`crate::bar::WidgetId`]) and
//!   this layout decides where it lands; the file remembers one
//!   `widget=<key>:<slot>` line per widget, in bar order.
//! * **Bar shape** — an optional height override and an optional corner
//!   radius with per-corner control (top-left / top-right / bottom-left /
//!   bottom-right), applied to the bar surface *and* the fly-out panels.
//!   Both are `Option`s: an override you haven't touched keeps the active
//!   panel's own value, so turning radius on never silently pins the bar
//!   height. The generated rules are appended after the panel and theme CSS,
//!   which is what lets them win ties (see [`Shape::css`]).
//!
//! State lives in `~/.config/mango-shell/settings.conf`, one `key=value` line
//! per setting, written best-effort like every other file the shell persists.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::bar::WidgetId;
use crate::flyout;
use crate::panel::Slot;
use crate::phone;
use crate::widgets::power;

/// Radius the Corner radius slider starts at when the override is switched
/// on, in pixels.
const DEFAULT_RADIUS: u32 = 16;

/// One widget's place in the bar, as edited by this app and persisted as
/// `widget=<key>:<slot>` (plus `:hidden` when it is turned off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutEntry {
    pub id: WidgetId,
    pub slot: Slot,
    pub visible: bool,
}

/// The user's bar-shape overrides.
///
/// `None` keeps the active panel's own value for that dimension; corners only
/// mean anything alongside a radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shape {
    /// Bar content height in pixels, or the panel's own height.
    pub height: Option<u32>,
    /// Corner radius in pixels, or the panel's own rounding.
    pub radius: Option<u32>,
    /// Which corners get that radius: `[top-left, top-right, bottom-left,
    /// bottom-right]`. All-false with a radius set squares the bar off.
    pub corners: [bool; 4],
}

impl Shape {
    /// Whether there is anything for the stylesheet to override.
    pub fn is_default(&self) -> bool {
        self.height.is_none() && self.radius.is_none()
    }

    /// The override block appended **last** in the stylesheet, after the
    /// panel profile and the theme — equal specificity with the panel's own
    /// `window.mango-bar.panel-<id> …` rules, so being last is what makes it
    /// win. Both selectors also carry enough classes to beat the base
    /// `style.css` rules on their own.
    pub fn css(&self) -> String {
        let mut css = String::new();

        if !self.is_default() {
            css.push_str("window.mango-bar.mango-user-shaped .mango-bar-surface {\n");
            if let Some(height) = self.height {
                css.push_str(&format!("    min-height: {height}px;\n"));
            }
            css.push_str(&self.corners_css());
            css.push_str("}\n");
        }

        if self.radius.is_some() {
            // The fly-outs hang under the bar and share its silhouette, so
            // the same corners round on every panel card.
            css.push_str("\nwindow.mango-flyout .mango-flyout-panel {\n");
            css.push_str(&self.corners_css());
            css.push_str("}\n");

            // The settings phone shows a live swatch of the bar above its
            // reset key, so it follows the silhouette too. With no radius
            // override it keeps the default rounding from the stylesheet.
            css.push_str("\nwindow.mango-phone-window .mango-shape-preview {\n");
            css.push_str(&self.corners_css());
            css.push_str("}\n");
        }

        css
    }

    fn corners_css(&self) -> String {
        let Some(radius) = self.radius else {
            return String::new();
        };
        let [top_left, top_right, bottom_left, bottom_right] = self.corners;
        let mut css = String::new();
        for (on, property) in [
            (top_left, "border-top-left-radius"),
            (top_right, "border-top-right-radius"),
            (bottom_left, "border-bottom-left-radius"),
            (bottom_right, "border-bottom-right-radius"),
        ] {
            css.push_str(&format!(
                "    {property}: {}px;\n",
                if on { radius } else { 0 }
            ));
        }
        css
    }
}

/// The default arrangement: the bar as it looked before it was movable —
/// launcher buttons, now-playing and the visualizer on the left, workspaces
/// centred, everything else on the right.
pub fn default_layout() -> Vec<LayoutEntry> {
    const DEFAULT: &[(WidgetId, Slot)] = &[
        (WidgetId::Dashboard, Slot::Start),
        (WidgetId::Theme, Slot::Start),
        (WidgetId::Panels, Slot::Start),
        (WidgetId::Overview, Slot::Start),
        (WidgetId::Shortcuts, Slot::Start),
        (WidgetId::Music, Slot::Start),
        (WidgetId::Visualizer, Slot::Start),
        (WidgetId::Tags, Slot::Center),
        (WidgetId::Resources, Slot::End),
        (WidgetId::Clock, Slot::End),
        (WidgetId::Wifi, Slot::End),
        (WidgetId::Volume, Slot::End),
        (WidgetId::Brightness, Slot::End),
        (WidgetId::Power, Slot::End),
        (WidgetId::Tray, Slot::End),
    ];
    DEFAULT
        .iter()
        .map(|(id, slot)| LayoutEntry {
            id: *id,
            slot: *slot,
            visible: true,
        })
        .collect()
}

// --- Persistence ----------------------------------------------------------

fn state_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/mango-shell/settings.conf"))
}

/// The saved layout and shape overrides, falling back to [`default_layout`]
/// and `None` for anything missing or unparseable.
pub fn load() -> (Vec<LayoutEntry>, Option<Shape>) {
    let Some(path) = state_path() else {
        return (default_layout(), None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return (default_layout(), None);
    };
    parse(&text)
}

/// Parse `settings.conf` contents. Widgets a newer version of the shell added
/// are appended at their default place; entries this version doesn't know
/// (older keys, typos) are ignored, and a duplicate widget keeps its first
/// occurrence.
fn parse(text: &str) -> (Vec<LayoutEntry>, Option<Shape>) {
    let mut layout: Vec<LayoutEntry> = Vec::new();
    let mut shape = Shape::default();
    let mut shape_seen = false;

    for line in text.lines() {
        if let Some(value) = line.strip_prefix("widget=") {
            let mut fields = value.split(':');
            let (Some(key), Some(slot)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Some(id), Some(slot)) = (WidgetId::from_key(key), slot_from_key(slot)) else {
                continue;
            };
            if layout.iter().any(|entry| entry.id == id) {
                continue;
            }
            layout.push(LayoutEntry {
                id,
                slot,
                visible: fields.next() != Some("hidden"),
            });
        } else if let Some(value) = line.strip_prefix("height=") {
            if let Ok(height) = value.trim().parse() {
                shape.height = Some(height);
                shape_seen = true;
            }
        } else if let Some(value) = line.strip_prefix("radius=") {
            if let Ok(radius) = value.trim().parse() {
                shape.radius = Some(radius);
                shape_seen = true;
            }
        } else if let Some(value) = line.strip_prefix("corners=") {
            shape.corners = parse_corners(value);
            shape_seen = true;
        }
    }

    for entry in default_layout() {
        if !layout.iter().any(|known| known.id == entry.id) {
            layout.push(entry);
        }
    }

    // Corners without a radius do nothing, so they don't count as an edit.
    let shape = (shape_seen && !shape.is_default()).then_some(shape);
    (layout, shape)
}

/// Persist both halves in one write. The shell holds the pair and calls this
/// on either kind of change, so neither writer can clobber the other.
pub fn save(layout: &[LayoutEntry], shape: Option<Shape>) {
    let Some(path) = state_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };

    let mut text = String::from("# mango-shell settings, written by the settings app\n");
    for entry in layout {
        text.push_str(&format!(
            "widget={}:{}{}\n",
            entry.id.key(),
            slot_key(entry.slot),
            if entry.visible { "" } else { ":hidden" },
        ));
    }
    if let Some(shape) = shape {
        if let Some(height) = shape.height {
            text.push_str(&format!("height={height}\n"));
        }
        if let Some(radius) = shape.radius {
            text.push_str(&format!("radius={radius}\n"));
            const CORNERS: [&str; 4] = ["top-left", "top-right", "bottom-left", "bottom-right"];
            let rounded: Vec<&str> = shape
                .corners
                .iter()
                .enumerate()
                .filter(|(_, on)| **on)
                .map(|(index, _)| CORNERS[index])
                .collect();
            if !rounded.is_empty() {
                text.push_str(&format!("corners={}\n", rounded.join(",")));
            }
        }
    }

    let _ = std::fs::create_dir_all(dir);
    if let Err(error) = std::fs::write(&path, text) {
        eprintln!("mango-shell: could not save settings: {error}");
    }
}

fn slot_key(slot: Slot) -> &'static str {
    match slot {
        Slot::Start => "start",
        Slot::Center => "center",
        Slot::End => "end",
    }
}

fn slot_from_key(key: &str) -> Option<Slot> {
    match key {
        "start" => Some(Slot::Start),
        "center" => Some(Slot::Center),
        "end" => Some(Slot::End),
        _ => None,
    }
}

fn parse_corners(value: &str) -> [bool; 4] {
    let mut corners = [false; 4];
    for name in value.split(',') {
        match name.trim() {
            "top-left" => corners[0] = true,
            "top-right" => corners[1] = true,
            "bottom-left" => corners[2] = true,
            "bottom-right" => corners[3] = true,
            _ => {}
        }
    }
    corners
}

// --- The settings app -----------------------------------------------------

/// What the shell hands the app at launch.
pub struct SettingsInit {
    /// Wallpaper for the panel header banner.
    pub wallpaper: Option<PathBuf>,
    pub layout: Vec<LayoutEntry>,
    pub shape: Option<Shape>,
    /// The active panel's height: what the Height slider shows until the
    /// override is switched on, and what it restores when switched off.
    pub default_height: u32,
}

pub struct SettingsModel {
    /// The phone body and the bottom-anchored surface it rises on.
    phone: phone::Phone,
    /// The wallpaper chip in the phone's header.
    thumb: flyout::Banner,
    /// The working copy; every edit is pushed up immediately.
    layout: Vec<LayoutEntry>,
    shape: Shape,
    default_height: u32,
    /// Shared with the widget signal handlers: while a state sync is in
    /// flight, programmatic `set_active` / `set_value` calls must not echo
    /// back out as edits.
    syncing: Rc<Cell<bool>>,
    /// The widget rows, rebuilt on every layout edit.
    list: gtk::Box,
    /// Shape widgets, kept for syncing.
    height_switch: gtk::ToggleButton,
    height_scale: gtk::Scale,
    radius_switch: gtk::ToggleButton,
    radius_scale: gtk::Scale,
    corners: [gtk::ToggleButton; 4],
    /// The live bar swatch at the bottom of the shape card.
    preview: gtk::Box,
}

#[derive(Debug)]
pub enum SettingsInput {
    /// Flip visibility (sent by the shell from the dashboard's tile).
    Toggle,
    /// Hide without editing (Escape, the header's close key, or the home
    /// indicator).
    Hide,
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
    /// Move widget `index` up or down the list.
    Move { index: usize, delta: isize },
    /// Cycle a widget between Start → Center → End.
    CycleSlot(usize),
    /// Show or hide a widget.
    SetVisible { index: usize, visible: bool },
    /// The Height override was switched on/off, or the slider moved.
    ToggleHeight(bool),
    SetHeight(u32),
    /// The Corner radius override was switched on/off, or the slider moved.
    ToggleRadius(bool),
    SetRadius(u32),
    /// A corner toggle was pressed.
    ToggleCorner(usize),
    /// Restore the default layout and drop the shape overrides.
    Reset,
    /// The active panel changed: its height becomes the Height slider's
    /// "off" position, so resync that (and nothing else).
    PanelHeight(u32),
    /// The shell reset everything; resync the working copy.
    SetState {
        layout: Vec<LayoutEntry>,
        shape: Option<Shape>,
    },
}

/// Messages the app sends up to the shell, which persists and applies them.
#[derive(Debug)]
pub enum SettingsOutput {
    LayoutChanged(Vec<LayoutEntry>),
    ShapeChanged(Shape),
    Reset,
}

#[relm4::component(pub)]
impl SimpleComponent for SettingsModel {
    type Init = SettingsInit;
    type Input = SettingsInput;
    type Output = SettingsOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-phone-window"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.phone.widget() {},
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The phone surface: bottom-centred, exclusive keyboard while open,
        // hidden until toggled.
        let phone = phone::Phone::new(&root);

        let thumb = flyout::make_thumb(46, 13.0);
        flyout::set_banner(&thumb, init.wallpaper.as_deref());

        let syncing = Rc::new(Cell::new(false));

        // --- Status bar (the phone's own chrome) ----------------------------
        let clock = gtk::Label::new(None);
        clock.add_css_class("mango-phone-clock");
        let date = gtk::Label::new(None);
        date.add_css_class("mango-phone-date");
        date.set_hexpand(true);
        date.set_xalign(0.0);

        let battery_icon = gtk::Image::new();
        battery_icon.set_pixel_size(15);
        let battery_text = gtk::Label::new(None);
        battery_text.add_css_class("mango-phone-battery");
        let battery = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        battery.add_css_class("mango-phone-battery");
        battery.set_valign(gtk::Align::Center);
        battery.append(&battery_icon);
        battery.append(&battery_text);

        let status = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        status.add_css_class("mango-phone-status");
        status.append(&clock);
        status.append(&date);
        status.append(&battery);
        phone.screen().append(&status);

        // Keep the status bar's clock and battery current. Cheap (two labels
        // and a couple of sysfs reads), so it ticks whether the phone is up
        // or not — it is always right the moment the phone rises.
        let update_status = {
            let clock = clock.clone();
            let date = date.clone();
            let battery_icon = battery_icon.clone();
            let battery_text = battery_text.clone();
            move || {
                let now = glib::DateTime::now_local().ok();
                clock.set_text(
                    &now.as_ref()
                        .and_then(|now| now.format("%H:%M").ok())
                        .map(|text| text.to_string())
                        .unwrap_or_default(),
                );
                date.set_text(
                    &now.as_ref()
                        .and_then(|now| now.format("%a, %d %b").ok())
                        .map(|text| text.to_string())
                        .unwrap_or_default(),
                );
                let state = power::read_battery();
                if state.present {
                    battery_icon.set_visible(true);
                    battery_text.set_visible(true);
                    battery_icon.set_icon_name(Some(state.icon_name()));
                    battery_text.set_text(&format!("{}%", state.percent));
                } else {
                    battery_icon.set_visible(false);
                    battery_text.set_visible(false);
                }
            }
        };
        update_status();
        glib::timeout_add_seconds_local(30, move || {
            update_status();
            glib::ControlFlow::Continue
        });

        // --- Header ---------------------------------------------------------
        let title = gtk::Label::new(Some("Settings"));
        title.add_css_class("mango-phone-title");
        title.set_xalign(0.0);
        let subtitle = gtk::Label::new(Some("Mango Shell"));
        subtitle.add_css_class("mango-phone-subtitle");
        subtitle.set_xalign(0.0);
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        titles.set_hexpand(true);
        titles.set_valign(gtk::Align::Center);
        titles.append(&title);
        titles.append(&subtitle);

        let close = gtk::Button::new();
        close.add_css_class("mango-phone-close");
        close.set_tooltip_text(Some("Close settings"));
        close.set_valign(gtk::Align::Center);
        let close_image = gtk::Image::from_icon_name("window-close-symbolic");
        close_image.set_pixel_size(15);
        close.set_child(Some(&close_image));
        let close_sender = sender.clone();
        close.connect_clicked(move |_| {
            close_sender.input(SettingsInput::Hide);
        });

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        head.add_css_class("mango-phone-head");
        head.append(&thumb.widget);
        head.append(&titles);
        head.append(&close);
        phone.screen().append(&head);

        // --- The two cards --------------------------------------------------
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        content.add_css_class("mango-phone-content");
        content.set_vexpand(true);
        content.set_homogeneous(true);

        let widgets_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        widgets_card.add_css_class("mango-phone-card");
        let shape_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        shape_card.add_css_class("mango-phone-card");
        content.append(&widgets_card);
        content.append(&shape_card);
        phone.screen().append(&content);

        // --- Bar widgets ----------------------------------------------------
        let widgets_heading = gtk::Label::new(Some("Bar widgets"));
        widgets_heading.add_css_class("mango-phone-section");
        widgets_heading.set_xalign(0.0);

        let widgets_hint = gtk::Label::new(Some(
            "Arrows reorder, the chip moves a widget between the left, centre \
             and right of the bar, and the eye hides it.",
        ));
        widgets_hint.add_css_class("mango-phone-note");
        widgets_hint.set_xalign(0.0);
        widgets_hint.set_wrap(true);
        widgets_hint.set_max_width_chars(46);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("mango-widget-list");

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_child(Some(&list));
        scrolled.set_min_content_height(120);
        scrolled.set_propagate_natural_height(false);
        scrolled.set_vexpand(true);

        widgets_card.append(&widgets_heading);
        widgets_card.append(&widgets_hint);
        widgets_card.append(&scrolled);

        // --- Bar shape ------------------------------------------------------
        let shape_heading = gtk::Label::new(Some("Bar shape"));
        shape_heading.add_css_class("mango-phone-section");
        shape_heading.set_xalign(0.0);

        let height_switch = override_switch("Override the panel's bar height");
        let height_scale = slider(20.0, 64.0);
        let height_row = row("Bar height", &height_scale, &height_switch);

        let radius_switch = override_switch("Override the panel's corner radius");
        let radius_scale = slider(0.0, 40.0);
        let radius_row = row("Corner radius", &radius_scale, &radius_switch);

        // The four corners, glyphed the way they sit on screen.
        const CORNER_GLYPHS: [&str; 4] = ["\u{2196}", "\u{2197}", "\u{2199}", "\u{2198}"];
        const CORNER_NAMES: [&str; 4] = [
            "Round the top-left corner",
            "Round the top-right corner",
            "Round the bottom-left corner",
            "Round the bottom-right corner",
        ];
        let corners_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        corners_row.add_css_class("mango-shape-row");
        let corners_label = gtk::Label::new(Some("Rounded corners"));
        corners_label.set_xalign(0.0);
        corners_label.set_hexpand(true);
        corners_row.append(&corners_label);
        let corners: [gtk::ToggleButton; 4] = [
            gtk::ToggleButton::new(),
            gtk::ToggleButton::new(),
            gtk::ToggleButton::new(),
            gtk::ToggleButton::new(),
        ];
        for (index, button) in corners.iter().enumerate() {
            button.add_css_class("mango-corner-button");
            button.set_tooltip_text(Some(CORNER_NAMES[index]));
            button.set_child(Some(&gtk::Label::new(Some(CORNER_GLYPHS[index]))));
            corners_row.append(button);
        }

        let shape_hint = gtk::Label::new(Some(
            "Drag a slider to take over that value; its switch hands it back \
             to the panel's own setting.",
        ));
        shape_hint.add_css_class("mango-phone-note");
        shape_hint.set_xalign(0.0);
        shape_hint.set_wrap(true);
        shape_hint.set_max_width_chars(46);

        // A live swatch of the bar, so the shape controls show their work.
        // The height is set in `sync_shape`; the corners come in with the
        // stylesheet (see `Shape::css`).
        let preview = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        preview.add_css_class("mango-shape-preview");
        preview.set_halign(gtk::Align::Fill);
        let preview_note = gtk::Label::new(Some("Live preview"));
        preview_note.add_css_class("mango-phone-note");
        preview_note.set_halign(gtk::Align::Center);

        // --- Reset ----------------------------------------------------------
        let reset = gtk::Button::new();
        reset.add_css_class("mango-settings-reset");
        reset.set_halign(gtk::Align::Center);
        reset.set_tooltip_text(Some(
            "Restore the default widget layout and the panel's own bar shape",
        ));
        reset.set_child(Some(&gtk::Label::new(Some("Reset to defaults"))));
        let reset_sender = sender.clone();
        reset.connect_clicked(move |_| {
            reset_sender.input(SettingsInput::Reset);
        });

        // A spacer so the preview and the reset sit on the card's bottom
        // edge, and the shape card fills the screen's height like the widget
        // list does.
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        spacer.set_valign(gtk::Align::Fill);

        shape_card.append(&shape_heading);
        shape_card.append(&height_row);
        shape_card.append(&radius_row);
        shape_card.append(&corners_row);
        shape_card.append(&shape_hint);
        shape_card.append(&spacer);
        shape_card.append(&preview);
        shape_card.append(&preview_note);
        shape_card.append(&reset);

        // --- Home indicator -------------------------------------------------
        // The phone's "swipe up": a pill on the screen's bottom edge. Tapping
        // it (or Escape, or the header's close key) puts the phone away.
        let home = gtk::Button::new();
        home.add_css_class("mango-phone-home");
        home.set_tooltip_text(Some("Close settings"));
        home.set_halign(gtk::Align::Center);
        let home_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        home_bar.add_css_class("mango-phone-home-bar");
        home_bar.set_halign(gtk::Align::Center);
        home_bar.set_valign(gtk::Align::Center);
        home.set_child(Some(&home_bar));
        let home_sender = sender.clone();
        home.connect_clicked(move |_| {
            home_sender.input(SettingsInput::Hide);
        });
        phone.screen().append(&home);

        // --- Wire the shape controls ---------------------------------------
        {
            let sender = sender.clone();
            let syncing = syncing.clone();
            height_switch.connect_toggled(move |button| {
                if !syncing.get() {
                    sender.input(SettingsInput::ToggleHeight(button.is_active()));
                }
            });
        }
        {
            let sender = sender.clone();
            let syncing = syncing.clone();
            height_scale.connect_value_changed(move |scale| {
                if !syncing.get() {
                    sender.input(SettingsInput::SetHeight(scale.value() as u32));
                }
            });
        }
        {
            let sender = sender.clone();
            let syncing = syncing.clone();
            radius_switch.connect_toggled(move |button| {
                if !syncing.get() {
                    sender.input(SettingsInput::ToggleRadius(button.is_active()));
                }
            });
        }
        {
            let sender = sender.clone();
            let syncing = syncing.clone();
            radius_scale.connect_value_changed(move |scale| {
                if !syncing.get() {
                    sender.input(SettingsInput::SetRadius(scale.value() as u32));
                }
            });
        }
        for (index, button) in corners.iter().enumerate() {
            let sender = sender.clone();
            let syncing = syncing.clone();
            button.connect_toggled(move |_| {
                if !syncing.get() {
                    sender.input(SettingsInput::ToggleCorner(index));
                }
            });
        }

        let mut model = SettingsModel {
            phone,
            thumb,
            layout: init.layout,
            shape: init.shape.unwrap_or_default(),
            default_height: init.default_height,
            syncing,
            list,
            height_switch,
            height_scale,
            radius_switch,
            radius_scale,
            corners,
            preview,
        };

        let widgets = view_output!();

        // Capture phase, so Escape is seen before the buttons.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let key_sender = sender.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                key_sender.input(SettingsInput::Hide);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        model.render_rows(&sender);
        model.sync_shape();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            SettingsInput::Toggle => {
                if self.phone.is_open() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            SettingsInput::Hide => self.hide(),
            SettingsInput::Banner(path) => flyout::set_banner(&self.thumb, path.as_deref()),

            SettingsInput::Move { index, delta } => {
                self.move_entry(index, delta);
                self.layout_changed(&sender);
            }
            SettingsInput::CycleSlot(index) => {
                if let Some(entry) = self.layout.get_mut(index) {
                    entry.slot = match entry.slot {
                        Slot::Start => Slot::Center,
                        Slot::Center => Slot::End,
                        Slot::End => Slot::Start,
                    };
                }
                self.layout_changed(&sender);
            }
            SettingsInput::SetVisible { index, visible } => {
                if let Some(entry) = self.layout.get_mut(index) {
                    entry.visible = visible;
                }
                self.layout_changed(&sender);
            }

            SettingsInput::ToggleHeight(active) => {
                if self.syncing.get() {
                    return;
                }
                // On: take the slider's position. Off: hand the height back
                // to the active panel (the slider keeps showing what the
                // panel uses, ready to be dragged again).
                self.shape.height = active.then(|| self.height_scale.value() as u32);
                self.emit_shape(&sender);
            }
            SettingsInput::SetHeight(value) => {
                if self.syncing.get() {
                    return;
                }
                // Dragging takes over: the override turns itself on, so the
                // slider can never be a dead control.
                self.shape.height = Some(value);
                set_toggle(&self.syncing, &self.height_switch, true);
                self.emit_shape(&sender);
            }
            SettingsInput::ToggleRadius(active) => {
                if self.syncing.get() {
                    return;
                }
                if active {
                    self.activate_radius();
                } else {
                    self.shape.radius = None;
                }
                self.emit_shape(&sender);
            }
            SettingsInput::SetRadius(value) => {
                if self.syncing.get() {
                    return;
                }
                // Same self-activation as the height slider: grabbing the
                // handle turns the override on at that value.
                self.activate_radius();
                self.shape.radius = Some(value);
                set_toggle(&self.syncing, &self.radius_switch, true);
                self.emit_shape(&sender);
            }
            SettingsInput::ToggleCorner(index) => {
                if self.syncing.get() {
                    return;
                }
                // Clicking a corner with no override on takes it over too
                // (and rounds some corner first, so something is visible).
                self.activate_radius();
                self.shape.corners[index] = !self.shape.corners[index];
                self.emit_shape(&sender);
            }

            SettingsInput::Reset => {
                let _ = sender.output(SettingsOutput::Reset);
            }
            SettingsInput::PanelHeight(height) => {
                self.default_height = height;
                if self.shape.height.is_none() {
                    self.sync_shape();
                }
            }
            SettingsInput::SetState { layout, shape } => {
                self.layout = layout;
                self.shape = shape.unwrap_or_default();
                self.render_rows(&sender);
                self.sync_shape();
            }
        }
    }
}

impl SettingsModel {
    fn show(&mut self) {
        self.sync_shape();
        self.phone.show();
    }

    fn hide(&mut self) {
        self.phone.hide();
    }

    /// Swap `index` one place towards `delta`, clamped at both ends.
    fn move_entry(&mut self, index: usize, delta: isize) {
        if self.layout.is_empty() {
            return;
        }
        let count = self.layout.len() as isize;
        let target = (index as isize + delta).clamp(0, count - 1) as usize;
        if target == index {
            return;
        }
        let entry = self.layout.remove(index);
        self.layout.insert(target, entry);
    }

    /// Rebuild the widget rows (order or state changed) and push the layout.
    fn layout_changed(&mut self, sender: &ComponentSender<Self>) {
        self.render_rows(sender);
        let _ = sender.output(SettingsOutput::LayoutChanged(self.layout.clone()));
    }

    fn emit_shape(&mut self, sender: &ComponentSender<Self>) {
        // The corners/radius travel in the stylesheet; the swatch's height
        // is sized here.
        self.preview
            .set_size_request(-1, self.shape.height.unwrap_or(self.default_height) as i32);
        let _ = sender.output(SettingsOutput::ShapeChanged(self.shape));
    }

    /// Recreate every row from the current layout. Rows are rebuilt rather
    /// than moved so order, slot chips, eye states and the up/down bounds are
    /// always consistent — fifteen widgets, so this costs nothing.
    fn render_rows(&mut self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let count = self.layout.len();
        for (index, entry) in self.layout.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("mango-widget-row");

            let name = gtk::Label::new(Some(entry.id.label()));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            row.append(&name);

            // The slot chip: click to cycle Start → Center → End.
            let slot = gtk::Button::new();
            slot.add_css_class("mango-slot-chip");
            slot.set_tooltip_text(Some(
                "Move to the next slot (left \u{2192} centre \u{2192} right)",
            ));
            slot.set_child(Some(&gtk::Label::new(Some(slot_text(entry.slot)))));
            let slot_sender = sender.clone();
            slot.connect_clicked(move |_| {
                slot_sender.input(SettingsInput::CycleSlot(index));
            });
            row.append(&slot);

            // The eye: whether this widget is on the bar at all.
            let visible = gtk::ToggleButton::new();
            visible.add_css_class("mango-visible-toggle");
            visible.set_active(entry.visible);
            visible.set_tooltip_text(Some(if entry.visible {
                "Hide this widget"
            } else {
                "Show this widget"
            }));
            let image = gtk::Image::from_icon_name(if entry.visible {
                "view-conceal-symbolic"
            } else {
                "view-reveal-symbolic"
            });
            image.set_pixel_size(16);
            visible.set_child(Some(&image));
            let visible_sender = sender.clone();
            visible.connect_toggled(move |button| {
                visible_sender.input(SettingsInput::SetVisible {
                    index,
                    visible: button.is_active(),
                });
            });
            row.append(&visible);

            let up = arrow_button("go-up-symbolic", "Move up");
            up.set_sensitive(index > 0);
            let up_sender = sender.clone();
            up.connect_clicked(move |_| {
                up_sender.input(SettingsInput::Move {
                    index,
                    delta: -1,
                });
            });
            row.append(&up);

            let down = arrow_button("go-down-symbolic", "Move down");
            down.set_sensitive(index + 1 < count);
            let down_sender = sender.clone();
            down.connect_clicked(move |_| {
                down_sender.input(SettingsInput::Move { index, delta: 1 });
            });
            row.append(&down);

            self.list.append(&row);
        }
    }

    /// Push `shape` into the shape widgets, blocking their handlers so a
    /// state sync never echoes back out as an edit.
    fn sync_shape(&mut self) {
        self.syncing.set(true);

        let height = self.shape.height;
        self.height_switch.set_active(height.is_some());
        self.height_scale
            .set_value(height.unwrap_or(self.default_height) as f64);
        self.preview
            .set_size_request(-1, height.unwrap_or(self.default_height) as i32);

        let radius = self.shape.radius;
        self.radius_switch.set_active(radius.is_some());
        self.radius_scale
            .set_value(radius.unwrap_or(DEFAULT_RADIUS) as f64);
        self.sync_corners();

        self.syncing.set(false);
    }

    /// The corner toggles mirror the saved set. They are always live: with
    /// no radius override on, clicking one just takes the override over.
    fn sync_corners(&mut self) {
        for (index, button) in self.corners.iter().enumerate() {
            set_toggle(&self.syncing, button, self.shape.corners[index]);
        }
    }

    /// Turn the radius override on at the slider's current value if it is
    /// off. A radius no corner uses is invisible, so the two bottom corners
    /// come along — unless the user already picked corners (including none
    /// at all, which squares the bar off on purpose).
    fn activate_radius(&mut self) {
        if self.shape.radius.is_some() {
            return;
        }
        self.shape.radius = Some(self.radius_scale.value() as u32);
        if !self.shape.corners.iter().any(|on| *on) {
            self.shape.corners = [false, false, true, true];
            self.sync_corners();
        }
    }
}

/// Flip a toggle from code without its handler echoing the edit back as a
/// new one. Nests inside an existing sync: the flag is only cleared by the
/// sync level that raised it.
fn set_toggle(syncing: &Rc<Cell<bool>>, button: &gtk::ToggleButton, active: bool) {
    if button.is_active() == active {
        return;
    }
    let was_syncing = syncing.replace(true);
    button.set_active(active);
    if !was_syncing {
        syncing.set(false);
    }
}

/// One label in a shape row: name, slider, override switch.
fn row(name: &str, slider: &gtk::Scale, switch: &gtk::ToggleButton) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("mango-shape-row");

    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    row.append(&label);

    slider.set_hexpand(true);
    row.append(slider);

    row.append(switch);
    row
}

/// The "override the panel's own value" switch, sharing one style everywhere.
fn override_switch(tooltip: &str) -> gtk::ToggleButton {
    let switch = gtk::ToggleButton::new();
    switch.add_css_class("mango-override-switch");
    switch.set_tooltip_text(Some(tooltip));
    switch.set_child(Some(&gtk::Label::new(Some("Use"))));
    switch
}

/// A slider with its value drawn to the right, in whole pixels.
fn slider(min: f64, max: f64) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, 1.0);
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.add_css_class("mango-shape-slider");
    scale
}

/// A small ↑ / ↓ row-order button.
fn arrow_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-move-button");
    button.set_tooltip_text(Some(tooltip));
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(14);
    button.set_child(Some(&image));
    button
}

/// The slot chip's text.
fn slot_text(slot: Slot) -> &'static str {
    match slot {
        Slot::Start => "Left",
        Slot::Center => "Centre",
        Slot::End => "Right",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_covers_every_widget_once() {
        let layout = default_layout();
        assert_eq!(layout.len(), WidgetId::ALL.len());
        for id in WidgetId::ALL {
            assert_eq!(
                layout.iter().filter(|entry| entry.id == id).count(),
                1,
                "{id:?} missing or duplicated"
            );
        }
        assert!(layout.iter().all(|entry| entry.visible));
        // The stock split: workspaces centred, everything else left/right.
        let tags = layout
            .iter()
            .find(|entry| entry.id == WidgetId::Tags)
            .expect("tags in the default layout");
        assert_eq!(tags.slot, Slot::Center);
        assert_eq!(
            layout.iter().filter(|entry| entry.slot == Slot::Center).count(),
            1
        );
    }

    #[test]
    fn parses_saved_settings_and_fills_in_missing_widgets() {
        let text = "\
widget=clock:start
widget=tags:center:hidden
widget=bogus:start
widget=clock:end
height=44
radius=18
corners=bottom-left,bottom-right
";
        let (layout, shape) = parse(text);

        // First entry wins, unknown keys are dropped, every widget present.
        assert_eq!(
            layout[0],
            LayoutEntry {
                id: WidgetId::Clock,
                slot: Slot::Start,
                visible: true,
            }
        );
        assert_eq!(layout.len(), WidgetId::ALL.len());
        assert!(layout
            .iter()
            .any(|entry| entry.id == WidgetId::Tags && !entry.visible));
        // Missing widgets (dashboard, music, …) fall back to their default.
        let dashboard = layout
            .iter()
            .find(|entry| entry.id == WidgetId::Dashboard)
            .expect("dashboard appended");
        assert_eq!(dashboard.slot, Slot::Start);
        assert!(dashboard.visible);

        let shape = shape.expect("shape present");
        assert_eq!(shape.height, Some(44));
        assert_eq!(shape.radius, Some(18));
        assert_eq!(shape.corners, [false, false, true, true]);
    }

    #[test]
    fn nothing_saved_means_defaults() {
        let (layout, shape) = parse("# just a comment\n");
        assert_eq!(layout, default_layout());
        assert!(shape.is_none());
        // Corners without a radius do nothing, so they are not an override.
        let (_, shape) = parse("corners=top-left,bottom-right\n");
        assert!(shape.is_none());
    }

    #[test]
    fn shape_css_overrides_the_bar_and_the_chrome() {
        // With no overrides, the stylesheet gains nothing (panels rule).
        assert!(Shape::default().css().is_empty());

        let shape = Shape {
            height: Some(44),
            radius: Some(18),
            corners: [false, false, true, true],
        };
        let css = shape.css();
        assert!(css.contains("window.mango-bar.mango-user-shaped .mango-bar-surface {"));
        assert!(css.contains("min-height: 44px;"));
        // Rounded corners get the radius, the rest are squared off.
        assert!(css.contains("border-bottom-left-radius: 18px;"));
        assert!(css.contains("border-bottom-right-radius: 18px;"));
        assert!(css.contains("border-top-left-radius: 0px;"));
        assert!(css.contains("border-top-right-radius: 0px;"));
        // The fly-out cards share the silhouette.
        assert!(css.contains("window.mango-flyout .mango-flyout-panel {"));
        // So does the phone's live bar preview.
        assert!(css.contains("window.mango-phone-window .mango-shape-preview {"));

        // A radius with no height still styles both, but not the height.
        let shape = Shape {
            radius: Some(12),
            ..Shape::default()
        };
        let css = shape.css();
        assert!(!css.contains("min-height"));
        assert!(css.contains("border-top-left-radius: 0px;"));
    }
}
