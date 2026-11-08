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
- [Byte deletes are measured against the client's last report](#byte-deletes-are-measured-against-the-clients-last-report)
- [Preserve literal-key history after a confirmed repair](#preserve-literal-key-history-after-a-confirmed-repair)
- [Tail-drop after tone + space](#tail-drop-after-tone--space)
- [KWin: keysym on a still-held key](#kwin-keysym-on-a-still-held-key)
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

**GTK ordering on wlroots (Ghostty).** Ghostty only enables text-input after its
first focus change, so a session starts synthetic and later becomes a real
im-v2 `ForwardKey` session. GDK applies `zwp_text_input_v3.commit_string` as
soon as it arrives but queues `wl_keyboard.key` events, so a replacement sent
as `commit_string` overtakes the Backspace sent just before it on the virtual
keyboard: `la` + `à` → `laà` → Backspace → `la`, i.e. every word comes out as
plain letters. im-v2 ForwardKey therefore always emits the replacement on the
virtual keyboard (same channel as the Backspaces); `commit_string` is only the
fallback for chars outside daklak's synthetic keymap.

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

Frame-triggered repairs use the same byte-delete safety check as key-triggered
edits. KWin's pre-edit duplicate is part of the generic confirmation transaction;
it requires no separate app-specific stale-echo classification.

## Byte deletes are measured against the client's last report

**Behavior:** A text-input-v3 client cannot delete bytes from its own buffer
directly; it converts the byte length of `delete_surrounding_text` to a
character count and deletes that many characters before its real caret. GTK's
im-wayland module (used by Firefox) does the conversion against its *cached*
surrounding text, the last text the application reported, by walking back the
byte length from the cached cursor. Firefox fills that cache from a content
cache that can lag behind the edit daklak just committed, so the conversion
can cover a different number of characters than daklak intended. KWin forwards
the byte length unchanged.

Example: in `anser` with the caret after `s`, Telex `w` inserts `ư`. Firefox
then reports the old text `anser` with the cursor one step on (`anse|r`). The
second `w` reverts `ư` to `w` with a 2-byte delete; measured against
`anse|r` those 2 bytes are `se`, two characters, so Firefox deletes `sư` and
the result is `anwer` instead of `answer`. The same happens to a tone key typed
before Firefox reports the previous vowel change (`nhieeuf` → `nều` plus a
stray `u`).

**Resolution:** The composer records the text before the cursor from every
surrounding-text frame, including duplicates and echoes the reseed gate skips
(`Composer::note_client_surrounding`). Before a SurroundingText correction
with backspaces, it checks that this report ends with exactly the characters
being deleted (`client_byte_delete_agrees`). Only then does the byte length
measure the intended characters on the client; otherwise the deletion goes
through ForwardKey Backspaces, which count characters at the real caret. When
reports keep up with edits (gedit, GTK4, Chromium, and Firefox most of the
time), corrections keep using `delete_surrounding_text`.

There is no separate Firefox stale-echo classifier or sticky delete-channel
policy. A cached report mismatch affects this deletion only; it does not prove
that `commit_string` is unusable. The independently detected dead-surrounding
condition still downgrades the transport and enables the key-channel commit
fallback. Each replacement string is emitted whole through one channel; KWin
IMv1 keeps ForwardKey replacements on its keysym channel.

Mixed expanding reverts (`tự` + `w` → `tụw`) use this same check, not a
special-case routing guard. Gedit 51 on KWin produced `tụw` at 0.1-, 0.5-, and
2.5-second pauses, three fresh documents per pause, both with and without the
former guard. This validates the installed version, not every GTK client or
older gedit version.

## Preserve literal-key history after a confirmed repair

In Firefox on KWin, the first line in a fresh ProseMirror or Lexical editor
can enter surrounding-confirmed composition. Typing `anwwser` then repairs
the first `w` to `ăn` and uses the second `w` to escape back to literal `anw`.
Resetting the engine before the post-edit reseed loses that escape history:
`anw` fails the render gate (replaying it produces `ăn`), and a later `r`
incorrectly tones the remaining `se`.

`Composer::refresh_retroactive_context_after_apply` therefore leaves the live
engine intact until the render gate accepts a replacement seed. A successful
`feed_context_gated` replaces the state itself; a rejected seed disables
retroactive refresh but preserves the live literal-key history. The regression
test checks the confirmed repair, the `ww` escape, and the following `ser`.
The complete cursor-edit sequence ending in `answer` was also verified in
fresh ProseMirror and Lexical windows on KWin with Firefox Developer Edition.

## Tail-drop after tone + space

**Behavior:** On some compositor/client combinations, committing a tone change
immediately followed by a space can race the client's keymap recompilation
against per-keysym keycode installation, dropping the tail of the edit.

**Resolution:** The KWin keysym path (`commit_via_keysym_v1` in
`crates/wayland-adapter/src/sink.rs`) inserts a per-char Wayland `conn.flush()`
after every keysym pair so each char's keymap dance fully settles before the
next event. A modifier guard avoids the scratch-keycode temp-keymap swap for mapped
keysyms, eliminating the race for common telex tails. No wall-clock sleep is
needed — the Wayland round-trip barrier is sufficient. There is no post-apply
barrier; the per-char flush + guard handles it.

## KWin: keysym on a still-held key

**Behavior:** KWin resolves an input-method keysym to the lowest keycode that
carries it at any level of the session keymap and sends a press/release of that
keycode. It ignores a press for a keycode it already considers pressed. With
daklak's keymap, `ô` lives on level 5 of KEY_O, so fast rollover (`o` still held
while `w` turns `o` into `ơ`) would make KWin drop the synthetic press.

Fast typing often holds more than one key at once: in `hanhj`, `n` and the
second `h` can both still be down when `j` composes `ạnh`, so a fix that only
remembers the last forwarded key drops the `n` (`hạh`).

**Resolution:** The adapter keeps every user key whose press it forwarded and
whose release it has not (`HeldKeys` in `crates/key-emitter`, cleared on
deactivate). Before each keysym, `commit_via_keysym_v1` looks up the keycode
KWin will pick (`XkbState::keysym_keycode`, same any-level search) and, if that
key is held, emits a release for it through the forward channel first and drops
it from the set, so a later char on the same keycode needs no second release.
The virtual-keyboard path (`emit_char`) does the same with the keycode from
`char_to_emit`, and so does every synthetic Backspace (`AdapterSink::vk_key`):
deleting fast and retyping leaves the user's own Backspace down when the next
key's edit presses Backspace (`hanh` + `j` became `haạnh` in terminals). All
three go through `HeldKeys::release_before_press`. Edits triggered by a
surrounding-text frame use the same set.

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
