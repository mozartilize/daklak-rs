use evdev::{AttributeSet, EventType, InputEvent, KeyCode, uinput::VirtualDevice};

/// Thin wrapper around an evdev `VirtualDevice` for the viet-ime synthetic
/// keyboard on `/dev/uinput`.
///
/// Registers only the keys we actually send: Backspace and modifiers that
/// may be temporarily released to avoid the Shift+Backspace conflict
/// (plan2.md "Tier 3 modifier conflict").
///
/// Requires `/dev/uinput` to be writable. See plan2.md "Local development"
/// for setup (udev rule or `sudo chmod 666 /dev/uinput`).
pub struct UinputDevice {
    dev: VirtualDevice,
}

impl UinputDevice {
    /// Open or create the virtual device. Returns `Err(PermissionDenied)` if
    /// `/dev/uinput` is not accessible — daemon should demote to ForwardKey.
    pub fn open() -> std::io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in [
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_LEFTMETA,
            KeyCode::KEY_RIGHTMETA,
        ] {
            keys.insert(k);
        }
        let dev = VirtualDevice::builder()?
            .name("viet-ime")
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
    /// `sudo chmod 666 /dev/uinput` — see plan2.md for setup.
    #[test]
    #[ignore]
    fn uinput_device_opens() {
        let dev = UinputDevice::open().expect(
            "Could not open /dev/uinput. \
             Run: sudo chmod 666 /dev/uinput  OR  install res/99-viet-ime.rules",
        );
        drop(dev);
        let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            devices.contains("viet-ime"),
            "viet-ime device not found in /proc/bus/input/devices after open"
        );
    }
}
