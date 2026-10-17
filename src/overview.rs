//! The workspace overview: an end-4-style grid of tag previews.
//!
//! Pressing the bar's overview button (or `pkill -RTMIN+1 mango-shell`) opens
//! an `Overlay`-layer fly-out on the focused monitor: the launcher's geometry
//! (see [`crate::flyout`]), a panel hanging below the shell bar over a blurred,
//! dimmed backdrop, rather than a surface that covers the screen. Every tag of
//! that monitor gets a card — a mini rendition of the monitor rect — and every
//! window on that tag is drawn as a tile at its real, scaled position and
//! size, the way [end-4's overview] arranges its `OverviewWindow`s (minus the
//! live screenshots, which Quickshell can capture per-toplevel but a GTK
//! client cannot: [mango]'s IPC has no surface capture, so the tiles are
//! schematic — icon, title and geometry — for *every* tag, not just the
//! visible one).
//!
//! Interaction, all of it resolved against one shared [`Scene`] of
//! window-space rects:
//!
//! - click a card → view that tag; click a tile → view the tag *and* focus
//!   that window ([`mango::jump_to`]); middle-click a tile → close it;
//! - drag a tile onto another card → move the window there;
//! - `1`–`9` jump straight to a tag, arrows move the selection between cards,
//!   `Tab` cycles tiles inside the selected card, `Enter` activates;
//! - `Escape`, a right-click, or a click on the backdrop closes it.
//!
//! The surface stays live: the shell pushes fresh compositor state on every
//! IPC update ([`OverviewInput::SetState`]), so cards repaint while the
//! overview is open — a middle-clicked window disappears, a dragged one lands.
//!
//! [end-4's overview]: https://github.com/end-4/dots-hyprland
//! [mango]: https://mangowm.github.io

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::LayerShell;
use relm4::prelude::*;

use crate::flyout;
use crate::mango::{self, ClientView, MonitorView};

// --- Layout parameters ------------------------------------------------------

/// The panel's width as a fraction of the monitor. The preview hangs below
/// the bar, so it is a panel over the desktop rather than a screenful.
const PANEL_W: f64 = 0.66;

/// The panel's height budget as a fraction of the monitor. Together with
/// [`flyout::BAR_GAP`] above it, this keeps the whole fly-out on screen.
const PANEL_H: f64 = 0.58;

/// `.mango-overview-panel`'s 1px CSS border plus its horizontal padding.
/// [`relayout`](OverviewModel::relayout) budgets the grid against this
/// without measuring it, so keep it in step with `style.css`.
const PANEL_BORDER: i32 = 1;
const PANEL_PAD_X: i32 = PANEL_BORDER + 18;

/// The distance from the surface's top edge down to the grid: the panel's
/// border plus its top padding. Hit-testing is arithmetic (see [`Card::rect`]),
/// so this has to be exactly what the CSS lays out.
const PANEL_PAD_TOP: i32 = PANEL_BORDER + 18;

/// The panel's bottom band: border plus the padding that houses the hint line.
const PANEL_PAD_BOTTOM: i32 = PANEL_BORDER + 42;

/// Gap between cards, as a fraction of the screen width.
const GAP: f64 = 0.012;

/// Tiles never shrink below this, so a sliver of a window is still readable.
const TILE_MIN_W: i32 = 30;
const TILE_MIN_H: i32 = 24;

/// The width of `.mango-ov-card`'s CSS border. A widget's border sits
/// *outside* its content box, so `GtkFixed` (which sizes itself to its
/// children's content minimum) would otherwise land 2px wider than the rect we
/// compute. Keep this in step with `style.css`.
const CARD_BORDER: i32 = 1;

/// A tile at least this wide, and with room for one more text line, shows its
/// title under the icon; smaller ones rely on the tooltip.
const LABEL_MIN_W: i32 = 90;
const LABEL_MIN_H: i32 = 26;

/// How far the pointer must travel before a press turns into a drag.
const DRAG_THRESHOLD: f64 = 6.0;

// --- Animation --------------------------------------------------------------

/// Open fade, close fade, and the settle spring as the grid lands.
const FADE_MS: u64 = 150;
const CLOSE_MS: u64 = 110;
const SETTLE_MS: u64 = 220;
const SETTLE_AMPLITUDE: f64 = 5.0;

/// The animation tick.
const STEP_MS: u64 = 8;

// --- Scene ------------------------------------------------------------------

/// One preview card: the tag it previews, its widget, its number label, and
/// the window-space rect hit-testing is done against.
struct Card {
    tag: u32,
    fixed: gtk::Fixed,
    number: gtk::Label,
    /// Position and size in *window* coordinates, computed by `relayout`.
    rect: (f64, f64, f64, f64),
}

/// One window tile, kept alive across updates so hover, drag and selection
/// classes survive a state push.
struct Tile {
    id: u32,
    /// The card (index into [`Scene::cards`]) this tile currently lives in.
    card: usize,
    widget: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    /// Position and size in *window* coordinates, computed by `sync_tiles`.
    rect: (f64, f64, f64, f64),
}

/// Everything the UI reads, shared with the controllers through an
/// `Rc<RefCell<…>>` — the same shape as the carousel's paint state.
struct Scene {
    monitors: HashMap<String, MonitorView>,
    clients: Vec<ClientView>,
    /// Connector name of the monitor being previewed.
    shown: Option<String>,
    cards: Vec<Card>,
    tiles: Vec<Tile>,
    /// Per-card tile ids in visual order (top-to-bottom, left-to-right),
    /// which is also the order `Tab` cycles them in.
    order: Vec<Vec<u32>>,
    /// Card size in pixels, set by `relayout` and read by `sync_tiles`.
    card_size: (i32, i32),
    /// Keyboard selection: card index, and a tile index within that card.
    selection: (usize, Option<usize>),
    /// A pointer press in flight: (client under it or None, card, x, y).
    press: Option<(Option<u32>, usize, f64, f64)>,
    /// The client id currently being dragged, if any.
    dragging: Option<u32>,
}

impl Scene {
    /// The card whose window-space rect contains `(x, y)`.
    fn card_at(&self, x: f64, y: f64) -> Option<usize> {
        self.cards.iter().position(|card| {
            let (rx, ry, rw, rh) = card.rect;
            x >= rx && x < rx + rw && y >= ry && y < ry + rh
        })
    }

    /// The topmost tile (and its card) whose window-space rect contains
    /// `(x, y)`.
    fn tile_at(&self, x: f64, y: f64) -> Option<(u32, usize)> {
        self.tiles.iter().rev().find_map(|tile| {
            let (rx, ry, rw, rh) = tile.rect;
            (x >= rx && x < rx + rw && y >= ry && y < ry + rh).then_some((tile.id, tile.card))
        })
    }
}

// --- Inputs -----------------------------------------------------------------

#[derive(Debug)]
pub enum OverviewInput {
    /// Flip visibility (sent by the shell on the bar button or `SIGRTMIN+1`).
    Toggle,
    /// Fresh compositor state, pushed by the shell on every IPC update.
    SetState {
        monitors: HashMap<String, MonitorView>,
        clients: Vec<ClientView>,
    },
    /// Pointer moved over the surface (window coordinates).
    Motion(f64, f64),
    /// Pointer button pressed (window coordinates, button number).
    Press(f64, f64, u32),
    /// Pointer button released (window coordinates, button number).
    Release(f64, f64, u32),
    /// A key survived the capture phase on the surface.
    Key(gdk::Key, gdk::ModifierType),
    /// The surface was mapped: rebuild against the real allocation.
    Mapped,
}

// --- Component --------------------------------------------------------------

pub struct OverviewModel {
    window: gtk::Window,
    grid: gtk::Grid,
    /// The pill that rides the pointer while a tile is dragged.
    ghost: gtk::Label,
    scene: Rc<RefCell<Scene>>,
    /// Bumped on every show/hide, so a fade-out that was superseded by a
    /// reopen knows not to take the surface down.
    generation: Rc<Cell<u64>>,
    /// The in-flight close animation, cancelled by a reopen. Shared with the
    /// fade itself, which clears it on completion: glib panics if a source is
    /// removed after it has already run, and a close fade finishes on its own
    /// whenever the user waits before reopening.
    closing: Rc<RefCell<Option<glib::SourceId>>>,
}

#[relm4::component(pub)]
impl SimpleComponent for OverviewModel {
    type Init = ();
    type Input = OverviewInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-overview"],
            set_decorated: false,
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fly-out geometry — the launcher's recipe (see `crate::flyout`): the
        // panel hangs just below the shell bar on whichever monitor is
        // focused, grabbing the keyboard while it is visible.
        flyout::prepare_layer(&root, "mango-overview");

        // The panel is the whole visible surface: its CSS dims what is behind
        // it (the compositor's layer blur supplies the frost) and the grid of
        // cards sits inside it. The surface is as wide as the monitor, so the
        // panel is centred and the scrim never reaches the screen edges.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.set_css_classes(&["mango-overview-panel"]);
        panel.set_halign(gtk::Align::Center);

        let grid = gtk::Grid::new();
        grid.set_css_classes(&["mango-overview-grid"]);
        grid.set_row_homogeneous(true);
        grid.set_column_homogeneous(true);
        grid.set_halign(gtk::Align::Center);
        grid.set_valign(gtk::Align::Center);
        panel.append(&grid);

        // A small hint at the bottom, like the carousel's "Escape to close".
        let hint = gtk::Label::new(Some(
            "Click a workspace to switch \u{b7} click a window to jump \u{b7} \
             drag to move \u{b7} middle-click to close \u{b7} Esc to close",
        ));
        hint.add_css_class("mango-overview-hint");
        hint.set_halign(gtk::Align::Center);
        hint.set_valign(gtk::Align::End);
        // Sits in the panel's bottom band (see PANEL_PAD_BOTTOM), under the
        // grid rather than at the screen edge as it was when this was a
        // fullscreen surface.
        hint.set_margin_bottom(8);
        hint.set_can_target(false);

        // The drag ghost rides above everything, positioned by margins.
        let ghost = gtk::Label::new(None);
        ghost.add_css_class("mango-overview-ghost");
        ghost.set_halign(gtk::Align::Start);
        ghost.set_valign(gtk::Align::Start);
        ghost.set_can_target(false);
        ghost.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&panel));
        overlay.add_overlay(&hint);
        overlay.add_overlay(&ghost);
        root.set_child(Some(&overlay));

        let scene = Rc::new(RefCell::new(Scene {
            monitors: HashMap::new(),
            clients: Vec::new(),
            shown: None,
            cards: Vec::new(),
            tiles: Vec::new(),
            order: Vec::new(),
            card_size: (0, 0),
            selection: (0, None),
            press: None,
            dragging: None,
        }));

        // --- Controllers ----------------------------------------------------
        // Keys in the capture phase, so nothing downstream gets them first.

        let key_sender = sender.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            key_sender.input(OverviewInput::Key(key, state));
            glib::Propagation::Stop
        });
        root.add_controller(keys);

        // One motion controller on the surface: it sees the pointer over any
        // descendant, which is exactly what hover and drag tracking need.
        let motion_sender = sender.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            motion_sender.input(OverviewInput::Motion(x, y));
        });
        root.add_controller(motion);

        // Presses and releases are resolved against the scene in `update`,
        // so a click can jump, switch, close — or start a drag. Button 0
        // listens to every button; the press handler sorts out left/middle.
        let press_sender = sender.clone();
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.connect_pressed(move |gesture, _, x, y| {
            let button = gesture.current_button();
            press_sender.input(OverviewInput::Press(x, y, button));
        });
        let release_sender = sender.clone();
        click.connect_released(move |gesture, _, x, y| {
            let button = gesture.current_button();
            release_sender.input(OverviewInput::Release(x, y, button));
        });
        root.add_controller(click);

        // Mapping means the compositor gave us a real allocation: rebuild the
        // grid against it.
        let map_sender = sender;
        root.connect_map(move |_| {
            map_sender.input(OverviewInput::Mapped);
        });

        let model = OverviewModel {
            window: root.clone(),
            grid,
            ghost,
            scene,
            generation: Rc::new(Cell::new(0)),
            closing: Rc::new(RefCell::new(None)),
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            OverviewInput::Toggle => {
                if self.window.is_visible() && self.closing.borrow().is_none() {
                    self.hide();
                } else {
                    self.show();
                }
            }

            OverviewInput::SetState { monitors, clients } => {
                let visible = self.window.is_visible();
                // The previewed monitor vanishing (hotplug) means closing.
                let gone = {
                    let mut scene = self.scene.borrow_mut();
                    let gone = scene
                        .shown
                        .as_deref()
                        .is_some_and(|name| !monitors.contains_key(name));
                    scene.monitors = monitors;
                    scene.clients = clients;
                    gone
                };
                if !visible {
                    return;
                }
                if gone {
                    self.hide();
                } else {
                    self.rebuild();
                }
            }

            OverviewInput::Motion(x, y) => self.motion(x, y),
            OverviewInput::Press(x, y, button) => self.press(x, y, button),
            OverviewInput::Release(x, y, button) => self.release(x, y, button),
            OverviewInput::Key(key, state) => self.key(key, state),
            OverviewInput::Mapped => self.rebuild(),
        }
    }
}

// --- Behaviour --------------------------------------------------------------

impl OverviewModel {
    /// Open on the focused monitor, with the grid built for its tags.
    fn show(&mut self) {
        // A close fade in flight would fight this reopen: cancel it. A fade
        // that already ran cleared the slot when it finished, so this never
        // tries to remove a dead source — glib panics on that.
        if let Some(source) = self.closing.borrow_mut().take() {
            source.remove();
        }
        self.generation.set(self.generation.get() + 1);

        // Prefer the monitor Mango calls focused; fall back to the first
        // known one. With no compositor state at all (not running inside
        // Mango), stay closed.
        let shown = {
            let scene = self.scene.borrow();
            scene
                .monitors
                .iter()
                .find(|(_, monitor)| monitor.active)
                .or_else(|| scene.monitors.iter().next())
                .map(|(name, _)| name.clone())
        };
        let Some(shown) = shown else {
            return;
        };
        self.scene.borrow_mut().shown = Some(shown.clone());

        // Bind the surface to that monitor before it maps.
        if let Some(monitor) = crate::find_monitor(&shown) {
            self.window.set_monitor(Some(&monitor));
        }

        self.rebuild();

        // Start the keyboard selection on the tag the user is already on.
        {
            let mut scene = self.scene.borrow_mut();
            let active = scene
                .monitors
                .get(&shown)
                .and_then(|monitor| monitor.tags.iter().position(|tag| tag.active))
                .unwrap_or(0);
            scene.selection = (active.min(scene.cards.len().saturating_sub(1)), None);
            scene.press = None;
            scene.dragging = None;
            update_classes(&mut scene);
        }

        self.ghost.set_visible(false);
        self.window.present();
        crate::anim::fade_in(&self.grid, FADE_MS);
        crate::anim::settle(&self.grid, SETTLE_MS, SETTLE_AMPLITUDE);
    }

    /// Fade the grid out, then take the surface down — unless it was reopened
    /// during the fade (then the generation check lets it stay up).
    fn hide(&mut self) {
        if !self.window.is_visible() {
            return;
        }
        self.generation.set(self.generation.get() + 1);
        let epoch = self.generation.get();
        let generation = self.generation.clone();
        let window = self.window.clone();
        {
            let mut scene = self.scene.borrow_mut();
            scene.press = None;
            scene.dragging = None;
            for card in &scene.cards {
                set_class(&card.fixed, "drop", false);
                set_class(&card.fixed, "hover", false);
            }
        }
        self.ghost.set_visible(false);
        let grid = self.grid.clone();
        let closing = self.closing.clone();
        let source = fade_out(&grid, CLOSE_MS, move || {
            if generation.get() == epoch {
                // This fade is still the current one and has run to the end:
                // drop its id so a later reopen cannot cancel a dead source.
                closing.borrow_mut().take();
                window.set_visible(false);
            }
        });
        *self.closing.borrow_mut() = Some(source);
    }

    /// Rebuild cards and tiles from the last pushed state, sized for the
    /// surface's current (or the monitor's) allocation.
    fn rebuild(&mut self) {
        let (shown, monitor) = {
            let scene = self.scene.borrow();
            let Some(shown) = scene.shown.clone() else {
                return;
            };
            let Some(monitor) = scene.monitors.get(&shown).cloned() else {
                return;
            };
            (shown, monitor)
        };
        if monitor.tags.is_empty() || monitor.width == 0 || monitor.height == 0 {
            return;
        }

        {
            let mut scene = self.scene.borrow_mut();
            sync_cards(&mut scene, &self.grid, &monitor);
        }
        self.relayout(&monitor);
        {
            let mut scene = self.scene.borrow_mut();
            sync_tiles(&mut scene, &shown, &monitor);
            // Keep a stale selection pointing at something real.
            let cards = scene.cards.len();
            if scene.selection.0 >= cards {
                scene.selection = (cards.saturating_sub(1), None);
            }
            let order = scene.order.get(scene.selection.0).map_or(0, |o| o.len());
            if scene.selection.1.is_some_and(|tile| tile >= order) {
                scene.selection.1 = None;
            }
            update_classes(&mut scene);
        }
    }

    /// Size the cards and the grid for the panel this surface draws.
    ///
    /// The budget is a fraction of the **monitor**, never of the surface's own
    /// allocation: the surface is anchored top/left/right, so its height *is*
    /// the grid's height. Deriving the card size from it would feed back into
    /// the size it came from.
    fn relayout(&mut self, monitor: &MonitorView) {
        let count = self.scene.borrow().cards.len();
        if count == 0 {
            return;
        }

        // Left/right anchored: the surface is a monitor wide even though the
        // panel inside it is not, which is what centres the grid below. Before
        // the first allocation fall back to the monitor rect — same width.
        let mut width = self.window.width();
        if width <= 0 {
            width = monitor.width as i32;
        }

        let cols = ((count as f64).sqrt().ceil() as usize).clamp(1, count);
        let rows = count.div_ceil(cols);

        let gap = (((monitor.width as f64) * GAP) as i32).max(8);
        let avail_w = (((monitor.width as f64) * PANEL_W) as i32
            - 2 * PANEL_PAD_X
            - (cols as i32 - 1) * gap)
            .max(1);
        let avail_h = (((monitor.height as f64) * PANEL_H) as i32
            - PANEL_PAD_TOP
            - PANEL_PAD_BOTTOM
            - (rows as i32 - 1) * gap)
            .max(1);

        // Cards keep the monitor's aspect ratio, like a scaled-down screen.
        let ratio = f64::from(monitor.width) / f64::from(monitor.height.max(1));
        let by_width = f64::from(avail_w) / cols as f64;
        let by_height = (f64::from(avail_h) / rows as f64) * ratio;
        let card_w = by_width.min(by_height).max(f64::from(TILE_MIN_W)) as i32;
        // Keep cards usable even on an extreme aspect ratio, where the
        // width-fitted card would be a sliver.
        let card_h = (f64::from(card_w) / ratio)
            .round()
            .max(f64::from(TILE_MIN_H)) as i32;

        self.grid.set_row_spacing(gap as u32);
        self.grid.set_column_spacing(gap as u32);
        // The padding lives on the panel's CSS, so the grid claims no margins
        // of its own — its natural size is exactly the card block, which is
        // what the surface takes its height from.
        self.grid.set_margin_start(0);
        self.grid.set_margin_end(0);
        self.grid.set_margin_top(0);
        self.grid.set_margin_bottom(0);

        let mut scene = self.scene.borrow_mut();
        scene.card_size = (card_w, card_h);
        // The panel centres the grid horizontally and hangs it PANEL_PAD_TOP
        // below the surface's top edge, so every card's window-space rect
        // follows from the arithmetic alone — hit-testing never has to wait
        // for GTK's layout.
        let grid_w = cols as i32 * card_w + (cols as i32 - 1) * gap;
        let grid_x = (width - grid_w) / 2;
        let grid_y = PANEL_PAD_TOP;
        for (index, card) in scene.cards.iter_mut().enumerate() {
            card.fixed.set_size_request(card_w, card_h);
            // The number label sizes the card's *content*; ask for 2px less so
            // the card's own 1px border brings the border-box to `card_w`.
            card.number.set_size_request(
                (card_w - 2 * CARD_BORDER).max(0),
                (card_h - 2 * CARD_BORDER).max(0),
            );
            card.rect = (
                f64::from(grid_x + (index as i32 % cols as i32) * (card_w + gap)),
                f64::from(grid_y + (index as i32 / cols as i32) * (card_h + gap)),
                f64::from(card_w),
                f64::from(card_h),
            );
        }
    }

    /// Pointer moved: hover tracking, and drag progress once a press has
    /// travelled past the threshold.
    fn motion(&mut self, x: f64, y: f64) {
        let mut scene = self.scene.borrow_mut();
        if let Some((client, _, px, py)) = scene.press
            && scene.dragging.is_none()
            && ((x - px).powi(2) + (y - py).powi(2)).sqrt() > DRAG_THRESHOLD
        {
            scene.dragging = client;
            if let Some(id) = client
                && let Some(tile) = scene.tiles.iter().find(|tile| tile.id == id)
            {
                tile.widget.add_css_class("dragging");
                let title = tile.title.text().to_string();
                self.ghost
                    .set_text(if title.is_empty() { "window" } else { &title });
                self.ghost.set_visible(true);
            }
        }

        if scene.dragging.is_some() {
            // The ghost follows the pointer, and the card under it lights up
            // as the drop target (but never the card we came from).
            self.ghost.set_margin_start((x - 48.0).max(0.0) as i32);
            self.ghost.set_margin_top((y - 18.0).max(0.0) as i32);
            let target = scene.card_at(x, y);
            let source = scene.press.map(|press| press.1);
            for (index, card) in scene.cards.iter().enumerate() {
                let is_target = target == Some(index) && source != Some(index);
                set_class(&card.fixed, "drop", is_target);
            }
        } else {
            // Plain hover feedback on cards (tiles get GTK's own :hover).
            let hovered = scene.card_at(x, y);
            for (index, card) in scene.cards.iter().enumerate() {
                set_class(&card.fixed, "hover", hovered == Some(index));
            }
        }
    }

    /// Pointer pressed: remember where; close on right-click or a backdrop
    /// click.
    fn press(&mut self, x: f64, y: f64, button: u32) {
        if button == 3 {
            self.hide();
            return;
        }
        let mut scene = self.scene.borrow_mut();
        match (button, scene.card_at(x, y)) {
            // Left-click on bare backdrop: close.
            (1, None) => {
                drop(scene);
                self.hide();
            }
            // Left or middle on a card: remember the press so `release` can
            // tell a click from a drag.
            (1 | 2, Some(card)) => {
                let client = scene.tile_at(x, y).map(|(id, _)| id);
                scene.press = Some((client, card, x, y));
            }
            // Anything else (middle on backdrop, …): ignore.
            _ => {}
        }
    }

    /// Pointer released: a drag lands (or cancels), a press becomes a click —
    /// jump, switch, or close.
    fn release(&mut self, x: f64, y: f64, button: u32) {
        let mut scene = self.scene.borrow_mut();
        let Some((client, card, _, _)) = scene.press.take() else {
            return;
        };

        if let Some(id) = scene.dragging.take() {
            // The drag ended wherever the button came up: clear the
            // highlights, and move the window if a *other* card caught it.
            self.ghost.set_visible(false);
            for card in &scene.cards {
                set_class(&card.fixed, "drop", false);
                set_class(&card.fixed, "hover", false);
            }
            if let Some(tile) = scene.tiles.iter().find(|tile| tile.id == id) {
                tile.widget.remove_css_class("dragging");
            }
            if button == 1
                && let Some(target) = scene.card_at(x, y)
                && target != card
            {
                let tag = scene.cards[target].tag;
                mango::move_client_to_tag(id, tag);
            }
            return;
        }

        let tag = scene.cards.get(card).map(|card| card.tag);
        match (button, client) {
            // Middle-click a tile: close the window, stay open so several
            // can be cleared in one pass.
            (2, Some(id)) => mango::kill_client(id),
            // Left-click a tile: view its tag and focus it.
            (1, Some(id)) => {
                if let Some(tag) = tag {
                    mango::jump_to(tag, id);
                }
                drop(scene);
                self.hide();
            }
            // Left-click a card's bare area: just view the tag.
            (1, None) => {
                if let Some(tag) = tag {
                    mango::switch_tag(tag);
                }
                drop(scene);
                self.hide();
            }
            _ => {}
        }
    }

    /// Keyboard navigation.
    fn key(&mut self, key: gdk::Key, state: gdk::ModifierType) {
        if key == gdk::Key::Escape {
            self.hide();
            return;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let mut scene = self.scene.borrow_mut();
        let cards = scene.cards.len();
        if cards == 0 {
            return;
        }
        let cols = ((cards as f64).sqrt().ceil() as usize).clamp(1, cards);
        let (mut card, mut tile) = scene.selection;

        // Digits 1–9 view that tag outright.
        if let Some(digit) = key.to_unicode().filter(|c| c.is_ascii_digit() && *c != '0') {
            let tag = digit as usize - '0' as usize;
            if tag <= cards {
                mango::switch_tag(scene.cards[tag - 1].tag);
                drop(scene);
                self.hide();
            }
            return;
        }

        match key {
            gdk::Key::Tab => {
                let count = scene.order.get(card).map_or(0, |order| order.len());
                tile = if count == 0 {
                    None
                } else {
                    Some(match (tile, shift) {
                        (None, false) => 0,
                        (None, true) => count - 1,
                        (Some(index), false) => (index + 1) % count,
                        (Some(index), true) => (index + count - 1) % count,
                    })
                };
            }
            gdk::Key::Left => {
                if card > 0 && card % cols != 0 {
                    card -= 1;
                }
                tile = None;
            }
            gdk::Key::Right => {
                if card + 1 < cards && (card + 1) % cols != 0 {
                    card += 1;
                }
                tile = None;
            }
            gdk::Key::Up => {
                if card >= cols {
                    card -= cols;
                }
                tile = None;
            }
            gdk::Key::Down => {
                if card + cols < cards {
                    card += cols;
                }
                tile = None;
            }
            gdk::Key::Home => {
                card = 0;
                tile = None;
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => {
                let tag = scene.cards[card].tag;
                match tile.and_then(|index| scene.order.get(card).and_then(|o| o.get(index))) {
                    Some(id) => mango::jump_to(tag, *id),
                    None => mango::switch_tag(tag),
                }
                drop(scene);
                self.hide();
                return;
            }
            _ => return,
        }

        scene.selection = (card, tile);
        update_classes(&mut scene);
    }
}

// --- Scene maintenance ------------------------------------------------------

/// Set `class` on `widget` when `on`, remove it otherwise.
fn set_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// Make sure the grid has exactly one card per tag, numbered and flagged.
fn sync_cards(scene: &mut Scene, grid: &gtk::Grid, monitor: &MonitorView) {
    let count = monitor.tags.len();
    if count == 0 {
        return;
    }
    let cols = ((count as f64).sqrt().ceil() as usize).max(1);

    if scene.cards.len() != count {
        for card in scene.cards.drain(..) {
            grid.remove(&card.fixed);
        }
        for index in 0..count {
            let fixed = gtk::Fixed::new();
            fixed.add_css_class("mango-ov-card");
            // A window's tile can sit flush with the card edge, so clip at the
            // card boundary to keep even a fullscreen window inside it.
            fixed.set_overflow(gtk::Overflow::Hidden);

            let number = gtk::Label::new(Some(&monitor.tags[index].index.to_string()));
            number.add_css_class("mango-ov-number");
            number.set_xalign(0.5);
            number.set_yalign(0.5);
            number.set_can_target(false);
            fixed.put(&number, 0.0, 0.0);

            scene.cards.push(Card {
                tag: monitor.tags[index].index,
                fixed,
                number,
                rect: (0.0, 0.0, 0.0, 0.0),
            });
        }
        for (index, card) in scene.cards.iter().enumerate() {
            grid.attach(
                &card.fixed,
                (index % cols) as i32,
                (index / cols) as i32,
                1,
                1,
            );
        }
    }

    for (index, card) in scene.cards.iter().enumerate() {
        set_class(
            &card.fixed,
            "active",
            monitor.tags.get(index).is_some_and(|tag| tag.active),
        );
        set_class(
            &card.fixed,
            "urgent",
            monitor.tags.get(index).is_some_and(|tag| tag.urgent),
        );
    }
}

/// Create, move, restyle and hide tiles so the shown monitor's cards match
/// the client list.
fn sync_tiles(scene: &mut Scene, shown: &str, monitor: &MonitorView) {
    let (card_w, card_h) = scene.card_size;
    if card_w <= 0 || card_h <= 0 {
        return;
    }
    // Tiles live inside the card's border, so they scale into the content box.
    let inner_w = (card_w - 2 * CARD_BORDER).max(1);
    let inner_h = (card_h - 2 * CARD_BORDER).max(1);
    let scale_x = f64::from(inner_w) / f64::from(monitor.width.max(1));
    let scale_y = f64::from(inner_h) / f64::from(monitor.height.max(1));

    // Which card each client belongs on, and where its tile sits. A window
    // pinned to several tags is drawn on the first of them (one widget can
    // only live in one card); a global one rides along on every card the
    // window list already says it is on.
    let mut wanted: HashMap<u32, (usize, f64, f64, f64, f64)> = HashMap::new();
    let mut order: Vec<Vec<(u32, i64)>> = vec![Vec::new(); scene.cards.len()];
    for client in &scene.clients {
        if client.monitor != shown {
            continue;
        }
        let Some(card_index) = scene.cards.iter().position(|card| {
            client.tags.contains(&card.tag) || (client.is_global && client.tags.is_empty())
        }) else {
            continue;
        };

        // Place the tile by its real geometry, clamped into the card. The
        // clamps leave room for at least a `TILE_MIN_*` tile, so the content
        // minimum below can never push the allocation past this rect.
        let x = ((f64::from(client.x) - f64::from(monitor.x)) * scale_x)
            .round()
            .clamp(0.0, f64::from(inner_w - TILE_MIN_W).max(0.0));
        let y = ((f64::from(client.y) - f64::from(monitor.y)) * scale_y)
            .round()
            .clamp(0.0, f64::from(inner_h - TILE_MIN_H).max(0.0));
        let w = (f64::from(client.width) * scale_x)
            .round()
            .max(f64::from(TILE_MIN_W))
            .min(f64::from(inner_w) - x)
            .max(1.0);
        let h = (f64::from(client.height) * scale_y)
            .round()
            .max(f64::from(TILE_MIN_H))
            .min(f64::from(inner_h) - y)
            .max(1.0);

        // Reading order for `Tab`: top-to-bottom, then left-to-right.
        let sort_key = ((y.round() as i64) << 32) | (x.round() as i64).max(0);
        order[card_index].push((client.id, sort_key));
        wanted.insert(client.id, (card_index, x, y, w, h));
    }
    for order in &mut order {
        order.sort_by_key(|(_, key)| *key);
    }
    scene.order = order
        .into_iter()
        .map(|o| o.into_iter().map(|(id, _)| id).collect())
        .collect();

    // Drop tiles whose window is gone — and take them out of the card while
    // doing so. The old code only set the widget invisible before forgetting
    // the `Tile`, which left it parented to the card forever: one dead widget
    // per closed window, with the preview's absence hanging entirely on that
    // single flag. Unparenting is what actually retires it. A window that
    // comes back gets a fresh widget built below, tooltip and all.
    scene.tiles.retain(|tile| {
        if wanted.contains_key(&tile.id) {
            return true;
        }
        if tile.widget.parent().is_some() {
            tile.widget.unparent();
        }
        false
    });

    for client in &scene.clients {
        let Some(&(card_index, x, y, w, h)) = wanted.get(&client.id) else {
            continue;
        };

        // Build the widget on first sight.
        if !scene.tiles.iter().any(|tile| tile.id == client.id) {
            let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
            widget.add_css_class("mango-ov-tile");
            widget.set_overflow(gtk::Overflow::Hidden);
            widget.set_halign(gtk::Align::Fill);
            widget.set_valign(gtk::Align::Fill);

            let icon = gtk::Image::from_icon_name(&icon_for(&client.appid));
            icon.set_can_target(false);
            icon.set_halign(gtk::Align::Center);
            icon.set_valign(gtk::Align::Center);
            widget.append(&icon);

            let title = gtk::Label::new(None);
            title.add_css_class("mango-ov-tile-title");
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.set_max_width_chars(18);
            title.set_xalign(0.0);
            title.set_can_target(false);
            title.set_visible(false);
            widget.append(&title);

            widget.set_tooltip_text(Some(&tooltip_for(client)));
            scene.tiles.push(Tile {
                id: client.id,
                card: card_index,
                widget,
                icon,
                title,
                rect: (0.0, 0.0, 0.0, 0.0),
            });
        }

        let Some(tile) = scene.tiles.iter_mut().find(|tile| tile.id == client.id) else {
            continue;
        };
        // A window that changed card (or tag) is moved, not rebuilt.
        tile.card = card_index;
        // Its window-space rect is the card's border-box plus the border (the
        // content box's inset) plus the in-card offset — computed, not
        // measured, so hit-testing is correct on the first visible frame.
        let (card_x, card_y, _, _) = scene.cards[card_index].rect;
        tile.rect = (
            card_x + f64::from(CARD_BORDER) + x,
            card_y + f64::from(CARD_BORDER) + y,
            w,
            h,
        );
        if tile.widget.parent().is_none() {
            scene.cards[card_index].fixed.put(&tile.widget, x, y);
        } else {
            scene.cards[card_index].fixed.move_(&tile.widget, x, y);
        }
        tile.widget.set_size_request(w as i32, h as i32);
        tile.widget.set_visible(true);

        // Icon and label scale with the tile. `GtkFixed` allocates each child
        // its *minimum* size (see `gtkfixedlayout.c`), so the tile comes out
        // exactly the `size_request` set above — as long as its content's
        // minimum stays inside. Keep the icon well under the tile, and only
        // add the title when a text line still fits beside it.
        let icon_px = ((w.min(h) * 0.42) as i32)
            .clamp(12, 64)
            .min((w as i32 - 12).max(8))
            .min((h as i32 - 8).max(8));
        tile.icon.set_pixel_size(icon_px);
        let show_title = w >= f64::from(LABEL_MIN_W) && h >= f64::from(icon_px + LABEL_MIN_H);
        tile.title.set_visible(show_title);
        if show_title {
            tile.title.set_text(&title_for(client));
        }

        set_class(&tile.widget, "focused", client.is_focused);
        set_class(&tile.widget, "floating", client.is_floating);
        set_class(&tile.widget, "global", client.is_global);
    }
}

/// Ring the selection: the selected card, and the selected tile inside it.
fn update_classes(scene: &mut Scene) {
    let (selected, tile) = scene.selection;
    for (index, card) in scene.cards.iter().enumerate() {
        set_class(&card.fixed, "selected", index == selected);
    }
    for (index, order) in scene.order.iter().enumerate() {
        for (tile_index, id) in order.iter().enumerate() {
            if let Some(entry) = scene.tiles.iter_mut().find(|entry| entry.id == *id) {
                let on = index == selected && Some(tile_index) == tile;
                set_class(&entry.widget, "selected", on);
            }
        }
    }
}

/// The best icon name we can guess for an app id.
fn icon_for(appid: &str) -> String {
    let display = gdk::Display::default();
    let theme = display.as_ref().map(gtk::IconTheme::for_display);
    // App ids are usually reverse-DNS ("org.gnome.Nautilus"), while icon
    // themes know either that exact name or the bare app name.
    let candidates = [
        appid.to_string(),
        appid.to_lowercase(),
        appid.split('.').next().unwrap_or(appid).to_string(),
    ];
    if let Some(theme) = theme {
        for candidate in candidates {
            if theme.has_icon(&candidate) {
                return candidate;
            }
        }
    }
    "application-x-executable-symbolic".to_string()
}

/// The tooltip: title first, app id underneath.
fn tooltip_for(client: &ClientView) -> String {
    if client.appid.is_empty() || client.title.is_empty() {
        format!("{}{}", client.title, client.appid)
    } else {
        let appid = &client.appid;
        format!("{}\n[{appid}]", client.title)
    }
}

/// The in-tile label: the title, falling back to the app id.
fn title_for(client: &ClientView) -> String {
    if client.title.is_empty() {
        client.appid.clone()
    } else {
        client.title.clone()
    }
}

/// Fade `widget` out over `ms`, then run `done`. Returns the animation's
/// source id so a reopen can cancel it mid-flight.
fn fade_out<W: IsA<gtk::Widget> + Clone>(
    widget: &W,
    ms: u64,
    done: impl FnOnce() + 'static,
) -> glib::SourceId {
    let widget = widget.clone().upcast::<gtk::Widget>();
    let from = widget.opacity();
    let start = Instant::now();
    let seconds = ms.max(1) as f64 / 1000.0;
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(STEP_MS), move || {
        let t = start.elapsed().as_secs_f64() / seconds;
        if t >= 1.0 {
            widget.set_opacity(1.0);
            if let Some(done) = done.take() {
                done();
            }
            return glib::ControlFlow::Break;
        }
        widget.set_opacity(from * (1.0 - crate::anim::ease_out(t)));
        glib::ControlFlow::Continue
    })
}
