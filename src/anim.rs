//! A few shared tweening helpers for the shell's little transitions.
//!
//! GTK 4 has no CSS `transform` and `GtkRevealer` only offers a fixed set of
//! transitions, so anything past a plain colour fade is driven here with a
//! short timeout that nudges widget properties on its own. Every curve is an
//! ease-out (or a damped settle), so the results read as *snappy* rather than
//! bouncy.
//!
//! These are used by the fly-out panels ([`crate::flyout`]) and the bar
//! popovers (the theme switcher and the system applets).

use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;

/// How often an animation ticks. ~8 ms keeps the short moves smooth without
/// waking the main loop at a full 60 Hz for a 150 ms transition.
const STEP_MS: u64 = 8;

/// Popover open animation: length, and how far the content rises into place.
const POP_MS: u64 = 130;
const POP_OFFSET: f64 = 7.0;

/// Ease-out cubic: quick off the mark, soft on arrival.
pub fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A damped settle in pixels: starts and ends at zero and bulges out to
/// `amplitude` in between. Added to a widget's own top margin it reads as a
/// small spring as the widget lands.
pub fn settle_offset(t: f64, amplitude: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    amplitude * (std::f64::consts::PI * t).sin() * (1.0 - t)
}

/// Fade `widget` in from transparent to fully opaque over `ms`.
pub fn fade_in<W: IsA<gtk::Widget> + Clone>(widget: &W, ms: u64) {
    if ms == 0 {
        widget.clone().upcast::<gtk::Widget>().set_opacity(1.0);
        return;
    }
    let widget = widget.clone().upcast::<gtk::Widget>();
    widget.set_opacity(0.0);
    let start = Instant::now();
    let seconds = ms as f64 / 1000.0;
    glib::timeout_add_local(Duration::from_millis(STEP_MS), move || {
        let t = start.elapsed().as_secs_f64() / seconds;
        if t >= 1.0 {
            widget.set_opacity(1.0);
            return glib::ControlFlow::Break;
        }
        widget.set_opacity(ease_out(t));
        glib::ControlFlow::Continue
    });
}

/// Give `widget` a subtle spring by bulging its top margin out and back. The
/// widget's existing top margin is preserved.
pub fn settle<W: IsA<gtk::Widget> + Clone>(widget: &W, ms: u64, amplitude: f64) {
    if ms == 0 {
        return;
    }
    let widget = widget.clone().upcast::<gtk::Widget>();
    let base = widget.margin_top();
    let start = Instant::now();
    let seconds = ms as f64 / 1000.0;
    glib::timeout_add_local(Duration::from_millis(STEP_MS), move || {
        let t = start.elapsed().as_secs_f64() / seconds;
        if t >= 1.0 {
            widget.set_margin_top(base);
            return glib::ControlFlow::Break;
        }
        widget.set_margin_top(base + settle_offset(t, amplitude).round() as i32);
        glib::ControlFlow::Continue
    });
}

/// Animate a popover open: a quick fade plus a small rise into place. Wired to
/// the popover's `map` signal, so it replays every time it is shown.
pub fn pop_in(popover: &gtk::Popover) {
    popover.connect_map(|popover| {
        fade_in(popover, POP_MS);
        if let Some(child) = popover.child() {
            settle(&child, POP_MS, POP_OFFSET);
        }
    });
}
