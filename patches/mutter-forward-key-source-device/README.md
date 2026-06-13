# Mutter ForwardKeyEvent source-device preload

Local GNOME/Mutter experiment for daklak IBus development. Not a daklak feature.

## Problem

GNOME Shell receives IBus `ForwardKeyEvent` and calls Mutter's
`clutter_input_method_forward_key()`. Mutter 50.x builds the synthetic Clutter
key event with a `NULL` source device, but `clutter_event_key_new()` rejects
that:

```text
clutter_event_key_new: assertion 'CLUTTER_IS_INPUT_DEVICE (source_device)' failed
```

The event is therefore never created, so it never reaches the native Wayland
client. (The "silent drop" of forwarded BackSpace seen on GNOME is this
assertion failing.)

## Why plain symbol interposition does NOT work

Exporting overrides of `clutter_input_method_forward_key()` /
`clutter_event_key_new()` from an `LD_PRELOAD` object has no effect here, for
two independently verified reasons:

1. **GI bypasses the global scope.** gnome-shell calls `forward_key` through
   GObject-Introspection, which resolves the C symbol with
   `g_module_symbol()` → `dlsym(module_handle, …)`. `dlsym` on a `dlopen`'d
   handle searches that object's own scope, not the global `LD_PRELOAD` scope,
   so the preloaded override is never chosen.

2. **The internal call is locally bound.** `libmutter-clutter-18` is built
   `-fno-semantic-interposition`, so the same-DSO `forward_key → event_key_new`
   call binds to the local definition at link time. `readelf -rW` shows **zero**
   dynamic relocations naming `clutter_event_key_new` — there is no
   interposable PLT/GOT slot for that call.

## What this object actually does: runtime inline detour

`mutter-forward-key-source-device.c` does not rely on symbol resolution. Its
constructor:

1. `dlopen("libmutter-clutter-18.so.0", RTLD_NOW | RTLD_NOLOAD)` — acts only in
   processes where the library is already mapped (gnome-shell). Every other
   process in the session is left untouched.
2. Resolves the real `forward_key`, `clutter_event_key_new`, and the seat /
   backend / event helpers by address from that handle.
3. Overwrites the first 16 bytes of `forward_key` with
   `endbr64; movabs rax, &handler; jmp rax`, redirecting all callers (including
   GI's indirect call) into our full reimplementation.

The reimplementation calls the **real** `clutter_event_key_new()` by resolved
address with the seat's virtual source pointer as the source device, so the
assertion passes and the event is created and `clutter_event_put()`.

### CET / IBT

gnome-shell and `libmutter-clutter-18` are built with IBT (Indirect Branch
Tracking). GI's call into `forward_key` is indirect, so the patched entry must
still start with `endbr64`; the detour preserves it. The handler is built
`-fcf-protection=full` so it has its own `endbr64`, making the `jmp rax` into it
IBT-safe too.

## ABI scope

ABI-specific to `/usr/lib/mutter-18` (mutter 50.x, GCC 16, x86-64 SysV).
Rebuild after any GNOME/Mutter upgrade and re-verify the prologue bytes.

## Build

```sh
./patches/mutter-forward-key-source-device/build.sh
```

Installs to `~/.local/lib/daklak/libmutter-forward-key-source-device.so`.
It links only against `libc` (no clutter/glib); all Clutter symbols are
resolved at runtime.

`build.sh` **self-verifies after compiling**: it parses the target soname from
the `.c`, locates that library on this machine, builds a probe that load-time
links it, preloads the freshly built object, and asserts the detour actually
installs (`endbr64` preserved + `movabs`/`jmp` shape). The build **fails** if
the soname is absent or the patch does not take — i.e. it catches an ABI drift
(soname bump or changed `clutter_event_key_new` signature) at build time
instead of silently at next login. Skip with `DAKLAK_SKIP_VERIFY=1` when
building on a box without GNOME.

## Enable for your GNOME session

Add before `exec gnome-session` in `/usr/local/bin/greetd-gnome2`:

```sh
export LD_PRELOAD="$HOME/.local/lib/daklak/libmutter-forward-key-source-device.so${LD_PRELOAD:+:$LD_PRELOAD}"
```

Then restart the GNOME session.

## Verify

The detour install is already checked at build time (`build.sh` fails if the
patch does not take against the real library). After login:

```sh
# constructor ran and patched forward_key:
grep "installed inline detour" /tmp/daklak-mutter-forward-key.log

# the assertion no longer fires when daklak forwards BackSpace:
journalctl --user -b --since "5 minutes ago" | grep -i CLUTTER_IS_INPUT_DEVICE
```

Then trigger daklak IBus ForwardKey BackSpace into a native Wayland client and
confirm the deletion lands.

## Result: live-proven (2026-06-13)

Confirmed working on a live GNOME 50.2 / mutter 50.2 session (Arch):

- `grep "installed inline detour" /tmp/daklak-mutter-forward-key.log` shows the
  constructor patched `forward_key` in each gnome-shell process.
- The `CLUTTER_IS_INPUT_DEVICE` assertion no longer appears in the journal.
- Daklak's forwarded BackSpace **lands in native Wayland clients** (foot etc.).

This answers the prior open questions: the repaired event with a *pointer*
source device survives Mutter's later event filtering, reaches
`meta_wayland_keyboard_broadcast_key()`, and is accepted by the native client.
A keyboard-class source device is **not** required — the seat's virtual source
pointer is enough.

## Portability — will this work on another distro (e.g. Ubuntu)?

**No, not as-is. It must be rebuilt and adapted per target, and it may not apply
at all.** This is an ABI- and version-pinned hack, not a portable binary. The
binary built on Arch will not work on Ubuntu. Things that are coupled to the
exact mutter build:

1. **Library soname is hardcoded.** The constructor does
   `dlopen("libmutter-clutter-18.so.0", …)`. The `18` is mutter 50's Clutter API
   version. Ubuntu ships an older GNOME, so the soname differs (e.g. mutter 46 →
   `libmutter-clutter-14`, 47 → `-15`, 48 → `-16`). On Ubuntu the `dlopen`
   returns NULL and the detour silently never installs. You must change the
   soname in the `.c` to match the target.

2. **`clutter_event_key_new` ABI is version-volatile.** The handler calls it by
   resolved address using a hand-written prototype (arg order + the 12-byte
   `ClutterModifierSet` passed by value). Mutter's Clutter event API changes
   across releases — devices fields dropped, signatures reshuffled — and on
   older mutter this function may not exist at all (events were built a
   different way). If the target's signature differs, the call marshals garbage
   and crashes gnome-shell. Verify against the target's headers / disassembly.

3. **The bug must actually exist on that version.** The `NULL`-source-device
   assertion is specific to this `forward_key` implementation. Older mutter may
   use a different code path — in which case the patch is either unnecessary or
   simply wrong. Confirm the `CLUTTER_IS_INPUT_DEVICE` assertion appears in the
   target's journal *before* bothering.

4. **x86-64 only.** The detour writes raw `endbr64; movabs rax; jmp rax`
   opcodes. On an arm64 Ubuntu install those bytes are nonsense. (CET/`endbr64`
   itself is harmless if the target lacks IBT, and required if it has it — that
   part already adapts.)

### Adapting it to Ubuntu (or any other GNOME)

1. On the target box: `ls /usr/lib*/mutter-*/libmutter-clutter-*.so.0` (or
   `gnome-shell --version`) to learn the soname.
2. Change the `dlopen` soname in `mutter-forward-key-source-device.c` to match.
3. Confirm `clutter_input_method_forward_key` still passes `NULL` and that the
   `clutter_event_key_new` signature matches (check that distro's mutter source
   for the same GNOME version).
4. Rebuild **on the target** (or against its mutter) with `build.sh`.
5. Run the pre-flight byte-pattern check and the live `Verify` steps above.

Treat each distro/version as a fresh port, not a copy.
