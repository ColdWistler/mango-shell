//! Shared plumbing for the shell's "fly-out" applets: the app launcher, the
//! wallpaper picker and the theme gallery.
//!
//! Instead of a full-screen scrim, each applet is an `Overlay`-layer surface
//! that hugs the top of the monitor and hangs just below the shell bar — it
//! overlays the window underneath without ever covering the whole screen. The
//! panel itself lives inside a [`gtk::Revealer`] with a *slide down*
//! transition, so opening an applet visibly morphs it out from under the bar,
//! the way the rofi launchers in the
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
const BANNER_RADIUS: f64 = 15.0;

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

/// A revealer that slides its panel down out of the bar (faster than GTK's
/// default, so it still feels snappy).
pub fn make_revealer() -> gtk::Revealer {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(190);
    revealer.set_halign(gtk::Align::Center);
    revealer.set_valign(gtk::Align::Start);
    revealer
}

/// Hide the surface once its revealer has finished sliding back up. The
/// revealer only reports *fully* hidden children, so the window disappears
/// exactly when the animation ends.
pub fn wire_hide(window: &gtk::Window, revealer: &gtk::Revealer) {
    let window = window.clone();
    revealer.connect_child_revealed_notify(move |revealer| {
        if !revealer.is_child_revealed() {
            window.set_visible(false);
        }
    });
}

/// Present the surface and slide the panel out, focusing `focus` if given.
pub fn show(window: &gtk::Window, revealer: &gtk::Revealer, focus: Option<&gtk::Widget>) {
    window.present();
    revealer.set_reveal_child(true);
    if let Some(widget) = focus {
        widget.grab_focus();
    }
}

/// Start sliding the panel back up; the surface hides itself when done.
pub fn hide(revealer: &gtk::Revealer) {
    revealer.set_reveal_child(false);
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
