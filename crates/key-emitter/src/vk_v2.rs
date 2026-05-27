//! `KeyEmitter` over `zwp_virtual_keyboard_v1` (wlroots / IM v2 path).
//!
//! Constructed per-call in the sink. Borrows the live `vk` proxy off
//! `AdapterState`.

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

use crate::KeyEmitter;

pub struct VkV2Emitter<'a> {
    pub vk: &'a ZwpVirtualKeyboardV1,
}

impl<'a> VkV2Emitter<'a> {
    pub fn new(vk: &'a ZwpVirtualKeyboardV1) -> Self {
        Self { vk }
    }
}

impl KeyEmitter for VkV2Emitter<'_> {
    fn emit_key(&mut self, time: u32, keycode: u32, value: u32) {
        self.vk.key(time, keycode, value);
    }

    fn emit_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.vk.modifiers(depressed, latched, locked, group);
    }

    fn modifier_echo_through_grab(&self) -> bool {
        true
    }
}
