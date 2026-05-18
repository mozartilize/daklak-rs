//! Wayland adapter — thin protocol I/O layer for the daklak Vietnamese IME.
//!
//! Owns:
//! - `zwp_input_method_v2` + `zwp_virtual_keyboard_v1` proxies
//! - xkb keymap loading + char translation
//! - Daklak synthetic keymap upload to vk (Path C / Tier 4 enablement)
//! - Tier 3 grab-release/regrab dance around /dev/uinput emissions
//! - Self-emit suppression queue + synthetic-mods echo suppression
//! - Focus tracking via `wlr-foreign-toplevel-management-v1` + X11 bridge
//! - `last_forwarded_key` / `last_forwarded_release` bookkeeping for Path A
//!
//! Does NOT own:
//! - Engine state / composition logic
//! - Strategy / shadow buffer (lives in `edit-strategy`)
//! - Per-window routing decisions / capability probe
//! - Config, IPC, killer-feature seeding
//!
//! Daemon implements `AdapterHandler` and is called as protocol events fire.

pub mod focus;
pub mod frame;
pub mod keymap;
pub mod xkb;

mod dispatch;
mod sink;
mod state;

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use wayland_client::{
    globals::registry_queue_init, protocol::wl_seat::WlSeat, Connection, EventQueue, QueueHandle,
};

use crate::focus::{wlr::WlrForeignToplevelBackend, x11::X11Bridge, FocusBackend};

use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

pub use crate::sink::AdapterSink;
pub use crate::state::AdapterState;
pub use viet_ime_edit_strategy::{BackspaceMethod, KeyState, ModifierState, OutputSink};

// ── Public types ─────────────────────────────────────────────────────────────

/// Frame snapshot delivered to the handler at each Done event.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub activate: bool,
    pub deactivate: bool,
    pub surrounding_text: Option<(String, u32)>,
    pub purpose: u32,
    /// Focused app_id at this Done frame's activate. Read from the cached
    /// `FocusBackend` snapshot (no IPC fork). `Some` only when `activate`
    /// is true and a focused toplevel is known.
    pub app_id: Option<String>,
    /// Whether the focused window is an XWayland-backed surface. Meaningful
    /// only when `app_id` is Some.
    pub is_xwayland: bool,
}

/// Daemon's decision after processing a single key press.
pub enum KeyDecision {
    /// Engine consumed the key; no emit needed.
    Consumed,
    /// Engine did not consume; adapter forwards via `vk.key(press)` and
    /// stamps `last_forwarded_key`.
    ForwardRaw,
    /// Engine consumed and produced an edit. Adapter wraps `apply_pending`
    /// in the Tier-3 grab dance (when method == UInput) and computes
    /// `held_user_kc` (Path A) before passing both to the handler.
    Apply {
        method: BackspaceMethod,
        backspaces: usize,
        commit: String,
    },
}

/// Daemon trait. The adapter calls these methods as protocol events fire.
pub trait AdapterHandler: 'static {
    /// Compositor delivered a Done frame. Examine `frame` to decide activate
    /// / deactivate / re-seed behavior.
    fn on_done_frame(&mut self, ctx: &mut AdapterCtx<'_>, frame: &FrameSnapshot);

    /// User pressed a key. `ch` is the xkb-translated char (None for keys
    /// that don't produce a printable code). Daemon returns a `KeyDecision`
    /// telling the adapter how to dispatch.
    fn on_key_pressed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        evdev: u32,
        ch: Option<char>,
    ) -> KeyDecision;

    /// Called by the adapter inside the Tier-3 grab dance (if applicable),
    /// with `raw_mods` and `held_user_kc` snapshotted just before this call.
    /// Daemon constructs a sink via `ctx.with_sink` and runs `strategy.apply`.
    fn apply_pending(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        method: BackspaceMethod,
        backspaces: usize,
        commit: &str,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
    );

    /// User released a key. Adapter has already forwarded the release and
    /// stamped `last_forwarded_release` before this call — daemon impl is
    /// usually a no-op.
    fn on_key_released(&mut self, _ctx: &mut AdapterCtx<'_>, _time: u32, _evdev: u32) {}

    /// Modifier mask changed (post-suppression of synthetic echoes + xkb-state
    /// already updated adapter-side + vk.modifiers already mirrored).
    fn on_modifiers(&mut self, ctx: &mut AdapterCtx<'_>, mods: ModifierState);

    /// Focused app changed (fired by the active `FocusBackend`).
    fn on_focus_changed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        app_id: Option<String>,
        is_xwayland: bool,
    );
}

/// Context handed to handler callbacks. Borrows adapter state.
pub struct AdapterCtx<'a> {
    pub(crate) state: &'a mut AdapterState,
}

impl<'a> AdapterCtx<'a> {
    pub fn serial(&self) -> u32 {
        self.state.serial
    }

    pub fn raw_mods(&self) -> (u32, u32, u32, u32) {
        self.state.raw_mods
    }

    pub fn modifier_state(&self) -> ModifierState {
        self.state.modifiers
    }

    pub fn last_forwarded_key(&self) -> Option<(u32, Instant)> {
        self.state.last_forwarded_key
    }

    pub fn last_forwarded_release(&self) -> Option<(u32, Instant)> {
        self.state.last_forwarded_release
    }

    /// Forward a raw press through vk and stamp last_forwarded_key.
    /// Used by daemon when a key bypasses composition (no active window,
    /// xkb has no char for it, nav key, etc.).
    pub fn forward_press(&mut self, time: u32, key: u32) {
        if let Some(vk) = &self.state.vk {
            vk.key(time, key, 1);
        }
        self.state.last_forwarded_key = Some((key, Instant::now()));
    }

    /// Forward a raw press WITHOUT stamping last_forwarded_key. Used by the
    /// modifier-shortcut path — those keys don't participate in Path A.
    pub fn vk_key_press_unstamped(&mut self, time: u32, key: u32) {
        if let Some(vk) = &self.state.vk {
            vk.key(time, key, 1);
        }
    }

    /// Construct an AdapterSink bound to live adapter proxies + the supplied
    /// per-emit hints (raw_mods snapshot, held_user_kc for Path A), then run
    /// the closure with `&mut sink`. The closure typically invokes
    /// `strategy.apply` on the resulting sink.
    pub fn with_sink<F>(
        &mut self,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
        f: F,
    ) where
        F: FnOnce(&mut AdapterSink<'_>),
    {
        let im = match &self.state.im {
            Some(x) => x,
            None => return,
        };
        let vk = match &self.state.vk {
            Some(x) => x,
            None => return,
        };
        let serial = self.state.serial;
        let mut sink = AdapterSink {
            im,
            vk,
            uinput: self.state.uinput.as_mut(),
            pending_self_emits: &mut self.state.pending_self_emits,
            synthetic_mods_pending: &mut self.state.synthetic_mods_pending,
            synthetic_mods_emitted_at: &mut self.state.synthetic_mods_emitted_at,
            serial,
            raw_mods,
            held_user_kc,
        };
        f(&mut sink);
    }
}

// ── Wayland adapter struct ───────────────────────────────────────────────────

/// Owns the Wayland proxies, the user-supplied handler, and the QueueHandle
/// used to register new objects (Tier-3 grab re-acquire).
pub struct WaylandAdapter<H: AdapterHandler> {
    pub(crate) handler: H,
    pub(crate) state: AdapterState,
    pub(crate) qh: Option<QueueHandle<WaylandAdapter<H>>>,
}

impl<H: AdapterHandler> WaylandAdapter<H> {
    /// Forward release (adapter-side bookkeeping), then notify daemon.
    pub(crate) fn dispatch_key_release(&mut self, time: u32, key: u32) {
        if self.state.suppress_self_emit(key, 0) {
            tracing::trace!(key, value = 0, "self-emit suppressed");
            return;
        }
        if let Some(vk) = &self.state.vk {
            vk.key(time, key, 0);
        }
        self.state.last_forwarded_release = Some((key, Instant::now()));
        let mut ctx = AdapterCtx { state: &mut self.state };
        self.handler.on_key_released(&mut ctx, time, key);
    }

    /// Top-level dispatch for a grab Key press. Handles self-emit suppression
    /// + xkb translation, then delegates to handler.on_key_pressed and acts
    /// on the returned KeyDecision (including Tier-3 grab dance for UInput).
    pub(crate) fn dispatch_key_press(&mut self, time: u32, key: u32) {
        if self.state.suppress_self_emit(key, 1) {
            tracing::trace!(key, value = 1, "self-emit suppressed");
            return;
        }
        let ch = self.state.xkb.as_ref().and_then(|x| x.key_to_char(key));

        let decision = {
            let mut ctx = AdapterCtx { state: &mut self.state };
            self.handler.on_key_pressed(&mut ctx, time, key, ch)
        };

        match decision {
            KeyDecision::Consumed => {}
            KeyDecision::ForwardRaw => {
                if let Some(vk) = &self.state.vk {
                    vk.key(time, key, 1);
                }
                self.state.last_forwarded_key = Some((key, Instant::now()));
            }
            KeyDecision::Apply {
                method,
                backspaces,
                commit,
            } => {
                let uinput_path = method == BackspaceMethod::UInput;

                // Tier 3 race-free grab dance: release grab + flush + brief
                // sleep so compositor processes the release before kernel BS
                // arrives.
                if uinput_path {
                    if let Some(g) = self.state.grab.take() {
                        g.release();
                    }
                    if let Some(c) = &self.state.conn {
                        let _ = c.flush();
                    }
                    tokio::task::block_in_place(|| {
                        std::thread::sleep(Duration::from_millis(3));
                    });
                }

                let held_user_kc = compute_held_user_kc(&self.state);

                // DUPLICATE-TAIL diagnostic for VkOnly (Path A observability).
                if method == BackspaceMethod::VkOnly {
                    log_duplicate_tail_diagnostic(
                        &self.state,
                        &commit,
                        backspaces,
                        held_user_kc,
                    );
                }

                let raw_mods = self.state.raw_mods;
                {
                    let mut ctx = AdapterCtx { state: &mut self.state };
                    self.handler.apply_pending(
                        &mut ctx,
                        time,
                        method,
                        backspaces,
                        &commit,
                        raw_mods,
                        held_user_kc,
                    );
                }

                if uinput_path {
                    if let Some(c) = &self.state.conn {
                        let _ = c.flush();
                    }
                    tokio::task::block_in_place(|| {
                        std::thread::sleep(Duration::from_millis(3));
                    });
                    if let (Some(im), Some(qh)) = (self.state.im.as_ref(), self.qh.as_ref()) {
                        self.state.grab = Some(im.grab_keyboard(qh, ()));
                        if let Some(c) = &self.state.conn {
                            let _ = c.flush();
                        }
                    }
                }
            }
        }
    }

    /// Apply the accumulated pending frame. Calls handler.on_done_frame
    /// with a fully populated FrameSnapshot, then resets the frame.
    pub(crate) fn apply_done_frame(&mut self) {
        // Daklak-local serial — never imported from compositor.
        self.state.serial = self.state.serial.wrapping_add(1);

        let activate = self.state.pending_frame.pending_activate;
        let deactivate = self.state.pending_frame.pending_deactivate;
        let purpose = self.state.pending_frame.purpose;

        let surrounding_text = self
            .state
            .pending_frame
            .surrounding_text
            .as_ref()
            .map(|st| (st.text.clone(), st.cursor));

        // Activate-time focused-app lookup. Reads the cached snapshot the
        // active `FocusBackend` (wlr dispatch / sway poller) maintains in
        // `state.focus_current`. No I/O on this path.
        let (app_id, is_xwayland) = if activate {
            match self.state.focus_current.lock().ok().and_then(|g| g.clone()) {
                Some(ev) => (ev.app_id, ev.is_xwayland),
                None => (None, false),
            }
        } else {
            (None, false)
        };

        let snapshot = FrameSnapshot {
            activate,
            deactivate,
            surrounding_text,
            purpose,
            app_id,
            is_xwayland,
        };

        tracing::debug!(
            serial = self.state.serial,
            activate,
            deactivate,
            purpose,
            has_surrounding = snapshot.surrounding_text.is_some(),
            "Done frame"
        );

        {
            let mut ctx = AdapterCtx { state: &mut self.state };
            self.handler.on_done_frame(&mut ctx, &snapshot);
        }

        if deactivate {
            // Sticky-purpose clear: next activate must not inherit this app's
            // purpose (e.g. chromium right after foot must not carry purpose=13).
            self.state.pending_frame.end_session();
        }
        self.state.pending_frame.reset();
    }

    /// Modifier event from the grab. Updates xkb + mirrors to vk + applies
    /// synthetic-echo suppression before notifying the handler.
    pub(crate) fn handle_modifiers(
        &mut self,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        // TTL safety net for synthetic_mods_pending counter — see field doc
        // in state.rs. 50ms covers the common compositor-coalescing case.
        const SYNTHETIC_MODS_TTL: Duration = Duration::from_millis(50);
        if self.state.synthetic_mods_pending > 0
            && self
                .state
                .synthetic_mods_emitted_at
                .is_some_and(|t| t.elapsed() > SYNTHETIC_MODS_TTL)
        {
            tracing::trace!(
                pending = self.state.synthetic_mods_pending,
                "on_modifiers: synthetic counter TTL expired, force-reset"
            );
            self.state.synthetic_mods_pending = 0;
            self.state.synthetic_mods_emitted_at = None;
        }
        if self.state.synthetic_mods_pending > 0 {
            self.state.synthetic_mods_pending = self.state.synthetic_mods_pending.saturating_sub(1);
            if self.state.synthetic_mods_pending == 0 {
                self.state.synthetic_mods_emitted_at = None;
            }
            tracing::trace!(
                pending_after = self.state.synthetic_mods_pending,
                "on_modifiers: skipping synthetic echo"
            );
            return;
        }

        // Update xkb state
        if let Some(xkb) = &mut self.state.xkb {
            xkb.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
        }

        // Track modifier bitmask for daemon's shortcut detection
        let mut m = ModifierState::empty();
        if mods_depressed & 0x01 != 0 {
            m |= ModifierState::SHIFT;
        }
        if mods_depressed & 0x04 != 0 {
            m |= ModifierState::CTRL;
        }
        if mods_depressed & 0x08 != 0 {
            m |= ModifierState::ALT;
        }
        if mods_depressed & 0x40 != 0 {
            m |= ModifierState::SUPER;
        }
        self.state.modifiers = m;
        self.state.raw_mods = (mods_depressed, mods_latched, mods_locked, group);

        // Mirror to virtual keyboard
        if let Some(vk) = &self.state.vk {
            vk.modifiers(mods_depressed, mods_latched, mods_locked, group);
        }

        let mut ctx = AdapterCtx { state: &mut self.state };
        self.handler.on_modifiers(&mut ctx, m);
    }
}

/// Path-A `held_user_kc` computation from adapter-side bookkeeping. Held when
/// there's a press without a matching newer release of the same keycode.
fn compute_held_user_kc(state: &AdapterState) -> Option<u32> {
    match (state.last_forwarded_key, state.last_forwarded_release) {
        (Some((kc_p, t_p)), Some((kc_r, t_r))) if kc_p == kc_r && t_r > t_p => None,
        (Some((kc_p, _)), _) => Some(kc_p),
        (None, _) => None,
    }
}

/// VkOnly DUPLICATE-TAIL diagnostic — checks whether the commit's tail char
/// resolves to a keycode the user just pressed/released. The Path A prelude
/// release (in `AdapterSink::vk_commit_char`) will fire when `held` matches.
fn log_duplicate_tail_diagnostic(
    state: &AdapterState,
    commit: &str,
    backspaces: usize,
    held_user_kc: Option<u32>,
) {
    let Some(tail) = commit.chars().last() else {
        return;
    };
    let Some(spec) = crate::keymap::char_to_emit(tail) else {
        return;
    };
    let press_match = state
        .last_forwarded_key
        .filter(|(kc, _)| *kc == spec.keycode);
    let release_match = state
        .last_forwarded_release
        .filter(|(kc, _)| *kc == spec.keycode);
    let press_gap_us = press_match.map(|(_, t)| t.elapsed().as_micros() as u64);
    let release_gap_us = release_match.map(|(_, t)| t.elapsed().as_micros() as u64);
    let held = held_user_kc == Some(spec.keycode);
    if press_match.is_some() || release_match.is_some() {
        tracing::warn!(
            tail = %tail,
            tail_kc = spec.keycode,
            ?press_gap_us,
            ?release_gap_us,
            held,
            backspaces,
            commit,
            "DUPLICATE-TAIL: vk_only commit tail keycode matches user's last forwarded press/release → Path A prelude release will be emitted if held"
        );
    } else {
        tracing::debug!(
            tail = %tail,
            tail_kc = spec.keycode,
            last_press_kc = ?state.last_forwarded_key.map(|(k, _)| k),
            last_release_kc = ?state.last_forwarded_release.map(|(k, _)| k),
            "tail-check: vk_only commit tail keycode differs from last forwarded"
        );
    }
}

// ── Wrapper for fd used with tokio AsyncFd ───────────────────────────────────

struct WlRawFd(RawFd);
impl AsRawFd for WlRawFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Connect to Wayland, bind globals, create input method + virtual keyboard,
/// then drive the event loop until ctrl-c or compositor `Unavailable`.
pub async fn run<H: AdapterHandler>(handler: H) -> Result<()> {
    let (conn, event_queue, app, backend) = connect(handler)?;
    run_event_loop(conn, event_queue, app, backend).await
}

/// Connect to the Wayland compositor, bind globals, create input method + vk.
fn connect<H: AdapterHandler>(
    handler: H,
) -> Result<(
    Connection,
    EventQueue<WaylandAdapter<H>>,
    WaylandAdapter<H>,
    Option<Box<dyn FocusBackend>>,
)> {
    let conn = Connection::connect_to_env().context("connect to Wayland display")?;

    let (globals, mut event_queue) =
        registry_queue_init::<WaylandAdapter<H>>(&conn).context("registry_queue_init")?;

    let qh = event_queue.handle();
    let state = AdapterState::new();
    let mut app = WaylandAdapter {
        handler,
        state,
        qh: Some(qh.clone()),
    };

    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=8, ())
        .context("bind wl_seat")?;
    app.state.seat = Some(seat.clone());

    let im_manager = globals
        .bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
        .context("bind zwp_input_method_manager_v2 — requires wlroots compositor")?;
    app.state.im_manager = Some(im_manager.clone());

    let vk_manager = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .context("bind zwp_virtual_keyboard_manager_v1")?;
    app.state.vk_manager = Some(vk_manager.clone());

    // Focus backend: wlr-foreign-toplevel-management. Bound BEFORE the initial
    // roundtrip so toplevel events emitted at bind time populate state.toplevels
    // before any IM activate frame fires. X11 bridge augments XWayland detection
    // when $DISPLAY is set.
    let backend: Option<Box<dyn FocusBackend>> =
        match globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ()) {
            Ok(m) => {
                app.state.ftl_manager = Some(m);
                app.state.x11_bridge = X11Bridge::spawn();
                let (b, tx) =
                    WlrForeignToplevelBackend::new(app.state.focus_current.clone());
                app.state.focus_tx = Some(tx);
                tracing::info!("focus backend: wlr-foreign-toplevel-management v3");
                Some(Box::new(b))
            }
            Err(e) => {
                tracing::warn!(
                    "wlr-foreign-toplevel unavailable ({e:#}); focus tracking disabled"
                );
                None
            }
        };

    event_queue.roundtrip(&mut app).context("initial roundtrip")?;

    let im = im_manager.get_input_method(&seat, &qh, ());
    let grab = im.grab_keyboard(&qh, ());
    let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());
    tracing::info!("input method and virtual keyboard created");

    app.state.im = Some(im);
    app.state.grab = Some(grab);
    app.state.vk = Some(vk);
    app.state.conn = Some(conn.clone());

    event_queue.roundtrip(&mut app).context("second roundtrip")?;

    Ok((conn, event_queue, app, backend))
}

async fn run_event_loop<H: AdapterHandler>(
    _conn: Connection,
    mut event_queue: EventQueue<WaylandAdapter<H>>,
    mut app: WaylandAdapter<H>,
    mut focus_backend: Option<Box<dyn FocusBackend>>,
) -> Result<()> {
    use tokio::signal;

    let raw = event_queue.as_fd().as_raw_fd();
    let wl_fd = AsyncFd::with_interest(WlRawFd(raw), Interest::READABLE)
        .context("AsyncFd on Wayland socket")?;

    if let Some(b) = focus_backend.as_ref() {
        tracing::debug!(backend = b.name(), "focus backend active");
    }

    loop {
        // Flush queued outgoing requests
        event_queue.flush().ok();

        let read_guard = event_queue.prepare_read();

        tokio::select! {
            biased;

            // Ctrl-C / SIGTERM
            _ = signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                drop(read_guard);
                break;
            }

            // Wayland socket readable
            ready = wl_fd.readable() => {
                let mut guard = ready.context("AsyncFd poll")?;
                guard.clear_ready();
                if let Some(rg) = read_guard {
                    rg.read().ok();
                }
                event_queue.dispatch_pending(&mut app)
                    .context("Wayland dispatch_pending")?;

                if app.state.should_exit {
                    tracing::info!("compositor sent Unavailable — exiting");
                    break;
                }
            }

            // Focus backend event (sway poller today; wlr+x11 later).
            Some(ev) = async {
                match focus_backend.as_mut() {
                    Some(b) => b.next_event().await,
                    None => std::future::pending().await,
                }
            } => {
                drop(read_guard);
                tracing::debug!(?ev, "focus backend: focused app changed");
                let mut ctx = AdapterCtx { state: &mut app.state };
                app.handler.on_focus_changed(&mut ctx, ev.app_id, ev.is_xwayland);
            }
        }
    }

    // Clean up Wayland objects
    if let Some(grab) = app.state.grab.take() {
        grab.release();
    }
    if let Some(im) = app.state.im.take() {
        im.destroy();
    }
    if let Some(vk) = app.state.vk.take() {
        vk.destroy();
    }
    event_queue.flush().ok();

    Ok(())
}
