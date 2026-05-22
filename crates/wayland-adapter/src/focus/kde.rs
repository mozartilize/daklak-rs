//! KDE Plasma window-management focus backend.
//!
//! Uses `org_kde_plasma_window_management` to track focused window changes.
//! Dispatch (in `dispatch.rs`) accumulates per-window state and on each state
//! change pushes a `FocusEvent` through the channel this backend exposes.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::OrgKdePlasmaWindow;

use super::{FocusBackend, FocusEvent};

/// Per-window state accumulated from plasma window management events.
/// Unlike wlr-foreign-toplevel, there is no batched `Done` event — each
/// field is updated immediately as the corresponding event fires.
#[derive(Debug, Default, Clone)]
pub(crate) struct PlasmaToplevelEntry {
    #[allow(dead_code)]
    pub uuid: String,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub activated: bool,
    pub pid: Option<u32>,
    /// Proxy handle for the window object.
    pub handle: Option<OrgKdePlasmaWindow>,
}

pub struct KdePlasmaBackend {
    rx: mpsc::UnboundedReceiver<FocusEvent>,
    current: Arc<Mutex<Option<FocusEvent>>>,
}

impl KdePlasmaBackend {
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
impl FocusBackend for KdePlasmaBackend {
    async fn next_event(&mut self) -> Option<FocusEvent> {
        self.rx.recv().await
    }

    fn current(&self) -> Option<FocusEvent> {
        self.current.lock().ok().and_then(|g| g.clone())
    }

    fn name(&self) -> &'static str {
        "kde-plasma-window-management"
    }
}
