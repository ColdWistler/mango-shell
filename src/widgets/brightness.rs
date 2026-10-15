//! Screen brightness widget.
//!
//! The level is read straight from `/sys/class/backlight` (no daemon needed)
//! on a background thread. Adjusting it is harder because that file is
//! normally root-owned; scrolling the bar widget, or dragging the slider in
//! its popover, tries, in order:
//!
//! 1. writing the sysfs file (works if a udev rule grants access),
//! 2. `brightnessctl set` (works if `brightnessctl` is installed),
//! 3. `logind`'s `SetBrightness` over D-Bus (works for the active session).

use std::cell::Cell;
use std::process::Stdio;
use std::rc::Rc;
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
    /// Relative step in `-1.0..=1.0`.
    Adjust(f64),
    /// Absolute level in `0.0..=1.0`.
    Set(f64),
}

pub struct BrightnessModel {
    state: BrightnessState,
    /// The root, kept so availability can hide the whole widget.
    widget: Option<gtk::MenuButton>,
    commands: Option<Sender<Command>>,
    slider: gtk::Scale,
    percent: gtk::Label,
    /// True while the slider is being set from the model, so its signal is
    /// ignored (otherwise every poll would echo a command straight back).
    syncing: Rc<Cell<bool>>,
}

#[derive(Debug)]
pub enum BrightnessInput {
    Update(BrightnessState),
    Adjust(f64),
    Set(f64),
}

#[relm4::component(pub)]
impl SimpleComponent for BrightnessModel {
    type Init = ();
    type Input = BrightnessInput;
    type Output = ();

    view! {
        #[root]
        gtk::MenuButton {
            set_css_classes: &["mango-widget"],
            set_always_show_arrow: false,
            set_tooltip_text: Some("Brightness"),

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

        // The popover holds a single slider.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("mango-applet");

        let title = gtk::Label::new(Some("Brightness"));
        title.set_xalign(0.0);
        title.add_css_class("mango-applet-title");
        content.append(&title);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_valign(gtk::Align::Center);

        row.append(&gtk::Image::from_icon_name("display-brightness-symbolic"));

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(190, -1);
        slider.add_css_class("mango-slider");
        row.append(&slider);

        let percent = gtk::Label::new(Some("—"));
        percent.add_css_class("mango-applet-value");
        percent.set_width_chars(4);
        percent.set_xalign(1.0);
        row.append(&percent);

        content.append(&row);
        popover.set_child(Some(&content));

        let syncing = Rc::new(Cell::new(false));
        let mut model = BrightnessModel {
            state,
            widget: Some(root.clone()),
            commands: None,
            slider: slider.clone(),
            percent: percent.clone(),
            syncing: syncing.clone(),
        };

        let widgets = view_output!();
        root.set_popover(Some(&popover));

        // Dragging the slider sets an absolute brightness.
        slider.connect_value_changed({
            let sender = sender.clone();
            let syncing = syncing.clone();
            move |slider| {
                if syncing.get() {
                    return;
                }
                sender.input(BrightnessInput::Set(slider.value() / 100.0));
            }
        });

        // Scrolling the bar widget steps the brightness.
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
        model.sync();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            BrightnessInput::Update(state) => {
                self.state = state;
                if let Some(widget) = &self.widget {
                    widget.set_visible(state.available);
                }
                self.sync();
            }
            BrightnessInput::Adjust(delta) => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Command::Adjust(delta));
                }
            }
            BrightnessInput::Set(level) => {
                self.state.level = level;
                self.set_slider(level);
                self.percent.set_text(&format!("{:.0}%", level * 100.0));
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Command::Set(level));
                }
            }
        }
    }
}

impl BrightnessModel {
    fn sync(&self) {
        self.set_slider(self.state.level);
        self.percent.set_text(&format!("{:.0}%", self.state.level * 100.0));
    }

    fn set_slider(&self, level: f64) {
        self.syncing.set(true);
        self.slider.set_value((level * 100.0).clamp(0.0, 100.0));
        self.syncing.set(false);
    }

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

            match command {
                Some(Command::Adjust(delta)) => {
                    if let Some(backlight) = find_backlight() {
                        let current = read_level(&backlight).unwrap_or(0.5);
                        apply(&backlight, current + delta, &mut session);
                    }
                }
                Some(Command::Set(level)) => {
                    if let Some(backlight) = find_backlight() {
                        apply(&backlight, level, &mut session);
                    }
                }
                None => {}
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
