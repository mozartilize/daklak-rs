use wayland_client::{
    event_created_child,
    protocol::wl_keyboard::{self, WlKeyboard},
    Dispatch, Proxy, QueueHandle, WEnum,
};

use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_v1::{
    self, ZwpInputMethodV1,
};
use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_context_v1::{
    self, ZwpInputMethodContextV1,
};

use crate::{AdapterHandler, WaylandAdapter};

// ── v1 Input Method dispatch ──────────────────────────────────────────────

impl<H: AdapterHandler> Dispatch<ZwpInputMethodV1, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        proxy: &ZwpInputMethodV1,
        event: zwp_input_method_v1::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v1::Event::Activate { id } => {
                tracing::trace!(
                    im_id = ?proxy.id(),
                    ctx_id = ?id.id(),
                    "im_v1: Activate — creating keyboard grab"
                );
                state.state.wire.activate_v1(id, qh);
                state.state.serial = 0;
                state.state.pending_frame.reset();
                state.state.pending_frame.end_session();
                state.state.apply_event(crate::frame::FrameEvent::Activate);
                // Don't fire apply_done_frame yet — wait for CommitState so
                // that surrounding_text and purpose are available when the
                // handler runs detect_capability (which distinguishes
                // SurroundingText vs ForwardKey).
            }
            zwp_input_method_v1::Event::Deactivate { context } => {
                tracing::trace!(
                    ctx_id = ?context.id(),
                    "im_v1: Deactivate — dropping context + keyboard"
                );
                if !state.state.wire.deactivate_v1(&context) {
                    return;
                }
                state.state.client_repeat.cancel();
                state.state.held_forwarded.clear();
                state.state.apply_event(crate::frame::FrameEvent::Deactivate);
                // Deactivate has no trailing CommitState — fire immediately.
                state.apply_done_frame();
            }
            other => {
                tracing::warn!(?other, "im_v1: unhandled ZwpInputMethodV1 event");
            }
        }
    }

    event_created_child!(WaylandAdapter<H>, ZwpInputMethodV1, [
        0 => (ZwpInputMethodContextV1, ()),
    ]);
}

// ── v1 Input Method Context dispatch (text events) ────────────────────────

impl<H: AdapterHandler> Dispatch<ZwpInputMethodContextV1, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        proxy: &ZwpInputMethodContextV1,
        event: zwp_input_method_context_v1::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if !state.state.wire.is_active_context(proxy) {
            return;
        }
        match event {
            zwp_input_method_context_v1::Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                state.state.apply_event(crate::frame::FrameEvent::SurroundingText {
                    text,
                    cursor,
                    anchor,
                });
            }

            zwp_input_method_context_v1::Event::ContentType { hint: _, purpose } => {
                // text-input-unstable-v1 purpose enum differs from
                // text-input-v3: v3 added `pin=9`, shifting everything
                // 9+ by one. The v3 numbering (terminal=13) is what the
                // logged `purpose` diagnostic expects. Translate at this
                // boundary so KWin's v1 value 12 (=terminal in v1) becomes 13
                // (=terminal in v3).
                //
                //   v1 0..=8  → v3 0..=8   (normal/alpha/.../password)
                //   v1 9 date → v3 10 date
                //   v1 10 time → v3 11 time
                //   v1 11 datetime → v3 12 datetime
                //   v1 12 terminal → v3 13 terminal
                let purpose_v3 = if purpose >= 9 { purpose + 1 } else { purpose };
                state.state.apply_event(crate::frame::FrameEvent::Purpose(purpose_v3));
            }

            zwp_input_method_context_v1::Event::CommitState { serial } => {
                tracing::trace!(
                    ctx_id = ?proxy.id(),
                    serial,
                    "im_v1: CommitState — applying frame"
                );
                state.state.serial = serial;
                let pending = state.state.wire.take_pending_commit();
                if pending || state.state.pending_frame.pending_activate {
                    state.apply_done_frame();
                }
            }

            zwp_input_method_context_v1::Event::Reset => {
                tracing::trace!("im_v1: Reset");
                state.state.pending_frame.reset();
            }

            zwp_input_method_context_v1::Event::InvokeAction { .. } => {}

            other => {
                tracing::warn!(
                    ctx_id = ?proxy.id(),
                    ?other,
                    "im_v1: unhandled context event"
                );
            }
        }
    }
}

// ── v1 wl_keyboard dispatch (grab_keyboard) ───────────────────────────────
// v1 delivers keymap / key / modifiers events through a wl_keyboard object
// created via ZwpInputMethodContextV1::grab_keyboard(), mirroring the v2
// ZwpInputMethodKeyboardGrabV2 pattern.

impl<H: AdapterHandler> Dispatch<WlKeyboard, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        keyboard: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if !state.state.wire.is_active_keyboard(keyboard) {
            return;
        }
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                use std::os::fd::AsFd;
                state.state.wire.on_keymap(format.into(), fd.as_fd(), size);
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    size,
                    "v1: wl_keyboard keymap"
                );
                if state.state.xkb.is_none() {
                    match viet_ime_keymap::xkb::XkbState::from_fd(fd, size) {
                        Ok(xkb) => {
                            state.state.xkb = Some(xkb);
                            tracing::debug!("xkb state initialized via v1 wl_keyboard Keymap");
                        }
                        Err(e) => {
                            tracing::error!("xkb init failed (v1 Keymap): {e}");
                        }
                    }
                }
            }

            wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: key_state,
                ..
            } => {
                // KWin delivers server-side key REPEAT as state=2, surfaced
                // here as `WEnum::Unknown(2)` (the enum has no Repeated variant
                // in this protocol version). Route it to dispatch_key_repeat so
                // the client sees a value=2 repeat — collapsing it into a press
                // breaks continuous-key for rate-0 clients (Chromium on KWin).
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    key,
                    ?key_state,
                    serial,
                    time,
                    "v1: wl_keyboard key"
                );
                match key_state {
                    WEnum::Value(wl_keyboard::KeyState::Pressed) => {
                        state.dispatch_key_press(time, key)
                    }
                    // KWin sends repeat as KeyState::Repeated; Unknown(2) is a
                    // fallback for protocol versions lacking the variant.
                    WEnum::Value(wl_keyboard::KeyState::Repeated) | WEnum::Unknown(2) => {
                        state.dispatch_key_repeat(time, key)
                    }
                    _ => state.dispatch_key_release(time, key),
                }
            }

            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                tracing::debug!(
                    kbd_id = ?keyboard.id(),
                    mods_depressed = format!("{:#x}", mods_depressed),
                    "v1: wl_keyboard modifiers"
                );
                state.handle_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }

            wl_keyboard::Event::Enter { .. } => {
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    "v1: wl_keyboard enter"
                );
            }

            wl_keyboard::Event::Leave { .. } => {
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    "v1: wl_keyboard leave"
                );
            }

            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                tracing::debug!(
                    kbd_id = ?keyboard.id(),
                    rate,
                    delay,
                    "v1: wl_keyboard repeat_info"
                );
                state.state.client_repeat.set_info(rate, delay);
            }

            other => {
                tracing::warn!(
                    kbd_id = ?keyboard.id(),
                    ?other,
                    "v1: wl_keyboard UNHANDLED event"
                );
            }
        }
    }
}
