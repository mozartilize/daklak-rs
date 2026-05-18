use wayland_client::{
    delegate_noop, event_created_child,
    globals::GlobalListContents,
    protocol::{
        wl_keyboard,
        wl_registry, wl_seat,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
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
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use crate::focus::wlr::ToplevelEntry;
use crate::focus::FocusEvent;
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

// ── wlr-foreign-toplevel-management dispatch ─────────────────────────────────

impl<H: AdapterHandler> Dispatch<ZwlrForeignToplevelManagerV1, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        _manager: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                tracing::trace!(id = ?toplevel.id(), "ftl: new toplevel");
                state
                    .state
                    .toplevels
                    .insert(toplevel.id(), ToplevelEntry::default());
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                tracing::debug!("ftl: manager finished");
                state.state.toplevels.clear();
                state.state.active_toplevel = None;
                state.state.ftl_manager = None;
            }
            _ => {}
        }
    }

    event_created_child!(WaylandAdapter<H>, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl<H: AdapterHandler> Dispatch<ZwlrForeignToplevelHandleV1, ()> for WaylandAdapter<H> {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(e) = state.state.toplevels.get_mut(&id) {
                    e.pending_app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(e) = state.state.toplevels.get_mut(&id) {
                    e.pending_title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => {
                // `state` is array of u32 LE bytes; each u32 is a State enum
                // variant. ACTIVATED variant == 2 per wlr-protocols XML.
                let activated = bytes
                    .chunks_exact(4)
                    .any(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) == 2);
                if let Some(e) = state.state.toplevels.get_mut(&id) {
                    e.pending_activated = Some(activated);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                let entry_changed = state
                    .state
                    .toplevels
                    .get_mut(&id)
                    .map(|e| e.commit())
                    .unwrap_or(false);
                let new_active = state
                    .state
                    .toplevels
                    .iter()
                    .find(|(_, e)| e.activated)
                    .map(|(k, _)| k.clone());
                let active_changed = new_active != state.state.active_toplevel;
                if entry_changed || active_changed {
                    state.state.active_toplevel = new_active.clone();
                    let ev = match new_active.as_ref().and_then(|k| state.state.toplevels.get(k)) {
                        Some(e) => {
                            let app_id = e
                                .app_id
                                .as_ref()
                                .filter(|s| !s.trim().is_empty())
                                .cloned();
                            let title = e.title.as_deref();
                            let is_xwayland = state
                                .state
                                .x11_bridge
                                .as_ref()
                                .map(|b| b.matches(app_id.as_deref(), title))
                                .unwrap_or(false);
                            FocusEvent { app_id, is_xwayland }
                        }
                        None => FocusEvent::default(),
                    };
                    if let Ok(mut g) = state.state.focus_current.lock() {
                        *g = Some(ev.clone());
                    }
                    if let Some(tx) = state.state.focus_tx.as_ref() {
                        let _ = tx.send(ev);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.state.toplevels.remove(&id);
                if state.state.active_toplevel.as_ref() == Some(&id) {
                    state.state.active_toplevel = None;
                    let ev = FocusEvent::default();
                    if let Ok(mut g) = state.state.focus_current.lock() {
                        *g = Some(ev.clone());
                    }
                    if let Some(tx) = state.state.focus_tx.as_ref() {
                        let _ = tx.send(ev);
                    }
                }
                handle.destroy();
            }
            _ => {}
        }
    }
}
