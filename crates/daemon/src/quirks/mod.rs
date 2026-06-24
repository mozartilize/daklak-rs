//! Daemon-local workaround state for app/protocol quirks.
//!
//! These modules intentionally contain behavior that should be easy to delete
//! when upstream clients or protocol adapters stop needing the workaround.

pub(crate) mod firefox;

#[cfg(feature = "ibus")]
pub(crate) mod ibus;
