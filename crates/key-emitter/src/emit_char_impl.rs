//! Backend-agnostic `emit_char` body.
//!
//! Routes a single Vietnamese precomposed `char` to whatever backend the
//! caller picked (`VkV2Emitter` / `VkV1Emitter`). Owns the tail-char-drop
//! prelude-release fix and the modifier dance.
//!
//! Sites:
//!
//! - `AdapterSink::vk_commit_char` (ForwardKey synthetic-keymap channel from
//!   the IM virtual keyboard).
//! - `EvdevVkSink` (Tier 5 from the evdev grab) — planned.
//!
//! Returns `true` when `c` was emitted, `false` when `c` is outside
//! `char_to_emit`'s inventory. The caller is responsible for any
//! fallback (`commit_string` on Wayland).

use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_keymap::{char_to_emit, plan_mod_dance};

use crate::KeyEmitter;

pub type ModifierSnapshot = (u32, u32, u32, u32);

pub struct SyntheticMods<'a> {
    pub pending: &'a mut u32,
    pub expected: &'a mut VecDeque<ModifierSnapshot>,
    pub emitted_at: &'a mut Option<Instant>,
}

/// Keycodes whose user press was forwarded to the focused client and whose
/// release has not been forwarded yet.
///
/// Tail-char-drop fix. The client (or KWin, or XWayland) ignores a press for
/// a keycode it already considers down, so a synthetic press of a key the
/// user is still holding is lost. Fast typing holds several keys at once
/// (`hanh` + `j`: `n` and `h` are both down when `j` composes `ạnh`), so
/// every held key must be tracked, not just the last one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldKeys(Vec<u32>);

impl HeldKeys {
    pub fn press(&mut self, keycode: u32) {
        if !self.0.contains(&keycode) {
            self.0.push(keycode);
        }
    }

    /// Forget `keycode`; returns whether it was held.
    pub fn release(&mut self, keycode: u32) -> bool {
        let before = self.0.len();
        self.0.retain(|&kc| kc != keycode);
        self.0.len() != before
    }

    pub fn contains(&self, keycode: u32) -> bool {
        self.0.contains(&keycode)
    }

    /// Tail-char-drop fix: a client drops a synthetic press of a keycode it
    /// still sees down from the user (fast rollover: the user's Backspace or
    /// letter is still held when daklak re-presses it for an edit). If
    /// `keycode` is held, release it on `emitter` first and forget it: the
    /// client now sees it up, so later presses of it need no release.
    /// Returns whether a release was emitted.
    pub fn release_before_press(
        &mut self,
        emitter: &mut dyn KeyEmitter,
        time: u32,
        keycode: u32,
    ) -> bool {
        if !self.release(keycode) {
            return false;
        }
        tracing::debug!(keycode, "prelude release for still-held user key");
        emitter.emit_key(time, keycode, 0);
        true
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

pub struct EmitCharParams<'a> {
    pub raw_mods: ModifierSnapshot,
    /// Keys the focused client sees as held. A synthetic press of one of
    /// them is preceded by a release, and the key is removed: the client now
    /// sees it up, so later chars on the same keycode need no release.
    pub held: &'a mut HeldKeys,
    pub time: u32,
    pub c: char,
}

pub fn emit_char(
    emitter: &mut dyn KeyEmitter,
    synthetic_mods: &mut SyntheticMods<'_>,
    params: EmitCharParams<'_>,
) -> bool {
    let EmitCharParams {
        raw_mods,
        held,
        time,
        c,
    } = params;

    let Some(spec) = char_to_emit(c) else {
        tracing::trace!(char = %c, "emit_char: char not in synthetic keymap");
        return false;
    };
    let dance = plan_mod_dance(raw_mods.0, spec.mods);
    let (_, lat, lock, group) = raw_mods;
    let echo = emitter.modifier_echo_through_grab();

    held.release_before_press(emitter, time, spec.keycode);

    if let Some((emit_mask, _)) = dance {
        emitter.emit_modifiers(emit_mask, lat, lock, group);
        if echo {
            synthetic_mods
                .expected
                .push_back((emit_mask, lat, lock, group));
            *synthetic_mods.pending = synthetic_mods.expected.len() as u32;
            *synthetic_mods.emitted_at = Some(Instant::now());
        }
    }
    emitter.emit_key(time, spec.keycode, 1);
    emitter.emit_key(time, spec.keycode, 0);
    if let Some((_, restore_mask)) = dance {
        emitter.emit_modifiers(restore_mask, lat, lock, group);
        if echo {
            synthetic_mods
                .expected
                .push_back((restore_mask, lat, lock, group));
            *synthetic_mods.pending = synthetic_mods.expected.len() as u32;
            *synthetic_mods.emitted_at = Some(Instant::now());
        }
    }
    tracing::trace!(
        char = %c,
        keycode = spec.keycode,
        dep_mods = format!("{:#x}", raw_mods.0),
        spec_mods = format!("{:#x}", spec.mods),
        danced = dance.is_some(),
        "emit_char emitted"
    );
    true
}
