//! `zwlr_foreign_toplevel_manager_v1` (v3) focus backend.
//!
//! Dispatch (in `dispatch.rs`) accumulates per-handle state, on each `done`
//! event recomputes the active toplevel + writes the resulting `FocusEvent`
//! into `AdapterState::focus_current` and pushes it through the channel this
//! backend exposes via `next_event`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{FocusBackend, FocusEvent};

#[derive(Debug, Default, Clone)]
pub(crate) struct ToplevelEntry {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub activated: bool,
    pub pending_app_id: Option<String>,
    pub pending_title: Option<String>,
    pub pending_activated: Option<bool>,
}

impl ToplevelEntry {
    pub(crate) fn commit(&mut self) -> bool {
        if let Some(a) = self.pending_app_id.take() {
            self.app_id = Some(a);
        }
        if let Some(t) = self.pending_title.take() {
            self.title = Some(t);
        }
        match self.pending_activated.take() {
            Some(new) if new != self.activated => {
                self.activated = new;
                true
            }
            _ => false,
        }
    }
}

pub struct WlrForeignToplevelBackend {
    rx: mpsc::UnboundedReceiver<FocusEvent>,
    current: Arc<Mutex<Option<FocusEvent>>>,
}

impl WlrForeignToplevelBackend {
    /// Build the backend + sender. Caller installs `tx` into
    /// `AdapterState::focus_tx` and shares `current` via
    /// `AdapterState::focus_current` so dispatch handlers can update both.
    pub(crate) fn new(
        current: Arc<Mutex<Option<FocusEvent>>>,
    ) -> (Self, mpsc::UnboundedSender<FocusEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx, current }, tx)
    }
}

#[async_trait]
impl FocusBackend for WlrForeignToplevelBackend {
    async fn next_event(&mut self) -> Option<FocusEvent> {
        self.rx.recv().await
    }

    fn current(&self) -> Option<FocusEvent> {
        self.current.lock().ok().and_then(|g| g.clone())
    }

    fn name(&self) -> &'static str {
        "wlr-foreign-toplevel"
    }
}
