//! Probe which evdev keycodes actually reach Wayland clients.
//!
//! Run alongside `wev` (or any Wayland event viewer) focused. Probe
//! emits each candidate keycode press/release with a brief delay and
//! prints the emitted code. Compare daklak's printed sequence against
//! wev's `sym:` lines — codes wev never reports were filtered by
//! libinput / scroll / wlroots before reaching clients.
//!
//! Build + run:
//!
//! ```sh
//! cargo build -p viet-ime-daemon --example probe_keycodes
//! # In one terminal:
//! wev
//! # Focus the wev window, then in another terminal:
//! ./target/debug/examples/probe_keycodes
//! ```
//!
//! Output to stderr matches each emit so you can cross-reference.
//! Compositor must already have daklak's xkb keymap applied to the
//! `0:0:viet-ime` device — easiest: start daklak first so the
//! `~/.config/sway/config.d/50-viet-ime.conf` snippet is in place,
//! then run this probe (it creates a *separate* uinput device so it
//! won't conflict with daklak's).
//!
//! Caveat: probe's uinput device has its own id (vendor 0:0). Apply
//! the daklak keymap to it manually if scroll only matched it to
//! `56001:44033`:
//!
//! ```sh
//! swaymsg input "0:0:daklak-probe" xkb_file "$XDG_RUNTIME_DIR/daklak/keymap.xkb"
//! ```

use std::thread;
use std::time::Duration;

use evdev::{
    uinput::VirtualDevice, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
};

const PROBE_NAME: &str = "daklak-probe";

fn main() -> std::io::Result<()> {
    // Candidate evdev codes: 128..255 sweep (covers all "consumer key"
    // territory below the X11 ceiling). Skip codes already proven
    // hostile so probe doesn't trigger hibernate / brightness:
    let candidates: Vec<u16> = (128..=255)
        .filter(|c| !matches!(c,
            116        // KEY_POWER
            | 142      // KEY_SLEEP
            | 143      // KEY_WAKEUP
            | 205      // KEY_SUSPEND
            | 224      // KEY_BRIGHTNESSDOWN
            | 225      // KEY_BRIGHTNESSUP
            | 227      // KEY_SWITCHVIDEOMODE
        ))
        .collect();

    let mut keys = AttributeSet::<KeyCode>::new();
    for &c in &candidates {
        keys.insert(KeyCode::new(c));
    }

    let mut dev = VirtualDevice::builder()?
        .name(PROBE_NAME)
        .input_id(InputId::new(BusType::BUS_USB, 0, 0, 1))
        .with_keys(&keys)?
        .build()?;

    eprintln!("probe: uinput device '{PROBE_NAME}' created. Sleeping 2s — focus wev now.");
    thread::sleep(Duration::from_secs(2));

    for code in candidates {
        eprintln!("probe: emit code={code}");
        let press = InputEvent::new(EventType::KEY.0, code, 1);
        let rel = InputEvent::new(EventType::KEY.0, code, 0);
        dev.emit(&[press])?;
        thread::sleep(Duration::from_millis(40));
        dev.emit(&[rel])?;
        thread::sleep(Duration::from_millis(80));
    }

    eprintln!("probe: done. Compare daklak's emitted codes against wev's `sym:` lines.");
    Ok(())
}
