# IBus lifecycle probe

Ephemeral dev spike — **not part of the product**. It answers, on a real IBus
session, whether daklak's IBus transport can be torn down and rebuilt
in-process without ibus-daemon killing the daemon. This is the load-bearing
assumption behind runtime native↔evdev backend switching (plan12, Task 5/6).

- Binary: [`src/bin/ibus_lifecycle_probe.rs`](src/bin/ibus_lifecycle_probe.rs)
- Component template: [`daklak-probe.component.xml.in`](daklak-probe.component.xml.in)
- Driver: [`ibus-lifecycle-probe.sh`](ibus-lifecycle-probe.sh)

The probe reuses daklak's **real** registration path from
`viet-ime-ibus-adapter` (`EngineState` + `Factory` + `resolve_ibus_address` +
`request_name`), so ibus-daemon runs the identical `bus/component.c`
spawn/lifecycle code as for production daklak. The engine is a no-op
passthrough: `process_key` always returns `ForwardRaw`, so while it is selected,
typing works normally — that is the "engine is alive" signal.

> **It does NOT type Vietnamese — that is expected.** The probe intentionally
> does zero conversion; it only proves the engine is *alive and forwarding*.
> When `daklak-probe` is selected you should see **plain Latin letters** appear
> as you type. That is success. Vietnamese output would come from the real
> daklak engine, not this probe. The only failure signal is keys going
> completely dead (nothing appears) — that means the engine wasn't selected or
> the process died.

## Questions it answers

- **Q1 — Survival:** when the engine drops its D-Bus connection (simulating a
  switch to the evdev grab backend), does ibus-daemon `SIGTERM` the process?
- **Q2 — Switch-back:** can the still-alive process reconnect + re-register and
  resume routing **without** the user re-selecting the engine, and **without**
  ibus spawning a second instance?

## Why it must be ibus-spawned

The ibus code path that could kill us (`bus_component_factory_destroy_cb` →
`bus_component_stop` → `kill(pid, SIGTERM)`, in `vendors/ibus/bus/component.c`)
only applies to a component whose child PID ibus tracks via `g_child_watch_add`
and for which a `BusFactoryProxy` exists (i.e. after `CreateEngine`). So a
faithful test requires:

1. ibus **spawns** the probe via the component `<exec>` (parent = `ibus-daemon`), and
2. a client actually **selects** the engine (triggers `CreateEngine`).

Running the binary by hand from a shell does **not** satisfy (1) — it connects
as an ordinary bus client, so the child-watch/factory-destroy path is never
exercised. Use `install` + engine selection for the authoritative result.

## Run (real GNOME/IBus session)

```sh
tools/probe/ibus-lifecycle-probe.sh install      # build, install component, ibus restart
ibus engine daklak-probe                          # select it; type to confirm alive
tools/probe/ibus-lifecycle-probe.sh pid           # parent MUST be ibus-daemon
tools/probe/ibus-lifecycle-probe.sh drop          # SIGUSR1: drop conn; watch survival 5s  (Q1)
tools/probe/ibus-lifecycle-probe.sh reconnect     # SIGUSR2: re-register; then retype       (Q2)
tools/probe/ibus-lifecycle-probe.sh log           # full timeline
tools/probe/ibus-lifecycle-probe.sh cleanup       # remove component + ibus restart
```

### Impersonation profile

By default the probe registers under distinct names
(`org.freedesktop.IBus.DaklakProbe` / engine `daklak-probe`) so it never clashes
with an installed daklak. To reproduce the **exact** production component
identity (`org.freedesktop.IBus.Daklak` / engine `daklak`) — e.g. to rule out
name-specific ibus behavior — use:

```sh
tools/probe/ibus-lifecycle-probe.sh install --impersonate
```

This refuses to run if a real `daklak.xml` component is found in the user or
system ibus component dirs. Only use it when daklak itself is **not** installed.
The lifecycle code path ibus runs is identical either way; impersonation only
changes the well-known name.

## Interpreting the output

The probe appends a timestamped, PID-tagged timeline to
`$XDG_RUNTIME_DIR/daklak-ibus-probe.log`. Key lines and what they mean:

| Log line / observation | Meaning |
| --- | --- |
| `registered … — awaiting CreateEngine` | Probe connected and owns the bus name. |
| `engine ACTIVATED …` | A client selected the engine → `CreateEngine`/enable reached us. Confirms the faithful (spawned + selected) scenario. |
| `drop` → PID stays `ALIVE` for 5s, no `SIGTERM received` line | **Q1 = survives.** ibus did not kill us on connection drop. Matches the vendored-source reading (XML component, `destroy_with_factory` = false). → drop-and-reconnect design is viable. |
| `drop` → `PID … GONE` and/or `SIGTERM received — ibus-daemon … killed` | **Q1 = killed.** ibus tears the process down on drop. → must use the *keep-connection passthrough* design instead (never drop the IBus connection; let the evdev `EVIOCGRAB` silence the ibus path). |
| `re-registered … OK — switch-back path VIABLE` **and** typing routes again without re-selecting, and `pid` still shows a single instance | **Q2 = viable.** In-process switch-back works. |
| `re-register FAILED …`, or typing dead until re-select, or `pid` shows a second instance | **Q2 = needs work.** Switch-back can't reattach the input context in-process; revisit the design. |

## Feeding the result into plan12

- **Q1 survives + Q2 viable** → plan12's drop-and-reconnect assumption holds; fix
  the misleading "ibus … SIGTERMs the process … park here forever" comment in
  `crates/ibus-adapter/src/engine.rs` to reflect that only ibus-daemon shutdown
  SIGTERMs the component.
- **Otherwise** → adopt the keep-connection passthrough alternative for the IBus
  case before implementing Task 5/6.

## Cleanup / notes

- `cleanup` removes the component file, SIGTERMs any running probe, and restarts
  ibus.
- This directory is a scratch/dev-tools area; nothing here is referenced by
  product code or `docs/`.
