# mango-shell

A minimal Wayland **desktop shell / status bar** for the
[Mango compositor](https://github.com/mangowm/mango) (mangowm), written in Rust
with [Relm4](https://relm4.org) and GTK4.

It is a starting point, not a finished product, but the structure is the same
one a real shell uses. The architecture is modelled on
[**OkShell**](https://github.com/JohnOberhauser/OkShell), a mature Relm4
Wayland shell — see [How this maps to OkShell](#how-this-maps-to-okshell).

| Layer | Crate | Why |
| --- | --- | --- |
| Become a real bar/panel | [`gtk4-layer-shell`](https://github.com/wmww/gtk4-layer-shell) | `wlr-layer-shell` lets GTK windows anchor to screen edges and reserve space |
| Reactive UI | [`relm4`](https://github.com/Relm4/Relm4) | Elm-style model/update/view on top of GTK4 |
| Talk to Mango | [`mango-ipc`](https://crates.io/crates/mango-ipc) | Direct IPC over Mango's Unix socket (no `mmsg` process spawning) |
| System tray | [`system-tray`](https://crates.io/crates/system-tray) | Hosts `StatusNotifierItem` + `DBusMenu` on the session bus; runs on Tokio |

---

## Look & feel

The theme follows [**pctrade/end4-pC**](https://github.com/pctrade/end4-pC)
(the illogical-impulse Quickshell): a Material 3 / Material You dark surface,
the "hug" bar layout. Concretely:

- A full-width translucent **`layer 0`** surface with rounded bottom corners.
- Each cluster of widgets sits in its own **`layer 1`** pill, fully rounded:
  the launcher button, the workspace tags, the clock, and the status cluster.
- Workspace tags follow end4's workspaces: the number is always visible,
  **occupied** tags get a subtle `secondary container` pill, and the **active**
  tag is a filled `primary` pill with `on primary` text. `urgent` uses the
  error container.
- The right cluster holds the **system tray** plus **Wi-Fi, volume and
  brightness** readouts, each pill-shaped and hidden when it has nothing to
  show (no backlight, no tray items, and so on).
- The launcher, wallpaper picker and theme gallery are **fly-outs**: instead of
  a full-screen scrim they hang just below the bar and slide *and fade* down
  out of it with a small spring, floating over whatever window is open. Each is
  styled after the app
  launchers in the [HyDE project](https://github.com/HyDE-Project/HyDE) — a
  near-opaque rounded card with the current wallpaper pinned to the top, a
  pill search field and an accent-highlighted selection.
- The **wallpaper** is drawn by the shell itself — no `swaybg`/`swww`: one
  background surface per monitor, plus a fly-out **picker** holding a grid of
  thumbnails.
- A built-in **theme switcher**: a round button in the left cluster opens a
  popover of palettes (Nord, Catppuccin Mocha, Gruvbox, Tokyo Night, plus the
  built-in Mango), and its *Browse themes…* row opens the fly-out gallery.

Everything lives in [`src/style.css`](src/style.css) as plain GTK4 CSS with
Material 3 tokens declared up top (`@define-color layer0 …`, `@primary …`).

### Themes

Four hand-tuned colour schemes ship with the shell, each a small block of
`@define-color` overrides in [`src/themes/`](src/themes):

| Theme | id | Base palette |
| --- | --- | --- |
| Mango (default) | `mango` | the warm Material You tokens built into `style.css` |
| [Nord](https://www.nordtheme.com) | `nord` | polar night + frost |
| [Catppuccin](https://catppuccin.com) | `catppuccin` | Mocha (base `#1e1e2e`, mauve accent) |
| [Gruvbox](https://github.com/morhetz/gruvbox) | `gruvbox` | dark (retro-groove, yellow accent) |
| [Tokyo Night](https://github.com/folke/tokyonight.nvim) | `tokyonight` | night (bg `#1a1b26`, blue accent) |

Switch them with the **theme button** in the bar (next to the launcher and
wallpaper buttons): the popover applies a theme instantly and stays useful for
quick flips, while *Browse themes…* opens the fly-out gallery with a larger
palette preview per theme (arrow keys to move, Enter to apply, Escape/backdrop
to close). The gallery also recolours live as you click, so you can compare
against each palette before closing it. The choice is written to
`~/.config/mango-shell/theme` and restored on the next start.

> Adding a theme is just an extra `${id}.css` file in `src/themes/` plus one
> entry in [`src/theme.rs`](src/theme.rs).

### Panels

The bar's **layout** is configurable too. A *panel* is a ported polybar theme
(from [adi1090x/polybar-themes](https://github.com/adi1090x/polybar-themes)):
it decides the bar height, whether it floats or spans the full width, which
widget clusters (launcher buttons, workspace tags, clock, tray, indicators,
monitor name) sit on the left/centre/right, and whether they look like pills,
flat blocks or hairline-separated groups — plus a *default palette*. Panels
and themes compose: the active colour theme overrides the panel's palette, so
you can run, say, the Cuts layout recoloured in Gruvbox.

The round **panel button** in the bar (the grid icon, next to the wallpaper
button) opens the **panel gallery**: a fly-out of bar mocks, each card showing
a live preview of the layout (arrow keys to move, Enter to apply,
Escape/backdrop to close). The choice is written to
`~/.config/mango-shell/panel` and restored on the next start.

The shipped panels cover the distinct polybar layout families:

| Panel | Polybar original | Look |
| --- | --- | --- |
| Mango (hug) | — (built-in default) | full-width, rounded bottom, pill clusters |
| Material | `material` | floating fully-rounded bar, amber accent |
| Shades | `shades` | full-width flat bar, blue shades, hairlines |
| Cuts | `cuts` | thin sharp bar, yellow accent, block separators |
| Blocks | `blocks` / `colorblocks` / `grayblocks` / `forest` | light blocks on a dark bar, monitor name centred |
| Docky | `docky` | floating pill dock with a red underline |
| Hack | `hack` | thin terminal-style bar, cyan underline, tags centred |

> Adding a panel is a small block of CSS in `src/panels/<id>.css` (default
> palette + structural rules scoped to the `panel-<id>` class) plus one entry
> in [`src/panel.rs`](src/panel.rs) describing the slots, height and margins.

### Theming it to your wallpaper

end4 derives its palette from the wallpaper (via matugen). This shell does the
same thing in the simplest possible way: the stylesheet is layered as *base
`style.css` → selected theme → optional user override*, so an optional user
stylesheet at `~/.config/mango-shell/style.css` wins over everything. Generate
it however you like — for example a matugen template mapping the wallpaper's
Material 3 roles onto these tokens:

```css
/* ~/.config/mango-shell/style.css */
@define-color layer0            rgba(20, 19, 19, 0.86);
@define-color layer1            rgba(38, 37, 38, 0.72);
@define-color primary           #d0bcff;
@define-color on_primary        #381e72;
@define-color secondary_container #4a4458;
@define-color on_secondary_container #e8def8;
/* … */
```

Restart the shell (`SUPER+SHIFT,b` with the keybind below, or
`cargo install --path .` then re-exec) to pick it up.

> The full token list is at the top of `src/style.css`.

### Frosted-glass blur

end4's surfaces are blurred by the compositor. Mango exposes that too — in
`~/.config/mango/config.conf`:

```ini
blur=1
blur_layer=1
```

The bar itself is already translucent, so it picks up the blur automatically.

### Transitions & animation

Motion stays short and ease-out so it reads as *snappy* rather than busy. The
pieces live in one place, [`src/anim.rs`](src/anim.rs), which drives the
couple of transitions GTK can't do on its own with a lightweight timeout:

- **Fly-outs** (launcher, wallpaper, theme and panel pickers, shortcuts) slide
  down *and* fade: two nested `GtkRevealer`s — a slide with a crossfade inside
  it — then a small spring as the panel lands.
- **Bar popovers** (theme switcher, Wi-Fi, volume, brightness) fade in with a
  short rise into place.
- **Wallpapers** change through a randomly chosen transition from a wide set
  (`GtkStackTransitionType`: fades, slides, overs, unders and rotates) at a
  random duration, and roughly one change in six fades through black instead.
- **The wallpaper carousel** eases its glide with a slight overshoot (a subtle
  spring) and draws a soft glow behind the nearest cards.

Durations, offsets and easing constants sit at the top of `anim.rs`,
`flyout.rs`, `wallpaper.rs` and `carousel.rs`, so they are easy to tune or to
turn down.

---

## 1. Prerequisites (CachyOS / Arch)

```bash
# Compositor itself. CachyOS has `mangowm` in its repo, but the AUR `-git`
# package tracks upstream faster.
paru -S mangowm-git          # or: sudo pacman -S mangowm

# Build/runtime deps for this project
sudo pacman -S --needed rust gtk4 gtk4-layer-shell git

# Handy while developing
sudo pacman -S --needed foot rofi   # defaults used by Mango's keybinds
cargo install cargo-watch            # optional: auto-rebuild on save

# The status widgets read the usual system services (all present on a normal
# Mango session): PipeWire/PulseAudio (`pactl`), NetworkManager (`nmcli`) and
# systemd-logind (`busctl`). Only brightness is optional:
sudo pacman -S --needed brightnessctl   # optional; see "Status indicators" below
```

Check your toolchain:

```bash
rustc --version        # >= 1.93 is required by relm4 0.11
pkg-config --modversion gtk4
pkg-config --modversion gtk4-layer-shell
```

Then give Mango a config and start it:

```bash
mkdir -p ~/.config/mango
cp /etc/mango/config.conf ~/.config/mango/config.conf
# Log out and run `mango` from a TTY, or add a session entry so your display
# manager can launch it.
```

> Mango's IPC socket is only available **inside a Mango session**. Its path is
> exported as `MANGO_INSTANCE_SIGNATURE`; `mango-ipc` reads it automatically.
> Outside Mango the bar still runs, it just shows no tags.

---

## 2. Build & run

```bash
cargo run            # development build
cargo run --release  # smaller/faster
```

To autostart the bar with the compositor, add it to your Mango config
(`~/.config/mango/config.conf`) — either directly:

```ini
exec_once=mango-shell
```

or in `~/.config/mango/autostart.sh` (which the default config calls via
`exec_once=~/.config/mango/autostart.sh`).

Install it somewhere on `$PATH`:

```bash
cargo install --path .
```

### Application launcher

The shell includes an app launcher: an `Overlay`-layer **fly-out** that hangs
just below the bar (no full-screen scrim) and slides down out of it, so it
floats over the window you are working in. It enumerates every visible
`.desktop` entry through `gio::AppInfo`, filters as you type, and launches the
highlighted one on Enter or click. The card follows HyDE's launchers: the
current wallpaper on top, a pill search field and accent-highlighted rows.

Open it either with the round **launcher button** at the left end of the bar,
or from a keybind in `~/.config/mango/config.conf`:

```ini
bind=SUPER,d,spawn,pkill -SIGUSR1 mango-shell
```

`SIGUSR1` toggles the launcher inside the already-running shell (see
`spawn_signal_watcher` in `src/main.rs`), so there is only ever one
`mango-shell` process. `Escape`, or a click anywhere outside the panel,
closes it. Nothing is spawned at startup; the surface is created hidden and
only presented on demand.

### Wallpaper

The shell draws the desktop background itself — no `swaybg`, `swww` or any
other daemon. Each monitor gets one `Background`-layer surface, anchored to all
four edges with an exclusive zone of `-1` so it fills the screen and stretches
behind the top panel; it holds a `GtkPicture` fitted to **cover**. A single
`Overlay`-layer **fly-out picker** hangs below the bar and slides out of it
(same recipe as the launcher), showing a thumbnail grid of the images in
`~/Pictures/Wallpapers` (override the folder with the `MANGO_WALLPAPER_DIR`
environment variable).

Open the picker with the round **wallpaper button** next to the launcher
button, or from a keybind:

```ini
bind=SUPER,w,spawn,pkill -SIGUSR2 mango-shell
```

Click a thumbnail (or press Enter) to apply it — the picker closes on
selection so the new wallpaper is revealed at once; type to filter by file
name and arrow keys move the highlight. `Escape`, a click outside the panel,
or the toggle again closes it without choosing. The choice is written to
`~/.config/mango-shell/wallpaper` and restored the next time the shell starts.

Previews are decoded on a background thread. `GdkPixbuf` is not `Send` and
`gdk::MemoryTexture` may only be built on the main thread, so each image is
downscaled to a tiny RGBA buffer (plain `Vec<u8>`, which *is* `Send`) that the
main thread wraps in a texture. Nothing blocks the bar.

#### Theming from the wallpaper

If you create an executable `~/.config/mango-shell/on-wallpaper-change.sh`, the
shell runs it with the new wallpaper path every time you pick one, then reloads
`~/.config/mango-shell/style.css` (see
[Theming it to your wallpaper](#theming-it-to-your-wallpaper)). Point it at
[matugen](https://github.com/InioX/matugen) or pywal and the shell recolours
with the wallpaper:

```bash
#!/usr/bin/env bash
# ~/.config/mango-shell/on-wallpaper-change.sh
# (template matugen to write ~/.config/mango-shell/style.css)
matugen image "$1"
```

It is a no-op when the script is absent.

### Status indicators & system tray

The right-hand cluster is made of small widgets, each polling a backend on a
background thread so a slow tool never freezes the bar:

| Widget | Backend | Interaction |
| --- | --- | --- |
| **Wi-Fi** | NetworkManager (`nmcli`) | click opens a popover: radio on/off, refresh, network list, connect (password prompt); scroll is free |
| **Volume** | PipeWire / PulseAudio (`pactl`) | click opens a popover: slider, mute and output-device picker; scroll ±5 % |
| **Brightness** | `/sys/class/backlight` | click opens a popover with a slider; scroll ±5 % (see below) |
| **Tray** | `org.kde.StatusNotifierItem` over D-Bus | left = activate, middle = secondary, right = menu |

Clicking **Wi-Fi** shows every visible network, strongest first with the active
one on top. Picking an open network connects straight away; a secured one opens
an inline password prompt (leave it empty to reuse saved credentials). Errors
reported by `nmcli` are shown under the header.

Clicking **Volume** shows a slider, a mute button and the list of output
devices (sinks); picking one makes it the default output. The bar icon and
percentage update live while you drag.


The **system tray** widget *hosts* `org.kde.StatusNotifierWatcher` itself, so
no other tray host is needed — any app that publishes a `StatusNotifierItem`
(Discord, Steam, Telegram, …) shows up as an icon, and its `DBusMenu` is
rendered into a GTK popover. Icons come from the icon theme when the item names
one, and are otherwise decoded from the raw `IconPixmap`.

Adjusting the backlight is normally the awkward part, because the sysfs file is
root-owned. Both the slider and scrolling try, in order:

1. writing `/sys/class/backlight/<dev>/brightness` (works with a udev rule),
2. `brightnessctl set` (if installed — `sudo pacman -S brightnessctl`),
3. logind's `SetBrightness` over D-Bus (works for the active session with no
   extra packages or permissions).

The other backends (`pactl`, `nmcli`, `busctl`) ship with PipeWire/PulseAudio,
NetworkManager and systemd, which a Mango session already has.

---

## 3. Project layout

```text
src/
  main.rs           Shell: one invisible 1x1 layer surface. Watches monitors,
                    owns the Mango IPC feed, spawns one bar + wallpaper surface
                    per monitor, and loads the stylesheet (defaults + panel +
                    theme + optional user override, reloaded when the theme,
                    panel or wallpaper changes).
  bar.rs            BarModel: one Top-layer layer surface per monitor. Builds
                    the widget clusters and distributes them into the three
                    CentreBox slots according to the active panel (src/panel.rs).
                    Its buttons forward launcher/wallpaper/theme/panel toggles
                    to the shell.
  anim.rs           Shared tweening: fade-in, a landing spring, and the popover
                    open animation. GTK4 can't scale widgets from CSS, so these
                    short ease-out moves are driven with a timeout.
  flyout.rs         Shared recipe for the launcher/wallpaper/theme "fly-outs":
                    an Overlay-layer panel that hangs under the bar and slides
                    + fades out of it (a Flyout: nested GtkRevealers with a
                    landing spring), plus the wallpaper banner header.
  launcher.rs       LauncherModel: an Overlay-layer fly-out panel. Searches
                    .desktop entries with gio::AppInfo; toggled via SIGUSR1 or
                    the bar button.
  wallpaper.rs      WallpaperModel: one Background-layer surface per monitor
                    showing the current image (cover, full-screen behind the
                    bar). WallpaperPickerModel: a fly-out thumbnail grid whose
                    previews decode on a worker thread; toggled via SIGUSR2 or
                    the bar button.
  theme.rs          Built-in themes (Theme/THEMES), persistence
                    (~/.config/mango-shell/theme) and ThemePickerModel, a
                    fly-out gallery of palette cards opened from the bar's
                    theme menu.
  panel.rs          Panels (Panel/PANELS): bar-layout profiles ported from
                    adi1090x/polybar-themes, persistence
                    (~/.config/mango-shell/panel) and PanelPickerModel, a
                    fly-out gallery of bar-layout mocks opened from the bar's
                    panel button.
  mango.rs          Mango IPC: TagView, monitor stream, tag dispatch.
  themes/           `@define-color` override blocks per theme; the shell
                    splices the active one between the panel CSS and the user
                    override.
  panels/           `@define-color` palette + structural CSS per panel,
                    scoped to the `panel-<id>` class on the bar window; the
                    active one is spliced between style.css and the theme.
  widgets/
    clock.rs        ClockModel: date • time, 1 Hz timer.
    tags.rs         TagsModel: clickable tag buttons (occupied/active/urgent).
    theme.rs        ThemeButtonModel: the bar's round theme button with its
                    popover of palettes (live cairo previews + checkmarks).
    volume.rs       VolumeModel: pactl-backed volume/mute + output picker.
    wifi.rs         WifiModel: nmcli-backed radio, network list + connect.
    brightness.rs   BrightnessModel: sysfs backlight, slider + scroll.
    tray.rs         TrayModel: StatusNotifierItem host + DBusMenu popover.
  style.css         Base Material 3 theme (shapes + default tokens), loaded
                    into a custom CssProvider so it can be reloaded live. The
                    selected panel's CSS, theme's tokens and any user
                    stylesheet are appended after it, in that order of
                    precedence.
```

### The two things that actually matter

**1. Layer shells must be configured before realize.** Relm4 builds the root
window during GTK's `startup` phase and only shows it on `activate`, so doing
this inside `init()` is guaranteed to be early enough:

```rust
root.init_layer_shell();
root.set_monitor(Some(&monitor));   // which output this surface belongs to
root.set_layer(Layer::Top);
root.auto_exclusive_zone_enable();  // reserve the bar's height
root.set_anchor(Edge::Top, true);
root.set_anchor(Edge::Left, true);
root.set_anchor(Edge::Right, true);
root.set_visible(true);
```

If the bar ever renders as a normal floating window, this ordering is the first
thing to check.

**2. You need an invisible root window.** A GTK application needs at least one
window to stay alive, and a layer surface belongs to exactly one monitor, so
the shell itself cannot be a layer surface. `Shell` therefore creates a 1×1
transparent `Background` surface and keeps it hidden. OkShell does the same
(`okshell-invisible-root`).

---

## 4. How this maps to OkShell

[OkShell](https://github.com/JohnOberhauser/OkShell) is the reference for the
structure here. The relevant pieces:

| This project | OkShell | Notes |
| --- | --- | --- |
| `Shell` in `src/main.rs` | `Shell` in `crates/okshell-core/src/relm_app.rs` | Invisible 1×1 layer-shell root; owns per-monitor window groups and coordinates services |
| `setup_monitor_watcher` / `sync_monitors` | `crates/okshell-core/src/monitors.rs` | `gdk::Display::monitors()` + `items-changed`; one component per connector |
| `BarModel` in `src/bar.rs` | `Frame` in `crates/okshell-frame/src/frame.rs` | Layer-shell config lives here (`init_layer_shell`, `set_monitor`, `set_layer`, anchors). OkShell's `Frame` is fullscreen and hosts top/bottom/left/right `Bar`s; this starter collapses that into a single top bar |
| `widgets/` | `crates/okshell-frame/src/bars/bar_widgets/` | One `SimpleComponent` per widget |
| `widgets/clock.rs` | `bar_widgets/clock.rs` | Same idea: a `glib` timeout whose `SourceId` is removed in `shutdown()` |
| `widgets/tags.rs` | `bar_widgets/hyprland_workspaces.rs` | Per-monitor workspace/tag buttons. OkShell uses a `DynamicBox` factory; this starter rebuilds a plain `gtk::Box` since there are only a handful of tags |
| `widgets/volume.rs`, `widgets/wifi.rs`, `widgets/brightness.rs` | `bar_widgets/audio.rs`, `bar_widgets/network.rs`, `bar_widgets/battery.rs` | Small poll-and-display widgets; each runs its backend on a background thread and reports back with `ComponentSender::input` |
| `widgets/tray.rs` | `bar_widgets/sys_tray.rs` | `StatusNotifierItem` host + `DBusMenu`. OkShell uses its own D-Bus layer; this uses the `system-tray` crate on a dedicated Tokio runtime thread |
| `mango.rs` | `crates/okshell-core/src/ipc.rs` | OkShell talks to Hyprland via `wayle`; here `mango-ipc` plays the same role |

Notable differences, and why:

- **Children are wired with `append = &child.widget().clone() {}`**, the same
  trick OkShell uses in `frame.rs` to drop child-component widgets into a view.
- **No config system or service layer yet.** OkShell has `okshell-config`
  (reactive `reactive_graph` stores) and `okshell-services`. That is the
  natural next step, but it is a lot of machinery for a starter. Theming and
  layout are covered more simply: built-in `@define-color` themes, ported
  panel layouts, plus an optional user stylesheet, all spliced into one
  `CssProvider` (see `src/theme.rs` and `src/panel.rs`).

---

## 5. Next steps / ideas

- **Configure it.** Add a config file (TOML) to tune the theme/panel list, bar
  height and behaviour, instead of hardcoding them.
- **More themes.** Add a `${id}.css` in `src/themes/` plus a `THEMES` entry,
  or wire up `matugen`/pywal palettes (the optional user stylesheet already
  layers on top of any theme).
- **More panels.** Add a `${id}.css` in `src/panels/` plus a `PANELS` entry to
  port another polybar layout (the remaining `simple/` themes are `shapes`,
  `forest`, `panels` and `pwidgets`; `panels` and `pwidgets` need extra
  widget types before they map cleanly onto a single top bar).
- **More modules.** Battery (UPower), media player (MPRIS), notifications,
  keyboard layout, a power/session menu. (Wi-Fi, volume, brightness and the
  system tray are already in `widgets/`.)
- **Launcher polish.** The launcher matches by substring; a fuzzy scorer,
  MRU/recency ordering, and launching `.desktop` actions would make it feel
  closer to rofi/fuzzel.
- **Popups.** Layer-shell popovers/context menus work out of the box with GTK
  popovers anchored to a bar widget.
- **Icons.** [`relm4-icons`](https://github.com/Relm4/icons) or Nerd Font
  glyphs in labels.
- **Read the references:**
  - [OkShell](https://github.com/JohnOberhauser/OkShell) — architecture,
    `crates/okshell-core/src/{relm_app,monitors}.rs` and
    `crates/okshell-frame/src/{frame.rs,bars/}` in particular.
  - [Relm4 examples](https://github.com/Relm4/Relm4/tree/main/examples) —
    `simple_manual.rs`, `macro_reference.rs`, `factory.rs`, `worker.rs`.
  - Mango docs: <https://mangowm.github.io/docs> (especially the *IPC* page).

---

## 6. Push it to GitHub

A repo is already initialized with an initial commit. To publish it:

```bash
# Create the repo on GitHub and add it as `origin` in one go:
gh repo create mango-shell --public --source=. --remote=origin --push

# ...or, if you prefer to create it in the browser first:
git remote add origin git@github.com:<your-username>/mango-shell.git
git branch -M main
git push -u origin main
```

Then uncomment the `repository` line in `Cargo.toml` and commit it.

## License

MIT — see [LICENSE](LICENSE).
