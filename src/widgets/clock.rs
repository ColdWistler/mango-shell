//! Clock + date widget, with a calendar popover.
//!
//! The bar shows the date and a 12-hour time (`Thu, 15/10 • 9:05 PM`). Clicking
//! it opens a popover with the full date and a [`gtk::Calendar`], so the widget
//! doubles as a quick month view. Like the rest of the strip, the labels are
//! refreshed by a timer aligned to the minute boundary — not every second.

use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::Sender;
use relm4::prelude::*;

pub struct ClockModel {
    time: String,
    date: String,
    /// The full date shown atop the calendar, e.g. `Thursday, 15 October 2026`.
    heading: gtk::Label,
    /// The popover's month view, re-pointed at today when it opens.
    calendar: gtk::Calendar,
    /// Day-of-month at the last tick, so a rollover can follow the month.
    day: i32,
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
        gtk::MenuButton {
            set_css_classes: &["mango-clock-button"],
            set_always_show_arrow: false,

            #[wrap(Some)]
            set_child = &gtk::Box {
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
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let today = glib::DateTime::now_local().ok();

        // The popover holds the full date and a month view. It is built
        // imperatively because we keep a handle on the calendar to re-point it
        // at today, and because that is how the rest of the strip does it.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("mango-applet");

        let heading = gtk::Label::new(Some(&today.as_ref().map(full_date).unwrap_or_default()));
        heading.set_xalign(0.0);
        heading.add_css_class("mango-applet-title");

        let calendar = gtk::Calendar::new();
        calendar.add_css_class("mango-calendar");
        if let Some(today) = &today {
            calendar.select_day(today);
        }

        content.append(&heading);
        content.append(&calendar);
        popover.set_child(Some(&content));
        root.set_popover(Some(&popover));
        crate::anim::pop_in(&popover);

        // Always open on the current month, even if the shell has been up for
        // weeks or was left browsing another month.
        popover.connect_map({
            let calendar = calendar.clone();
            move |_| {
                if let Ok(today) = glib::DateTime::now_local() {
                    calendar.select_day(&today);
                }
            }
        });

        let mut model = ClockModel {
            time: today.as_ref().map(time_label).unwrap_or_default(),
            date: today.as_ref().map(date_label).unwrap_or_default(),
            heading,
            calendar,
            day: today.as_ref().map(|d| d.day_of_month()).unwrap_or(0),
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
                let now = glib::DateTime::now_local().ok();
                self.time = now.as_ref().map(time_label).unwrap_or_default();
                self.date = now.as_ref().map(date_label).unwrap_or_default();
                self.heading
                    .set_label(&now.as_ref().map(full_date).unwrap_or_default());

                // If the day rolled over while the bar was up, follow it so the
                // month view does not sit on yesterday.
                if let Some(now) = &now {
                    let day = now.day_of_month();
                    if day != self.day {
                        self.day = day;
                        self.calendar.select_day(now);
                    }
                }

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

/// The bar's time label: 12-hour with an AM/PM suffix (`9:05 PM`).
fn time_label(dt: &glib::DateTime) -> String {
    format_time(dt.hour(), dt.minute())
}

/// Build a 12-hour clock string from raw components (kept pure for testing).
fn format_time(hour: i32, minute: i32) -> String {
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let hour = match hour % 12 {
        0 => 12,
        h => h,
    };
    format!("{hour}:{minute:02} {suffix}")
}

/// The bar's date label: `Thu, 15/10`.
fn date_label(dt: &glib::DateTime) -> String {
    format(dt, "%a, %d/%m")
}

/// The full date shown above the calendar: `Thursday, 15 October 2026`.
fn full_date(dt: &glib::DateTime) -> String {
    let weekday = format(dt, "%A");
    let month = format(dt, "%B");
    format!("{weekday}, {} {month} {}", dt.day_of_month(), dt.year())
}

/// Format with glib, falling back to an empty string.
fn format(dt: &glib::DateTime, spec: &str) -> String {
    dt.format(spec).map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_is_twelve_am() {
        assert_eq!(format_time(0, 5), "12:05 AM");
    }

    #[test]
    fn morning_has_no_leading_zero() {
        assert_eq!(format_time(9, 5), "9:05 AM");
    }

    #[test]
    fn noon_is_twelve_pm() {
        assert_eq!(format_time(12, 0), "12:00 PM");
    }

    #[test]
    fn afternoon_converts() {
        assert_eq!(format_time(13, 30), "1:30 PM");
        assert_eq!(format_time(23, 59), "11:59 PM");
    }
}
