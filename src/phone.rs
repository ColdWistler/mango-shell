//! The phone the settings app lives in.
//!
//! Modelled on [SekaiPop](https://github.com/Blapples/SekaiPop)'s landscape
//! Settings phone: a thick rounded slab with a dark bezel, a speaker slit and
//! a camera cut-out, and a screen set into it. SekaiPop draws the real thing
//! in 2.5D (a stack of rounded rectangles pushed through a 4x4 matrix); GTK
//! has no CSS transforms, so this is the *flat* face of that phone — the look
//! SekaiPop itself falls back to (`faceDirect`) — with the depth faked by the
//! body's offset shadow.
//!
//! The phone is a bottom-centred `Overlay`-layer surface, exactly the size of
//! the slab plus room for its shadow, and it arrives the way SekaiPop's does:
//! from below the bottom edge of the screen, overshooting slightly and
//! settling back. That rise is the surface's own top margin, animated frame
//! by frame, so it costs the compositor a handful of integer moves rather
//! than a relayout: the phone slides, nothing redraws.
//!
//! [`Phone::screen`] is the box the app fills; this module owns only the
//! hardware around it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::anim;

/// Room left around the slab inside the surface, for the drop shadow to fall
/// into (the surface itself is invisible).
const SHADOW_PAD: i32 = 30;

/// How far above the bottom edge of the monitor the phone rests.
const BOTTOM_GAP: i32 = 26;

/// The body's design aspect, taken from SekaiPop's landscape Settings phone
/// (face 1440 x 760).
const ASPECT: f64 = 1440.0 / 760.0;

/// Height of the bezel strip above the screen that carries the speaker and
/// the camera. The other three bezels come from the body's CSS padding.
const BEZEL: i32 = 16;

/// Tick of the entrance / exit animation. ~8 ms, like [`crate::anim`].
const TICK_MS: u64 = 8;

/// How long the phone takes to rise (with its settle), and to slide away.
const ENTER_MS: u64 = 430;
const EXIT_MS: u64 = 220;

/// Where the ease-out hands over to the settle, and how far the phone
/// overshoots above its resting spot before it drops back (in pixels).
const SETTLE_START: f64 = 0.74;
const OVERSHOOT_PX: f64 = 13.0;

/// The phone's resting size and where it sits on the monitor, in surface
/// pixels. Pure so the geometry is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Metrics {
    /// Size of the slab itself.
    width: i32,
    height: i32,
    /// Top margin that puts the phone just below the screen bottom.
    offscreen: i32,
    /// Top margin at rest, `BOTTOM_GAP` above the bottom edge.
    rest: i32,
    /// Left margin that centres the surface.
    left: i32,
}

/// The surface and the phone body inside it.
pub struct Phone {
    window: gtk::Window,
    /// The slab with its bezel — the widget that fades in and out.
    body: gtk::Box,
    /// The screen; the app appends its widgets here.
    screen: gtk::Box,
    /// The window's child (centres the body in the surface).
    holder: gtk::Box,
    /// Whether the phone is meant to be up (it stays mapped through the exit
    /// animation, so this is not `window.is_visible()`).
    open: Rc<Cell<bool>>,
    /// Cancels a running animation by bumping this.
    token: Rc<Cell<u64>>,
    /// The surface's current top margin, so an interrupted exit can be
    /// re-shown from wherever it got to.
    top: Rc<Cell<i32>>,
    /// Resting metrics, refreshed every time the phone is shown.
    metrics: Cell<Metrics>,
}

impl Phone {
    /// Turn `window` into the phone's bottom-anchored surface and build the
    /// body around a fresh screen box.
    pub fn new(window: &gtk::Window) -> Self {
        window.init_layer_shell();
        window.set_namespace(Some("mango-phone"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        // -1 tells the compositor to ignore the bar's exclusive zone: the
        // margins below are measured against the whole output, so letting the
        // panel's reserved strip shift the phone down would put it off the
        // bottom edge.
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_visible(false);
        window.add_css_class("mango-phone-window");

        let metrics = measure();
        let width = metrics.width;
        let height = metrics.height;

        // The screen: rounded glass with the app inside.
        let screen = gtk::Box::new(gtk::Orientation::Vertical, 0);
        screen.add_css_class("mango-phone-screen");
        screen.set_hexpand(true);
        screen.set_vexpand(true);

        // The bezel strip above the screen: speaker slit and camera. A
        // vertical box keeps the bezels symmetric without absolute
        // positioning (the body pads the other three sides).
        let hardware = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        hardware.add_css_class("mango-phone-hardware");
        hardware.set_halign(gtk::Align::Center);
        hardware.set_valign(gtk::Align::Center);
        hardware.set_size_request(-1, BEZEL);

        let camera = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        camera.add_css_class("mango-phone-camera");
        camera.set_valign(gtk::Align::Center);
        let speaker = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        speaker.add_css_class("mango-phone-speaker");
        speaker.set_valign(gtk::Align::Center);
        hardware.append(&camera);
        hardware.append(&speaker);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.add_css_class("mango-phone-body");
        body.append(&hardware);
        body.append(&screen);
        body.set_size_request(width, height);

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_halign(gtk::Align::Center);
        body.set_valign(gtk::Align::Center);
        holder.append(&body);

        window.set_default_size(width + 2 * SHADOW_PAD, height + 2 * SHADOW_PAD);
        window.set_resizable(false);

        let phone = Phone {
            window: window.clone(),
            body,
            screen,
            holder,
            open: Rc::new(Cell::new(false)),
            token: Rc::new(Cell::new(0)),
            top: Rc::new(Cell::new(metrics.offscreen)),
            metrics: Cell::new(metrics),
        };
        phone.apply_layout();
        phone.top.set(phone.metrics.get().offscreen);
        phone
    }

    /// The widget to make the window's child.
    pub fn widget(&self) -> gtk::Box {
        self.holder.clone()
    }

    /// The box the app builds its screen content in.
    pub fn screen(&self) -> &gtk::Box {
        &self.screen
    }

    /// Whether the phone is meant to be up (it is, while it slides away too).
    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    /// Rise from below the bottom edge, fading in on the way.
    pub fn show(&self) {
        self.open.set(true);
        self.metrics.set(measure());
        self.apply_layout();

        let metrics = self.metrics.get();
        self.top.set(metrics.offscreen);
        self.window.set_margin(Edge::Top, metrics.offscreen);
        self.body.set_opacity(0.0);
        self.window.present();
        self.animate(true);
    }

    /// Slide back below the bottom edge, hiding the surface when it is gone.
    pub fn hide(&self) {
        if !self.open.get() {
            return;
        }
        self.open.set(false);
        self.animate(false);
    }

    /// Re-centre and resize the surface for the current monitor.
    fn apply_layout(&self) {
        let metrics = self.metrics.get();
        self.body.set_size_request(metrics.width, metrics.height);
        self.window
            .set_default_size(metrics.width + 2 * SHADOW_PAD, metrics.height + 2 * SHADOW_PAD);
        self.window.set_margin(Edge::Left, metrics.left);
    }

    fn animate(&self, enter: bool) {
        let token = self.token.get().wrapping_add(1);
        self.token.set(token);

        let metrics = self.metrics.get();
        let from = if enter {
            metrics.offscreen
        } else {
            self.top.get()
        };
        let to = if enter { metrics.rest } else { metrics.offscreen };
        let duration = if enter { ENTER_MS } else { EXIT_MS };

        let window = self.window.clone();
        let body = self.body.clone();
        let tokens = self.token.clone();
        let top = self.top.clone();
        let start = Instant::now();
        let seconds = duration as f64 / 1000.0;

        glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            if tokens.get() != token {
                return glib::ControlFlow::Break;
            }
            let t = start.elapsed().as_secs_f64() / seconds;
            if t >= 1.0 {
                top.set(to);
                window.set_margin(Edge::Top, to);
                body.set_opacity(1.0);
                if !enter {
                    window.set_visible(false);
                }
                return glib::ControlFlow::Break;
            }

            let y = if enter {
                rise(from as f64, to as f64, t)
            } else {
                from as f64 + (to - from) as f64 * ease_in(t)
            };
            let y = y.round() as i32;
            top.set(y);
            window.set_margin(Edge::Top, y);

            if enter {
                // Fade in quickly: the phone is mostly opaque by the time it
                // has cleared the screen edge.
                body.set_opacity(anim::ease_out((t * 3.0).min(1.0)));
            } else {
                // Stay solid for the first third of the way out, then fade.
                body.set_opacity(1.0 - anim::ease_out(((t - 0.35) / 0.65).max(0.0)));
            }
            glib::ControlFlow::Continue
        });
    }
}

/// The phone's rise: an ease-out most of the way, then a short overshoot
/// above the resting line that settles back onto it. Both the start and the
/// end are exact, so nothing wobbles in the surface's final position.
fn rise(start: f64, rest: f64, t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < SETTLE_START {
        let u = t / SETTLE_START;
        start + (rest - start) * anim::ease_out(u)
    } else {
        let u = (t - SETTLE_START) / (1.0 - SETTLE_START);
        rest - OVERSHOOT_PX * (std::f64::consts::PI * u).sin()
    }
}

/// Ease-in cubic: a slow start that speeds up as the phone leaves.
fn ease_in(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// Work out the slab's size and the margins that centre it and put it at
/// rest just above the monitor's bottom edge.
fn measure() -> Metrics {
    let (screen_width, screen_height) = monitor_size();
    let (width, height) = body_size(screen_width, screen_height);
    let surface_width = width + 2 * SHADOW_PAD;
    let left = ((screen_width - surface_width) / 2).max(0);
    Metrics {
        width,
        height,
        offscreen: screen_height,
        rest: screen_height - BOTTOM_GAP - height - SHADOW_PAD,
        left,
    }
}

/// The size of the slab on a screen of `screen_width` x `screen_height`:
/// a landscape phone a little over half the screen tall, kept inside the
/// monitor's width.
fn body_size(screen_width: i32, screen_height: i32) -> (i32, i32) {
    let height = ((f64::from(screen_height) * 0.56).round() as i32).clamp(440, 700);
    let mut width = (f64::from(height) * ASPECT).round() as i32;

    // Leave room for the shadow pad on both sides before the monitor edge.
    let max_width = (screen_width - 120).max(600);
    if width > max_width {
        width = max_width;
        let height = (f64::from(width) / ASPECT).round() as i32;
        return (width, height);
    }
    (width, height)
}

/// The geometry of the monitor the phone should open on. The shell has one
/// bar per output but the phone is a single surface; like the other overlays
/// it lives on the first monitor the display reports.
fn monitor_size() -> (i32, i32) {
    let display = gdk::Display::default();
    let monitor = display
        .as_ref()
        .and_then(|display| display.monitors().item(0))
        .and_then(|object| object.downcast::<gdk::Monitor>().ok());
    match monitor {
        Some(monitor) => {
            let geometry = monitor.geometry();
            (geometry.width(), geometry.height())
        }
        // A display always has at least one monitor; keep a sane size for the
        // hypothetical case where it does not.
        None => (1920, 1080),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rise_starts_and_ends_exactly() {
        assert_eq!(rise(1080.0, 420.0, 0.0), 1080.0);
        assert_eq!(rise(1080.0, 420.0, 1.0), 420.0);
        // ...and is exactly at rest when the settle begins.
        assert!((rise(1080.0, 420.0, SETTLE_START) - 420.0).abs() < 0.001);
    }

    #[test]
    fn the_rise_overshoots_gently_and_returns() {
        let mut highest = f64::MAX;
        for step in 0..=100 {
            let t = f64::from(step) / 100.0;
            highest = highest.min(rise(1080.0, 420.0, t));
        }
        // The phone's top margin goes at most the overshoot past the rest
        // line, then back down: a bounce, not a launch.
        assert!(highest < 420.0);
        assert!(highest > 420.0 - OVERSHOOT_PX - 0.5);
    }

    #[test]
    fn the_phone_keeps_its_aspect_and_fits_the_monitor() {
        let (width, height) = body_size(1920, 1080);
        let aspect = f64::from(width) / f64::from(height);
        assert!((aspect - ASPECT).abs() < 0.01, "aspect {aspect}");
        assert!(height >= 440 && height <= 700);
        // Room for the shadow pad on both sides.
        assert!(width + 2 * SHADOW_PAD <= 1920);

        // A narrow monitor shrinks the phone rather than cutting it off.
        let (width, height) = body_size(1024, 768);
        assert!(width + 2 * SHADOW_PAD <= 1024);
        let aspect = f64::from(width) / f64::from(height);
        assert!((aspect - ASPECT).abs() < 0.01, "aspect {aspect}");
    }
}
