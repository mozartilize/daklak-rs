//! Compositor-agnostic focused-app tracking.
//!
//! Re-exports `FocusBackend` + `FocusEvent` from `viet-ime-focus`.
//! The WLR backend stays here because it depends on `wayland-protocols-wlr`.

pub use viet_ime_focus::{FocusBackend, FocusEvent};
pub use viet_ime_focus::x11;

#[cfg(feature = "kde")]
pub mod kde;
pub mod wlr;
