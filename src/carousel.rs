//! Wallpaper carousel: a fullscreen, 3D-skewed wallpaper browser.
//!
//! This is a port of [wallpaperCarousel], the DankMaterialShell / Noctalia
//! Quickshell plugin by motor-dev (itself derived from ilyamiro's picker),
//! onto this shell's Relm4 + GTK4 stack.
//!
//! The plugin is a fullscreen `Overlay`-layer panel: the backdrop is kept
//! transparent so the desktop and any open windows show through behind the
//! cards (the plugin dims it with a scrim; set [`SCRIM`] to bring that back),
//! and every image in the wallpaper directory is laid out along a
//! horizontal line through the centre. Each one is a sheared parallelogram —
//! the *frame* leans by [`SKEW`] while the *image* inside is counter-cropped
//! so the picture itself stays upright — and the frame recedes with distance
//! from the selection, which is how the carousel reads as 3D. The selected
//! card is the biggest and brightest; cards further out shrink, dim and lean
//! away with the plugin's `1 / (1 + distance²)` falloff.
//!
//! Interaction follows the plugin: `←`/`→` (or `h`/`l`) slide, `Home`/`End`
//! jump, `Enter` or a click on a card applies it behind a short zoom-out /
//! fade confirm, `Ctrl+F` opens a search field, the wheel scrolls, and
//! `Escape` backs out one step at a time — clear the filter, then close the
//! search bar, then close the overlay. Clicking empty backdrop space closes it
//! too.
//!
//! GTK CSS cannot shear a widget, so the scrim and every card are painted by
//! hand with cairo in one [`gtk::DrawingArea`] draw function. The paint state
//! lives in a [`Scene`] behind an `Rc<RefCell<…>>`, and a 16 ms tick eases the
//! carousel position and the overall fade; only the search field and the
//! status / empty-state labels are real widgets, stacked over the canvas with
//! a [`gtk::Overlay`].
//!
//! Thumbnails are decoded once at startup, on a worker thread, into the card's
//! aspect ratio and cairo's premultiplied `ARgb32` byte order. `GdkPixbuf` is
//! not `Send`, so only plain bytes leave the worker; the main thread wraps
//! them in a `cairo::ImageSurface`.
//!
//! [wallpaperCarousel]: https://github.com/motor-dev/wallpaperCarousel

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::prelude::*;

use crate::wallpaper;

// --- Ported parameters (the plugin's default settings) --------------------

/// Base card size, in pixels (`itemWidth` / `itemHeight`).
const CARD_WIDTH: f64 = 300.0;
const CARD_HEIGHT: f64 = 420.0;

/// Black border drawn between the card edge and the image (`borderWidth`).
const BORDER: f64 = 3.0;

/// The shear applied to the card frame: `x' = x + SKEW · y`.
const SKEW: f64 = -0.35;

/// Backdrop scrim opacity. `0.0` leaves the overlay fully transparent so the
/// desktop and any open windows show through behind the cards. Raise it (the
/// plugin's `overlayOpacity` default, 80, is `0.8` → `#CC000000`) to dim
/// whatever is underneath instead.
const SCRIM: f64 = 0.0;

/// Scale / opacity falloff: `scale = BASE + (SELECTED - BASE) · falloff(d)`,
/// `alpha = 0.1 + 0.9 · falloff`, `falloff = 1 / (1 + d²)`.
const BASE_SCALE: f64 = 0.75;
const SELECTED_SCALE: f64 = 1.08;
/// Extra scale an `expandSelected`-style hover adds, times the falloff.
const HOVER_BOOST: f64 = 0.20;

/// The picked card zooms to this while everything fades for the confirm.
const CONFIRM_SCALE: f64 = 1.6;

/// Open/close fade, glide between cards, and the confirm animation.
const FADE_MS: u64 = 150;
const MOVE_MS: u64 = 150;
const CONFIRM_MS: u64 = 300;

/// The animation tick.
const FRAME_MS: u64 = 16;

/// Coalesces high-resolution wheel events into one step per notch.
const SCROLL_COOLDOWN_MS: u64 = 80;

/// `true` wraps at the ends (the plugin's default `wrap` mode), `false`
/// clamps (its `standard` mode).
const WRAP: bool = true;

/// Thumbnails are fitted into this box — twice the card so the selected one
/// stays sharp when it scales up — then centre-cropped to the card's aspect
/// ratio.
const DECODE_BOX: (i32, i32) = (600, 840);

// --- Parallax -------------------------------------------------------------

/// Pointer parallax: as the pointer crosses the overlay the centre card's
/// image drifts up to this many pixels towards it. Outer cards drift less, so
/// the picture reads as sunk behind its frame.
const PARALLAX_POINTER: f64 = 60.0;

/// Motion parallax: while the carousel glides, each image lags its frame by
/// this fraction of the per-frame travel, so a slide has a little depth.
const PARALLAX_SCROLL: f64 = 0.6;

/// How much the image is over-scaled so a drifting one still covers the frame.
const PARALLAX_OVERSCAN: f64 = 1.25;

/// Depth weight for the drift: the centre card (`falloff` = 1) drifts fully,
/// distant cards about a third as much.
fn depth_weight(d: f64) -> f64 {
    0.35 + 0.65 * falloff(d)
}

// --- Easing / layout helpers ----------------------------------------------

/// A scalar eased from `from` to `to` with an in-out cubic curve.
struct Anim {
    from: f64,
    to: f64,
    start: Instant,
    ms: u64,
}

impl Anim {
    /// The eased value, or `None` once the animation has run out.
    fn value(&self, now: Instant) -> Option<f64> {
        let t = (now - self.start).as_secs_f64() * 1000.0 / self.ms as f64;
        (t < 1.0).then(|| self.from + (self.to - self.from) * ease(t))
    }
}

/// In-out cubic, matching the plugin's `Easing.InOutCubic`.
fn ease(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// The plugin's `falloff = 1 / (1 + distance²)`.
fn falloff(d: f64) -> f64 {
    1.0 / (1.0 + d * d)
}

/// Card scale at signed distance `d` from the selection.
fn base_scale(d: f64) -> f64 {
    BASE_SCALE + (SELECTED_SCALE - BASE_SCALE) * falloff(d)
}

/// Card opacity at signed distance `d` from the selection.
fn base_alpha(d: f64) -> f64 {
    0.1 + 0.9 * falloff(d)
}

// --- Scene ----------------------------------------------------------------

/// Everything the painter reads, shared with the draw function.
struct Scene {
    /// Every wallpaper on disk, in display order; fixed after init.
    paths: Vec<PathBuf>,
    /// Lowercased file names, parallel to `paths`, for the filter.
    names: Vec<String>,
    /// One decoded card image per wallpaper (worker → main thread).
    thumbs: Vec<Option<cairo::ImageSurface>>,
    /// Indices into `paths` matching the current query.
    matches: Vec<usize>,
    /// The active lowercased query (empty when unfiltered).
    query: String,
    /// Whether the `Ctrl+F` search field is on screen.
    search_open: bool,
    /// The selected slot within the active list, and the animated
    /// fractional position used for painting.
    target: usize,
    pos: f64,
    anim: Option<Anim>,
    /// The overall open/close fade, animated.
    fade: f64,
    fade_anim: Option<Anim>,
    /// Take the surface down once the fade-out finishes.
    hide_when_faded: bool,
    /// The confirm zoom/fade: the picked slot and when it started.
    confirm: Option<(usize, Instant)>,
    /// The slot under the pointer, if any.
    hover: Option<usize>,
    /// The pointer position in canvas pixels, for the parallax drift. `None`
    /// when the pointer is off the overlay.
    pointer: Option<(f64, f64)>,
    /// Cards travelled per frame on the last tick, for the motion parallax.
    velocity: f64,
    /// Thumbnail loading progress.
    loading: bool,
    loaded: usize,
}

impl Scene {
    /// How many slots the carousel is currently laid out over.
    fn active_len(&self) -> usize {
        if self.query.is_empty() {
            self.paths.len()
        } else {
            self.matches.len()
        }
    }

    /// The index into `paths` sitting in slot `slot`.
    fn active_image(&self, slot: usize) -> usize {
        if self.query.is_empty() {
            slot
        } else {
            self.matches[slot]
        }
    }

    /// The slot wallpaper `image` currently occupies, if it is in the list.
    fn slot_of_image(&self, image: usize) -> Option<usize> {
        if self.query.is_empty() {
            (image < self.paths.len()).then_some(image)
        } else {
            self.matches.iter().position(|&m| m == image)
        }
    }

    /// Signed distance from the animated position, wrapped to the nearest
    /// copy when [`WRAP`] — the reason the loop is seamless.
    fn signed_dist(&self, slot: usize) -> f64 {
        let n = self.active_len() as f64;
        let mut d = slot as f64 - self.pos;
        if WRAP && n > 1.0 {
            let half = n / 2.0;
            if d > half {
                d -= n;
            } else if d < -half {
                d += n;
            }
        }
        d
    }

    /// The slot under `(x, y)`, if any. Parallelograms are tested from the
    /// centre outward, so overlapping cards resolve like the plugin's z order:
    /// the card nearest the centre wins.
    fn hit_test(&self, width: i32, height: i32, x: f64, y: f64) -> Option<usize> {
        let w = f64::from(width);
        let h = f64::from(height);
        let max_d = (w / 2.0 + CARD_WIDTH * SELECTED_SCALE) / CARD_WIDTH + 1.0;
        let mut best: Option<(usize, f64)> = None;
        for slot in 0..self.active_len() {
            let d = self.signed_dist(slot);
            if d.abs() > max_d {
                continue;
            }
            let sc = base_scale(d);
            // Invert `translate · scale · shear`.
            let v = (y - h / 2.0) / sc;
            let u = (x - (w / 2.0 + d * CARD_WIDTH)) / sc - SKEW * v;
            if u.abs() <= CARD_WIDTH / 2.0
                && v.abs() <= CARD_HEIGHT / 2.0
                && best.is_none_or(|(_, best_d)| d.abs() < best_d.abs())
            {
                best = Some((slot, d));
            }
        }
        best.map(|(slot, _)| slot)
    }
}

// --- Inputs / outputs -----------------------------------------------------

#[derive(Debug)]
pub enum CarouselInput {
    /// Flip visibility (sent by the shell on `SIGUSR2` or the bar button).
    Toggle,
    /// `Escape`: clear the filter, then close the search, then close.
    Escape,
    /// Show/hide the `Ctrl+F` search field.
    ToggleSearch,
    /// The query text changed.
    Search(String),
    /// Slide the carousel `delta` cards (keys).
    Move(isize),
    /// Slide one card from the mouse wheel, rate-limited.
    Scroll(isize),
    /// Jump to a slot, clamped to the last card (`Home`/`End`).
    Jump(usize),
    /// Apply the selected wallpaper (`Enter`).
    Activate,
    /// A click on the canvas: apply the card, or close on empty space.
    Click { x: f64, y: f64 },
    /// The active wallpaper changed elsewhere.
    SetCurrent(PathBuf),
    /// Advance the animations one frame.
    Tick,
    /// A decoded card image (worker → main).
    Thumb {
        index: usize,
        width: i32,
        height: i32,
        bgra: Vec<u8>,
    },
}

#[derive(Debug)]
pub enum CarouselOutput {
    /// The user picked this wallpaper; the shell persists and shows it.
    Apply(PathBuf),
}

// --- Component ------------------------------------------------------------

pub struct CarouselModel {
    window: gtk::Window,
    canvas: gtk::DrawingArea,
    search: gtk::SearchEntry,
    status: gtk::Label,
    empty: gtk::Box,
    scene: Rc<RefCell<Scene>>,
    /// Whether the animation tick is running.
    ticking: Rc<Cell<bool>>,
    sender: ComponentSender<Self>,
    /// The active wallpaper, centred on the next open.
    current: Option<PathBuf>,
    /// The last text pushed to `status`, so ticks don't relayout the label.
    last_status: String,
    last_scroll: Instant,
}

#[relm4::component(pub)]
impl SimpleComponent for CarouselModel {
    type Init = Option<PathBuf>;
    type Input = CarouselInput;
    type Output = CarouselOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-carousel"],
            set_decorated: false,
        }
    }

    fn init(
        current: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fullscreen overlay: covers every other surface, grabs the keyboard
        // while open, hidden until toggled — the plugin's `PanelWindow`.
        root.init_layer_shell();
        root.set_namespace(Some("mango-carousel"));
        root.set_layer(Layer::Overlay);
        root.set_keyboard_mode(KeyboardMode::Exclusive);
        root.set_exclusive_zone(-1);
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Bottom, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);
        root.set_visible(false);

        // The corpus: every image in the wallpaper directory, sorted by name.
        let dir = wallpaper::wallpaper_dir();
        let paths = dir
            .as_deref()
            .map(wallpaper::scan_images)
            .unwrap_or_default();
        let names = paths.iter().map(|path| file_name(path).to_lowercase()).collect();

        // Shared paint state. The remembered wallpaper is centred right away,
        // so the first frame is already framed (the plugin's `tryFocus`).
        let scene = Rc::new(RefCell::new(Scene {
            thumbs: Vec::new(),
            matches: Vec::new(),
            query: String::new(),
            search_open: false,
            target: 0,
            pos: 0.0,
            anim: None,
            fade: 0.0,
            fade_anim: None,
            hide_when_faded: false,
            confirm: None,
            hover: None,
            pointer: None,
            velocity: 0.0,
            loading: false,
            loaded: 0,
            paths,
            names,
        }));
        {
            let mut scene = scene.borrow_mut();
            scene.thumbs = scene.paths.iter().map(|_| None).collect();
            if let Some(current) = &current
                && let Some(image) = scene.paths.iter().position(|path| path == current)
            {
                scene.target = image;
                scene.pos = image as f64;
            }
        }

        // The one widget everything is painted into.
        let canvas = gtk::DrawingArea::new();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        let paint_scene = scene.clone();
        canvas.set_draw_func(move |_, cr, width, height| {
            paint(cr, width, height, &paint_scene.borrow());
        });

        // The `Ctrl+F` search pill, mirroring the plugin's search box.
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search wallpapers…"));
        search.add_css_class("mango-carousel-search");
        search.set_size_request(360, -1);
        search.set_halign(gtk::Align::Center);
        search.set_valign(gtk::Align::Start);
        search.set_margin_top(24);
        search.set_visible(false);

        // The bottom status line: selected file, position, match counts.
        let status = gtk::Label::new(None);
        status.add_css_class("mango-carousel-status");
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::End);
        status.set_margin_bottom(28);
        status.set_can_target(false);
        status.set_visible(false);

        // The empty-state card the plugin prints for a missing or empty
        // folder.
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
        empty.add_css_class("mango-carousel-empty");
        empty.set_halign(gtk::Align::Center);
        empty.set_valign(gtk::Align::Center);
        empty.set_can_target(false);
        empty.set_visible(false);
        let title = gtk::Label::new(Some("No images found"));
        title.add_css_class("mango-carousel-title");
        let subtitle = gtk::Label::new(Some(&match &dir {
            Some(dir) => format!("The folder “{}” is empty.", dir.display()),
            None => "No wallpaper directory is configured.".to_string(),
        }));
        subtitle.add_css_class("mango-carousel-sub");
        subtitle.set_justify(gtk::Justification::Center);
        subtitle.set_wrap(true);
        subtitle.set_max_width_chars(44);
        let hint = gtk::Label::new(Some("Press Escape to close"));
        hint.add_css_class("mango-carousel-hint");
        empty.append(&title);
        empty.append(&subtitle);
        empty.append(&hint);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&canvas));
        overlay.add_overlay(&search);
        overlay.add_overlay(&status);
        overlay.add_overlay(&empty);
        root.set_child(Some(&overlay));

        // Capture phase, so the keys are seen before the search entry. While
        // the search is open only `Escape` and `Enter` are taken; every other
        // key reaches the text field so the caret and IME keep working.
        let key_scene = scene.clone();
        let key_sender = sender.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            let (search_open, confirming) = {
                let scene = key_scene.borrow();
                (scene.search_open, scene.confirm.is_some())
            };
            if confirming {
                // The confirm animation owns the overlay for its 300 ms.
                return glib::Propagation::Stop;
            }
            if search_open {
                return match key {
                    gdk::Key::Escape => {
                        key_sender.input(CarouselInput::Escape);
                        glib::Propagation::Stop
                    }
                    gdk::Key::Return | gdk::Key::KP_Enter => {
                        key_sender.input(CarouselInput::Activate);
                        glib::Propagation::Stop
                    }
                    // Typing, cursor movement, IME — all the entry's.
                    _ => glib::Propagation::Proceed,
                };
            }
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Escape => {
                    key_sender.input(CarouselInput::Escape);
                    glib::Propagation::Stop
                }
                gdk::Key::f if ctrl => {
                    key_sender.input(CarouselInput::ToggleSearch);
                    glib::Propagation::Stop
                }
                gdk::Key::Left | gdk::Key::h => {
                    key_sender.input(CarouselInput::Move(-1));
                    glib::Propagation::Stop
                }
                gdk::Key::Right | gdk::Key::l => {
                    key_sender.input(CarouselInput::Move(1));
                    glib::Propagation::Stop
                }
                gdk::Key::Home => {
                    key_sender.input(CarouselInput::Jump(0));
                    glib::Propagation::Stop
                }
                gdk::Key::End => {
                    key_sender.input(CarouselInput::Jump(usize::MAX));
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    key_sender.input(CarouselInput::Activate);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        root.add_controller(keys);

        // Hovering lights a card up; the plugin's `containsMouse`.
        let motion_scene = scene.clone();
        let motion_canvas = canvas.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            let mut scene = motion_scene.borrow_mut();
            if scene.confirm.is_some() {
                return;
            }
            let old = scene.pointer;
            scene.pointer = Some((x, y));
            let hit = scene.hit_test(motion_canvas.width(), motion_canvas.height(), x, y);
            let changed = hit != scene.hover || old != scene.pointer;
            scene.hover = hit;
            drop(scene);
            if changed {
                motion_canvas.queue_draw();
            }
        });
        let leave_scene = scene.clone();
        let leave_canvas = canvas.clone();
        motion.connect_leave(move |_| {
            let mut scene = leave_scene.borrow_mut();
            let hover_changed = scene.hover.take().is_some();
            let pointer_changed = scene.pointer.take().is_some();
            if hover_changed || pointer_changed {
                drop(scene);
                leave_canvas.queue_draw();
            }
        });
        canvas.add_controller(motion);

        // Clicks are resolved in `update` (they can apply a wallpaper).
        let click_sender = sender.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            click_sender.input(CarouselInput::Click { x, y });
        });
        canvas.add_controller(click);

        // The wheel slides the carousel, like the plugin's views.
        let scroll_sender = sender.clone();
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll(move |_, dx, dy| {
            let delta = if dy != 0.0 { dy } else { dx };
            if delta != 0.0 {
                scroll_sender.input(CarouselInput::Scroll(if delta > 0.0 { 1 } else { -1 }));
            }
            glib::Propagation::Stop
        });
        canvas.add_controller(scroll);

        // Filter on every text change (immediate, unlike `search-changed`).
        let search_sender = sender.clone();
        search.connect_changed(move |entry| {
            search_sender.input(CarouselInput::Search(entry.text().to_string()));
        });

        // Pre-cache every thumbnail at boot, like the plugin does.
        let loading = !scene.borrow().paths.is_empty();
        if loading {
            scene.borrow_mut().loading = true;
            let paths = scene.borrow().paths.clone();
            spawn_loader(paths, sender.clone());
        }

        let model = CarouselModel {
            window: root.clone(),
            canvas,
            search,
            status,
            empty,
            scene,
            ticking: Rc::new(Cell::new(false)),
            sender,
            current,
            last_status: String::new(),
            last_scroll: Instant::now() - Duration::from_secs(1),
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            CarouselInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            CarouselInput::Escape => self.escape(),
            CarouselInput::ToggleSearch => self.toggle_search(),
            CarouselInput::Search(query) => self.apply_query(&query),
            CarouselInput::Move(delta) => self.move_selection(delta),
            CarouselInput::Scroll(delta) => {
                if self.last_scroll.elapsed() >= Duration::from_millis(SCROLL_COOLDOWN_MS) {
                    self.last_scroll = Instant::now();
                    self.move_selection(delta);
                }
            }
            CarouselInput::Jump(slot) => self.animate_to(slot),
            CarouselInput::Activate => self.activate(),
            CarouselInput::Click { x, y } => self.click(x, y),
            CarouselInput::SetCurrent(path) => self.current = Some(path),
            CarouselInput::Tick => self.tick(),
            CarouselInput::Thumb {
                index,
                width,
                height,
                bgra,
            } => self.set_thumb(index, width, height, bgra),
        }
    }
}

// The window's child is set imperatively in `init` (the canvas is a plain
// widget, not a model field the `view!` macro knows how to place). The overlay
// field keeps the whole widget tree alive for as long as the component does.
impl CarouselModel {
    /// Reset and present, centring on the active wallpaper — the plugin's
    /// `open()` + `tryFocus()`.
    fn show(&mut self) {
        {
            let mut scene = self.scene.borrow_mut();
            scene.query.clear();
            scene.matches.clear();
            scene.search_open = false;
            scene.confirm = None;
            scene.hover = None;
            scene.pointer = None;
            scene.velocity = 0.0;
            scene.hide_when_faded = false;
            let image = self
                .current
                .as_ref()
                .and_then(|current| scene.paths.iter().position(|path| path == current))
                .unwrap_or(0);
            scene.target = scene.slot_of_image(image).unwrap_or(0);
            scene.pos = scene.target as f64;
            scene.anim = None;
            scene.fade_anim = Some(Anim {
                from: scene.fade,
                to: 1.0,
                start: Instant::now(),
                ms: FADE_MS,
            });
        }
        self.search.set_text("");
        self.search.set_visible(false);
        self.window.present();
        self.refresh_status();
        self.ensure_tick();
    }

    /// Fade out, then take the surface down once the scrim is gone.
    fn hide(&mut self) {
        {
            let mut scene = self.scene.borrow_mut();
            if scene.confirm.is_some() {
                return;
            }
            scene.hide_when_faded = true;
            scene.fade_anim = Some(Anim {
                from: scene.fade,
                to: 0.0,
                start: Instant::now(),
                ms: FADE_MS,
            });
        }
        self.ensure_tick();
    }

    /// `Escape` backs out one layer at a time: clear the filter, then close
    /// the search bar, then close the overlay.
    fn escape(&mut self) {
        let (has_query, search_open) = {
            let scene = self.scene.borrow();
            (!scene.query.is_empty(), scene.search_open)
        };
        if has_query {
            self.search.set_text("");
        } else if search_open {
            self.toggle_search();
        } else {
            self.hide();
        }
    }

    fn toggle_search(&mut self) {
        let open = !self.scene.borrow().search_open;
        self.scene.borrow_mut().search_open = open;
        self.search.set_visible(open);
        if open {
            self.search.grab_focus();
        } else {
            self.window.child_focus(gtk::DirectionType::TabForward);
        }
    }

    /// Recompute `matches` for `query` and re-centre the carousel.
    fn apply_query(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        if query == self.scene.borrow().query {
            // Also fires when `show` clears the entry; nothing to do.
            return;
        }
        let mut scene = self.scene.borrow_mut();
        // Keep the same wallpaper selected when the filter clears. With no
        // active slots there is nothing to remember (guards the empty-match
        // case: a query that matched nothing leaves `matches` empty).
        let previous = (scene.active_len() > 0).then(|| scene.active_image(scene.target));
        scene.query = query.clone();
        scene.matches = if query.is_empty() {
            Vec::new()
        } else {
            (0..scene.paths.len())
                .filter(|&index| scene.names[index].contains(&query))
                .collect()
        };
        scene.target = match (query.is_empty(), previous) {
            (true, Some(previous)) => scene.slot_of_image(previous).unwrap_or(0),
            // The plugin's `_applySearch` jumps to the first match; an empty
            // result parks on slot 0 (nothing is laid out to show anyway).
            _ => 0,
        };
        scene.pos = scene.target as f64;
        scene.anim = None;
        drop(scene);
        self.refresh_status();
        self.canvas.queue_draw();
    }

    fn move_selection(&mut self, delta: isize) {
        let next = {
            let scene = self.scene.borrow();
            if scene.confirm.is_some() {
                return;
            }
            let n = scene.active_len() as isize;
            if n == 0 {
                return;
            }
            let slot = if WRAP {
                (scene.target as isize + delta).rem_euclid(n)
            } else {
                (scene.target as isize + delta).clamp(0, n - 1)
            };
            slot as usize
        };
        self.animate_to(next);
    }

    /// Glide `pos` to `slot` along the shortest path, wrap-aware.
    fn animate_to(&mut self, slot: usize) {
        {
            let mut scene = self.scene.borrow_mut();
            if scene.confirm.is_some() || scene.active_len() == 0 {
                return;
            }
            let slot = slot.min(scene.active_len() - 1);
            scene.target = slot;
            let from = scene.pos;
            let to = if WRAP {
                let n = scene.active_len() as f64;
                let mut d = slot as f64 - from;
                if d > n / 2.0 {
                    d -= n;
                } else if d < -n / 2.0 {
                    d += n;
                }
                from + d
            } else {
                slot as f64
            };
            if (to - from).abs() < f64::EPSILON {
                scene.pos = to;
                scene.anim = None;
            } else {
                scene.anim = Some(Anim {
                    from,
                    to,
                    start: Instant::now(),
                    ms: MOVE_MS,
                });
            }
        }
        self.ensure_tick();
        self.canvas.queue_draw();
        self.refresh_status();
    }

    fn activate(&mut self) {
        let slot = {
            let scene = self.scene.borrow();
            if scene.confirm.is_some() || scene.active_len() == 0 {
                return;
            }
            scene.target
        };
        self.apply(slot);
    }

    /// Click: a card applies itself from wherever it is (the plugin lets any
    /// card be picked); empty scrim space closes the overlay.
    fn click(&mut self, x: f64, y: f64) {
        let slot = {
            let scene = self.scene.borrow();
            if scene.confirm.is_some() {
                return;
            }
            scene.hit_test(self.canvas.width(), self.canvas.height(), x, y)
        };
        match slot {
            Some(slot) => self.apply(slot),
            None => self.hide(),
        }
    }

    /// Start the confirm zoom/fade and hand the wallpaper to the shell right
    /// away, like the plugin: the swap happens behind the fading scrim, then
    /// the overlay closes itself.
    fn apply(&mut self, slot: usize) {
        let path = {
            let scene = self.scene.borrow();
            if slot >= scene.active_len() {
                None
            } else {
                scene.paths.get(scene.active_image(slot)).cloned()
            }
        };
        let Some(path) = path else {
            return;
        };
        {
            let mut scene = self.scene.borrow_mut();
            scene.confirm = Some((slot, Instant::now()));
            scene.hover = None;
            scene.pointer = None;
        }
        self.canvas.queue_draw();
        self.ensure_tick();
        let _ = self.sender.output(CarouselOutput::Apply(path));
    }

    /// Turn the worker's bytes into a cairo surface the draw function paints.
    fn set_thumb(&mut self, index: usize, width: i32, height: i32, bgra: Vec<u8>) {
        let surface = match cairo::ImageSurface::create_for_data(
            bgra,
            cairo::Format::ARgb32,
            width,
            height,
            width * 4,
        ) {
            Ok(surface) => Some(surface),
            Err(error) => {
                eprintln!("mango-shell: could not build carousel thumbnail: {error}");
                None
            }
        };
        {
            let mut scene = self.scene.borrow_mut();
            if let Some(slot) = scene.thumbs.get_mut(index) {
                *slot = surface;
            }
            scene.loaded += 1;
            if scene.loaded >= scene.paths.len() {
                scene.loading = false;
            }
        }
        self.refresh_status();
        self.canvas.queue_draw();
    }

    /// Advance every animation one frame. The tick re-arms itself until
    /// nothing is animating, then stops until the next interaction.
    fn tick(&mut self) {
        let now = Instant::now();
        let mut busy = false;
        let mut hide_now = false;
        {
            let mut scene = self.scene.borrow_mut();

            match scene.anim.as_ref().and_then(|anim| anim.value(now)) {
                Some(value) => {
                    let vel = value - scene.pos;
                    scene.pos = value;
                    scene.velocity = scene.velocity * 0.6 + vel * 0.4;
                    busy = true;
                }
                None => {
                    if let Some(anim) = scene.anim.take() {
                        scene.pos = anim.to;
                    }
                    scene.velocity *= 0.8;
                    if scene.velocity.abs() < 0.02 {
                        scene.velocity = 0.0;
                    }
                }
            }

            match scene.fade_anim.as_ref().and_then(|anim| anim.value(now)) {
                Some(value) => {
                    scene.fade = value;
                    busy = true;
                }
                None => {
                    if let Some(anim) = scene.fade_anim.take() {
                        scene.fade = anim.to;
                        if anim.to == 0.0 && scene.hide_when_faded {
                            scene.hide_when_faded = false;
                            hide_now = true;
                        }
                    }
                }
            }

            if let Some((_, start)) = scene.confirm {
                if now.duration_since(start).as_millis() as u64 >= CONFIRM_MS {
                    scene.confirm = None;
                    scene.hide_when_faded = true;
                    scene.fade_anim = Some(Anim {
                        from: scene.fade,
                        to: 0.0,
                        start: now,
                        ms: FADE_MS,
                    });
                }
                busy = true;
            }
        }
        if hide_now {
            self.window.set_visible(false);
        }
        if busy {
            self.canvas.queue_draw();
        }
        self.refresh_status();
        if !busy {
            self.ticking.set(false);
        }
    }

    /// Keep a 16 ms tick running while anything animates.
    fn ensure_tick(&mut self) {
        if self.ticking.get() {
            return;
        }
        self.ticking.set(true);
        let sender = self.sender.clone();
        let ticking = self.ticking.clone();
        glib::timeout_add_local(Duration::from_millis(FRAME_MS), move || {
            if !ticking.get() {
                return glib::ControlFlow::Break;
            }
            sender.input(CarouselInput::Tick);
            glib::ControlFlow::Continue
        });
    }

    /// Sync the status line (and the empty-state card) with the scene.
    fn refresh_status(&mut self) {
        let scene = self.scene.borrow();
        let empty_folder = scene.paths.is_empty();
        let fade = scene.fade;
        let text = if empty_folder {
            // The empty-state card takes over.
            String::new()
        } else if scene.loading {
            format!("Loading previews… {}/{}", scene.loaded, scene.paths.len())
        } else if !scene.query.is_empty() {
            let count = scene.matches.len();
            if count == 0 {
                format!("No matches for “{}”", scene.query)
            } else {
                format!("{} match{}", count, if count == 1 { "" } else { "es" })
            }
        } else {
            let image = scene.active_image(scene.target);
            format!(
                "{}  ·  {}/{}",
                file_name(&scene.paths[image]),
                scene.target + 1,
                scene.active_len()
            )
        };
        drop(scene);

        if text != self.last_status {
            self.status.set_label(&text);
            self.last_status = text.clone();
        }
        self.status.set_visible(self.window.is_visible() && !text.is_empty());
        self.status.set_opacity(fade.clamp(0.0, 1.0));
        self.empty.set_visible(self.window.is_visible() && empty_folder);
        self.empty.set_opacity(fade.clamp(0.0, 1.0));
    }
}

/// The file name of `path` as UTF-8, or an empty string.
fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|name| name.to_str()).unwrap_or_default()
}

// --- Painting -------------------------------------------------------------

/// Paint one frame: the scrim, then every visible card, farthest first.
fn paint(cr: &cairo::Context, width: i32, height: i32, scene: &Scene) {
    let width = f64::from(width);
    let height = f64::from(height);
    if scene.fade <= f64::EPSILON {
        return;
    }

    // The backdrop. Transparent by default ([`SCRIM`] = 0), so this only does
    // anything if the scrim is turned back on.
    cr.set_source_rgba(0.0, 0.0, 0.0, SCRIM * scene.fade);
    let _ = cr.paint();

    if scene.active_len() == 0 {
        return;
    }

    // How far out cards are still (partly) on screen.
    let max_d = (width / 2.0 + CARD_WIDTH * SELECTED_SCALE) / CARD_WIDTH + 1.0;
    let mut cards: Vec<(usize, f64)> = (0..scene.active_len())
        .map(|slot| (slot, scene.signed_dist(slot)))
        .filter(|(_, d)| d.abs() <= max_d)
        .collect();
    // Farthest first, so nearer cards paint on top (the plugin's z order).
    cards.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));

    let confirm = scene.confirm.map(|(slot, start)| {
        let t = (start.elapsed().as_millis() as f64 / CONFIRM_MS as f64).min(1.0);
        (slot, ease(t))
    });

    for (slot, d) in cards {
        let Some(surface) = scene
            .thumbs
            .get(scene.active_image(slot))
            .and_then(Option::as_ref)
        else {
            // Not decoded yet: the plugin leaves the slot invisible too.
            continue;
        };

        let mut sc = base_scale(d);
        let mut alpha = base_alpha(d);
        if scene.confirm.is_none() && scene.hover == Some(slot) {
            // The plugin's hovered card: fully lit and slightly popped.
            sc += HOVER_BOOST * falloff(d);
            alpha = 1.0;
        }
        if let Some((picked, t)) = confirm {
            if picked == slot {
                sc += (CONFIRM_SCALE - sc) * t;
            } else {
                sc *= 1.0 - 0.2 * t;
            }
            alpha *= 1.0 - t;
        }
        alpha *= scene.fade;
        if alpha <= 0.005 {
            continue;
        }
        paint_card(cr, width / 2.0 + d * CARD_WIDTH, height / 2.0, sc, alpha, surface, d, scene);
    }
}

/// One card: a sheared frame with the image inside, re-fitted after clipping
/// so the picture itself stays upright.
fn paint_card(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    scale: f64,
    alpha: f64,
    surface: &cairo::ImageSurface,
    d: f64,
    scene: &Scene,
) {
    let hw = CARD_WIDTH * scale / 2.0;
    let hh = CARD_HEIGHT * scale / 2.0;

    let _ = cr.save();
    // The frame, filled black — the plugin's `borderWidth` ring.
    parallelogram(cr, cx, cy, hw, hh, SKEW);
    cr.clip();
    cr.set_source_rgba(0.0, 0.0, 0.0, alpha);
    let _ = cr.paint();

    // The inner well the image sits in, inset by the border.
    let inset = (BORDER * scale).min(hw.min(hh) / 2.0);
    parallelogram(cr, cx, cy, hw - inset, hh - inset, SKEW);
    cr.clip();
    // Cover-fit the (already card-aspect) image over the inner parallelogram's
    // axis-aligned bounding box, with a small overscan and pointer + motion
    // parallax to create a subtle depth effect.
    let sw = f64::from(surface.width());
    let sh = f64::from(surface.height());
    let box_w = 2.0 * (hw - inset) + 2.0 * SKEW.abs() * (hh - inset);
    let box_h = 2.0 * (hh - inset);
    let fit = (box_w / sw).max(box_h / sh) * PARALLAX_OVERSCAN;
    let scaled_w = sw * fit;
    let scaled_h = sh * fit;
    let (px_n, py_n) = scene
        .pointer
        .map(|(x, y)| {
            let vw = box_w * 2.0 / scale.max(0.01); // rough viewport width reference not needed; use screen
            (x, y)
        })
        .unwrap_or((0.0, 0.0));
    let (px_n2, py_n2) = scene
        .pointer
        .map(|(x, y)| ((x - cx) / (box_w * 0.8 + 120.0), (y - cy) / (box_h * 0.8 + 120.0)))
        .unwrap_or((0.0, 0.0));
    let (px_n, py_n) = ((px_n2 * 2.0).clamp(-1.0, 1.0), (py_n2 * 2.0).clamp(-1.0, 1.0));
    let depth = depth_weight(d);
    let ox = px_n * PARALLAX_POINTER * depth + (-scene.velocity) * PARALLAX_SCROLL * depth * CARD_WIDTH;
    let oy = py_n * (PARALLAX_POINTER * 0.5) * depth;
    let mut dx = ox;
    let mut dy = oy;
    let slack_x = (scaled_w - box_w).max(0.0) / 2.0;
    let slack_y = (scaled_h - box_h).max(0.0) / 2.0;
    dx = dx.clamp(-slack_x, slack_x);
    dy = dy.clamp(-slack_y, slack_y);
    cr.translate(cx - scaled_w / 2.0 + dx, cy - scaled_h / 2.0 + dy);
    cr.scale(fit, fit);
    let _ = cr.set_source_surface(surface, 0.0, 0.0);
    let _ = cr.paint_with_alpha(alpha);
    let _ = cr.restore();
}

/// The card outline: a rectangle sheared along x, the plugin's
/// `Matrix4x4(1, s, 0, 0, …)` — i.e. `x' = x + s·y`.
fn parallelogram(cr: &cairo::Context, cx: f64, cy: f64, hw: f64, hh: f64, s: f64) {
    cr.new_sub_path();
    cr.move_to(cx - hw + s * -hh, cy - hh);
    cr.line_to(cx + hw + s * -hh, cy - hh);
    cr.line_to(cx + hw + s * hh, cy + hh);
    cr.line_to(cx - hw + s * hh, cy + hh);
    cr.close_path();
}

// --- Thumbnail worker -----------------------------------------------------

/// A decoded card image safe to move across threads: tightly packed BGRA,
/// cairo's premultiplied `ARgb32` byte order.
struct Card {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

/// Decode `path` down to the card's aspect ratio.
///
/// The image is first fitted into [`DECODE_BOX`] (gdk-pixbuf's scaled decode
/// is cheap even for huge wallpapers), then centre-cropped so painting needs
/// no resampling decisions of its own.
fn decode_card(path: &Path) -> Option<Card> {
    let pixbuf =
        gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, DECODE_BOX.0, DECODE_BOX.1, true).ok()?;

    let (crop_w, crop_h) = card_crop(pixbuf.width(), pixbuf.height());
    let x0 = (pixbuf.width() - crop_w) / 2;
    let y0 = (pixbuf.height() - crop_h) / 2;
    let channels = pixbuf.n_channels() as usize;
    let stride = pixbuf.rowstride() as usize;
    let pixels = pixbuf.read_pixel_bytes();
    let data: &[u8] = pixels.as_ref();

    let mut bgra = Vec::with_capacity(crop_w as usize * crop_h as usize * 4);
    for y in 0..crop_h as usize {
        let row = &data[(y0 as usize + y) * stride + x0 as usize * channels..];
        match channels {
            3 => {
                for pixel in row.as_chunks::<3>().0.iter().take(crop_w as usize) {
                    bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
                }
            }
            4 => {
                for pixel in row.as_chunks::<4>().0.iter().take(crop_w as usize) {
                    // Straight alpha → cairo's premultiplied ARgb32.
                    let a = u16::from(pixel[3]);
                    bgra.extend_from_slice(&[
                        (u16::from(pixel[2]) * a / 255) as u8,
                        (u16::from(pixel[1]) * a / 255) as u8,
                        (u16::from(pixel[0]) * a / 255) as u8,
                        pixel[3],
                    ]);
                }
            }
            _ => return None,
        }
    }

    Some(Card {
        width: crop_w,
        height: crop_h,
        bgra,
    })
}

/// The largest centred rectangle with the card's aspect ratio that fits inside
/// `width x height`.
fn card_crop(width: i32, height: i32) -> (i32, i32) {
    let aspect = CARD_WIDTH / CARD_HEIGHT;
    if width as f64 / height as f64 > aspect {
        let w = ((height as f64) * aspect).round() as i32;
        (w.clamp(1, width), height)
    } else {
        let h = ((width as f64) / aspect).round() as i32;
        (width, h.clamp(1, height))
    }
}

/// Decode every wallpaper on a background thread, streaming cards back.
///
/// The thread exits when the component goes away: `send` fails once the
/// receiver is dropped, which is checked after each image.
fn spawn_loader(paths: Vec<PathBuf>, sender: ComponentSender<CarouselModel>) {
    let input = sender.input_sender().clone();
    std::thread::spawn(move || {
        for (index, path) in paths.iter().enumerate() {
            let Some(card) = decode_card(path) else {
                continue;
            };
            let message = CarouselInput::Thumb {
                index,
                width: card.width,
                height: card.height,
                bgra: card.bgra,
            };
            if input.send(message).is_err() {
                return;
            }
        }
    });
}
