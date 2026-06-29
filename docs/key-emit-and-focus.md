# Key emit & focus

[← Back to index](../README.md)

Two cross-transport support layers: how daklak synthesizes key events, and how
it knows which application is focused.

## Contents

- [Key emission](#key-emission)
- [Focus tracking](#focus-tracking)

## Key emission

Crate: [`key-emitter`](../crates/key-emitter/). All key synthesis goes through a
single trait so the rest of the code is independent of the underlying channel.
The trait signature and concrete backends are intentionally not duplicated here;
read the crate when changing emit code.

### Backends

- Wayland virtual-keyboard emit for compositors that expose it.
- Wayland input-method-context keysym emit for KWin's protocol path.
- uinput emit for device-level paths such as evdev mode.

The active backend is selected from the transport's
[capabilities](capability-model.md), not from compositor names.

## Focus tracking

Crate: [`focus`](../crates/focus/). daklak needs the focused application's
identity to drive per-app routing, terminal detection, and XWayland-specific
handling.

A runtime probe selects a focus backend based on what the compositor exposes.
The live source is recorded in the transport profile so routing code can reason
about where app identity came from.

### Backends

- wlroots foreign-toplevel tracking for wlroots compositors.
- KDE Plasma window-management tracking for KWin.
- X11 metadata bridge for XWayland clients.

### Why focus matters

- **Per-app routing** — apply force-vk-only / force-chars-delete / terminal
  overrides to the right application (see [Backspace tiers](backspace-tiers.md#per-app-overrides)).
- **XWayland detection** — XWayland clients behave differently from native
  Wayland clients on some compositors, so daklak distinguishes them.

The Wayland adapter uses the shared `focus` crate rather than maintaining its own
copy of focus logic.

## Next

- [Compositor quirks](compositor-quirks.md) — the concrete behaviors these
  layers exist to handle.
