//! Clock + date widget.

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
            time: now("%I:%M %p"),
            date: now("%a, %d/%m"),
            timer: None,
        };

        let widgets = view_output!();

        model.timer = Some(glib::timeout_add_seconds_local(1, move || {
            sender.input(ClockInput::Tick);
            glib::ControlFlow::Continue
        }));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            ClockInput::Tick => {
                self.time = now("%I:%M %p");
                self.date = now("%a, %d/%m");
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: Sender<Self::Output>) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
    }
}

/// Format the current local time with a glib format string.
fn now(format: &str) -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|dt| dt.format(format).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}
