//! The dashboard's weather card.
//!
//! The one network call the shell makes, and it makes it the cheapest way
//! there is: `wttr.in` answers a single line of pipe-separated fields for a
//! plain URL, so the fetch is a `curl` (or `wget`) subprocess — no HTTP
//! crate, no async runtime, no API key. The request runs on a thread and
//! arrives as [`DashboardInput::Weather`]; the card keeps the last report and
//! only refetches when it goes stale, so reopening the panel does not hammer
//! the service.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use relm4::prelude::*;

use super::{card, DashboardInput, DashboardModel};

/// wttr.in's one-line format: location | condition | temp | feels-like |
/// humidity | wind.
const URL: &str = "https://wttr.in/?format=%l|%c|%t|%f|%h|%w";
/// How long one report stays fresh.
const FRESH_FOR: Duration = Duration::from_secs(10 * 60);
/// Cap the subprocess so a dead network cannot wedge the card on "Loading…".
const TIMEOUT_SECONDS: &str = "8";

/// One parsed wttr.in line. Every field is kept as the service spelled it —
/// units and all — so nothing here needs a unit conversion table.
///
/// `pub` (rather than `pub(super)`) because it rides in
/// [`DashboardInput::Weather`], which is public.
#[derive(Debug, Clone)]
pub struct WeatherReport {
    pub location: String,
    pub condition: String,
    pub temp: String,
    pub feels: String,
    pub humidity: String,
    pub wind: String,
}

/// The dashboard's weather card.
pub(super) struct WeatherCard {
    root: gtk::Box,
    sender: ComponentSender<DashboardModel>,
    place: gtk::Label,
    temp: gtk::Label,
    condition: gtk::Label,
    detail: gtk::Label,
    /// When the card last *asked* (not when it last succeeded): a failed
    /// fetch counts toward the cooldown too, so an offline machine does not
    /// spawn a curl per panel open.
    asked: Rc<Cell<Option<Instant>>>,
}

impl WeatherCard {
    pub(super) fn new(sender: ComponentSender<DashboardModel>) -> Self {
        let (root, body) = card("Weather");

        let place = gtk::Label::new(Some("Fetching…"));
        place.add_css_class("mango-card-note");
        place.set_xalign(0.0);

        let temp = gtk::Label::new(None);
        temp.add_css_class("mango-card-value");

        let condition = gtk::Label::new(None);
        condition.add_css_class("mango-card-label");
        condition.set_xalign(0.0);

        let detail = gtk::Label::new(None);
        detail.add_css_class("mango-card-note");
        detail.set_xalign(0.0);

        body.append(&place);
        body.append(&temp);
        body.append(&condition);
        body.append(&detail);

        WeatherCard {
            root,
            sender,
            place,
            temp,
            condition,
            detail,
            asked: Rc::new(Cell::new(None)),
        }
    }

    pub(super) fn root(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Fetch on open, unless a request went out recently. The cooldown is
    /// stamped at request time so failures cool down as well.
    pub(super) fn refresh_if_stale(&self) {
        if self
            .asked
            .get()
            .is_some_and(|asked| asked.elapsed() < FRESH_FOR)
        {
            return;
        }
        self.asked.set(Some(Instant::now()));

        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let _ = sender.input(DashboardInput::Weather(fetch()));
        });
    }

    pub(super) fn set(&mut self, report: Option<&WeatherReport>) {
        match report {
            Some(report) => {
                self.place
                    .set_text(if report.location.is_empty() {
                        "Weather"
                    } else {
                        &report.location
                    });
                self.temp.set_text(&report.temp);
                self.condition.set_text(&report.condition);

                let mut parts = Vec::new();
                if !report.feels.is_empty() {
                    parts.push(format!("feels {}", report.feels));
                }
                if !report.humidity.is_empty() {
                    parts.push(format!("humidity {}", report.humidity));
                }
                if !report.wind.is_empty() {
                    parts.push(format!("wind {}", report.wind));
                }
                self.detail.set_text(&parts.join(" · "));
            }
            None => {
                self.place.set_text("Offline");
                self.temp.set_text("—");
                self.condition.set_text("Weather unavailable");
                self.detail.set_text("");
            }
        }
    }
}

/// Ask wttr.in for one line and parse it.
///
/// `curl` first, `wget` as the fallback; `None` means no answer we could use
/// (no tool, no network, timeout, or an HTML error page in place of the
/// format).
fn fetch() -> Option<WeatherReport> {
    let output = run("curl", &["-sf", "--max-time", TIMEOUT_SECONDS, URL]).or_else(|| {
        run(
            "wget",
            &["-q", "-O", "-", "--timeout", TIMEOUT_SECONDS, URL],
        )
    })?;

    let fields: Vec<&str> = output.split('|').map(str::trim).collect();
    // wttr.in answers an HTML page when something is wrong on its side; a
    // real report never contains a tag.
    if fields.len() < 6 || fields.iter().any(|field| field.contains('<')) {
        return None;
    }

    Some(WeatherReport {
        location: fields[0].to_string(),
        condition: fields[1].to_string(),
        temp: fields[2].to_string(),
        feels: fields[3].to_string(),
        humidity: fields[4].to_string(),
        wind: fields[5].to_string(),
    })
}

/// Run `program args`, returning its trimmed stdout on success.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
}
