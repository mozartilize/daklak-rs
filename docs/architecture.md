# Architecture

[← Back to index](../README.md)

This page is the structural map: the crates, who owns what, and how a keystroke
flows from device to committed text.

## Contents

- [Guiding principle](#guiding-principle)
- [Workspace crates](#workspace-crates)
- [The brain vs the transports](#the-brain-vs-the-transports)
- [Data flow](#data-flow)
- [Composition & seeding](#composition--seeding)
- [Composer transitions](#composer-transitions)
- [Failure boundaries](#failure-boundaries)
- [Crate responsibilities](#crate-responsibilities)

## Guiding principle

> **The composition logic is transport-agnostic. The transport adapters are
> thin wire glue.**

The brain (in the `daemon` crate) decides *what* should happen — "delete the
last 3 characters, commit `phở`". A transport adapter decides *how* that reaches
the application on this particular compositor. The boundary between them is the
`OutputSink` trait, so the brain can be unit-tested without any compositor.

## Workspace crates

Declared in [`Cargo.toml`](../Cargo.toml):

| Crate | Layer | Responsibility |
| ----- | ----- | -------------- |
| `vendors/vnkey/vnkey-engine` | vendored | The Vietnamese composition algorithm. |
| [`engine`](../crates/engine/) | linguistic | Thin wrapper presenting a clean `Engine` API over vnkey. |
| [`edit-strategy`](../crates/edit-strategy/) | policy | The backspace-tier model, capability detection, shadow buffer, `OutputSink` trait. |
| [`keymap`](../crates/keymap/) | support | Daklak synthetic xkb keymap (memfd) and xkb state. |
| [`key-emitter`](../crates/key-emitter/) | emit | The unified `KeyEmitter` trait and its backends. |
| [`focus`](../crates/focus/) | support | `FocusBackend` trait, focus-source detection, X11 bridge. |
| [`daemon`](../crates/daemon/) | **brain + entry** | Composition core, control plane, CLI, config. |
| [`wayland-adapter`](../crates/wayland-adapter/) | transport | Wayland IM v1/v2 wire I/O, capability profile, tier emit. |
| [`evdev-adapter`](../crates/evdev-adapter/) | transport | Raw evdev grab + uinput emit. |
| [`ibus-adapter`](../crates/ibus-adapter/) | transport | IBus D-Bus engine. |
| `tools/probe` | dev-only | Throwaway protocol spikes. |

## The brain vs the transports

```
                  ┌───────────────────────────────────────────────┐
                  │                  daemon                       │
                  │                                               │
                  │   handler::Daemon / handler::Router           │
                  │     · per-transport callbacks                 │
                  │     · transport-neutral key routing           │
                  │                                               │
                  │   composer::Composer  ("the brain")           │
                  │     · engine          (vnkey wrapper)         │
                  │     · EditModel       (shadow + edit planning)│
                  │     · SurroundingObserver (trust + reseed)    │
                  │                                               │
                  │   control plane: control / ipc / tray         │
                  └───────────────┬───────────────────────────────┘
                                  │  OutputSink + callbacks
        ┌─────────────────────────┼──────────────────────────────┐
        ▼                         ▼                              ▼
  wayland-adapter           ibus-adapter                  evdev-adapter
  IM v2 (wlroots) /         IBus engine over              EVIOCGRAB +
  IM v1 (KWin) +            D-Bus (GNOME)                 uinput
  virtual keyboard
        │                         │                              │
        ▼                         ▼                              ▼
   compositor               ibus-daemon                  /dev/input + /dev/uinput
```

The daemon has one active composing backend at a time, but the native desktop
backend and the evdev grab backend are switchable at runtime. The native choice
(IBus or Wayland) is fixed at startup. When IBus is the native backend and evdev
is enabled, the IBus D-Bus connection stays alive in suspended pass-through mode
while evdev owns the physical keyboard; switching back clears the suspension.
The control plane (enable/disable, backend switching, IPC socket, tray) is
started first and is shared by all backends.

## Data flow

A single composing keystroke, end to end:

1. **Key arrives** at the active composing backend (Wayland IM grab key event,
   IBus `process_key_event`, or a raw evdev event).
2. The adapter forwards it to the brain through a transport callback in
   `handler::Daemon`.
3. The **`Composer`** feeds the key to the `engine`, compares the new composed
   word against its shadow of what the app currently shows, and produces an edit
   plan: *delete N units, then commit string S*.
4. The brain calls the **`OutputSink`** with that plan.
5. The sink, inside the transport adapter, executes it using the
   [backspace tier](backspace-tiers.md) chosen for the current
   [capability profile](capability-model.md) — surrounding-text delete, forwarded
   `BackSpace` keys, or the synthesized-keymap path — and then
   commits the corrected text.
6. The shadow is updated to match.

## Composition & seeding

The Composer owns exactly **two** pieces of word state, with a strict division:

- **the engine** owns the *composition* state — the in-progress syllable and the
  raw keystrokes behind it (vnkey keeps its own keystroke buffer internally).
- **the shadow** owns daklak's mirror of the **app's visible text** before the
  cursor. It is the only thing daklak itself tracks per word.

Daklak keeps no third copy. The engine runs **continuously**: every printable
key is fed to it *without resetting it between keys*, so vowel-cluster context
survives across transforms — typing `tieengs`, the sắc tone lands on `iê`
because the engine still holds the cluster. All three transports (wayland IM
v1/v2, IBus, evdev) use this one path, `Composer::feed_key`.

The engine is **(re)seeded** from the word before the cursor in the shadow in
two cases: retroactive editing (the cursor jumping into existing text), and a
backspace that vnkey could not edit itself. After such a backspace vnkey has
only shortened its key history, which no longer matches the screen; after a
non-Vietnamese restore it would stay non-composable (`hanhjph`, three
backspaces, then `j` must give `hạnh`). This reseed is the same on every
backspace tier.

A seed is fed via `feed_context_gated` and is **render-gated**: the
reconstruction (composed glyphs → method keystrokes) is replayed and must
round-trip back to the original glyphs before it is trusted. A word that does
not round-trip (e.g. English `wor`, which telex-reads as a Vietnamese vowel) is
never seeded as garbage: on a cursor jump it is left unseeded and typed fresh.

After a backspace, raw letters left of a restored word are the keys the user
typed, even when they do not reproduce themselves (`thoongh` minus `h` leaves
`thoong`, which Telex shows as `thông`). They are seeded as typed keys, and the
next key acts as if they had just been typed: its result is written over the
raw letters (`thoong` + `s` → `thống`, `goon` + `g` → `gông`). Until then the
screen keeps the raw letters, and a further backspace is left to the app rather
than to vnkey, whose own backspace would edit its composed word. A word that is
not raw letters and does not round-trip passes keys through unchanged until the
next word break, so no new syllable starts mid-word.

Reconstructed keys follow the user's own spelling when it retypes the word:
shorthands survive (`ửo` typed as `wor`), and the tone key goes where it was
typed (`hạnh` → `hanhj`, `doén` typed as `doesn`).

## Composer transitions

`ConfirmationState` is the single source of truth for whether the Composer's
client context is live, derived from a frame, awaiting a fresh frame, or holding
a pending raw key. `LiveConfirmed` records that the client echoed the current
live shadow after the previous key; it authorizes the next post-idle edit to
apply without a raw-key flash. Every new surrounding frame revalidates that
proof, and a frame showing the caret somewhere other than the end of the live
shadow revokes it even when the frame is otherwise ignored as a recent echo.
Two kinds of frame do not revoke it:

- an exact repeat of the client's previous report, which carries no new
  evidence (clients resend the frame that confirmed a raw key after daklak
  repaired it). Matching the recorded diff baseline alone is not enough: that
  baseline goes stale while echoes are skipped, so a frame equal to it can
  still be new;
- a frame whose collapsed caret byte offset equals the shadow length but whose
  text lags. Firefox reports its new caret with the previous text after each
  edit; the caret is where daklak left it, and the next delete still goes
  through Backspace because the client's reported text disagrees
  (see [Compositor quirks](compositor-quirks.md#byte-deletes-are-measured-against-the-clients-last-report)).

Without a confirmed echo, the key remains
confirm-deferred and is repaired from the delayed frame. A frame confirms a
pending raw key when it is the recorded baseline, or the client's own previous
report, with exactly that key inserted before the caret. A confirmed repair
leaves the composition `LiveConfirmed`: the frame proved where the caret is,
so the next key edits directly instead of waiting for another frame that
Firefox, which reports its caret ahead of its text, may not send in time. `PendingRaw` keeps the
raw key, repairability, and any speculative engine edit together, so those facts
cannot drift into invalid flag combinations.

Within 150 ms of a key, frames are echoes of daklak's own edits and are
skipped unless they match the shadow, or show exactly one letter inserted since
the recorded baseline (the user's own raw key). That baseline lags while echoes
are skipped, so a half-applied echo can also look like one inserted letter:
Firefox applying `gôn` → `goonh` reports `g`, then `goo`, against a baseline of
`go`. A late echo of a raw key that daklak has since edited looks the same
(`ngu` arriving after `nguw` showed `ngư`). A frame whose text before the caret
is a strict prefix of the shadow, or one of the last few shadows daklak replaced
since it last trusted a frame, is therefore not counted as a typed letter: it
shows a state daklak has already moved past.

A one-letter insertion also cannot echo the user's key if no key was typed
since composition was last reset (activation, a shortcut, navigation). After
Ctrl+Backspace, a lagging Firefox can still report the deleted word growing
letter by letter (`vô s`, `vô sa`). Such a frame is treated like any external
change: it reseeds frame-derived context, so the next key waits for a frame
that proves where it landed instead of composing against the deleted word.

Keys typed faster than the client reports them overlap: only the first raw key
of a run is repairable from the frame that confirms it. When that frame shows
the word before the caret consisting of exactly the letters typed since the
word started, still raw (`D`, `d` typed right after Ctrl+Backspace show `Dd`),
those letters are retyped as fresh keys and the word is repaired to the result
(`Đ`). A word that also holds earlier text is left as it is.

On KWin im-v1 a Shift-level letter that daklak passes through is sent as
committed text, because a forwarded key loses its Shift level. The commit lands
at the client's caret just like the raw key, so it keeps the key's pending
confirmation.

The two-second idle policy is tier-specific. `ForwardKey` resets the engine and
clears its unverifiable shadow because it has no surrounding frames to report a
cursor move. `SurroundingText` retains continuous engine state: a click during
the pause arrives as a frame, which revokes or reseeds the composition. Idle
keeps live composition whether or not the client has echoed it yet (Firefox
does not echo daklak's edits until the next input, but reports a click at
once, so `six`, a pause, then `x` still gives `six`). It only defers context
derived from a frame or following a forwarded edit key, which was never
confirmed by daklak's own key stream.

`Composer::feed_key` owns the key-path mutations. It first snapshots immutable
`FeedKeyFacts`; pure `plan_key_start` and `plan_key_finish` functions then
compute the transition without borrowing or changing the Composer. `feed_key`
applies the selected engine, shadow, quirk, and confirmation mutations in one
ordered block. This keeps transition policy independently testable while making
the mutation owner explicit.

## Failure boundaries

Expected first-party operating errors are not silently discarded. Active I/O
errors are propagated with context where the current operation cannot continue;
best-effort cleanup and disconnected observers are classified and logged; and
optional IPC connection and tray tasks are owned and supervised. Failure of an
optional subsystem disables that subsystem while composition continues. Task
completion and `JoinError` values are observed, including during shutdown.

Programmer-invariant violations remain assertions or explicit panics. Release
builds use Rust's unwinding behavior, and the daemon does not try to continue
from possibly corrupted mutable session state with `catch_unwind`; restarting
such a process belongs to the launcher. This boundary does not promise recovery
from out-of-memory aborts, `SIGKILL`, stack overflow, undefined behavior, kernel
failure, or an abort inside a dependency.

## Crate responsibilities

### `engine`
Wraps the vendored vnkey engine and exposes the linguistic operations the
Composer needs. No I/O, no Wayland, no transport policy.

A word that stops being Vietnamese is restored to raw letters: either vnkey
restores it, or a key makes it longer than 7 letters on screen, the length of
the longest Vietnamese word (`nghiêng`; corrections such as a changed tone do
not count against it). Either way the wrapper writes the raw letters of the
word **as it was on screen**, plus the new key, then passes keys through
unchanged until vnkey's next word break. Each glyph is spelled as the user
could have typed it (`ư` as `uw`, `w` or `u`), choosing the first spelling
that is a subsequence of the keys actually typed. So undone keys never come
back: `corrrect` showed `correct`, and `i` gives `correcti`, not `corrrecti`;
`hanhjf` showed `hành`, and `p` gives `hanhfp`. vnkey's own restore would put
back its whole key history. Seeding a word longer than the cap passes keys
through the same way. Separately, one word may
feed vnkey at most 32 keys before its key history is compacted to keys that
reproduce the word on screen: vnkey's non-Vietnamese restore copies the whole
history into a fixed 128-entry buffer, so tone toggling on a short word would
otherwise overflow it.

### `edit-strategy`
The policy layer, independent of any transport:
- `BackspaceMethod` enum and `detect_method()` — tier selection.
- `CapabilityProbe` — the inputs to tier selection.
- `OutputSink` trait — the abstract edit interface the brain calls.
- The shadow buffer and its invalidation rules.

### `daemon`
- `composer::Composer` — the transport-neutral composition core.
- `handler::{Daemon, Router}` — implements per-transport callbacks
  (Wayland handler, `activate_ibus` / `activate_evdev`, etc.) and routes keys.
- `control` / `ipc` / `tray` — the always-on control plane.
- `config` — configuration and per-app routing.
- `main` — CLI parsing and transport selection.
- `quirks/` — private module group for removable app/protocol workaround state.
  Quirk modules must not own Vietnamese composition, shadow-buffer delete-span
  math, or transport emission. The IBus quirk answers whether repeated missing
  surrounding-text echoes require a downgrade. Cached byte-delete safety is
  handled directly by the composer, without a Firefox-specific classifier.

### `wayland-adapter`
Owns all Wayland wire concerns: input-method v1 and v2 protocols, the virtual
keyboard, the xkb keymap upload, the `TransportProfile` capability snapshot, and
the tier emit paths. See [Transports](transports.md) and
[Capability model](capability-model.md).

### `evdev-adapter`
Grabs a keyboard device (`EVIOCGRAB`), decodes via the system xkb layout, and
emits via `uinput`. Used as the universal fallback and switchable against the
native backend at runtime. Requires Daklak's custom xkb layout so emitted
keycodes decode to Vietnamese characters.

### `ibus-adapter`
A full IBus engine: registers on the IBus bus, handles `process_key_event`,
and commits text / forwards keys through the IBus protocol (including the
`IBusText` GVariant encoding).

### `key-emitter` & `focus`
Cross-transport support layers — the key-synthesis backends and the
focus-tracking backends respectively. See [Key emit & focus](key-emit-and-focus.md).

## Next

- [Backspace tiers](backspace-tiers.md) — the mechanism the whole design serves.
