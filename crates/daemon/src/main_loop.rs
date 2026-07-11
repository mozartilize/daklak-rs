use anyhow::Result;
use tokio::signal;

use viet_ime_wayland_adapter::wayland_handle::WaylandHandle;
use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler};

use crate::handler::Daemon;

pub async fn core_loop_with_wayland_shutdown(
    wayland: &mut WaylandHandle<Daemon>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut fb = wayland.focus_backend.take();

    loop {
        wayland.event_queue.flush().ok();
        let read_guard = wayland.event_queue.prepare_read();
        let repeat_deadline = wayland.next_client_repeat_deadline();

        tokio::select! {
            biased;

            changed = shutdown_rx.changed() => {
                drop(read_guard);
                if changed.is_ok() && *shutdown_rx.borrow() {
                    tracing::info!("wayland: supervisor shutdown requested");
                    break;
                }
            }

            _ = signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                drop(read_guard);
                break;
            }

            ready = wayland.wl_fd().readable() => {
                // AsyncFd readiness must be explicitly cleared, otherwise
                // the next `readable()` returns immediately and we spin.
                let mut guard = ready?;
                guard.clear_ready();
                // ReadEventsGuard must have `.read()` called on it to
                // actually pull bytes from the wayland socket into the
                // local queue — dropping it without `read()` just
                // releases the read intent. Skipping this caused the
                // compositor's outbound buffer to fill, blocking
                // scroll/sway's input delivery to every client.
                if let Some(rg) = read_guard {
                    rg.read().ok();
                }
                wayland.dispatch()?;

                if wayland.app.state.should_exit {
                    tracing::info!("compositor sent Unavailable — exiting");
                    break;
                }
            }

            // Biased select handles queued key releases above before a timer
            // expiring at the same instant, avoiding one trailing repeat.
            _ = async {
                match repeat_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                drop(read_guard);
                wayland.dispatch_client_repeat();
            }

            Some(ev) = async {
                match fb.as_mut() {
                    Some(b) => b.next_event().await,
                    None => std::future::pending().await,
                }
            } => {
                drop(read_guard);
                tracing::debug!(?ev, "focus backend: focused app changed");
                wayland.cancel_client_repeat();
                let mut ctx = AdapterCtx { state: &mut wayland.app.state };
                wayland.app.handler.on_focus_changed(
                    &mut ctx,
                    ev.app_id,
                    ev.is_xwayland,
                );
            }
        }
    }

    wayland.focus_backend = fb;

    if let Some(grab) = wayland.app.state.grab.take() {
        grab.release();
    }
    if let Some(im) = wayland.app.state.im.take() {
        im.destroy();
    }
    if let Some(vk) = wayland.app.state.vk.take() {
        vk.destroy();
    }
    wayland.event_queue.flush().ok();

    Ok(())
}
