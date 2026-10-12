//! Volume widget, backed by PipeWire/WirePlumber (`wpctl`).
//!
//! `wpctl` is polled on a background thread so a slow (or missing)
//! WirePlumber never blocks the GTK main loop. Clicking the widget toggles
//! mute, scrolling changes the volume.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

/// The PipeWire default sink.
const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
/// How often the sink is re-read, in milliseconds.
const POLL_MS: u64 = 1500;
/// Percent added or removed per scroll notch.
const STEP: i32 = 5;

/// A snapshot of the default sink.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeState {
    /// Linear volume in `0.0..=1.0`.
    pub level: f64,
    pub muted: bool,
}

impl Default for VolumeState {
    fn default() -> Self {
        Self {
            level: 0.0,
            muted: false,
        }
    }
}

/// Work handed to the polling thread.
enum Command {
    ToggleMute,
    Adjust(i32),
}

pub struct VolumeModel {
    state: VolumeState,
    commands: Option<Sender<Command>>,
}

#[derive(Debug)]
pub enum VolumeInput {
    Update(VolumeState),
    ToggleMute,
    Adjust(i32),
}

#[relm4::component(pub)]
impl SimpleComponent for VolumeModel {
    type Init = ();
    type Input = VolumeInput;
    type Output = ();

    view! {
        #[root]
        gtk::Button {
            set_css_classes: &["mango-widget"],

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
        let mut model = VolumeModel {
            state: VolumeState::default(),
            commands: None,
        };

        let widgets = view_output!();

        // Click = mute toggle, scroll = volume step.
        root.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(VolumeInput::ToggleMute)
        });

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

        model.commands = Some(spawn_poller(sender));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            VolumeInput::Update(state) => self.state = state,
            VolumeInput::ToggleMute => self.send(Command::ToggleMute),
            VolumeInput::Adjust(delta) => self.send(Command::Adjust(delta)),
        }
    }
}

impl VolumeModel {
    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
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
}

/// Poll `wpctl` forever on a background thread.
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
                Some(Command::ToggleMute) => {
                    run_wpctl(&["set-mute", DEFAULT_SINK, "toggle"]);
                }
                Some(Command::Adjust(delta)) => {
                    let step = if delta >= 0 {
                        format!("{delta}%+")
                    } else {
                        format!("{}%-", -delta)
                    };
                    run_wpctl(&["set-volume", DEFAULT_SINK, &step]);
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

fn run_wpctl(args: &[&str]) {
    let _ = std::process::Command::new("wpctl").args(args).status();
}

fn read_state() -> Option<VolumeState> {
    let output = std::process::Command::new("wpctl")
        .args(["get-volume", DEFAULT_SINK])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_volume(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `Volume: 0.65` / `Volume: 0.65 [MUTED]`.
fn parse_volume(stdout: &str) -> Option<VolumeState> {
    let rest = stdout.trim().strip_prefix("Volume:")?.trim_start();
    let muted = rest.contains("[MUTED]");
    let level = rest.split_whitespace().next()?.parse().ok()?;
    Some(VolumeState { level, muted })
}
