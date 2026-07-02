# Backspace tiers

[← Back to index](../README.md)

This is the central mechanism of daklak. Because there is
[no preedit](overview.md#the-core-design-axiom-no-preedit), correcting an
in-progress word means **deleting the previously committed tail and committing
the corrected text**. There is no single Wayland-wide way to do that, so daklak
ranks several mechanisms by cleanliness and picks the best one available.

## Contents

- [The active tiers](#the-active-tiers)
- [Selection logic](#selection-logic)
- [Per-app overrides](#per-app-overrides)
- [The shadow buffer](#the-shadow-buffer)

## The active tiers

The concrete enum and emit code live in the `edit-strategy` crate. Treat the
source as authoritative for names and module layout.

| Tier | Mechanism | Cleanliness |
| ---- | --------- | ----------- |
| 1 | `delete_surrounding_text` over the text-input protocol | cleanest — the app deletes exactly the range we name |
| 2 | synthesize `BackSpace` key events, then emit the replacement through one whole backend-selected channel | good — but relies on the client treating forwarded deletes as edits |

`BackspaceMethod` has exactly two variants: `SurroundingText` (Tier 1) and
`ForwardKey` (Tier 2). There is no separate "VkOnly" tier — the
synthetic-keymap virtual-keyboard emit that used to be its own tier is now one
of ForwardKey's replacement channels (see below).

### Why these tiers?

- **Tier 1** is preferred whenever the client advertises surrounding-text
  support — it is unambiguous and atomic.
- **Tier 2** is the general default when there's no surrounding-text channel:
  forward backspaces as key events, then emit the replacement as one whole
  string through a single channel. It has three possible replacement channels,
  chosen by the sink, never split within one replacement:
  - **whole `commit_string`** — for clients with a working text-input commit
    (e.g. foot on wlroots/im-v2, where `commit_string_functional` is true);
  - **keysym channel** — KWin/im-v1 emits the whole replacement through
    `zwp_input_method_context_v1::keysym`;
  - **virtual-keyboard synthetic keymap** — im-v2/vk emits each replacement
    char as a keycode against daklak's synthesized xkb keymap (Vietnamese
    precomposed chars at spare keycodes). This is the channel that reaches
    clients with **no usable text-input at all** (some Qt apps, XWayland apps
    such as OnlyOffice, wlroots terminals such as Ghostty). Such clients never
    fire an `Activate`, so daklak synthesizes a ForwardKey session from focus
    metadata with `commit_string_functional = false`, which forces this
    channel. IBus uses one whole `CommitText` after the forwarded deletes.

## Selection logic

The strategy code chooses a tier from the current client and transport
capabilities. The important rules:

- **Surrounding text wins.** If the probe shows surrounding-text was observed,
  use Tier 1.
- **Terminals get ForwardKey.** Clients with the terminal content purpose route
  to Tier 2 (`ForwardKey`) regardless of surrounding-text, unless a
  `terminal_override` (or the `DAKLAK_TERMINAL_TIER` env var) says otherwise.
  Surrounding-text would self-emit-loop and drop commits in a PTY. See
  [Compositor quirks](compositor-quirks.md#terminals--forwarded-key-routing).
- **No text-input at all → synthesized ForwardKey.** Clients that never enable
  text-input fire no `Activate`, so capability detection never runs. When the
  transport exposes a virtual keyboard, daklak synthesizes a ForwardKey session
  from focus metadata with `commit_string_functional = false`, routing the
  replacement through the virtual-keyboard synthetic keymap. A real `Activate`
  always wins and replaces the synthesized session.
- **The synthetic-keymap channel requires a virtual-keyboard-capable
  transport.** On the KWin/Mutter im-v1 relay (no vk keyboard), that channel is
  unavailable, so ForwardKey emits through the keysym channel instead.
- **Default is ForwardKey.** With no surrounding text and nothing special, Tier 2.

Tier output is delivered through the `OutputSink` trait, so the selection and
execution are independent of which transport is live.

## Per-app overrides

Some clients misbehave in tier-specific ways, so configuration can pin behavior
per application:

- **terminal override** — pin a specific method for terminals
  (`DAKLAK_TERMINAL_TIER`).

See [Compositor quirks](compositor-quirks.md) for the concrete behaviors that
motivate each override.

## The shadow buffer

To compute a minimal edit, the brain keeps a **shadow** of what it believes the
application currently shows for the in-progress word. Each keystroke diffs the
new composed form against the shadow to produce "delete N, commit S".

The shadow can drift out of sync with reality (the user clicks elsewhere, an app
rewrites its own field, focus changes). daklak therefore **invalidates** the
shadow on several signals — cursor jumps, deactivation, navigation/shortcut
keys, surrounding-text changes it didn't cause, and idle timeout — and reseeds
from the application's actual surrounding text where available. The
trust-and-reseed decision is isolated in the `SurroundingObserver`
(see [Architecture](architecture.md#crate-responsibilities)).

> **Re-seed gate caution:** navigation and shortcut keys must *not* count as
> composing activity, or they block legitimate mid-word reseeds. This is a
> recurring footgun when editing the composer.

## Next

- [Transports](transports.md) — how each tier maps onto each wire protocol.
- [Capability model](capability-model.md) — where the `CapabilityProbe` inputs
  come from.
