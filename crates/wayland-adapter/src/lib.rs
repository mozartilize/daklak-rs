#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

//! Wayland adapter — pure Wayland protocol I/O layer for the daklak Vietnamese IME.
//!
//! Owns:
//! - Wayland input-method negotiation and protocol lifecycles
//! - independently selected text and key output channels
//! - xkb keymap loading + char translation
//! - Daklak synthetic keymap upload to vk (key-channel Vietnamese emit)
//! - Synthetic-mods echo suppression
//! - Optional toplevel focus tracking + XWayland metadata
//! - Held forwarded-key bookkeeping for the tail-char-drop fix
//!
//! Does NOT own:
//! - Engine state / composition logic
//! - Strategy / shadow buffer (lives in `edit-strategy`)
//! - Session routing and tier selection (the daemon)
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
mod wire;

use std::os::fd::AsFd;
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use wayland_client::{
    globals::registry_queue_init, Connection,
};

use crate::focus::FocusTracker;

pub use crate::sink::AdapterSink;
pub use crate::state::AdapterState;
pub use viet_ime_edit_strategy::{BackspaceMethod, KeyState, ModifierState, OutputSink};

// ── Public types ─────────────────────────────────────────────────────────────

/// The input-method protocol negotiated at `connect()`. IMv2 always comes
/// with a virtual keyboard; see the adapter's negotiation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImProtocol {
    /// `zwp_input_method_v2` + `zwp_virtual_keyboard_v1`.
    ImV2,
    /// `zwp_input_method_v1`.
    ImV1,
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

    /// Called by the adapter with `raw_mods` snapshotted just before this
    /// call. Daemon constructs a sink via `ctx.with_sink` and runs
    /// `strategy.apply`.
    fn apply_pending(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        backspaces: usize,
        commit: &str,
        raw_mods: (u32, u32, u32, u32),
    );

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

    /// Replacement text can be typed as keysyms through the IMv1 context.
    pub fn has_keysym_commit(&self) -> bool {
        self.state.wire.has_keysym_commit()
    }

    /// A ForwardKey session can be created from focus alone, for a client
    /// that never activates text-input.
    pub fn supports_focus_session(&self) -> bool {
        self.state.wire.supports_focus_session()
    }

    /// Forward a raw press WITHOUT marking the key held. Used by the
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

    /// Type `ch`, decoded from the physical `key`, on the keyboard channel
    /// (im-v1 keysym) instead of forwarding the keycode. Returns `false` if
    /// nothing was sent (no key channel or no keysym for `ch`); the caller
    /// then needs another route.
    ///
    /// Used where a forwarded keycode would lose its modifier level (KWin
    /// re-decodes IM-forwarded keys at base level). Unlike `commit_string`,
    /// the keysym reaches the client as a key event, in order with the keys
    /// forwarded around it. The physical key is consumed, so it is repeated
    /// like an applied key.
    pub fn type_char_on_key_channel(
        &mut self,
        time: u32,
        key: u32,
        ch: char,
        commit_string_functional: bool,
    ) -> bool {
        let serial = self.state.serial;
        let raw_mods = self.state.raw_mods;
        let mut typed = false;
        self.with_sink(raw_mods, commit_string_functional, |sink| {
            typed = sink.commit_key_channel_text(serial, time, &ch.to_string());
        });
        if typed && !self.state.forwarding_repeat {
            self.state.arm_consumed_key_repeat(key, time);
        }
        typed
    }

    /// Construct an AdapterSink bound to live adapter proxies + the supplied
    /// raw_mods snapshot, then run the closure with `&mut sink`. The closure
    /// typically invokes `strategy.apply` on the resulting sink. The sink
    /// releases held user keys it re-presses (tail-char-drop fix) and drops
    /// them from the held set.
    pub fn with_sink<F>(
        &mut self,
        raw_mods: (u32, u32, u32, u32),
        commit_string_functional: bool,
        f: F,
    ) where
        F: FnOnce(&mut AdapterSink<'_>),
    {
        // None only between IMv1 text-input contexts.
        let Some((conn, text_ops, keys)) = self.state.wire.outputs(self.state.serial) else {
            return;
        };
        let mut sink = AdapterSink {
            text_ops,
            keys,
            echo: &mut self.state.echo,
            raw_mods,
            held: &mut self.state.held_forwarded,
            commit_string_functional,
            conn,
            xkb: self.state.xkb.as_ref(),
        };
        f(&mut sink);
    }
}

// ── Wayland adapter struct ───────────────────────────────────────────────────

pub struct WaylandAdapter<H: AdapterHandler> {
    pub handler: H,
    pub state: AdapterState,
}

fn repeat_forward_values(client_timer: bool) -> &'static [u32] {
    if client_timer {
        // A locally generated repeat follows an IM-consumed initial press, so
        // the focused app has no held key. Emit a complete pulse; clients such
        // as foot, gedit and Firefox ignore state=2 without an earlier press.
        &[1, 0]
    } else {
        // Compositor-delivered repeat: preserve the protocol repeat state.
        &[2]
    }
}

impl<H: AdapterHandler> WaylandAdapter<H> {
    pub(crate) fn dispatch_key_release(&mut self, time: u32, key: u32) {
        tracing::info!(key, "dispatch_key_release: IM grab delivered release");
        self.state.client_repeat.release(key);
        // Releases route through the same forward path as presses so the
        // focused app sees a balanced press/release pair.
        self.state.emit_forward_key(time, key, 0);
        self.state.held_forwarded.release(key);
    }

    pub(crate) fn dispatch_key_press(&mut self, time: u32, key: u32) {
        tracing::info!(key, "dispatch_key_press: IM grab delivered press");
        // wl_keyboard repeats the most recently pressed repeatable key. A new
        // physical press always supersedes any consumed-key timer; Apply below
        // arms this key again, while ForwardRaw leaves repetition to the app.
        self.state.client_repeat.cancel();
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
                self.state.held_forwarded.press(key);
            }
            KeyDecision::Apply {
                backspaces,
                commit,
            } => {
                self.state.arm_consumed_key_repeat(key, time);
                let raw_mods = self.state.raw_mods;
                {
                    let mut ctx = AdapterCtx {
                        state: &mut self.state,
                    };
                    self.handler
                        .apply_pending(&mut ctx, time, backspaces, &commit, raw_mods);
                }

                // (Tail-char drop on space-after-tone on Tier 2
                // ForwardKey + foot/KWin is handled per-keysym inside
                // `AdapterSink::commit_via_keysym_v1` — see that fn for
                // the kc 247 forwardKeySym race explanation. No
                // additional post-apply sleep needed at this boundary.)

            }
        }
    }

    /// IM grab delivered a key REPEAT (wl_keyboard state=2). KWin generates
    /// server-side repeat as state=2 key events; rate-0 clients (Chromium /
    /// Electron / ghostty on KWin) rely on those instead of self-repeating.
    ///
    /// A repeat is processed like a real press (matching evdev, where every
    /// kernel repeat is a real keystroke): the daemon runs its full decision.
    /// `forwarding_repeat` makes nav / modifier-shortcut / raw forwards emit
    /// `value=2` so the client repeats. When the held key composes, the daemon
    /// returns `Apply` and we apply the delete+commit edit so `a` → `â` → raw
    /// `aaa` plays out on hold. The edit's own key events must use normal
    /// press/release values, so `forwarding_repeat` is cleared around the apply.
    pub(crate) fn dispatch_key_repeat(&mut self, time: u32, key: u32) {
        self.dispatch_key_repeat_inner(time, key, false);
    }

    pub(crate) fn dispatch_client_key_repeat(&mut self, time: u32, key: u32) {
        self.dispatch_key_repeat_inner(time, key, true);
    }

    fn dispatch_key_repeat_inner(&mut self, time: u32, key: u32, client_timer: bool) {
        tracing::trace!(key, client_timer, "dispatch_key_repeat: repeat tick");
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
            // via the ctx helpers.
            KeyDecision::Consumed => {}
            // Forward-only keys (Enter, digits, punctuation, no active window /
            // no xkb char): emit the repeat directly as value=2. The key's
            // held state is unchanged by a repeat.
            KeyDecision::ForwardRaw => {
                for &value in repeat_forward_values(client_timer) {
                    self.state.emit_forward_key(time, key, value);
                }
            }
            // A held compose key recomposes: apply the delete+commit edit the
            // same way dispatch_key_press does. The edit's internal key events
            // (backspaces / forwarded commit) must be normal press/release, not
            // value=2, so clear forwarding_repeat for the apply.
            KeyDecision::Apply {
                backspaces,
                commit,
            } => {
                self.state.forwarding_repeat = false;
                let raw_mods = self.state.raw_mods;
                let mut ctx = AdapterCtx {
                    state: &mut self.state,
                };
                self.handler
                    .apply_pending(&mut ctx, time, backspaces, &commit, raw_mods);
            }
        }
        self.state.forwarding_repeat = false;
    }

    pub(crate) fn apply_done_frame(&mut self) {
        self.state.wire.begin_frame();

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
            match self.state.focus_snapshot() {
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
        self.state.wire.ack_frame(self.state.serial);

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
        let incoming_mods = (mods_depressed, mods_latched, mods_locked, group);
        if self.state.echo.suppress(incoming_mods) {
            return;
        }

        let modifier_masks = if let Some(xkb) = &mut self.state.xkb {
            xkb.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
            xkb.canonical_modifier_masks()
        } else {
            xkb::CanonicalModifierMasks::default()
        };

        let mut m = ModifierState::empty();
        if mods_depressed & modifier_masks.shift != 0 {
            m |= ModifierState::SHIFT;
        }
        if mods_depressed & modifier_masks.control != 0 {
            m |= ModifierState::CTRL;
        }
        if mods_depressed & modifier_masks.alt != 0 {
            m |= ModifierState::ALT;
        }
        if mods_depressed & modifier_masks.logo != 0 {
            m |= ModifierState::SUPER;
        }
        self.state.modifiers = m;
        self.state.raw_mods = (mods_depressed, mods_latched, mods_locked, group);

        self.state.wire.forward_modifiers(self.state.serial, incoming_mods);

        let mut ctx = AdapterCtx {
            state: &mut self.state,
        };
        self.handler.on_modifiers(&mut ctx, m);
    }
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
    let wire = wire::Wire::connect(&globals, &qh, conn.clone())?;
    tracing::info!(?wire, "Wayland channels ready");
    let (focus, backend) = FocusTracker::probe(&globals, &qh);
    let mut app = WaylandAdapter { handler, state: AdapterState::new(wire, focus) };

    // The complete state must exist before any activation callback.
    event_queue.roundtrip(&mut app).context("initial roundtrip")?;

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

    fn app<H: AdapterHandler>(handler: H) -> (WaylandAdapter<H>, crate::wire::TestPeer) {
        let (state, peer) = AdapterState::for_test(ImProtocol::ImV2);
        (WaylandAdapter { handler, state }, peer)
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

    struct ApplyHandler;

    impl AdapterHandler for ApplyHandler {
        fn on_done_frame(&mut self, _ctx: &mut AdapterCtx<'_>, _frame: &FrameSnapshot) {}

        fn on_key_pressed(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _time: u32,
            _key: u32,
            _ch: Option<char>,
        ) -> KeyDecision {
            KeyDecision::Apply {
                backspaces: 0,
                commit: "o".to_owned(),
            }
        }

        fn apply_pending(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _time: u32,
            _backspaces: usize,
            _commit: &str,
            _raw_mods: (u32, u32, u32, u32),
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
    fn applied_key_arms_client_repeat_until_release() {
        let (mut app, _peer) = app(ApplyHandler);
        app.state.xkb = Some(viet_ime_keymap::xkb::XkbState::us_for_test());
        app.state.client_repeat.set_info(25, 600);

        app.dispatch_key_press(1234, 24);
        assert!(app.state.client_repeat.deadline().is_some());

        app.dispatch_key_release(2234, 24);
        assert_eq!(app.state.client_repeat.deadline(), None);
    }

    #[test]
    fn applied_nonrepeatable_key_does_not_arm_client_repeat() {
        let (mut app, _peer) = app(ApplyHandler);
        app.state.xkb = Some(viet_ime_keymap::xkb::XkbState::us_for_test());
        app.state.client_repeat.set_info(25, 600);

        app.dispatch_key_press(1234, 42); // left shift

        assert_eq!(app.state.client_repeat.deadline(), None);
    }

    #[test]
    fn real_modifier_update_is_not_swallowed_by_pending_synthetic_echo() {
        let (mut app, _peer) = app(NoopHandler);
        app.state.xkb = Some(viet_ime_keymap::xkb::XkbState::us_for_test());
        app.state.echo.pending = 1;
        app.state.echo.expected.push_back((0, 0, 0, 0));
        app.state.echo.emitted_at = Some(std::time::Instant::now());

        app.handle_modifiers(0x01, 0, 0, 0);

        assert!(
            app.state.modifiers.contains(ModifierState::SHIFT),
            "a real Shift modifiers event must update adapter state even when a synthetic echo is pending"
        );
        assert_eq!(app.state.raw_mods, (0x01, 0, 0, 0));
    }

    #[test]
    fn client_timer_raw_repeat_is_a_balanced_pulse() {
        assert_eq!(repeat_forward_values(true), &[1, 0]);
        assert_eq!(repeat_forward_values(false), &[2]);
    }

    #[test]
    fn press_value_is_2_only_while_forwarding_repeat() {
        // The whole repeat fix hinges on this: a fresh press forwards as
        // wl_keyboard state=1, a key-repeat (state=2) as state=2 so rate-0
        // clients (Chromium on KWin) see continuous-key.
        let (mut s, _peer) = AdapterState::for_test(ImProtocol::ImV2);
        assert_eq!(s.press_value(), 1, "default press = state 1");
        s.forwarding_repeat = true;
        assert_eq!(s.press_value(), 2, "repeat press = state 2");
        s.forwarding_repeat = false;
        assert_eq!(s.press_value(), 1, "back to state 1 once repeat clears");
    }

    /// Forwards every key raw except KEY_J (36), which composes; records the
    /// held set each edit is applied with.
    #[derive(Default)]
    struct ToneOnJHandler {
        held_at_apply: Vec<viet_ime_key_emitter::HeldKeys>,
    }

    impl AdapterHandler for ToneOnJHandler {
        fn on_done_frame(&mut self, _ctx: &mut AdapterCtx<'_>, _frame: &FrameSnapshot) {}

        fn on_key_pressed(
            &mut self,
            _ctx: &mut AdapterCtx<'_>,
            _time: u32,
            key: u32,
            _ch: Option<char>,
        ) -> KeyDecision {
            if key == 36 {
                KeyDecision::Apply {
                    backspaces: 3,
                    commit: "ạnh".to_owned(),
                }
            } else {
                KeyDecision::ForwardRaw
            }
        }

        fn apply_pending(
            &mut self,
            ctx: &mut AdapterCtx<'_>,
            _time: u32,
            _backspaces: usize,
            _commit: &str,
            _raw_mods: (u32, u32, u32, u32),
        ) {
            self.held_at_apply.push(ctx.state.held_forwarded.clone());
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
    fn edit_sees_every_forwarded_key_still_held() {
        // Fast `hanhj`: when `j` composes `ạnh`, both `n` and the second `h`
        // are still down. The edit re-presses both keycodes, so both must be
        // known as held, not only the most recent press.
        const H: u32 = 35;
        const A: u32 = 30;
        const N: u32 = 49;
        const J: u32 = 36;
        let (mut app, _peer) = app(ToneOnJHandler::default());
        app.dispatch_key_press(0, H);
        app.dispatch_key_press(1, A);
        app.dispatch_key_release(2, H);
        app.dispatch_key_press(3, N);
        app.dispatch_key_press(4, H);
        app.dispatch_key_release(5, A);
        app.dispatch_key_press(6, J);

        let held = &app.handler.held_at_apply[0];
        assert!(held.contains(N), "n is still held: {held:?}");
        assert!(held.contains(H), "second h is still held: {held:?}");
        assert!(!held.contains(A), "a was released: {held:?}");
        assert!(!held.contains(J), "consumed j never reached the client");

        app.dispatch_key_release(7, N);
        assert!(!app.state.held_forwarded.contains(N));
        assert!(app.state.held_forwarded.contains(H));
    }

    #[test]
    fn key_repeat_does_not_mark_a_released_key_held() {
        let (mut app, _peer) = app(ToneOnJHandler::default());
        app.dispatch_key_press(0, 30);
        app.dispatch_key_release(1, 30);
        app.dispatch_key_repeat(2, 30);
        assert!(!app.state.held_forwarded.contains(30));
    }

    #[test]
    fn is_level_shifted_compares_decoded_char_to_base_level() {
        let (mut s, _peer) = AdapterState::for_test(ImProtocol::ImV2);
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
        let (mut s, _peer) = AdapterState::for_test(ImProtocol::ImV2);
        {
            let ctx = AdapterCtx { state: &mut s };
            assert!(!ctx.is_repeat());
        }
        s.forwarding_repeat = true;
        let ctx = AdapterCtx { state: &mut s };
        assert!(ctx.is_repeat());
    }
}
