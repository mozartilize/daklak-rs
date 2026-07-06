# Overview

[← Back to index](../README.md)

## What daklak is

daklak is a Vietnamese input method (IME) for Linux Wayland desktops. The user
types plain ASCII (Telex/VNI-style), and daklak transforms it into correctly
composed Vietnamese — accents, tone marks, and precomposed characters — directly
in whatever application has focus.

It runs as a background **daemon** with a system-tray indicator and a small
command-line control surface.

## The core design axiom: no preedit

Most IMEs, especially many CJK-oriented ones, show a *preedit* — an underlined,
not-yet-committed buffer that the application renders specially. daklak
deliberately does **not** do this. Instead it follows the Unikey / Gboard model:
every keystroke result is **committed directly** to the application, so the
visible text is always final.

This single decision shapes the entire architecture:

- There is no separate "composing" visual state to manage.
- To correct an in-progress word (e.g. add a tone mark to a vowel typed three
  keys ago), daklak must **retroactively delete** the previously committed tail
  and commit the corrected text. When the application exposes surrounding text,
  daklak can also reseed from the word around the cursor, so editing a word you
  already finished can still change its tone or marks.
- *How* those deletions are delivered to the application depends entirely on
  what the compositor and the focused client support — which is why the
  [backspace-tier model](backspace-tiers.md) exists and is the heart of the
  project.

## What makes it hard

Wayland has no single, universally-supported way to inject text or edits into an
arbitrary application. Different compositors expose different protocols, and even
when a protocol exists, individual clients implement it inconsistently. daklak
therefore has to:

1. **Detect** what each compositor/client supports (capability probing).
2. **Choose** the cleanest deletion + commit mechanism available (tier selection).
3. **Work around** concrete, well-understood upstream bugs without breaking the
   compositors that behave correctly (see [Compositor quirks](compositor-quirks.md)).

## High-level shape

```
   ASCII keystrokes
         │
         ▼
  ┌──────────────┐     transport-neutral
  │   Composer   │     composition core ("the brain")
  │  + engine    │
  └──────┬───────┘
         │ "delete N, commit S"
         ▼
  ┌──────────────┐
  │  OutputSink  │     abstracts the wire
  └──────┬───────┘
         ▼
   one of three transports
   (Wayland / IBus / evdev)
         │
         ▼
   focused application
```

The brain never knows which compositor it is talking to. It emits abstract
edit operations; a transport adapter turns those into the right wire protocol.

## The three transports

daklak has three ways to reach applications:

- **Wayland** — for wlroots-based compositors (Sway) via input-method v2, and
  for KWin/Plasma via input-method v1. This is the direct native Wayland path.
- **IBus** — for GNOME/mutter, registering as an IBus engine over D-Bus because
  that is GNOME's supported IME integration point.
- **evdev** — a universal fallback that grabs the keyboard device directly,
  emits through `uinput`, and uses Daklak's custom keymap so legacy Wayland,
  X11, and TTY clients receive Vietnamese characters.

At startup daklak chooses the native desktop backend (Wayland or IBus). The
evdev grab backend can also be selected at startup or switched on/off later
without restarting the daemon. Runtime switching is native ↔ evdev; it does not
switch between Wayland and IBus after startup.

See [Transports](transports.md) for the details and trade-offs of each.

## Next

- New here? Continue to [Getting started](getting-started.md).
- Want the structure? Jump to [Architecture](architecture.md).
