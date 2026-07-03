//! Evdev-only input adapter.
//!
//! Grabs every classified-as-keyboard `/dev/input/event*`, drives the
//! engine from raw keycodes (decoded against a daemon-owned "us" xkb
//! state), and emits via daklak's own uinput device. Pure pass-through
//! when the engine does not consume a key.
//!
//! Active when `enable_wayland = false && enable_evdev_grab = true`.
//! See `docs/evdev-only-setup.md` for the matching `daklak_vn` xkb
//! layout that makes Vietnamese precomposed chars emittable.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use evdev::{Device, EventStream, EventType, InputEvent, KeyCode};
use futures_util::{stream::SelectAll, StreamExt};
use tokio::signal;
use xkbcommon::xkb;

use viet_ime_edit_strategy::KeyDecision;
use viet_ime_keymap as keymap;
use viet_ime_keymap::xkb::XkbState;

use viet_ime_key_emitter::{KeyEmitter, UinputEmitter};

/// Trait the daemon implements so the evdev adapter can drive composition
/// without depending on the daemon's concrete `Daemon` struct.
///
/// Mirrors `viet-ime-wayland-adapter`'s `AdapterHandler` shape but trimmed
/// to what the evdev path actually needs.
pub trait EvdevHandler {
    fn handle_char(&mut self, code: u32, ch: char) -> KeyDecision;
    fn handle_backspace(&mut self) -> KeyDecision;
    /// Reset all session state on emergency-Esc.
    fn clear_session(&mut self);
    /// Clear daemon-side last-input-char memory.
    fn clear_last_input_char(&mut self);
    /// Full window reset (composition + shadow). No-op when no window.
    fn full_reset_window(&mut self);
    /// Idle-reset timer check on focused window. No-op when no window.
    fn check_idle_reset_window(&mut self);
}

const DAKLAK_NAME: &str = "daklak";
const KEY_BACKSPACE: u32 = 14;
const KEY_ESC: u32 = 1;

const MOD_LEFTCTRL: u32 = 29;
const MOD_LEFTSHIFT: u32 = 42;
const MOD_RIGHTSHIFT: u32 = 54;
const MOD_LEFTALT: u32 = 56;
const MOD_RIGHTCTRL: u32 = 97;
const MOD_RIGHTALT: u32 = 100;
// KEY_HENKAN — daklak's ISO_Level5_Shift carrier in the custom keymap.
// Pressed physically through uinput to address EIGHT_LEVEL slots L5..L8.
// See viet_ime_keymap::LEVEL5_SHIFT_EVDEV.
const MOD_HENKAN: u32 = 92;
const MOD_LEFTMETA: u32 = 125;
const MOD_RIGHTMETA: u32 = 127;

// xkbcommon mod indices for the default evdev rules.
const XKB_SHIFT_BIT: u32 = 1 << 0;
const XKB_CONTROL_BIT: u32 = 1 << 2;
const XKB_MOD1_BIT: u32 = 1 << 3; // Alt
const XKB_MOD3_BIT: u32 = 1 << 5; // LevelFive (daklak's HENK binding)
const XKB_MOD4_BIT: u32 = 1 << 6; // Super
const XKB_MOD5_BIT: u32 = 1 << 7; // AltGr / Level3

/// SAFETY: xkbcommon is thread-safe (uses internal mutexes). All other
/// fields (SelectAll<EventStream>, Box<dyn KeyEmitter + Send>, primitives)
/// are Send. The raw pointers inside XkbState are usable from any thread.
unsafe impl Send for EvdevAdapter {}

pub struct EvdevAdapter {
    streams: SelectAll<EventStream>,
    /// Output delivery sink (uinput today).
    /// Owns its own lifecycle — keymap activation, device teardown.
    output: Box<dyn KeyEmitter + Send>,
    xkb: XkbState,
    mod_mask: u32,
    /// Modifier mask currently visible to the compositor. Tracks
    /// Shift/AltGr only (the bits emit_string adjusts for FOUR_LEVEL
    /// keymap navigation). Diverges from mod_mask transiently during
    /// emit_string's mod dance.
    emitted_mods: u32,
    /// Physical evdev keycodes the user currently has depressed
    /// (`value=1` seen, `value=0` not yet). Tail-drop fix:
    /// when emit_string is about to press a key the user is still
    /// holding, libxkbcommon (in the focused client) treats the
    /// second press as a no-op duplicate. We emit a prelude release
    /// to clear that state. Same fix as `keymap::emit_char`'s
    /// `held_user_kc` path on the wayland side.
    held_physical: HashSet<u32>,
    /// Physical keycodes currently held whose *press* was forwarded raw
    /// (ForwardRaw / shortcut / non-printable). Only these have their
    /// release and autorepeat forwarded. A key the engine consumed
    /// (Consumed / Apply) leaves no mark, so its continuation events are
    /// dropped instead of reaching the client as dangling press-less
    /// releases — which, through the live IBus passthrough path, can strand
    /// the client in an auto-repeat.
    forwarded_press: HashSet<u32>,
    escape_taps: Vec<Instant>,
    should_exit: bool,
}

fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .map(|keys| keys.contains(KeyCode::KEY_A))
        .unwrap_or(false)
}

fn scan_keyboards() -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    for (path, dev) in evdev::enumerate() {
        if !is_keyboard(&dev) {
            continue;
        }
        let name = dev.name().unwrap_or("(unnamed)").to_owned();
        if name == DAKLAK_NAME {
            tracing::debug!(path = %path.display(), "evdev: skipping own uinput");
            continue;
        }
        tracing::info!(path = %path.display(), name, "evdev: keyboard discovered");
        found.push((path.clone(), name));
    }
    if found.is_empty() {
        tracing::warn!("evdev: no keyboards found — check `input` group membership");
    }
    found
}

fn build_us_xkb() -> Result<XkbState> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "pc105",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("xkb: failed to compile us keymap")?;
    Ok(XkbState::from_keymap(keymap))
}

fn mod_bit(code: u32) -> Option<u32> {
    match code {
        MOD_LEFTSHIFT | MOD_RIGHTSHIFT => Some(XKB_SHIFT_BIT),
        MOD_LEFTCTRL | MOD_RIGHTCTRL => Some(XKB_CONTROL_BIT),
        MOD_LEFTALT => Some(XKB_MOD1_BIT),
        MOD_LEFTMETA | MOD_RIGHTMETA => Some(XKB_MOD4_BIT),
        MOD_RIGHTALT => Some(XKB_MOD5_BIT),
        MOD_HENKAN => Some(XKB_MOD3_BIT),
        _ => None,
    }
}

impl EvdevAdapter {
    pub fn prepare() -> Result<Self> {
        let output: Box<dyn KeyEmitter + Send> = Box::new(
            UinputEmitter::open().context("output: uinput backend open failed")?,
        );
        let xkb = build_us_xkb().context("xkb: failed to build us keymap")?;
        Ok(Self {
            streams: SelectAll::new(),
            output,
            xkb,
            mod_mask: 0,
            emitted_mods: 0,
            held_physical: HashSet::new(),
            forwarded_press: HashSet::new(),
            escape_taps: Vec::new(),
            should_exit: false,
        })
    }

    pub fn grab_keyboards(&mut self) -> Result<()> {
        let keyboards = scan_keyboards();
        let mut streams = SelectAll::new();

        for (path, name) in &keyboards {
            let mut device = match Device::open(path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(name, path = %path.display(), error = %e, "evdev: open failed; skipping");
                    continue;
                }
            };
            if let Err(e) = device.grab() {
                tracing::warn!(name, path = %path.display(), error = %e,
                    "evdev: grab failed; skipping (another grabber likely holds device)");
                continue;
            }
            let stream = match device.into_event_stream() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(name, path = %path.display(), error = %e,
                        "evdev: into_event_stream failed; skipping");
                    continue;
                }
            };
            tracing::info!(name, path = %path.display(), "evdev: grab taken");
            streams.push(stream);
        }

        if streams.is_empty() {
            bail!("evdev: no keyboards grabbed — evdev-only mode cannot run");
        }

        self.streams = streams;
        Ok(())
    }

    pub fn open() -> Result<Self> {
        let mut adapter = Self::prepare()?;
        adapter.grab_keyboards()?;
        Ok(adapter)
    }

    fn shortcut_mods_held(&self) -> bool {
        // True when any non-shift modifier is depressed.
        (self.mod_mask & !XKB_SHIFT_BIT) != 0
    }

    fn passthrough(&mut self, code: u32, value: i32) {
        // `time` is ignored by `UinputEmitter` (kernel stamps its own).
        // The trait's `emit_key` swallows errors internally — at this layer
        // we surface a single trace line per attempt.
        self.output.emit_key(0, code, value as u32);
        tracing::trace!(code, value, "output.emit_key dispatched");
    }

    /// Forward a raw key *press* and record the keycode so its later
    /// autorepeat/release is forwarded too. Presses the engine consumes are
    /// never recorded, so their continuation events are dropped as dangling.
    fn forward_press_raw(&mut self, code: u32, value: i32) {
        self.forwarded_press.insert(code);
        self.passthrough(code, value);
    }

    /// Adjust uinput-emitted Shift / AltGr (Level3) / HENK (Level5) state
    /// to `target`. Used by `emit_string` to navigate the EIGHT_LEVEL
    /// daklak_vn keymap. Only touches Shift + Level3 + Level5; other mods
    /// (Ctrl/Alt/Meta) are untouched because emit_string is only reached
    /// when none are held.
    fn set_uinput_mods(&mut self, target: u32) {
        let cur = self.emitted_mods;
        let mask = keymap::MOD_SHIFT | keymap::MOD_LEVEL3 | keymap::MOD_LEVEL5;
        let target = target & mask;
        tracing::trace!(cur = format!("{:#x}", cur), target = format!("{:#x}", target),
            "set_uinput_mods enter");

        let want_shift = (target & keymap::MOD_SHIFT) != 0;
        let have_shift = (cur & keymap::MOD_SHIFT) != 0;
        if want_shift && !have_shift {
            tracing::trace!("set_uinput_mods: press LEFTSHIFT");
            self.passthrough(MOD_LEFTSHIFT, 1);
        } else if !want_shift && have_shift {
            tracing::trace!("set_uinput_mods: release LEFTSHIFT");
            self.passthrough(MOD_LEFTSHIFT, 0);
        }

        let want_l3 = (target & keymap::MOD_LEVEL3) != 0;
        let have_l3 = (cur & keymap::MOD_LEVEL3) != 0;
        if want_l3 && !have_l3 {
            tracing::trace!("set_uinput_mods: press RIGHTALT (Level3)");
            self.passthrough(MOD_RIGHTALT, 1);
        } else if !want_l3 && have_l3 {
            tracing::trace!("set_uinput_mods: release RIGHTALT (Level3)");
            self.passthrough(MOD_RIGHTALT, 0);
        }

        let want_l5 = (target & keymap::MOD_LEVEL5) != 0;
        let have_l5 = (cur & keymap::MOD_LEVEL5) != 0;
        if want_l5 && !have_l5 {
            tracing::trace!("set_uinput_mods: press HENKAN (Level5)");
            self.passthrough(MOD_HENKAN, 1);
        } else if !want_l5 && have_l5 {
            tracing::trace!("set_uinput_mods: release HENKAN (Level5)");
            self.passthrough(MOD_HENKAN, 0);
        }

        self.emitted_mods = (self.emitted_mods & !mask) | target;
    }

    fn emit_string(&mut self, s: &str) {
        let original = self.emitted_mods
            & (keymap::MOD_SHIFT | keymap::MOD_LEVEL3 | keymap::MOD_LEVEL5);
        tracing::debug!(s, original = format!("{:#x}", original), "emit_string enter");
        for ch in s.chars() {
            let Some(spec) = keymap::char_to_emit(ch) else {
                tracing::warn!(ch = %ch,
                    "evdev emit: char outside daklak_vn + us inventory; dropped");
                continue;
            };
            tracing::debug!(
                ch = %ch,
                keycode = spec.keycode,
                mods = format!("{:#x}", spec.mods),
                "emit_string: char_to_emit"
            );
            self.set_uinput_mods(spec.mods);
            // Tail-drop fix: if the user is still physically holding the key
            // we're about to emit, libxkbcommon (in the focused
            // client) sees the second press as a duplicate of an
            // already-held key and drops it. Emit a prelude release
            // first so the press lands as a fresh state transition.
            if self.held_physical.contains(&spec.keycode) {
                tracing::debug!(
                    keycode = spec.keycode,
                    "emit_string: prelude release for still-held user key (tail-drop fix)"
                );
                self.passthrough(spec.keycode, 0);
            }
            self.passthrough(spec.keycode, 1);
            self.passthrough(spec.keycode, 0);
        }
        tracing::trace!("emit_string: restoring mods");
        self.set_uinput_mods(original);
    }

    fn emit_backspaces(&mut self, n: usize) {
        tracing::debug!(n, "emit_backspaces");
        for _ in 0..n {
            self.passthrough(KEY_BACKSPACE, 1);
            self.passthrough(KEY_BACKSPACE, 0);
        }
    }

    fn process_event<H: EvdevHandler>(&mut self, handler: &mut H, ev: &InputEvent) {
        if ev.event_type() != EventType::KEY {
            return;
        }
        let code = ev.code() as u32;
        let value = ev.value();

        // Track physical key state for the tail-drop fix (in
        // emit_string). value=1 press, value=0 release; autorepeat=2
        // doesn't change held state.
        match value {
            1 => {
                self.held_physical.insert(code);
            }
            0 => {
                self.held_physical.remove(&code);
            }
            _ => {}
        }

        // Modifier events: update mask + xkb state, forward verbatim.
        if let Some(bit) = mod_bit(code) {
            match value {
                1 => self.mod_mask |= bit,
                0 => self.mod_mask &= !bit,
                _ => {}
            }
            self.xkb.update_modifiers(self.mod_mask, 0, 0, 0);
            // Mirror Shift / AltGr bits into emitted_mods (the
            // backend-visible mod state) so
            // emit_string knows the compositor-visible mod state at the
            // start of its dance.
            let dance_bit =
                bit & (keymap::MOD_SHIFT | keymap::MOD_LEVEL3 | keymap::MOD_LEVEL5);
            if dance_bit != 0 {
                match value {
                    1 => self.emitted_mods |= dance_bit,
                    0 => self.emitted_mods &= !dance_bit,
                    _ => {}
                }
            }
            self.passthrough(code, value);
            return;
        }

        // Triple-Esc emergency escape (press edge only).
        if code == KEY_ESC && value == 1 {
            let now = Instant::now();
            self.escape_taps
                .retain(|t| now.duration_since(*t) < Duration::from_secs(1));
            self.escape_taps.push(now);
            if self.escape_taps.len() >= 3 {
                tracing::warn!("evdev: triple-Esc emergency escape — exiting loop");
                self.escape_taps.clear();
                self.should_exit = true;
                handler.clear_session();
                self.forward_press_raw(KEY_ESC, 1);
                return;
            }
        }

        // Releases and autorepeats are continuation events: only meaningful
        // for a key whose press was forwarded raw. If the engine consumed the
        // press (Consumed / Apply), the client never saw a matching press, so
        // forwarding the release/autorepeat would emit a dangling event —
        // which, through the live IBus passthrough path, can strand the client
        // in an auto-repeat. Modifiers are handled above and always forward
        // both edges.
        if value != 1 {
            if self.forwarded_press.contains(&code) {
                self.passthrough(code, value);
                if value == 0 {
                    self.forwarded_press.remove(&code);
                }
            }
            return;
        }

        // From here: press events (value == 1).

        // Ctrl/Alt/Super shortcuts bypass engine. Reset composition
        // state — the shortcut may have edited the display behind our
        // back (Alt+BS deletes a word, Ctrl+A selects all, etc.), so
        // any pending engine shadow no longer matches what's on screen.
        if self.shortcut_mods_held() {
            handler.full_reset_window();
            handler.clear_last_input_char();
            self.forward_press_raw(code, value);
            return;
        }

        // Non-printable keys (arrows, F-keys, Home/End, Esc not in
        // emergency-tap, Insert/Delete, etc.) forward raw. Same reset
        // rationale — cursor moves invalidate the engine's word context.
        let Some(ch) = self.xkb.key_to_char(code) else {
            handler.full_reset_window();
            handler.clear_last_input_char();
            self.forward_press_raw(code, value);
            return;
        };

        handler.check_idle_reset_window();

        if code == KEY_BACKSPACE {
            match handler.handle_backspace() {
                KeyDecision::Consumed => {}
                KeyDecision::ForwardRaw => self.forward_press_raw(code, value),
                KeyDecision::Apply { backspaces, .. } => self.emit_backspaces(backspaces),
            }
            return;
        }

        match handler.handle_char(code, ch) {
            KeyDecision::Consumed => {}
            KeyDecision::ForwardRaw => self.forward_press_raw(code, value),
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                self.emit_backspaces(backspaces);
                if !commit.is_empty() {
                    self.emit_string(&commit);
                }
            }
        }
    }

    pub async fn run_until_shutdown<H: EvdevHandler>(
        &mut self,
        handler: &mut H,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                biased;

                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        tracing::info!("evdev: supervisor shutdown requested");
                        break;
                    }
                }

                _ = signal::ctrl_c() => {
                    tracing::info!("evdev: ctrl_c — exiting");
                    break;
                }

                ev = self.streams.next() => {
                    match ev {
                        Some(Ok(ev)) => {
                            self.process_event(handler, &ev);
                            if self.should_exit {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "evdev: stream error — exiting");
                            break;
                        }
                        None => {
                            tracing::info!("evdev: all streams ended — exiting");
                            break;
                        }
                    }
                }
            }
        }

        // SelectAll drop releases each device's grab (evdev::Device::Drop
        // calls ungrab). Explicit clear so it happens before we return.
        self.streams = SelectAll::new();
        tracing::info!("evdev: grabs released");

        Ok(())
    }

    pub async fn run<H: EvdevHandler>(&mut self, handler: &mut H) -> Result<()> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        self.run_until_shutdown(handler, rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn own_uinput_device_name_is_daklak() {
        assert_eq!(DAKLAK_NAME, "daklak");
    }

    #[derive(Clone, Default)]
    struct CapturingEmitter {
        events: Arc<Mutex<Vec<(u32, u32)>>>,
    }

    impl KeyEmitter for CapturingEmitter {
        fn emit_key(&mut self, _time: u32, keycode: u32, value: u32) {
            self.events.lock().unwrap().push((keycode, value));
        }
        fn emit_modifiers(&mut self, _d: u32, _l: u32, _lo: u32, _g: u32) {}
    }

    #[derive(Default)]
    struct MockHandler {
        char_decisions: Vec<KeyDecision>,
    }

    impl EvdevHandler for MockHandler {
        fn handle_char(&mut self, _code: u32, _ch: char) -> KeyDecision {
            if self.char_decisions.is_empty() {
                KeyDecision::Consumed
            } else {
                self.char_decisions.remove(0)
            }
        }
        fn handle_backspace(&mut self) -> KeyDecision {
            KeyDecision::ForwardRaw
        }
        fn clear_session(&mut self) {}
        fn clear_last_input_char(&mut self) {}
        fn full_reset_window(&mut self) {}
        fn check_idle_reset_window(&mut self) {}
    }

    fn test_adapter(emitter: CapturingEmitter) -> EvdevAdapter {
        EvdevAdapter {
            streams: SelectAll::new(),
            output: Box::new(emitter),
            xkb: build_us_xkb().expect("us keymap"),
            mod_mask: 0,
            emitted_mods: 0,
            held_physical: HashSet::new(),
            forwarded_press: HashSet::new(),
            escape_taps: Vec::new(),
            should_exit: false,
        }
    }

    fn key(code: u16, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code, value)
    }

    const KEY_S: u16 = 31;

    /// A press the engine consumed must not forward its release. Otherwise the
    /// client receives a press-less release (dangling through the live IBus
    /// passthrough path), which strands it in an auto-repeat until the next key.
    #[test]
    fn consumed_press_drops_its_release() {
        let emitter = CapturingEmitter::default();
        let events = emitter.events.clone();
        let mut adapter = test_adapter(emitter);
        let mut handler = MockHandler {
            char_decisions: vec![KeyDecision::Consumed],
        };

        adapter.process_event(&mut handler, &key(KEY_S, 1)); // press → consumed
        adapter.process_event(&mut handler, &key(KEY_S, 0)); // release

        assert!(
            events.lock().unwrap().is_empty(),
            "consumed press must emit nothing on press or release"
        );
        assert!(!adapter.forwarded_press.contains(&(KEY_S as u32)));
    }

    /// A press the engine forwarded raw must forward its release too, so the
    /// press/release pair stays balanced at the client.
    #[test]
    fn forwarded_press_forwards_its_release() {
        let emitter = CapturingEmitter::default();
        let events = emitter.events.clone();
        let mut adapter = test_adapter(emitter);
        let mut handler = MockHandler {
            char_decisions: vec![KeyDecision::ForwardRaw],
        };

        adapter.process_event(&mut handler, &key(KEY_S, 1)); // press → forward raw
        adapter.process_event(&mut handler, &key(KEY_S, 0)); // release

        let s = KEY_S as u32;
        assert_eq!(&*events.lock().unwrap(), &[(s, 1), (s, 0)]);
        assert!(
            !adapter.forwarded_press.contains(&s),
            "release must clear the forwarded-press mark"
        );
    }

    /// Autorepeats follow the same rule: forwarded for a forwarded press,
    /// dropped for a consumed one.
    #[test]
    fn autorepeat_follows_press_disposition() {
        let s = KEY_S as u32;

        // Consumed press: autorepeat dropped.
        let emitter = CapturingEmitter::default();
        let events = emitter.events.clone();
        let mut adapter = test_adapter(emitter);
        let mut handler = MockHandler {
            char_decisions: vec![KeyDecision::Consumed],
        };
        adapter.process_event(&mut handler, &key(KEY_S, 1));
        adapter.process_event(&mut handler, &key(KEY_S, 2)); // autorepeat
        assert!(events.lock().unwrap().is_empty());

        // Forwarded press: autorepeat forwarded.
        let emitter = CapturingEmitter::default();
        let events = emitter.events.clone();
        let mut adapter = test_adapter(emitter);
        let mut handler = MockHandler {
            char_decisions: vec![KeyDecision::ForwardRaw],
        };
        adapter.process_event(&mut handler, &key(KEY_S, 1));
        adapter.process_event(&mut handler, &key(KEY_S, 2));
        assert_eq!(&*events.lock().unwrap(), &[(s, 1), (s, 2)]);
    }
}
