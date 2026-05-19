//! Output backend abstraction for evdev-only mode.
//!
//! The evdev adapter receives raw keystrokes (grabbed from
//! `/dev/input/event*`), runs them through the daklak engine, then
//! needs to deliver the engine's output (raw pass-through or Vietnamese
//! commits) back to the focused Wayland client. *How* that delivery
//! happens is the only thing that varies by backend.
//!
//! **Current — UinputBackend**: creates `/dev/uinput` device, writes
//! daklak's synthetic xkb keymap to `$XDG_RUNTIME_DIR/daklak/keymap.xkb`,
//! tells the compositor (sway/scroll today) to load the keymap on the
//! uinput device via a `~/.config/sway/config.d/*.conf` snippet. Works
//! on every wlroots compositor + KDE if user installs the snippet.
//!
//! **Future — LibeiBackend**: blocked on
//! `xdg-desktop-portal-wlr` issue #323 (RemoteDesktop portal for
//! wlroots). Once landed, libei delivers keysyms / typed text to the
//! compositor portal directly. No uinput device. No xkb keymap upload.
//! No kernel-level keycode landmines. See the libei subsection below
//! for the planned skeleton.
//!
//! Trait surface is intentionally minimal — backends emit single key
//! events; SYN/framing is each backend's concern. Modifier-dance logic
//! (Shift / AltGr around Vietnamese precomposed slots) stays in the
//! evdev adapter because it's tied to the daklak synthetic keymap
//! layout, not the backend.

use anyhow::Result;

/// Sink for key events the evdev adapter wants to deliver to clients.
///
/// Implementations route events to a Wayland compositor by whatever
/// mechanism they support (uinput device today, libei session
/// tomorrow). All operate on **evdev-compatible keycodes** the daklak
/// engine produces:
///
/// - 1..127: standard pc/us keyboard codes.
/// - `viet_ime_keymap::SAFE_KEYCODES[..]`: Vietnamese precomposed
///   slots, resolved against daklak's custom xkb keymap.
pub trait OutputBackend: Send {
    /// Emit a single key event. `value`: 1 = press, 0 = release,
    /// 2 = autorepeat. Backend handles SYN_REPORT / device framing
    /// internally.
    fn emit_key(&mut self, code: u16, value: i32) -> Result<()>;
}

// ─── UinputBackend ────────────────────────────────────────────────────

use viet_ime_edit_strategy::uinput_device::UinputDevice;

/// Wraps daklak's `viet-ime` uinput device. The compositor-side
/// keymap (`daklak gen-keymap`) is set up out-of-band by the user
/// or their systemd unit — see `docs/evdev-only-setup.md`.
pub struct UinputBackend {
    dev: UinputDevice,
}

impl UinputBackend {
    pub fn open() -> Result<Self> {
        let dev = UinputDevice::open()
            .map_err(|e| anyhow::anyhow!("uinput: open /dev/uinput failed: {e}"))?;
        Ok(Self { dev })
    }
}

impl OutputBackend for UinputBackend {
    fn emit_key(&mut self, code: u16, value: i32) -> Result<()> {
        self.dev
            .emit(code, value)
            .map_err(|e| anyhow::anyhow!("uinput emit code={code} value={value}: {e}"))
    }
}

// ─── LibeiBackend (future) ────────────────────────────────────────────
//
// pub struct LibeiBackend {
//     // ei_handle: reis::Context,
//     // device: reis::ei::keyboard::Keyboard,
//     // serial: u32,
// }
//
// impl LibeiBackend {
//     pub fn open() -> Result<Self> {
//         // 1. ashpd / xdg-desktop-portal-rs: create RemoteDesktop session.
//         // 2. Request keyboard device via SelectDevices.
//         // 3. Call Start() — portal returns an ei socket fd.
//         // 4. reis::Context::new() over that fd; bind ei_keyboard interface.
//         // 5. Upload daklak's synthetic keymap via ei_keyboard_keymap
//         //    (same memfd-backed keymap_text() the wayland adapter uses
//         //    for zwp_virtual_keyboard_v1).
//         //
//         // Blocked on wlroots libeis support
//         // (xdg-desktop-portal-wlr#323). Mutter, niri ready today; KWin
//         // partial.
//         anyhow::bail!("libei backend not implemented — waiting on wlroots libeis support")
//     }
// }
//
// impl OutputBackend for LibeiBackend {
//     fn emit_key(&mut self, code: u16, value: i32) -> Result<()> {
//         // ei_keyboard.key(time, code, state)  +  ei_device.frame(time)
//         // (libei is the Wayland-protocol-shaped equivalent of vk_key + frame)
//         todo!("ei_keyboard_key + ei_device_frame")
//     }
// }
