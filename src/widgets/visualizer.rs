//! Audio visualizer widget, backed by [`cava`](https://github.com/karlstav/cava).
//!
//! `cava` is the same small FFT server Waybar and friends use. It is spawned
//! with a generated config that makes it print the analysed spectrum to stdout
//! in *raw ASCII*: one frame per line, one decimal value per band, separated by
//! `;` (`0;120;45;…`). A background thread parses those frames and hands them to
//! the UI, which paints them as bars in a [`gtk::DrawingArea`] using cairo.
//!
//! Two properties matter for a bar that runs on every monitor and gets rebuilt
//! whenever the panel changes:
//!
//! * the child is **killed on shutdown**, so a rebuild never leaves a stray
//!   `cava` behind (the thread also exits as soon as the pipe closes);
//! * the widget **hides itself** when `cava` is missing or exits, and retries
//!   on a timer — so a sound server that comes up *after* the shell does not
//!   leave a dead gap in the bar.

use std::cell::RefCell;
use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use relm4::Sender;
use relm4::prelude::*;

/// Analysed bands — one per drawn bar.
const BARS: usize = 12;
/// Analysis frames per second. 30 is smooth enough for a bar and light on CPU.
const FRAMERATE: u32 = 30;
/// `cava`'s `ascii_max_range`; a band's raw value is `0..=MAX_RANGE`.
const MAX_RANGE: f64 = 1000.0;
/// Painted size of the drawing area, in pixels.
const WIDTH: i32 = 72;
const HEIGHT: i32 = 18;
/// Gap between neighbouring bars, in pixels.
const GAP: f64 = 2.0;
/// Bars never shrink below this, so silence still reads as a thin baseline.
const MIN_BAR: f64 = 2.0;
/// How long to wait before retrying when `cava` is missing or dies.
const RETRY: Duration = Duration::from_secs(5);

/// Distinguishes each widget's generated config file (one per bar/monitor).
static CONFIG_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct VisualizerModel {
    /// The latest frame: one level in `0.0..=1.0` per band. Shared with the
    /// draw function, which cannot borrow the model.
    levels: Rc<RefCell<Vec<f64>>>,
    /// The area repainted when a new frame lands.
    canvas: gtk::DrawingArea,
    /// The `cava` child; killed by [`VisualizerModel::stop`].
    child: Option<Child>,
    /// The generated config file, removed when the child is stopped.
    config: Option<PathBuf>,
    /// True while the widget is hidden (no `cava` running).
    ended: bool,
    /// Pending retry timer, cancelled on shutdown.
    retry: Option<glib::SourceId>,
}

#[derive(Debug)]
pub enum VisualizerInput {
    /// A frame from `cava`, one level per band.
    Frame(Vec<f64>),
    /// `cava` exited; hide the widget and try again shortly.
    Ended,
    /// (Re)start `cava`.
    Retry,
}

#[relm4::component(pub)]
impl SimpleComponent for VisualizerModel {
    type Init = ();
    type Input = VisualizerInput;
    type Output = ();

    view! {
        #[root]
        gtk::Box {
            set_css_classes: &["mango-group", "mango-visualizer"],
            set_orientation: gtk::Orientation::Horizontal,
            set_valign: gtk::Align::Center,
            // Hiding the root hides the pill too, so a missing `cava` leaves
            // no empty bubble in the bar.
            #[watch]
            set_visible: !model.ended,

            #[name = "canvas"]
            gtk::DrawingArea {
                set_content_width: WIDTH,
                set_content_height: HEIGHT,
                set_valign: gtk::Align::Center,
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let levels = Rc::new(RefCell::new(vec![0.0; BARS]));
        let mut model = VisualizerModel {
            levels: levels.clone(),
            // Replaced with the real area right after `view_output!`.
            canvas: gtk::DrawingArea::new(),
            child: None,
            config: None,
            ended: false,
            retry: None,
        };

        let widgets = view_output!();
        model.canvas = widgets.canvas.clone();

        widgets.canvas.set_draw_func({
            let levels = levels.clone();
            move |area, cr, width, height| draw(area, cr, width, height, &levels.borrow())
        });

        if !model.start(&sender) {
            // No `cava` yet (not installed, or the sound server is not up):
            // stay hidden and keep checking.
            model.ended = true;
            root.set_visible(false);
            model.retry = Some(schedule_retry(&sender));
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            VisualizerInput::Frame(levels) => {
                let changed = {
                    let mut current = self.levels.borrow_mut();
                    if *current == levels {
                        false
                    } else {
                        *current = levels;
                        true
                    }
                };
                // Only repaint when the picture actually changed, so an idle
                // (all-zero) stream costs no rendering at all.
                if changed {
                    self.ended = false;
                    self.canvas.queue_draw();
                }
            }
            VisualizerInput::Ended => {
                self.ended = true;
                self.stop();
                self.retry = Some(schedule_retry(&sender));
            }
            VisualizerInput::Retry => {
                // The retry timer fired to deliver this message, so its source
                // is already gone: drop the id before maybe arming a new one,
                // or `shutdown` later removes a dead source and glib panics.
                self.retry = None;
                if self.start(&sender) {
                    self.ended = false;
                } else {
                    self.retry = Some(schedule_retry(&sender));
                }
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: Sender<Self::Output>) {
        if let Some(timer) = self.retry.take() {
            timer.remove();
        }
        self.stop();
    }
}

impl VisualizerModel {
    /// Spawn `cava` and a reader thread feeding [`VisualizerInput::Frame`]s.
    /// Returns `false` if it could not be started.
    fn start(&mut self, sender: &ComponentSender<Self>) -> bool {
        self.stop();

        let Some((child, stdout, config)) = spawn_cava() else {
            return false;
        };

        let sender = sender.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                // EOF (cava exited) or a read error both end the stream.
                let Ok(line) = line else { break };
                let levels = parse_frame(&line);
                if levels.is_empty() {
                    continue;
                }
                sender.input(VisualizerInput::Frame(levels));
            }
            sender.input(VisualizerInput::Ended);
        });

        self.child = Some(child);
        self.config = Some(config);
        true
    }

    /// Kill the child and drop its config file.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(config) = self.config.take() {
            let _ = std::fs::remove_file(config);
        }
    }
}

/// Paint the bars: `levels` are `0.0..=1.0`, drawn bottom-up in the widget's
/// CSS foreground colour.
fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, width: i32, height: i32, levels: &[f64]) {
    let w = f64::from(width);
    let h = f64::from(height);
    if w <= 0.0 || h <= 0.0 || levels.is_empty() {
        return;
    }

    let color = area.style_context().color();
    if color.alpha() <= 0.0 {
        return;
    }
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()),
    );

    let slot = w / levels.len() as f64;
    let bar_w = (slot - GAP).max(1.0);
    for (index, level) in levels.iter().enumerate() {
        let bar_h = (level.clamp(0.0, 1.0) * h).max(MIN_BAR);
        cr.rectangle(index as f64 * slot, h - bar_h, bar_w, bar_h);
    }
    let _ = cr.fill();
}

/// Parse one raw frame (`"0;120;45;"`) into levels in `0.0..=1.0`.
fn parse_frame(line: &str) -> Vec<f64> {
    line.split(';')
        .filter(|field| !field.is_empty())
        .map(|field| field.trim().parse::<f64>().unwrap_or(0.0) / MAX_RANGE)
        .collect()
}

/// The `cava` config that turns it into a line-per-frame raw ascii source.
fn cava_config() -> String {
    format!(
        "[general]\n\
         framerate = {FRAMERATE}\n\
         bars = {BARS}\n\
         \n\
         [output]\n\
         method = raw\n\
         raw_target = /dev/stdout\n\
         data_format = ascii\n\
         ascii_max_range = {max}\n\
         bar_delimiter = 59\n\
         frame_delimiter = 10\n",
        max = MAX_RANGE as u32,
    )
}

/// Write the config and spawn `cava`, or `None` if either step fails.
fn spawn_cava() -> Option<(Child, ChildStdout, PathBuf)> {
    let config = std::env::temp_dir().join(format!(
        "mango-shell-cava-{}-{}.conf",
        std::process::id(),
        CONFIG_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    if std::fs::write(&config, cava_config()).is_err() {
        return None;
    }

    let mut child = match Command::new("cava")
        .arg("-p")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = std::fs::remove_file(&config);
            return None;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&config);
        return None;
    };

    Some((child, stdout, config))
}

/// Ask for a [`VisualizerInput::Retry`] after [`RETRY`].
fn schedule_retry(sender: &ComponentSender<VisualizerModel>) -> glib::SourceId {
    let sender = sender.clone();
    glib::timeout_add_local_once(RETRY, move || {
        sender.input(VisualizerInput::Retry);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_raw_frame() {
        let levels = parse_frame("0;500;1000;");
        assert_eq!(levels, vec![0.0, 0.5, 1.0]);
    }

    #[test]
    fn ignores_the_trailing_delimiter() {
        // cava ends every frame with the bar delimiter; the empty tail is
        // dropped rather than read as an extra band.
        assert_eq!(parse_frame("1000;1000;").len(), 2);
    }

    #[test]
    fn a_garbage_field_reads_as_silence() {
        assert_eq!(parse_frame("nonsense;250;"), vec![0.0, 0.25]);
    }

    #[test]
    fn config_requests_raw_ascii() {
        let config = cava_config();
        assert!(config.contains("method = raw"));
        assert!(config.contains("data_format = ascii"));
        assert!(config.contains(&format!("bars = {BARS}")));
    }
}
