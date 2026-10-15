//! Keyboard shortcut reference: a read-only fly-out that lists the
//! compositor's `bind=` lines.
//!
//! Unlike the launcher and the pickers there is nothing to select here, so the
//! applet is a searchable, sectioned list. The bindings are re-parsed from
//! Mango's config (`~/.config/mango/config.conf`, or `$MANGO_CONFIG`) every
//! time it opens, so edits show up without restarting the shell.
//!
//! The parser is deliberately tolerant: it follows `bind=` lines in file
//! order, keeps the `# comments` that Mango configs use as section headings,
//! and skips anything it does not understand rather than failing.

use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;

use crate::flyout;

// --- Parsing --------------------------------------------------------------

/// One `bind=` line, split into the pieces the list renders.
struct Shortcut {
    /// Modifier keycaps, canonicalised (`SUPER` → `Super`), in bind order.
    mods: Vec<String>,
    /// The trigger key, prettified (`Return` → `Enter`).
    key: String,
    /// Human-readable description of the action.
    description: String,
    /// Lowercased everything, for the search box.
    haystack: String,
}

/// A run of binds under one `# comment` heading.
struct Section {
    title: Option<String>,
    items: Vec<Shortcut>,
}

/// Where Mango keeps its config, honouring `$MANGO_CONFIG` and
/// `$XDG_CONFIG_HOME` before the usual `~/.config`.
fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MANGO_CONFIG") {
        return Some(PathBuf::from(path));
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("mango/config.conf"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/mango/config.conf"))
}

/// Read and parse `path`; a missing or unreadable file yields no sections.
fn read_sections(path: &Path) -> Vec<Section> {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Split a config into sections of binds, keeping `# comment` headings.
fn parse(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut pending_title: Option<String> = None;
    // A comment is only a *heading* when it starts a block (a blank line above
    // it); the notes Mango configs interleave between binds are ignored.
    let mut prev_blank = true;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            prev_blank = true;
            continue;
        }

        if let Some(comment) = trimmed.strip_prefix('#') {
            if prev_blank {
                let heading = comment.trim();
                if !heading.is_empty() {
                    pending_title = Some(heading.to_string());
                }
            }
            prev_blank = false;
            continue;
        }

        prev_blank = false;

        let Some((kind, value)) = trimmed.split_once('=') else {
            continue;
        };
        // `bind=`, and the flag variants `bindl=` / `bindr=` / `bindp=` / …
        if !kind.trim().starts_with("bind") {
            continue;
        }
        let Some(shortcut) = parse_bind(value) else {
            continue;
        };

        // A pending heading (or the very first bind) starts a new section;
        // otherwise the bind joins the current one.
        if pending_title.is_some() || sections.is_empty() {
            sections.push(Section {
                title: pending_title.take(),
                items: Vec::new(),
            });
        }
        if let Some(section) = sections.last_mut() {
            section.items.push(shortcut);
        }
    }

    sections
}

/// Turn the `MODS,KEY,ACTION,PARAMS` tail of a `bind=` line into a row.
fn parse_bind(value: &str) -> Option<Shortcut> {
    let mut parts = value.splitn(4, ',');
    let mods_raw = parts.next()?.trim();
    let key_raw = parts.next()?.trim();
    let action = parts.next().unwrap_or("").trim();
    let params = parts.next().unwrap_or("").trim();

    if key_raw.is_empty() || action.is_empty() {
        return None;
    }

    let mods = parse_mods(mods_raw);
    let key = pretty_key(key_raw);
    let description = describe(action, params);

    let mut haystack = String::new();
    for word in mods.iter().map(String::as_str).chain([
        key.as_str(),
        description.as_str(),
        action,
        params,
    ]) {
        haystack.push_str(&word.to_lowercase());
        haystack.push(' ');
    }

    Some(Shortcut {
        mods,
        key,
        description,
        haystack,
    })
}

/// Split and canonicalise a modifier list (`SUPER+SHIFT+ctrl`).
fn parse_mods(raw: &str) -> Vec<String> {
    raw.split('+')
        .map(str::trim)
        .filter(|modifier| !modifier.is_empty() && !modifier.eq_ignore_ascii_case("none"))
        .map(|modifier| match modifier.to_ascii_lowercase().as_str() {
            "super" | "mod4" => "Super".to_string(),
            "ctrl" | "control" => "Ctrl".to_string(),
            "alt" | "mod1" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            _ => capitalize(modifier),
        })
        .collect()
}

/// Uppercase the first letter, leaving the rest alone (`shift_l` → `Shift_l`).
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Prettify a key name for its keycap.
fn pretty_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "return" | "kp_enter" | "enter" => "Enter".to_string(),
        "escape" | "esc" => "Esc".to_string(),
        "space" => "Space".to_string(),
        "tab" => "Tab".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "backslash" => "\\".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "slash" => "/".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "grave" | "backtick" => "`".to_string(),
        "semicolon" => ";".to_string(),
        "apostrophe" => "'".to_string(),
        _ if key.chars().count() == 1 => key.to_uppercase(),
        _ => key.to_string(),
    }
}

/// A friendly description of a Mango action and its parameters.
fn describe(action: &str, params: &str) -> String {
    let args: Vec<&str> = if params.is_empty() {
        Vec::new()
    } else {
        params.split(',').map(str::trim).collect()
    };
    let arg = args.first().copied().unwrap_or("");

    match action {
        "spawn" | "spawn_shell" => format!("Run {params}"),
        "spawn_on_empty" => format!("Open on empty tag: {arg}"),
        "reload_config" => "Reload config".to_string(),
        "quit" => "Quit Mango".to_string(),
        "killclient" if arg.is_empty() => "Close window".to_string(),
        "killclient" => "Force close window".to_string(),
        "toggleoverview" => "Toggle overview".to_string(),
        "enteroverview" => "Enter overview".to_string(),
        "leaveoverview" => "Leave overview".to_string(),
        "togglejump" => "Overview: jump mode".to_string(),
        "togglefloating" => "Toggle floating".to_string(),
        "toggle_all_floating" => "Toggle all floating".to_string(),
        "togglefullscreen" => "Toggle fullscreen".to_string(),
        "togglefakefullscreen" => "Toggle fake fullscreen".to_string(),
        "togglemaximizescreen" | "togglemaxmizescreen" => "Toggle maximize".to_string(),
        "toggleoverlay" => "Toggle overlay".to_string(),
        "toggle_render_border" => "Toggle window border".to_string(),
        "toggleglobal" => "Pin to all tags".to_string(),
        "centerwin" => "Center window".to_string(),
        "minimized" => "Minimize to scratchpad".to_string(),
        "restore_minimized" => "Restore minimized".to_string(),
        "toggle_scratchpad" => "Toggle scratchpad".to_string(),
        "toggle_named_scratchpad" => "Toggle named scratchpad".to_string(),
        "toggle_special_tag" => "Toggle special tag".to_string(),
        "tag_special_tag" => "Move to special tag".to_string(),
        "tag_special_silent" => "Send to special tag".to_string(),
        "focusid" => "Focus window".to_string(),
        "focusdir" => format!("Focus {}", direction(arg)),
        "focusstack" => match arg {
            "next" => "Focus next window".to_string(),
            "prev" => "Focus previous window".to_string(),
            "all_next" | "all_tag_next" => "Focus next window (all tags)".to_string(),
            "all_prev" | "all_tag_prev" => "Focus previous window (all tags)".to_string(),
            _ => "Focus window".to_string(),
        },
        "exchange_client" => format!("Swap window {}", direction(arg)),
        "move_client" => format!("Move window {}", direction(arg)),
        "view" => view_label(&args),
        "viewtoleft" => "Previous tag".to_string(),
        "viewtoright" => "Next tag".to_string(),
        "viewtoleft_have_client" | "viewprev_have_client" => "Previous tag with window".to_string(),
        "viewtoright_have_client" | "viewnext_have_client" => "Next tag with window".to_string(),
        "tag" => format!("Move window to tag {arg}"),
        "tagsilent" => format!("Send window to tag {arg}"),
        "tagtoleft" => "Move tag left".to_string(),
        "tagtoright" => "Move tag right".to_string(),
        "focusmon" => format!("Focus {} monitor", direction(arg)),
        "tagmon" => format!("Send to {} monitor", direction(arg)),
        "incgaps" => format!("Adjust gaps by {arg}"),
        "togglegaps" => "Toggle gaps".to_string(),
        "movewin" => format!("Move window {}", args.join(" ")),
        "resizewin" => format!("Resize window {}", args.join(" ")),
        "set_proportion" => format!("Set proportion {arg}"),
        "switch_proportion_preset" => "Cycle proportion".to_string(),
        "switch_layout" => "Switch layout".to_string(),
        "scroller_stack" => format!("Stack {}", direction(arg)),
        "dwindle_toggle_split_direction" => "Toggle split direction".to_string(),
        "setkeymode" => format!("Enter {arg} mode"),
        "switch_keyboard_layout" => "Switch keyboard layout".to_string(),
        _ => {
            let mut label = action.replace('_', " ");
            if !params.is_empty() {
                label.push(' ');
                label.push_str(&params.replace(',', " "));
            }
            label
        }
    }
}

/// `View tag 3` / `View all tags` for a `view` bind.
fn view_label(args: &[&str]) -> String {
    match args.first() {
        None => "View tag".to_string(),
        Some(&"0") => "View all tags".to_string(),
        Some(&"-1") => "View previous tagset".to_string(),
        Some(mask) if mask.contains('|') => format!("View tags {mask}"),
        Some(tag) => format!("View tag {tag}"),
    }
}

/// Keep direction words lowercase, otherwise pass the argument through.
fn direction(arg: &str) -> String {
    match arg.to_ascii_lowercase().as_str() {
        "left" | "right" | "up" | "down" => arg.to_ascii_lowercase(),
        _ => arg.to_string(),
    }
}

// --- Fly-out applet -------------------------------------------------------

pub struct ShortcutsModel {
    window: gtk::Window,
    /// Slides + fades the panel out from under the bar.
    flyout: flyout::Flyout,
    /// The wallpaper snapshot in the panel header.
    banner: flyout::Banner,
    search: gtk::SearchEntry,
    status: gtk::Label,
    /// The scrollable stack of section headings and shortcut rows.
    list: gtk::Box,
    /// The config this was parsed from, for the "not found" message.
    source: Option<PathBuf>,
    /// Parsed every time the applet opens, so edits are picked up.
    sections: Vec<Section>,
}

#[derive(Debug)]
pub enum ShortcutsInput {
    /// Flip visibility (sent by the shell from the bar button).
    Toggle,
    /// Hide (Escape, backdrop click).
    Hide,
    /// The query text changed.
    Search(String),
    /// The wallpaper changed; repaint the panel header.
    Banner(Option<PathBuf>),
}

#[relm4::component(pub)]
impl SimpleComponent for ShortcutsModel {
    type Init = Option<PathBuf>;
    type Input = ShortcutsInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-flyout", "mango-shortcuts"],
            set_decorated: false,

            #[wrap(Some)]
            set_child = &model.flyout.widget() {},
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Fly-out surface: hugs the top of the monitor just below the bar,
        // exclusive keyboard while open, hidden until toggled.
        flyout::prepare_layer(&root, "mango-shortcuts");

        let banner = flyout::make_banner();
        flyout::set_banner(&banner, init.as_deref());

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search shortcuts…"));
        search.set_hexpand(true);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_halign(gtk::Align::Start);
        status.add_css_class("mango-shortcut-status");

        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("mango-shortcut-list");

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_max_content_height(460);
        scrolled.set_propagate_natural_height(true);
        scrolled.set_child(Some(&list));

        // The panel: wallpaper banner, then the search pill and the list.
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
        panel.add_css_class("mango-shortcuts-panel");
        panel.add_css_class("mango-flyout-panel");
        panel.append(&banner.widget);
        panel.append(&search);
        panel.append(&status);
        panel.append(&scrolled);

        let flyout_panel = flyout::Flyout::new(&root);
        flyout_panel.set_child(&panel);

        // Re-filter on every text change (immediate, unlike `search-changed`).
        let search_sender = sender.clone();
        search.connect_changed(move |entry| {
            search_sender.input(ShortcutsInput::Search(entry.text().to_string()));
        });

        // Capture phase, so Escape is seen before the search entry.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let key_sender = sender.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                key_sender.input(ShortcutsInput::Hide);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(keys);

        let source = config_path();
        let sections = source.as_deref().map(read_sections).unwrap_or_default();

        let model = ShortcutsModel {
            window: root.clone(),
            flyout: flyout_panel,
            banner,
            search,
            status,
            list,
            source,
            sections,
        };

        let widgets = view_output!();

        // Clicking anywhere outside the panel closes it.
        let click_root = root.clone();
        let click = gtk::GestureClick::new();
        let hide_sender = sender.clone();
        click.connect_pressed(move |_, _, x, y| {
            if !flyout::click_inside(&panel, &click_root, x, y) {
                hide_sender.input(ShortcutsInput::Hide);
            }
        });
        root.add_controller(click);

        model.render("");

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            ShortcutsInput::Toggle => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show();
                }
            }
            ShortcutsInput::Hide => self.hide(),
            ShortcutsInput::Search(query) => self.render(&query),
            ShortcutsInput::Banner(path) => flyout::set_banner(&self.banner, path.as_deref()),
        }
    }
}

impl ShortcutsModel {
    /// Re-read the config, reset the query and slide the panel out.
    fn show(&mut self) {
        self.search.set_text("");
        self.sections = self.source.as_deref().map(read_sections).unwrap_or_default();
        self.render("");
        self.flyout.show(Some(self.search.upcast_ref()));
    }

    fn hide(&mut self) {
        self.flyout.hide();
    }

    /// Rebuild the list for `query`, dropping rows (and empty sections) that
    /// do not match.
    fn render(&self, query: &str) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let needle = query.trim().to_lowercase();
        let mut shown = 0usize;
        for section in &self.sections {
            let matches: Vec<&Shortcut> = section
                .items
                .iter()
                .filter(|shortcut| needle.is_empty() || shortcut.haystack.contains(&needle))
                .collect();
            if matches.is_empty() {
                continue;
            }
            if let Some(title) = &section.title {
                let heading = gtk::Label::new(Some(title));
                heading.add_css_class("mango-shortcut-section");
                heading.set_xalign(0.0);
                heading.set_halign(gtk::Align::Start);
                self.list.append(&heading);
            }
            for shortcut in &matches {
                self.list.append(&make_row(shortcut));
            }
            shown += matches.len();
        }

        self.refresh_status(shown, needle.is_empty());
    }

    fn refresh_status(&self, shown: usize, unfiltered: bool) {
        let total: usize = self.sections.iter().map(|section| section.items.len()).sum();
        if total == 0 {
            let where_ = self
                .source
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "~/.config/mango/config.conf".to_string());
            self.status
                .set_label(&format!("No key bindings found in {where_}"));
        } else if unfiltered {
            self.status
                .set_label(&format!("{total} shortcut{}", plural(total)));
        } else {
            self.status
                .set_label(&format!("{shown} of {total} shortcuts"));
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One row: the keycaps on the left, the description filling the rest.
fn make_row(shortcut: &Shortcut) -> gtk::Box {
    let keys = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    keys.add_css_class("mango-shortcut-keys");
    keys.set_halign(gtk::Align::Start);
    keys.set_valign(gtk::Align::Center);
    for modifier in &shortcut.mods {
        keys.append(&keycap(modifier));
    }
    keys.append(&keycap(&shortcut.key));

    let label = gtk::Label::new(Some(&shortcut.description));
    label.add_css_class("mango-shortcut-label");
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("mango-shortcut-row");
    row.append(&keys);
    row.append(&label);
    row
}

/// A single key or modifier, drawn as a keycap.
fn keycap(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("mango-keycap");
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_and_binds() {
        let config = "\
# Key Bindings

# reload config
bind=SUPER,r,reload_config

# menu and terminal
# a note that should be ignored
bind=SUPER,d,spawn,pkill -SIGUSR1 mango-shell
bind=Alt,Return,spawn,foot
";
        let sections = parse(config);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title.as_deref(), Some("reload config"));
        assert_eq!(sections[0].items.len(), 1);
        assert_eq!(sections[1].title.as_deref(), Some("menu and terminal"));
        assert_eq!(sections[1].items.len(), 2);

        // `spawn` keeps the whole command, commas and all.
        let menu = &sections[1].items[0];
        assert_eq!(menu.mods, vec!["Super"]);
        assert_eq!(menu.key, "D");
        assert_eq!(menu.description, "Run pkill -SIGUSR1 mango-shell");

        let term = &sections[1].items[1];
        assert_eq!(term.mods, vec!["Alt"]);
        assert_eq!(term.key, "Enter");
    }

    #[test]
    fn parses_combined_mods_and_skips_unknowns() {
        let sections = parse(
            "blur=1\nbind=CTRL+SUPER,Left,tagtoleft,0\nmousebind=SUPER,btn_left,moveresize,curmove\n",
        );

        assert_eq!(sections.len(), 1);
        let shortcut = &sections[0].items[0];
        assert_eq!(shortcut.mods, vec!["Ctrl", "Super"]);
        assert_eq!(shortcut.key, "←");
        assert_eq!(shortcut.description, "Move tag left");
    }

    #[test]
    fn describes_common_actions() {
        assert_eq!(describe("killclient", ""), "Close window");
        assert_eq!(describe("view", "3,0"), "View tag 3");
        assert_eq!(describe("view", "0"), "View all tags");
        assert_eq!(describe("focusdir", "left"), "Focus left");
        assert_eq!(describe("tag", "2,0"), "Move window to tag 2");
        assert_eq!(describe("whatever_action", "a,b"), "whatever action a b");
    }
}
