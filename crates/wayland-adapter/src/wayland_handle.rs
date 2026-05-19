use std::os::unix::io::{AsRawFd, RawFd};

use anyhow::Context;
use tokio::io::unix::AsyncFd;

use wayland_client::{Connection, EventQueue};

use viet_ime_focus::{FocusBackend, FocusEvent};

use crate::{AdapterHandler, WaylandAdapter};

/// Simple wrapper around a Wayland socket FD for use with `AsyncFd`.
pub struct WlRawFd(pub RawFd);

impl AsRawFd for WlRawFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Thin wrapper around the Wayland protocol I/O layer, owned by the daemon.
pub struct WaylandHandle<H: AdapterHandler> {
    pub conn: Connection,
    pub event_queue: EventQueue<WaylandAdapter<H>>,
    pub app: WaylandAdapter<H>,
    pub wl_fd: AsyncFd<WlRawFd>,
    pub focus_backend: Option<Box<dyn FocusBackend>>,
}

impl<H: AdapterHandler> WaylandHandle<H> {
    pub fn dispatch(&mut self) -> anyhow::Result<bool> {
        self.event_queue.flush().ok();
        self.event_queue
            .dispatch_pending(&mut self.app)
            .context("Wayland dispatch_pending")?;
        Ok(!self.app.state.should_exit)
    }

    pub fn focus_snapshot(&self) -> Option<FocusEvent> {
        self.app
            .state
            .focus_current
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    pub fn focus_backend(&mut self) -> &mut Option<Box<dyn FocusBackend>> {
        &mut self.focus_backend
    }

    pub fn wl_fd(&self) -> &AsyncFd<WlRawFd> {
        &self.wl_fd
    }
}
