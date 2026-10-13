//! Built-in colour themes and the theme gallery.
//!
//! A theme is nothing but a small block of `@define-color` overrides in
//! `src/themes/<id>.css`. The shell appends the selected theme's block after
//! the base stylesheet `src/style.css`, so the later definitions win — the
//! same trick the optional `~/.config/mango-shell/style.css` override already
//! uses, and the reason a user stylesheet (e.g. a matugen-generated palette)
//! can still override any theme.
//!
//! [`ThemePickerModel`] is a [`Layer::Overlay`] fly-out panel (see
//! [`crate::flyout`]), matching the launcher and wallpaper picker: it hangs
//! under the bar and slides out of it, holding a grid of theme cards, each
//! showing a live palette preview drawn with cairo. Selecting a card emits
//! [`ThemeOutput::Apply`] and leaves the gallery open so the whole shell
//! recolours as you browse. The quick switcher lives in the bar
//! (`widgets/theme.rs`).

use std::path::PathBuf;

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;

/// A selectable colour scheme.
pub struct Theme {
    /// Stable identifier, persisted in `~/.config/mango-shell/theme`.
    pub id: &'static str,
    /// Human-readable name shown in the switchers.
    pub name: &'static str,
    /// `@define-color` overrides appended after the base stylesheet.
    pub css: &'static str,
    /// Colours the switcher UI draws, as packed `0xRRGGBB`:
    /// `[background, surface, primary, secondary, tertiary]`.
    pub preview: [u32; 5],
}

/// Every built-in theme, in switcher order. `THEMES[0]` is the fallback.
pub const THEMES: &[Theme] = &[
    Theme {
        id: "mango",
        name: "Mango",
        css: include_str!("themes/mango.css"),
        preview: [0x201b14, 0x322c23, 0xc9b890, 0x4a4436, 0x3b3226],
    },
    Theme {
        id: "nord",
        name: "Nord",
        css: include_str!("themes/nord.css"),
        preview: [0x2e3440, 0x3b4252, 0x88c0d0, 0x5e81ac, 0xa3be8c],
    },
    Theme {
        id: "catppuccin",
        name: "Catppuccin Mocha",
        css: include_str!("themes/catppuccin.css"),
        preview: [0x1e1e2e, 0x313244, 0xcba6f7, 0x89b4fa, 0xa6e3a1],
    },
    Theme {
        id: "gruvbox",
        name: "Gruvbox",
        css: include_str!("themes/gruvbox.css"),
        preview: [0x282828, 0x3c3836, 0xfabd2f, 0xfe8019, 0xb8bb26],
    },
    Theme {
        id: "tokyonight",
        name: "Tokyo Night",
        css: include_str!("themes/tokyonight.css"),
        preview: [0x1a1b26, 0x292e42, 0x7aa2f7, 0xbb9af7, 0x9ece6a],
    },
];

/// The theme used when nothing has been chosen yet.
pub const DEFAULT_ID: &str = "mango";

/// Look up a theme by id, falling back to the default.
pub fn get(id: &str) -> &'static Theme {
    THEMES
        .iter()
        .find(|theme| theme.id == id)
        .unwrap_or(&THEMES[0])
}

/// Whether `id` names one of the built-in themes.
pub fn is_known(id: &str) -> bool {
    THEMES.iter().any(|theme| theme.id == id)
}

// --- Persistence ----------------------------------------------------------

fn state_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/mango-shell/theme"))
}

/// The theme remembered from a previous run, or the default.
pub fn load_current() -> &'static Theme {
    let Some(state) = state_path() else {
        return get(DEFAULT_ID);
    };
    match std::fs::read_to_string(state) {
        Ok(id) => get(id.trim()),
        Err(_) => get(DEFAULT_ID),
    }
}

/// Remember the chosen theme id (best effort).
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
        eprintln!("mango-shell: could not save theme state: {error}");
    }
}

// --- Palette previews -----------------------------------------------------

/// A widget that paints `theme.preview` as five rounded chips.
///
/// Used by the bar's popover rows and the gallery cards. The chips are drawn
/// with cairo so each switcher shows the real palette without per-theme CSS.
pub fn preview_widget(theme: &'static Theme, width: i32, height: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(width);
    area.set_content_height(height);
    let colors = theme.preview;
    area.set_draw_func(move |_, cr, w, h| draw_preview(cr, w, h, &colors));
    area
}

fn draw_preview(cr: &cairo::Context, width: i32, height: i32, colors: &[u32; 5]) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let count = colors.len() as f64;
    let gap = 3.0;
    let segment = ((w - gap * (count - 1.0)) / count).max(1.0);
    let radius = (h.min(segment) / 2.0).max(1.0);

    for (index, color) in colors.iter().enumerate() {
        let x = index as f64 * (segment + gap);
        rounded_rect(cr, x, 0.0, segment, h, radius);
        let (r, g, b) = unpack(*color);
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
    /// The "this is the active theme" checkmark, toggled when it changes.
    check: gtk::Image,
}

pub struct ThemePickerModel {
    window: gtk::Window,
    /// Slides the panel out from under the bar.
    revealer: gtk::Revealer,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
    flow: gtk::FlowBox,
    cards: Vec<Card>,
    current: String,
}

#[derive(Debug)]
pub enum ThemePickerInput {
    /// Flip visibility (sent by the shell from the theme menu).
    Toggle,
    /// Hide without choosing (Escape, backdrop click).
    Hide,
    /// Move the grid highlight by this many cards.
    Move(isize),
    /// Apply the highlighted card (Enter).
    Activate,
    /// Apply the card at this position in the grid (click).
    Activated(usize),
    /// The active theme changed elsewhere (e.g. via the bar popover).
    SetCurrent(String),
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
}

#[derive(Debug)]
pub enum ThemeOutput {
    /// The user picked this theme; the shell persists and applies it.
    Apply(String),
}

#[relm4::component(pub)]
impl SimpleComponent for ThemePickerModel {
    type Init = String;
    type Input = ThemePickerInput;
    type Output = ThemeOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-picker", "mango-theme-picker"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.revealer.clone() {},
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fly-out surface: hugs the top of the monitor just below the bar,
        // exclusive keyboard while open, hidden until toggled.
        flyout::prepare_layer(&root, "mango-theme-picker");

        let banner = flyout::make_banner();

        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::Single);
        flow.set_column_spacing(10);
        flow.set_row_spacing(10);
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(3);
        flow.set_homogeneous(true);
        flow.set_valign(gtk::Align::Start);
        flow.add_css_class("mango-theme-grid");

        let mut cards = Vec::with_capacity(THEMES.len());
        for theme in THEMES {
            let card = make_card(theme);
            flow.append(&card.child);
            cards.push(card);
        }

        // Clicking a card applies it.
        let activated_sender = sender.clone();
        flow.connect_child_activated(move |_, child| {
            let index = child.index();
            if index >= 0 {
                activated_sender.input(ThemePickerInput::Activated(index as usize));
            }
        });

        // The panel: wallpaper banner, a title, then the grid of cards.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
        panel.add_css_class("mango-theme-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        let title = gtk::Label::new(Some("Theme"));
        title.add_css_class("mango-theme-title");
        title.set_xalign(0.0);
        title.set_halign(gtk::Align::Start);
        panel.append(&title);
        panel.append(&flow);

        let revealer = flyout::make_revealer();
        revealer.set_child(Some(&panel));
        flyout::wire_hide(&root, &revealer);

        let mut model = ThemePickerModel {
            window: root.clone(),
            revealer,
            banner,
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
                key_sender.input(ThemePickerInput::Hide);
                glib::Propagation::Stop
            }
            gdk::Key::Left | gdk::Key::Up => {
                key_sender.input(ThemePickerInput::Move(-1));
                glib::Propagation::Stop
            }
            gdk::Key::Right | gdk::Key::Down => {
                key_sender.input(ThemePickerInput::Move(1));
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                key_sender.input(ThemePickerInput::Activate);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        // Clicking anywhere outside the panel — the transparent fly-out areas
        // over the open window — closes the gallery; clicks on cards stay
        // inside, so they pass through to the grid.
        let hide_sender = sender.clone();
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                hide_sender.input(ThemePickerInput::Hide);
            }
        });
        root.add_controller(click);

        model.highlight_current();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            ThemePickerInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            ThemePickerInput::Hide => self.hide(),
            ThemePickerInput::Move(delta) => self.move_selection(delta),
            ThemePickerInput::Activate => {
                if let Some(index) = self.selected_index() {
                    self.apply(index, &sender);
                }
            }
            ThemePickerInput::Activated(index) => self.apply(index, &sender),
            ThemePickerInput::SetCurrent(id) => self.set_current(&id),
            ThemePickerInput::Banner(path) => flyout::set_banner(&self.banner, path.as_deref()),
        }
    }
}

impl ThemePickerModel {
    fn show(&mut self) {
        self.highlight_current();
        flyout::show(&self.window, &self.revealer, Some(self.flow.upcast_ref()));
    }

    fn hide(&mut self) {
        flyout::hide(&self.revealer);
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

    /// Emit the theme of card `index` upwards and mark it active.
    fn apply(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let Some(card) = self.cards.get(index) else {
            return;
        };
        let id = card.id.clone();
        self.set_current(&id);
        let _ = sender.output(ThemeOutput::Apply(id));
    }

    /// Mark `id` as the active theme, updating the checkmarks.
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

    /// Align the grid with the active theme and select it for the keyboard.
    fn highlight_current(&mut self) {
        let id = self.current.clone();
        self.set_current(&id);
        if let Some(position) = self.cards.iter().position(|card| card.id == id) {
            self.flow.select_child(&self.cards[position].child);
        }
    }
}

/// Build the card for a theme: a palette preview over its name and checkmark.
fn make_card(theme: &'static Theme) -> Card {
    let preview = preview_widget(theme, 168, 40);

    let name = gtk::Label::new(Some(theme.name));
    name.add_css_class("mango-theme-name");
    name.set_xalign(0.0);
    name.set_halign(gtk::Align::Start);
    name.set_hexpand(true);

    let check = gtk::Image::from_icon_name("object-select-symbolic");
    check.add_css_class("mango-theme-check");
    check.set_visible(false);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(&name);
    header.append(&check);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.append(&preview);
    body.append(&header);

    let child = gtk::FlowBoxChild::new();
    child.add_css_class("mango-theme-card");
    child.set_child(Some(&body));

    Card {
        id: theme.id.to_string(),
        child,
        check,
    }
}
