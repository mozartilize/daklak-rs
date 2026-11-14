use wayland_client::{
    delegate_noop,
    globals::GlobalListContents,
    protocol::{wl_keyboard, wl_registry, wl_seat},
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

// Focus dispatch lives alongside its protocol drivers in focus/.
delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpInputMethodManagerV2);
delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(@<H: AdapterHandler> WaylandAdapter<H>: ignore ZwpVirtualKeyboardV1);

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

impl<H: AdapterHandler> Dispatch<wl_seat::WlSeat, ()> for WaylandAdapter<H> {
    fn event(
        _state: &mut Self,
        _seat: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl<H: AdapterHandler> Dispatch<ZwpInputMethodV2, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        _im: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use crate::frame::FrameEvent;
        match event {
            zwp_input_method_v2::Event::Activate => {
                state.state.apply_event(FrameEvent::Activate);
            }
            zwp_input_method_v2::Event::Deactivate => {
                state.state.client_repeat.cancel();
                state.state.held_forwarded.clear();
                state.state.apply_event(FrameEvent::Deactivate);
            }
            zwp_input_method_v2::Event::SurroundingText { text, cursor, anchor } => {
                state.state.apply_event(FrameEvent::SurroundingText { text, cursor, anchor });
            }
            zwp_input_method_v2::Event::TextChangeCause { cause } => {
                state.state.apply_event(FrameEvent::ChangeCause(cause.into()));
            }
            zwp_input_method_v2::Event::ContentType { hint: _, purpose } => {
                state.state.apply_event(FrameEvent::Purpose(purpose.into()));
            }
            zwp_input_method_v2::Event::Done => {
                tracing::trace!("im_v2: Done");
                state.state.serial = state.state.serial.wrapping_add(1);
                state.apply_done_frame();
            }
            zwp_input_method_v2::Event::Unavailable => {
                state.state.apply_event(FrameEvent::Unavailable);
            }
            _ => {}
        }
    }
}

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
                state.state.wire.on_keymap(format.into(), fd.as_fd(), size);
                // Decode physical input with the compositor keymap, independently
                // of the synthetic keymap uploaded to the virtual keyboard.
                if state.state.xkb.is_none() {
                    match viet_ime_keymap::xkb::XkbState::from_fd(fd, size) {
                        Ok(xkb) => {
                            state.state.xkb = Some(xkb);
                            tracing::debug!("xkb state initialized");
                        }
                        Err(e) => tracing::error!("xkb init failed: {e}"),
                    }
                }
            }
            zwp_input_method_keyboard_grab_v2::Event::Key { time, key, state: key_state, .. } => {
                tracing::trace!(key, ?key_state, "grab.Key");
                match key_state {
                    WEnum::Value(wl_keyboard::KeyState::Pressed) => state.dispatch_key_press(time, key),
                    // Older protocol definitions surface server-side repeats as Unknown(2).
                    WEnum::Value(wl_keyboard::KeyState::Repeated) | WEnum::Unknown(2) => {
                        state.dispatch_key_repeat(time, key)
                    }
                    _ => state.dispatch_key_release(time, key),
                }
            }
            zwp_input_method_keyboard_grab_v2::Event::Modifiers {
                mods_depressed, mods_latched, mods_locked, group, ..
            } => {
                tracing::trace!(mods_depressed = format!("{:#x}", mods_depressed), "grab.Modifiers");
                state.handle_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            zwp_input_method_keyboard_grab_v2::Event::RepeatInfo { rate, delay } => {
                tracing::debug!(rate, delay, "grab.RepeatInfo");
                state.state.client_repeat.set_info(rate, delay);
            }
            _ => {}
        }
    }
}
