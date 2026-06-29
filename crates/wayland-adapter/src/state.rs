use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use viet_ime_edit_strategy::ModifierState;
use wayland_client::backend::ObjectId;
use wayland_client::Connection;

use crate::focus::wlr::ToplevelEntry;
use crate::focus::x11::X11Bridge;
use crate::focus::FocusEvent;
use crate::frame::DoneFrame;
use crate::keymap::{self, DaklakKeymap};
use viet_ime_keymap::xkb::XkbState;
use wayland_protocols_misc::{
    zwp_input_method_v2::client::{
        zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::ZwpInputMethodV2,
    },
    zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
};
use wayland_protocols::wp::input_method::zv1::client::{
    zwp_input_method_v1::ZwpInputMethodV1,
    zwp_input_method_context_v1::ZwpInputMethodContextV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

#[cfg(feature = "kde")]
use crate::focus::kde::PlasmaToplevelEntry;
#[cfg(feature = "kde")]
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::OrgKdePlasmaWindowManagement;

/// Adapter-side state. Owns Wayland proxies + emit-history. The handler
/// (daemon) never sees these directly — it interacts via `AdapterCtx`.
pub struct AdapterState {
    // Wayland proxies (set after globals binding + setup)
    pub seat: Option<wayland_client::protocol::wl_seat::WlSeat>,
    pub im_manager: Option<ZwpInputMethodManagerV2>,
    pub vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    pub im: Option<ZwpInputMethodV2>,
    pub grab: Option<ZwpInputMethodKeyboardGrabV2>,
    pub vk: Option<ZwpVirtualKeyboardV1>,

    /// Connection clone used for explicit flushes after batched key emits.
    pub conn: Option<Connection>,

    // xkb state (set on first Keymap event from the grab)
    pub xkb: Option<XkbState>,
    pub keymap_init: bool,

    /// Synthetic xkb keymap with Vietnamese precomposed chars at evdev
    /// 200+. Handed to `vk.keymap()` on the first Keymap event so vk_key
    /// events can deliver Vietnamese chars without `commit_string` / a
    /// `zwp_text_input_v3` activate (Qt5/XWayland path). `None` if
    /// xkbcommon rejects the synthesized keymap — daklak then falls back
    /// to forwarding the compositor's keymap to `vk.keymap()`.
    pub daklak_keymap: Option<DaklakKeymap>,

    // Protocol state
    pub serial: u32,
    pub modifiers: ModifierState,
    pub raw_mods: (u32, u32, u32, u32), // depressed, latched, locked, group

    // Pending double-buffered frame (applied at Done)
    pub pending_frame: DoneFrame,

    // v1-only: set on SurroundingText/ContentType, consumed on CommitState.
    // Prevents apply_done_frame spam when KWin sends events in batches.
    pub pending_commit: bool,


    /// Counter of outgoing `vk.modifiers` calls daklak has made but not yet
    /// seen mirrored back through the IM grab's `Modifiers` event. Used by
    /// `vk_commit_char`'s level-selecting dance (Tier 4).
    pub synthetic_mods_pending: u32,

    /// Timestamp of the last `vk.modifiers` emit that bumped
    /// `synthetic_mods_pending`. TTL safety net (50ms).
    pub synthetic_mods_emitted_at: Option<Instant>,

    /// Last keycode + timestamp daklak forwarded as a vk.key press.
    pub last_forwarded_key: Option<(u32, Instant)>,
    /// Last keycode + timestamp daklak forwarded as a vk.key release.
    pub last_forwarded_release: Option<(u32, Instant)>,
    /// True while dispatching a key-REPEAT event (wl_keyboard state=2) rather
    /// than a fresh press. Forward helpers read this to emit `value=2` so the
    /// focused client sees a proper repeat. Without it, rate-0 clients that
    /// rely on the compositor's server-side repeat (Chromium/Electron on KWin)
    /// get no continuous-key behavior through the IM grab. See dispatch_key_repeat.
    pub forwarding_repeat: bool,

    /// Set true by `AdapterSink::commit()` (V2 path) whenever daklak emits
    /// `zwp_input_method_v2.commit`. Reset at the start of every
    /// `apply_done_frame`. If still false after the handler runs, daklak
    /// emits a bare ack-commit so wlroots' `handle_im_commit` fires and
    /// sends `text_input_v3.done` back to the v3 client — unblocking
    /// chromium's state machine which otherwise stops sending
    /// `set_surrounding_text` after the initial enable.
    pub pending_im_commit_ack: bool,

    pub should_exit: bool,

    // ── Compositor backend selection ─────────────────────────────────────────
    /// Process-scoped transport capability profile. Set during `connect()`.
    pub profile: crate::TransportProfile,
    /// v1 IM global (only on KWin/Mutter). `None` on wlroots.
    pub im_v1: Option<ZwpInputMethodV1>,
    /// v1 context proxy — short-lived, one per text-input session.
    /// Replaced on each `activate` event and dropped on `deactivate`.
    pub im_ctx_v1: Option<ZwpInputMethodContextV1>,
    /// wl_keyboard obtained via `grab_keyboard()` on the v1 context.
    /// Delivers keymap / key / modifiers events in v1, mirroring v2's
    /// `ZwpInputMethodKeyboardGrabV2`.
    pub v1_keyboard: Option<wayland_client::protocol::wl_keyboard::WlKeyboard>,

    // ── wlr-foreign-toplevel-management focus tracking ───────────────────────
    /// Manager proxy, bound when the compositor exports the global.
    pub ftl_manager: Option<ZwlrForeignToplevelManagerV1>,
    /// Per-handle accumulated app_id/title/activated state, keyed by ObjectId.
    pub(crate) toplevels: HashMap<ObjectId, ToplevelEntry>,
    /// ObjectId of the currently-activated toplevel, if any.
    pub active_toplevel: Option<ObjectId>,
    /// Sender into the active focus backend's channel. Populated by both wlr
    /// and sway backends so dispatch can push focus diffs uniformly.
    pub focus_tx: Option<mpsc::UnboundedSender<FocusEvent>>,
    /// Shared snapshot of the currently-focused app. Written by dispatch
    /// (wlr) or the sway poller; read synchronously from
    /// `apply_done_frame` at activate time.
    pub focus_current: Arc<Mutex<Option<FocusEvent>>>,
    /// X11 bridge for XWayland detection. `Some` when `$DISPLAY` is set and
    /// the x11rb connection succeeded.
    pub(crate) x11_bridge: Option<X11Bridge>,

    // ── KDE Plasma window-management focus tracking ─────────────────────────
    #[cfg(feature = "kde")]
    pub(crate) plasma_toplevels: HashMap<ObjectId, PlasmaToplevelEntry>,
    #[cfg(feature = "kde")]
    pub(crate) plasma_manager: Option<OrgKdePlasmaWindowManagement>,
}

impl AdapterState {
    pub fn new() -> Self {
        Self {
            seat: None,
            im_manager: None,
            vk_manager: None,
            im: None,
            grab: None,
            vk: None,
            conn: None,
            xkb: None,
            keymap_init: false,
            daklak_keymap: match keymap::build() {
                Ok(km) => {
                    tracing::info!(
                        size = km.size,
                        vn_pairs = keymap::vn_pairs(),
                        "synthetic Vietnamese keymap built (Tier 4 VkOnly, FOUR_LEVEL ≤255)"
                    );
                    Some(km)
                }
                Err(e) => {
                    tracing::warn!(
                        "daklak keymap synthesis failed → falling back to compositor passthrough: {e:#}"
                    );
                    None
                }
            },
            serial: 0,
            pending_commit: false,
            modifiers: ModifierState::empty(),
            raw_mods: (0, 0, 0, 0),
            pending_frame: DoneFrame::default(),
            synthetic_mods_pending: 0,
            synthetic_mods_emitted_at: None,
            last_forwarded_key: None,
            last_forwarded_release: None,
            forwarding_repeat: false,
            pending_im_commit_ack: false,
            should_exit: false,
            // Placeholder until `connect()` builds the real profile. Never
            // observed before connect overwrites it.
            profile: crate::TransportProfile::for_protocol(crate::ImProtocol::ImV2),
            im_v1: None,
            im_ctx_v1: None,
            v1_keyboard: None,
            ftl_manager: None,
            toplevels: HashMap::new(),
            active_toplevel: None,
            focus_tx: None,
            focus_current: Arc::new(Mutex::new(None)),
            x11_bridge: None,
            #[cfg(feature = "kde")]
            plasma_toplevels: HashMap::new(),
            #[cfg(feature = "kde")]
            plasma_manager: None,
        }
    }

    pub(crate) fn held_user_kc(&self) -> Option<u32> {
        match (self.last_forwarded_key, self.last_forwarded_release) {
            (Some((kc_p, t_p)), Some((kc_r, t_r))) if kc_p == kc_r && t_r > t_p => None,
            (Some((kc_p, _)), _) => Some(kc_p),
            (None, _) => None,
        }
    }

    pub(crate) fn log_duplicate_tail_diagnostic(
        &self,
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
        let press_match = self.last_forwarded_key.filter(|(kc, _)| *kc == spec.keycode);
        let release_match = self
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
                last_press_kc = ?self.last_forwarded_key.map(|(k, _)| k),
                last_release_kc = ?self.last_forwarded_release.map(|(k, _)| k),
                "tail-check: vk_only commit tail keycode differs from last forwarded"
            );
        }
    }

    /// Forward-path key emit. Falls through to `zwp_virtual_keyboard_v1`
    /// (v2) or `zwp_input_method_context_v1.key` (v1). Used by
    /// `forward_press` / `dispatch_key_release` / `KeyDecision::ForwardRaw`.
    ///
    /// Tier 4 (VkOnly Vietnamese precomposed chars) does NOT route here;
    /// it goes through `with_sink → synth_keymap_emitter` which is always
    /// vk_v2.
    /// The wl_keyboard key state for a press: `2` (Repeated) while dispatching
    /// a repeat, else `1` (Pressed). Release is always `0` (passed explicitly).
    pub fn press_value(&self) -> u32 {
        if self.forwarding_repeat {
            2
        } else {
            1
        }
    }

    pub fn emit_forward_key(&mut self, time: u32, key: u32, value: u32) {
        let serial = self.serial;
        match self.profile.protocol {
            crate::ImProtocol::ImV2 => {
                if let Some(vk) = &self.vk {
                    vk.key(time, key, value);
                }
            }
            crate::ImProtocol::ImV1 => {
                if let Some(ctx) = &self.im_ctx_v1 {
                    ctx.key(serial, time, key, value);
                }
            }
        }
    }

    /// Single funnel for input-method events that mutate `pending_frame`.
    /// Backend dispatchers translate their protocol enum into `FrameEvent`
    /// and call this. Centralizes tracing, field assignment, and v1's
    /// `pending_commit` bookkeeping so changes can't drift between v1/v2.
    pub fn apply_event(&mut self, event: crate::frame::FrameEvent) {
        use crate::frame::{FrameEvent, SurroundingText};
        match event {
            FrameEvent::Activate => {
                tracing::trace!("im: Activate");
                self.pending_frame.pending_activate = true;
                // v1 resets pending_commit at activate (CommitState
                // batch starts fresh). v2 doesn't track this field.
                self.pending_commit = false;
            }
            FrameEvent::Deactivate => {
                tracing::trace!("im: Deactivate");
                self.pending_frame.pending_deactivate = true;
            }
            FrameEvent::SurroundingText { text, cursor, anchor } => {
                tracing::trace!(text = %text, cursor, anchor, "im: SurroundingText");
                self.pending_frame.surrounding_text =
                    Some(SurroundingText { text, cursor, anchor });
                self.pending_commit = true;
            }
            FrameEvent::Purpose(purpose) => {
                tracing::trace!(purpose, "im: ContentType");
                self.pending_frame.purpose = purpose;
                self.pending_commit = true;
            }
            FrameEvent::ChangeCause(cause) => {
                tracing::trace!(cause, "im: TextChangeCause");
                self.pending_frame.change_cause = Some(cause);
            }
            FrameEvent::Unavailable => {
                tracing::error!("compositor sent Unavailable — another IM is registered");
                self.should_exit = true;
            }
        }
    }
}
