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
- The launcher is a full-screen scrim with a centred Material 3 panel, a
  pill-shaped search field and rounded, highlighted result rows.
- The **wallpaper** is drawn by the shell itself — no `swaybg`/`swww`: one
  background surface per monitor, plus an overlay **picker** holding a grid of
  thumbnails.

Everything lives in [`src/style.css`](src/style.css) as plain GTK4 CSS with
Material 3 tokens declared up top (`@define-color layer0 …`, `@primary …`).

### Theming it to your wallpaper

end4 derives its palette from the wallpaper (via matugen). This shell does the
same thing in the simplest possible way: it appends an optional user stylesheet
at `~/.config/mango-shell/style.css` **after** the built-in one, so any
`@define-color` you put there wins. Generate it however you like — for example
a matugen template mapping the wallpaper's Material 3 roles onto these tokens:

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
# Mango session): PipeWire/WirePlumber (`wpctl`), NetworkManager (`nmcli`) and
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

The shell includes an app launcher: a full-screen `Overlay`-layer surface with
a dimmed backdrop and a centred Material 3 panel. It enumerates every visible
`.desktop` entry through `gio::AppInfo`, filters as you type, and launches the
highlighted one on Enter or click.

Open it either with the round **launcher button** at the left end of the bar,
or from a keybind in `~/.config/mango/config.conf`:

```ini
bind=SUPER,d,spawn,pkill -SIGUSR1 mango-shell
```

`SIGUSR1` toggles the launcher inside the already-running shell (see
`spawn_signal_watcher` in `src/main.rs`), so there is only ever one
`mango-shell` process. `Escape`, or a click on the backdrop outside the panel,
closes it. Nothing is spawned at startup; the surface is created hidden and
only presented on demand.

### Wallpaper

The shell draws the desktop background itself — no `swaybg`, `swww` or any
other daemon. Each monitor gets one `Background`-layer surface holding a
`GtkPicture` fitted to **cover**, and a single `Overlay`-layer **picker** shows
a thumbnail grid of the images in `~/Pictures/Wallpapers` (override the folder
with the `MANGO_WALLPAPER_DIR` environment variable).

Open the picker with the round **wallpaper button** next to the launcher
button, or from a keybind:

```ini
bind=SUPER,w,spawn,pkill -SIGUSR2 mango-shell
```

Click a thumbnail to apply it live — the panel stays open so you can flip
through them; type to filter by file name, arrow keys move the highlight and
Enter applies it. `Escape`, a click on the backdrop, or the toggle again closes
it. The choice is written to `~/.config/mango-shell/wallpaper` and restored the
next time the shell starts.

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
| **Wi-Fi** | NetworkManager (`nmcli`) | tooltip shows the SSID + signal; read-only |
| **Volume** | PipeWire / WirePlumber (`wpctl`) | click toggles mute, scroll ±5 % |
| **Brightness** | `/sys/class/backlight` | scroll ±5 % (see below) |
| **Tray** | `org.kde.StatusNotifierItem` over D-Bus | left = activate, middle = secondary, right = menu |

The **system tray** widget *hosts* `org.kde.StatusNotifierWatcher` itself, so
no other tray host is needed — any app that publishes a `StatusNotifierItem`
(Discord, Steam, Telegram, …) shows up as an icon, and its `DBusMenu` is
rendered into a GTK popover. Icons come from the icon theme when the item names
one, and are otherwise decoded from the raw `IconPixmap`.

Adjusting the backlight is normally the awkward part, because the sysfs file is
root-owned. Scrolling tries, in order:

1. writing `/sys/class/backlight/<dev>/brightness` (works with a udev rule),
2. `brightnessctl set` (if installed — `sudo pacman -S brightnessctl`),
3. logind's `SetBrightness` over D-Bus (works for the active session with no
   extra packages or permissions).

The other backends (`wpctl`, `nmcli`, `busctl`) ship with PipeWire/WirePlumber,
NetworkManager and systemd, which a Mango session already has.

---

## 3. Project layout

```text
src/
  main.rs           Shell: one invisible 1x1 layer surface. Watches monitors,
                    owns the Mango IPC feed, spawns one bar per monitor, and
                    loads the stylesheet (+ optional user override).
  bar.rs            BarModel: one Top-layer layer surface per monitor. Holds
                    the widget components and lays them out in a CenterBox,
                    each cluster wrapped in a `.mango-group` pill. Its launcher
                    button forwards a ToggleLauncher output to the shell.
  launcher.rs       LauncherModel: one full-screen Overlay-layer surface with a
                    scrim and a centred panel. Searches .desktop entries with
                    gio::AppInfo; toggled via SIGUSR1 or the bar button.
  mango.rs          Mango IPC: TagView, monitor stream, tag dispatch.
  widgets/
    clock.rs        ClockModel: date • time, 1 Hz timer.
    tags.rs         TagsModel: clickable tag buttons (occupied/active/urgent).
    volume.rs       VolumeModel: wpctl-backed volume/mute, click + scroll.
    wifi.rs         WifiModel: nmcli-backed SSID/signal readout.
    brightness.rs   BrightnessModel: sysfs backlight, scroll to adjust.
    tray.rs         TrayModel: StatusNotifierItem host + DBusMenu popover.
  style.css         Material 3 theme. Loaded at startup with
                    relm4::set_global_css(); ~/.config/mango-shell/style.css is
                    appended so wallpaper palettes can override the tokens.
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
- **No config system, theming, or service layer yet.** OkShell has
  `okshell-config` (reactive `reactive_graph` stores) and `okshell-services`.
  That is the natural next step, but it is a lot of machinery for a starter.

---

## 5. Next steps / ideas

- **Configure/theme it.** Add a config file (TOML) and read colors/height from
  it instead of `style.css`; or wire up `matugen` like OkShell does.
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
