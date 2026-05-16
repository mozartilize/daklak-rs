use wayland_client::{
    delegate_noop,
    globals::GlobalListContents,
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

use crate::protocols::{
    input_method_v2::{
        zwp_input_method_keyboard_grab_v2::{self, ZwpInputMethodKeyboardGrabV2},
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::{self, ZwpInputMethodV2},
    },
    virtual_keyboard_v1::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
};
use super::AppState;

// ── No-op dispatches ─────────────────────────────────────────────────────────

delegate_noop!(AppState: ignore ZwpInputMethodManagerV2);
delegate_noop!(AppState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(AppState: ignore ZwpVirtualKeyboardV1);

// ── Registry dispatch ────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handled by registry_queue_init
    }
}

// ── Seat dispatch ─────────────────────────────────────────────────────────────

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for AppState {
    fn event(
        _state: &mut Self,
        _seat: &wayland_client::protocol::wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── Input method v2 dispatch ─────────────────────────────────────────────────

impl Dispatch<ZwpInputMethodV2, ()> for AppState {
    fn event(
        state: &mut Self,
        _im: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => {
                tracing::trace!("im_v2: Activate");
                state.pending_frame.pending_activate = true;
            }
            zwp_input_method_v2::Event::Deactivate => {
                tracing::trace!("im_v2: Deactivate");
                state.pending_frame.pending_deactivate = true;
            }
            zwp_input_method_v2::Event::SurroundingText { text, cursor, anchor } => {
                tracing::trace!("im_v2: SurroundingText cursor={cursor}");
                state.pending_frame.surrounding_text =
                    Some(super::frame::SurroundingText { text, cursor, anchor });
            }
            zwp_input_method_v2::Event::TextChangeCause { cause } => {
                state.pending_frame.change_cause = Some(cause.into());
            }
            zwp_input_method_v2::Event::ContentType { hint: _, purpose } => {
                state.pending_frame.purpose = purpose.into();
            }
            zwp_input_method_v2::Event::Done => {
                tracing::trace!("im_v2: Done");
                state.apply_done_frame();
            }
            zwp_input_method_v2::Event::Unavailable => {
                tracing::error!("compositor sent Unavailable — another IM is registered");
                state.should_exit = true;
            }
            _ => {}
        }
    }
}

// ── Keyboard grab v2 dispatch ─────────────────────────────────────────────────

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for AppState {
    fn event(
        state: &mut Self,
        _grab: &ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_keyboard_grab_v2::Event::Keymap { format, fd, size } => {
                use std::os::fd::AsFd;

                // Forward keymap to virtual keyboard FIRST (borrows fd) — must
                // be called once before any vk.key() calls (kime pattern: state.rs:620-626).
                //
                // Path C: prefer our synthesized keymap (Vietnamese precomposed
                // chars at evdev 200+). Standard QWERTY is preserved via
                // `include "evdev+aliases(qwerty)"` so existing tiers' BS=14
                // and other forwarded keycodes still work. If synthesis
                // failed at startup, fall back to forwarding the compositor's
                // keymap so vk_key still does *something* sensible.
                if !state.keymap_init {
                    if let Some(ref vk) = state.vk {
                        if let Some(ref km) = state.daklak_keymap {
                            // 1 == zwp_virtual_keyboard_v1::KeymapFormat::XkbV1
                            vk.keymap(1, km.fd.as_fd(), km.size);
                            tracing::debug!(
                                size = km.size,
                                "vk.keymap → daklak synthetic keymap"
                            );
                        } else {
                            vk.keymap(format.into(), fd.as_fd(), size);
                            tracing::debug!(
                                size,
                                "vk.keymap → compositor passthrough (daklak keymap unavailable)"
                            );
                        }
                        state.keymap_init = true;
                    }
                }

                // Initialize xkb state — consumes fd after borrow above has ended
                if state.xkb.is_none() {
                    match super::xkb::XkbState::from_fd(fd, size) {
                        Ok(xkb) => {
                            state.xkb = Some(xkb);
                            tracing::debug!("xkb state initialized");
                        }
                        Err(e) => {
                            tracing::error!("xkb init failed: {e}");
                        }
                    }
                }
            }

            zwp_input_method_keyboard_grab_v2::Event::Key { time, key, state: key_state, .. } => {
                // key_state is u32 (enum= stripped from XML): 0=released, 1=pressed, 2=repeat
                tracing::trace!(key, key_state, "grab.Key");
                if key_state != 0 {
                    state.on_key_pressed(time, key);
                } else {
                    state.on_key_released(time, key);
                }
            }

            zwp_input_method_keyboard_grab_v2::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                tracing::trace!(mods_depressed = format!("{:#x}", mods_depressed), "grab.Modifiers");
                state.on_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }

            zwp_input_method_keyboard_grab_v2::Event::RepeatInfo { .. } => {
                // Key repeat not implemented yet (Stage 3 scope)
            }

            _ => {}
        }
    }
}
