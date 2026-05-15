# Protocol Behavior — Sway/wlroots (ground truth)

Collected with `tools/probe` on Sway (wlroots). Date: 2026-05-16.

---

## zwp_input_method_v2 — wlroots vs public spec

**Critical divergence**: wlroots ships its own version of `input-method-unstable-v2` that differs
from the staged `wayland-protocols` version. Always use the XML from the wlroots source tree, not
from the system `wayland-protocols` package.

| Field | wlroots | public wayland-protocols |
|---|---|---|
| `activate` event | ✓ evt[0] | ✗ absent |
| `deactivate` event | ✓ evt[1] | ✗ absent |
| `surrounding_text` opcode | 2 | 0 |
| `done` args | none | `serial: uint` |
| `unavailable` opcode | 6 | 4 |

The `activate`/`deactivate` events are wlroots extensions. The daemon **must** handle them —
the compositor sends `activate` immediately when a text input gains focus and `deactivate` on
blur, before any surrounding_text state.

**Implementation note**: treat `deactivate` as implicit focus-leave — reset engine and shadow
buffer. Treat `activate` as focus-enter — re-detect capability.

---

## done-frame event ordering (Sway)

Observed order within a single done-frame (consistent across all frames):

```
surrounding_text → text_change_cause → content_type → done
```

Matches spec requirement ("all state updates become active at done() boundary"). No frames
observed where `surrounding_text` arrived AFTER `done`. Guarantee holds on Sway.

---

## Focus change ordering (activate/deactivate)

Observed sequence when switching focus:

```
deactivate (old window)
  → surrounding_text + text_change_cause + content_type + done  (new window state)
activate (new window)
  → text_change_cause + content_type + done
```

**Key finding**: `deactivate` of old window always arrives **before** `activate` of new window.
No enter-before-leave inversion observed on Sway. The plan's defensive fallback is for
other compositors only.

**Double deactivate observed**: Sway sometimes sends two consecutive `deactivate` events
with no `activate` between (seen at rapid focus switches). Daemon must treat consecutive
deactivates as a single deactivate — idempotent reset.

---

## surrounding_text — per app

**foot terminal** (`content_type hint=0 purpose=13`):
```
surrounding_text text="" cursor=0 anchor=0
```
Terminals send empty surrounding text. foot does not expose buffer contents.
→ Daemon must auto-select Tier 2 (`forward_key`) for `purpose=TERMINAL`.

**gedit** (`content_type hint=1 purpose=0`) — Tier 1 confirmed:
```
surrounding_text text="tran viet ha" cursor=12 anchor=12
```
Sends `surrounding_text` on EVERY keystroke with full buffer and exact cursor position.
`cursor == anchor` when no selection. `change_cause=0` (InputMethod) per keystroke,
`change_cause=1` (Other) on click/focus-change.

**VSCode** (`hint=0 purpose=0`): never sends `surrounding_text`. Tier 2/3.

**deactivate includes final state**: when focus leaves gedit, Sway sends the last
`surrounding_text` inside the deactivate frame. Shadow buffer can be validated on blur.

`purpose=13` (TERMINAL) reliably identifies terminals without probing surrounding_text quality.

---

## content_type values observed

| App | hint | purpose | Tier |
|---|---|---|---|
| foot | 0 | 13 | 2 — forward_key |
| gedit | 1 | 0 | 1 — surrounding_text |
| VSCode | 0 | 0 | 2/3 — no surrounding_text |

---

## Keyboard grab

Received **immediately** after `grab_keyboard()`, before first blocking_dispatch returns:
1. `keymap format=xkb_v1 size=35356`
2. `repeat_info rate=25/s delay=600ms`

No extra roundtrip needed.

---

## Virtual keyboard permissions

`zwp_virtual_keyboard_v1` created successfully with **no special config on Sway**. No udev
rules or group membership required (unlike `/dev/uinput` which requires the `input` group).

---

## Shadow buffer invalidation signals (gedit observed)

All confirmed on Sway with gedit:

| User action | surrounding_text change | change_cause | Daemon action |
|---|---|---|---|
| Type char | text grows, cursor+1 | 0 (InputMethod) | normal — shadow in sync |
| Backspace | text shrinks, cursor-1 | 1 (Other) | cursor delta detected → reset shadow |
| Arrow Left/Right | text same, cursor changes | 1 (Other) | cursor delta detected → reset shadow |
| Click | text same or new, cursor jumps | 1 (Other) | cursor delta → reset shadow |
| Focus enter | full state sent on activate | 1 (Other) | treat as new word start |

**Rule confirmed**: `change_cause` alone is NOT reliable — backspace and arrow key both
report `cause=1`. The reliable signal is **cursor delta** (`last_known_cursor != new_cursor`
when the daemon didn't cause the change). This matches the plan's priority-1 invalidation.

**Typing mid-word works**: after navigating cursor to position 3 in existing text and typing,
`surrounding_text` reflects the insertion at the correct position with `cause=0`. Tier 1
can correctly handle mid-word IME operation when surrounding_text is available.

**Retroactive word editing** — the killer feature of Tier 1:

```
"tran" cursor=2 (after 'a')
→ engine.feed_context("tra")  ← seed from surrounding_text before cursor
→ type 'a': delete_surrounding_text(before=1, after=0) + commit("â")  → "trân" cursor=3
→ type 'f': delete_surrounding_text(before=3, after=0) + commit("ầ")  → "trần" cursor=4
```

`delete_surrounding_text(before, after=0)` only deletes BEFORE the cursor — characters after
the cursor ('n' here) are untouched. This means the entire word can be retroactively corrected
without any awareness of what follows the cursor.

Daemon implementation: on `activate` with cursor mid-word, call `engine.feed_context(text_before_cursor)`
to seed vnkey-engine state. Then process keys normally. The engine handles the rest.

---

## done serial

`done` has **no serial** in the wlroots implementation. The `commit(serial)` request serial
is presumably a frame generation counter (count of `done` events received). Needs a commit
test to confirm exact semantics.

---

## Key codes

KBD events use raw Linux evdev keycodes (not XKB):
- 28 = KEY_ENTER, 29 = KEY_LEFTCTRL, 46 = KEY_C, 125 = KEY_LEFTMETA

Modifier bitmask: `0x00000004` = Ctrl, `0x00000040` = Super (Mod4).

---

## Action items for stages 1–4

- [ ] Stage 3: handle `activate`/`deactivate` as primary focus signals; deactivate is idempotent (double-deactivate observed)
- [ ] Stage 3: `done` has no serial — use frame counter for `commit(serial)`
- [ ] Stage 4: use wlroots XML (from wlroots source tree), not system wayland-protocols
- [ ] Stage 3: `purpose=TERMINAL` → auto-downgrade to Tier 2 (`forward_key`)
- [x] Stage 2: cursor=0 on empty surrounding_text is foot limitation — gedit populates correctly
