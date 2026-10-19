//! The dashboard's resource card: CPU, RAM, GPU and the mounted disks.
//!
//! It reuses the bar popover's readers through [`Sampler`] rather than writing
//! a second set of sysfs parsers, but owns its own cadence: the panel wakes it
//! when it opens ([`ResourcesCard::set_active`]), it samples every couple of
//! seconds while on screen, and it goes quiet — no reads at all — once the
//! panel closes, so an idle shell never touches `/proc`.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::prelude::*;
use relm4::prelude::*;

use crate::widgets::resources::{fraction, human, DiskUsage, ResourceSample, Sampler};

use super::{card, DashboardInput, DashboardModel};

/// Cadence while the panel is open. Slow enough to read as a trend, fast
/// enough that a compile spike is visible.
const SAMPLE_MS: u64 = 2_000;
/// Cadence while closed: effectively "never" — the thread just parks. The
/// point is that no sysfs read happens at all, not the exact interval.
const IDLE_MS: u64 = 30_000;
/// How many mounted disks get a row before the card stops counting.
const MAX_DISKS: usize = 3;

/// Messages from the panel to the sampling thread.
enum Poll {
    /// The panel opened or closed.
    SetActive(bool),
}

/// The dashboard's resource card.
pub(super) struct ResourcesCard {
    root: gtk::Box,
    /// Handle to the sampling thread; dropping the card drops it.
    poll: Sender<Poll>,
    cpu: Stat,
    ram: Stat,
    gpu: Stat,
    /// The GPU row itself, hidden when the machine has none.
    gpu_row: gtk::Box,
    /// Container for the disk rows, rebuilt when the mount set changes.
    disk_box: gtk::Box,
    /// The disks currently rendered, so an unchanged poll skips the rebuild.
    rendered_disks: Vec<DiskUsage>,
}

impl ResourcesCard {
    /// Build the card and start its sampler on a background thread.
    ///
    /// The thread samples once immediately, so the card is never blank when
    /// the panel first opens, and once more each time the panel wakes it.
    pub(super) fn new(sender: ComponentSender<DashboardModel>) -> Self {
        let (root, body) = card("Resources");

        let cpu = stat("CPU");
        let ram = stat("RAM");
        let gpu = stat("GPU");
        let gpu_row = gpu.row.clone();
        gpu_row.set_visible(false);

        body.append(&cpu.row);
        body.append(&ram.row);
        body.append(&gpu.row);

        let disk_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.append(&disk_box);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Constructed on this thread: it sleeps briefly to establish a
            // CPU baseline, so the first read is a real percentage.
            let mut sampler = Sampler::new();
            let mut active = false;

            sender.input(DashboardInput::Resources(sampler.next()));

            loop {
                let idle = Duration::from_millis(if active { SAMPLE_MS } else { IDLE_MS });
                match rx.recv_timeout(idle) {
                    Ok(Poll::SetActive(value)) => {
                        let woke = value && !active;
                        active = value;
                        // Waking samples immediately: a fresh reading beats
                        // showing the last one from before the panel closed.
                        if woke {
                            sender.input(DashboardInput::Resources(sampler.next()));
                        }
                    }
                    // While closed the timeout fires over and over and we
                    // deliberately read nothing.
                    Err(RecvTimeoutError::Timeout) => {
                        if active {
                            sender.input(DashboardInput::Resources(sampler.next()));
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });

        ResourcesCard {
            root,
            poll: tx,
            cpu,
            ram,
            gpu,
            gpu_row,
            disk_box,
            rendered_disks: Vec::new(),
        }
    }

    pub(super) fn root(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// The panel tells the card when it opens and closes.
    pub(super) fn set_active(&self, active: bool) {
        let _ = self.poll.send(Poll::SetActive(active));
    }

    /// Render one sample.
    pub(super) fn set(&mut self, sample: &ResourceSample) {
        let load = sample
            .load
            .map(|load| format!("{load:.2}"))
            .unwrap_or_default();
        let freq = sample
            .freq_mhz
            .map(|mhz| format!("{:.1} GHz", f64::from(mhz) / 1000.0))
            .unwrap_or_default();
        self.cpu.set(
            sample.cpu,
            &percent(sample.cpu),
            &join_details(&[freq, load]),
        );

        self.ram.set(
            fraction(sample.mem_used, sample.mem_total),
            &percent(fraction(sample.mem_used, sample.mem_total)),
            &format!("{} / {}", human(sample.mem_used), human(sample.mem_total)),
        );

        match sample.gpu.as_ref() {
            Some(gpu) => {
                self.gpu_row.set_visible(true);
                let vram = format!("{} / {}", human(gpu.mem_used), human(gpu.mem_total));
                let temp = gpu
                    .temp
                    .map(|deg| format!("{deg}°"))
                    .unwrap_or_default();
                self.gpu.set(
                    gpu.util,
                    &percent(gpu.util),
                    &join_details(&[vram, temp]),
                );
            }
            None => self.gpu_row.set_visible(false),
        }

        if self.rendered_disks != sample.disks {
            self.rebuild_disks(&sample.disks);
        }
    }

    /// Disk rows change with the mount set, so they are rebuilt only when it
    /// actually differs (USB stick in, another volume mounted, …).
    fn rebuild_disks(&mut self, disks: &[DiskUsage]) {
        while let Some(child) = self.disk_box.first_child() {
            self.disk_box.remove(&child);
        }

        for disk in disks.iter().take(MAX_DISKS) {
            let stat = stat(&short_mount(&disk.mount));
            let used = fraction(disk.used, disk.total);
            stat.set(
                used,
                &percent(used),
                &format!("{} / {}", human(disk.used), human(disk.total)),
            );
            self.disk_box.append(&stat.row);
        }

        if disks.len() > MAX_DISKS {
            let more = gtk::Label::new(Some(&format!(
                "+{} more mounted filesystem{}",
                disks.len() - MAX_DISKS,
                if disks.len() - MAX_DISKS == 1 { "" } else { "s" },
            )));
            more.add_css_class("mango-card-note");
            more.set_xalign(0.0);
            self.disk_box.append(&more);
        }

        self.rendered_disks = disks.to_vec();
    }
}

/// One labelled meter: name on the left, bar in the middle, percentage and a
/// muted detail line on the right.
struct Stat {
    row: gtk::Box,
    value: gtk::Label,
    detail: gtk::Label,
    bar: gtk::ProgressBar,
}

fn stat(name: &str) -> Stat {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("mango-stat");

    let label = gtk::Label::new(Some(name));
    label.add_css_class("mango-stat-name");
    label.set_xalign(0.0);
    row.append(&label);

    let bar = gtk::ProgressBar::new();
    bar.set_hexpand(true);
    bar.set_valign(gtk::Align::Center);
    bar.add_css_class("mango-stat-bar");
    row.append(&bar);

    let value = gtk::Label::new(None);
    value.add_css_class("mango-stat-value");
    value.set_xalign(1.0);
    row.append(&value);

    let detail = gtk::Label::new(None);
    detail.add_css_class("mango-card-note");
    detail.set_xalign(0.0);
    row.append(&detail);

    Stat {
        row,
        value,
        detail,
        bar,
    }
}

impl Stat {
    fn set(&self, fraction: f64, value: &str, detail: &str) {
        self.bar.set_fraction(fraction.clamp(0.0, 1.0));
        self.value.set_text(value);
        self.detail.set_text(detail);

        // Over 90% is painted in the warning colour, matching the popover.
        let warned = fraction >= 0.90;
        if warned != self.bar.has_css_class("warning") {
            if warned {
                self.bar.add_css_class("warning");
            } else {
                self.bar.remove_css_class("warning");
            }
        }
    }
}

/// `0.123` as `"12%"`.
fn percent(fraction: f64) -> String {
    format!("{}%", (fraction.clamp(0.0, 1.0) * 100.0).round())
}

/// Join the non-empty detail fragments with a separator.
fn join_details(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `/home` stays `/home`, `/` stays `/`, and long paths show their last
/// component rather than a truncated string.
fn short_mount(mount: &str) -> String {
    if mount == "/" || mount.len() <= 8 {
        return mount.to_string();
    }
    mount
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(mount)
        .to_string()
}
