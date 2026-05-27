//! `KeyEmitter` over `/dev/uinput`.
//!
//! Owns a `viet_ime_edit_strategy::uinput_device::UinputDevice` and emits
//! kernel input events; the focused client sees them via
//! libinput → compositor → wl_keyboard.
//!
//! Uinput events on the wayland-adapter Tier 3 path DO round-trip through
//! the IME's own `zwp_input_method_keyboard_grab_v2`. That suppression
//! queue currently lives at the call site
//! (`AdapterState::pending_self_emits`); this emitter only owns the
//! device. (Phase B may move the queue ownership in here.)
//!
//! `time` is ignored — uinput events carry a kernel-stamped timestamp
//! from `evdev`. `value` is interpreted directly:
//!
//! - 0 = release
//! - 1 = press
//! - 2 = autorepeat
//!
//! Modifier echo: uinput has no `modifiers(...)` channel. The kernel
//! tracks modifier state from key events alone. `emit_modifiers` is a
//! no-op here.

use anyhow::Result;
use viet_ime_edit_strategy::uinput_device::UinputDevice;

use crate::KeyEmitter;

pub struct UinputEmitter {
    pub dev: UinputDevice,
}

impl UinputEmitter {
    pub fn open() -> Result<Self> {
        let dev = UinputDevice::open()
            .map_err(|e| anyhow::anyhow!("uinput: open /dev/uinput failed: {e}"))?;
        Ok(Self { dev })
    }

    pub fn from_device(dev: UinputDevice) -> Self {
        Self { dev }
    }

    /// Raw emit ignoring the `value: u32` widening. Useful for callers
    /// (Tier 3 backspace) that already speak `i32` press/release and want
    /// to handle errors themselves.
    pub fn raw_emit(&mut self, code: u16, value: i32) -> std::io::Result<()> {
        self.dev.emit(code, value)
    }
}

impl KeyEmitter for UinputEmitter {
    fn emit_key(&mut self, _time: u32, keycode: u32, value: u32) {
        let code = match u16::try_from(keycode) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!(keycode, "uinput emit: keycode out of u16 range, dropping");
                return;
            }
        };
        // u32 → i32 narrowing is safe for {0,1,2}.
        if let Err(e) = self.dev.emit(code, value as i32) {
            tracing::warn!(?e, keycode, value, "uinput emit failed");
        }
    }

    fn emit_modifiers(
        &mut self,
        _depressed: u32,
        _latched: u32,
        _locked: u32,
        _group: u32,
    ) {
        // Uinput has no modifier echo channel; modifier state is derived
        // by the kernel from press/release events on KEY_LEFTSHIFT etc.
    }
}
