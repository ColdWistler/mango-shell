//! Wi-Fi widget, backed by NetworkManager (`nmcli`).
//!
//! Read-only: it shows whether Wi-Fi is enabled, the active SSID and the
//! signal strength. `nmcli` can take tens of milliseconds, so it is polled on
//! a background thread.

use std::time::Duration;

use gtk::prelude::*;
use relm4::prelude::*;

/// How often the state is re-read, in milliseconds.
const POLL_MS: u64 = 3000;

/// A snapshot of the Wi-Fi radio.
#[derive(Debug, Clone, Default)]
pub struct WifiState {
    pub enabled: bool,
    pub ssid: Option<String>,
    pub signal: u8,
}

pub struct WifiModel {
    state: WifiState,
    /// Dropping this closes the polling thread's wait channel. Never read
    /// directly; it exists purely as a keep-alive handle.
    _stop: Option<std::sync::mpsc::Sender<()>>,
}

#[derive(Debug)]
pub enum WifiInput {
    Update(WifiState),
}

#[relm4::component(pub)]
impl SimpleComponent for WifiModel {
    type Init = ();
    type Input = WifiInput;
    type Output = ();

    view! {
        #[root]
        gtk::Box {
            set_css_classes: &["mango-widget", "mango-widget-static"],
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 5,
            #[watch]
            set_tooltip_text: Some(&model.tooltip()),

            append = &gtk::Image {
                set_css_classes: &["mango-widget-icon"],
                #[watch]
                set_icon_name: Some(model.icon_name()),
            },

            append = &gtk::Label {
                set_css_classes: &["mango-widget-label"],
                #[watch]
                set_label: &model.label(),
            },
        }
    }

    fn init(
        _init: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let (stop, wait) = std::sync::mpsc::channel();
        let model = WifiModel {
            // Neutral until the first `nmcli` result lands, so the bar never
            // flashes "off" on startup.
            state: WifiState {
                enabled: true,
                ssid: None,
                signal: 0,
            },
            _stop: Some(stop),
        };

        let widgets = view_output!();

        std::thread::spawn(move || {
            loop {
                if let Some(state) = read_state() {
                    sender.input(WifiInput::Update(state));
                }
                match wait.recv_timeout(Duration::from_millis(POLL_MS)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    // Signalled, or the widget went away: stop polling.
                    _ => return,
                }
            }
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            WifiInput::Update(state) => self.state = state,
        }
    }
}

impl WifiModel {
    fn icon_name(&self) -> &'static str {
        if !self.state.enabled {
            "network-wireless-offline-symbolic"
        } else if self.state.ssid.is_none() {
            "network-wireless-symbolic"
        } else {
            match self.state.signal {
                0..=25 => "network-wireless-signal-weak-symbolic",
                26..=50 => "network-wireless-signal-ok-symbolic",
                51..=75 => "network-wireless-signal-good-symbolic",
                _ => "network-wireless-signal-excellent-symbolic",
            }
        }
    }

    fn label(&self) -> String {
        if !self.state.enabled {
            "off".to_string()
        } else if self.state.ssid.is_some() {
            format!("{}%", self.state.signal)
        } else {
            "—".to_string()
        }
    }

    fn tooltip(&self) -> String {
        if !self.state.enabled {
            return "Wi-Fi disabled".to_string();
        }
        match &self.state.ssid {
            Some(ssid) => format!("{ssid} — {}%", self.state.signal),
            None => "Wi-Fi not connected".to_string(),
        }
    }
}

fn read_state() -> Option<WifiState> {
    let enabled = nmcli(&["radio", "wifi"]).is_some_and(|out| out.trim() == "enabled");
    let (ssid, signal) = if enabled { active_network() } else { (None, 0) };
    Some(WifiState {
        enabled,
        ssid,
        signal,
    })
}

/// Find the SSID and signal of the network NM reports as `IN-USE`.
fn active_network() -> (Option<String>, u8) {
    let Some(output) = nmcli(&["-t", "-f", "IN-USE,SSID,SIGNAL", "dev", "wifi"]) else {
        return (None, 0);
    };

    for line in output.lines() {
        if !line.starts_with('*') {
            continue;
        }
        // `*:SSID:89` — split the signal off the end because SSIDs may
        // themselves contain colons.
        let rest = line.trim_start_matches('*').trim_start_matches(':');
        let Some((ssid, signal)) = rest.rsplit_once(':') else {
            continue;
        };
        let signal = signal.trim().parse().unwrap_or(0);
        let ssid = ssid.replace("\\:", ":");
        if !ssid.is_empty() {
            return (Some(ssid), signal);
        }
    }

    (None, 0)
}

fn nmcli(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("nmcli")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
