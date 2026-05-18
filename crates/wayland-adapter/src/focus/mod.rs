//! Compositor-agnostic focused-app tracking.
//!
//! `FocusBackend` abstracts the source of focus events. Implementations:
//! - `wlr::WlrForeignToplevelBackend` — `zwlr_foreign_toplevel_manager_v1` v3.
//! - `x11::X11Bridge` — augmenter setting `is_xwayland` via the X server.
//! - `kde::KdePlasmaBackend` — `org_kde_plasma_window_management` (future).

use async_trait::async_trait;

#[cfg(feature = "kde")]
pub mod kde;
pub mod wlr;
pub mod x11;

/// A focus-change observation emitted by a backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FocusEvent {
    /// Identifier for the focused toplevel. `None` when no IME-eligible window
    /// is focused. Canonicalized lowercase by the daemon, but backends are
    /// expected to emit raw values.
    pub app_id: Option<String>,
    /// Whether the focused window is XWayland-backed. May be set by the
    /// primary backend itself (sway IPC) or by a separate augmenter
    /// (X11 bridge) composed via `CompositeBackend` in a later commit.
    pub is_xwayland: bool,
}

/// Source of focus-change events.
#[async_trait]
pub trait FocusBackend: Send {
    /// Await the next focus change. Returns `None` when the backend has
    /// shut down (compositor disconnect, child task dead, etc.) — caller
    /// should stop polling.
    async fn next_event(&mut self) -> Option<FocusEvent>;

    /// Last known focus state. Used for activate-time synchronous lookup
    /// inside `apply_done_frame`. Implementations cache the most recent
    /// event emitted by `next_event` and return it here without I/O.
    fn current(&self) -> Option<FocusEvent>;

    /// Human-readable backend identifier for logging.
    fn name(&self) -> &'static str;
}
