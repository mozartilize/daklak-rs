# Compositor quirks

[← Back to index](../README.md)

These are concrete, well-understood behaviors of specific compositors and
clients that daklak must accommodate. Most are upstream bugs or spec
disagreements, **not** daklak bugs. Each has a settled resolution — please do
not re-litigate or add ad-hoc workarounds beyond what's described here.

## Contents

- [GNOME / IBus ForwardKeyEvent fails in Mutter](#gnome--ibus-forwardkeyevent-fails-in-mutter)
- [Terminals — forwarded-key routing](#terminals--forwarded-key-routing)
- [KWin / gedit: delayed surrounding cursor updates](#kwin--gedit-delayed-surrounding-cursor-updates)
- [Firefox contenteditable: stale-echo delete bypass](#firefox-contenteditable-stale-echo-delete-bypass)
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

**Decision:** Terminals are not special-cased at capability detection. A
terminal that advertises an empty surrounding-text frame (foot on KWin) is
initially detected as Tier 1 `SurroundingText`, then the **runtime ST→FK
liveness downgrade** takes over: the watchdog sees dead surrounding-text frames
(`text="" cursor=0`) during the first non-destructive keystrokes — KWin re-emits
several per keystroke, so the strike limit is reached within the first letter or
two — and downgrades to `ForwardKey` *before* the first `delete_surrounding_text`
is issued. Because Vietnamese Telex words begin with base letters that commit
with no delete, no surrounding-text delete ever reaches the PTY, so there is no
self-emit-loop or dropped commit. On wlroots, terminals (foot, Ghostty) send no
surrounding-text at all and resolve directly to `ForwardKey`.

## KWin / gedit: delayed surrounding cursor updates

**Behavior:** With KWin's input-method-v1 path, gedit can process a forwarded
cursor or editing key without first returning a surrounding-text frame for the
new cursor. It may also repeat the unchanged pre-action frame before the
forwarded key takes effect. A later printable key is the first event that causes
gedit to report the actual insertion point. Treating the repeated frame as the
destination can seed the engine from the wrong word; a cursor-relative delete
then applies at gedit's real cursor and edits another word.

A second daklak failure mode made this unbounded: a recent frame after a raw
forward was classified as an edit echo and skipped without recording it as the
new surrounding baseline. Repeated printable keys continually reopened the
recent-action window, so the baseline remained empty and raw Telex keys could
accumulate indefinitely.

**Resolution:** Surrounding text and IM composition have separate lifetimes.
Resetting composition for a forwarded action preserves the last confirmed
surrounding snapshot, so unchanged pre-action frames remain duplicates. More
importantly, daklak never emits a cursor-relative retroactive edit solely from
surrounding-derived engine state. It first forwards the printable key raw and
waits for a client frame proving that the new text equals the previous confirmed
text plus exactly that character immediately before the reported cursor. The
frame is recorded immediately, the engine is seeded from the actual word at
that cursor, and the key is replayed. If replay composes, daklak replaces the
raw character with the composed result; otherwise the raw character remains.

This rule is deliberately independent of how the cursor moved (mouse, arrows,
Home/End, modifier navigation, or native editing commands). If a frame cannot
prove the insertion, daklak synchronizes to it but emits no positional repair.
Overlapping unconfirmed keys likewise degrade to synchronization only. The raw
key may therefore be visible for one client frame during retroactive editing;
that bounded visual glitch is preferred to deleting at an unverified cursor.
Live composition built from the current keyboard stream is unchanged.

Frame-triggered repairs do not use the Firefox stale-echo classifier: KWin's
pre-edit duplicate is part of this generic confirmation transaction and must
not arm Firefox-specific ForwardKey fallback.

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

## Tail-drop after tone + space

**Behavior:** On some compositor/client combinations, committing a tone change
immediately followed by a space can race the client's keymap recompilation
against per-keysym keycode installation, dropping the tail of the edit.

**Resolution:** The KWin keysym path (`commit_via_keysym_v1` in
`crates/wayland-adapter/src/sink.rs`) inserts a per-char Wayland `conn.flush()`
after every keysym pair so each char's keymap dance fully settles before the
next event. A modifier guard avoids the kc 247 temp-keymap swap for mapped
keysyms, eliminating the race for common telex tails. No wall-clock sleep is
needed — the Wayland round-trip barrier is sufficient. There is no post-apply
barrier; the per-char flush + guard handles it.

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
hacks. The quirk responses documented here are deliberate, narrowly-scoped
responses to concrete upstream bugs — not a general escape hatch. When a new
client misbehaves, first confirm whether a correctly-behaving client uses the
same path before adding any special case.

## Next

- [Contributing](contributing.md) — conventions for changing any of this safely.
