# Compositor quirks

[← Back to index](../README.md)

These are concrete, well-understood behaviors of specific compositors and
clients that daklak must accommodate. Most are upstream bugs or spec
disagreements, **not** daklak bugs. Each has a settled resolution — please do
not re-litigate or add ad-hoc workarounds beyond what's described here.

## Contents

- [GNOME / IBus ForwardKeyEvent fails in Mutter](#gnome--ibus-forwardkeyevent-fails-in-mutter)
- [Terminals — forwarded-key routing](#terminals--forwarded-key-routing)
- [Firefox contenteditable: stale-echo delete bypass](#firefox-contenteditable-stale-echo-delete-bypass)
- [Synthesized-keymap channel crashes some clients](#synthesized-keymap-channel-crashes-some-clients)
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

Implementation note: IBus surrounding-text echo tracking lives in
`crates/daemon/src/quirks/ibus.rs`. It nudges the per-backspace decision toward
ForwardKey when the previous correction was not echoed, capping downgrade rate
at one per surrounding-text frame. The quirk has no engine or transport
knowledge — it answers a single yes/no question.

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

## Firefox contenteditable: stale-echo delete bypass

**Behavior:** The text-input spec defines `delete_surrounding_text` lengths in
**bytes**, but Firefox contenteditable can return stale surrounding-text echoes.
When the echo is stale and the cursor drifts into the next word, a
char-count delete can target the wrong character (for example deleting the
inter-word space or the next word's prefix), so repeated tone edits start
mutating text ahead of the intended word.

**Resolution:** Keep spec-correct byte deletes on healthy surrounding-text
frames. When the Firefox quirk detects a stale correction echo, daklak now
bypasses `delete_surrounding_text` and emits one ForwardKey Backspace pair per
requested delete instead (Tier 2 key path). Replacement text is emitted as one
whole string through a single channel: key channel where that fallback is needed
(vk/keysym), or one whole `commit_string` where text-input commit is the working
channel. daklak does not split a logical replacement across key events and
`commit_string`. For retroactive reseed sessions this ForwardKey choice is kept
sticky, so rapid stale cursor shifts cannot alternate between delete channels
mid-word. This keeps retroactive surrounding-text reseeding but avoids Firefox's
mis-targeted surrounding deletes and post-first-edit channel desync.

Implementation note: daemon-local Firefox contenteditable state lives in
`crates/daemon/src/quirks/firefox.rs`. The ForwardKey bypass is wired in
`Composer::apply_to_sink` and remains a removable workaround: when
Firefox/contenteditable paths consistently echo spec-compliant
surrounding-text updates, the quirk module and this Composer hook can be
deleted without touching `crates/engine`.

Validation status: this behavior has been live-validated on wlroots/im-v2 and
KWin/im-v1 terminal paths. KWin/im-v1 keeps the whole replacement on the keysym
channel because splitting keysym and `commit_string` is not a coherent edit for
some terminals.

## Synthesized-keymap channel crashes some clients

**Behavior:** The virtual-keyboard synthesized-keymap replacement channel (the
keymap-swap emit that ForwardKey uses when no working text-input commit exists)
crashes some chromium-class applications.

**Resolution:** This channel is only reached automatically where there is no
usable text-input commit: a client that never enables text-input (session
synthesized from focus metadata with `commit_string_functional = false`), or a
client whose surrounding-text bridge proved dead and was downgraded to
ForwardKey. Clients with a healthy text-input commit keep
`commit_string_functional = true` and never touch this channel.

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
