use async_trait::async_trait;

#[cfg(feature = "x11")]
pub mod x11;

/// Which focus-tracking source won at `connect()`. A separate axis from the IM
/// protocol (plan82 #5): probed independently, so v1 and v2 both pick whichever
/// source the compositor actually exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusSource {
    /// `wlr-foreign-toplevel-management-v1` (wlroots and any compositor that
    /// exposes it — not implied by the IM protocol).
    WlrForeignToplevel,
    /// `org_kde_plasma_window_management` (compiled only under the `kde`
    /// cargo feature).
    KdePlasma,
    /// No focus source available — focus tracking disabled.
    None,
}

/// A focus-change observation emitted by a backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FocusEvent {
    /// Identifier for the focused toplevel. `None` when no IME-eligible window
    /// is focused. Canonicalized lowercase by the daemon, but backends are
    /// expected to emit raw values.
    pub app_id: Option<String>,
    /// Whether the focused window is XWayland-backed.
    pub is_xwayland: bool,
}

/// Source of focus-change events.
#[async_trait]
pub trait FocusBackend: Send {
    /// Await the next focus change. Returns `None` when the backend has
    /// shut down — caller should stop polling.
    async fn next_event(&mut self) -> Option<FocusEvent>;

    /// Last known focus state. Used for activate-time synchronous lookup.
    fn current(&self) -> Option<FocusEvent>;

    /// Human-readable backend identifier for logging.
    fn name(&self) -> &'static str;
}
