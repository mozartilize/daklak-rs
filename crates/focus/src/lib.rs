#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

use async_trait::async_trait;

#[cfg(feature = "x11")]
pub mod x11;

/// A focus-change observation emitted by a backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FocusEvent {
    /// Identifier for the focused toplevel, as the compositor reports it.
    /// `None` when no window with an app id is focused.
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

    /// Human-readable backend identifier for logging.
    fn name(&self) -> &'static str;
}
