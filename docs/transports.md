# Transports

[← Back to index](../README.md)

daklak speaks to the desktop through one of three transports, chosen at startup
in [`main.rs`](../crates/daemon/src/main.rs). This page covers what each mode
requires and how it maps onto the [backspace tiers](backspace-tiers.md).

## Contents

- [Selection order](#selection-order)
- [Wayland](#wayland)
- [IBus / GNOME](#ibus--gnome)
- [evdev](#evdev)

## Selection order

At startup the daemon picks the first applicable mode:

1. **IBus** — built with `--features ibus` and IBus mode enabled (the `--ibus`
   flag, or being launched by `ibus-daemon`).
2. **Wayland** — built with `--features wayland` and Wayland enabled.
3. **evdev** — the fallback when neither of the above applies.

The control plane (enable/disable, IPC, tray) starts before the mode loop and is
identical across all three.

## Wayland

Crate: [`wayland-adapter`](../crates/wayland-adapter/). This mode targets two
distinct compositor families through two input-method protocol versions.

### Input-method v2 (wlroots / Sway)

- Uses `zwp_input_method_v2` plus `zwp_virtual_keyboard_v1`.
- Has a real **virtual keyboard**, so it can drive Tier 2 (`ForwardKey`) and
  Tier 4 (`VkOnly`).
- Commits text via the commit-string path; daklak's own commit acknowledges the
  frame (the v2 client does not heartbeat `done` on its own).

### Input-method v1 (KWin / Plasma)

- Uses `zwp_input_method_v1` via the input-method context.
- **No** virtual keyboard exposed; instead it commits characters through the
  context **keysym** path.
- The v1 protocol applies updates immediately per frame and heartbeats the
  text-input-v3 client through its commit state — it does not batch via a `done`
  event the way v2 does.

These differences are captured as capability booleans rather than hard-coded
branches; see [Capability model](capability-model.md).

### Focus

Wayland mode tracks the focused application through the
[`focus`](key-emit-and-focus.md#focus-tracking) backends
(wlr-foreign-toplevel, KDE Plasma window management, or an X11 bridge for
XWayland), which feeds per-app routing and tier selection.

### Key emission

The actual key synthesis goes through the unified `KeyEmitter` backends —
virtual-keyboard v2 or input-method-context v1 — see
[Key emit & focus](key-emit-and-focus.md).

## IBus / GNOME

Crate: [`ibus-adapter`](../crates/ibus-adapter/). On GNOME/mutter the Wayland
input-method protocols are not generally available to third-party IMEs, so
daklak registers as a standard **IBus engine** over D-Bus.

- Connects to `ibus-daemon` and serves the engine interface
  (`process_key_event`, focus in/out, etc.).
- Commits text using the `IBusText` GVariant encoding.
- Launched either with `daklak --ibus` or by `ibus-daemon` via the IBus
  component exec line; the `--ibus` flag forces IBus mode regardless of config.

For a Meson debug install, the generated IBus component points at the checkout's
`daklak-ibus-wrap.sh` wrapper. If GNOME Settings does not list Daklak from the
user-local component path, copy the generated component to IBus' system component
directory, refresh the cache, and restart IBus:

```sh
sudo cp ~/.local/share/ibus/component/daklak.xml /usr/share/ibus/component/daklak.xml
ibus write-cache
ibus restart
```

Log out and back in if GNOME Settings still does not show Daklak.

> **Important limitation:** GNOME's IBus/Mutter path does not provide a usable
> ForwardKey fallback for deletion. Use surrounding-text delete when available;
> otherwise native-client backspace on GNOME remains unsolved — see
> [Compositor quirks](compositor-quirks.md#gnome--ibus-forwardkeyevent-fails-in-mutter).

## evdev

Crate: [`evdev-adapter`](../crates/evdev-adapter/). The universal fallback,
independent of any compositor protocol.

- Grabs the keyboard device with `EVIOCGRAB` so the raw keystrokes don't reach
  applications directly.
- Decodes keycodes using the **system xkb layout**.
- Emits both ASCII pass-through and composed Vietnamese through `uinput`.

Because it emits at the device layer, Vietnamese output relies on a custom
system xkb layout being installed so the emitted keycodes decode to the right
characters. Generate it with `daklak gen-keymap` and apply it via the
platform-appropriate path (e.g. `swaymsg input … xkb_file` per-device on Sway,
or `xkbcomp … $DISPLAY` session-wide on X11). Setup is manual.

This mode is the most portable (works anywhere with `/dev/input` + `/dev/uinput`)
but the most invasive to install. See [Evdev-only setup](evdev-only-setup.md)
for the synthetic keymap, per-platform install steps, and troubleshooting.

## Next

- [Capability model](capability-model.md) — how the Wayland adapter records what
  each protocol can do.
- [Compositor quirks](compositor-quirks.md) — the upstream behaviors each
  transport has to accommodate.
