//! Screen brightness widget.
//!
//! The level is read straight from `/sys/class/backlight` (no daemon needed)
//! on a background thread. Adjusting it is harder because that file is
//! normally root-owned; scrolling tries, in order:
//!
//! 1. writing the sysfs file (works if a udev rule grants access),
//! 2. `brightnessctl set` (works if `brightnessctl` is installed),
//! 3. `logind`'s `SetBrightness` over D-Bus (works for the active session).

use std::process::Stdio;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

/// How often the backlight is re-read, in milliseconds.
const POLL_MS: u64 = 2000;
/// Fraction added or removed per scroll notch.
const STEP: f64 = 0.05;

/// A snapshot of the primary backlight.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrightnessState {
    /// False when the machine has no controllable backlight.
    pub available: bool,
    /// Linear brightness in `0.0..=1.0`.
    pub level: f64,
}

/// A backlight device discovered under `/sys/class/backlight`.
struct Backlight {
    device: String,
    max: u32,
}

enum Command {
    Adjust(f64),
}

pub struct BrightnessModel {
    state: BrightnessState,
    /// The root, kept so availability can hide the whole widget.
    widget: Option<gtk::Button>,
    commands: Option<Sender<Command>>,
}

#[derive(Debug)]
pub enum BrightnessInput {
    Update(BrightnessState),
    Adjust(f64),
}

#[relm4::component(pub)]
impl SimpleComponent for BrightnessModel {
    type Init = ();
    type Input = BrightnessInput;
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
                    set_icon_name: Some("display-brightness-symbolic"),
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
        // Reading the sysfs backlight is cheap, so the first paint is correct
        // instead of flashing a placeholder.
        let state = read_state().unwrap_or_default();
        root.set_visible(state.available);

        let mut model = BrightnessModel {
            state,
            widget: Some(root.clone()),
            commands: None,
        };

        let widgets = view_output!();

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll({
            let sender = sender.clone();
            move |_, _dx, dy| {
                if dy < 0.0 {
                    sender.input(BrightnessInput::Adjust(STEP));
                } else if dy > 0.0 {
                    sender.input(BrightnessInput::Adjust(-STEP));
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
            BrightnessInput::Update(state) => {
                self.state = state;
                if let Some(widget) = &self.widget {
                    widget.set_visible(state.available);
                }
            }
            BrightnessInput::Adjust(delta) => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Command::Adjust(delta));
                }
            }
        }
    }
}

impl BrightnessModel {
    fn label(&self) -> String {
        format!("{:.0}%", self.state.level * 100.0)
    }
}

/// Poll the backlight forever on a background thread.
fn spawn_poller(sender: ComponentSender<BrightnessModel>) -> Sender<Command> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        // Resolved lazily: only needed the first time we fall back to logind.
        let mut session: Option<String> = None;

        loop {
            let command = match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };

            if let Some(Command::Adjust(delta)) = command
                && let Some(backlight) = find_backlight()
            {
                let current = read_level(&backlight).unwrap_or(0.5);
                apply(&backlight, current + delta, &mut session);
            }

            if let Some(state) = read_state() {
                sender.input(BrightnessInput::Update(state));
            }
        }
    });

    tx
}

fn read_state() -> Option<BrightnessState> {
    let backlight = find_backlight()?;
    let level = read_level(&backlight)?;
    Some(BrightnessState {
        available: true,
        level,
    })
}

/// The device with the highest resolution; `intel_backlight`, `amdgpu_bl0`…
fn find_backlight() -> Option<Backlight> {
    let mut best: Option<Backlight> = None;

    for entry in std::fs::read_dir("/sys/class/backlight").ok()?.flatten() {
        let Ok(max) = std::fs::read_to_string(entry.path().join("max_brightness")) else {
            continue;
        };
        let Ok(max) = max.trim().parse::<u32>() else {
            continue;
        };
        if max == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|backlight| max > backlight.max) {
            best = Some(Backlight {
                device: entry.file_name().to_string_lossy().into_owned(),
                max,
            });
        }
    }

    best
}

fn read_level(backlight: &Backlight) -> Option<f64> {
    let path = format!("/sys/class/backlight/{}/brightness", backlight.device);
    let raw = std::fs::read_to_string(path).ok()?;
    let value = raw.trim().parse::<u32>().ok()?;
    Some(f64::from(value) / f64::from(backlight.max))
}

/// Set an absolute level, trying every backend until one sticks.
fn apply(backlight: &Backlight, level: f64, session: &mut Option<String>) {
    let level = level.clamp(0.01, 1.0);
    let value = ((level * f64::from(backlight.max)).round() as u32).max(1);

    if write_sysfs(backlight, value) {
        return;
    }
    if set_with_brightnessctl(value) {
        return;
    }

    if session.is_none() {
        *session = login_session();
    }
    if let Some(session) = session {
        let _ = set_with_logind(session, &backlight.device, value);
    }
}

fn write_sysfs(backlight: &Backlight, value: u32) -> bool {
    let path = format!("/sys/class/backlight/{}/brightness", backlight.device);
    std::fs::write(path, value.to_string()).is_ok()
}

fn set_with_brightnessctl(value: u32) -> bool {
    std::process::Command::new("brightnessctl")
        .args(["set", &value.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The caller's logind session object path, e.g.
/// `/org/freedesktop/login1/session/_33`.
fn login_session() -> Option<String> {
    let output = std::process::Command::new("busctl")
        .args([
            "--system",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "GetSessionByPID",
            "u",
            &std::process::id().to_string(),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let start = text.find('"')? + 1;
    let end = text.rfind('"')?;
    (end > start).then(|| text[start..end].to_string())
}

fn set_with_logind(session: &str, device: &str, value: u32) -> bool {
    std::process::Command::new("busctl")
        .args([
            "--system",
            "call",
            "org.freedesktop.login1",
            session,
            "org.freedesktop.login1.Session",
            "SetBrightness",
            "ssu",
            "backlight",
            device,
            &value.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
