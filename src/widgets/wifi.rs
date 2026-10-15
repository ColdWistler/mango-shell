//! Wi-Fi widget, backed by NetworkManager (`nmcli`).
//!
//! The bar shows whether the radio is on, the active SSID and the signal
//! strength. Clicking it opens a popover with an on/off switch, a refresh
//! button and the list of visible networks; picking one connects to it,
//! asking for a password first when the network is secured. `nmcli` can take
//! tens of milliseconds (and a connect can take seconds), so it is driven from
//! a background thread.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::prelude::*;
use relm4::prelude::*;

/// How often the state is re-read, in milliseconds.
const POLL_MS: u64 = 3000;

/// One visible network.
#[derive(Debug, Clone, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: u8,
    pub secured: bool,
    pub active: bool,
}

/// A snapshot of the Wi-Fi radio.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WifiState {
    pub enabled: bool,
    pub ssid: Option<String>,
    pub signal: u8,
    pub networks: Vec<WifiNetwork>,
}

/// Work handed to the polling thread.
enum Command {
    SetEnabled(bool),
    Rescan,
    Connect {
        ssid: String,
        /// `None` (or empty) lets NetworkManager reuse any saved credentials.
        password: Option<String>,
    },
}

pub struct WifiModel {
    state: WifiState,
    commands: Option<Sender<Command>>,
    radio: gtk::Switch,
    status: gtk::Label,
    list_scroll: gtk::ScrolledWindow,
    list_box: gtk::Box,
    /// The inline password prompt, shown instead of the list while it is open.
    prompt: gtk::Box,
    prompt_title: gtk::Label,
    prompt_entry: gtk::PasswordEntry,
    /// The SSID whose password is currently being asked for.
    prompt_ssid: Option<String>,
    /// The list the rows were built from, so polls that change nothing do not
    /// rebuild (and flicker) the whole list.
    rendered: Vec<WifiNetwork>,
    /// True while the radio switch is set from the model.
    syncing: Rc<Cell<bool>>,
}

#[derive(Debug)]
pub enum WifiInput {
    Update(WifiState),
    SetEnabled(bool),
    /// A network row was clicked.
    Pick(WifiNetwork),
    Rescan,
    /// The password prompt's Connect button (or Enter) was hit.
    SubmitPassword,
    CancelPassword,
    /// A transient message to show under the header.
    Status(Option<String>),
}

#[relm4::component(pub)]
impl SimpleComponent for WifiModel {
    type Init = ();
    type Input = WifiInput;
    type Output = ();

    view! {
        #[root]
        gtk::MenuButton {
            set_css_classes: &["mango-widget"],
            set_always_show_arrow: false,
            #[watch]
            set_tooltip_text: Some(&model.tooltip()),

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 5,

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
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The popover is built imperatively: the network list changes shape.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("mango-applet");

        // Header: title + refresh + radio switch.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(Some("Wi-Fi"));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.add_css_class("mango-applet-title");

        let refresh = gtk::Button::new();
        refresh.add_css_class("mango-applet-icon-button");
        refresh.set_tooltip_text(Some("Scan again"));
        refresh.set_child(Some(&gtk::Image::from_icon_name("view-refresh-symbolic")));

        let radio = gtk::Switch::new();
        radio.add_css_class("mango-switch");
        radio.set_valign(gtk::Align::Center);

        header.append(&title);
        header.append(&refresh);
        header.append(&radio);
        content.append(&header);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_visible(false);
        status.add_css_class("mango-applet-status");
        content.append(&status);

        // Inline password prompt (hidden until a secured network is picked).
        let prompt = gtk::Box::new(gtk::Orientation::Vertical, 6);
        prompt.set_visible(false);

        let prompt_title = gtk::Label::new(None);
        prompt_title.set_xalign(0.0);
        prompt_title.set_wrap(true);
        prompt_title.add_css_class("mango-applet-title");

        let prompt_entry = gtk::PasswordEntry::new();
        prompt_entry.set_placeholder_text(Some("Password"));
        prompt_entry.set_show_peek_icon(true);

        let prompt_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        prompt_buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("mango-applet-button");
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("mango-applet-button");
        connect.add_css_class("suggested");
        prompt_buttons.append(&cancel);
        prompt_buttons.append(&connect);

        prompt.append(&prompt_title);
        prompt.append(&prompt_entry);
        prompt.append(&prompt_buttons);
        content.append(&prompt);

        // The scrollable network list.
        let list_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list_box.add_css_class("mango-applet-list");

        let list_scroll = gtk::ScrolledWindow::new();
        list_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        list_scroll.set_propagate_natural_height(true);
        list_scroll.set_max_content_height(320);
        list_scroll.set_has_frame(false);
        list_scroll.set_child(Some(&list_box));
        content.append(&list_scroll);

        popover.set_child(Some(&content));

        let syncing = Rc::new(Cell::new(false));
        let mut model = WifiModel {
            state: WifiState {
                // Neutral until the first `nmcli` result lands, so the bar
                // never flashes "off" on startup.
                enabled: true,
                ssid: None,
                signal: 0,
                networks: Vec::new(),
            },
            commands: None,
            radio: radio.clone(),
            status: status.clone(),
            list_scroll: list_scroll.clone(),
            list_box: list_box.clone(),
            prompt: prompt.clone(),
            prompt_title: prompt_title.clone(),
            prompt_entry: prompt_entry.clone(),
            prompt_ssid: None,
            rendered: Vec::new(),
            syncing: syncing.clone(),
        };

        let widgets = view_output!();
        root.set_popover(Some(&popover));
        crate::anim::pop_in(&popover);

        refresh.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(WifiInput::Rescan)
        });

        radio.connect_active_notify({
            let sender = sender.clone();
            let syncing = syncing.clone();
            move |switch| {
                if syncing.get() {
                    return;
                }
                sender.input(WifiInput::SetEnabled(switch.is_active()));
            }
        });

        connect.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(WifiInput::SubmitPassword)
        });
        prompt_entry.connect_activate({
            let sender = sender.clone();
            move |_| sender.input(WifiInput::SubmitPassword)
        });
        cancel.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(WifiInput::CancelPassword)
        });

        model.commands = Some(spawn_poller(sender.clone()));
        model.sync(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            WifiInput::Update(state) => {
                self.state = state;
                self.sync(&sender);
            }
            WifiInput::SetEnabled(enabled) => {
                self.state.enabled = enabled;
                self.send(Command::SetEnabled(enabled));
                self.set_status(None);
            }
            WifiInput::Rescan => {
                self.send(Command::Rescan);
                self.set_status(Some("Scanning…"));
            }
            WifiInput::Pick(network) => {
                if network.active {
                    return;
                }
                if network.secured {
                    self.prompt_ssid = Some(network.ssid.clone());
                    self.prompt_title
                        .set_text(&format!("Password for “{}”", network.ssid));
                    self.prompt_entry.set_text("");
                    self.set_status(None);
                    self.update_visibility();
                    self.prompt_entry.grab_focus();
                } else {
                    self.send(Command::Connect {
                        ssid: network.ssid.clone(),
                        password: None,
                    });
                    self.set_status(Some(&format!("Connecting to {}…", network.ssid)));
                }
            }
            WifiInput::SubmitPassword => {
                let Some(ssid) = self.prompt_ssid.take() else {
                    return;
                };
                let password = self.prompt_entry.text().to_string();
                let password = (!password.is_empty()).then_some(password);
                self.prompt_entry.set_text("");
                self.send(Command::Connect {
                    ssid: ssid.clone(),
                    password,
                });
                self.set_status(Some(&format!("Connecting to {ssid}…")));
                self.update_visibility();
            }
            WifiInput::CancelPassword => {
                self.prompt_ssid = None;
                self.prompt_entry.set_text("");
                self.update_visibility();
            }
            WifiInput::Status(message) => self.set_status(message.as_deref()),
        }
    }
}

impl WifiModel {
    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    /// Push the whole state into the popover widgets.
    fn sync(&mut self, sender: &ComponentSender<WifiModel>) {
        self.syncing.set(true);
        self.radio.set_active(self.state.enabled);
        self.syncing.set(false);

        if !self.state.enabled {
            self.set_status(Some("Wi-Fi is off"));
        } else if self.status.text() == "Wi-Fi is off" {
            self.set_status(None);
        }

        self.render_networks(sender);
        self.update_visibility();
    }

    /// Rebuild the network list only when it changed.
    fn render_networks(&mut self, sender: &ComponentSender<WifiModel>) {
        if self.state.networks == self.rendered {
            return;
        }
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }
        for network in &self.state.networks {
            self.list_box.append(&build_network_row(network, sender));
        }
        self.rendered = self.state.networks.clone();
    }

    /// Show the prompt or the list, whichever is appropriate.
    fn update_visibility(&self) {
        let prompting = self.prompt_ssid.is_some();
        self.prompt.set_visible(prompting);
        self.list_scroll
            .set_visible(self.state.enabled && !prompting);
    }

    fn set_status(&self, message: Option<&str>) {
        match message {
            Some(message) if !message.is_empty() => {
                self.status.set_text(message);
                self.status.set_visible(true);
            }
            _ => {
                self.status.set_text("");
                self.status.set_visible(false);
            }
        }
    }

    fn icon_name(&self) -> &'static str {
        if !self.state.enabled {
            "network-wireless-offline-symbolic"
        } else if self.state.ssid.is_none() {
            "network-wireless-symbolic"
        } else {
            signal_icon(self.state.signal)
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

/// A signal-strength icon for a 0–100 value.
fn signal_icon(signal: u8) -> &'static str {
    match signal {
        0..=25 => "network-wireless-signal-weak-symbolic",
        26..=50 => "network-wireless-signal-ok-symbolic",
        51..=75 => "network-wireless-signal-good-symbolic",
        _ => "network-wireless-signal-excellent-symbolic",
    }
}

fn build_network_row(network: &WifiNetwork, sender: &ComponentSender<WifiModel>) -> gtk::Button {
    let row = gtk::Button::new();
    row.add_css_class("mango-applet-item");

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.set_valign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name(signal_icon(network.signal));
    icon.add_css_class("mango-applet-item-icon");

    let label = gtk::Label::new(Some(&network.ssid));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    line.append(&icon);
    line.append(&label);

    if network.active {
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("mango-applet-check");
        line.append(&check);
    } else if network.secured {
        let lock = gtk::Image::from_icon_name("channel-secure-symbolic");
        lock.add_css_class("mango-applet-item-lock");
        line.append(&lock);
    }

    row.set_child(Some(&line));

    row.connect_clicked({
        let sender = sender.clone();
        let network = network.clone();
        move |_| sender.input(WifiInput::Pick(network.clone()))
    });

    row
}

/// Drive `nmcli` forever on a background thread.
fn spawn_poller(sender: ComponentSender<WifiModel>) -> Sender<Command> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || loop {
        let command = match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            // The widget went away: stop polling.
            Err(RecvTimeoutError::Disconnected) => return,
        };

        if let Some(command) = command {
            handle_command(&sender, command);
        }
        if let Some(state) = read_state() {
            sender.input(WifiInput::Update(state));
        }
    });

    tx
}

fn handle_command(sender: &ComponentSender<WifiModel>, command: Command) {
    match command {
        Command::SetEnabled(enabled) => {
            let _ = nmcli(&["radio", "wifi", if enabled { "on" } else { "off" }]);
        }
        Command::Rescan => {
            let _ = nmcli(&["dev", "wifi", "rescan"]);
        }
        Command::Connect { ssid, password } => {
            let mut args = vec!["dev", "wifi", "connect", ssid.as_str()];
            if let Some(password) = &password {
                args.push("password");
                args.push(password.as_str());
            }
            match run_nmcli(&args) {
                Ok(_) => sender.input(WifiInput::Status(Some(format!("Connected to {ssid}")))),
                Err(error) => sender.input(WifiInput::Status(Some(error))),
            }
        }
    }
}

fn read_state() -> Option<WifiState> {
    let enabled = nmcli(&["radio", "wifi"]).is_some_and(|out| out.trim() == "enabled");
    let networks = if enabled { read_networks() } else { Vec::new() };
    let active = networks.iter().find(|network| network.active);

    Some(WifiState {
        enabled,
        ssid: active.map(|network| network.ssid.clone()),
        signal: active.map(|network| network.signal).unwrap_or(0),
        networks,
    })
}

/// Every visible network, strongest first with the active one on top.
fn read_networks() -> Vec<WifiNetwork> {
    let Some(output) = nmcli(&["-t", "-f", "IN-USE,SSID,SIGNAL,SECURITY", "dev", "wifi", "list"])
    else {
        return Vec::new();
    };
    parse_networks(&output)
}

/// Parse `nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY dev wifi list` output.
fn parse_networks(output: &str) -> Vec<WifiNetwork> {
    let mut networks: Vec<WifiNetwork> = Vec::new();

    for line in output.lines() {
        let active = line.starts_with('*');

        // Terse output is `<IN-USE>:<SSID>:<SIGNAL>:<SECURITY>`; the SSID may
        // itself contain colons (escaped as `\:`), so split from the right.
        let Some(rest) = line.split_once(':').map(|(_, rest)| rest) else {
            continue;
        };
        let Some((rest, security)) = rest.rsplit_once(':') else {
            continue;
        };
        let Some((ssid, signal)) = rest.rsplit_once(':') else {
            continue;
        };

        let ssid = unescape(ssid);
        if ssid.is_empty() {
            continue;
        }

        let security = security.trim();
        let network = WifiNetwork {
            ssid,
            signal: signal.trim().parse().unwrap_or(0),
            secured: !security.is_empty() && security != "--",
            active,
        };

        // Collapse the duplicate rows NM reports for the same SSID.
        match networks.iter_mut().find(|entry| entry.ssid == network.ssid) {
            Some(existing) => {
                if network.active {
                    *existing = network;
                } else if network.signal > existing.signal {
                    existing.signal = network.signal;
                    existing.secured = network.secured;
                }
            }
            None => networks.push(network),
        }
    }

    networks.sort_by(|a, b| b.active.cmp(&a.active).then(b.signal.cmp(&a.signal)));
    networks
}

/// Undo nmcli's terse-mode escaping (`\:` → `:`, `\\` → `\`).
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn nmcli(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("nmcli").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `nmcli`, reporting the first line of its stderr as the error.
fn run_nmcli(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("nmcli")
        .args(args)
        .output()
        .map_err(|error| format!("Could not run nmcli: {error}"))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Connection failed")
        .trim_start_matches("Error:")
        .trim();
    Err(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_networks() {
        // Active network first, then the rest by descending signal.
        let out = " :sst111_2:97:WPA2\n\
                   *:sst111_5:52:WPA2\n\
                   :OpenCafe:80:\n";
        let networks = parse_networks(out);
        assert_eq!(networks.len(), 3);
        assert_eq!(networks[0].ssid, "sst111_5");
        assert!(networks[0].active);
        assert_eq!(networks[1].ssid, "sst111_2");
        assert_eq!(networks[1].signal, 97);
        assert_eq!(networks[2].ssid, "OpenCafe");
        assert!(!networks[2].secured);
    }

    #[test]
    fn keeps_strongest_duplicate() {
        let out = ":Cafe:10:WPA2\n:Cafe:70:WPA2\n";
        let networks = parse_networks(out);
        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].signal, 70);
    }

    #[test]
    fn drops_hidden_and_empty_rows() {
        let out = " ::35:WPA2\n:Visible:50:--\n";
        let networks = parse_networks(out);
        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].ssid, "Visible");
        assert!(!networks[0].secured);
    }

    #[test]
    fn unescapes_colons_in_ssids() {
        assert_eq!(unescape(r"My\:Net"), "My:Net");
        assert_eq!(unescape(r"Back\\slash"), r"Back\slash");
        let networks = parse_networks(r" :My\:Net:60:WPA2");
        assert_eq!(networks[0].ssid, "My:Net");
    }
}
