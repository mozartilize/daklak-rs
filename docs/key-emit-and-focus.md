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

Crates: [`focus`](../crates/focus/) defines the shared event and X11 bridge;
[`wayland-adapter`](../crates/wayland-adapter/src/focus/) owns Wayland toplevel
drivers. Focus is optional metadata, not a requirement for text-input sessions.

The Wayland drivers normalize toplevels into one window model: application ID,
title, and activated state. A newly activated window wins over an older window
that has not reported deactivation yet. The X11 bridge only adds the XWayland
flag to that normalized focus event.

### Backends

- wlroots foreign-toplevel tracking for wlroots compositors.
- KDE Plasma window-management tracking for KWin.
- X11 metadata bridge for XWayland clients.

### Why focus matters

- **No-text-input fallback** — when the transport has a global keyboard grab and
  synthetic virtual-keyboard output, a focused client that never enables
  text-input gets a ForwardKey session synthesized from focus metadata. IMv1
  cannot do this because its keyboard grab exists only inside a text-input context.
- **XWayland detection** — XWayland clients behave differently from native
  Wayland clients on some compositors, so daklak distinguishes them.

## Next

- [Compositor quirks](compositor-quirks.md) — the concrete behaviors these
  layers exist to handle.
