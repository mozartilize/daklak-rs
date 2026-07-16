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

pub struct EmitCharParams {
    pub raw_mods: ModifierSnapshot,
    pub held_user_kc: Option<u32>,
    pub time: u32,
    pub c: char,
}

pub fn emit_char(
    emitter: &mut dyn KeyEmitter,
    synthetic_mods: &mut SyntheticMods<'_>,
    params: EmitCharParams,
) -> bool {
    let EmitCharParams {
        raw_mods,
        held_user_kc,
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

    // XWayland tail-char-drop fix. When the user is currently
    // holding a key whose keycode equals the one we're about to press,
    // X's input thread still has it DOWN and silently no-ops our
    // synthetic press as a duplicate. Emit a synthetic release first.
    if held_user_kc == Some(spec.keycode) {
        tracing::debug!(
            keycode = spec.keycode,
            char = %c,
            "emit_char: prelude release for still-held user key (tail-char-drop fix)"
        );
        emitter.emit_key(time, spec.keycode, 0);
    }

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
