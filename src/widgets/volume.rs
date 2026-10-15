//! Volume widget, backed by PulseAudio/PipeWire (`pactl`).
//!
//! The bar shows the default sink's volume and a matching icon. Clicking it
//! opens a popover with a volume slider, a mute button and the list of output
//! devices (sinks), so the default output can be switched without leaving the
//! bar. Scrolling the bar widget still nudges the volume a few percent.
//!
//! `pactl` is polled on a background thread so a slow (or missing) sound
//! server never blocks the GTK main loop.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

/// The PipeWire/PulseAudio default sink.
const DEFAULT_SINK: &str = "@DEFAULT_SINK@";
/// How often the sink is re-read, in milliseconds.
const POLL_MS: u64 = 1500;
/// Percent added or removed per scroll notch.
const STEP: i32 = 5;

/// One selectable output device.
#[derive(Debug, Clone, PartialEq)]
pub struct SinkInfo {
    pub name: String,
    pub description: String,
}

/// A snapshot of the default sink and the devices we can switch to.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeState {
    /// Linear volume in `0.0..=1.0`.
    pub level: f64,
    pub muted: bool,
    /// The default sink's name (empty until the first read lands).
    pub sink: String,
    pub sinks: Vec<SinkInfo>,
}

impl Default for VolumeState {
    fn default() -> Self {
        Self {
            // Neutral until the first read lands, so the bar never flashes a
            // muted icon on startup.
            level: 1.0,
            muted: false,
            sink: String::new(),
            sinks: Vec::new(),
        }
    }
}

/// Work handed to the polling thread.
enum Command {
    /// Absolute level in `0.0..=1.0`.
    SetVolume(f64),
    /// Relative step, in percent.
    Adjust(i32),
    ToggleMute,
    SetDefaultSink(String),
}

pub struct VolumeModel {
    state: VolumeState,
    commands: Option<Sender<Command>>,
    /// The popover, kept so a device row can close it after switching.
    popover: gtk::Popover,
    slider: gtk::Scale,
    mute_button: gtk::Button,
    mute_icon: gtk::Image,
    percent: gtk::Label,
    sinks_box: gtk::Box,
    /// Check icons of the device rows, keyed by sink name.
    sink_rows: Vec<(String, gtk::Image)>,
    /// The list the rows were built from, so polls that change nothing do not
    /// rebuild (and reset the highlight of) every row.
    rendered_sinks: Vec<SinkInfo>,
    /// True while the slider is being set from the model, so its signal is
    /// ignored: otherwise every poll would echo a command straight back.
    syncing: Rc<Cell<bool>>,
}

#[derive(Debug)]
pub enum VolumeInput {
    Update(VolumeState),
    SetVolume(f64),
    ToggleMute,
    Adjust(i32),
    SetDefaultSink(String),
}

#[relm4::component(pub)]
impl SimpleComponent for VolumeModel {
    type Init = ();
    type Input = VolumeInput;
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
        // The popover is built imperatively: its contents (the device list)
        // change shape at runtime, which the declarative view cannot express.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("mango-applet");

        // Row 1: mute + slider + percentage.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_valign(gtk::Align::Center);

        let mute_button = gtk::Button::new();
        mute_button.add_css_class("mango-applet-icon-button");
        let mute_icon = gtk::Image::from_icon_name("audio-volume-high-symbolic");
        mute_button.set_child(Some(&mute_icon));

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(190, -1);
        slider.add_css_class("mango-slider");

        let percent = gtk::Label::new(Some("—"));
        percent.add_css_class("mango-applet-value");
        percent.set_width_chars(4);
        percent.set_xalign(1.0);

        header.append(&mute_button);
        header.append(&slider);
        header.append(&percent);
        content.append(&header);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("mango-applet-sep");
        content.append(&separator);

        let title = gtk::Label::new(Some("Output device"));
        title.set_xalign(0.0);
        title.add_css_class("mango-applet-title");
        content.append(&title);

        let sinks_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.append(&sinks_box);

        popover.set_child(Some(&content));

        let syncing = Rc::new(Cell::new(false));
        let mut model = VolumeModel {
            // Reading `pactl` once is cheap and gives a correct first paint.
            state: read_state().unwrap_or_default(),
            commands: None,
            popover: popover.clone(),
            slider: slider.clone(),
            mute_button: mute_button.clone(),
            mute_icon: mute_icon.clone(),
            percent: percent.clone(),
            sinks_box: sinks_box.clone(),
            sink_rows: Vec::new(),
            rendered_sinks: Vec::new(),
            syncing: syncing.clone(),
        };

        let widgets = view_output!();
        root.set_popover(Some(&popover));
        crate::anim::pop_in(&popover);

        // Clicking the mute icon toggles mute.
        mute_button.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(VolumeInput::ToggleMute)
        });

        // Dragging the slider sets an absolute volume.
        slider.connect_value_changed({
            let sender = sender.clone();
            let syncing = syncing.clone();
            move |slider| {
                if syncing.get() {
                    return;
                }
                sender.input(VolumeInput::SetVolume(slider.value() / 100.0));
            }
        });

        // Scrolling the bar widget still steps the volume.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let sender = sender.clone();
            move |_, _dx, dy| {
                if dy < 0.0 {
                    sender.input(VolumeInput::Adjust(STEP));
                } else if dy > 0.0 {
                    sender.input(VolumeInput::Adjust(-STEP));
                }
                glib::Propagation::Stop
            }
        });
        root.add_controller(scroll);

        model.commands = Some(spawn_poller(sender.clone()));
        model.sync(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            VolumeInput::Update(state) => {
                self.state = state;
                self.sync(&sender);
            }
            VolumeInput::SetVolume(level) => {
                // Reflect the drag at once, then let the poller confirm it.
                self.state.level = level;
                self.set_slider(level);
                self.percent.set_text(&format!("{:.0}%", level * 100.0));
                self.send(Command::SetVolume(level));
            }
            VolumeInput::ToggleMute => {
                self.state.muted = !self.state.muted;
                self.sync_mute();
                self.send(Command::ToggleMute);
            }
            VolumeInput::Adjust(delta) => self.send(Command::Adjust(delta)),
            VolumeInput::SetDefaultSink(name) => {
                self.state.sink = name.clone();
                self.sync_checks();
                self.send(Command::SetDefaultSink(name));
            }
        }
    }
}

impl VolumeModel {
    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    /// Push the whole state into the popover widgets.
    fn sync(&mut self, sender: &ComponentSender<VolumeModel>) {
        self.set_slider(self.state.level);
        self.percent.set_text(&format!("{:.0}%", self.state.level * 100.0));
        self.sync_mute();
        self.render_sinks(sender);
    }

    fn set_slider(&self, level: f64) {
        self.syncing.set(true);
        self.slider.set_value((level * 100.0).clamp(0.0, 100.0));
        self.syncing.set(false);
    }

    fn sync_mute(&self) {
        let (icon, tip) = if self.state.muted {
            ("audio-volume-muted-symbolic", "Unmute")
        } else {
            ("audio-volume-high-symbolic", "Mute")
        };
        self.mute_icon.set_icon_name(Some(icon));
        self.mute_button.set_tooltip_text(Some(tip));
    }

    /// Rebuild the device list when it changed; otherwise just move the
    /// checkmark onto the new default sink.
    fn render_sinks(&mut self, sender: &ComponentSender<VolumeModel>) {
        if self.state.sinks != self.rendered_sinks {
            while let Some(child) = self.sinks_box.first_child() {
                self.sinks_box.remove(&child);
            }
            self.sink_rows.clear();

            for sink in &self.state.sinks {
                let (row, check) = build_sink_row(sink, sender, &self.popover);
                self.sinks_box.append(&row);
                self.sink_rows.push((sink.name.clone(), check));
            }
            self.rendered_sinks = self.state.sinks.clone();
        }
        self.sync_checks();
    }

    fn sync_checks(&self) {
        for (name, check) in &self.sink_rows {
            check.set_visible(*name == self.state.sink);
        }
    }

    fn icon_name(&self) -> &'static str {
        if self.state.muted || self.state.level <= 0.001 {
            "audio-volume-muted-symbolic"
        } else if self.state.level < 0.34 {
            "audio-volume-low-symbolic"
        } else if self.state.level < 0.67 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        }
    }

    fn label(&self) -> String {
        format!("{:.0}%", self.state.level * 100.0)
    }

    fn tooltip(&self) -> String {
        if self.state.muted {
            "Muted".to_string()
        } else {
            format!("Volume {:.0}%", self.state.level * 100.0)
        }
    }
}

/// One selectable output-device row: icon, description and a checkmark when it
/// is the current default.
fn build_sink_row(
    sink: &SinkInfo,
    sender: &ComponentSender<VolumeModel>,
    popover: &gtk::Popover,
) -> (gtk::Button, gtk::Image) {
    let row = gtk::Button::new();
    row.add_css_class("mango-applet-item");
    row.set_tooltip_text(Some(&sink.name));

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.set_valign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name("audio-card-symbolic");
    icon.add_css_class("mango-applet-item-icon");

    let label = gtk::Label::new(Some(&sink.description));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let check = gtk::Image::from_icon_name("object-select-symbolic");
    check.add_css_class("mango-applet-check");
    check.set_visible(false);

    line.append(&icon);
    line.append(&label);
    line.append(&check);
    row.set_child(Some(&line));

    row.connect_clicked({
        let sender = sender.clone();
        let popover = popover.clone();
        let name = sink.name.clone();
        move |_| {
            sender.input(VolumeInput::SetDefaultSink(name.clone()));
            popover.popdown();
        }
    });

    (row, check)
}

/// Poll `pactl` forever on a background thread.
fn spawn_poller(sender: ComponentSender<VolumeModel>) -> Sender<Command> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        loop {
            let command = match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };

            match command {
                Some(Command::SetVolume(level)) => {
                    let pct = (level * 100.0).round().clamp(0.0, 150.0) as i32;
                    run_pactl(&["set-sink-volume", DEFAULT_SINK, &format!("{pct}%")]);
                }
                Some(Command::Adjust(delta)) => {
                    let step = if delta >= 0 {
                        format!("+{delta}%")
                    } else {
                        format!("{delta}%")
                    };
                    run_pactl(&["set-sink-volume", DEFAULT_SINK, &step]);
                }
                Some(Command::ToggleMute) => {
                    run_pactl(&["set-sink-mute", DEFAULT_SINK, "toggle"]);
                }
                Some(Command::SetDefaultSink(name)) => {
                    run_pactl(&["set-default-sink", &name]);
                }
                None => {}
            }

            if let Some(state) = read_state() {
                sender.input(VolumeInput::Update(state));
            }
        }
    });

    tx
}

fn run_pactl(args: &[&str]) {
    let _ = std::process::Command::new("pactl").args(args).status();
}

fn pactl(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("pactl").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_state() -> Option<VolumeState> {
    let sink = pactl(&["get-default-sink"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let level = pactl(&["get-sink-volume", DEFAULT_SINK])
        .and_then(|out| parse_volume(&out))
        .unwrap_or(0.0);
    let muted = pactl(&["get-sink-mute", DEFAULT_SINK])
        .is_some_and(|out| out.trim_end().ends_with("yes"));

    Some(VolumeState {
        level,
        muted,
        sink,
        sinks: read_sinks(),
    })
}

/// Parse the percentage out of
/// `Volume: front-left: 42601 / 65% / -11.22 dB, front-right: …`.
fn parse_volume(stdout: &str) -> Option<f64> {
    let rest = stdout.trim().strip_prefix("Volume:")?.trim();
    // The first numeric field after the first `/` is the (shared) percentage.
    let percent = rest.split('/').nth(1)?.trim();
    let percent = percent.strip_suffix('%')?.trim();
    let percent: f64 = percent.parse().ok()?;
    Some(percent / 100.0)
}

/// Every sink `pactl` knows about, as (name, human description) pairs.
fn read_sinks() -> Vec<SinkInfo> {
    pactl(&["list", "sinks"])
        .map(|output| parse_sinks(&output))
        .unwrap_or_default()
}

/// Parse the `Name:` / `Description:` pairs out of `pactl list sinks`.
fn parse_sinks(output: &str) -> Vec<SinkInfo> {
    let mut sinks = Vec::new();
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();

        // A new device block starts at `Sink #<id>`; flush the previous one.
        if trimmed.starts_with("Sink #") {
            if let Some(name) = name.take() {
                sinks.push(SinkInfo {
                    description: description.take().unwrap_or_else(|| name.clone()),
                    name,
                });
            }
            description = None;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("Name:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Description:") {
            description = Some(rest.trim().to_string());
        }
    }

    if let Some(name) = name.take() {
        sinks.push(SinkInfo {
            description: description.unwrap_or_else(|| name.clone()),
            name,
        });
    }

    sinks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stereo_volume() {
        let out = "Volume: front-left: 42601 /  65% / -11.22 dB,   \
                   front-right: 42601 /  65% / -11.22 dB\n        balance 0.00\n";
        assert_eq!(parse_volume(out), Some(0.65));
    }

    #[test]
    fn parses_muted_volume() {
        let out = "Volume: front-left: 65536 / 100% / 0.00 dB [MUTED]";
        assert_eq!(parse_volume(out), Some(1.0));
    }

    #[test]
    fn rejects_unrelated_output() {
        assert_eq!(parse_volume("Failure: No such entity"), None);
    }

    #[test]
    fn parses_sink_list() {
        let out = "\
Sink #54
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tDescription: Built-in Audio Analog Stereo
\tProperties:
\t\tnode.name = \"should-be-ignored\"
\t\tdevice.description = \"also-ignored\"
Sink #60
\tName: bluez_output.AC_12_2F_00_11_22.1
\tDescription: WH-1000XM4
";
        let sinks = parse_sinks(out);
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(sinks[0].description, "Built-in Audio Analog Stereo");
        assert_eq!(sinks[1].description, "WH-1000XM4");
    }

    #[test]
    fn falls_back_to_name_without_description() {
        let sinks = parse_sinks("Sink #1\n\tName: dummy\n");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].description, "dummy");
    }
}
