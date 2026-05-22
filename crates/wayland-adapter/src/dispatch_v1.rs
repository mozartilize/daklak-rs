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
                let ctx = id;
                let keyboard = ctx.grab_keyboard(qh, ());
                tracing::trace!(
                    ctx_id = ?ctx.id(),
                    kbd_id = ?keyboard.id(),
                    "im_v1: keyboard grab created"
                );
                // Send modifier map (matching fcitx5 — Shift, Control, Mod1, Mod4)
                ctx.modifiers_map(
                    "Shift\0Control\0Mod1\0Mod4".as_bytes().to_vec(),
                );
                tracing::trace!(
                    ctx_id = ?ctx.id(),
                    "im_v1: modifiers_map sent"
                );
                state.state.im_ctx_v1 = Some(ctx);
                state.state.v1_keyboard = Some(keyboard);
                state.state.pending_frame.pending_activate = true;
                state.state.pending_commit = false;
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
                state.state.im_ctx_v1 = None;
                state.state.v1_keyboard = None;
                state.state.pending_frame.pending_deactivate = true;
                // Deactivate has no trailing CommitState — fire immediately.
                state.apply_done_frame();
            }
            _ => {
                tracing::trace!("im_v1: unhandled event");
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
        match event {
            zwp_input_method_context_v1::Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                tracing::trace!(
                    ctx_id = ?proxy.id(),
                    cursor,
                    anchor,
                    text_len = text.len(),
                    "im_v1: SurroundingText"
                );
                state.state.pending_frame.surrounding_text =
                    Some(crate::frame::SurroundingText { text, cursor, anchor });
                state.state.pending_commit = true;
            }

            zwp_input_method_context_v1::Event::ContentType { hint: _, purpose } => {
                tracing::trace!(
                    ctx_id = ?proxy.id(),
                    purpose,
                    "im_v1: ContentType"
                );
                state.state.pending_frame.purpose = purpose;
                state.state.pending_commit = true;
            }

            zwp_input_method_context_v1::Event::CommitState { serial } => {
                tracing::trace!(
                    ctx_id = ?proxy.id(),
                    serial,
                    "im_v1: CommitState — applying frame"
                );
                state.state.serial = serial;
                if state.state.pending_commit || state.state.pending_frame.pending_activate {
                    state.state.pending_commit = false;
                    state.apply_done_frame();
                }
            }

            zwp_input_method_context_v1::Event::Reset => {
                tracing::trace!("im_v1: Reset");
                state.state.pending_frame.reset();
            }

            zwp_input_method_context_v1::Event::InvokeAction { .. } => {}

            _ => {
                tracing::trace!(
                    ctx_id = ?proxy.id(),
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
        match event {
            wl_keyboard::Event::Keymap { format: _, fd, size } => {
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
                let pressed = matches!(
                    key_state,
                    WEnum::Value(wl_keyboard::KeyState::Pressed) | WEnum::Unknown(2)
                );
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    key,
                    pressed,
                    serial,
                    time,
                    "v1: wl_keyboard key"
                );
                if pressed {
                    state.dispatch_key_press(time, key);
                } else {
                    state.dispatch_key_release(time, key);
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
                tracing::trace!(
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
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    rate,
                    delay,
                    "v1: wl_keyboard repeat_info"
                );
            }

            _ => {
                tracing::trace!(
                    kbd_id = ?keyboard.id(),
                    "v1: wl_keyboard UNHANDLED event"
                );
            }
        }
    }
}
