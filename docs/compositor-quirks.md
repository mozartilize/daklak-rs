# Compositor quirks

[← Back to index](../README.md)

These are concrete, well-understood behaviors of specific compositors and
clients that daklak must accommodate. Most are upstream bugs or spec
disagreements, **not** daklak bugs. Each has a settled resolution — please do
not re-litigate or add ad-hoc workarounds beyond what's described here.

## Contents

- [GNOME / IBus ForwardKeyEvent fails in Mutter](#gnome--ibus-forwardkeyevent-fails-in-mutter)
- [Terminals — forwarded-key routing](#terminals--forwarded-key-routing)
- [Surrounding-text delete: chars vs bytes](#surrounding-text-delete-chars-vs-bytes)
- [VkOnly crashes some clients](#vkonly-crashes-some-clients)
- [Tail-drop after tone + space](#tail-drop-after-tone--space)
- [Preedit rendering of forwarded keys](#preedit-rendering-of-forwarded-keys)

## GNOME / IBus ForwardKeyEvent fails in Mutter

**Behavior:** IBus exposes a `ForwardKeyEvent` signal and GNOME Shell forwards it
into Clutter. In the current Mutter path, that forward-key handler builds a
Clutter key event with a `NULL` source device; `clutter_event_key_new()` rejects
that with `CLUTTER_IS_INPUT_DEVICE (source_device)`, so the event is not created
and cannot reach native Wayland clients. The useful IBus/Mutter text-input
channels are surrounding-text delete, commit text, and preedit. Mutter's
text-input action channel only covers submit-style actions, not delete/backspace.

**Diagnostic:** The failure appears in the user journal as:

```text
clutter_event_key_new: assertion 'CLUTTER_IS_INPUT_DEVICE (source_device)' failed
```

**Decision:** Default GNOME/IBus routing still prefers surrounding-text delete
plus commit text when the client provides surrounding text. The blanket "no
ForwardKey fallback on GNOME/IBus" rule above no longer holds: with the
upstream Mutter ForwardKeyEvent fix and our `IBUS_FORWARD_MASK` on every
forwarded BackSpace, real key events do reach native Wayland clients on GNOME.
daklak may still downgrade to ForwardKey at runtime when surrounding-text proves
non-functional (for example repeated empty frames or unechoed corrections).

**Rejected alternatives:** Commit-only appends without deleting the old tail;
preedit conflicts with daklak's direct-commit design; synthetic keyboard/device
injection belongs to non-IBus transports such as evdev or future compositor-level
input APIs, not the IBus engine path.

## Terminals — forwarded-key routing

**Decision:** Clients with the terminal content purpose default to forwarded-key
routing, regardless of surrounding-text. Surrounding-text can self-emit-loop and
drop commits in a terminal's PTY, while device-level backspace can race the
terminal's own read loop. Users can override per session with
`DAKLAK_TERMINAL_TIER`, or per configuration with `terminal_override`.

## Surrounding-text delete: chars vs bytes

**Behavior:** The text-input spec defines `delete_surrounding_text` lengths in
**bytes**, but some clients (notably Firefox) interpret them as **characters**.
Using a byte length against such a client deletes the wrong amount.

**Resolution:** A per-app "force-chars-delete" flag makes daklak express the
delete length in characters for the affected clients; other clients keep the
spec-correct byte semantics.

## VkOnly crashes some clients

**Behavior:** The Tier 4 synthesized-keymap path (`VkOnly`) crashes
chromium-class applications.

**Resolution:** Those apps are pinned away from the Tier 4 path through
configuration. Tier 4 itself is reserved for clients that expose no text-input
protocol and do not crash on the synthesized-keymap path.

## Tail-drop after tone + space

**Behavior:** On some compositor/client combinations, committing a tone change
immediately followed by a space can race the client's keymap recompilation
against per-keysym keycode installation, dropping the tail of the edit.

**Resolution:** Two short sleep barriers on the forward-key path — one between
keysyms and one after applying the edit — serialize the two windows so the
client settles before the next event.

## Preedit rendering of forwarded keys

**Behavior:** Some clients (e.g. the foot terminal) render forwarded
virtual-keyboard characters as **preedit**, which is the wrong presentation for
daklak's direct-commit model. Other clients on the same code path (e.g. Chrome)
render correctly.

**Resolution:** This is a client-side text-input issue. daklak does **not** add a
per-app workaround for it; the correct path is unchanged and the misbehaving
client should be fixed upstream.

## A note on philosophy

daklak's bias is to keep the protocol path correct and **not** accumulate per-app
hacks. The handful of per-app overrides that do exist
([backspace tiers](backspace-tiers.md#per-app-overrides)) are deliberate,
narrowly-scoped responses to concrete upstream bugs — not a general escape
hatch. When a new client misbehaves, first confirm whether a correctly-behaving
client uses the same path before adding any special case.

## Next

- [Contributing](contributing.md) — conventions for changing any of this safely.
