//! Per-transport wire glue. Each module translates one transport's events
//! into `Daemon` routing calls + `Composer` brain operations. Adding a
//! transport touches one new file here, not the composition core.

mod evdev;
mod wayland;

#[cfg(feature = "ibus")]
mod ibus;
