# Transports

[← Back to index](../README.md)

daklak speaks to the desktop through three transport families: native Wayland,
IBus, and evdev/uinput. Startup chooses the native backend or evdev, and the
running daemon can switch between the native backend and evdev without a restart.
This page covers what each backend requires and how it maps onto the
[backspace tiers](backspace-tiers.md).

## Contents

- [Selection order](#selection-order)
- [Wayland](#wayland)
- [IBus / GNOME](#ibus--gnome)
- [evdev](#evdev)

## Selection order

At startup the daemon picks the first applicable active backend:

1. **IBus** — built with `--features ibus` and IBus mode enabled (the `--ibus`
   flag, or being launched by `ibus-daemon`). IBus is started first so evdev can
   later layer on top while the IBus connection remains alive in pass-through.
2. **evdev** — selected when `enable_evdev_grab = true` and IBus did not force
   the native path first.
3. **Wayland** — selected when Wayland is enabled and evdev was not selected.

The native desktop backend is still either IBus or Wayland; that native choice is
fixed for the daemon lifetime. The control plane (enable/disable, backend
switching, IPC, tray) starts before the backend loop and is shared.

### Runtime switching

You can switch between the **native desktop backend** (IBus or Wayland,
whichever was chosen at startup) and the **evdev grab backend** at runtime
without restarting the daemon:

```
daklak backend            # query: prints current backend
daklak backend native     # switch to native desktop backend (IBus/Wayland)
daklak backend evdev      # switch to evdev grab backend
```

The tray icon also exposes an "Enable evdev" / "Disable evdev" checkbox when
evdev-grab support is compiled in.

**Switching is limited to native desktop ↔ evdev grab.** Switching between
IBus and Wayland at runtime is not supported; the initial native choice is
fixed for the daemon's lifetime.

#### What happens on switch

| Direction | Action |
|---|---|
| Native → evdev | Setup hooks run, keyboards are grabbed, engine starts using evdev |
| Evdev → native | Input grabs are released, cleanup hooks run, engine uses native transport |

#### Keep-connection passthrough (IBus)

When switching from IBus to evdev, the **IBus D-Bus connection is kept alive**.
The engine's `suspended` flag is set so IBus forwards keys raw without
processing them. On switch-back the flag is cleared and IBus resumes composing
— no connection renegotiation, no input-context binding loss.

While the evdev grab is layered on top, daklak's own synthetic keystrokes
(emitted through its uinput device) still travel back through the live IBus
engine — the grab only starves the *physical* keyboard, not daklak's own
output. The engine therefore serves those key events strictly in the order it
receives them (its D-Bus interface is dispatched serially rather than one task
per call). Without in-order dispatch, a key release can overtake its own press
on the way back to the application, stranding a pressed key that auto-repeats
until the next keystroke.

#### What happens if the target backend is unavailable

If the requested backend was not compiled in or cannot be initialized (e.g.
evdev keyboards cannot be grabbed), the current backend continues running
unaffected.

## Wayland

Crate: [`wayland-adapter`](../crates/wayland-adapter/). This mode targets two
distinct compositor families through two input-method protocol versions.

### Input-method v2 (wlroots / Sway)

- Uses `zwp_input_method_v2` plus `zwp_virtual_keyboard_v1`.
- Has a real **virtual keyboard**, so Tier 2 (`ForwardKey`) can drive both the
  synthesized backspaces and the virtual-keyboard synthetic-keymap replacement
  channel.
- Commits text via the commit-string path when that channel is healthy;
  daklak's own commit acknowledges the frame (the v2 client does not heartbeat
  `done` on its own). When ForwardKey must avoid a stale or absent text-input
  channel (`commit_string_functional = false`), replacement text instead goes
  through the virtual keyboard's synthetic keymap as one whole key-channel
  replacement. Clients that never enable text-input get a ForwardKey session
  synthesized from focus metadata, which uses exactly this channel.

### Input-method v1 (KWin / Plasma)

- Uses `zwp_input_method_v1` via the input-method context.
- **No** virtual keyboard exposed; ForwardKey replacement text is kept on the
  context **keysym** path as one whole replacement. daklak does not split one
  replacement between keysym and `commit_string` on this transport.
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
- Commits text using the `IBusText` GVariant encoding. ForwardKey deletes are
  sent as `ForwardKeyEvent` Backspace signals, but replacement text stays one
  whole `CommitText`; IBus does not provide a universal Unicode key-channel
  replacement equivalent to Wayland `zwp_virtual_keyboard_v1`.
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

> **Compatibility note:** ForwardKey on IBus depends on the compositor/toolkit
> honoring `ForwardKeyEvent` Backspace. daklak preserves signal ordering with a
> barrier before the whole replacement `CommitText`; see
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
but the most invasive to install. See [Evdev setup](evdev-setup.md)
for the synthetic keymap, per-platform install steps, and troubleshooting.

## Next

- [Capability model](capability-model.md) — how the Wayland adapter records what
  each protocol can do.
- [Compositor quirks](compositor-quirks.md) — the upstream behaviors each
  transport has to accommodate.
