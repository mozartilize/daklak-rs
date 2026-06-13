# Capability model

[← Back to index](../README.md)

daklak decides *how* to edit based on what a transport can actually do, not on
which compositor it happens to be. This page describes the capability
abstraction that makes that possible.

## Contents

- [Capability over identity](#capability-over-identity)
- [TransportProfile](#transportprofile)
- [The orthogonal axes](#the-orthogonal-axes)
- [How it is used](#how-it-is-used)

## Capability over identity

The principle: **branch on capabilities, not on compositor names.**

A single "which backend am I" flag is tempting but quickly fuses unrelated
concerns — the protocol version, how focus is tracked, whether a virtual
keyboard exists — into one value, and every new compositor either fits an
existing label or forces a new one. daklak instead records a small set of
**independent capability facts**, fixed once at connect time, and lets each
decision read exactly the fact it needs.

## TransportProfile

A `TransportProfile` is captured **once at `connect()`** and not re-detected
afterwards. It combines protocol identity, focus source, and the capability
facts needed by downstream decisions. Read the `wayland-adapter` crate when
changing the concrete profile shape.

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

What the transport can actually do: whether it has a virtual keyboard, whether
it can commit text through protocol paths, whether it receives surrounding text,
and how frame acknowledgement is handled. Keep this list conceptual in docs; the
source and tests define the exact matrix.

## How it is used

- **Tier selection** consults capabilities before choosing a deletion mechanism.
- **Emit path** chooses a backend based on what the active transport exposes.
- **Focus-driven routing** uses the focus source plus the focused app id.

## Next

- [Key emit & focus](key-emit-and-focus.md) — the emit backends and focus
  backends referenced here.
