//! Panel (bar layout) configurations, ported from
//! [adi1090x/polybar-themes](https://github.com/adi1090x/polybar-themes).
//!
//! A **colour theme** (`src/theme.rs`) only changes the palette; a **panel**
//! changes the *shape* of the bar and where each cluster of widgets sits. The
//! two compose: the panel's default palette is applied first, then the active
//! colour theme overrides the tokens on top (and a user stylesheet wins over
//! both — see [`crate::load_css`]).
//!
//! Each [`Panel`] carries:
//!
//! * a block of CSS (`src/panels/<id>.css`) holding both its default
//!   `@define-color` palette and its structural rules, all scoped to the
//!   `panel-<id>` class the bar puts on its root window;
//! * the bar's height and layer-shell margins (a floating bar insets itself
//!   from the screen edges; a full-width one does not);
//! * the [`Slot`] each cluster of widgets is placed in, mirroring polybar's
//!   `modules-left` / `modules-center` / `modules-right`;
//! * an optional [`Sep`] separator drawn *between* clusters.
//!
//! [`PanelPickerModel`] is the fly-out gallery, opened from the bar's panel
//! button, that lets you flip between them live. The choice is written to
//! `~/.config/mango-shell/panel` and restored on the next start.

use std::path::PathBuf;

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;

/// One of the three CentreBox slots a cluster of widgets can live in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Start,
    Center,
    End,
}

/// A separator drawn between clusters inside a slot.
#[derive(Clone, Copy, Debug)]
pub enum Sep {
    /// No separators (`material`, `docky`).
    None,
    /// A thin vertical hairline (`shades`, `hack`).
    Line,
    /// A small coloured block (`blocks`, `cuts`).
    Block,
    /// A glyph, e.g. a powerline arrow (`shapes`).
    #[allow(dead_code)]
    Glyph(&'static str),
}

/// A ported bar layout.
#[derive(Clone, Copy)]
pub struct Panel {
    /// Stable identifier, persisted in `~/.config/mango-shell/panel`.
    pub id: &'static str,
    /// Human-readable name shown in the picker.
    pub name: &'static str,
    /// One-line description shown under the name.
    pub blurb: &'static str,
    /// Palette + structural `@define-color`/rules appended after `style.css`.
    pub css: &'static str,
    /// Bar content height in pixels.
    pub height: i32,
    /// Layer-shell top margin. Also shifts the fly-outs down so they keep
    /// hanging just below the bar (see [`Panel::flyout_gap`]).
    pub margin_top: i32,
    /// Layer-shell left/right margin. `0` keeps the bar full width; anything
    /// larger floats it in from the screen edges.
    pub margin_side: i32,
    /// Which slot the launcher/wallpaper/theme/panel buttons live in.
    pub apps: Slot,
    /// Which slot the workspace (tag) pills live in.
    pub tags: Slot,
    /// Which slot the clock lives in.
    pub clock: Slot,
    /// Which slot the system tray lives in.
    pub tray: Slot,
    /// Which slot the Wi-Fi/volume/brightness readouts live in.
    pub system: Slot,
    /// Separator style drawn between clusters.
    pub sep: Sep,
    /// Corner radius used when drawing the picker preview.
    pub preview_radius: f64,
    /// Whether the picker preview draws the bar inset from the edges.
    pub preview_floating: bool,
    /// Preview chips, as packed `0xRRGGBB`: `[bar, chip1, chip2, chip3]`.
    pub preview: [u32; 4],
}

impl Panel {
    /// Distance from the top of the monitor to where a fly-out should start,
    /// so it hangs just below the (possibly inset and short) bar.
    pub fn flyout_gap(&self) -> i32 {
        self.margin_top + self.height + 6
    }
}

/// Every ported panel, in picker order. `PANELS[0]` is the fallback.
pub const PANELS: &[Panel] = &[
    Panel {
        id: "hug",
        name: "Mango (hug)",
        blurb: "The default bar: rounded bottom, pill clusters",
        css: include_str!("panels/hug.css"),
        height: 40,
        margin_top: 0,
        margin_side: 0,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::Center,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::None,
        preview_radius: 0.0,
        preview_floating: false,
        preview: [0x201b14, 0xc9b890, 0x4a4436, 0x322c23],
    },
    Panel {
        id: "material",
        name: "Material",
        blurb: "Floating rounded bar, amber accent",
        css: include_str!("panels/material.css"),
        height: 40,
        margin_top: 8,
        margin_side: 16,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::End,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::None,
        preview_radius: 0.0,
        preview_floating: true,
        preview: [0x1f1f1f, 0xffb300, 0xe53935, 0x7cb342],
    },
    Panel {
        id: "shades",
        name: "Shades",
        blurb: "Full-width flat bar, blue shade, hairlines",
        css: include_str!("panels/shades.css"),
        height: 36,
        margin_top: 0,
        margin_side: 0,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::End,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::Line,
        preview_radius: 0.0,
        preview_floating: false,
        preview: [0x1f1f1f, 0x64b5f6, 0x1565c0, 0x0d47a1],
    },
    Panel {
        id: "cuts",
        name: "Cuts",
        blurb: "Thin sharp bar, yellow accent, block separators",
        css: include_str!("panels/cuts.css"),
        height: 30,
        margin_top: 0,
        margin_side: 0,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::End,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::Block,
        preview_radius: 0.0,
        preview_floating: false,
        preview: [0x0a0a0a, 0xfdd835, 0xff5250, 0x43a047],
    },
    Panel {
        id: "blocks",
        name: "Blocks",
        blurb: "Flat bar of light blocks, monitor name centered",
        css: include_str!("panels/blocks.css"),
        height: 34,
        margin_top: 0,
        margin_side: 0,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::End,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::Block,
        preview_radius: 0.0,
        preview_floating: false,
        preview: [0x2f343f, 0xc4c7c5, 0xb4bc67, 0x75a4cd],
    },
    Panel {
        id: "docky",
        name: "Docky",
        blurb: "Floating pill dock with a red underline",
        css: include_str!("panels/docky.css"),
        height: 40,
        margin_top: 6,
        margin_side: 24,
        apps: Slot::Start,
        tags: Slot::Start,
        clock: Slot::Center,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::None,
        preview_radius: 0.0,
        preview_floating: true,
        preview: [0x1f1f1f, 0xe53935, 0x7cb342, 0xffffff],
    },
    Panel {
        id: "hack",
        name: "Hack",
        blurb: "Thin terminal-style bar, cyan underline, tags centered",
        css: include_str!("panels/hack.css"),
        height: 26,
        margin_top: 0,
        margin_side: 0,
        apps: Slot::Start,
        tags: Slot::Center,
        clock: Slot::End,
        tray: Slot::End,
        system: Slot::End,
        sep: Sep::Line,
        preview_radius: 0.0,
        preview_floating: false,
        preview: [0x263238, 0x00bcd4, 0xdfdfdf, 0x90a4ae],
    },
];

/// The panel used when nothing has been chosen yet.
pub const DEFAULT_ID: &str = "hug";

/// Look up a panel by id, falling back to the default.
pub fn get(id: &str) -> &'static Panel {
    PANELS
        .iter()
        .find(|panel| panel.id == id)
        .unwrap_or(&PANELS[0])
}

/// Whether `id` names one of the ported panels.
pub fn is_known(id: &str) -> bool {
    PANELS.iter().any(|panel| panel.id == id)
}

// --- Persistence ----------------------------------------------------------

fn state_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/mango-shell/panel"))
}

/// The panel remembered from a previous run, or the default.
pub fn load_current() -> &'static Panel {
    let Some(state) = state_path() else {
        return get(DEFAULT_ID);
    };
    match std::fs::read_to_string(state) {
        Ok(id) => get(id.trim()),
        Err(_) => get(DEFAULT_ID),
    }
}

/// Remember the chosen panel id (best effort).
pub fn save_current(id: &str) {
    let Some(state) = state_path() else {
        return;
    };
    if let Some(parent) = state.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("mango-shell: could not create config dir: {error}");
        return;
    }
    if let Err(error) = std::fs::write(&state, id.as_bytes()) {
        eprintln!("mango-shell: could not save panel state: {error}");
    }
}

// --- Palette previews -----------------------------------------------------

/// A widget that paints a small mock of `panel`: a bar with a couple of
/// clusters, coloured by the panel's palette and rounded by its radius.
pub fn preview_widget(panel: &'static Panel, width: i32, height: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(width);
    area.set_content_height(height);
    let draw = *panel;
    area.set_draw_func(move |_, cr, w, h| draw_preview(cr, w, h, &draw));
    area
}

fn draw_preview(cr: &cairo::Context, width: i32, height: i32, panel: &Panel) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let inset = if panel.preview_floating {
        w * 0.05
    } else {
        0.0
    };
    let bar_w = w - inset * 2.0;
    let bar_h = h * 0.44;
    let bar_y = (h - bar_h) / 2.0;

    // The bar background.
    rounded_rect(cr, inset, bar_y, bar_w, bar_h, panel.preview_radius);
    let (r, g, b) = unpack(panel.preview[0]);
    cr.set_source_rgba(r, g, b, 0.9);
    let _ = cr.fill();

    // Clusters: two chips on the left, two on the right.
    let chip_h = (bar_h * 0.5).max(3.0);
    let chip_y = bar_y + (bar_h - chip_h) / 2.0;
    let chip_w = (bar_w * 0.11).max(6.0);
    let radius = 0.0;
    let left_x = inset + bar_w * 0.05;
    let right_x = inset + bar_w * 0.95 - chip_w;

    for (index, color) in panel.preview[1..].iter().enumerate() {
        let x = if index < 2 {
            left_x + index as f64 * (chip_w + 4.0)
        } else {
            right_x - (index as f64 - 2.0) * (chip_w + 4.0)
        };
        rounded_rect(cr, x, chip_y, chip_w, chip_h, radius);
        let (r, g, b) = unpack(*color);
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();
    }

    // The accent underline some panels have.
    if panel.id == "cuts" || panel.id == "hack" || panel.id == "docky" {
        cr.rectangle(inset, bar_y + bar_h - 1.5, bar_w, 1.5);
        let (r, g, b) = unpack(panel.preview[1]);
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();
    }
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let radius = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - radius, y + radius, radius, -pi / 2.0, 0.0);
    cr.arc(x + w - radius, y + h - radius, radius, 0.0, pi / 2.0);
    cr.arc(x + radius, y + h - radius, radius, pi / 2.0, pi);
    cr.arc(x + radius, y + radius, radius, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

fn unpack(color: u32) -> (f64, f64, f64) {
    (
        f64::from((color >> 16) & 0xff) / 255.0,
        f64::from((color >> 8) & 0xff) / 255.0,
        f64::from(color & 0xff) / 255.0,
    )
}

// --- Gallery overlay ------------------------------------------------------

/// One card in the gallery grid.
struct Card {
    id: String,
    child: gtk::FlowBoxChild,
    /// The "this is the active panel" checkmark, toggled when it changes.
    check: gtk::Image,
}

pub struct PanelPickerModel {
    window: gtk::Window,
    /// Slides + fades the panel out from under the bar.
    flyout: flyout::Flyout,
    flow: gtk::FlowBox,
    cards: Vec<Card>,
    current: String,
}

#[derive(Debug)]
pub enum PanelPickerInput {
    /// Flip visibility (sent by the shell from the panel button).
    Toggle,
    /// Hide without choosing (Escape, backdrop click).
    Hide,
    /// Move the grid highlight by this many cards.
    Move(isize),
    /// Apply the highlighted card (Enter).
    Activate,
    /// Apply the card at this position in the grid (click).
    Activated(usize),
    /// The active panel changed elsewhere.
    SetCurrent(String),
}

#[derive(Debug)]
pub enum PanelOutput {
    /// The user picked this panel; the shell persists, applies and rebuilds.
    Apply(String),
}

#[relm4::component(pub)]
impl SimpleComponent for PanelPickerModel {
    type Init = String;
    type Input = PanelPickerInput;
    type Output = PanelOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-picker", "mango-panel-picker"],
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
        // exclusive keyboard while open, hidden until toggled.
        flyout::prepare_layer(&root, "mango-panel-picker");

        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::Single);
        flow.set_column_spacing(10);
        flow.set_row_spacing(10);
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(3);
        flow.set_homogeneous(true);
        flow.set_valign(gtk::Align::Start);
        flow.add_css_class("mango-panel-grid");

        let mut cards = Vec::with_capacity(PANELS.len());
        for panel in PANELS {
            let card = make_card(panel);
            flow.append(&card.child);
            cards.push(card);
        }

        let activated_sender = sender.clone();
        flow.connect_child_activated(move |_, child| {
            let index = child.index();
            if index >= 0 {
                activated_sender.input(PanelPickerInput::Activated(index as usize));
            }
        });

        let panel_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        panel_box.add_css_class("mango-panel-panel");
        panel_box.add_css_class("mango-flyout-panel");
        let title = gtk::Label::new(Some("Panel layout"));
        title.add_css_class("mango-panel-title");
        title.set_xalign(0.0);
        title.set_halign(gtk::Align::Start);
        panel_box.append(&title);
        panel_box.append(&flow);

        let flyout_panel = flyout::Flyout::new(&root);
        flyout_panel.set_child(&panel_box);

        let mut model = PanelPickerModel {
            window: root.clone(),
            flyout: flyout_panel,
            flow,
            cards,
            current: init,
        };

        let widgets = view_output!();

        // Capture phase, so these keys are seen before anything inside the
        // panel.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let key_sender = sender.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                key_sender.input(PanelPickerInput::Hide);
                glib::Propagation::Stop
            }
            gdk::Key::Left | gdk::Key::Up => {
                key_sender.input(PanelPickerInput::Move(-1));
                glib::Propagation::Stop
            }
            gdk::Key::Right | gdk::Key::Down => {
                key_sender.input(PanelPickerInput::Move(1));
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                key_sender.input(PanelPickerInput::Activate);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        // Clicking anywhere outside the panel closes the gallery; clicks on
        // cards stay inside, so they pass through to the grid.
        let hide_sender = sender.clone();
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel_box, &click_root, x, y) {
                hide_sender.input(PanelPickerInput::Hide);
            }
        });
        root.add_controller(click);

        model.highlight_current();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            PanelPickerInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            PanelPickerInput::Hide => self.hide(),
            PanelPickerInput::Move(delta) => self.move_selection(delta),
            PanelPickerInput::Activate => {
                if let Some(index) = self.selected_index() {
                    self.apply(index, &sender);
                }
            }
            PanelPickerInput::Activated(index) => self.apply(index, &sender),
            PanelPickerInput::SetCurrent(id) => self.set_current(&id),
        }
    }
}

impl PanelPickerModel {
    fn show(&mut self) {
        self.highlight_current();
        self.flyout.show(Some(self.flow.upcast_ref()));
    }

    fn hide(&mut self) {
        self.flyout.hide();
    }

    fn selected_index(&self) -> Option<usize> {
        self.flow.selected_children().first().and_then(|child| {
            let index = child.index();
            (index >= 0).then_some(index as usize)
        })
    }

    /// Move the highlight, wrapping around the grid.
    fn move_selection(&self, delta: isize) {
        if self.cards.is_empty() {
            return;
        }
        let count = self.cards.len() as isize;
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        self.flow.select_child(&self.cards[next].child);
    }

    /// Emit the panel of card `index` upwards and mark it active.
    fn apply(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let Some(card) = self.cards.get(index) else {
            return;
        };
        let id = card.id.clone();
        self.set_current(&id);
        let _ = sender.output(PanelOutput::Apply(id));
    }

    /// Mark `id` as the active panel, updating the checkmarks.
    fn set_current(&mut self, id: &str) {
        self.current = id.to_string();
        for card in &self.cards {
            if card.id == id {
                card.check.set_visible(true);
                card.child.add_css_class("current");
            } else {
                card.check.set_visible(false);
                card.child.remove_css_class("current");
            }
        }
    }

    /// Align the grid with the active panel and select it for the keyboard.
    fn highlight_current(&mut self) {
        let id = self.current.clone();
        self.set_current(&id);
        if let Some(position) = self.cards.iter().position(|card| card.id == id) {
            self.flow.select_child(&self.cards[position].child);
        }
    }
}

/// Build the card for a panel: a mock of the bar over its name, blurb and
/// checkmark.
fn make_card(panel: &'static Panel) -> Card {
    let preview = preview_widget(panel, 184, 46);

    let name = gtk::Label::new(Some(panel.name));
    name.add_css_class("mango-panel-name");
    name.set_xalign(0.0);
    name.set_halign(gtk::Align::Start);
    name.set_hexpand(true);

    let check = gtk::Image::from_icon_name("object-select-symbolic");
    check.add_css_class("mango-panel-check");
    check.set_visible(false);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(&name);
    header.append(&check);

    let blurb = gtk::Label::new(Some(panel.blurb));
    blurb.add_css_class("mango-panel-blurb");
    blurb.set_xalign(0.0);
    blurb.set_halign(gtk::Align::Start);
    blurb.set_wrap(true);
    // GTK CSS has no `max-width`; cap the label in points instead so the
    // blurb wraps to two lines rather than stretching the card.
    blurb.set_max_width_chars(34);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.append(&preview);
    body.append(&header);
    body.append(&blurb);

    let child = gtk::FlowBoxChild::new();
    child.add_css_class("mango-panel-card");
    child.set_child(Some(&body));

    Card {
        id: panel.id.to_string(),
        child,
        check,
    }
}
