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
        // v1 batch flush: KWin's IM bridge for some Qt/KDE clients (kate,
        // kwrite) sends `SurroundingText` updates **without** a trailing
        // `CommitState`. Daklak's dispatch_v1 handler only triggers
        // `apply_done_frame` on CommitState, so those updates would sit
        // unprocessed forever. After draining the queue, if v1
        // `pending_commit` is still set, force the frame apply here so
        // the daemon sees the text change.
        if self.app.state.pending_commit
            && matches!(self.app.state.im_backend, crate::ImBackend::V1Kde)
        {
            tracing::debug!(
                "v1: dispatch end — pending_commit still set, forcing apply_done_frame"
            );
            self.app.state.pending_commit = false;
            self.app.apply_done_frame();
        }
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
