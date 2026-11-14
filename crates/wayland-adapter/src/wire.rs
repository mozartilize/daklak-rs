//! The negotiated Wayland connection. It owns every protocol object and is the
//! single source of capability facts; nothing else records what was negotiated.

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;

use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{bail, Context, Result};
use viet_ime_key_emitter::KeyEmitter;
use wayland_client::{globals::GlobalList, protocol::{wl_keyboard::WlKeyboard, wl_seat::WlSeat}, Connection, Proxy, QueueHandle};
use wayland_protocols::wp::input_method::zv1::client::{
    zwp_input_method_context_v1::ZwpInputMethodContextV1,
    zwp_input_method_v1::ZwpInputMethodV1,
};
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

use crate::{keymap, sink::{KeyOutput, TextOpsTarget}, AdapterHandler, ImProtocol, WaylandAdapter};

/// What the compositor offers. Read before binding: binding an input method
/// already registers Daklak as one.
#[derive(Debug, Clone, Copy, Default)]
struct Advertised {
    im_v2: bool,
    im_v1: bool,
    vk: bool,
}

impl Advertised {
    fn from_globals(globals: &GlobalList) -> Self {
        globals.contents().with_list(|list| {
            let has = |interface| list.iter().any(|g| g.interface == interface);
            Self {
                im_v2: has("zwp_input_method_manager_v2"),
                im_v1: has("zwp_input_method_v1"),
                vk: has("zwp_virtual_keyboard_manager_v1"),
            }
        })
    }

    /// IMv2 grabs keys but cannot hand them back, so it needs the virtual
    /// keyboard. IMv1 relays keys through its own context.
    fn protocol(self) -> Result<ImProtocol> {
        if self.im_v2 && self.vk {
            Ok(ImProtocol::ImV2)
        } else if self.im_v1 {
            Ok(ImProtocol::ImV1)
        } else if self.im_v2 {
            bail!("IMv2 requires zwp_virtual_keyboard_manager_v1 for raw-key passthrough; no IMv1 fallback available")
        } else {
            bail!("neither zwp_input_method_v2 nor zwp_input_method_v1 exposed by compositor")
        }
    }
}

pub(crate) struct Wire {
    conn: Connection,
    kind: WireKind,
}

enum WireKind {
    V2 {
        im: ZwpInputMethodV2,
        grab: ZwpInputMethodKeyboardGrabV2,
        vk: VirtualKeyboard,
        acked: bool,
    },
    V1 {
        _im: ZwpInputMethodV1,
        active: Option<V1Session>,
        vk: Option<VirtualKeyboard>,
    },
}

struct V1Session {
    ctx: ZwpInputMethodContextV1,
    keyboard: WlKeyboard,
    pending_commit: bool,
}

impl Drop for V1Session {
    fn drop(&mut self) {
        // Old wl_keyboard versions have no release request.
        if self.keyboard.version() >= 3 {
            self.keyboard.release();
        }
        self.ctx.destroy();
    }
}

struct VirtualKeyboard {
    proxy: ZwpVirtualKeyboardV1,
    synthetic_keymap: bool,
    keymap_ready: bool,
}

impl VirtualKeyboard {
    fn new(proxy: ZwpVirtualKeyboardV1) -> Self {
        let synthetic_keymap = match keymap::build() {
            Ok(km) => {
                proxy.keymap(1, km.fd.as_fd(), km.size);
                tracing::info!(size = km.size, "vk.keymap → daklak synthetic keymap");
                true
            }
            Err(error) => {
                tracing::warn!(%error, "synthetic keymap unavailable; awaiting compositor keymap");
                false
            }
        };
        Self { proxy, synthetic_keymap, keymap_ready: synthetic_keymap }
    }

    fn output(&self) -> KeyOutput<'_> {
        KeyOutput::VirtualKeyboard { vk: &self.proxy, synthetic_keymap: self.synthetic_keymap }
    }
}

impl Wire {
    pub fn connect<H: AdapterHandler>(globals: &GlobalList, qh: &QueueHandle<WaylandAdapter<H>>, conn: Connection) -> Result<Self> {
        let protocol = Advertised::from_globals(globals).protocol()?;
        let seat = globals.bind::<WlSeat, _, _>(qh, 1..=8, ()).context("bind wl_seat")?;
        let kind = match protocol {
            ImProtocol::ImV2 => {
                // Bind the output path before taking the keyboard grab.
                let manager = globals.bind::<ZwpVirtualKeyboardManagerV1, _, _>(qh, 1..=1, ())
                    .context("bind zwp_virtual_keyboard_manager_v1")?;
                let vk = VirtualKeyboard::new(manager.create_virtual_keyboard(&seat, qh, ()));
                let manager = globals.bind::<ZwpInputMethodManagerV2, _, _>(qh, 1..=1, ())
                    .context("bind zwp_input_method_manager_v2")?;
                let im = manager.get_input_method(&seat, qh, ());
                let grab = im.grab_keyboard(qh, ());
                WireKind::V2 { im, grab, vk, acked: false }
            }
            ImProtocol::ImV1 => {
                let im = globals.bind::<ZwpInputMethodV1, _, _>(qh, 1..=1, ())
                    .context("bind zwp_input_method_v1")?;
                // shortcut: a virtual keyboard next to IMv1 is supported by the
                // key routing but never bound; enable once a compositor is
                // known to keep its keys out of the IMv1 grab.
                WireKind::V1 { _im: im, active: None, vk: None }
            }
        };
        Ok(Self { conn, kind })
    }

    /// Replacement text can be typed as keysyms on the IMv1 context.
    pub fn has_keysym_commit(&self) -> bool {
        matches!(self.kind, WireKind::V1 { vk: None, .. })
    }

    /// A session can be synthesized from focus alone: the grab must exist
    /// outside a text-input context and Vietnamese must be typeable as keys.
    pub fn supports_focus_session(&self) -> bool {
        matches!(&self.kind, WireKind::V2 { vk, .. } if vk.synthetic_keymap)
    }

    pub fn outputs(&mut self, serial: u32) -> Option<(&Connection, TextOpsTarget<'_>, KeyOutput<'_>)> {
        let (text, keys) = match &mut self.kind {
            WireKind::V2 { im, vk, acked, .. } => (
                TextOpsTarget::V2 { im, acked }, vk.output(),
            ),
            WireKind::V1 { active, vk, .. } => {
                let session = active.as_ref()?;
                let keys = vk.as_ref().map(VirtualKeyboard::output)
                    .unwrap_or(KeyOutput::V1Context { ctx: &session.ctx, serial });
                (TextOpsTarget::V1 { ctx: &session.ctx, serial }, keys)
            }
        };
        Some((&self.conn, text, keys))
    }

    fn key_output(&self, serial: u32) -> Option<KeyOutput<'_>> {
        match &self.kind {
            WireKind::V2 { vk, .. } => Some(vk.output()),
            WireKind::V1 { vk: Some(vk), .. } => Some(vk.output()),
            WireKind::V1 { active, .. } => active.as_ref().map(|s| KeyOutput::V1Context { ctx: &s.ctx, serial }),
        }
    }

    pub fn forward_key(&self, serial: u32, time: u32, key: u32, value: u32) {
        if let Some(mut keys) = self.key_output(serial) {
            keys.emit_key(time, key, value);
        }
    }

    pub fn forward_modifiers(&self, serial: u32, mods: (u32, u32, u32, u32)) {
        if let Some(mut keys) = self.key_output(serial) {
            keys.emit_modifiers(mods.0, mods.1, mods.2, mods.3);
        }
    }

    pub fn on_keymap(&mut self, format: u32, fd: BorrowedFd<'_>, size: u32) {
        let vk = match &mut self.kind {
            WireKind::V2 { vk, .. } => Some(vk),
            WireKind::V1 { vk, .. } => vk.as_mut(),
        };
        if let Some(vk) = vk.filter(|vk| !vk.keymap_ready) {
            vk.proxy.keymap(format, fd, size);
            vk.keymap_ready = true;
        }
    }

    pub fn begin_frame(&mut self) {
        if let WireKind::V2 { acked, .. } = &mut self.kind {
            *acked = false;
        }
    }

    pub fn ack_frame(&mut self, serial: u32) {
        if let WireKind::V2 { im, acked, .. } = &mut self.kind {
            if !*acked {
                im.commit(serial);
                *acked = true;
            }
        }
    }

    pub fn activate_v1<H: AdapterHandler>(&mut self, ctx: ZwpInputMethodContextV1, qh: &QueueHandle<WaylandAdapter<H>>) {
        if let WireKind::V1 { active, .. } = &mut self.kind {
            active.take();
            let keyboard = ctx.grab_keyboard(qh, ());
            // KWin requires the final NUL as well as the separators.
            ctx.modifiers_map(b"Shift\0Control\0Mod1\0Mod4\0".to_vec());
            *active = Some(V1Session { ctx, keyboard, pending_commit: false });
        }
    }

    pub fn is_active_context(&self, ctx: &ZwpInputMethodContextV1) -> bool {
        matches!(&self.kind, WireKind::V1 { active: Some(s), .. } if s.ctx == *ctx)
    }

    pub fn is_active_keyboard(&self, keyboard: &WlKeyboard) -> bool {
        matches!(&self.kind, WireKind::V1 { active: Some(s), .. } if s.keyboard == *keyboard)
    }

    pub fn deactivate_v1(&mut self, ctx: &ZwpInputMethodContextV1) -> bool {
        if !self.is_active_context(ctx) {
            return false;
        }
        if let WireKind::V1 { active, .. } = &mut self.kind {
            active.take();
        }
        true
    }

    pub fn mark_pending_commit(&mut self, pending: bool) {
        if let WireKind::V1 { active: Some(s), .. } = &mut self.kind {
            s.pending_commit = pending;
        }
    }

    pub fn take_pending_commit(&mut self) -> bool {
        match &mut self.kind {
            WireKind::V1 { active: Some(s), .. } => std::mem::take(&mut s.pending_commit),
            _ => false,
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        match &mut self.kind {
            WireKind::V2 { grab, im, vk, .. } => {
                grab.release();
                im.destroy();
                vk.proxy.destroy();
            }
            WireKind::V1 { active, vk, .. } => {
                active.take();
                if let Some(vk) = vk {
                    vk.proxy.destroy();
                }
            }
        }
        // Hand the keyboard back now, not whenever the socket closes.
        if let Err(error) = self.conn.flush() {
            tracing::debug!(%error, "flush Wayland teardown requests failed");
        }
    }
}

impl std::fmt::Debug for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (im, vk) = match &self.kind {
            WireKind::V2 { vk, .. } => ("IMv2", Some(vk)),
            WireKind::V1 { vk, .. } => ("IMv1", vk.as_ref()),
        };
        let keys = match vk {
            Some(vk) if vk.synthetic_keymap => "virtual keyboard, synthetic keymap",
            Some(_) => "virtual keyboard, compositor keymap",
            None => "input-method context",
        };
        write!(f, "{im}, keys via {keys}")
    }
}

/// A real `Wire` over a socket pair, for tests that have no compositor.
/// Requests are written to the peer; no events ever arrive.
#[cfg(any(test, feature = "test-util"))]
pub struct TestPeer {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) socket: std::os::unix::net::UnixStream,
    registry: wayland_client::protocol::wl_registry::WlRegistry,
    qh: QueueHandle<NoEvents>,
    _queue: wayland_client::EventQueue<NoEvents>,
}

#[cfg(any(test, feature = "test-util"))]
struct NoEvents;

#[cfg(any(test, feature = "test-util"))]
impl<I: Proxy + 'static> wayland_client::Dispatch<I, ()> for NoEvents {
    fn event(_: &mut Self, _: &I, _: I::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

#[cfg(any(test, feature = "test-util"))]
impl TestPeer {
    pub(crate) fn bind<I: Proxy + 'static>(&self) -> I {
        self.registry.bind::<I, _, _>(1, 1, &self.qh, ())
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Wire {
    pub(crate) fn for_test(protocol: ImProtocol) -> (Self, TestPeer) {
        let (client, socket) = std::os::unix::net::UnixStream::pair()
            .unwrap_or_else(|error| panic!("test socket pair: {error}"));
        let conn = Connection::from_socket(client)
            .unwrap_or_else(|error| panic!("test Wayland connection: {error}"));
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let peer = TestPeer { socket, registry, qh, _queue: queue };
        let kind = match protocol {
            ImProtocol::ImV2 => {
                let im = peer.bind::<ZwpInputMethodV2>();
                let grab = im.grab_keyboard(&peer.qh, ());
                WireKind::V2 { im, grab, vk: peer.test_vk(), acked: false }
            }
            ImProtocol::ImV1 => WireKind::V1 { _im: peer.bind(), active: None, vk: None },
        };
        (Self { conn, kind }, peer)
    }
}

#[cfg(any(test, feature = "test-util"))]
impl TestPeer {
    fn test_vk(&self) -> VirtualKeyboard {
        VirtualKeyboard { proxy: self.bind(), synthetic_keymap: true, keymap_ready: true }
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;

    #[test]
    fn negotiation_requires_a_complete_keyboard_path() {
        // Hand-written matrix: advertising IMv2 must not hide a usable IMv1
        // when the virtual keyboard is absent, and VK alone receives nothing.
        let cases = [
            ((false, false, false), None),
            ((false, false, true), None),
            ((true, false, false), None),
            ((true, false, true), Some(ImProtocol::ImV2)),
            ((false, true, false), Some(ImProtocol::ImV1)),
            ((false, true, true), Some(ImProtocol::ImV1)),
            ((true, true, false), Some(ImProtocol::ImV1)),
            ((true, true, true), Some(ImProtocol::ImV2)),
        ];
        for ((im_v2, im_v1, vk), expected) in cases {
            assert_eq!(Advertised { im_v2, im_v1, vk }.protocol().ok(), expected,
                "im_v2={im_v2}, im_v1={im_v1}, vk={vk}");
        }
    }

    #[test]
    fn imv2_only_error_explains_missing_passthrough() {
        let error = Advertised { im_v2: true, ..Default::default() }.protocol().unwrap_err().to_string();
        assert!(error.contains("raw-key passthrough"));
        assert!(error.contains("zwp_virtual_keyboard_manager_v1"));
    }
}
