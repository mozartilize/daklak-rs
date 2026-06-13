# Architecture

[← Back to index](../README.md)

This page is the structural map: the crates, who owns what, and how a keystroke
flows from device to committed text.

## Contents

- [Guiding principle](#guiding-principle)
- [Workspace crates](#workspace-crates)
- [The brain vs the transports](#the-brain-vs-the-transports)
- [Data flow](#data-flow)
- [Crate responsibilities](#crate-responsibilities)

## Guiding principle

> **The composition logic is transport-agnostic. The transport adapters are
> thin wire glue.**

The brain (in the `daemon` crate) decides *what* should happen — "delete the
last 3 characters, commit `phở`". A transport adapter decides *how* that reaches
the application on this particular compositor. The boundary between them is the
`OutputSink` trait, so the brain can be unit-tested without any compositor.

## Workspace crates

Declared in [`Cargo.toml`](../Cargo.toml):

| Crate | Layer | Responsibility |
| ----- | ----- | -------------- |
| `vendors/vnkey/vnkey-engine` | vendored | The Vietnamese composition algorithm. |
| [`engine`](../crates/engine/) | linguistic | Thin wrapper presenting a clean `Engine` API over vnkey. |
| [`edit-strategy`](../crates/edit-strategy/) | policy | The backspace-tier model, capability detection, shadow buffer, `OutputSink` trait. |
| [`keymap`](../crates/keymap/) | support | Daklak synthetic xkb keymap (memfd) and xkb state. |
| [`key-emitter`](../crates/key-emitter/) | emit | The unified `KeyEmitter` trait and its backends. |
| [`focus`](../crates/focus/) | support | `FocusBackend` trait, focus-source detection, X11 bridge. |
| [`daemon`](../crates/daemon/) | **brain + entry** | Composition core, control plane, CLI, config. |
| [`wayland-adapter`](../crates/wayland-adapter/) | transport | Wayland IM v1/v2 wire I/O, capability profile, tier emit. |
| [`evdev-adapter`](../crates/evdev-adapter/) | transport | Raw evdev grab + uinput emit. |
| [`ibus-adapter`](../crates/ibus-adapter/) | transport | IBus D-Bus engine. |
| `tools/probe` | dev-only | Throwaway protocol spikes. |

## The brain vs the transports

```
                  ┌────────────────────────────────────────────┐
                  │                  daemon                       │
                  │                                               │
                  │   handler::Daemon / handler::Router           │
                  │     · per-transport callbacks                 │
                  │     · transport-neutral key routing           │
                  │                                               │
                  │   composer::Composer  ("the brain")           │
                  │     · engine          (vnkey wrapper)         │
                  │     · EditModel       (shadow + edit planning)│
                  │     · SurroundingObserver (trust + reseed)    │
                  │                                               │
                  │   control plane: control / ipc / tray         │
                  └───────────────┬──────────────────────────────┘
                                  │  OutputSink + callbacks
        ┌─────────────────────────┼─────────────────────────────┐
        ▼                         ▼                              ▼
  wayland-adapter           ibus-adapter                  evdev-adapter
  IM v2 (wlroots) /         IBus engine over              EVIOCGRAB +
  IM v1 (KWin) +            D-Bus (GNOME)                 uinput
  virtual keyboard
        │                         │                              │
        ▼                         ▼                              ▼
   compositor               ibus-daemon                  /dev/input + /dev/uinput
```

Only **one** transport is live per daemon process. The control plane
(enable/disable, IPC socket, tray) is started first and is shared by all modes.

## Data flow

A single composing keystroke, end to end:

1. **Key arrives** at the active transport adapter (IM grab key event, IBus
   `process_key_event`, or a raw evdev event).
2. The adapter forwards it to the brain through a transport callback in
   `handler::Daemon`.
3. The **`Composer`** feeds the key to the `engine`, compares the new composed
   word against its shadow of what the app currently shows, and produces an edit
   plan: *delete N units, then commit string S*.
4. The brain calls the **`OutputSink`** with that plan.
5. The sink, inside the transport adapter, executes it using the
   [backspace tier](backspace-tiers.md) chosen for the current
   [capability profile](capability-model.md) — surrounding-text delete, forwarded
   `BackSpace` keys, uinput injection, or the synthesized-keymap path — and then
   commits the corrected text.
6. The shadow is updated to match.

## Crate responsibilities

### `engine`
Wraps the vendored vnkey engine and exposes the linguistic operations the
Composer needs. No I/O, no Wayland, no policy.

### `edit-strategy`
The policy layer, independent of any transport:
- `BackspaceMethod` enum and `detect_method()` — tier selection.
- `CapabilityProbe` — the inputs to tier selection.
- `OutputSink` trait — the abstract edit interface the brain calls.
- The shadow buffer and its invalidation rules.

### `daemon`
- `composer::Composer` — the transport-neutral composition core.
- `handler::{Daemon, Router}` — implements per-transport callbacks
  (Wayland handler, `activate_ibus` / `activate_evdev`, etc.) and routes keys.
- `control` / `ipc` / `tray` — the always-on control plane.
- `config` — configuration and per-app routing.
- `main` — CLI parsing and transport selection.

### `wayland-adapter`
Owns all Wayland wire concerns: input-method v1 and v2 protocols, the virtual
keyboard, the xkb keymap upload, the `TransportProfile` capability snapshot, and
the tier emit paths. See [Transports](transports.md) and
[Capability model](capability-model.md).

### `evdev-adapter`
Grabs a keyboard device (`EVIOCGRAB`), decodes via the system xkb layout, and
emits via `uinput`. Used as the universal fallback. Requires a custom system
xkb layout so emitted keycodes decode to Vietnamese characters.

### `ibus-adapter`
A full IBus engine: registers on the IBus bus, handles `process_key_event`,
and commits text / forwards keys through the IBus protocol (including the
`IBusText` GVariant encoding).

### `key-emitter` & `focus`
Cross-transport support layers — the key-synthesis backends and the
focus-tracking backends respectively. See [Key emit & focus](key-emit-and-focus.md).

## Next

- [Backspace tiers](backspace-tiers.md) — the mechanism the whole design serves.
