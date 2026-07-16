//! Pins down `emit_char`'s shared body against a recording `MockEmitter`,
//! independent of any Wayland/uinput wiring.

use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_key_emitter::{emit_char, EmitCharParams, KeyEmitter, SyntheticMods};

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Key(u32, u32, u32),            // time, keycode, value
    Modifiers(u32, u32, u32, u32), // dep, lat, lock, group
}

struct MockEmitter {
    log: Vec<Event>,
    echo_through_grab: bool,
}

impl MockEmitter {
    fn new(echo_through_grab: bool) -> Self {
        Self {
            log: Vec::new(),
            echo_through_grab,
        }
    }
}

struct EmitCharFixture {
    pending: u32,
    expected: VecDeque<(u32, u32, u32, u32)>,
    at: Option<Instant>,
}

impl EmitCharFixture {
    fn new() -> Self {
        Self {
            pending: 0,
            expected: VecDeque::new(),
            at: None,
        }
    }

    fn emit(
        &mut self,
        emitter: &mut MockEmitter,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
        time: u32,
        c: char,
    ) -> bool {
        let mut synthetic_mods = SyntheticMods {
            pending: &mut self.pending,
            expected: &mut self.expected,
            emitted_at: &mut self.at,
        };
        emit_char(
            emitter,
            &mut synthetic_mods,
            EmitCharParams {
                raw_mods,
                held_user_kc,
                time,
                c,
            },
        )
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
    let mut fixture = EmitCharFixture::new();
    let ok = fixture.emit(&mut em, (0, 0, 0, 0), None, 0, '\u{FFFF}');
    assert!(!ok);
    assert!(em.log.is_empty());
    assert_eq!(fixture.pending, 0);
    assert!(fixture.expected.is_empty());
}

#[test]
fn emit_char_known_char_emits_press_release() {
    let mut em = MockEmitter::new(true);
    let mut fixture = EmitCharFixture::new();
    // 'â' is in the daklak inventory; we don't assert on the specific
    // keycode (depends on slot table), only on the shape of the event
    // stream: optional mod dance, then press(1) then release(0).
    let ok = fixture.emit(&mut em, (0, 0, 0, 0), None, 42, 'â');
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
    let mut fixture = EmitCharFixture::new();
    let ok = fixture.emit(&mut em, (0, 0, 0, 0), None, 0, 'Â');
    assert!(ok);
    assert_eq!(
        fixture.pending, 2,
        "set + restore bump on grab-echo emitter"
    );
    let emitted_modifiers = em
        .log
        .iter()
        .filter_map(|e| match e {
            Event::Modifiers(dep, lat, lock, group) => Some((*dep, *lat, *lock, *group)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        fixture.expected.into_iter().collect::<Vec<_>>(),
        emitted_modifiers,
        "expected echo masks are tracked in wire order"
    );
    assert!(fixture.at.is_some());
}

#[test]
fn emit_char_no_bump_when_no_grab_echo() {
    let mut em = MockEmitter::new(false);
    let mut fixture = EmitCharFixture::new();
    let ok = fixture.emit(&mut em, (0, 0, 0, 0), None, 0, 'Â');
    assert!(ok);
    assert_eq!(
        fixture.pending, 0,
        "no bump on emitters that don't echo through grab"
    );
    assert!(fixture.expected.is_empty());
    assert!(fixture.at.is_none());
}

#[test]
fn emit_char_path_a_prelude_release() {
    // When `held_user_kc` matches the keycode emit_char is about to press,
    // the very first event must be a release of that keycode.
    let mut em = MockEmitter::new(true);
    let mut fixture = EmitCharFixture::new();
    // First emit 'â' once with no held_user_kc to discover its keycode.
    let _ = fixture.emit(&mut em, (0, 0, 0, 0), None, 0, 'â');
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
    let mut fixture = EmitCharFixture::new();
    let ok = fixture.emit(&mut em, (0, 0, 0, 0), Some(kc), 0, 'â');
    assert!(ok);
    let first = em.log.first().expect("at least one event");
    assert_eq!(
        *first,
        Event::Key(0, kc, 0),
        "prelude release must come first"
    );
}
