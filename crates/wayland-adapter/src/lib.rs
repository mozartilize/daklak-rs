//! Wayland adapter — pure Wayland protocol I/O layer for the daklak Vietnamese IME.
//!
//! Owns:
//! - `zwp_input_method_v2` + `zwp_virtual_keyboard_v1` proxies
//! - xkb keymap loading + char translation
//! - Daklak synthetic keymap upload to vk (Tier 4 VkOnly enablement)
//! - Tier 3 grab-release/regrab dance around /dev/uinput emissions
//! - Self-emit suppression queue + synthetic-mods echo suppression
//! - Focus tracking via `wlr-foreign-toplevel-management-v1` + X11 bridge
//! - `last_forwarded_key` / `last_forwarded_release` bookkeeping for the tail-char-drop fix
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
pub mod wayland_handle;
// `XkbState` lives in `viet-ime-keymap::xkb` now.
pub use viet_ime_keymap::xkb;

mod dispatch;
mod dispatch_v1;
mod sink;
mod state;

use std::os::fd::AsFd;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use wayland_client::{
    globals::registry_queue_init, protocol::wl_seat::WlSeat, Connection, QueueHandle,
};

use crate::focus::{x11::X11Bridge, FocusBackend};

#[cfg(feature = "kde")]
use crate::focus::kde::KdePlasmaBackend;
use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_v1::ZwpInputMethodV1;
use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
};
#[cfg(feature = "kde")]
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::OrgKdePlasmaWindowManagement;
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

use crate::focus::wlr::WlrForeignToplevelBackend;

pub use crate::sink::AdapterSink;
pub use crate::state::AdapterState;
pub use viet_ime_edit_strategy::{BackspaceMethod, KeyState, ModifierState, OutputSink};

// ── Public types ─────────────────────────────────────────────────────────────

/// Which input-method protocol the connected compositor speaks.
/// Set during `connect()` and surfaced via `AdapterCtx::protocol()`.
/// The daemon matches on this to adjust tier routing (e.g. VkOnly is
/// unavailable on v1 because there's no separate vk keyboard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImProtocol {
    /// `zwp_input_method_v2` + `zwp_virtual_keyboard_v1` — wlroots path.
    ImV2,
    /// `zwp_input_method_v1` (KWin/Mutter) — no vk, v1 context is the
    /// key-emission path.
    ImV1,
}

/// Everything daklak learns about the transport at `connect()` — fixed for the
/// life of the process. The single source of every process-scoped capability;
/// no use site re-derives a capability by matching on the protocol/backend name.
/// `focus` is added in a later milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportProfile {
    /// IM protocol the compositor speaks. Identity only — capability decisions
    /// read the bools below, never this.
    pub protocol: ImProtocol,
    /// Focus-tracking source, probed independently of `protocol`.
    /// `for_protocol` leaves this `None`; `connect()` overwrites it after the
    /// focus probe.
    pub focus: crate::focus::FocusSource,
    /// `zwp_virtual_keyboard_v1` is available. Required for Tier 2 ForwardKey's
    /// BS emit on v2 and for Tier 4 VkOnly. False on the v1 IM relay.
    pub has_vk_keyboard: bool,
    /// Commit chars can be synthesized via `zwp_input_method_context_v1::keysym`
    /// (the v1 terminal path). True on ImV1, false on ImV2.
    pub has_keysym_commit: bool,
    /// `commit_string` actually reaches the client (false on evdev / vk-only).
    pub delivers_commit_string: bool,
    /// The transport auto-acks the v3 client's `done` (v1 `CommitState`
    /// heartbeat); v2 needs daklak's bare commit to drive the ack.
    pub heartbeats_done: bool,
    /// The protocol can carry surrounding-text at all. This is the PROCESS
    /// capability — distinct from whether a frame has actually been *seen*
    /// (that runtime evidence lives in `CapabilityProbe.surrounding_text_seen`).
    pub can_receive_surrounding: bool,
}

impl TransportProfile {
    /// The capability bundle implied by an IM protocol. `connect()` calls this,
    /// then may override `has_vk_keyboard` from the actual VK-manager bind and
    /// fills `focus` from an independent probe (a per-connection fact, not a
    /// function of the protocol). Pure — unit-testable without a compositor.
    pub fn for_protocol(protocol: ImProtocol) -> Self {
        match protocol {
            // wlroots v2: separate vk keyboard, no keysym path, v2 has no
            // implicit `done` heartbeat.
            ImProtocol::ImV2 => Self {
                protocol,
                focus: crate::focus::FocusSource::None,
                has_vk_keyboard: true,
                has_keysym_commit: false,
                delivers_commit_string: true,
                heartbeats_done: false,
                can_receive_surrounding: true,
            },
            // v1 IM relay: no vk keyboard (BS via the v1 context), keysym commit
            // available, v1 `CommitState` heartbeats the v3 client.
            ImProtocol::ImV1 => Self {
                protocol,
                focus: crate::focus::FocusSource::None,
                has_vk_keyboard: false,
                has_keysym_commit: true,
                delivers_commit_string: true,
                heartbeats_done: true,
                can_receive_surrounding: true,
            },
        }
    }
}

/// Frame snapshot delivered to the handler at each Done event.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub activate: bool,
    pub deactivate: bool,
    pub surrounding_text: Option<(String, u32, u32)>,
    pub purpose: u32,
    /// Focused app_id at this Done frame's activate. Read from the cached
    /// `FocusBackend` snapshot (no IPC fork). `Some` only when `activate`
    /// is true and a focused toplevel is known.
    pub app_id: Option<String>,
    /// Whether the focused window is an XWayland-backed surface. Meaningful
    /// only when `app_id` is Some.
    pub is_xwayland: bool,
}

// `KeyDecision` lives in `viet-ime-edit-strategy` so both adapters
// (wayland + evdev) can import it without a cross-adapter dep.
pub use viet_ime_edit_strategy::KeyDecision;

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
    pub state: &'a mut AdapterState,
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

    pub fn protocol(&self) -> ImProtocol {
        self.state.profile.protocol
    }

    /// The process-scoped transport capability profile, built once at
    /// `connect()`. Read this for capability decisions — never re-match the
    /// protocol/backend name at a use site.
    pub fn profile(&self) -> TransportProfile {
        self.state.profile
    }

    pub fn last_forwarded_key(&self) -> Option<(u32, Instant)> {
        self.state.last_forwarded_key
    }

    pub fn last_forwarded_release(&self) -> Option<(u32, Instant)> {
        self.state.last_forwarded_release
    }

    /// Forward a raw press through vk (or v1 context) and stamp
    /// last_forwarded_key. Used by daemon when a key bypasses composition
    /// (no active window, xkb has no char for it, nav key, etc.).
    pub fn forward_press(&mut self, time: u32, key: u32) {
        let value = self.state.press_value();
        self.state.emit_forward_key(time, key, value);
        // A repeat is not a fresh press — don't refresh the tail-char-drop window.
        if !self.state.forwarding_repeat {
            self.state.last_forwarded_key = Some((key, Instant::now()));
        }
    }

    /// Forward a raw press WITHOUT stamping last_forwarded_key. Used by the
    /// modifier-shortcut path — those keys don't participate in the tail-char-drop fix.
    /// Emits `value=2` while dispatching a repeat (see [`AdapterCtx::is_repeat`]).
    pub fn vk_key_press_unstamped(&mut self, time: u32, key: u32) {
        let value = self.state.press_value();
        self.state.emit_forward_key(time, key, value);
    }

    /// True when the current `on_key_pressed` call is a key-REPEAT (wl_keyboard
    /// state=2), not a fresh press. The daemon reads this to skip engine
    /// mutation / re-composition and to swallow repeats of compose keys.
    pub fn is_repeat(&self) -> bool {
        self.state.forwarding_repeat
    }

    /// True when `ch` (the char decoded at the active modifier level) differs
    /// from what `key` produces at its BASE level — i.e. a level-shifting
    /// modifier (Shift, AltGr/Level3, CapsLock, …) changed the output.
    ///
    /// On the v1/KWin ForwardKey path a raw-forwarded keycode is re-decoded by
    /// the client at base level (KWin doesn't refresh the client's modifier
    /// state for an IM-forwarded key), so a level-shifted char must be
    /// committed as text instead. Returns false when there's no keymap to
    /// compare against — then the raw-forward default stands.
    pub fn is_level_shifted(&self, key: u32, ch: char) -> bool {
        match self.state.xkb.as_ref() {
            Some(xkb) => xkb.base_char(key) != Some(ch),
            None => false,
        }
    }

    /// Construct an AdapterSink bound to live adapter proxies + the supplied
    /// per-emit hints (raw_mods snapshot, held_user_kc for the tail-char-drop fix), then run
    /// the closure with `&mut sink`. The closure typically invokes
    /// `strategy.apply` on the resulting sink.
    pub fn with_sink<F>(
        &mut self,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
        commit_string_functional: bool,
        f: F,
    ) where
        F: FnOnce(&mut AdapterSink<'_>),
    {
        let serial = self.state.serial;
        let conn = self.state.conn.as_ref();
        match self.state.profile.protocol {
            ImProtocol::ImV2 => {
                let im = match &self.state.im {
                    Some(x) => x,
                    None => return,
                };
                let vk = match &self.state.vk {
                    Some(x) => x,
                    None => return,
                };
                // synth_keymap_emitter always points at vk_v2 — only it can
                // drive daklak's uploaded keymap. forward_emitter is vk_v2.
                let mut vk_forward = viet_ime_key_emitter::VkV2Emitter::new(vk);
                let mut vk_synth = viet_ime_key_emitter::VkV2Emitter::new(vk);
                let synth_dyn: &mut dyn viet_ime_key_emitter::KeyEmitter = &mut vk_synth;

                let mut sink = AdapterSink {
                    text_ops: crate::sink::TextOpsTarget::V2 { im },
                    forward_emitter: &mut vk_forward,
                    synth_keymap_emitter: Some(synth_dyn),
                    uinput: self.state.uinput.as_mut(),
                    pending_self_emits: &mut self.state.pending_self_emits,
                    synthetic_mods_pending: &mut self.state.synthetic_mods_pending,
                    synthetic_mods_emitted_at: &mut self.state.synthetic_mods_emitted_at,
                    raw_mods,
                    held_user_kc,
                    commit_string_functional,
                    conn,
                    xkb: self.state.xkb.as_ref(),
                    pending_im_commit_ack: &mut self.state.pending_im_commit_ack,
                };
                f(&mut sink);
            }
            ImProtocol::ImV1 => {
                let ctx = match &self.state.im_ctx_v1 {
                    Some(x) => x,
                    None => return,
                };
                // ImV1: forward_emitter is VkV1Emitter (ctx.key).
                let mut vk_forward = viet_ime_key_emitter::VkV1Emitter::new(ctx, serial);
                let mut sink = AdapterSink {
                    text_ops: crate::sink::TextOpsTarget::V1 { ctx, serial },
                    forward_emitter: &mut vk_forward,
                    synth_keymap_emitter: None,
                    uinput: self.state.uinput.as_mut(),
                    pending_self_emits: &mut self.state.pending_self_emits,
                    synthetic_mods_pending: &mut self.state.synthetic_mods_pending,
                    synthetic_mods_emitted_at: &mut self.state.synthetic_mods_emitted_at,
                    raw_mods,
                    held_user_kc,
                    commit_string_functional,
                    conn,
                    xkb: self.state.xkb.as_ref(),
                    pending_im_commit_ack: &mut self.state.pending_im_commit_ack,
                };
                f(&mut sink);
            }
        }
    }
}

// ── Wayland adapter struct ───────────────────────────────────────────────────

pub struct WaylandAdapter<H: AdapterHandler> {
    pub handler: H,
    pub state: AdapterState,
    pub qh: Option<QueueHandle<WaylandAdapter<H>>>,
}

impl<H: AdapterHandler> WaylandAdapter<H> {
    pub(crate) fn dispatch_key_release(&mut self, time: u32, key: u32) {
        tracing::info!(key, "dispatch_key_release: IM grab delivered release");
        if self.state.suppress_self_emit(key, 0) {
            tracing::trace!(key, value = 0, "self-emit suppressed (IM grab roundtrip)");
            return;
        }
        // Releases route through the same forward path as presses so the
        // focused app sees a balanced press/release pair.
        self.state.emit_forward_key(time, key, 0);
        self.state.last_forwarded_release = Some((key, Instant::now()));
        let mut ctx = AdapterCtx {
            state: &mut self.state,
        };
        self.handler.on_key_released(&mut ctx, time, key);
    }

    pub(crate) fn dispatch_key_press(&mut self, time: u32, key: u32) {
        tracing::info!(key, "dispatch_key_press: IM grab delivered press");
        if self.state.suppress_self_emit(key, 1) {
            tracing::trace!(key, value = 1, "self-emit suppressed");
            return;
        }

        let ch = self.state.xkb.as_ref().and_then(|x| x.key_to_char(key));

        let decision = {
            let mut ctx = AdapterCtx {
                state: &mut self.state,
            };
            self.handler.on_key_pressed(&mut ctx, time, key, ch)
        };

        match decision {
            KeyDecision::Consumed => {}
            KeyDecision::ForwardRaw => {
                self.state.emit_forward_key(time, key, 1);
                self.state.last_forwarded_key = Some((key, Instant::now()));
            }
            KeyDecision::Apply {
                method,
                backspaces,
                commit,
            } => {
                let uinput_path = method == BackspaceMethod::UInput;

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

                if method == BackspaceMethod::VkOnly {
                    log_duplicate_tail_diagnostic(&self.state, &commit, backspaces, held_user_kc);
                }

                let raw_mods = self.state.raw_mods;
                {
                    let mut ctx = AdapterCtx {
                        state: &mut self.state,
                    };
                    self.handler.apply_pending(
                        &mut ctx,
                        time,
                        backspaces,
                        &commit,
                        raw_mods,
                        held_user_kc,
                    );
                }

                // (Tail-char drop on space-after-tone on Tier 2
                // ForwardKey + foot/KWin is handled per-keysym inside
                // `AdapterSink::commit_via_keysym` — see that fn for
                // the kc 247 forwardKeySym race explanation. No
                // additional post-apply sleep needed at this boundary.)

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

    /// IM grab delivered a key REPEAT (wl_keyboard state=2). KWin generates
    /// server-side repeat as state=2 key events; rate-0 clients (Chromium /
    /// Electron on KWin) rely on those instead of self-repeating. The press
    /// path collapses these into `value=1`, so the client never sees a repeat
    /// and continuous-key actions (Ctrl+Arrow, hold-arrow) die. Here we set
    /// `forwarding_repeat` so the forward helpers emit `value=2`, re-run the
    /// daemon's forward decision WITHOUT mutating the compose engine (it reads
    /// `ctx.is_repeat()` to skip re-composition), and never re-Apply a commit.
    pub(crate) fn dispatch_key_repeat(&mut self, time: u32, key: u32) {
        tracing::trace!(key, "dispatch_key_repeat: IM grab delivered repeat");
        // A repeat of our own synthetic emit shouldn't loop back, but guard
        // identically to a press to stay safe against grab roundtrips.
        if self.state.suppress_self_emit(key, 1) {
            return;
        }

        let ch = self.state.xkb.as_ref().and_then(|x| x.key_to_char(key));

        self.state.forwarding_repeat = true;
        let decision = {
            let mut ctx = AdapterCtx {
                state: &mut self.state,
            };
            self.handler.on_key_pressed(&mut ctx, time, key, ch)
        };

        match decision {
            // Consumed: nav / modifier-shortcut already forwarded with value=2
            // via the ctx helpers. Compose-key repeats are swallowed by the
            // daemon (returns Consumed) so we never re-type a held letter.
            KeyDecision::Consumed => {}
            // Forward-only keys (no active window / no xkb char): emit the
            // repeat directly. Don't stamp last_forwarded_key — a repeat is
            // not a fresh press for the tail-char-drop window.
            KeyDecision::ForwardRaw => {
                self.state.emit_forward_key(time, key, 2);
            }
            // A repeat must never drive a fresh delete+commit. The daemon
            // shouldn't return Apply on repeat, but guard regardless.
            KeyDecision::Apply { .. } => {
                tracing::debug!(key, "repeat resolved to Apply — ignored");
            }
        }
        self.state.forwarding_repeat = false;
    }

    pub(crate) fn apply_done_frame(&mut self) {
        self.state.serial = self.state.serial.wrapping_add(1);
        self.state.pending_im_commit_ack = false;

        let activate = self.state.pending_frame.pending_activate;
        let deactivate = self.state.pending_frame.pending_deactivate;
        let purpose = self.state.pending_frame.purpose;

        let surrounding_text = self
            .state
            .pending_frame
            .surrounding_text
            .as_ref()
            .map(|st| (st.text.clone(), st.cursor, st.anchor));

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
            let mut ctx = AdapterCtx {
                state: &mut self.state,
            };
            self.handler.on_done_frame(&mut ctx, &snapshot);
        }

        // Heartbeat ack on V2/wlroots: sway only emits `text_input_v3.done`
        // when daklak commits on its `zwp_input_method_v2`. If the handler
        // didn't produce any IM output this frame, the v3 client (e.g.
        // chromium) never sees its commit ack'd — its state machine stalls
        // and it stops sending `set_surrounding_text` updates. Emit a bare
        // commit so sway's `handle_im_commit` fires and acks the v3 client.
        if matches!(self.state.profile.protocol, ImProtocol::ImV2)
            && !self.state.pending_im_commit_ack
        {
            if let Some(im) = &self.state.im {
                im.commit(self.state.serial);
            }
        }

        if deactivate {
            self.state.pending_frame.end_session();
        }
        self.state.pending_frame.reset();
    }

    pub(crate) fn handle_modifiers(
        &mut self,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
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

        if let Some(xkb) = &mut self.state.xkb {
            xkb.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
        }

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

        let serial = self.state.serial;
        match self.state.profile.protocol {
            ImProtocol::ImV2 => {
                if let Some(vk) = &self.state.vk {
                    vk.modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            ImProtocol::ImV1 => {
                if let Some(ctx) = &self.state.im_ctx_v1 {
                    ctx.modifiers(serial, mods_depressed, mods_latched, mods_locked, group);
                }
            }
        }

        let mut ctx = AdapterCtx {
            state: &mut self.state,
        };
        self.handler.on_modifiers(&mut ctx, m);
    }
}

fn compute_held_user_kc(state: &AdapterState) -> Option<u32> {
    match (state.last_forwarded_key, state.last_forwarded_release) {
        (Some((kc_p, t_p)), Some((kc_r, t_r))) if kc_p == kc_r && t_r > t_p => None,
        (Some((kc_p, _)), _) => Some(kc_p),
        (None, _) => None,
    }
}

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
            "DUPLICATE-TAIL: vk_only commit tail keycode matches user's last forwarded press/release → tail-drop prelude release will be emitted if held"
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

/// Probe the focus-tracking source, independent of the IM protocol.
/// Order: `wlr-foreign-toplevel-management` (wlroots and anything exposing it),
/// then KDE Plasma (only under the `kde` feature), else none. Runs for both v1
/// and v2 — a non-KDE v1 compositor that exposes wlr gets focus tracking, and a
/// v2 compositor on KDE could get Plasma. Installs the winner's proxies + the
/// X11 bridge into `app.state`.
fn probe_focus<H: AdapterHandler>(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<WaylandAdapter<H>>,
    app: &mut WaylandAdapter<H>,
) -> (crate::focus::FocusSource, Option<Box<dyn FocusBackend>>) {
    use crate::focus::FocusSource;

    match globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(qh, 1..=3, ()) {
        Ok(m) => {
            app.state.ftl_manager = Some(m);
            app.state.x11_bridge = X11Bridge::spawn();
            let (b, tx) = WlrForeignToplevelBackend::new(app.state.focus_current.clone());
            app.state.focus_tx = Some(tx);
            tracing::info!("focus backend: wlr-foreign-toplevel-management v3");
            return (FocusSource::WlrForeignToplevel, Some(Box::new(b)));
        }
        Err(e) => tracing::debug!("wlr-foreign-toplevel unavailable ({e:#})"),
    }

    #[cfg(feature = "kde")]
    match globals.bind::<OrgKdePlasmaWindowManagement, _, _>(qh, 1..=18, ()) {
        Ok(m) => {
            app.state.plasma_manager = Some(m.clone());
            app.state.x11_bridge = X11Bridge::spawn();
            let (b, tx) = KdePlasmaBackend::new(app.state.focus_current.clone());
            app.state.focus_tx = Some(tx);
            tracing::info!("focus backend: org_kde_plasma_window_management v20");
            return (FocusSource::KdePlasma, Some(Box::new(b)));
        }
        Err(e) => tracing::debug!("org_kde_plasma_window_management unavailable ({e:#})"),
    }

    tracing::warn!("no focus backend available; focus tracking disabled");
    (FocusSource::None, None)
}

pub fn connect<H: AdapterHandler>(handler: H) -> Result<crate::wayland_handle::WaylandHandle<H>> {
    let conn = Connection::connect_to_env().context("connect to Wayland display")?;

    if std::env::var("WAYLAND_SOCKET").is_ok() {
        tracing::info!("connected via WAYLAND_SOCKET (likely launched by KWin)");
    }

    let (globals, mut event_queue) =
        registry_queue_init::<WaylandAdapter<H>>(&conn).context("registry_queue_init")?;

    // Diagnostic: dump every advertised global so we can see what the
    // compositor exposes on this connection. Critical for kwin probing
    // since kwin restricts IM globals to the privileged --inputmethod fd.
    {
        let list = globals.contents();
        let snapshot = list.with_list(|gs| {
            gs.iter()
                .map(|g| format!("  {} v{} (name={})", g.interface, g.version, g.name))
                .collect::<Vec<_>>()
                .join("\n")
        });
        tracing::info!("compositor registry globals:\n{snapshot}");
    }

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

    // ── Compositor backend detection: v2 (wlroots) or v1 (KWin/Mutter) ─────
    let mut profile = if let Ok(v2) = globals.bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
    {
        // ── v2 (wlroots) path ────────────────────────────────────────────────
        app.state.im_manager = Some(v2.clone());

        let vk_manager = globals
            .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
            .context("bind zwp_virtual_keyboard_manager_v1")?;
        app.state.vk_manager = Some(vk_manager.clone());

        // Focus source is probed once, after protocol detection, in
        // `probe_focus` — independent of v1/v2.

        event_queue
            .roundtrip(&mut app)
            .context("initial roundtrip")?;

        let im = v2.get_input_method(&seat, &qh, ());
        let grab = im.grab_keyboard(&qh, ());
        let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());
        tracing::info!("input method and virtual keyboard created (v2/wlroots)");

        // Pre-upload daklak synthetic keymap to vk
        if let Some(km) = &app.state.daklak_keymap {
            vk.keymap(1, km.fd.as_fd(), km.size);
            app.state.keymap_init = true;
            tracing::info!(
                size = km.size,
                "vk.keymap → daklak synthetic keymap (pre-uploaded at connect)"
            );
        } else {
            tracing::warn!(
                    "daklak keymap unavailable at connect; vk awaits compositor keymap passthrough via IM grab"
                );
        }

        app.state.im = Some(im);
        app.state.grab = Some(grab);
        app.state.vk = Some(vk);
        app.state.conn = Some(conn.clone());

        // v2/wlroots: vk_manager is bound above (required, errors otherwise)
        // so the vk keyboard is always present.
        TransportProfile::for_protocol(ImProtocol::ImV2)
    } else if let Ok(v1) = globals.bind::<ZwpInputMethodV1, _, _>(&qh, 1..=1, ()) {
        // ── v1 (KWin/Mutter) path ────────────────────────────────────────────
        app.state.im_v1 = Some(v1.clone());
        app.state.conn = Some(conn.clone());

        // Focus source is probed once, after protocol detection, in
        // `probe_focus` — independent of v1/v2.

        event_queue
            .roundtrip(&mut app)
            .context("initial roundtrip")?;

        tracing::info!("input method v1 bound (KWin/Mutter)");
        TransportProfile::for_protocol(ImProtocol::ImV1)
    } else {
        anyhow::bail!("neither zwp_input_method_v2 nor zwp_input_method_v1 exposed by compositor");
    };

    // Focus source — probed once, independent of the IM protocol.
    let (focus_source, backend) = probe_focus(&globals, &qh, &mut app);
    profile.focus = focus_source;
    app.state.profile = profile;

    event_queue
        .roundtrip(&mut app)
        .context("second roundtrip")?;

    let raw = event_queue.as_fd().as_raw_fd();
    let wl_fd = AsyncFd::with_interest(crate::wayland_handle::WlRawFd(raw), Interest::READABLE)
        .context("AsyncFd on Wayland socket")?;

    Ok(crate::wayland_handle::WaylandHandle {
        conn,
        event_queue,
        app,
        wl_fd,
        focus_backend: backend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use viet_ime_edit_strategy::ModifierState;

    #[test]
    fn profile_for_imv2_has_vk_no_keysym() {
        let p = TransportProfile::for_protocol(ImProtocol::ImV2);
        assert_eq!(p.protocol, ImProtocol::ImV2);
        assert!(p.has_vk_keyboard, "v2 has a virtual keyboard");
        assert!(!p.has_keysym_commit, "v2 has no keysym commit path");
        assert!(!p.heartbeats_done, "v2 needs daklak's bare commit to ack");
        assert!(p.delivers_commit_string && p.can_receive_surrounding);
    }

    #[test]
    fn profile_for_imv1_no_vk_has_keysym() {
        let p = TransportProfile::for_protocol(ImProtocol::ImV1);
        assert_eq!(p.protocol, ImProtocol::ImV1);
        assert!(!p.has_vk_keyboard, "v1 IM relay exposes no vk keyboard");
        assert!(p.has_keysym_commit, "v1 commits chars via context keysym");
        assert!(p.heartbeats_done, "v1 CommitState heartbeats the v3 client");
        assert!(p.delivers_commit_string && p.can_receive_surrounding);
    }

    #[test]
    fn vk_keyboard_capability_follows_protocol_not_name() {
        // The whole point of #3/#5: feasibility reads the bool, and the bool
        // differs by protocol — v1 cannot run VkOnly, v2 can.
        assert!(!TransportProfile::for_protocol(ImProtocol::ImV1).has_vk_keyboard);
        assert!(TransportProfile::for_protocol(ImProtocol::ImV2).has_vk_keyboard);
    }

    #[test]
    fn focus_is_not_implied_by_protocol() {
        // `for_protocol` leaves focus unset — connect()'s independent probe owns
        // it. Protocol must NOT carry a focus assumption.
        use crate::focus::FocusSource;
        assert_eq!(
            TransportProfile::for_protocol(ImProtocol::ImV1).focus,
            FocusSource::None
        );
        assert_eq!(
            TransportProfile::for_protocol(ImProtocol::ImV2).focus,
            FocusSource::None
        );
    }

    #[test]
    fn profile_can_pair_any_protocol_with_any_focus() {
        // The combination the old V1Kde/V2Wlroots enum could not represent:
        // a v1 compositor tracking focus via wlr-foreign-toplevel, and a v2
        // compositor tracking via KDE Plasma. Both are now expressible.
        use crate::focus::FocusSource;
        let mut v1_wlr = TransportProfile::for_protocol(ImProtocol::ImV1);
        v1_wlr.focus = FocusSource::WlrForeignToplevel;
        assert_eq!(v1_wlr.protocol, ImProtocol::ImV1);
        assert_eq!(v1_wlr.focus, FocusSource::WlrForeignToplevel);
        assert!(!v1_wlr.has_vk_keyboard, "still v1: no vk keyboard");

        let mut v2_plasma = TransportProfile::for_protocol(ImProtocol::ImV2);
        v2_plasma.focus = FocusSource::KdePlasma;
        assert_eq!(v2_plasma.protocol, ImProtocol::ImV2);
        assert_eq!(v2_plasma.focus, FocusSource::KdePlasma);
    }

    struct NoopHandler;

    impl AdapterHandler for NoopHandler {
        fn on_done_frame(&mut self, _ctx: &mut AdapterCtx<'_>, _frame: &FrameSnapshot) {}

        fn on_key_pressed(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _time: u32,
            _key: u32,
            _ch: Option<char>,
        ) -> KeyDecision {
            KeyDecision::Consumed
        }

        fn apply_pending(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _time: u32,
            _backspaces: usize,
            _commit: &str,
            _raw_mods: (u32, u32, u32, u32),
            _held_user_kc: Option<u32>,
        ) {
        }

        fn on_modifiers(&mut self, _ctx: &mut AdapterCtx<'_>, _mods: ModifierState) {}

        fn on_focus_changed(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _app_id: Option<String>,
            _is_xwayland: bool,
        ) {
        }
    }

    #[test]
    fn press_value_is_2_only_while_forwarding_repeat() {
        // The whole repeat fix hinges on this: a fresh press forwards as
        // wl_keyboard state=1, a key-repeat (state=2) as state=2 so rate-0
        // clients (Chromium on KWin) see continuous-key.
        let mut s = AdapterState::new();
        assert_eq!(s.press_value(), 1, "default press = state 1");
        s.forwarding_repeat = true;
        assert_eq!(s.press_value(), 2, "repeat press = state 2");
        s.forwarding_repeat = false;
        assert_eq!(s.press_value(), 1, "back to state 1 once repeat clears");
    }

    #[test]
    fn is_level_shifted_compares_decoded_char_to_base_level() {
        let mut s = AdapterState::new();
        // No keymap → can't compare → never diverts (raw-forward default).
        {
            let ctx = AdapterCtx { state: &mut s };
            assert!(!ctx.is_repeat());
            assert!(
                !ctx.is_level_shifted(38, 'L'),
                "no keymap → not level-shifted"
            );
        }
        // With a real us keymap, key 38 base = 'l'.
        s.xkb = Some(viet_ime_keymap::xkb::XkbState::us_for_test());
        let ctx = AdapterCtx { state: &mut s };
        assert!(ctx.is_level_shifted(38, 'L'), "'L' differs from base 'l'");
        assert!(!ctx.is_level_shifted(38, 'l'), "'l' IS the base level");
    }

    #[test]
    fn ctx_is_repeat_mirrors_state_flag() {
        // The daemon reads ctx.is_repeat() to skip engine mutation; it must
        // track forwarding_repeat exactly.
        let mut s = AdapterState::new();
        {
            let ctx = AdapterCtx { state: &mut s };
            assert!(!ctx.is_repeat());
        }
        s.forwarding_repeat = true;
        let ctx = AdapterCtx { state: &mut s };
        assert!(ctx.is_repeat());
    }
}
