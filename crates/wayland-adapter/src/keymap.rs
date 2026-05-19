//! Wayland-tied wrappers around the shared `viet-ime-keymap` data crate.
//!
//! The pure keymap data (Vietnamese inventory, FOUR_LEVEL slot math,
//! `keymap_text()` generator, `plan_mod_dance`) lives in `viet-ime-keymap`
//! and is reused by the daemon's evdev-only mode. This module adds the
//! pieces that depend on the Wayland virtual-keyboard protocol:
//!
//! - `emit_char(...)` — drives `zwp_virtual_keyboard_v1::key()` / `modifiers()`.
//! - `DaklakKeymap` + `build()` — memfd-backed handle for
//!   `zwp_virtual_keyboard_v1.keymap()` upload.
//!
//! ### Hazard — Chromium-class apps
//!
//! Chromium has its own hard-coded `LinuxKeyCode → DomCode` table for
//! keyboard introspection. Evdev codes 200+ are KEY_KBDILLUMUP/KEY_FN_F*
//! in that table — feeding them Unicode keysyms crashes the renderer.
//! Use `force_uinput_apps` for chromium-class instead.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use xkbcommon::xkb::{
    Context as XkbCtx, Keymap, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS,
    KEYMAP_FORMAT_TEXT_V1,
};

// Re-export shared data + helpers so existing call sites
// (`use viet_ime_wayland_adapter::keymap::{char_to_emit, …}`) keep working.
pub use viet_ime_keymap::{
    char_to_emit, keymap_text, plan_mod_dance, vn_pairs, EmitSpec, BASE_EVDEV, MOD_LEVEL3,
    MOD_SHIFT,
};

/// Shared `vk_commit_char` body. Both `AdapterSink` (Tier 4 from IM grab)
/// and `EvdevVkSink` (Tier 5 from evdev grab) delegate here so the
/// Path A prelude-release fix and the modifier dance live in one place.
///
/// Returns `true` when `c` was emitted, `false` when `c` is outside
/// `char_to_emit`'s inventory (caller's responsibility to fall back).
pub fn emit_char(
    vk: &ZwpVirtualKeyboardV1,
    synthetic_mods_pending: &mut u32,
    synthetic_mods_emitted_at: &mut Option<Instant>,
    raw_mods: (u32, u32, u32, u32),
    held_user_kc: Option<u32>,
    time: u32,
    c: char,
) -> bool {
    let Some(spec) = char_to_emit(c) else {
        tracing::trace!(char = %c, "emit_char: char not in synthetic keymap");
        return false;
    };
    let dance = plan_mod_dance(raw_mods.0, spec.mods);
    let (_, lat, lock, group) = raw_mods;

    // Path A (XWayland tail-char-drop fix). When the user is currently
    // holding a key whose keycode equals the one we're about to press,
    // X's input thread still has it DOWN and silently no-ops our
    // synthetic press as a duplicate. Emit a synthetic release first.
    if held_user_kc == Some(spec.keycode) {
        tracing::debug!(
            keycode = spec.keycode,
            char = %c,
            "emit_char: prelude release for still-held user key (Path A)"
        );
        vk.key(time, spec.keycode, 0);
    }

    if let Some((emit_mask, _)) = dance {
        vk.modifiers(emit_mask, lat, lock, group);
        *synthetic_mods_pending = synthetic_mods_pending.saturating_add(1);
        *synthetic_mods_emitted_at = Some(Instant::now());
    }
    vk.key(time, spec.keycode, 1);
    vk.key(time, spec.keycode, 0);
    if let Some((_, restore_mask)) = dance {
        vk.modifiers(restore_mask, lat, lock, group);
        *synthetic_mods_pending = synthetic_mods_pending.saturating_add(1);
        *synthetic_mods_emitted_at = Some(Instant::now());
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

/// Daemon-owned handle to the synthetic keymap. Lives on `AppState`.
pub struct DaklakKeymap {
    pub fd: OwnedFd,
    pub size: u32,
}

/// Build the keymap, validate via libxkbcommon parse, dump into a memfd.
/// Compositor reads the fd via `zwp_virtual_keyboard_v1::keymap()`.
pub fn build() -> Result<DaklakKeymap> {
    let text = keymap_text();

    let ctx = XkbCtx::new(CONTEXT_NO_FLAGS);
    let _ = Keymap::new_from_string(
        &ctx,
        text.clone(),
        KEYMAP_FORMAT_TEXT_V1,
        KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow!("xkbcommon rejected synthesized daklak keymap"))?;

    let name = CString::new("daklak-keymap").unwrap();
    // SAFETY: memfd_create is a stable Linux syscall (>= 3.17).
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: raw is a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.extend_from_slice(text.as_bytes());
    buf.push(0);

    let mut offset = 0;
    while offset < buf.len() {
        // SAFETY: writing into our own freshly-created fd.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                buf[offset..].as_ptr() as *const _,
                buf.len() - offset,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error())
                .context("write daklak keymap into memfd");
        }
        offset += n as usize;
    }

    Ok(DaklakKeymap { fd, size: buf.len() as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_parses() {
        let km = build().expect("daklak keymap must parse");
        assert!(km.size > 100);
    }
}
