//! Pins down `emit_char`'s shared body against a recording `MockEmitter`,
//! independent of any Wayland/uinput wiring.

use std::time::Instant;

use viet_ime_key_emitter::{emit_char, KeyEmitter};

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Key(u32, u32, u32),                 // time, keycode, value
    Modifiers(u32, u32, u32, u32),      // dep, lat, lock, group
}

struct MockEmitter {
    log: Vec<Event>,
    echo_through_grab: bool,
}

impl MockEmitter {
    fn new(echo_through_grab: bool) -> Self {
        Self { log: Vec::new(), echo_through_grab }
    }
}

impl KeyEmitter for MockEmitter {
    fn emit_key(&mut self, time: u32, keycode: u32, value: u32) {
        self.log.push(Event::Key(time, keycode, value));
    }
    fn emit_modifiers(&mut self, dep: u32, lat: u32, lock: u32, group: u32) {
        self.log.push(Event::Modifiers(dep, lat, lock, group));
    }
    fn modifier_echo_through_grab(&self) -> bool {
        self.echo_through_grab
    }
}

#[test]
fn emit_char_unknown_returns_false() {
    let mut em = MockEmitter::new(true);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    let ok = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), None, 0, '\u{FFFF}');
    assert!(!ok);
    assert!(em.log.is_empty());
    assert_eq!(pending, 0);
}

#[test]
fn emit_char_known_char_emits_press_release() {
    let mut em = MockEmitter::new(true);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    // 'â' is in the daklak inventory; we don't assert on the specific
    // keycode (depends on slot table), only on the shape of the event
    // stream: optional mod dance, then press(1) then release(0).
    let ok = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), None, 42, 'â');
    assert!(ok);
    let keys: Vec<_> = em
        .log
        .iter()
        .filter_map(|e| match e {
            Event::Key(t, kc, v) => Some((*t, *kc, *v)),
            _ => None,
        })
        .collect();
    assert_eq!(keys.len(), 2, "should be one press + one release");
    assert_eq!(keys[0].2, 1);
    assert_eq!(keys[1].2, 0);
    assert_eq!(keys[0].1, keys[1].1, "press and release keycodes match");
    assert_eq!(keys[0].0, 42);
}

#[test]
fn emit_char_bumps_pending_when_echo_grab() {
    // Char with mods (uppercase Â needs Shift) — should provoke a mod
    // dance, and on an echo-through-grab emitter the counter bumps twice
    // (set + restore).
    let mut em = MockEmitter::new(true);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    let ok = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), None, 0, 'Â');
    assert!(ok);
    assert_eq!(pending, 2, "set + restore bump on grab-echo emitter");
    assert!(at.is_some());
}

#[test]
fn emit_char_no_bump_when_no_grab_echo() {
    let mut em = MockEmitter::new(false);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    let ok = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), None, 0, 'Â');
    assert!(ok);
    assert_eq!(pending, 0, "no bump on emitters that don't echo through grab");
    assert!(at.is_none());
}

#[test]
fn emit_char_path_a_prelude_release() {
    // When `held_user_kc` matches the keycode emit_char is about to press,
    // the very first event must be a release of that keycode.
    let mut em = MockEmitter::new(true);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    // First emit 'â' once with no held_user_kc to discover its keycode.
    let _ = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), None, 0, 'â');
    let kc = em
        .log
        .iter()
        .find_map(|e| match e {
            Event::Key(_, kc, 1) => Some(*kc),
            _ => None,
        })
        .expect("first emit produces a press");

    // Now emit again with held_user_kc == kc and assert prelude release.
    let mut em = MockEmitter::new(true);
    let mut pending = 0u32;
    let mut at: Option<Instant> = None;
    let ok = emit_char(&mut em, &mut pending, &mut at, (0, 0, 0, 0), Some(kc), 0, 'â');
    assert!(ok);
    let first = em.log.first().expect("at least one event");
    assert_eq!(*first, Event::Key(0, kc, 0), "prelude release must come first");
}
