//! Battery + power-menu widget.
//!
//! The bar shows the battery level and a matching icon; clicking it opens a
//! popover with the battery status and the session actions — **lock**, **log
//! out**, **suspend**, **reboot** and **shut down**. On a machine with no
//! battery (a desktop) the bar shows a power glyph instead and the popover
//! keeps just the actions, so the widget is still the way to the power menu.
//!
//! The battery is read straight from `/sys/class/power_supply` (no daemon, so
//! nothing extra to install); the actions shell out to the usual tools:
//! `mmsg` for log-out, `systemctl` for suspend/reboot/poweroff, and any
//! `wlr-session-lock-v1` client (`swaylock`, `gtklock`, …) for lock.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::prelude::*;
use relm4::prelude::*;

/// How often the battery is re-read, in milliseconds. Reading a few sysfs
/// files is essentially free, so this is a plain timer (no subprocess).
const POLL_MS: u64 = 15_000;

/// How the battery is charging, mirroring the Linux `status` word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charge {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Full,
    NotCharging,
}

impl Charge {
    fn from_sysfs(status: Option<&str>) -> Self {
        match status {
            Some("Charging") => Self::Charging,
            Some("Discharging") => Self::Discharging,
            Some("Full") => Self::Full,
            Some("Not charging") => Self::NotCharging,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Charging => "Charging",
            Self::Discharging => "Discharging",
            Self::Full => "Fully charged",
            Self::NotCharging => "Not charging",
            Self::Unknown => "Unknown",
        }
    }

    /// True while power is flowing in (or the pack is topped up).
    pub(crate) fn charging(self) -> bool {
        matches!(self, Self::Charging | Self::Full)
    }
}

/// A snapshot of the battery, or the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatteryState {
    /// False when the machine has no battery at all (a desktop).
    pub present: bool,
    /// Charge in `0..=100`.
    pub percent: u32,
    pub charge: Charge,
}

impl BatteryState {
    /// The icon shown on the bar and atop the popover. All names come from the
    /// Adwaita set, so they render with the default icon theme.
    pub(crate) fn icon_name(self) -> &'static str {
        if !self.present {
            return "system-shutdown-symbolic";
        }

        let level = if self.percent >= 90 {
            4
        } else if self.percent >= 60 {
            3
        } else if self.percent >= 35 {
            2
        } else if self.percent >= 15 {
            1
        } else {
            0
        };

        match (self.charge.charging(), level) {
            (true, 4) => "battery-full-charging-symbolic",
            (true, 3) => "battery-good-charging-symbolic",
            (true, 2) => "battery-low-charging-symbolic",
            (true, 1) => "battery-caution-charging-symbolic",
            (true, _) => "battery-empty-charging-symbolic",
            (false, 4) => "battery-full-symbolic",
            (false, 3) => "battery-good-symbolic",
            (false, 2) => "battery-low-symbolic",
            (false, 1) => "battery-caution-symbolic",
            (false, _) => "battery-empty-symbolic",
        }
    }

    pub(crate) fn label(self) -> String {
        format!("{}%", self.percent)
    }

    pub(crate) fn status(self) -> &'static str {
        if self.present {
            self.charge.label()
        } else {
            ""
        }
    }

    fn tooltip(self) -> String {
        if self.present {
            format!("Battery {} · {}", self.label(), self.charge.label())
        } else {
            "Power".to_string()
        }
    }
}

/// One entry in the session menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Lock,
    LogOut,
    Suspend,
    Reboot,
    Shutdown,
}

impl PowerAction {
    /// Every action, in the order the popover and the dashboard show them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Lock,
        Self::LogOut,
        Self::Suspend,
        Self::Reboot,
        Self::Shutdown,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::LogOut => "Log out",
            Self::Suspend => "Suspend",
            Self::Reboot => "Reboot",
            Self::Shutdown => "Shut down",
        }
    }

    /// All from the Adwaita set, so they render with the default icon theme.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Lock => "system-lock-screen-symbolic",
            Self::LogOut => "system-log-out-symbolic",
            Self::Suspend => "weather-clear-night-symbolic",
            Self::Reboot => "system-reboot-symbolic",
            Self::Shutdown => "system-shutdown-symbolic",
        }
    }

    /// Destructive actions are tinted with the error colour.
    pub(crate) fn danger(self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

/// Work handed to the polling thread.
enum Command {
    /// Re-read the battery at once (e.g. the popover was just opened).
    Refresh,
}

pub struct PowerModel {
    state: BatteryState,
    commands: Option<Sender<Command>>,
    /// Popover widgets that mirror `state`.
    battery_box: gtk::Box,
    separator: gtk::Separator,
    header_icon: gtk::Image,
    percent_label: gtk::Label,
    status_label: gtk::Label,
}

#[derive(Debug)]
pub enum PowerInput {
    Update(BatteryState),
    /// Re-read the battery now.
    Refresh,
    /// Run a session action.
    Run(PowerAction),
}

#[relm4::component(pub)]
impl SimpleComponent for PowerModel {
    type Init = ();
    type Input = PowerInput;
    type Output = ();

    view! {
        #[root]
        gtk::MenuButton {
            set_css_classes: &["mango-widget"],
            set_always_show_arrow: false,
            #[watch]
            set_tooltip_text: Some(&model.state.tooltip()),

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 5,

                append = &gtk::Image {
                    set_css_classes: &["mango-widget-icon"],
                    #[watch]
                    set_icon_name: Some(model.state.icon_name()),
                },

                append = &gtk::Label {
                    set_css_classes: &["mango-widget-label"],
                    #[watch]
                    set_visible: model.state.present,
                    #[watch]
                    set_label: &model.state.label(),
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Reading the sysfs battery is cheap, so the first paint is correct
        // instead of flashing a placeholder.
        let state = read_battery();

        // The popover is built imperatively: the action buttons each need a
        // click handler that closes it.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("mango-applet");
        content.add_css_class("mango-power-menu");

        // Battery header: icon, a large percentage and the charging word.
        let battery_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        battery_box.add_css_class("mango-power-battery");
        battery_box.set_valign(gtk::Align::Center);

        let header_icon = gtk::Image::from_icon_name(state.icon_name());
        header_icon.add_css_class("mango-power-battery-icon");
        battery_box.append(&header_icon);

        let percent_label = gtk::Label::new(Some(&state.label()));
        percent_label.add_css_class("mango-power-percent");
        percent_label.set_xalign(0.0);
        battery_box.append(&percent_label);

        let status_label = gtk::Label::new(Some(state.status()));
        status_label.add_css_class("mango-applet-status");
        status_label.set_xalign(0.0);
        status_label.set_hexpand(true);
        battery_box.append(&status_label);

        content.append(&battery_box);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("mango-applet-sep");
        content.append(&separator);

        let title = gtk::Label::new(Some("Power"));
        title.set_xalign(0.0);
        title.add_css_class("mango-applet-title");
        content.append(&title);

        // The five actions, in one even row.
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_homogeneous(true);
        for action in PowerAction::ALL {
            actions.append(&action_button(action, &sender, &popover));
        }
        content.append(&actions);

        popover.set_child(Some(&content));

        let mut model = PowerModel {
            state,
            commands: None,
            battery_box,
            separator,
            header_icon,
            percent_label,
            status_label,
        };

        let widgets = view_output!();
        root.set_popover(Some(&popover));
        crate::anim::pop_in(&popover);

        // Opening the popover refreshes the reading, so the header is never
        // stale by up to a poll interval.
        popover.connect_map({
            let sender = sender.clone();
            move |_| sender.input(PowerInput::Refresh)
        });

        model.commands = Some(spawn_poller(sender));
        model.sync();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            PowerInput::Update(state) => {
                self.state = state;
                self.sync();
            }
            PowerInput::Refresh => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Command::Refresh);
                }
            }
            PowerInput::Run(action) => run_action(action),
        }
    }
}

impl PowerModel {
    /// Push the battery state into the popover widgets.
    fn sync(&self) {
        self.header_icon.set_icon_name(Some(self.state.icon_name()));
        self.percent_label.set_text(&self.state.label());
        self.status_label.set_text(self.state.status());

        // Hide the whole battery block (and its separator) on a desktop.
        self.battery_box.set_visible(self.state.present);
        self.separator.set_visible(self.state.present);
    }
}

/// One action button: an icon over a caption.
fn action_button(
    action: PowerAction,
    sender: &ComponentSender<PowerModel>,
    popover: &gtk::Popover,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-power-action");
    if action.danger() {
        button.add_css_class("danger");
    }
    button.set_tooltip_text(Some(action.label()));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.set_valign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name(action.icon());
    icon.add_css_class("mango-power-action-icon");
    body.append(&icon);

    let label = gtk::Label::new(Some(action.label()));
    label.add_css_class("mango-power-action-label");
    body.append(&label);

    button.set_child(Some(&body));

    button.connect_clicked({
        let sender = sender.clone();
        let popover = popover.clone();
        move |_| {
            sender.input(PowerInput::Run(action));
            popover.popdown();
        }
    });

    button
}

/// Re-read the battery forever on a background thread.
fn spawn_poller(sender: ComponentSender<PowerModel>) -> Sender<Command> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || loop {
        match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
            Ok(Command::Refresh) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        sender.input(PowerInput::Update(read_battery()));
    });

    tx
}

/// Read the first battery found under `/sys/class/power_supply`.
pub(crate) fn read_battery() -> BatteryState {
    let Some(dir) = find_battery() else {
        return BatteryState::default();
    };

    let percent = read_trim(&dir, "capacity")
        .and_then(|value| value.parse::<u32>().ok())
        .or_else(|| percent_from_totals(&dir))
        .unwrap_or(0)
        .min(100);

    let charge = Charge::from_sysfs(read_trim(&dir, "status").as_deref());

    BatteryState {
        present: true,
        percent,
        charge,
    }
}

/// The first `type == Battery` supply, preferring the lowest name so the
/// choice is stable when a machine reports several.
fn find_battery() -> Option<PathBuf> {
    let mut batteries: Vec<PathBuf> = std::fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (read_trim(&path, "type").as_deref() == Some("Battery")).then_some(path)
        })
        .collect();
    batteries.sort();
    batteries.into_iter().next()
}

/// Fallback for batteries without a `capacity` file: `charge_now / charge_full`
/// (or the `energy_*` pair).
fn percent_from_totals(dir: &Path) -> Option<u32> {
    for (now, full) in [("charge_now", "charge_full"), ("energy_now", "energy_full")] {
        let now = read_trim(dir, now)?.parse::<u64>().ok()?;
        let full = read_trim(dir, full)?.parse::<u64>().ok()?;
        if let Some(percent) = now.saturating_mul(100).checked_div(full) {
            return Some(percent as u32);
        }
    }
    None
}

/// Read and trim one sysfs attribute of a power supply.
fn read_trim(dir: &Path, file: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Session lockers we know how to launch, in preference order. Any
/// `wlr-session-lock-v1` client works with Mango.
const LOCKERS: &[&str] = &[
    "swaylock",
    "gtklock",
    "hyprlock",
    "waylock",
    "swaylock-effects",
];

/// Run one session action (lock, log out, suspend, reboot, shut down).
/// `pub(crate)` for the dashboard's power card; each helper spawns and never
/// waits, so calling it from the UI thread is safe.
pub(crate) fn run_action(action: PowerAction) {
    match action {
        PowerAction::Lock => lock(),
        PowerAction::LogOut => log_out(),
        PowerAction::Suspend => {
            spawn("systemctl", &["suspend"]);
        }
        PowerAction::Reboot => {
            spawn("systemctl", &["reboot"]);
        }
        PowerAction::Shutdown => {
            spawn("systemctl", &["poweroff"]);
        }
    }
}

fn lock() {
    for locker in LOCKERS {
        if spawn(locker, &[]) {
            return;
        }
    }
    // No session-lock client installed: ask logind to lock the session. That
    // takes effect once a locker is listening — see the README.
    spawn("loginctl", &["lock-session"]);
}

fn log_out() {
    // Mango's own exit, matching the compositor's `quit` keybind.
    if spawn("mmsg", &["dispatch", "quit"]) {
        return;
    }
    // Fall back to ending the login session directly.
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        spawn("loginctl", &["terminate-session", &id]);
    }
}

/// Launch `program` detached, returning whether it started at all. A missing
/// program is not an error the user needs to see; the caller falls back.
fn spawn(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(percent: u32, charge: Charge) -> BatteryState {
        BatteryState {
            present: true,
            percent,
            charge,
        }
    }

    #[test]
    fn maps_the_linux_status_words() {
        assert_eq!(Charge::from_sysfs(Some("Charging")), Charge::Charging);
        assert_eq!(Charge::from_sysfs(Some("Discharging")), Charge::Discharging);
        assert_eq!(Charge::from_sysfs(Some("Full")), Charge::Full);
        assert_eq!(Charge::from_sysfs(Some("Not charging")), Charge::NotCharging);
        assert_eq!(Charge::from_sysfs(Some("Unplugged")), Charge::Unknown);
        assert_eq!(Charge::from_sysfs(None), Charge::Unknown);
    }

    #[test]
    fn charging_batteries_get_the_bolt() {
        assert_eq!(
            battery(95, Charge::Charging).icon_name(),
            "battery-full-charging-symbolic"
        );
        assert_eq!(
            battery(10, Charge::Charging).icon_name(),
            "battery-empty-charging-symbolic"
        );
    }

    #[test]
    fn discharging_batteries_step_through_the_levels() {
        assert_eq!(
            battery(95, Charge::Discharging).icon_name(),
            "battery-full-symbolic"
        );
        assert_eq!(
            battery(20, Charge::Discharging).icon_name(),
            "battery-caution-symbolic"
        );
        assert_eq!(
            battery(2, Charge::Discharging).icon_name(),
            "battery-empty-symbolic"
        );
    }

    #[test]
    fn no_battery_shows_a_power_glyph() {
        let none = BatteryState::default();
        assert!(!none.present);
        assert_eq!(none.icon_name(), "system-shutdown-symbolic");
        assert_eq!(none.tooltip(), "Power");
        assert_eq!(none.status(), "");
    }

    #[test]
    fn labels_the_percentage_and_status() {
        let state = battery(80, Charge::NotCharging);
        assert_eq!(state.label(), "80%");
        assert_eq!(state.status(), "Not charging");
        assert_eq!(state.tooltip(), "Battery 80% · Not charging");
    }
}
