# Daklak — Vietnamese Input Method

A framework-independent Vietnamese input-method daemon. Daklak connects directly to the
Wayland compositor via `zwp_input_method_v1` (KWin/Mutter) or `zwp_input_method_v2`
(wlroots compositors), or bypasses the compositor entirely by reading raw `/dev/input/event*`
devices in evdev grab mode — working on any Linux environment (Wayland, X11, TTY, or
remote session). Keystrokes pass through the core vnkey engine, which transforms them
according to the configured input method (Telex, VNI, VIQR, etc.), and the resulting
edits are applied back to the focused application through strategy-dependent emit paths
(`delete_surrounding_text` + `commit_string`, virtual-keyboard key events, or `/dev/uinput`).

## Dependencies

### Build-time

| Package | Required | Notes |
|---------|----------|-------|
| Rust toolchain | Yes | rustc + cargo |
| `libxkbcommon-dev` | Yes | Provides `xkbcommon.pc`; the only C library linked at build time |
| Wayland libraries | **No** | The pure-Rust backend (`wayland-backend` rs module) handles the Wayland protocol |
| `libevdev` / `libinput` | **No** | The evdev crate is pure Rust |
| KDE development packages | **No** | KDE protocol support comes from bundled Rust crates |

### Runtime

| Component | Required for |
|-----------|-------------|
| Wayland compositor | Normal operation (sway, Hyprland, KDE Plasma 6, river, etc.) |
| `/dev/uinput` access | Synthetic keystroke injection into focused applications |
| `input` group membership | Reading raw `/dev/input/event*` devices (evdev grab mode) |
| KDE Plasma 6 | KDE-specific input-method protocol (`-Dkde=true`) |

### Building and installing with Meson

```sh
git clone --recursive <url> daklak
cd daklak

# Debug build, no extras
meson setup build
meson compile -C build
sudo meson install -C build

# Release build with all optional features
meson setup --reconfigure build -Dkde=true -Devdev_grab=true --buildtype=release
meson compile -C build
sudo meson install -C build
```

If `-Devdev_grab=true` was used, Meson also runs `udevadm` to reload udev rules, and you
should add your user to the `input` group:

```sh
sudo usermod -aG input $USER
# then log out and back in
```

If `-Dkde=true` was used, a desktop entry is installed to `$datadir/applications/daklak.desktop`
so KDE Plasma can auto-launch the daemon.

Meson also installs a config template at `$datadir/daklak/config.toml.example`.
Copy it to `$XDG_CONFIG_HOME/daklak/config.toml` (or `~/.config/daklak/config.toml`
when `XDG_CONFIG_HOME` is unset), then adjust values as needed.

## Architecture

Daklak has two independent input paths, selected at runtime:

- **Wayland path** — connects to the compositor via `zwp_input_method_v2` (wlroots) or
  `zwp_input_method_v1` (KWin/Mutter). Key events arrive from the compositor's input-method
  grab; the response is dispatched through one of four tiers depending on compositor
  capability:
  - Tier 1 `SurroundingText` — `delete_surrounding_text` + `commit_string`
  - Tier 2 `ForwardKey` — virtual-keyboard backspace key events + commit
  - Tier 3 `UInput` — `/dev/uinput` backspace key events + commit
  - Tier 4 `VkOnly` — daklak synthetic keymap chars via virtual-keyboard only
- **Evdev path** — opens `/dev/input/event*` devices directly, bypassing the compositor.
  Works on Wayland, X11, and TTY. The daemon reads raw key events, runs them through the
  engine, and writes synthetic keystrokes to `/dev/uinput`.

Both paths converge on the same pipeline: engine → edit-strategy → key-emitter.

```
                    ┌──────────────────────────────────────┐
                    │         Wayland Compositor           │
                    │  (wlroots / KWin / Mutter)           │
                    └──────┬────────────────────▲──────────┘
                           │ input-method-v1/v2 │ commit/vk/uinput
                    ┌──────▼────────────────────┴──────────┐
                    │    viet-ime-wayland-adapter          │
                    │  IM protocol + focus tracking        │
                    │  Tier dispatch: commit/vk/uinput     │
                    └──────┬───────────────────────────────┘
                           │ key press / commit request
                    ┌──────▼───────────────────────────────┐
                    │    Daemon (viet-ime-daemon)          │
                    │  Event loop, config, routing         │
                    └────────────────┬─────────────────────┘
                                     │
                 ┌───────────────────▼───────────────────────┐
                 │  engine -> edit-strategy -> key-emitter   │
                 ├───────────────────┬───────────────────────┤
                 │ viet-ime-engine   │ viet-ime-edit-strategy│
                 │ (vnkey core)      │ shadow + tier logic   │
                 ├───────────────────┴───────────────────────┤
                 │ viet-ime-key-emitter (uinput / vk / v1)   │
                 └───────────────────────────────────────────┘

              ┌────────────────────────────────────────────────┐
              │    viet-ime-evdev-adapter                      │
              │  /dev/input/event* → engine → /dev/uinput      │
              │  Env-agnostic: Wayland, X11, TTY               │
              └────────────────────────────────────────────────┘
```

The project is structured as a Rust workspace with these crates:

| Crate | Role |
|-------|------|
| **viet-ime-daemon** | Main binary — CLI entrypoint, event loop, config loading, per-window policy/state, runtime routing between Wayland and evdev paths |
| **viet-ime-engine** | Wraps `vnkey-engine`, providing the core Vietnamese input-method logic (Telex, VNI, VIQR, etc.) |
| **viet-ime-wayland-adapter** | Wayland protocol I/O — connects to compositor, detects v1 or v2 backend, handles input-method protocol, focus tracking (`wlr-foreign-toplevel`, KDE Plasma), synthetic keymap upload, multi-tier key dispatch |
| **viet-ime-evdev-adapter** | Raw input path — reads `/dev/input/event*` devices directly, environment-agnostic (Wayland, X11, TTY) |
| **viet-ime-focus** | X11-based focus tracking via `x11rb` (X11Bridge) |
| **viet-ime-key-emitter** | Keystroke emission abstraction — wraps uinput, virtual-keyboard (v1/v2), and v1 context key emission behind a uniform `KeyEmitter` trait |
| **viet-ime-keymap** | Generates the daklak synthetic xkb keymap that maps key codes to Vietnamese character compositions |
| **viet-ime-edit-strategy** | Edit strategy/routing layer — manages shadow state, selection-aware delete spans, backspace behavior, and commit ordering |
