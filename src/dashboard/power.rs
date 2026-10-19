//! The dashboard's power card: the battery at a glance plus the session
//! actions the bar's power popover carries (lock, log out, suspend, reboot,
//! shut down).
//!
//! The battery is re-read on a slow timer rather than on every panel open:
//! charge moves in minutes, and the read walks sysfs, so a 15-second cadence
//! is plenty and keeps the card honest even while the panel is closed. The
//! action buttons call the same [`run_action`] the popover uses and then close
//! the panel, so whatever was pressed (lock screen, power off) is what the
//! user sees next.

use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::widgets::power::{read_battery, run_action, BatteryState, PowerAction};

use super::{card, DashboardInput, DashboardModel};

/// How often the battery is re-read.
const REFRESH_MS: u64 = 15_000;

/// The dashboard's power card.
pub(super) struct PowerCard {
    root: gtk::Box,
    /// The battery block, hidden entirely on a desktop.
    battery_row: gtk::Box,
    meter: gtk::ProgressBar,
    icon: gtk::Image,
    percent: gtk::Label,
    status: gtk::Label,
    /// Shown instead of the battery block when there is no battery.
    note: gtk::Label,
}

impl PowerCard {
    /// Build the card and start its battery timer.
    ///
    /// The timer runs whether or not the panel is open — one sysfs walk every
    /// quarter minute is cheaper than the bookkeeping to pause it, and it
    /// means the card is correct the instant it slides out of the bar.
    pub(super) fn new(sender: ComponentSender<DashboardModel>) -> Self {
        let (root, body) = card("Power");

        // --- Battery: icon, big percentage, status, then the meter.
        let icon = gtk::Image::from_icon_name("battery-good-symbolic");
        icon.set_pixel_size(24);
        icon.set_valign(gtk::Align::Center);

        let percent = gtk::Label::new(None);
        percent.add_css_class("mango-card-value");

        let status = gtk::Label::new(None);
        status.add_css_class("mango-card-note");
        status.set_xalign(0.0);

        let captions = gtk::Box::new(gtk::Orientation::Vertical, 2);
        captions.set_valign(gtk::Align::Center);
        captions.append(&percent);
        captions.append(&status);

        let battery_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        battery_row.append(&icon);
        battery_row.append(&captions);

        let meter = gtk::ProgressBar::new();
        meter.add_css_class("mango-stat-bar");

        let note = gtk::Label::new(Some("No battery — desktop session"));
        note.add_css_class("mango-card-note");
        note.set_xalign(0.0);
        note.set_visible(false);

        body.append(&battery_row);
        body.append(&meter);
        body.append(&note);

        // --- Session actions: one icon button each, wired straight to the
        // compositor helpers. Reboot and Shut down carry the danger class,
        // exactly like the popover's buttons.
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.add_css_class("mango-actions");
        for action in PowerAction::ALL {
            let button = gtk::Button::new();
            let mut classes = vec!["mango-action-button"];
            if action.danger() {
                classes.push("danger");
            }
            button.set_css_classes(&classes);
            button.set_tooltip_text(Some(action.label()));

            let image = gtk::Image::from_icon_name(action.icon());
            image.set_pixel_size(18);
            button.set_child(Some(&image));

            let sender = sender.clone();
            button.connect_clicked(move |_| {
                run_action(action);
                // Close the panel so the effect is what the user sees next.
                let _ = sender.input(DashboardInput::Hide);
            });
            actions.append(&button);
        }
        body.append(&actions);

        {
            let sender = sender.clone();
            glib::timeout_add_local(Duration::from_millis(REFRESH_MS), move || {
                sender.input(DashboardInput::Battery(read_battery()));
                glib::ControlFlow::Continue
            });
        }
        // First read immediately, so the card is filled before the first open.
        sender.input(DashboardInput::Battery(read_battery()));

        PowerCard {
            root,
            battery_row,
            meter,
            icon,
            percent,
            status,
            note,
        }
    }

    pub(super) fn root(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub(super) fn set(&mut self, state: &BatteryState) {
        self.battery_row.set_visible(state.present);
        self.meter.set_visible(state.present);
        self.note.set_visible(!state.present);
        if !state.present {
            return;
        }

        self.icon.set_icon_name(Some(state.icon_name()));
        self.percent.set_text(&state.label());
        self.status.set_text(state.status());
        self.meter
            .set_fraction((f64::from(state.percent) / 100.0).clamp(0.0, 1.0));
    }
}
