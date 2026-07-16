#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

//! Evdev-only adapter — daemon-owned EVIOCGRAB of `/dev/input/event*`
//! keyboards, composition through the engine, emit through uinput.
//!
//! Active when the daemon runs with `enable_wayland = false` &&
//! `enable_evdev_grab = true`. Parallel to `viet-ime-wayland-adapter`:
//! both expose an "adapter" + a `Handler` trait the daemon implements.

pub mod adapter;
pub mod output_backend;

pub use adapter::{EvdevAdapter, EvdevHandler};
pub use output_backend::{OutputBackend, UinputBackend};
