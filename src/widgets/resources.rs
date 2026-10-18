//! System-resources widget: CPU, memory and GPU at a glance.
//!
//! The bar shows three compact readouts — `CPU 12%  RAM 47%  GPU 3%` — and
//! clicking the widget opens a popover with the full picture:
//!
//! * **CPU** — usage, cores, average frequency and the 1-minute load;
//! * **Memory** — a used / cache / free breakdown, so the reclaimable cache
//!   (idle memory the kernel can hand back) is told apart from memory that is
//!   actually held by applications, plus swap;
//! * **GPU** — utilisation, VRAM and temperature (NVIDIA via `nvidia-smi`,
//!   AMD via `sysfs`); the row hides itself when no supported GPU is found;
//! * **Storage** — the busiest mounted filesystems (`df`).
//!
//! CPU, memory and GPU each get a live **sparkline** (the recent history of
//! the polled value) painted with cairo in the widget's CSS colour, and every
//! filesystem gets a small usage bar — so the popover is a real little monitor,
//! not just numbers.
//!
//! Everything is read on one background thread (the CPU percentage needs the
//! delta between two readings), which also keeps a single long-lived
//! `nvidia-smi` process alive so the GPU is sampled without spawning a command
//! every tick. The thread exits — and the GPU child is killed — as soon as the
//! widget goes away, so rebuilding the bar never leaves anything behind.

use std::cell::RefCell;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use gtk::cairo;
use gtk::prelude::*;
use relm4::prelude::*;

/// How often every metric is re-read while the popover is closed. The bar only
/// needs a coarse reading, so this is deliberately unhurried; opening the
/// popover drops to [`FAST_MS`] for live graphs.
const POLL_MS: u64 = 3_000;
/// Faster cadence used while the popover is open.
const FAST_MS: u64 = 1_000;
/// How many samples the sparklines keep (a few minutes of history; the exact
/// span follows the current cadence).
const HISTORY: usize = 60;
/// Storage is re-read every this many ticks: disk usage barely moves.
const DISK_EVERY: u32 = 15;
/// Seconds between `nvidia-smi` samples while it is streaming.
const GPU_SECS: u64 = 2;
/// Ticks to wait before trying to restart a dead `nvidia-smi`.
const GPU_RETRY_TICKS: u32 = 15;
/// Height of a sparkline, in pixels.
const GRAPH_H: i32 = 40;
/// Width of a storage usage bar, in pixels.
const USAGE_W: i32 = 56;
/// Height of the memory breakdown bar, in pixels.
const MEM_BAR_H: i32 = 10;
/// Usage at or above this fraction is painted in the warning colour.
const HIGH: f64 = 0.90;
/// The three memory segments all share the block's colour and are told apart by
/// these alphas: apps (used) solid, reclaimable cache mid, untouched free faint.
const MEM_USED_ALPHA: f64 = 0.95;
const MEM_CACHE_ALPHA: f64 = 0.42;
const MEM_FREE_ALPHA: f64 = 0.14;
/// Filesystems to keep out of the Storage list even if `df` reports them.
const SKIP_MOUNTS: &[&str] = &["/sys", "/proc", "/dev", "/run", "/snap"];

/// A GPU snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuSample {
    /// Human name, e.g. `NVIDIA GeForce RTX 2060`.
    pub name: String,
    /// Utilisation in `0.0..=1.0`.
    pub util: f64,
    /// VRAM in use, in bytes.
    pub mem_used: u64,
    /// Total VRAM, in bytes.
    pub mem_total: u64,
    /// Temperature in degrees Celsius, when reported.
    pub temp: Option<u32>,
}

/// One mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskUsage {
    pub mount: String,
    pub used: u64,
    pub total: u64,
}

impl DiskUsage {
    fn fraction(&self) -> f64 {
        fraction(self.used, self.total)
    }
}

/// Everything the widget renders.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResourceSample {
    /// CPU busy fraction in `0.0..=1.0`.
    pub cpu: f64,
    /// Memory held by applications: `MemTotal - MemAvailable`, the same figure
    /// `free` reports as "used" (it excludes reclaimable cache).
    pub mem_used: u64,
    /// Completely untouched memory (`MemFree`).
    pub mem_free: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub cores: usize,
    /// Average current frequency, in MHz.
    pub freq_mhz: Option<u32>,
    /// 1-minute load average.
    pub load: Option<f64>,
    pub gpu: Option<GpuSample>,
    pub disks: Vec<DiskUsage>,
}

impl ResourceSample {
    /// Fraction of RAM held by applications (excludes reclaimable cache).
    fn mem_fraction(&self) -> f64 {
        fraction(self.mem_used, self.mem_total)
    }

    /// Reclaimable, idle memory: what the kernel could free on demand. This is
    /// `MemAvailable - MemFree`, which is exactly the page cache and reclaimable
    /// slab `free` folds into "buff/cache" — computed from the two figures the
    /// kernel already publishes so it stays correct across kernel versions.
    fn mem_cache(&self) -> u64 {
        self.mem_total
            .saturating_sub(self.mem_used)
            .saturating_sub(self.mem_free)
    }

    /// Fraction of RAM that is reclaimable cache.
    fn mem_cache_fraction(&self) -> f64 {
        fraction(self.mem_cache(), self.mem_total)
    }

    /// Fraction of RAM that is completely free.
    fn mem_free_fraction(&self) -> f64 {
        fraction(self.mem_free, self.mem_total)
    }
}

/// Work handed to the polling thread.
enum Poll {
    /// Re-read everything at once (the popover was just opened).
    Refresh,
}

pub struct ResourcesModel {
    sample: ResourceSample,
    /// The colour a sparkline is painted in comes from the graph's CSS, not the
    /// model; these histories are the data it reads.
    cpu_hist: Rc<RefCell<Vec<f64>>>,
    mem_hist: Rc<RefCell<Vec<f64>>>,
    gpu_hist: Rc<RefCell<Vec<f64>>>,
    commands: Option<Sender<Poll>>,
    /// Popover widgets mirroring `sample`.
    cpu_value: gtk::Label,
    cpu_detail: gtk::Label,
    cpu_graph: gtk::DrawingArea,
    mem_value: gtk::Label,
    mem_detail: gtk::Label,
    mem_graph: gtk::DrawingArea,
    /// The Memory block's used / cache / free breakdown: the bar is shared with
    /// the model through `mem_parts`, and the three labels spell the segments
    /// out in bytes.
    mem_bar: gtk::DrawingArea,
    mem_parts: Rc<RefCell<MemParts>>,
    mem_used_label: gtk::Label,
    mem_cache_label: gtk::Label,
    mem_free_label: gtk::Label,
    gpu_value: gtk::Label,
    gpu_detail: gtk::Label,
    gpu_graph: gtk::DrawingArea,
    /// The whole GPU block (header + graph + detail) plus its separator, hidden
    /// when no supported GPU is present.
    gpu_block: gtk::Box,
    gpu_sep: gtk::Separator,
    /// The Storage section (separator + title + rows), hidden when empty.
    storage_section: gtk::Box,
    storage_box: gtk::Box,
    /// The rows currently rendered, so unchanged polls skip the rebuild.
    rendered_disks: Vec<DiskUsage>,
}

#[derive(Debug)]
pub enum ResourcesInput {
    Update(ResourceSample),
    /// Re-read everything now (the popover was opened).
    Refresh,
}

#[relm4::component(pub)]
impl SimpleComponent for ResourcesModel {
    type Init = ();
    type Input = ResourcesInput;
    type Output = ();

    view! {
        #[root]
        gtk::MenuButton {
            set_css_classes: &["mango-widget", "mango-resources"],
            set_always_show_arrow: false,
            #[watch]
            set_tooltip_text: Some(&model.tooltip()),

            #[wrap(Some)]
            set_child = &gtk::Image {
                set_css_classes: &["mango-widget-icon"],
                set_icon_name: Some("computer-symbolic"),
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The popover is built imperatively: its graphs are cairo drawing areas
        // that share mutable histories with the model.
        let cpu_hist = Rc::new(RefCell::new(Vec::new()));
        let mem_hist = Rc::new(RefCell::new(Vec::new()));
        let gpu_hist = Rc::new(RefCell::new(Vec::new()));

        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-applet-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("mango-applet");
        content.add_css_class("mango-resource-menu");

        let title = gtk::Label::new(Some("System resources"));
        title.set_xalign(0.0);
        title.add_css_class("mango-applet-title");
        content.append(&title);

        let cpu = metric_block("CPU", "computer-symbolic", "cpu", cpu_hist.clone(), None);
        content.append(&cpu.root);

        // Memory gets an extra used / cache / free breakdown under its graph.
        let breakdown = mem_breakdown();
        let mem = metric_block(
            "Memory",
            "media-flash-symbolic",
            "mem",
            mem_hist.clone(),
            Some(&breakdown.root),
        );
        content.append(&mem.root);

        let gpu = metric_block(
            "GPU",
            "video-x-generic-symbolic",
            "gpu",
            gpu_hist.clone(),
            None,
        );
        let gpu_sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        gpu_sep.add_css_class("mango-applet-sep");
        content.append(&gpu_sep);
        content.append(&gpu.root);

        // Storage: a separator, a heading and a box of one row per filesystem.
        let storage_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let storage_sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        storage_sep.add_css_class("mango-applet-sep");
        storage_section.append(&storage_sep);

        let storage_title = gtk::Label::new(Some("Storage"));
        storage_title.set_xalign(0.0);
        storage_title.add_css_class("mango-applet-title");
        storage_section.append(&storage_title);

        let storage_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        storage_section.append(&storage_box);
        content.append(&storage_section);

        popover.set_child(Some(&content));

        let popover_open = Arc::new(AtomicBool::new(false));

        let icon = gtk::Image::from_icon_name("computer-symbolic");
        icon.add_css_class("mango-widget-icon");

        let mut model = ResourcesModel {
            sample: ResourceSample::default(),
            cpu_hist,
            mem_hist,
            gpu_hist,
            commands: None,
            cpu_value: cpu.value,
            cpu_detail: cpu.detail,
            cpu_graph: cpu.graph,
            mem_value: mem.value,
            mem_detail: mem.detail,
            mem_graph: mem.graph,
            mem_bar: breakdown.bar,
            mem_parts: breakdown.parts,
            mem_used_label: breakdown.used,
            mem_cache_label: breakdown.cache,
            mem_free_label: breakdown.free,
            gpu_value: gpu.value,
            gpu_detail: gpu.detail,
            gpu_graph: gpu.graph,
            gpu_block: gpu.root,
            gpu_sep,
            storage_section,
            storage_box,
            rendered_disks: Vec::new(),
        };

        let widgets = view_output!();
        root.set_child(Some(&icon));
        root.set_popover(Some(&popover));
        crate::anim::pop_in(&popover);

        // Opening the popover refreshes the reading so it is never stale by up
        // to a poll interval, and raises the poll cadence; closing it drops back
        // to the idle rate.
        popover.connect_map({
            let sender = sender.clone();
            let popover_open = popover_open.clone();
            move |_| {
                popover_open.store(true, Ordering::Relaxed);
                sender.input(ResourcesInput::Refresh);
            }
        });
        popover.connect_unmap({
            let popover_open = popover_open.clone();
            move |_| popover_open.store(false, Ordering::Relaxed)
        });

        model.commands = Some(spawn_poller(sender, popover_open));
        model.sync();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            ResourcesInput::Update(sample) => {
                self.push_history(&sample);
                self.sample = sample;
                self.sync();
            }
            ResourcesInput::Refresh => {
                if let Some(commands) = &self.commands {
                    let _ = commands.send(Poll::Refresh);
                }
            }
        }
    }
}

impl ResourcesModel {
    /// Push the new sample into the three sparkline histories.
    fn push_history(&mut self, sample: &ResourceSample) {
        push(&self.cpu_hist, sample.cpu);
        push(&self.mem_hist, sample.mem_fraction());
        if let Some(gpu) = &sample.gpu {
            push(&self.gpu_hist, gpu.util);
        }
    }

    /// Push the whole sample into the popover widgets, skipping work for values
    /// that did not move and only repainting graphs that are actually on screen.
    fn sync(&mut self) {
        set_text(&self.cpu_value, &self.percent(self.sample.cpu));
        set_text(&self.cpu_detail, &self.cpu_detail_text());
        redraw(&self.cpu_graph);

        set_text(&self.mem_value, &self.percent(self.sample.mem_fraction()));
        set_text(&self.mem_detail, &self.mem_detail_text());
        redraw(&self.mem_graph);

        // The breakdown bar reads the shared fractions; the legend spells the
        // same three segments out in bytes.
        {
            let mut parts = self.mem_parts.borrow_mut();
            parts.used = self.sample.mem_fraction();
            parts.cache = self.sample.mem_cache_fraction();
            parts.free = self.sample.mem_free_fraction();
        }
        redraw(&self.mem_bar);
        set_text(
            &self.mem_used_label,
            &format!("used {}", human(self.sample.mem_used)),
        );
        set_text(
            &self.mem_cache_label,
            &format!("cache {}", human(self.sample.mem_cache())),
        );
        set_text(
            &self.mem_free_label,
            &format!("free {}", human(self.sample.mem_free)),
        );

        let has_gpu = self.sample.gpu.is_some();
        set_visible(&self.gpu_block, has_gpu);
        set_visible(&self.gpu_sep, has_gpu);
        if has_gpu {
            set_text(&self.gpu_value, &self.gpu_percent());
            set_text(&self.gpu_detail, &self.gpu_detail_text());
        }
        redraw(&self.gpu_graph);

        self.render_storage();
    }

    /// Rebuild the Storage rows when the filesystem list changed.
    fn render_storage(&mut self) {
        if self.sample.disks != self.rendered_disks {
            while let Some(child) = self.storage_box.first_child() {
                self.storage_box.remove(&child);
            }
            for disk in &self.sample.disks {
                self.storage_box.append(&storage_row(disk));
            }
            self.rendered_disks = self.sample.disks.clone();
        }
        self.storage_section
            .set_visible(!self.sample.disks.is_empty());
    }

    fn percent(&self, fraction: f64) -> String {
        format!("{:.0}%", (fraction * 100.0).clamp(0.0, 100.0))
    }

    fn gpu_percent(&self) -> String {
        match &self.sample.gpu {
            Some(gpu) => self.percent(gpu.util),
            None => "—".to_string(),
        }
    }

    fn cpu_detail_text(&self) -> String {
        let mut parts = Vec::new();
        if self.sample.cores > 0 {
            parts.push(format!("{} cores", self.sample.cores));
        }
        if let Some(mhz) = self.sample.freq_mhz {
            parts.push(format!("{:.2} GHz", f64::from(mhz) / 1000.0));
        }
        if let Some(load) = self.sample.load {
            parts.push(format!("load {load:.2}"));
        }
        parts.join(" · ")
    }

    fn mem_detail_text(&self) -> String {
        let mut text = format!(
            "{} / {} used",
            human(self.sample.mem_used),
            human(self.sample.mem_total)
        );
        if self.sample.swap_total > 0 {
            text.push_str(&format!(
                " · swap {} / {}",
                human(self.sample.swap_used),
                human(self.sample.swap_total)
            ));
        }
        text
    }

    fn gpu_detail_text(&self) -> String {
        let Some(gpu) = &self.sample.gpu else {
            return String::new();
        };
        let mut parts = vec![format!(
            "{} / {} VRAM",
            human(gpu.mem_used),
            human(gpu.mem_total)
        )];
        if let Some(temp) = gpu.temp {
            parts.push(format!("{temp} °C"));
        }
        if !gpu.name.is_empty() {
            parts.push(gpu.name.clone());
        }
        parts.join(" · ")
    }

    fn tooltip(&self) -> String {
        let mut text = format!(
            "CPU {} · RAM {} ({} used · {} cache · {} free)",
            self.percent(self.sample.cpu),
            self.percent(self.sample.mem_fraction()),
            human(self.sample.mem_used),
            human(self.sample.mem_cache()),
            human(self.sample.mem_free),
        );
        if let Some(gpu) = &self.sample.gpu {
            text.push_str(&format!(" · GPU {}", self.percent(gpu.util)));
        }
        text
    }
}

/// The pieces of one metric block in the popover.
struct MetricBlock {
    root: gtk::Box,
    value: gtk::Label,
    detail: gtk::Label,
    graph: gtk::DrawingArea,
}

/// Build one metric block: a header (icon, name, value), a sparkline, an
/// optional extra widget and a detail line. `class` picks both the icon and the
/// graph colour; `extra` is inserted between the graph and the detail (the
/// Memory block uses it for its used / cache / free breakdown).
fn metric_block(
    title: &str,
    icon_name: &str,
    class: &str,
    history: Rc<RefCell<Vec<f64>>>,
    extra: Option<&gtk::Box>,
) -> MetricBlock {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.add_css_class("mango-resource-metric");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_valign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name(icon_name);
    icon.add_css_class("mango-resource-icon");
    icon.add_css_class(class);
    header.append(&icon);

    let name = gtk::Label::new(Some(title));
    name.add_css_class("mango-resource-name");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    header.append(&name);

    let value = gtk::Label::new(Some("—"));
    value.add_css_class("mango-resource-value");
    value.add_css_class(class);
    value.set_xalign(1.0);
    header.append(&value);

    root.append(&header);

    let graph = gtk::DrawingArea::new();
    graph.set_content_height(GRAPH_H);
    graph.set_hexpand(true);
    graph.set_valign(gtk::Align::Center);
    graph.add_css_class("mango-resource-graph");
    graph.add_css_class(class);
    graph.set_draw_func({
        let history = history.clone();
        move |area, cr, width, height| draw_spark(area, cr, width, height, &history.borrow())
    });
    root.append(&graph);

    if let Some(extra) = extra {
        root.append(extra);
    }

    let detail = gtk::Label::new(None);
    detail.add_css_class("mango-resource-detail");
    detail.set_xalign(0.0);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    root.append(&detail);

    MetricBlock {
        root,
        value,
        detail,
        graph,
    }
}

/// The three memory segments, as fractions of total RAM, shared between the
/// model and the breakdown bar's draw function.
#[derive(Debug, Clone, Copy, Default)]
struct MemParts {
    used: f64,
    cache: f64,
    free: f64,
}

/// The Memory block's extra widgets: a segmented bar and the legend whose
/// labels the model keeps in sync.
struct MemBreakdown {
    root: gtk::Box,
    bar: gtk::DrawingArea,
    parts: Rc<RefCell<MemParts>>,
    used: gtk::Label,
    cache: gtk::Label,
    free: gtk::Label,
}

/// Build the used / cache / free breakdown: a bar that reads `MemParts` and a
/// legend of three colour-keyed labels.
fn mem_breakdown() -> MemBreakdown {
    let parts = Rc::new(RefCell::new(MemParts::default()));

    let bar = gtk::DrawingArea::new();
    bar.set_content_height(MEM_BAR_H);
    bar.set_hexpand(true);
    bar.add_css_class("mango-resource-mem-bar");
    bar.set_draw_func({
        let parts = parts.clone();
        move |area, cr, width, height| draw_mem_bar(area, cr, width, height, &parts.borrow())
    });

    let legend = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    legend.add_css_class("mango-resource-legend");
    let used = mem_legend_entry(&legend, "used", MEM_USED_ALPHA);
    let cache = mem_legend_entry(&legend, "cache", MEM_CACHE_ALPHA);
    let free = mem_legend_entry(&legend, "free", MEM_FREE_ALPHA);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.add_css_class("mango-resource-breakdown");
    root.append(&bar);
    root.append(&legend);

    MemBreakdown {
        root,
        bar,
        parts,
        used,
        cache,
        free,
    }
}

/// One legend entry: a colour swatch matching a bar segment, then its label.
fn mem_legend_entry(legend: &gtk::Box, class: &str, alpha: f64) -> gtk::Label {
    let entry = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    entry.set_hexpand(true);

    let swatch = gtk::DrawingArea::new();
    swatch.set_content_width(9);
    swatch.set_content_height(9);
    swatch.set_valign(gtk::Align::Center);
    swatch.add_css_class("mango-resource-swatch");
    swatch.add_css_class(class);
    swatch
        .set_draw_func(move |area, cr, width, height| draw_swatch(area, cr, width, height, alpha));
    entry.append(&swatch);

    let label = gtk::Label::new(None);
    label.add_css_class("mango-resource-legend-value");
    label.add_css_class(class);
    label.set_xalign(0.0);
    entry.append(&label);

    legend.append(&entry);
    label
}

/// Paint the breakdown bar: a faint free track, the cache band, then the used
/// portion on top — all in the block's CSS colour, told apart by alpha.
fn draw_mem_bar(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    parts: &MemParts,
) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let color = area.style_context().color();
    let (r, g, b) = (
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
    );

    // Free is the whole track; cache and used are laid over it left to right.
    cr.set_source_rgba(r, g, b, MEM_FREE_ALPHA);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let used_w = w * parts.used.clamp(0.0, 1.0);
    let cache_w = w * parts.cache.clamp(0.0, 1.0);
    cr.set_source_rgba(r, g, b, MEM_CACHE_ALPHA);
    cr.rectangle(used_w, 0.0, cache_w, h);
    let _ = cr.fill();

    cr.set_source_rgba(r, g, b, MEM_USED_ALPHA);
    cr.rectangle(0.0, 0.0, used_w, h);
    let _ = cr.fill();
}

/// Paint a legend swatch in the block's CSS colour at `alpha`.
fn draw_swatch(area: &gtk::DrawingArea, cr: &cairo::Context, width: i32, height: i32, alpha: f64) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let color = area.style_context().color();
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        alpha,
    );
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();
}

/// One Storage row: mount point, a usage bar, `used / total` and the percent.
fn storage_row(disk: &DiskUsage) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("mango-resource-storage");

    let mount = gtk::Label::new(Some(&disk.mount));
    mount.set_xalign(0.0);
    mount.set_hexpand(true);
    mount.set_ellipsize(gtk::pango::EllipsizeMode::End);
    mount.add_css_class("mango-resource-mount");
    row.append(&mount);

    row.append(&usage_bar(disk.fraction()));

    let amount = gtk::Label::new(Some(&format!(
        "{} / {}",
        human(disk.used),
        human(disk.total)
    )));
    amount.add_css_class("mango-resource-amount");
    row.append(&amount);

    let percent = gtk::Label::new(Some(&format!("{:.0}%", disk.fraction() * 100.0)));
    percent.add_css_class("mango-resource-percent");
    percent.set_width_chars(4);
    percent.set_xalign(1.0);
    row.append(&percent);

    row
}

/// A small rounded bar filled to `fraction`, painted in the CSS colour.
fn usage_bar(fraction: f64) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(USAGE_W);
    area.set_content_height(6);
    area.set_valign(gtk::Align::Center);
    area.add_css_class("mango-resource-usage");
    if fraction >= HIGH {
        // Near-full disks read in the warning colour (see `.high` in style.css).
        area.add_css_class("high");
    }
    area.set_draw_func(move |area, cr, width, height| {
        draw_usage(area, cr, width, height, fraction)
    });
    area
}

/// Paint a sparkline: the series `values` (each `0.0..=1.0`) as a line with a
/// soft fill under it, newest sample on the right.
fn draw_spark(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    values: &[f64],
) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 1.0 || h <= 1.0 || values.len() < 2 {
        return;
    }

    let color = area.style_context().color();
    let (r, g, b) = (
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
    );

    // A faint baseline keeps an idle series from looking broken.
    cr.set_source_rgba(r, g, b, 0.20);
    cr.rectangle(0.0, h - 1.0, w, 1.0);
    let _ = cr.fill();

    let slots = (HISTORY as f64 - 1.0).max(1.0);
    let step = w / slots;
    let x0 = w - step * (values.len() as f64 - 1.0);
    let y = |value: f64| h - (value.clamp(0.0, 1.0) * (h - 2.0) + 1.0);

    // The filled area under the line.
    let gradient = cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    gradient.add_color_stop_rgba(0.0, r, g, b, 0.38);
    gradient.add_color_stop_rgba(1.0, r, g, b, 0.02);
    cr.move_to(x0, y(values[0]));
    for (index, value) in values.iter().enumerate().skip(1) {
        cr.line_to(x0 + index as f64 * step, y(*value));
    }
    cr.line_to(w, h);
    cr.line_to(x0, h);
    cr.close_path();
    let _ = cr.set_source(&gradient);
    let _ = cr.fill();

    // The line itself, on top.
    cr.move_to(x0, y(values[0]));
    for (index, value) in values.iter().enumerate().skip(1) {
        cr.line_to(x0 + index as f64 * step, y(*value));
    }
    cr.set_source_rgba(r, g, b, 0.95);
    cr.set_line_width(1.6);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
}

/// Paint a usage bar: a track with a filled portion.
fn draw_usage(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    fraction: f64,
) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let radius = 0.0;

    let color = area.style_context().color();
    let (r, g, b) = (
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
    );

    rounded_rect(cr, 0.0, 0.0, w, h, radius);
    cr.set_source_rgba(r, g, b, 0.18);
    let _ = cr.fill();

    let filled = (fraction.clamp(0.0, 1.0) * w).max(radius * 2.0).min(w);
    rounded_rect(cr, 0.0, 0.0, filled, h, radius);
    cr.set_source_rgba(r, g, b, 0.9);
    let _ = cr.fill();
}

/// A rounded rectangle path (GTK CSS can't draw these inside a cairo context).
fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let radius = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - radius, y + radius, radius, -pi / 2.0, 0.0);
    cr.arc(x + w - radius, y + h - radius, radius, 0.0, pi / 2.0);
    cr.arc(x + radius, y + h - radius, radius, pi / 2.0, pi);
    cr.arc(x + radius, y + radius, radius, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

/// Append `value` to `history`, dropping the oldest sample past [`HISTORY`].
fn push(history: &Rc<RefCell<Vec<f64>>>, value: f64) {
    let mut history = history.borrow_mut();
    history.push(value);
    if history.len() > HISTORY {
        history.remove(0);
    }
}

/// Set a label only when its text actually differs, so an unchanged poll leaves
/// the widget clean (no property change, no relayout, no repaint).
fn set_text(label: &gtk::Label, text: &str) {
    if label.text().as_str() != text {
        label.set_text(text);
    }
}

/// Toggle visibility only on a change, for the same reason as [`set_text`].
fn set_visible(widget: &impl IsA<gtk::Widget>, visible: bool) {
    if widget.is_visible() != visible {
        widget.set_visible(visible);
    }
}

/// Repaint a sparkline only while it is on screen: with the popover closed the
/// drawing areas are unmapped and a queued redraw would be discarded anyway.
fn redraw(area: &gtk::DrawingArea) {
    if area.is_mapped() {
        area.queue_draw();
    }
}

/// `part / total` as a `0.0..=1.0` fraction, or `0.0` when `total` is zero.
fn fraction(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// `1_536` bytes as `1.5 GiB`, or `512 MiB` below a gibibyte.
fn human(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else {
        format!("{:.0} MiB", value / MIB)
    }
}

// --- Polling --------------------------------------------------------------

/// Re-read every metric forever on a background thread, polling at [`POLL_MS`]
/// while the popover is closed and [`FAST_MS`] while it is open.
fn spawn_poller(
    sender: ComponentSender<ResourcesModel>,
    popover_open: Arc<AtomicBool>,
) -> Sender<Poll> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut gpu = Gpu::start();
        let mut prev = read_cpu_times();

        // A brief baseline gives the very first paint a real CPU percentage
        // instead of a placeholder zero.
        std::thread::sleep(Duration::from_millis(250));
        let cur = read_cpu_times();
        let mut cpu = cpu_fraction(prev, cur);
        prev = cur;

        let mut disks = read_disks();
        let mut tick: u32 = 0;

        sender.input(ResourcesInput::Update(sample(cpu, &mut gpu, &disks)));

        loop {
            // Idle when the popover is closed, brisk while it is open.
            let interval = if popover_open.load(Ordering::Relaxed) {
                FAST_MS
            } else {
                POLL_MS
            };
            let refresh = match rx.recv_timeout(Duration::from_millis(interval)) {
                Ok(Poll::Refresh) => true,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };

            // Only recompute the CPU fraction over a full interval: a refresh
            // arriving mid-interval would otherwise read as a spike.
            if !refresh {
                let cur = read_cpu_times();
                cpu = cpu_fraction(prev, cur);
                prev = cur;
            }

            if tick.is_multiple_of(DISK_EVERY) {
                disks = read_disks();
            }
            tick = tick.wrapping_add(1);

            sender.input(ResourcesInput::Update(sample(cpu, &mut gpu, &disks)));
        }
    });

    tx
}

/// Read the cheap metrics and attach a fresh GPU sample.
fn sample(cpu: f64, gpu: &mut Gpu, disks: &[DiskUsage]) -> ResourceSample {
    let mem = read_meminfo();
    ResourceSample {
        cpu,
        mem_used: mem.total.saturating_sub(mem.available),
        mem_free: mem.free,
        mem_total: mem.total,
        swap_used: mem.swap_total.saturating_sub(mem.swap_free),
        swap_total: mem.swap_total,
        cores: std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(0),
        freq_mhz: read_freq(),
        load: read_load(),
        gpu: gpu.sample(),
        disks: disks.to_vec(),
    }
}

/// Aggregated CPU jiffies: `total` is every counter, `busy` is everything but
/// idle and iowait.
#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    busy: u64,
}

fn read_cpu_times() -> CpuTimes {
    let Some(stat) = std::fs::read_to_string("/proc/stat").ok() else {
        return CpuTimes { total: 0, busy: 0 };
    };
    let Some(line) = stat.lines().next() else {
        return CpuTimes { total: 0, busy: 0 };
    };
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if values.len() < 5 {
        return CpuTimes { total: 0, busy: 0 };
    }
    let idle = values[3] + values[4];
    CpuTimes {
        total: values.iter().sum(),
        busy: values.iter().sum::<u64>() - idle,
    }
}

/// The busy fraction between two readings, in `0.0..=1.0`.
fn cpu_fraction(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let total = cur.total.saturating_sub(prev.total);
    if total == 0 {
        return 0.0;
    }
    let busy = cur.busy.saturating_sub(prev.busy);
    (busy as f64 / total as f64).clamp(0.0, 1.0)
}

struct Meminfo {
    total: u64,
    available: u64,
    free: u64,
    swap_total: u64,
    swap_free: u64,
}

/// Read `/proc/meminfo` (values are in kB, converted to bytes).
fn read_meminfo() -> Meminfo {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut mem = Meminfo {
        total: 0,
        available: 0,
        free: 0,
        swap_total: 0,
        swap_free: 0,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(kb) = value
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse::<u64>()
        else {
            continue;
        };
        let bytes = kb.saturating_mul(1024);
        match key {
            "MemTotal" => mem.total = bytes,
            "MemAvailable" => mem.available = bytes,
            "MemFree" => mem.free = bytes,
            "SwapTotal" => mem.swap_total = bytes,
            "SwapFree" => mem.swap_free = bytes,
            _ => {}
        }
    }
    mem
}

/// The average current CPU frequency across every core, in MHz.
fn read_freq() -> Option<u32> {
    let dir = std::fs::read_dir("/sys/devices/system/cpu").ok()?;
    let mut sum = 0u64;
    let mut count = 0u64;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(index) = name.strip_prefix("cpu") else {
            continue;
        };
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path().join("cpufreq/scaling_cur_freq"))
            && let Ok(khz) = text.trim().parse::<u64>()
        {
            sum += khz;
            count += 1;
        }
    }
    (count > 0).then(|| (sum / count / 1000) as u32)
}

/// The 1-minute load average.
fn read_load() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// The mounted filesystems worth showing, busiest first.
fn read_disks() -> Vec<DiskUsage> {
    let output = Command::new("df")
        .args([
            "-B1", "-P", "-x", "tmpfs", "-x", "devtmpfs", "-x", "squashfs", "-x", "overlay", "-x",
            "efivarfs",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    parse_df(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `df -P` output, keeping real mounts only and de-duplicating by device
/// (so bind mounts and btrfs subvolumes do not repeat the same filesystem).
fn parse_df(output: &str) -> Vec<DiskUsage> {
    let mut disks: Vec<DiskUsage> = Vec::new();
    let mut devices: Vec<String> = Vec::new();

    for line in output.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 6 {
            continue;
        }
        let mount = columns[5..].join(" ");
        if SKIP_MOUNTS.iter().any(|skip| mount.starts_with(skip)) {
            continue;
        }
        let Ok(total) = columns[1].parse::<u64>() else {
            continue;
        };
        let Ok(used) = columns[2].parse::<u64>() else {
            continue;
        };
        if total == 0 {
            continue;
        }
        let device = columns[0].to_string();
        if devices.contains(&device) {
            continue;
        }
        devices.push(device);
        disks.push(DiskUsage { mount, used, total });
    }

    disks.sort_by_key(|disk| std::cmp::Reverse(disk.total));
    disks.truncate(6);
    disks
}

// --- GPU backends ---------------------------------------------------------

/// The GPU sampler: NVIDIA's streaming `nvidia-smi`, AMD's sysfs counters, or
/// nothing at all.
enum Gpu {
    Nvidia(NvidiaGpu),
    Amd(AmdGpu),
    None,
}

impl Gpu {
    /// Pick a backend once: prefer NVIDIA when `nvidia-smi` answers, else AMD.
    fn start() -> Self {
        if let Some(name) = nvidia_name() {
            return Gpu::Nvidia(NvidiaGpu::new(name));
        }
        if let Some(amd) = AmdGpu::detect() {
            return Gpu::Amd(amd);
        }
        Gpu::None
    }

    fn sample(&mut self) -> Option<GpuSample> {
        match self {
            Gpu::Nvidia(nvidia) => nvidia.sample(),
            Gpu::Amd(amd) => amd.sample(),
            Gpu::None => None,
        }
    }
}

/// A long-lived `nvidia-smi … -l N` process, drained without blocking.
struct NvidiaGpu {
    name: String,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    buffer: String,
    last: Option<GpuSample>,
    /// Ticks left before a dead stream is restarted.
    retry: u32,
}

impl NvidiaGpu {
    fn new(name: String) -> Self {
        let mut gpu = NvidiaGpu {
            name,
            child: None,
            stdout: None,
            buffer: String::new(),
            last: None,
            retry: 0,
        };
        gpu.spawn();
        gpu
    }

    fn spawn(&mut self) {
        self.stop();
        let interval = GPU_SECS.to_string();
        let spawned = Command::new("nvidia-smi")
            .args([
                "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu",
                "--format=csv,noheader,nounits",
                "-l",
                &interval,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned
            && let Some(stdout) = child.stdout.take()
        {
            set_nonblocking(&stdout);
            self.stdout = Some(stdout);
            self.child = Some(child);
            return;
        }
        self.retry = GPU_RETRY_TICKS;
    }

    fn sample(&mut self) -> Option<GpuSample> {
        if self.stdout.is_none() {
            if self.retry == 0 {
                self.spawn();
            } else {
                self.retry -= 1;
            }
            return self.last.clone();
        }

        let mut buffer = [0u8; 512];
        loop {
            let stdout = self.stdout.as_mut()?;
            match stdout.read(&mut buffer) {
                // EOF: the stream ended (driver reset, process killed).
                Ok(0) => {
                    self.stop();
                    break;
                }
                Ok(read) => {
                    self.buffer
                        .push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.stop();
                    break;
                }
            }
        }

        // Every complete line is one frame; the newest wins.
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            if let Some(sample) = parse_nvidia(line.trim(), &self.name) {
                self.last = Some(sample);
            }
        }

        self.last.clone()
    }

    fn stop(&mut self) {
        self.stdout = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for NvidiaGpu {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One `nvidia-smi` CSV row: `util, mem_used, mem_total, temp` (MiB, MiB, °C).
fn parse_nvidia(line: &str, name: &str) -> Option<GpuSample> {
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    if fields.len() < 4 {
        return None;
    }
    const MIB: u64 = 1024 * 1024;
    Some(GpuSample {
        name: name.to_string(),
        util: (fields[0].parse::<f64>().ok()? / 100.0).clamp(0.0, 1.0),
        mem_used: fields[1].parse::<u64>().ok()?.saturating_mul(MIB),
        mem_total: fields[2].parse::<u64>().ok()?.saturating_mul(MIB),
        temp: fields[3].parse::<u32>().ok(),
    })
}

/// The first GPU's name, or `None` when `nvidia-smi` is missing or reports no
/// GPU. Also serves as the "is NVIDIA present?" probe.
fn nvidia_name() -> Option<String> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout);
    let name = name.lines().next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// An AMD GPU read straight from sysfs (no process at all).
struct AmdGpu {
    device: PathBuf,
    hwmon: Option<PathBuf>,
    name: String,
}

impl AmdGpu {
    fn detect() -> Option<Self> {
        let entries = std::fs::read_dir("/sys/class/drm").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `card0`, but not `card0-HDMI-A-1`.
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device = entry.path().join("device");
            if !device.join("gpu_busy_percent").exists() {
                continue;
            }
            let label = std::fs::read_to_string(device.join("product_name"))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "AMD GPU".to_string());
            return Some(AmdGpu {
                hwmon: find_amdgpu_hwmon(&device),
                device,
                name: label,
            });
        }
        None
    }

    fn sample(&self) -> Option<GpuSample> {
        let util = read_u64(&self.device.join("gpu_busy_percent"))? as f64 / 100.0;
        let mem_used = read_u64(&self.device.join("mem_info_vram_used")).unwrap_or(0);
        let mem_total = read_u64(&self.device.join("mem_info_vram_total")).unwrap_or(0);
        let temp = self
            .hwmon
            .as_ref()
            .and_then(|dir| read_u64(&dir.join("temp1_input")))
            .map(|milli| (milli / 1000) as u32);
        Some(GpuSample {
            name: self.name.clone(),
            util: util.clamp(0.0, 1.0),
            mem_used,
            mem_total,
            temp,
        })
    }
}

/// The `amdgpu` hwmon directory under a card's `device`, if any.
fn find_amdgpu_hwmon(device: &std::path::Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(device.join("hwmon")).ok()?.flatten() {
        if let Ok(name) = std::fs::read_to_string(entry.path().join("name"))
            && name.trim() == "amdgpu"
        {
            return Some(entry.path());
        }
    }
    None
}

fn read_u64(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Put a file descriptor into non-blocking mode so the `nvidia-smi` stream can
/// be drained without stalling the poll loop.
fn set_nonblocking(fd: &impl AsRawFd) {
    let raw = fd.as_raw_fd();
    // SAFETY: `fcntl` only touches the descriptor's own flags.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_fraction_uses_the_delta_between_readings() {
        let prev = CpuTimes {
            total: 1_000,
            busy: 400,
        };
        let cur = CpuTimes {
            total: 1_200,
            busy: 500,
        };
        // 100 busy jiffies over 200 total = 50%.
        assert!((cpu_fraction(prev, cur) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cpu_fraction_is_zero_without_elapsed_time() {
        let same = CpuTimes { total: 5, busy: 2 };
        assert_eq!(cpu_fraction(same, same), 0.0);
    }

    #[test]
    fn memory_splits_into_used_cache_and_free() {
        // 4 GiB held by apps, 2 GiB completely free, so the remaining 4 GiB is
        // reclaimable cache. `mem_used` is `total - available`, so available is
        // implied to be 6 GiB and cache = available - free = 4 GiB.
        let sample = ResourceSample {
            mem_used: 4,
            mem_free: 2,
            mem_total: 10,
            ..ResourceSample::default()
        };
        assert_eq!(sample.mem_cache(), 4);
        assert!((sample.mem_fraction() - 0.4).abs() < 1e-9);
        assert!((sample.mem_cache_fraction() - 0.4).abs() < 1e-9);
        assert!((sample.mem_free_fraction() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn memory_fractions_survive_a_zero_total() {
        // A missing `/proc/meminfo` leaves everything at zero: no NaN, no panic.
        let sample = ResourceSample::default();
        assert_eq!(sample.mem_cache(), 0);
        assert_eq!(sample.mem_fraction(), 0.0);
        assert_eq!(sample.mem_cache_fraction(), 0.0);
        assert_eq!(sample.mem_free_fraction(), 0.0);
    }

    #[test]
    fn parses_an_nvidia_row() {
        let gpu = parse_nvidia("37, 2048, 6144, 55", "RTX 2060").unwrap();
        assert!((gpu.util - 0.37).abs() < 1e-9);
        assert_eq!(gpu.mem_used, 2048 * 1024 * 1024);
        assert_eq!(gpu.mem_total, 6144 * 1024 * 1024);
        assert_eq!(gpu.temp, Some(55));
        assert_eq!(gpu.name, "RTX 2060");
    }

    #[test]
    fn an_unavailable_temperature_is_none() {
        let gpu = parse_nvidia("0, 9, 6144, N/A", "GPU").unwrap();
        assert_eq!(gpu.temp, None);
    }

    #[test]
    fn rejects_a_short_nvidia_row() {
        assert!(parse_nvidia("garbage", "GPU").is_none());
    }

    #[test]
    fn parses_df_and_drops_duplicate_devices() {
        let out = "\
Filesystem     1B-blocks         Used    Available Capacity Mounted on
/dev/nvme0n1p2 507813089280 383148462080 121893384192      76% /
/dev/nvme0n1p2 507813089280 383148462080 121893384192      76% /home
/dev/nvme0n1p1    536870912     10485760    526385152       2% /boot
tmpfs               1000000       100000       900000      10% /run/user/1000
";
        let disks = parse_df(out);
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].mount, "/");
        assert_eq!(disks[1].mount, "/boot");
    }

    #[test]
    fn skips_virtual_mounts() {
        let out = "\
Filesystem 1B-blocks Used Available Capacity Mounted on
sysfs             0    0         0        - /sys
/dev/sda1  10000 5000      5000      50% /data
";
        let disks = parse_df(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount, "/data");
    }

    #[test]
    fn formats_bytes_for_humans() {
        assert_eq!(human(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(human(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn history_is_capped() {
        let history = Rc::new(RefCell::new(Vec::new()));
        for value in 0..(HISTORY + 10) {
            push(&history, value as f64);
        }
        assert_eq!(history.borrow().len(), HISTORY);
        assert_eq!(*history.borrow().first().unwrap(), 10.0);
    }

    #[test]
    fn disk_fraction_handles_zero_capacity() {
        let disk = DiskUsage {
            mount: "/".to_string(),
            used: 0,
            total: 0,
        };
        assert_eq!(disk.fraction(), 0.0);
    }
}
