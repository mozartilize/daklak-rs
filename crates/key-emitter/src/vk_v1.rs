//! `KeyEmitter` over `zwp_input_method_context_v1` (KWin / Mutter / IM v1 path).
//!
//! v1 carries a per-frame `serial` argument on every `key()` /
//! `modifiers()` call; the emitter captures it at construction time.
//! Sink constructs a fresh `VkV1Emitter` per `with_sink` so the serial
//! is always current.

use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_context_v1::ZwpInputMethodContextV1;

use crate::KeyEmitter;

pub struct VkV1Emitter<'a> {
    pub ctx: &'a ZwpInputMethodContextV1,
    pub serial: u32,
}

impl<'a> VkV1Emitter<'a> {
    pub fn new(ctx: &'a ZwpInputMethodContextV1, serial: u32) -> Self {
        Self { ctx, serial }
    }
}

impl KeyEmitter for VkV1Emitter<'_> {
    fn emit_key(&mut self, time: u32, keycode: u32, value: u32) {
        self.ctx.key(self.serial, time, keycode, value);
    }

    fn emit_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.ctx
            .modifiers(self.serial, depressed, latched, locked, group);
    }

    fn modifier_echo_through_grab(&self) -> bool {
        true
    }
}
