//! KDE Plasma window-management focus backend — stub.
//!
//! `org_kde_plasma_window_management` exposes `app_id_changed`,
//! `title_changed`, `state_changed`, plus a `pid` field. XML at
//! invent.kde.org/libraries/plasma-wayland-protocols is wayland-scanner
//! compatible (confirmed). XWayland detection still routed through
//! `focus::x11::X11Bridge` (XWayland sets `$DISPLAY` regardless of
//! compositor).
//!
//! Implementation deferred: vendor XML into `crates/wayland-adapter/protocols/`,
//! add `build.rs` scanner entry, write Dispatch impls mirroring `wlr.rs`.
//! Compose with the same `FocusBackend` trait + `X11Bridge` as wlr.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{FocusBackend, FocusEvent};

pub struct KdePlasmaBackend {
    rx: mpsc::UnboundedReceiver<FocusEvent>,
}

impl KdePlasmaBackend {
    pub fn spawn() -> Option<Self> {
        tracing::warn!("KDE Plasma backend not yet implemented");
        None
    }
}

#[async_trait]
impl FocusBackend for KdePlasmaBackend {
    async fn next_event(&mut self) -> Option<FocusEvent> {
        self.rx.recv().await
    }

    fn current(&self) -> Option<FocusEvent> {
        None
    }

    fn name(&self) -> &'static str {
        "kde-plasma-window-management"
    }
}
