//! Shared plumbing for the shell's "fly-out" applets: the app launcher, the
//! theme gallery, the panel gallery and the shortcut reference.
//!
//! Instead of a full-screen scrim, each applet is an `Overlay`-layer surface
//! that hugs the top of the monitor and hangs just below the shell bar — it
//! overlays the window underneath without ever covering the whole screen. The
//! panel itself lives inside a [`Flyout`]: a slide revealer with a fade
//! nested inside it and a small spring as it lands, so opening an applet
//! visibly morphs it out from under the bar, the way the rofi launchers in the
//! [HyDE project](https://github.com/HyDE-Project/HyDE) (style 10/CompactPanel)
//! drop their search panel below the top bar.
//!
//! Each applet also gets the HyDE signature header: a rounded snapshot of the
//! current wallpaper pinned to the top of the card.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::cairo;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

/// How far below the bar surface a flyout hangs. The bar reserves ~40px; the
/// extra 6px is the visible gap between the bar and the panel.
pub const BAR_GAP: i32 = 46;

/// Height of the wallpaper banner at the top of every fly-out, in pixels.
const BANNER_HEIGHT: i32 = 92;

/// Corner radius of the banner, matching `.mango-flyout-banner` in the CSS.
/// The shell uses sharp, boxy corners, so the banner is square too.
const BANNER_RADIUS: f64 = 0.0;

/// Turn `window` into a full-width overlay that hugs the top of the monitor,
/// sits just underneath the shell bar and grabs the keyboard while it is
/// visible. The surface itself is transparent: only the panel inside it is
/// visible, so it floats over whatever window is open.
pub fn prepare_layer(window: &gtk::Window, namespace: &str) {
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_margin(Edge::Top, BAR_GAP);
    window.set_visible(false);
}

/// How long the panel takes to slide out of the bar, and to fade in.
const SLIDE_MS: u32 = 190;
const FADE_MS: u32 = 150;

/// The little spring the panel does once it has landed.
const SPRING_MS: u64 = 240;
const SPRING_AMPLITUDE: f64 = 5.0;

/// The animated container every fly-out panel lives in.
///
/// Two nested [`gtk::Revealer`]s: the outer *slides* the panel down out of the
/// bar, the inner *crossfades* it, so opening reads as slide + fade rather
/// than a bare wipe. Once the slide has landed the panel gets a short spring
/// ([`crate::anim::settle`]), and the surface hides itself when the slide back
/// up finishes.
pub struct Flyout {
    window: gtk::Window,
    /// Slides the panel (the outer revealer, dropped into the window).
    slide: gtk::Revealer,
    /// Fades the panel (nested inside `slide`).
    fade: gtk::Revealer,
    /// The panel itself, kept so the landing spring can nudge it.
    panel: Rc<RefCell<Option<gtk::Widget>>>,
}

impl Flyout {
    /// Build the revealer pair for `window` and wire the surface to hide once
    /// the slide back up has finished.
    pub fn new(window: &gtk::Window) -> Self {
        let fade = gtk::Revealer::new();
        fade.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        fade.set_transition_duration(FADE_MS);
        fade.set_halign(gtk::Align::Center);
        fade.set_valign(gtk::Align::Start);

        let slide = gtk::Revealer::new();
        slide.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        slide.set_transition_duration(SLIDE_MS);
        slide.set_halign(gtk::Align::Center);
        slide.set_valign(gtk::Align::Start);
        slide.set_child(Some(&fade));

        // `child_revealed` only reports *fully* revealed children, so it tells
        // us exactly when the slide has settled — or fully backed out.
        let panel: Rc<RefCell<Option<gtk::Widget>>> = Rc::new(RefCell::new(None));
        let window_clone = window.clone();
        let panel_clone = panel.clone();
        slide.connect_child_revealed_notify(move |revealer| {
            if revealer.is_child_revealed() {
                if let Some(panel) = panel_clone.borrow().as_ref() {
                    crate::anim::settle(panel, SPRING_MS, SPRING_AMPLITUDE);
                }
            } else {
                window_clone.set_visible(false);
            }
        });

        Flyout {
            window: window.clone(),
            slide,
            fade,
            panel,
        }
    }

    /// The revealer to drop into the window in the `view!`.
    pub fn widget(&self) -> gtk::Revealer {
        self.slide.clone()
    }

    /// Place the panel inside the revealer pair.
    pub fn set_child<W: IsA<gtk::Widget> + Clone>(&self, child: &W) {
        self.fade.set_child(Some(child));
        *self.panel.borrow_mut() = Some(child.clone().upcast());
    }

    /// Present the surface and slide + fade the panel out, focusing `focus`.
    pub fn show(&self, focus: Option<&gtk::Widget>) {
        self.window.present();
        self.fade.set_reveal_child(true);
        self.slide.set_reveal_child(true);
        if let Some(widget) = focus {
            widget.grab_focus();
        }
    }

    /// Slide + fade the panel back up; the surface hides itself when done.
    pub fn hide(&self) {
        self.fade.set_reveal_child(false);
        self.slide.set_reveal_child(false);
    }
}

/// Whether a click at `(x, y)` (in `window`'s coordinates) landed inside
/// `panel`. The panel is nested inside a revealer, so its own `allocation()`
/// is not relative to the window; `compute_bounds` gives the real on-screen
/// rectangle. Used to tell a backdrop click (close) from a click on the panel.
pub fn click_inside(panel: &impl IsA<gtk::Widget>, window: &gtk::Window, x: f64, y: f64) -> bool {
    let Some(bounds) = panel.compute_bounds(window) else {
        return false;
    };
    let left = f64::from(bounds.x());
    let top = f64::from(bounds.y());
    let right = left + f64::from(bounds.width());
    let bottom = top + f64::from(bounds.height());
    x >= left && x < right && y >= top && y < bottom
}

/// A rounded corner snapshot of the current wallpaper for the top of a panel.
///
/// This is a `GtkDrawingArea` rather than a `GtkPicture` on purpose. A
/// `GtkPicture` reports the *image's* natural size as its own, and a wallpaper
/// is normally taller than the whole screen, so the fly-out grew to that height
/// and pushed the search field and the list off the bottom of the monitor. A
/// drawing area requests a fixed [`BANNER_HEIGHT`] instead, and the wallpaper
/// is painted into it cover-fitted by cairo (see [`draw_cover`]).
pub struct Banner {
    /// The widget to drop into the panel.
    pub widget: gtk::DrawingArea,
    /// The image being painted, shared with the draw function.
    image: Rc<RefCell<Option<Pixbuf>>>,
}

/// A round-cornered wallpaper banner, hidden until [`set_banner`] gives it a
/// file to show.
pub fn make_banner() -> Banner {
    let widget = gtk::DrawingArea::new();
    widget.set_content_height(BANNER_HEIGHT);
    widget.set_hexpand(true);
    widget.add_css_class("mango-flyout-banner");
    widget.set_visible(false);

    let image: Rc<RefCell<Option<Pixbuf>>> = Rc::new(RefCell::new(None));
    let painted = image.clone();
    widget.set_draw_func(move |_, cr, width, height| {
        if let Some(pixbuf) = painted.borrow().as_ref() {
            draw_cover(cr, pixbuf, width, height);
        }
    });

    Banner { widget, image }
}

/// Point the banner at `path`, or hide it when there is nothing to show.
pub fn set_banner(banner: &Banner, path: Option<&Path>) {
    match path.and_then(|path| Pixbuf::from_file(path).ok()) {
        Some(pixbuf) => {
            *banner.image.borrow_mut() = Some(pixbuf);
            banner.widget.set_visible(true);
            banner.widget.queue_draw();
        }
        None => {
            *banner.image.borrow_mut() = None;
            banner.widget.set_visible(false);
        }
    }
}

/// Paint `pixbuf` into `width` x `height`, scaled to cover and centre-cropped,
/// then clipped to the banner's rounded corners.
fn draw_cover(cr: &cairo::Context, pixbuf: &Pixbuf, width: i32, height: i32) {
    let image_width = f64::from(pixbuf.width());
    let image_height = f64::from(pixbuf.height());
    let width = f64::from(width);
    let height = f64::from(height);
    if image_width <= 0.0 || image_height <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }

    let scale = (width / image_width).max(height / image_height);
    let draw_width = image_width * scale;
    let draw_height = image_height * scale;
    let x = (width - draw_width) / 2.0;
    let y = (height - draw_height) / 2.0;

    let _ = cr.save();
    rounded_rect(cr, 0.0, 0.0, width, height, BANNER_RADIUS);
    cr.clip();
    cr.translate(x, y);
    cr.scale(scale, scale);
    cr.set_source_pixbuf(pixbuf, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();
}

/// A rounded rectangle path, used to clip the banner to the card's corners.
fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -pi / 2.0, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        pi / 2.0,
    );
    cr.arc(x + radius, y + height - radius, radius, pi / 2.0, pi);
    cr.arc(x + radius, y + radius, radius, pi, 3.0 * pi / 2.0);
    cr.close_path();
}
