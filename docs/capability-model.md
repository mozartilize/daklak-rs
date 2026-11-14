# Capability model

[← Back to index](../README.md)

daklak decides *how* to edit based on what a transport can actually do, not on
which compositor it happens to be. This page describes the capability
abstraction that makes that possible.

## Contents

- [Capability over identity](#capability-over-identity)
- [One source of truth](#one-source-of-truth)
- [The orthogonal axes](#the-orthogonal-axes)
- [How it is used](#how-it-is-used)

## Capability over identity

The principle: **branch on capabilities, not on compositor names.**

A single "which backend am I" flag is tempting but quickly fuses unrelated
concerns — the protocol version, how focus is tracked, whether a virtual
keyboard exists — into one value, and every new compositor either fits an
existing label or forces a new one. daklak instead exposes a small set of
**independent capability facts**, fixed once at connect time, and lets each
decision read exactly the fact it needs.

## One source of truth

The Wayland adapter negotiates its channels **once at `connect()`** and keeps
them in one connection object that owns the protocol objects. Capability facts
are questions answered by that object, not a separate copy of what was
negotiated, so they cannot drift from what is actually bound. The daemon reads
them through the adapter context. Read the `wayland-adapter` crate when
changing what can be asked.

## The orthogonal axes

The three things that used to be conflated are now separate:

### Protocol identity

Which input-method protocol was negotiated. Identity is useful for reporting and
protocol-specific setup, but downstream policy should avoid branching on it when
a capability fact is available.

### Focus source

How the focused application is discovered. Focus tracking is independent from the
input-method protocol; a new compositor may pair an existing protocol with a new
focus source.

### Capability facts

What the transport can actually do, for the decisions that need to know:
whether replacement text can be typed as keysyms, and whether a session can be
created from focus alone (the keyboard grab exists outside a text-input session
and the virtual keyboard carries Daklak's synthetic keymap). Keep this list
conceptual in docs; the source and tests define the exact set.

## How it is used

- **Tier selection** consults capabilities before choosing a deletion mechanism.
- **Emit path** chooses a backend based on what the active transport exposes.
- **Focus-driven routing** uses the focused app id when a focus source exists.

## Next

- [Key emit & focus](key-emit-and-focus.md) — the emit backends and focus
  backends referenced here.
