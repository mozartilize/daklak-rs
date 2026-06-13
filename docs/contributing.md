# Contributing

[← Back to index](../README.md)

Conventions for maintainers working in this codebase.

## Contents

- [Mental model first](#mental-model-first)
- [Where things live](#where-things-live)
- [Invariants — do not break these](#invariants--do-not-break-these)
- [Adding support for a new compositor](#adding-support-for-a-new-compositor)
- [Adding a per-app workaround](#adding-a-per-app-workaround)
- [Testing](#testing)

## Mental model first

Before changing anything, hold these three facts:

1. **No preedit.** Every result is committed directly; corrections delete and
   re-commit. ([Overview](overview.md))
2. **One brain, three transports.** Composition logic is transport-neutral; the
   adapters are thin wire glue. ([Architecture](architecture.md))
3. **Capabilities, not compositor names.** Decisions read capability facts from
   the transport profile. ([Capability model](capability-model.md))

## Where things live

| You want to change… | Go to |
| ------------------- | ----- |
| The Vietnamese composition behavior | `engine` (wraps vendored vnkey — never edit `vendors/`) |
| When/which tier is chosen | `edit-strategy` (`detect_method`) |
| How an edit is executed on the wire | the relevant transport adapter's sink |
| Composition state / shadow / reseed | `daemon::composer` |
| Key routing per transport | `daemon::handler` |
| Capability detection | `wayland-adapter` (`TransportProfile`) |
| Focus tracking | `focus` |
| Key synthesis | `key-emitter` |
| CLI / mode selection / config | `daemon::{main, config}` |
| Control plane (toggle/IPC/tray) | `daemon::{control, ipc, tray}` |

## Invariants — do not break these

- **The `OutputSink` boundary.** The brain must not learn which compositor it is
  talking to. Keep transport-specific logic in the adapters.
- **Profile is fixed at connect.** `TransportProfile` is captured once and not
  re-detected mid-session. Don't sprinkle live capability probes through the
  decision paths.
- **Branch on capabilities, not protocol names.** Prefer capability facts over
  protocol identity when choosing behavior.
- **The re-seed gate.** Navigation and shortcut keys must not be treated as
  composing activity, or mid-word reseeds break. Be careful around any code that
  touches "last action" timing in the composer.
- **Don't accumulate per-app hacks.** See
  [the philosophy note](compositor-quirks.md#a-note-on-philosophy).
- **Never touch `vendors/`.** It's vendored upstream source (vnkey, etc.), kept verbatim for reference and patch authoring. Read it, never
  change/write it.

## Adding support for a new compositor

1. Identify which input-method protocol it exposes, or whether it needs IBus or
   evdev.
2. If it's a new Wayland protocol surface, add a transport profile with honest
   capability facts. Add a unit test asserting the profile (no compositor needed).
3. Wire focus tracking by adding or selecting a focus backend/source.
4. Make sure tier selection and emit read the new capabilities rather than the
   protocol name.

## Adding a per-app workaround

Only after confirming it's a genuine client/compositor bug **and** that a
correctly-behaving client uses the same path:

1. Document the behavior in [Compositor quirks](compositor-quirks.md).
2. Add the narrowest possible config-driven override (an app-list flag), not a
   hard-coded special case.
3. Default it conservatively.

## Testing

```sh
cargo test                  # core + wayland
cargo test --features ibus  # IBus-gated tests too
```

Favor unit tests that don't need a live compositor — the `TransportProfile`
capability matrix and `detect_method` tier selection are both designed to be
testable in isolation. Prefer adding a profile/selection test over a manual
end-to-end check where possible.
