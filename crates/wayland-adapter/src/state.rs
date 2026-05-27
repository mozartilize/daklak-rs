use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use viet_ime_edit_strategy::uinput_device::UinputDevice;
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

/// Window in which to drop daklak's own uinput round-trips. ~3× Tier 3
/// grab-dance budget (~9ms) keeps clear of any human keystroke interval.
pub(crate) const SELF_EMIT_WINDOW: Duration = Duration::from_millis(20);

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

    /// Connection clone for grab-release/regrab flush around Tier 3 uinput
    /// emission.
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

    // uinput device for Tier 3 — None if /dev/uinput is not accessible
    pub uinput: Option<UinputDevice>,

    /// Queue of (keycode, value, emitted_at) for kernel events daklak just
    /// synthesized via /dev/uinput. Each entry is round-tripped through the
    /// IM grab; on_key_pressed / on_key_released match and drop the matching
    /// entry so we don't re-process our own emissions.
    pub pending_self_emits: VecDeque<(u16, i32, Instant)>,

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

    pub should_exit: bool,

    // ── Compositor backend selection ─────────────────────────────────────────
    /// Which IM protocol is active. Set during `connect()`.
    pub im_backend: crate::ImBackend,
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
        let uinput = match UinputDevice::open() {
            Ok(d) => {
                tracing::info!("uinput device opened (Tier 3 available)");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("uinput unavailable ({e}); Tier 3 demoted to ForwardKey");
                None
            }
        };

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
                        "synthetic Vietnamese keymap built (Path C, FOUR_LEVEL ≤255)"
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
            uinput,
            pending_self_emits: VecDeque::new(),
            synthetic_mods_pending: 0,
            synthetic_mods_emitted_at: None,
            last_forwarded_key: None,
            last_forwarded_release: None,
            should_exit: false,
            im_backend: crate::ImBackend::V2Wlroots,
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

    /// Check whether the incoming grab key event matches a recent self-emit
    /// from /dev/uinput. Drains expired entries (>20ms old) before matching.
    /// Returns true if the event should be suppressed (i.e. dropped silently).
    ///
    /// Matching is **strict FIFO**.
    pub fn suppress_self_emit(&mut self, key: u32, value: i32) -> bool {
        while let Some(&(_, _, t)) = self.pending_self_emits.front() {
            if t.elapsed() > SELF_EMIT_WINDOW {
                self.pending_self_emits.pop_front();
            } else {
                break;
            }
        }
        match self.pending_self_emits.front() {
            Some(&(k, v, _)) if k as u32 == key && v == value => {
                self.pending_self_emits.pop_front();
                true
            }
            _ => false,
        }
    }

    /// Forward-path key emit. Falls through to `zwp_virtual_keyboard_v1`
    /// (v2) or `zwp_input_method_context_v1.key` (v1). Used by
    /// `forward_press` / `dispatch_key_release` / `KeyDecision::ForwardRaw`.
    ///
    /// Tier 4 (VkOnly Vietnamese precomposed chars) does NOT route here;
    /// it goes through `with_sink → synth_keymap_emitter` which is always
    /// vk_v2.
    pub fn emit_forward_key(&mut self, time: u32, key: u32, value: u32) {
        let serial = self.serial;
        match self.im_backend {
            crate::ImBackend::V2Wlroots => {
                if let Some(vk) = &self.vk {
                    vk.key(time, key, value);
                }
            }
            crate::ImBackend::V1Kde => {
                if let Some(ctx) = &self.im_ctx_v1 {
                    ctx.key(serial, time, key, value);
                }
            }
        }
    }
}
