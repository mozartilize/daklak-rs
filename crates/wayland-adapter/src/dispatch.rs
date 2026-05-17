use wayland_client::{
    delegate_noop,
    globals::GlobalListContents,
    protocol::{
        wl_keyboard,
        wl_registry, wl_seat,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};

use wayland_protocols_misc::{
    zwp_input_method_v2::client::{
        zwp_input_method_keyboard_grab_v2::{self, ZwpInputMethodKeyboardGrabV2},
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::{self, ZwpInputMethodV2},
    },
    zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
};
use crate::{AdapterHandler, WaylandAdapter};

// ── No-op dispatches ─────────────────────────────────────────────────────────

delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpInputMethodManagerV2);
delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpVirtualKeyboardV1);

// ── Registry dispatch ────────────────────────────────────────────────────────

impl<H: AdapterHandler> Dispatch<wl_registry::WlRegistry, GlobalListContents>
    for WaylandAdapter<H>
{
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

// ── Seat dispatch ────────────────────────────────────────────────────────────

impl<H: AdapterHandler> Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()>
    for WaylandAdapter<H>
{
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

impl<H: AdapterHandler> Dispatch<ZwpInputMethodV2, ()> for WaylandAdapter<H> {
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
                state.state.pending_frame.pending_activate = true;
            }
            zwp_input_method_v2::Event::Deactivate => {
                tracing::trace!("im_v2: Deactivate");
                state.state.pending_frame.pending_deactivate = true;
            }
            zwp_input_method_v2::Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                tracing::trace!("im_v2: SurroundingText cursor={cursor}");
                state.state.pending_frame.surrounding_text =
                    Some(crate::frame::SurroundingText { text, cursor, anchor });
            }
            zwp_input_method_v2::Event::TextChangeCause { cause } => {
                state.state.pending_frame.change_cause = Some(cause.into());
            }
            zwp_input_method_v2::Event::ContentType { hint: _, purpose } => {
                state.state.pending_frame.purpose = purpose.into();
            }
            zwp_input_method_v2::Event::Done => {
                tracing::trace!("im_v2: Done");
                state.apply_done_frame();
            }
            zwp_input_method_v2::Event::Unavailable => {
                tracing::error!("compositor sent Unavailable — another IM is registered");
                state.state.should_exit = true;
            }
            _ => {}
        }
    }
}

// ── Keyboard grab v2 dispatch ────────────────────────────────────────────────

impl<H: AdapterHandler> Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        _grab: &ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_keyboard_grab_v2::Event::Keymap { format, fd, size } => {
                use std::os::fd::AsFd;

                // Forward keymap to virtual keyboard FIRST (borrows fd) — must
                // be called once before any vk.key() calls.
                //
                // Path C: prefer our synthesized keymap (Vietnamese precomposed
                // chars at evdev 200+). Standard QWERTY is preserved via
                // `include "evdev+aliases(qwerty)"` so existing tiers' BS=14
                // and other forwarded keycodes still work. If synthesis
                // failed at startup, fall back to forwarding the compositor's
                // keymap so vk_key still does *something* sensible.
                if !state.state.keymap_init {
                    if let Some(vk) = &state.state.vk {
                        if let Some(km) = &state.state.daklak_keymap {
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
                        state.state.keymap_init = true;
                    }
                }

                // Initialize xkb state — consumes fd after borrow above has ended
                if state.state.xkb.is_none() {
                    match crate::xkb::XkbState::from_fd(fd, size) {
                        Ok(xkb) => {
                            state.state.xkb = Some(xkb);
                            tracing::debug!("xkb state initialized");
                        }
                        Err(e) => {
                            tracing::error!("xkb init failed: {e}");
                        }
                    }
                }
            }

            zwp_input_method_keyboard_grab_v2::Event::Key {
                time,
                key,
                state: key_state,
                ..
            } => {
                // wayland-protocols-misc types `state` as `WEnum<KeyState>`.
                // Treat `Pressed` (and `Unknown(2)` per kime's compat hack
                // for compositors that incorrectly send "repeated" here) as
                // a press; everything else as release.
                let pressed = matches!(
                    key_state,
                    WEnum::Value(wl_keyboard::KeyState::Pressed) | WEnum::Unknown(2)
                );
                tracing::trace!(key, pressed, "grab.Key");
                if pressed {
                    state.dispatch_key_press(time, key);
                } else {
                    state.dispatch_key_release(time, key);
                }
            }

            zwp_input_method_keyboard_grab_v2::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                tracing::trace!(
                    mods_depressed = format!("{:#x}", mods_depressed),
                    "grab.Modifiers"
                );
                state.handle_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }

            zwp_input_method_keyboard_grab_v2::Event::RepeatInfo { .. } => {
                // Key repeat not implemented yet
            }

            _ => {}
        }
    }
}
