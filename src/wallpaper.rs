//! Wallpaper: the self-drawn desktop background and its shared plumbing.
//!
//! There is deliberately no external wallpaper daemon here: the shell paints
//! the desktop itself. [`WallpaperModel`] is one [`Layer::Background`] surface
//! per monitor holding a `GtkPicture` fitted to cover the screen. Applying a
//! wallpaper swaps the picture on every background surface and remembers the
//! choice in `~/.config/mango-shell/wallpaper`, so it is restored on the next
//! start.
//!
//! The browser for picking one — a fullscreen skewed carousel ported from
//! DankMaterialShell's [wallpaperCarousel] plugin — lives in
//! [`crate::carousel`]. This module owns the pieces it shares with the rest of
//! the shell: where wallpapers are read from ([`wallpaper_dir`]), how the
//! directory is scanned ([`scan_images`]), and the persisted selection
//! ([`load_current`] / [`save_current`] / [`run_hook`]).
//!
//! [wallpaperCarousel]: https://github.com/motor-dev/wallpaperCarousel

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::prelude::*;

/// Where wallpapers are read from, relative to `$HOME`.
const DEFAULT_DIR: &str = "Pictures/Wallpapers";

/// Extensions the carousel treats as images (the plugin's `nameFilters`).
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "jxl", "avif", "heif", "exr", "tif", "tiff",
];

// --- Directory + state plumbing ------------------------------------------

/// The directory scanned for wallpapers, overridable with `MANGO_WALLPAPER_DIR`.
pub fn wallpaper_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MANGO_WALLPAPER_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_DIR))
}

fn state_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/mango-shell/wallpaper"))
}

/// The wallpaper remembered from a previous run, if the file still exists.
pub fn load_current() -> Option<PathBuf> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_file().then_some(path)
}

/// Remember the chosen wallpaper (best effort).
pub fn save_current(path: &Path) {
    let Some(state) = state_path() else {
        return;
    };
    if let Some(parent) = state.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("mango-shell: could not create config dir: {error}");
        return;
    }
    if let Err(error) = std::fs::write(&state, path.to_string_lossy().as_bytes()) {
        eprintln!("mango-shell: could not save wallpaper state: {error}");
    }
}

/// Run `~/.config/mango-shell/on-wallpaper-change.sh <path>`, if it exists.
///
/// This is the seam for wallpaper-derived theming: point it at `matugen` or
/// `pywal` and regenerate `~/.config/mango-shell/style.css` whenever the
/// wallpaper changes. Blocking, so callers should run it off the UI thread and
/// reload the stylesheet once it returns. A no-op when the script is absent.
pub fn run_hook(path: &Path) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let hook = Path::new(&home).join(".config/mango-shell/on-wallpaper-change.sh");
    if !hook.is_file() {
        return;
    }
    match std::process::Command::new(&hook).arg(path).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("mango-shell: wallpaper hook exited with {status}"),
        Err(error) => eprintln!("mango-shell: could not run wallpaper hook: {error}"),
    }
}

/// Every image directly inside `dir`, sorted by name.
pub fn scan_images(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut images: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
                    })
        })
        .collect();
    images.sort();
    images
}

// Randomize transition types for variety (similar to awww's effects).
static TRANSITION_COUNTER: AtomicUsize = AtomicUsize::new(0);
fn next_transition() -> gtk::StackTransitionType {
    let idx = TRANSITION_COUNTER.fetch_add(1, Ordering::Relaxed);
    match idx % 7 {
        0 => gtk::StackTransitionType::Crossfade,
        1 => gtk::StackTransitionType::SlideRight,
        2 => gtk::StackTransitionType::SlideLeft,
        3 => gtk::StackTransitionType::SlideDown,
        4 => gtk::StackTransitionType::SlideUp,
        5 => gtk::StackTransitionType::OverUp,
        6 => gtk::StackTransitionType::OverDown,
        _ => gtk::StackTransitionType::Crossfade,
    }
}

/// Everything a wallpaper surface needs to exist.
pub struct WallpaperInit {
    /// The GDK monitor this surface is drawn on.
    pub monitor: gdk::Monitor,
    /// The wallpaper to show, if any. `None` leaves the compositor's own
    /// `root_color` visible.
    pub path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum WallpaperInput {
    /// Show this file, or hide the surface when `None`.
    SetPath(Option<PathBuf>),
}

pub struct WallpaperModel {
    window: gtk::Window,
    stack: gtk::Stack,
    picture_a: gtk::Picture,
    picture_b: gtk::Picture,
    use_a: bool,
}

#[relm4::component(pub)]
impl SimpleComponent for WallpaperModel {
    type Init = WallpaperInit;
    type Input = WallpaperInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-wallpaper"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.stack.clone() {},
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Below every other surface, covering the monitor, and never taking
        // input. This is the layer the shell's invisible root already uses.
        root.init_layer_shell();
        root.set_monitor(Some(&init.monitor));
        root.set_namespace(Some("mango-wallpaper"));
        root.set_layer(Layer::Background);
        root.set_keyboard_mode(KeyboardMode::None);
        // -1 tells the compositor to ignore the bar's exclusive zone, so the
        // image stretches all the way to the top edge — behind the panel —
        // instead of being pushed below it.
        root.set_exclusive_zone(-1);
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Bottom, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);

        let picture_a = gtk::Picture::new();
        picture_a.set_content_fit(gtk::ContentFit::Cover);
        picture_a.set_can_shrink(true);
        picture_a.set_hexpand(true);
        picture_a.set_vexpand(true);

        let picture_b = gtk::Picture::new();
        picture_b.set_content_fit(gtk::ContentFit::Cover);
        picture_b.set_can_shrink(true);
        picture_b.set_hexpand(true);
        picture_b.set_vexpand(true);

        let stack = gtk::Stack::new();
        stack.add_child(&picture_a);
        stack.add_child(&picture_b);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(280);

        let mut model = WallpaperModel {
            window: root.clone(),
            stack: stack.clone(),
            picture_a: picture_a.clone(),
            picture_b: picture_b.clone(),
            use_a: true,
        };

        let widgets = view_output!();

        apply_crossfade(&mut model, init.path.as_deref());

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            WallpaperInput::SetPath(path) => apply_crossfade(self, path.as_deref()),
        }
    }
}

/// Point the surface at `path`, or hide it when there is nothing to show.
fn apply_crossfade(model: &mut WallpaperModel, path: Option<&Path>) {
    let Some(path) = path else {
        model.window.set_visible(false);
        return;
    };

    if let Ok(texture) = gdk::Texture::from_filename(path) {
        if model.use_a {
            model.picture_b.set_paintable(Some(&texture));
            model.stack.set_transition_type(next_transition());
            model.stack.set_visible_child(&model.picture_b);
        } else {
            model.picture_a.set_paintable(Some(&texture));
            model.stack.set_transition_type(next_transition());
            model.stack.set_visible_child(&model.picture_a);
        }
        model.use_a = !model.use_a;
        model.window.set_visible(true);
    } else {
        eprintln!("mango-shell: could not load wallpaper: {}", path.display());
        model.window.set_visible(false);
    }
}
