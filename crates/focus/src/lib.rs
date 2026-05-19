use async_trait::async_trait;

#[cfg(feature = "x11")]
pub mod x11;

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
