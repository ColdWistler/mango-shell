//! Clock + date widget.

use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::Sender;
use relm4::prelude::*;

pub struct ClockModel {
    time: String,
    date: String,
    /// Kept around so the timer can be cancelled when the widget shuts down.
    timer: Option<glib::SourceId>,
}

#[derive(Debug)]
pub enum ClockInput {
    Tick,
}

#[relm4::component(pub)]
impl SimpleComponent for ClockModel {
    type Init = ();
    type Input = ClockInput;
    type Output = ();

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 6,
            set_css_classes: &["mango-clock"],

            gtk::Label {
                #[watch]
                set_label: &model.date,
                set_css_classes: &["mango-clock-date"],
            },

            gtk::Label {
                set_label: "•",
                set_css_classes: &["mango-clock-sep"],
            },

            gtk::Label {
                #[watch]
                set_label: &model.time,
                set_css_classes: &["mango-clock-time"],
            },
        }
    }

    fn init(
        _init: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = ClockModel {
            time: now("%H:%M"),
            date: now("%a, %d/%m"),
            timer: None,
        };

        let widgets = view_output!();

        // The clock only shows hours:minutes, so wake once a minute — aligned
        // to the minute boundary — instead of every second.
        model.timer = Some(arm(&sender));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            ClockInput::Tick => {
                self.time = now("%H:%M");
                self.date = now("%a, %d/%m");
                // Re-arm for the next minute boundary; the drift corrects
                // itself because the delay is recomputed from the wall clock.
                self.timer = Some(arm(&sender));
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: Sender<Self::Output>) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
    }
}

/// Schedule the next tick at the coming minute boundary, so labels change
/// exactly when the displayed minute rolls over.
fn arm(sender: &ComponentSender<ClockModel>) -> glib::SourceId {
    let delay = glib::DateTime::now_local()
        .ok()
        .map(|dt| (60 - dt.second()).clamp(1, 60) as u64)
        .unwrap_or(60);
    let sender = sender.clone();
    glib::timeout_add_local_once(Duration::from_secs(delay), move || {
        sender.input(ClockInput::Tick);
    })
}

/// Format the current local time with a glib format string.
fn now(format: &str) -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|dt| dt.format(format).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}
