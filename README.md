# Daklak — UniKey-like Vietnamese Input Method

## Why Daklak feels different

- UniKey-like: no preedit, ever — not even as a fallback.
- A head-on solution for modern Wayland desktops (KDE/GNOME/Sway): protocol-first, with correct protocol implementations.
- Surrounding text is the tier-1 model: it enables retroactive word correction (adding or changing tones and marks) after a word is typed.
- IPC commands enable desktop-agnostic integration (e.g. binding an on/off shortcut).

## Release Signatures

Releases are signed with [7E41F540DBA56839722E1B10354BFCE527EA6812](https://keys.openpgp.org/search?q=7E41F540DBA56839722E1B10354BFCE527EA6812) and published [on GitHub](https://github.com/mozartilize/daklak-rs/releases).

## Quick installation

From a fresh checkout:

```sh
git clone --recursive <url> daklak
cd daklak
meson setup build -Dkde=true -Devdev_grab=true --buildtype=release
meson compile -C build
sudo meson install -C build
```

Start Daklak:

```sh
daklak
```

Useful commands while it is running:

```sh
daklak status          # show whether Vietnamese composition is enabled
daklak toggle          # turn composition on/off
daklak backend         # show the active backend
daklak backend native  # switch back to native Wayland/IBus
daklak backend evdev   # switch to evdev/uinput fallback
```

Notes:

- GNOME users normally use the IBus path; see [Getting started](docs/getting-started.md).
- evdev/uinput is powerful and needs extra input permissions; see [Evdev setup](docs/evdev-setup.md).
- For a local development build without installing system files, see [Getting started](docs/getting-started.md).

## How Daklak talks to your desktop

Daklak has one typing brain and several ways to deliver the result to apps:

- **Native Wayland** — talks directly to Wayland input-method protocols where the compositor supports them.
- **IBus on GNOME** — uses GNOME's normal IBus integration point.
- **evdev/uinput fallback** — grabs the physical keyboard and emits from a virtual keyboard for apps or desktops that do not work well with native input-method protocols.

The important details:

- Daklak is not built around the IBus/Fcitx architecture.
- On Wayland, Daklak sends ordered delete+commit edits so corrections do not race ahead asynchronously.
- The evdev/uinput fallback uses Daklak's generated keymap so legacy Wayland, X11, and TTY clients can still receive Vietnamese characters.
- You can switch between the native backend and the evdev grab backend at runtime.
- Switching at runtime means **native ↔ evdev**. It does not switch between Wayland and IBus after startup.

## Documentation

Full documentation lives in [`docs/`](docs/). If you are new, read only the first
two pages first.

| # | Page | What it explains |
| - | ---- | ---------------- |
| 1 | [Overview](docs/overview.md) | What Daklak is and why it avoids underlined preedit text. |
| 2 | [Getting started](docs/getting-started.md) | Build, run, backend switching, and common commands. |
| 3 | [Architecture](docs/architecture.md) | How the typing brain, backends, and control plane fit together. |
| 4 | [Backspace tiers](docs/backspace-tiers.md) | How Daklak edits text that was already committed to the app. |
| 5 | [Transports](docs/transports.md) | Wayland, IBus/GNOME, and evdev/uinput behavior. |
| 6 | [Evdev setup](docs/evdev-setup.md) | Permissions, keymaps, hooks, and troubleshooting for evdev mode. |
| 7 | [Capability model](docs/capability-model.md) | How Daklak decides what each compositor/app can support. |
| 8 | [Key emit & focus](docs/key-emit-and-focus.md) | Key-emission backends and focus tracking. |
| 9 | [Compositor quirks](docs/compositor-quirks.md) | Known compositor/app behaviors and workarounds. |
| 10 | [Contributing](docs/contributing.md) | Rules and conventions for maintainers. |

## For technical readers

- Daklak keeps Vietnamese composition in one transport-neutral core.
- The core computes edits such as “delete N characters, then commit this replacement”.
- Each backend decides how to deliver that edit to the focused app.
- The best path is surrounding-text delete+commit when the app supports it.
- The fallback path is forwarded Backspace plus one whole replacement channel.
- Wayland capability snapshots, IBus surrounding-text frames, and evdev/uinput events all feed the same edit model.

See [Architecture](docs/architecture.md), [Backspace tiers](docs/backspace-tiers.md),
and [Capability model](docs/capability-model.md) for the deeper design.

## Credits

- Kime https://github.com/Riey/kime
- anthywl https://github.com/tadeokondrak/anthywl
- vnkey https://github.com/marixdev/vnkey
- Fcitx5 https://github.com/fcitx/fcitx5
