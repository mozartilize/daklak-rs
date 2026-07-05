# Daklak — Vietnamese Input Method

A framework-independent Vietnamese input-method daemon. Daklak runs in one of three
transport modes, chosen at startup: the **Wayland** compositor protocols
(`zwp_input_method_v2` on wlroots, `zwp_input_method_v1` on KWin/Plasma), an **IBus**
engine over D-Bus (GNOME/Mutter), or a raw **evdev** grab on `/dev/input/event*` that
bypasses the compositor entirely (works on any Wayland, X11, or TTY session). Keystrokes
pass through the core vnkey engine, which transforms them according to the configured
input method (Telex, VNI, VIQR, etc.), and the resulting edits are committed directly to
the focused application — no preedit.

## Documentation

Full documentation lives in [`docs/`](docs/). Read in this order — each page is
self-contained but builds on the previous one:

| # | Page | For |
| - | ---- | --- |
| 1 | [Overview](docs/overview.md) | What daklak is, the design axioms, the big picture. Read first. |
| 2 | [Getting started](docs/getting-started.md) | Build, run, the three modes, the CLI. |
| 3 | [Architecture](docs/architecture.md) | Crate map, data flow, the transport-neutral "brain". |
| 4 | [Backspace tiers](docs/backspace-tiers.md) | The core mechanism: how retroactive edits reach the app. |
| 5 | [Transports](docs/transports.md) | Wayland (IM v1/v2), IBus/GNOME, evdev — wire details per mode. |
| 6 | [Evdev setup](docs/evdev-setup.md) | Deep dive: the synthetic keymap and per-platform install for evdev mode. |
| 7 | [Capability model](docs/capability-model.md) | `TransportProfile`, `ImProtocol`, `FocusSource` — capability over identity. |
| 8 | [Key emit & focus](docs/key-emit-and-focus.md) | The `KeyEmitter` backends and focus tracking. |
| 9 | [Compositor quirks](docs/compositor-quirks.md) | Known upstream behaviors and their settled resolutions. |
| 10 | [Contributing](docs/contributing.md) | Conventions for maintainers. |

In one paragraph: daklak runs as a daemon in one of three transport modes
(Wayland, IBus, or raw evdev), chosen at startup. A single transport-neutral
composition core (the "brain") watches keystrokes, runs the Vietnamese engine,
and commits corrected text. Because there is no preedit, every correction
requires *retroactively deleting* the previous tail; the backspace-tier model
picks the best deletion mechanism for whatever the compositor and client
actually support. Compositor capabilities are captured once at connect time as a
`TransportProfile` and drive every downstream decision.

## Build And Install

See [Getting started](docs/getting-started.md) for prerequisites, development
runs, transport selection, debug logging, and desktop integration details.

```sh
git clone --recursive <url> daklak
cd daklak

# Local debug build
meson setup build
meson compile -C build

# Release install with optional desktop integrations
meson setup --reconfigure build -Dkde=true -Devdev_grab=true --buildtype=release
meson compile -C build
sudo meson install -C build
```

## Architecture

See [docs/architecture.md](docs/architecture.md) for the crate map, data flow, and
the transport-neutral design. The [backspace-tier model](docs/backspace-tiers.md) and
the [capability model](docs/capability-model.md) cover the two ideas that matter most.
