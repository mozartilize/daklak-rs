# Wayland input-method protocol landscape

[← Back to index](../README.md)

Daklak has one native Wayland transport, but that transport has to accommodate
several incompatible input-method protocol designs. Their version numbers do not
form a simple progression: each design changes the division of responsibility
between the input method, compositor, focused application, and optional keyboard
injection protocols.

This page records the protocol differences that matter to Daklak, the upstream
design rationale behind them, and the resulting support boundaries. Protocols
marked experimental or draft can still change incompatibly.

## Contents

- [Executive summary](#executive-summary)
- [Protocol provenance and status](#protocol-provenance-and-status)
- [Comparison](#comparison)
- [Input-method unstable v1](#input-method-unstable-v1)
- [Unofficial input-method unstable v2](#unofficial-input-method-unstable-v2)
- [What v2 actually improved](#what-v2-actually-improved)
- [Why v2 is harder for Daklak](#why-v2-is-harder-for-daklak)
- [Virtual keyboard and the legacy-client boundary](#virtual-keyboard-and-the-legacy-client-boundary)
- [Experimental xx-input-method-v2](#experimental-xx-input-method-v2)
- [Experimental xx-keyboard-filter-v1](#experimental-xx-keyboard-filter-v1)
- [Draft input-method-v3 from MR !368](#draft-input-method-v3-from-mr-368)
- [Implications for Daklak](#implications-for-daklak)
- [Recommended capability model](#recommended-capability-model)
- [Primary sources](#primary-sources)

## Executive summary

- **Input-method v1 is the best integrated active-context keyboard contract for
  Daklak today.** Its context combines keyboard interception, raw forwarding,
  keysym output, and text operations. It still cannot help an application that
  never activates text-input, because no context exists in that case.
- **Unofficial input-method v2 has a cleaner text-state model but a worse
  keyboard contract for Daklak.** It adds persistent per-seat state,
  double-buffered frames, explicit edit commits, text-change causes, repeat
  metadata, and a candidate popup. It removes v1's scoped `key`, `keysym`, and
  modifier forwarding, so Daklak needs the separate wlroots virtual-keyboard
  protocol to pass through or synthesize keys.
- **`xx-input-method-v2` is the upstream experimental redesign.** It retains the
  useful v2 text transaction, clarifies deletion and state rules, and adds
  cursor/action and richer popup facilities. It deliberately leaves keyboard
  interception to `xx-keyboard-filter-v1`.
- **`xx-keyboard-filter-v1` fixes ordinary raw passthrough without granting
  arbitrary input generation.** The compositor queues the original event and
  the input method chooses consume or passthrough. It does not synthesize
  Backspace, replacement characters, or text for clients without text-input.
- **MR !368's draft input-method-v3 contains useful ideas but is not an upstream
  staging protocol.** Its direct `key_forward(serial)` and `activate(app_id)`
  would help Daklak, but the XML is unfinished and its filtering contract is
  less rigorous than the later keyboard-filter protocol.

The practical ordering is therefore not `v1 < v2 < xx`. It is:

```text
v1
  strongest integrated keyboard capabilities while a context is active

unofficial v2 + virtual keyboard
  cleaner text state, but a more complex and compositor-specific key path

xx input method + keyboard filter
  best long-term standards architecture for normal text-input clients,
  but intentionally incomplete for synthetic and legacy compatibility
```

## Protocol provenance and status

### Input-method unstable v1

[`input-method-unstable-v1`](https://wayland.app/protocols/input-method-unstable-v1)
is part of upstream `wayland-protocols`. Daklak uses this family on the
KWin/Plasma path. GNOME/Mutter is served through Daklak's IBus transport, not a
Wayland input-method protocol — see [Transports](transports.md#ibus--gnome).

### Input-method unstable v2

The commonly deployed
[`input-method-unstable-v2`](https://wayland.app/protocols/input-method-unstable-v2)
XML is not in current upstream `wayland-protocols`. It is an unofficial protocol
from the wlroots/Smithay ecosystem with copies in other compositor and IM
projects. It was proposed upstream in 2018–2019 but did not merge. Daklak uses
this protocol on its wlroots path.

The original proposal described v2 as v1 reshaped to mirror text-input-v3 and
newer `wl_keyboard` behavior. See the
[Wayland development thread](https://lore.freedesktop.org/wayland-devel/20181109184050.27c3c55b.dorota.czaplejewicz@puri.sm/T/).

### Experimental xx input method

[`xx-input-method-v2`](https://wayland.app/protocols/xx-input-method-v2)
is the current upstream experimental successor. The protocol file is called v2,
its manager is `xx_input_method_manager_v2`, and its current input-method object
is `xx_input_method_v1`; these numbers belong to different protocol/interface
versioning layers.

[MR !397](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/397)
started by copying unofficial v2, then deliberately removed controversial or
insufficiently understood functionality so the core text-input relay could
progress independently. Follow-up work remains active, so the current XML is
not a settled replacement.

### Virtual keyboard

[`virtual-keyboard-unstable-v1`](https://wayland.app/protocols/virtual-keyboard-unstable-v1)
is a wlroots-family protocol rather than an upstream Wayland protocol. A
separate upstream proposal,
[ext-virtual-keyboard MR !211](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/211),
scopes virtual keyboards primarily to on-screen keyboards and remote desktop,
not to repairing legacy applications on behalf of an input method.

### Draft input-method-v3

[MR !368](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/368)
contains a file named `staging/input-method/input-method-v3.xml`, but the MR is
open and draft. The path is a proposal inside the source branch; the protocol
has not entered upstream staging. The findings below refer to commit
[`398359a2e49a78a9f21003451ad6c0d4c8931f66`](https://gitlab.freedesktop.org/rano/wayland-protocols/-/blob/398359a2e49a78a9f21003451ad6c0d4c8931f66/staging/input-method/input-method-v3.xml).

## Comparison

| Area | Input-method v1 | Unofficial input-method v2 | Experimental xx input method | MR !368 input-method-v3 |
| --- | --- | --- | --- | --- |
| IM lifecycle | New context per activation | Persistent per-seat active/inactive object | Persistent per-seat active/inactive object | Persistent object; proposes several IMs |
| State synchronization | Latest `commit_state` serial on output requests | Double-buffered input frames ending in `done`; output applied by `commit(serial)` | Refined v2 model with compatibility-dependent serial rules | Mostly the unofficial v2 transaction |
| Delete API | Signed index plus length | Explicit bytes before/after cursor | Before/after with selected-text and UTF-8 boundary rules | Mostly unofficial v2 semantics |
| Keyboard interception | Context `grab_keyboard()` | Separate exclusive keyboard-grab object | Not in core protocol | Key events directly on IM object |
| Raw passthrough | `ctx.key()` and `ctx.modifiers()` recreate/relay events | No relay request; normally needs virtual keyboard | Separate keyboard filter passes original event | `key_forward(serial, type)` passes original event |
| Arbitrary key output | Scoped `ctx.keysym()` and `ctx.key()` | None in IM protocol | None by design | None |
| Repeat | Grabbed `wl_keyboard` behavior | Dedicated grab with guaranteed `repeat_info` | Keyboard filter prefers compositor-side repeat | IM receives repeat data; forwarding has repeating/non-repeating hint |
| App identity | None in activation | None | None in current contract | Target app ID on activation and destruction |
| Multiple IMs | One object per seat | One object per seat | Current contract uses unavailable/one-per-seat semantics | Several may run; compositor associates one per text input |
| Cursor movement | `cursor_position` tied to `commit_string` | Preedit cursor only | General `move_cursor` and selection | Preedit cursor only |
| Preedit styling/language | Styling, language, direction | Removed | Not present in current contract | Restored/extended |
| Popup | Input-panel interfaces, mainly OSK-oriented | Simple compositor-positioned popup | Dedicated XDG-style positioner/configure/reposition protocol | Existing parentless `xdg_popup` adopted by compositor |
| No-text-input clients | Not supported: no context | Not supported by IMv2 itself; Daklak uses VK workaround | Deliberately left to compositor/fallback | Not supported |

## Input-method unstable v1

### Lifecycle and state

`zwp_input_method_v1.activate` creates a short-lived
`zwp_input_method_context_v1`. Deactivation names that context, and the context
should be destroyed after deactivation. State does not survive the session.

The text-input side sends `commit_state(serial)`. Output operations such as
`commit_string`, `preedit_string`, and `keysym` include the latest serial so the
client can ignore output based on stale state. There is no IM-side batch
`commit` request.

Daklak adapts `commit_state` into the same internal frame boundary used by its
v2 `done` handler. See
[`dispatch_v1.rs`](../crates/wayland-adapter/src/dispatch_v1.rs).

### Text operations

The context provides:

- `commit_string(serial, text)`;
- preedit string, styling, and cursor requests;
- `delete_surrounding_text(index, length)`;
- `cursor_position(index, anchor)` processed with `commit_string`;
- surrounding text, reset, content type, invoke action, and preferred language.

Daklak converts its transport-neutral before/after byte counts to v1's negative
index plus total length in
[`sink.rs`](../crates/wayland-adapter/src/sink.rs).

### Integrated keyboard relay

The context also provides:

- `grab_keyboard()` returning a `wl_keyboard`;
- `key()` and `modifiers()` for events the IM did not consume;
- `keysym()` for scoped synthetic keysym output;
- `modifiers_map()`.

This makes v1 effectively a combined active-context keyboard grab, implicit
filter, relay, keysym emitter, and text-operation channel. Daklak can consume a
physical key by not forwarding it, pass an ordinary key through the context,
and use the context keysym path for ForwardKey replacement output.

This capability exists only while a text-input context is active. Applications
that never activate text-input do not create a context, so v1 does not solve
that compatibility case.

### Other v1 facilities

V1 includes language and text-direction output and input-panel interfaces. The
input-panel interfaces are oriented toward implementing keyboards, not a modern
candidate popup lifecycle.

## Unofficial input-method unstable v2

### Intended redesign

The original v2 proposal made these explicit changes from v1:

- attach an input method to a chosen seat;
- replace per-activation contexts with one persistent object;
- mirror text-input-v3 requests and events;
- add double buffering and explicit transaction boundaries;
- remove language indicators and v1's reset fallback text;
- remove cursor movement outside preedit;
- remove keyboard-event sending from the IM object;
- replace general input-panel surfaces with a compositor-positioned popup.

### Persistent, double-buffered state

The per-seat input-method object transitions between active and inactive.
Compositor-to-IM state is accumulated and applied atomically by `done`. IM output
is accumulated through `commit_string`, `set_preedit_string`, and
`delete_surrounding_text`, then applied with `commit(serial)`.

The serial is the number of `done` events seen. A stale serial should not prevent
the edit from being processed, but should prevent that edit from changing the
input-method object's current state.

The commit order is explicitly defined: replace old preedit with the cursor,
delete surrounding text, insert committed text, calculate surrounding text,
install new preedit, then place its cursor.

### Cleaner text API

Deletion uses explicit byte lengths before and after the cursor rather than
v1's signed index and total length. V2 also aligns surrounding text, content
type, and text-change cause with text-input-v3.

Daklak already hides the v1/v2 deletion difference behind one `OutputSink`, so
the cleaner wire API does not materially simplify the engine. The explicit v2
batch matches Daklak's delete-then-commit edit plan, but v1's serial-ordered
operations already work; this is a structural improvement rather than a proven
user-visible atomicity gain.

### Dedicated exclusive keyboard grab

V2 moves keyboard events to a child keyboard-grab object with keymap, key,
modifiers, and repeat-info events. Repeat information is guaranteed before the
first key event.

The critical contract is that once an event is delivered to the grab holder,
the compositor must not process it further. Unlike v1, the input-method object
has no `key`, `keysym`, or modifier-forwarding requests. The input method must
use another protocol to recreate raw passthrough and synthetic output.

### Popup and availability

V2 adds a popup surface placed near the text-input rectangle and tied to active
state. It also adds an `unavailable` event for duplicate or unusable per-seat
input-method objects.

## What v2 actually improved

For Daklak, the genuine improvements are:

1. **Cleaner lifecycle:** one persistent object rather than repeated child
   contexts.
2. **Explicit transactions:** `done` and `commit(serial)` define frame and edit
   boundaries.
3. **Text-input-v3 alignment:** content-purpose values and text-change cause
   require less translation and can distinguish external edits.
4. **Clearer deletion:** explicit before/after byte counts.
5. **Repeat metadata:** guaranteed repeat information on the dedicated grab.
6. **Candidate popup:** a better model for candidate/preedit-heavy IMEs than
   v1's input panel.
7. **Availability signaling:** explicit failure/inert state for duplicate or
   removed-seat objects.

Most of these improve protocol structure, implementation clarity, or candidate
UI. Daklak is a commit-oriented Vietnamese IME with a transport-neutral edit
engine, so few are user-visible.

## Why v2 is harder for Daklak

### Loss of scoped forwarding

V1 bundles interception and forwarding. V2 intercepts but does not forward. On
Daklak's wlroots path this requires both:

```text
zwp_input_method_v2
+ zwp_virtual_keyboard_v1
```

Daklak forwards ordinary keys and modifiers through the virtual keyboard and
uses a synthetic keymap for replacement characters when the text-input commit
channel is unavailable. See
[`wayland-adapter/src/lib.rs`](../crates/wayland-adapter/src/lib.rs) and
[`key-emitter`](../crates/key-emitter/).

This introduces complexity around:

- synthetic keymap upload and safe keycode allocation;
- modifier clearing/restoration;
- press/release pairing;
- client-side versus compositor-side repeat;
- raw passthrough equivalence;
- XWayland and toolkit keycode behavior.

### Heartbeat coupling

Daklak's v2 path emits a bare input-method commit when a `done` frame produces no
output. Without it, Sway may not acknowledge the corresponding text-input-v3
frame, after which clients such as Chromium can stop updating surrounding text.
The workaround is in
[`wayland-adapter/src/lib.rs`](../crates/wayland-adapter/src/lib.rs).

This is deployed compositor/protocol coupling rather than a benefit of v2's
transaction model.

### Capability inference remains

V2 adds text-change cause but does not tell Daklak that surrounding-text updates
will remain functional. Daklak still observes frames and uses liveness checks to
select or downgrade its backspace strategy. See
[Backspace tiers](backspace-tiers.md) and
[Capability model](capability-model.md).

## Virtual keyboard and the legacy-client boundary

The upstream design position is that virtual keyboard should not be required so
an input method can compensate for applications lacking text-input. Compatibility
with those applications belongs in the compositor.

This is a sound responsibility boundary:

- the compositor owns focus, seat and authoritative keyboard state;
- the compositor can enforce privilege and security policy;
- it owns XWayland and any toolkit-specific bridges;
- it can preserve event ordering, keymap and repeat semantics.

It is not a complete deployment solution. A pure Wayland client without
text-input has no generic "insert this UTF-8 text" operation. A compositor-side
bridge would have to create an internal text-input facade, translate commits to
keyboard/keymap behavior, use XWayland integration, or accept that the client
does not support IM input.

Daklak currently crosses that boundary pragmatically on wlroots:

1. A client without text-input never sends an activation.
2. Daklak infers a ForwardKey session from focus metadata.
3. It sends Backspace and replacement characters through virtual keyboard and
   its synthetic keymap.

That path reaches clients described in [Backspace tiers](backspace-tiers.md),
but it is wlroots-specific, more privileged than normal IM operation, and
exposed to keymap/modifier/repeat edge cases. It should remain an optional
compatibility capability rather than the foundation of portable Wayland IM
support.

The upstream
[ext-virtual-keyboard proposal](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/211)
focuses instead on on-screen keyboard and remote-desktop clients that genuinely
need synthetic keyboard input.

## Experimental xx-input-method-v2

### Core model

XX retains unofficial v2's persistent object, active/inactive lifecycle,
double-buffered input frames, and output commit transaction. It deliberately
strips keyboard handling from the core protocol.

### Text-operation improvements

The current protocol adds or clarifies:

- selected text must be removed as part of replacement;
- deletion ranges are adjusted to available text and UTF-8 boundaries;
- the complete ordered edit sequence;
- general cursor movement and selection through `move_cursor`;
- available actions and `perform_action`;
- maximum text/selection behavior;
- preedit and one-shot state reset/lifetime rules.

Current preedit persistence is more clearly specified, but it should not be
presented as entirely new: unofficial v2 already had persistent current preedit
state after a successful commit.

### Compatibility and features

XX can announce whether the compositor is relaying legacy text-input-v3 or the
experimental XX text-input contract, which changes serial handling:

- serial `0` for XX text input;
- done-count serial for legacy text-input-v3 compatibility.

It also supports optional-feature negotiation. In the current paired
XX text-input protocol, the advertised feature is primarily `move_cursor`; it
does not directly advertise that surrounding-text delivery is healthy. Daklak's
surrounding-text capability and liveness logic therefore remains necessary.

### Popup redesign

[MR !407](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/407)
added a dedicated popup protocol inspired by XDG popup behavior because the
unofficial v2 popup was considered too limited. The current design has a
positioner, configure/acknowledge sequencing, constraints, reactive placement,
and explicit repositioning.

### Other XX history

[MR !433](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/433)
added action and cursor/selection work motivated by mobile navigation without
emulating physical keypresses. Further proposals continue to change
compatibility, feature negotiation, and multi-IM behavior, so implementation
must follow the current XML rather than assume an older draft's semantics.

## Experimental xx-keyboard-filter-v1

### Consume or pass the original event

[`xx-keyboard-filter-v1`](https://wayland.app/protocols/xx-keyboard-filter-v1)
binds a `wl_keyboard` to an XX input method. The compositor sends intercepted
keyboard events to that keyboard and queues copies. For each serial-bearing key
event, the client answers:

- `consume`: remove the event without delivering it to the focused surface;
- `passthrough`: deliver the compositor's queued original event.

Responses must process the oldest queued event first. Non-keyboard-key events
without a suitable serial cannot be filtered. Unbinding immediately stops
interception and treats unanswered events as passthrough.

This fixes the most important raw-forwarding problem of unofficial v2: Daklak no
longer has to recreate an ordinary physical event through virtual keyboard.

### Deliberately limited authority

The protocol cannot generate arbitrary keys. That is intentional. The rationale
in
[keyboard-filter MR !465](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/465)
rejects grab-and-regenerate as the general IM design because it broadens the
protocol beyond input methods and has caused layout and shortcut failures in
less-tested configurations.

The filter therefore does not provide:

- synthetic Backspace;
- arbitrary replacement keycodes or keysyms;
- virtual-keyboard-style Vietnamese output;
- recovery from a dead text-input commit channel;
- a path for applications that never activate text-input.

It works best with compositor-side repeat from newer `wl_seat`; client-side
repeat mismatches remain a protocol concern.

### Daklak mapping

For a clean XX path, Daklak's key decisions conceptually map to:

```text
raw ordinary key       → passthrough
composition/apply key  → consume, then commit the text edit
```

The mapping is not just a switch on the current `KeyDecision` enum: some
`Consumed` paths currently emit navigation or shortcut keys through helper
calls, and press/release decisions must stay paired. A future adapter must carry
event serials through the decision and answer the filter queue in order.

## Draft input-method-v3 from MR !368

### Status

MR !368 is a WIP alternative proposal, not a merged staging protocol. Its draft
XML contains only two interfaces:

- `wp_input_method_v3`;
- `wp_input_method_manager_v3`.

Text operations, keyboard events, filtering, actions, styling, language, popup
hookup, and cursor rectangle are all placed on the main input-method object.

### Text-state model

The core text state remains close to unofficial v2:

- persistent active/inactive object;
- activate/deactivate;
- surrounding text, change cause, and content type;
- frames ending in `done`;
- double-buffered preedit, string, and deletion output applied by
  `commit(serial)`.

The proposal renames `commit_string(text)` to `set_string(text)`.

### Embedded keyboard filter

Instead of a child grab or separate filter protocol, the main object receives:

- keymap;
- repeat information;
- key events with serials;
- modifiers.

`key_forward(serial, non_repeating|repeating)` asks the compositor to deliver
the original event to the application. Not forwarding it is intended to consume
it. This would remove virtual-keyboard recreation for ordinary passthrough.

Compared with `xx-keyboard-filter-v1`, the draft is under-specified:

- no explicit consume response;
- no oldest-first queue contract;
- no unbind or pending-event lifecycle;
- no timeout/error behavior;
- repeat forwarding uses a one-bit hint and describes compositor-generated fake
  releases.

It still cannot generate arbitrary Backspace or replacement events.

### Multiple IMs and application identity

The manager explicitly permits several input methods to run, with one associated
with a text input at a time. `get_input_method` includes the IM's own app ID.
Activation includes the target application's app ID, and
`text_input_destroyed(app_id)` reports lifecycle cleanup.

For Daklak, target app ID on activation could replace much of the independent
focus probing needed for per-app routing—but only for applications that
activate text-input. It does nothing for no-text-input clients.

### Restored and added facilities

The draft restores or adds features absent from unofficial v2:

- language reporting;
- commit-or-clear behavior for visible preedit on focus/cursor changes;
- styled preedit ranges with underline, text color, and background color;
- available actions and action selection;
- an existing parentless `xdg_popup` adopted by the compositor;
- cursor rectangle information.

It does not include current XX facilities such as general `move_cursor`,
protocol compatibility, supported-feature negotiation, `unavailable`, or XX's
dedicated popup positioner/configure protocol.

### Companion text-input changes

The MR also carries a matching text-input draft with actions, language, styling,
and `process_keys`. `process_keys` lets a client request reprocessing of recent
keyboard events that caused a text field to become active, such as the first
characters in a type-to-search interface. Full adoption therefore requires
coordinated text-input/toolkit changes, not only a compositor and IM client.

### WIP quality

The XML visibly remains a draft:

- `commit` refers to the old `commit_string` name while the request is
  `set_string`;
- `set_string` refers to the wrong-side `wp_text_input_v3.commit`;
- preedit styling descriptions reference text-input event names rather than the
  input-method requests;
- deactivation refers to a nonexistent `wp_input_popup_surface_v3` interface.

These defects make it useful design evidence, not an implementation target.

### Daklak value and gaps

The two most attractive ideas are:

1. original-event forwarding by serial without virtual keyboard;
2. target app ID on activation.

Normal surrounding-text delete-plus-string commits map naturally. The proposal
still does not solve:

- ForwardKey's synthetic Backspaces;
- synthetic Vietnamese key output;
- dead text-input commit channels;
- applications with no text-input activation.

The later XX input-method plus separate keyboard-filter design is narrower but
more rigorous. App identity and multi-IM routing remain notable ideas from this
draft.

## Implications for Daklak

### One transport, orthogonal capabilities

These protocol families are not separate Daklak transports. They are wire
contracts and capabilities inside the one Wayland adapter. Protocol identity is
useful for setup and diagnostics, but engine policy should branch on what the
negotiated connection can do.

### Best fit by use case

#### Current active text-input on KWin/Plasma

Input-method v1 is the better integrated keyboard fit because context key and
keysym requests provide both raw relay and fallback emission.

#### Current wlroots compatibility

Unofficial input-method v2 plus virtual keyboard is necessary for Daklak's
present behavior. IMv2 alone cannot forward grabbed raw keys or emit synthetic
fallback output.

#### Future standards path

XX input method plus keyboard filter is the preferred architecture for clients
with a healthy text-input implementation:

- structured text transactions;
- original-event passthrough;
- less arbitrary input authority;
- improved deletion, cursor/action, and popup semantics.

It is not a complete replacement for ForwardKey or legacy-client compatibility.

### Backspace tiers

Daklak has two editing strategies:

1. **SurroundingText:** request exact deletion, then commit replacement text.
2. **ForwardKey:** synthesize Backspace events, then emit one whole replacement
   through a transport-selected channel.

XX plus keyboard filter supports the first cleanly. It can pass or consume the
physical composition key, but it cannot produce ForwardKey's synthetic
Backspaces. A compositor bridge, scoped synthetic-output protocol, or fallback
transport remains required.

### No-text-input applications

No input-method protocol can communicate with an application that never
activates text-input unless some other component creates a bridge. The options
are:

- compositor-owned compatibility;
- IBus/toolkit integration;
- Daklak's optional wlroots virtual-keyboard workaround;
- Daklak's evdev/uinput fallback;
- accepting that the application has no IM support.

## Recommended capability model

Keep protocol setup inside `wayland-adapter`, but represent downstream policy
with independent facts such as:

- can receive surrounding text;
- surrounding-text delivery is currently healthy;
- can commit strings;
- can delete surrounding text;
- can filter and passthrough original keyboard events;
- can emit arbitrary keycodes;
- can emit keysyms;
- can move cursor/change selection;
- app identity source;
- repeat ownership and repeat-event support;
- popup/preedit styling capabilities;
- compositor-provided legacy bridge availability.

Do not infer these capabilities solely from compositor name or a protocol's
version number. This follows the existing [Capability model](capability-model.md).

For current deployments:

- retain v1 on KWin/Plasma;
- retain unofficial v2 plus virtual keyboard on wlroots where compatibility
  requires it;
- keep evdev/uinput as the high-trust fallback;
- treat XX input method plus keyboard filter as a future negotiated capability
  path once compositor support and protocol stability are sufficient;
- do not implement MR !368 directly, though its app-ID and original-event
  forwarding requirements are worth preserving in future design discussions.

## Primary sources

### Protocol specifications

- [Input-method unstable v1](https://wayland.app/protocols/input-method-unstable-v1)
- [Input-method unstable v2](https://wayland.app/protocols/input-method-unstable-v2)
- [Virtual-keyboard unstable v1](https://wayland.app/protocols/virtual-keyboard-unstable-v1)
- [Experimental xx input method](https://wayland.app/protocols/xx-input-method-v2)
- [Experimental xx text input](https://wayland.app/protocols/xx-text-input-v3)
- [Experimental xx keyboard filter](https://wayland.app/protocols/xx-keyboard-filter-v1)

### Design history

- [2018–2019 input-method-v2 proposal thread](https://lore.freedesktop.org/wayland-devel/20181109184050.27c3c55b.dorota.czaplejewicz@puri.sm/T/)
- [MR !397: Start an input-method protocol](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/397)
- [MR !407: Define an XX input popup](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/407)
- [MR !433: Add actions and selection/navigation](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/433)
- [MR !465: New experimental keyboard-filter protocol](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/465)
- [MR !211: ext-virtual-keyboard proposal](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/211)
- [MR !368: Draft input method next](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/368)
- [MR !368 input-method-v3 XML at reviewed commit](https://gitlab.freedesktop.org/rano/wayland-protocols/-/blob/398359a2e49a78a9f21003451ad6c0d4c8931f66/staging/input-method/input-method-v3.xml)

### Daklak implementation and maintained documentation

- [`crates/wayland-adapter/`](../crates/wayland-adapter/)
- [`crates/key-emitter/`](../crates/key-emitter/)
- [Architecture](architecture.md)
- [Transports](transports.md)
- [Backspace tiers](backspace-tiers.md)
- [Capability model](capability-model.md)
- [Key emit and focus](key-emit-and-focus.md)
- [Evdev setup](evdev-setup.md)
