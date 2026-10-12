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

---

## 3. Project layout

```text
src/
  main.rs           Shell: one invisible 1x1 layer surface. Watches monitors,
                    owns the Mango IPC feed, spawns one bar per monitor.
  bar.rs            BarModel: one Top-layer layer surface per monitor. Holds
                    the widget components and lays them out in a CenterBox.
  mango.rs          Mango IPC: TagView, monitor stream, tag dispatch.
  widgets/
    clock.rs        ClockModel: clock + date, 1 Hz timer.
    tags.rs         TagsModel: clickable tag buttons.
  style.css         Loaded at startup with relm4::set_global_css().
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
- **More modules.** Media player (MPRIS), battery (UPower), audio (PipeWire),
  network (NetworkManager), a launcher popup, notifications.
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
