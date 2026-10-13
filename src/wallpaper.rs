//! Wallpaper: a self-drawn desktop background plus a picker overlay.
//!
//! There is deliberately no external wallpaper daemon here: the shell paints
//! the desktop itself. [`WallpaperModel`] is one [`Layer::Background`] surface
//! per monitor holding a `GtkPicture` fitted to cover the screen, and
//! [`WallpaperPickerModel`] is a single [`Layer::Overlay`] fly-out panel (see
//! [`crate::flyout`]): it hangs under the bar and slides out of it, holding a
//! grid of thumbnails (toggled by the shell, like the launcher). Applying a
//! thumbnail simply swaps the picture on every background surface and
//! remembers the choice in `~/.config/mango-shell/wallpaper`, so it is
//! restored on the next start.
//!
//! Thumbnails are the only fiddly part. `GdkPixbuf` is not `Send`, but
//! `gdk::Texture` is, and `gdk::MemoryTexture::new` may only run on the main
//! thread. So the worker decodes each image down to a tiny RGBA buffer (plain
//! `Vec<u8>`, which *is* `Send`), ships it over the component channel, and the
//! main thread wraps it in a texture. That keeps all decoding off the UI
//! thread while respecting GTK's threading rules.

use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::prelude::*;

use crate::flyout;

/// Where wallpapers are read from, relative to `$HOME`.
const DEFAULT_DIR: &str = "Pictures/Wallpapers";

/// Thumbnail bounding box in the picker grid, in pixels.
const THUMB_WIDTH: i32 = 220;
const THUMB_HEIGHT: i32 = 124;

/// How many tiles fit side by side; also how far Up/Down move the highlight.
const COLUMNS: isize = 4;

/// Extensions the picker treats as images.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "jxl", "avif", "bmp", "tif", "tiff",
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

// --- Background surface (one per monitor) --------------------------------

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
    picture: gtk::Picture,
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
            set_child = &model.picture.clone() {},
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

        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let model = WallpaperModel {
            window: root.clone(),
            picture,
        };

        let widgets = view_output!();

        apply(&model.picture, &model.window, init.path.as_deref());

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            WallpaperInput::SetPath(path) => apply(&self.picture, &self.window, path.as_deref()),
        }
    }
}

/// Point the surface at `path`, or hide it when there is nothing to show.
fn apply(picture: &gtk::Picture, window: &gtk::Window, path: Option<&Path>) {
    let Some(path) = path else {
        window.set_visible(false);
        return;
    };
    match gdk::Texture::from_filename(path) {
        Ok(texture) => {
            picture.set_paintable(Some(&texture));
            window.set_visible(true);
        }
        Err(error) => {
            eprintln!(
                "mango-shell: could not load wallpaper {}: {error}",
                path.display()
            );
            window.set_visible(false);
        }
    }
}

// --- Picker overlay ------------------------------------------------------

/// A single tile in the grid: the clickable child and the picture whose
/// texture is swapped in when the thumbnail finishes decoding.
struct Tile {
    child: gtk::FlowBoxChild,
    picture: gtk::Picture,
}

pub struct WallpaperPickerModel {
    window: gtk::Window,
    /// Slides the panel out from under the bar.
    revealer: gtk::Revealer,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
    search: gtk::SearchEntry,
    status: gtk::Label,
    flow: gtk::FlowBox,
    /// Every wallpaper on disk, in display order; never mutated after init.
    images: Vec<PathBuf>,
    /// One tile per entry in `images`, so indices line up exactly.
    tiles: Vec<Tile>,
    /// Indices into `images` that match the current query, in display order.
    matches: Vec<usize>,
    /// Position within `matches` of the highlighted tile.
    selected: usize,
    /// The active wallpaper, to mark its tile.
    current: Option<PathBuf>,
    query: String,
    /// Whether the thumbnail worker has been started.
    loading: bool,
    /// How many thumbnails have arrived.
    loaded: usize,
}

#[derive(Debug)]
pub enum WallpaperPickerInput {
    /// Flip visibility (sent by the shell on `SIGUSR2` or the bar button).
    Toggle,
    /// Hide without choosing (Escape, backdrop click).
    Hide,
    /// The query text changed.
    Search(String),
    /// Move the highlight within the filtered grid.
    Move(isize),
    /// Apply the highlighted tile (Enter).
    Activate,
    /// Apply the tile at this position in `matches` (grid click).
    Activated(usize),
    /// The active wallpaper changed elsewhere (e.g. the shell restored it).
    SetCurrent(PathBuf),
    /// A decoded thumbnail (worker → main).
    Thumb {
        index: usize,
        width: i32,
        height: i32,
        rgba: Vec<u8>,
    },
}

#[derive(Debug)]
pub enum WallpaperOutput {
    /// The user picked this wallpaper; the shell persists and broadcasts it.
    Apply(PathBuf),
}

#[relm4::component(pub)]
impl SimpleComponent for WallpaperPickerModel {
    type Init = Option<PathBuf>;
    type Input = WallpaperPickerInput;
    type Output = WallpaperOutput;

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-picker"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.revealer.clone() {},
        }
    }

    fn init(
        current: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fly-out surface: hugs the top of the monitor just below the bar,
        // exclusive keyboard while open, hidden until toggled.
        flyout::prepare_layer(&root, "mango-picker");

        let banner = flyout::make_banner();
        flyout::set_banner(&banner, current.as_deref());

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search wallpapers…"));
        search.set_hexpand(true);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_halign(gtk::Align::Start);
        status.add_css_class("mango-picker-status");

        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::Single);
        flow.set_valign(gtk::Align::Start);
        flow.set_column_spacing(8);
        flow.set_row_spacing(8);
        flow.set_min_children_per_line(2);
        flow.set_max_children_per_line(COLUMNS as u32);
        flow.set_homogeneous(true);
        flow.add_css_class("mango-picker-grid");

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_min_content_height(440);
        scrolled.set_child(Some(&flow));

        // The panel: wallpaper banner, then the search pill and the grid.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
        panel.add_css_class("mango-picker-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        panel.append(&search);
        panel.append(&status);
        panel.append(&scrolled);

        let revealer = flyout::make_revealer();
        revealer.set_child(Some(&panel));
        flyout::wire_hide(&root, &revealer);

        // Build a tile for every image up front (cheap: only the visible ones
        // are realized), so indices stay stable and filtering is just a
        // visibility toggle.
        let dir = wallpaper_dir();
        let images = dir.as_deref().map(scan_images).unwrap_or_default();
        let mut tiles = Vec::with_capacity(images.len());
        for path in &images {
            let (child, picture) = make_tile(path);
            flow.append(&child);
            tiles.push(Tile { child, picture });
        }

        // Filter on every text change (immediate, unlike `search-changed`).
        let search_sender = sender.clone();
        search.connect_changed(move |entry| {
            search_sender.input(WallpaperPickerInput::Search(entry.text().to_string()));
        });

        // Clicking a tile applies it.
        let activate_sender = sender.clone();
        flow.connect_child_activated(move |_, child| {
            let index = child.index();
            if index >= 0 {
                activate_sender.input(WallpaperPickerInput::Activated(index as usize));
            }
        });

        // Used by the backdrop click handler below.
        let backdrop_sender = sender.clone();

        // Capture phase, so these keys are seen before the search entry.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let key_sender = sender.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                key_sender.input(WallpaperPickerInput::Hide);
                glib::Propagation::Stop
            }
            gdk::Key::Left => {
                key_sender.input(WallpaperPickerInput::Move(-1));
                glib::Propagation::Stop
            }
            gdk::Key::Right => {
                key_sender.input(WallpaperPickerInput::Move(1));
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                key_sender.input(WallpaperPickerInput::Move(-COLUMNS));
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                key_sender.input(WallpaperPickerInput::Move(COLUMNS));
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                key_sender.input(WallpaperPickerInput::Activate);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        let mut model = WallpaperPickerModel {
            window: root.clone(),
            revealer,
            banner,
            search,
            status,
            flow,
            images,
            tiles,
            matches: Vec::new(),
            selected: 0,
            current,
            query: String::new(),
            loading: false,
            loaded: 0,
        };

        let widgets = view_output!();

        // Clicking anywhere outside the panel — the transparent fly-out areas
        // over the open window — closes the picker; clicks on tiles stay
        // inside, so they pass through to the grid.
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                backdrop_sender.input(WallpaperPickerInput::Hide);
            }
        });
        root.add_controller(click);

        model.filter("");
        model.highlight_current();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            WallpaperPickerInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show(&sender);
                }
            }
            WallpaperPickerInput::Hide => self.hide(),
            WallpaperPickerInput::Search(query) => self.filter(&query),
            WallpaperPickerInput::Move(delta) => self.move_selection(delta),
            WallpaperPickerInput::Activate => {
                if let Some(&index) = self.matches.get(self.selected) {
                    self.apply(index, &sender);
                }
            }
            WallpaperPickerInput::Activated(index) => self.apply(index, &sender),
            WallpaperPickerInput::SetCurrent(path) => self.set_current(path),
            WallpaperPickerInput::Thumb {
                index,
                width,
                height,
                rgba,
            } => self.set_thumb(index, width, height, rgba),
        }
    }
}

impl WallpaperPickerModel {
    /// Reset the query and slide the panel out of the bar.
    fn show(&mut self, sender: &ComponentSender<Self>) {
        self.search.set_text("");
        self.filter("");
        flyout::show(&self.window, &self.revealer, Some(self.search.upcast_ref()));
        self.start_loading(sender);
    }

    fn hide(&mut self) {
        flyout::hide(&self.revealer);
    }

    fn start_loading(&mut self, sender: &ComponentSender<Self>) {
        if self.loading || self.images.is_empty() {
            return;
        }
        self.loading = true;
        self.refresh_status();
        spawn_thumbnail_loader(self.images.clone(), sender.clone());
    }

    /// Recompute `matches` for `query` and toggle tile visibility.
    fn filter(&mut self, query: &str) {
        self.query = query.trim().to_string();
        let needle = self.query.to_lowercase();

        self.matches.clear();
        for (index, path) in self.images.iter().enumerate() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_lowercase();
            let visible = needle.is_empty() || name.contains(&needle);
            if let Some(tile) = self.tiles.get(index) {
                tile.child.set_visible(visible);
            }
            if visible {
                self.matches.push(index);
            }
        }

        self.selected = 0;
        self.update_selection();
        self.refresh_status();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.update_selection();
    }

    fn update_selection(&self) {
        if let Some(&index) = self.matches.get(self.selected)
            && let Some(tile) = self.tiles.get(index)
        {
            self.flow.select_child(&tile.child);
        }
    }

    /// Store a freshly decoded thumbnail into its tile.
    fn set_thumb(&mut self, index: usize, width: i32, height: i32, rgba: Vec<u8>) {
        if let Some(tile) = self.tiles.get(index) {
            let bytes = glib::Bytes::from_owned(rgba);
            let texture = gdk::MemoryTexture::new(
                width,
                height,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                (width * 4) as usize,
            );
            tile.picture.set_paintable(Some(&texture));
        }
        self.loaded += 1;
        self.refresh_status();
    }

    /// Mark `path` as the active wallpaper and emit it upwards.
    fn apply(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let Some(path) = self.images.get(index).cloned() else {
            return;
        };
        self.set_current(path.clone());
        let _ = sender.output(WallpaperOutput::Apply(path));
    }

    fn set_current(&mut self, path: PathBuf) {
        if let Some(previous) = self.current.replace(path.clone()) {
            self.mark(&previous, false);
        }
        self.mark(&path, true);
        // The panel header shows the current wallpaper too.
        flyout::set_banner(&self.banner, Some(&path));
    }

    /// Apply the remembered active wallpaper to the grid without emitting.
    fn highlight_current(&mut self) {
        if let Some(path) = self.current.clone() {
            self.mark(&path, true);
        }
    }

    fn mark(&self, path: &Path, current: bool) {
        if let Some(index) = self.images.iter().position(|image| image == path)
            && let Some(tile) = self.tiles.get(index)
        {
            if current {
                tile.child.add_css_class("current");
            } else {
                tile.child.remove_css_class("current");
            }
        }
    }

    fn refresh_status(&self) {
        if self.images.is_empty() {
            let dir = wallpaper_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "~/Pictures/Wallpapers".to_string());
            self.status.set_label(&format!("No wallpapers in {dir}"));
        } else if !self.query.is_empty() {
            self.status.set_label(&format!(
                "{} match{}",
                self.matches.len(),
                plural(self.matches.len())
            ));
        } else if self.loading && self.loaded < self.images.len() {
            self.status.set_label(&format!(
                "Loading previews… {}/{}",
                self.loaded,
                self.images.len()
            ));
        } else {
            self.status
                .set_label(&format!("{} wallpapers", self.images.len()));
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "es" }
}

/// A tile: a cover-fitted picture over its file name.
fn make_tile(path: &Path) -> (gtk::FlowBoxChild, gtk::Picture) {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(THUMB_WIDTH, THUMB_HEIGHT);

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let label = gtk::Label::new(Some(name));
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.set_max_width_chars(28);
    label.add_css_class("mango-picker-name");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(&picture);
    content.append(&label);

    let child = gtk::FlowBoxChild::new();
    child.add_css_class("mango-picker-tile");
    child.set_child(Some(&content));

    (child, picture)
}

// --- Thumbnail worker ----------------------------------------------------

/// A decoded thumbnail in a form that is safe to move across threads.
struct Thumb {
    width: i32,
    height: i32,
    rgba: Vec<u8>,
}

/// Decode `path` down to a small, tightly packed RGBA8 buffer.
///
/// `GdkPixbuf` is not `Send`, so nothing but plain bytes leaves this function.
/// It runs on the worker thread; only [`WallpaperPickerModel::set_thumb`],
/// which builds the `gdk::Texture`, touches GTK.
fn decode_thumbnail(path: &Path) -> Option<Thumb> {
    let pixbuf =
        gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, THUMB_WIDTH, THUMB_HEIGHT, true).ok()?;

    let width = pixbuf.width();
    let height = pixbuf.height();
    let channels = pixbuf.n_channels() as usize;
    let rowstride = pixbuf.rowstride() as usize;
    let pixels = pixbuf.read_pixel_bytes();
    let data: &[u8] = pixels.as_ref();

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let start = y * rowstride;
        let row = &data[start..start + width as usize * channels];
        match channels {
            // Already RGBA.
            4 => rgba.extend_from_slice(row),
            // RGB gets an opaque alpha byte.
            3 => {
                let (pixels, _remainder) = row.as_chunks::<3>();
                for pixel in pixels {
                    rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
            }
            _ => return None,
        }
    }

    Some(Thumb {
        width,
        height,
        rgba,
    })
}

/// Decode every wallpaper on a background thread, streaming thumbnails back.
///
/// The thread exits when the component goes away: `send` fails once the
/// receiver is dropped, which is checked after each image.
fn spawn_thumbnail_loader(images: Vec<PathBuf>, sender: ComponentSender<WallpaperPickerModel>) {
    let sender = sender.input_sender().clone();
    std::thread::spawn(move || {
        for (index, path) in images.iter().enumerate() {
            let Some(thumb) = decode_thumbnail(path) else {
                continue;
            };
            let message = WallpaperPickerInput::Thumb {
                index,
                width: thumb.width,
                height: thumb.height,
                rgba: thumb.rgba,
            };
            if sender.send(message).is_err() {
                return;
            }
        }
    });
}
