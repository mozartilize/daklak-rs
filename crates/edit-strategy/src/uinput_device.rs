use evdev::{
    AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, uinput::VirtualDevice,
};

/// Vendor/product IDs daklak claims for its uinput device. Picked from
/// the unallocated USB-vendor space so a higher-level remapper (keyd,
/// scroll, kanata, etc.) can exclude daklak's device by id:
///
/// ```text
/// # /etc/keyd/default.conf
/// [ids]
/// *
/// -dac1:ac01
/// ```
///
/// Bus is `BUS_USB`, not `BUS_VIRTUAL`. Sway/wlroots libinput inspects
/// the bus type when deciding whether a device's input feeds the
/// compositor's keybinding pipeline; `BUS_VIRTUAL` is treated as
/// "synthetic-only" and excluded from binding triggers, which silently
/// breaks Super+arrow / Super+w / ctrl+alt+t passthrough when daklak's
/// emits route through libinput. `BUS_USB` keeps daklak indistinguishable
/// from a normal keyboard for binding purposes. External remappers
/// (e.g. keyd) that need to filter daklak out should match on
/// vendor:product (`0xdac1:0xac01`).
pub const DAKLAK_UINPUT_VENDOR: u16 = 0xdac1;
pub const DAKLAK_UINPUT_PRODUCT: u16 = 0xac01;
pub const DAKLAK_UINPUT_VERSION: u16 = 0x0001;

/// Thin wrapper around an evdev `VirtualDevice` for the daklak synthetic
/// keyboard on `/dev/uinput`.
///
/// Registers a **full keyboard surface** — letters, digits, punctuation,
/// modifiers, arrows, F-keys, navigation. Required for Tier 5 pass-through:
/// when daklak's evdev grab swallows the user's physical keyboard, every
/// key (incl. modifier+arrow combos for compositor shortcuts) must
/// re-emerge through this uinput device so the compositor sees them as
/// a "real" keyboard input and fires its bindings. `zwp_virtual_keyboard_v1`
/// is insufficient — sway/wlroots blocks vk source from compositor
/// keybindings as a security measure (only kernel-level evdev sources
/// trigger them).
///
/// Requires `/dev/uinput` to be writable (udev rule or
/// `sudo chmod 666 /dev/uinput`).
pub struct UinputDevice {
    dev: VirtualDevice,
}

impl UinputDevice {
    /// Open or create the virtual device. Returns `Err(PermissionDenied)` if
    /// `/dev/uinput` is not accessible — daemon should demote to ForwardKey.
    pub fn open() -> std::io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        // Wide keyboard surface. Tier 5 pass-through needs all typing
        // keys + modifiers + nav/F-keys to route compositor shortcuts
        // (super+arrow, ctrl+alt+t, alt+tab, etc.) through the kernel
        // input layer so libinput-reading compositors fire bindings.
        let codes: &[KeyCode] = &[
            KeyCode::KEY_ESC,
            KeyCode::KEY_1, KeyCode::KEY_2, KeyCode::KEY_3, KeyCode::KEY_4,
            KeyCode::KEY_5, KeyCode::KEY_6, KeyCode::KEY_7, KeyCode::KEY_8,
            KeyCode::KEY_9, KeyCode::KEY_0,
            KeyCode::KEY_MINUS, KeyCode::KEY_EQUAL,
            KeyCode::KEY_BACKSPACE, KeyCode::KEY_TAB,
            KeyCode::KEY_Q, KeyCode::KEY_W, KeyCode::KEY_E, KeyCode::KEY_R,
            KeyCode::KEY_T, KeyCode::KEY_Y, KeyCode::KEY_U, KeyCode::KEY_I,
            KeyCode::KEY_O, KeyCode::KEY_P,
            KeyCode::KEY_LEFTBRACE, KeyCode::KEY_RIGHTBRACE, KeyCode::KEY_ENTER,
            KeyCode::KEY_LEFTCTRL, KeyCode::KEY_A, KeyCode::KEY_S, KeyCode::KEY_D,
            KeyCode::KEY_F, KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_J,
            KeyCode::KEY_K, KeyCode::KEY_L,
            KeyCode::KEY_SEMICOLON, KeyCode::KEY_APOSTROPHE, KeyCode::KEY_GRAVE,
            KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_BACKSLASH,
            KeyCode::KEY_Z, KeyCode::KEY_X, KeyCode::KEY_C, KeyCode::KEY_V,
            KeyCode::KEY_B, KeyCode::KEY_N, KeyCode::KEY_M,
            KeyCode::KEY_COMMA, KeyCode::KEY_DOT, KeyCode::KEY_SLASH,
            KeyCode::KEY_RIGHTSHIFT, KeyCode::KEY_KPASTERISK,
            KeyCode::KEY_LEFTALT, KeyCode::KEY_SPACE, KeyCode::KEY_CAPSLOCK,
            KeyCode::KEY_F1, KeyCode::KEY_F2, KeyCode::KEY_F3, KeyCode::KEY_F4,
            KeyCode::KEY_F5, KeyCode::KEY_F6, KeyCode::KEY_F7, KeyCode::KEY_F8,
            KeyCode::KEY_F9, KeyCode::KEY_F10, KeyCode::KEY_F11, KeyCode::KEY_F12,
            // F13-F19 host part of daklak's EIGHT_LEVEL Vietnamese slots.
            // They're also added below by the daklak_slot_keycodes()
            // loop; listing them here too keeps the cap bitmap explicit
            // (and lets `evtest` show recognizable names). F20/F21 are
            // intentionally absent — observed silently dropped on xfce4
            // X11 sessions, see crates/keymap SAFE_KEYCODES doc.
            KeyCode::KEY_F13, KeyCode::KEY_F14, KeyCode::KEY_F15, KeyCode::KEY_F16,
            KeyCode::KEY_F17, KeyCode::KEY_F18, KeyCode::KEY_F19,
            KeyCode::KEY_NUMLOCK, KeyCode::KEY_SCROLLLOCK,
            KeyCode::KEY_KP7, KeyCode::KEY_KP8, KeyCode::KEY_KP9, KeyCode::KEY_KPMINUS,
            KeyCode::KEY_KP4, KeyCode::KEY_KP5, KeyCode::KEY_KP6, KeyCode::KEY_KPPLUS,
            KeyCode::KEY_KP1, KeyCode::KEY_KP2, KeyCode::KEY_KP3,
            KeyCode::KEY_KP0, KeyCode::KEY_KPDOT,
            KeyCode::KEY_KPENTER, KeyCode::KEY_RIGHTCTRL, KeyCode::KEY_KPSLASH,
            KeyCode::KEY_SYSRQ, KeyCode::KEY_RIGHTALT,
            // KEY_HENKAN — daklak's ISO_Level5_Shift carrier for the
            // EIGHT_LEVEL custom slots. Pressed via uinput in evdev
            // mode before a slot keycode to address L5..L8.
            KeyCode::KEY_HENKAN,
            KeyCode::KEY_HOME, KeyCode::KEY_UP, KeyCode::KEY_PAGEUP,
            KeyCode::KEY_LEFT, KeyCode::KEY_RIGHT,
            KeyCode::KEY_END, KeyCode::KEY_DOWN, KeyCode::KEY_PAGEDOWN,
            KeyCode::KEY_INSERT, KeyCode::KEY_DELETE,
            KeyCode::KEY_PAUSE,
            KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA, KeyCode::KEY_COMPOSE,
            KeyCode::KEY_VOLUMEUP, KeyCode::KEY_VOLUMEDOWN, KeyCode::KEY_MUTE,
            KeyCode::KEY_BRIGHTNESSUP, KeyCode::KEY_BRIGHTNESSDOWN,
            KeyCode::KEY_PLAYPAUSE, KeyCode::KEY_NEXTSONG, KeyCode::KEY_PREVIOUSSONG,
            KeyCode::KEY_STOPCD,
        ];
        for k in codes {
            keys.insert(*k);
        }
        // Daklak synthetic Vietnamese slots. Kernel uinput driver silently
        // drops emits for codes not declared in the capability bitmap, so
        // we must UI_SET_KEYBIT each slot. The slot→keycode list is
        // non-contiguous: skips codes systemd-logind / brightness daemons
        // / display managers grab system-wide (e.g. KEY_SUSPEND=205,
        // KEY_BRIGHTNESSDOWN=224, KEY_SWITCHVIDEOMODE=227).
        for &code in viet_ime_keymap::daklak_slot_keycodes() {
            keys.insert(KeyCode::new(code));
        }
        let dev = VirtualDevice::builder()?
            .name("daklak")
            .input_id(InputId::new(
                BusType::BUS_USB,
                DAKLAK_UINPUT_VENDOR,
                DAKLAK_UINPUT_PRODUCT,
                DAKLAK_UINPUT_VERSION,
            ))
            .with_keys(&keys)?
            .build()?;
        Ok(Self { dev })
    }

    /// Emit a single key event (press or release) followed by SYN_REPORT.
    /// `value`: 1 = press, 0 = release.
    ///
    /// Mirrors ydotool's uinput_emit pattern
    /// (vendors/ydotool/Client/ydotool.c:96-112); `VirtualDevice::emit`
    /// handles the SYN_REPORT automatically.
    pub fn emit(&mut self, code: u16, value: i32) -> std::io::Result<()> {
        let ev = InputEvent::new(EventType::KEY.0, code, value);
        self.dev.emit(&[ev])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: open the uinput device and verify it appears in
    /// /proc/bus/input/devices. Requires user in `input` group or
    /// `sudo chmod 666 /dev/uinput`.
    #[test]
    #[ignore]
    fn uinput_device_opens() {
        let dev = UinputDevice::open().expect(
            "Could not open /dev/uinput. \
             Run: sudo chmod 666 /dev/uinput  OR  install res/99-daklak.rules",
        );
        drop(dev);
        let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            devices.contains("daklak"),
            "daklak device not found in /proc/bus/input/devices after open"
        );
    }
}
