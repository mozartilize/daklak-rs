//! Synthetic xkb keymap that fits inside the X11 keycode ceiling (≤255).
//! Wired into `zwp_virtual_keyboard_v1.keymap()` so daklak can emit
//! Vietnamese chars via raw `vk_key()` events — no `commit_string`, no
//! `zwp_text_input_v3`. Works for any client that reads `wl_keyboard`
//! (Qt5/XWayland-bridged-X11/tui-in-terminal).
//!
//! ### Layout
//!
//! Standard QWERTY layout is preserved via `include "pc+us+inet(evdev)"`,
//! so daklak can emit plain ASCII via standard evdev codes:
//!
//! - `a..z` → evdev 30, 48, 46, … (Shift for uppercase)
//! - `0..9` → evdev 2..11
//! - space, punct → standard evdev
//!
//! Vietnamese precomposed chars live in custom slots `200..234` (≤255)
//! using **FOUR_LEVEL** xkb type. Each slot packs 4 chars at levels:
//!
//! | Level | Modifier   | Wayland mod bits |
//! |-------|------------|-------------------|
//! | 1     | (none)     | 0                 |
//! | 2     | Shift      | 0x01              |
//! | 3     | AltGr      | 0x80 (Mod5)       |
//! | 4     | Shift+AltGr| 0x81              |
//!
//! Slot N contains `[lower[2N], upper[2N], lower[2N+1], upper[2N+1]]`,
//! so the natural Shift/no-Shift toggle still flips case. The AltGr leg
//! addresses the next character in the lowercase array. 67 lowercase
//! Vietnamese chars × 2 (with uppercase) = 134 chars → 34 slots, all
//! ≤234, well under the X11 ceiling. XWayland-bridged X11 clients
//! receive these via their own X keymap (Sway merges daklak's keymap
//! into the seat's wl_keyboard.keymap that XWayland reads).
//!
//! ### Hazard — Chromium-class apps
//!
//! Chromium has its own hard-coded `LinuxKeyCode → DomCode` table for
//! keyboard introspection. Evdev codes 200+ are KEY_KBDILLUMUP/KEY_FN_F*
//! in that table — feeding them Unicode keysyms crashes the renderer.
//! Use `force_uinput_apps` for chromium-class instead.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use xkbcommon::xkb::{
    Context as XkbCtx, Keymap, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS,
    KEYMAP_FORMAT_TEXT_V1,
};

/// First evdev keycode allocated to daklak's Vietnamese slots. 200 sits
/// in the spare range below KEY_FN_F* (224+) and below the X11 ceiling
/// (255), so XWayland clients see the keysym after their X keymap
/// rebuild on wl_keyboard.keymap.
pub const BASE_EVDEV: u32 = 200;

/// Wayland modifier bits (mods_depressed) used to address xkb levels.
pub const MOD_SHIFT: u32 = 0x01;
/// LevelThree is bound to Mod5 (AltGr) by the standard `pc+us+inet(evdev)`
/// layout we include. Setting this bit selects xkb level 3.
pub const MOD_LEVEL3: u32 = 0x80;

/// Specification for emitting `c` through `zwp_virtual_keyboard_v1::key()`.
/// `mods` is the OR-mask to set on `vk.modifiers()` before pressing
/// `keycode`. Callers restore the previous modifier state afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitSpec {
    pub keycode: u32,
    pub mods: u32,
}

/// Vietnamese lowercase precomposed inventory. Order is stable — index N
/// determines (slot, level) in the synthesized keymap.
const VN_LOWER: &[char] = &[
    // a-family
    'à', 'á', 'ả', 'ã', 'ạ',
    'â', 'ầ', 'ấ', 'ẩ', 'ẫ', 'ậ',
    'ă', 'ằ', 'ắ', 'ẳ', 'ẵ', 'ặ',
    // e-family
    'è', 'é', 'ẻ', 'ẽ', 'ẹ',
    'ê', 'ề', 'ế', 'ể', 'ễ', 'ệ',
    // i-family
    'ì', 'í', 'ỉ', 'ĩ', 'ị',
    // o-family
    'ò', 'ó', 'ỏ', 'õ', 'ọ',
    'ô', 'ồ', 'ố', 'ổ', 'ỗ', 'ộ',
    'ơ', 'ờ', 'ớ', 'ở', 'ỡ', 'ợ',
    // u-family
    'ù', 'ú', 'ủ', 'ũ', 'ụ',
    'ư', 'ừ', 'ứ', 'ử', 'ữ', 'ự',
    // y-family
    'ỳ', 'ý', 'ỷ', 'ỹ', 'ỵ',
    // d-stroke
    'đ',
];

/// Uppercase Vietnamese — same order as `VN_LOWER`, paired by index.
const VN_UPPER: &[char] = &[
    'À', 'Á', 'Ả', 'Ã', 'Ạ',
    'Â', 'Ầ', 'Ấ', 'Ẩ', 'Ẫ', 'Ậ',
    'Ă', 'Ằ', 'Ắ', 'Ẳ', 'Ẵ', 'Ặ',
    'È', 'É', 'Ẻ', 'Ẽ', 'Ẹ',
    'Ê', 'Ề', 'Ế', 'Ể', 'Ễ', 'Ệ',
    'Ì', 'Í', 'Ỉ', 'Ĩ', 'Ị',
    'Ò', 'Ó', 'Ỏ', 'Õ', 'Ọ',
    'Ô', 'Ồ', 'Ố', 'Ổ', 'Ỗ', 'Ộ',
    'Ơ', 'Ờ', 'Ớ', 'Ở', 'Ỡ', 'Ợ',
    'Ù', 'Ú', 'Ủ', 'Ũ', 'Ụ',
    'Ư', 'Ừ', 'Ứ', 'Ử', 'Ữ', 'Ự',
    'Ỳ', 'Ý', 'Ỷ', 'Ỹ', 'Ỵ',
    'Đ',
];

const _: () = assert!(VN_LOWER.len() == VN_UPPER.len());

/// Number of FOUR_LEVEL slots needed to pack all Vietnamese chars.
/// `ceil(VN_LOWER.len() / 2)` since each slot holds 2 lowercase letters
/// (one at L1, one at L3) plus their uppercase mirrors (L2, L4).
const fn slots_needed() -> usize {
    (VN_LOWER.len() + 1) / 2
}

/// Mods to set on `vk.modifiers()` to address level `lv` (1..=4).
const fn level_mods(lv: u8) -> u32 {
    match lv {
        1 => 0,
        2 => MOD_SHIFT,
        3 => MOD_LEVEL3,
        4 => MOD_SHIFT | MOD_LEVEL3,
        _ => 0,
    }
}

/// Locate `c` in the Vietnamese inventory and return (slot index, level).
fn locate_vn(c: char) -> Option<(usize, u8)> {
    if let Some(i) = VN_LOWER.iter().position(|&x| x == c) {
        let slot = i / 2;
        let level: u8 = if i % 2 == 0 { 1 } else { 3 };
        Some((slot, level))
    } else if let Some(i) = VN_UPPER.iter().position(|&x| x == c) {
        let slot = i / 2;
        let level: u8 = if i % 2 == 0 { 2 } else { 4 };
        Some((slot, level))
    } else {
        None
    }
}

/// Map plain ASCII to its evdev keycode in the standard pc/us layout.
/// Caller adds `MOD_SHIFT` for uppercase / shifted punctuation. None
/// for chars outside the ASCII printable range.
fn ascii_evdev(c: char) -> Option<(u32, u32)> {
    Some(match c {
        // Lowercase letters
        'a' => (30, 0), 'b' => (48, 0), 'c' => (46, 0), 'd' => (32, 0),
        'e' => (18, 0), 'f' => (33, 0), 'g' => (34, 0), 'h' => (35, 0),
        'i' => (23, 0), 'j' => (36, 0), 'k' => (37, 0), 'l' => (38, 0),
        'm' => (50, 0), 'n' => (49, 0), 'o' => (24, 0), 'p' => (25, 0),
        'q' => (16, 0), 'r' => (19, 0), 's' => (31, 0), 't' => (20, 0),
        'u' => (22, 0), 'v' => (47, 0), 'w' => (17, 0), 'x' => (45, 0),
        'y' => (21, 0), 'z' => (44, 0),
        // Uppercase letters — same keycode + Shift
        'A' => (30, MOD_SHIFT), 'B' => (48, MOD_SHIFT), 'C' => (46, MOD_SHIFT),
        'D' => (32, MOD_SHIFT), 'E' => (18, MOD_SHIFT), 'F' => (33, MOD_SHIFT),
        'G' => (34, MOD_SHIFT), 'H' => (35, MOD_SHIFT), 'I' => (23, MOD_SHIFT),
        'J' => (36, MOD_SHIFT), 'K' => (37, MOD_SHIFT), 'L' => (38, MOD_SHIFT),
        'M' => (50, MOD_SHIFT), 'N' => (49, MOD_SHIFT), 'O' => (24, MOD_SHIFT),
        'P' => (25, MOD_SHIFT), 'Q' => (16, MOD_SHIFT), 'R' => (19, MOD_SHIFT),
        'S' => (31, MOD_SHIFT), 'T' => (20, MOD_SHIFT), 'U' => (22, MOD_SHIFT),
        'V' => (47, MOD_SHIFT), 'W' => (17, MOD_SHIFT), 'X' => (45, MOD_SHIFT),
        'Y' => (21, MOD_SHIFT), 'Z' => (44, MOD_SHIFT),
        // Digits — top row, no Shift
        '1' => (2, 0), '2' => (3, 0), '3' => (4, 0), '4' => (5, 0), '5' => (6, 0),
        '6' => (7, 0), '7' => (8, 0), '8' => (9, 0), '9' => (10, 0), '0' => (11, 0),
        // Shifted digit row
        '!' => (2, MOD_SHIFT), '@' => (3, MOD_SHIFT), '#' => (4, MOD_SHIFT),
        '$' => (5, MOD_SHIFT), '%' => (6, MOD_SHIFT), '^' => (7, MOD_SHIFT),
        '&' => (8, MOD_SHIFT), '*' => (9, MOD_SHIFT), '(' => (10, MOD_SHIFT),
        ')' => (11, MOD_SHIFT),
        // Common punct
        ' ' => (57, 0),
        '-' => (12, 0), '_' => (12, MOD_SHIFT),
        '=' => (13, 0), '+' => (13, MOD_SHIFT),
        '[' => (26, 0), '{' => (26, MOD_SHIFT),
        ']' => (27, 0), '}' => (27, MOD_SHIFT),
        ';' => (39, 0), ':' => (39, MOD_SHIFT),
        '\'' => (40, 0), '"' => (40, MOD_SHIFT),
        '`' => (41, 0), '~' => (41, MOD_SHIFT),
        '\\' => (43, 0), '|' => (43, MOD_SHIFT),
        ',' => (51, 0), '<' => (51, MOD_SHIFT),
        '.' => (52, 0), '>' => (52, MOD_SHIFT),
        '/' => (53, 0), '?' => (53, MOD_SHIFT),
        _ => return None,
    })
}

/// Compute the (keycode, mods) needed to deliver `c` through
/// `zwp_virtual_keyboard_v1::key()`. Tries Vietnamese custom slots first,
/// then falls through to the standard ASCII layout. `None` only for
/// chars outside both inventories.
pub fn char_to_emit(c: char) -> Option<EmitSpec> {
    if let Some((slot, level)) = locate_vn(c) {
        return Some(EmitSpec {
            keycode: BASE_EVDEV + slot as u32,
            mods: level_mods(level),
        });
    }
    if let Some((kc, mods)) = ascii_evdev(c) {
        return Some(EmitSpec { keycode: kc, mods });
    }
    None
}

/// Number of Vietnamese precomposed pairs daklak ships. Exposed for the
/// debug log at startup.
pub fn vn_pairs() -> usize {
    VN_LOWER.len()
}

fn keymap_text() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("xkb_keymap {\n");
    // ── keycodes — extend evdev with daklak custom slots ──────────────────
    s.push_str("  xkb_keycodes \"evdev+daklak\" {\n");
    s.push_str("    include \"evdev+aliases(qwerty)\"\n");
    let max_xkb = BASE_EVDEV + slots_needed() as u32 + 8 + 8;
    s.push_str(&format!("    maximum = {};\n", max_xkb));
    for i in 0..slots_needed() {
        let xkb_kc = BASE_EVDEV + i as u32 + 8;
        s.push_str(&format!("    <DK{:02}> = {};\n", i, xkb_kc));
    }
    s.push_str("  };\n");
    // ── xkb types & compat (LevelThree binding lives in compat=complete) ──
    s.push_str("  xkb_types \"complete\" { include \"complete\" };\n");
    s.push_str("  xkb_compat \"complete\" { include \"complete\" };\n");
    // ── symbols — standard pc+us layout + FOUR_LEVEL custom slots ─────────
    s.push_str("  xkb_symbols \"pc+us+daklak\" {\n");
    s.push_str("    include \"pc+us+inet(evdev)\"\n");
    for i in 0..slots_needed() {
        let li_a = i * 2;
        let li_b = i * 2 + 1;
        let l1 = VN_LOWER[li_a] as u32;
        let l2 = VN_UPPER[li_a] as u32;
        let (l3, l4) = if li_b < VN_LOWER.len() {
            (VN_LOWER[li_b] as u32, VN_UPPER[li_b] as u32)
        } else {
            // Pad the last slot with VoidSymbol — protocol-safe sentinel.
            (0xFFFFFF, 0xFFFFFF)
        };
        let kc_name = format!("DK{:02}", i);
        if l3 == 0xFFFFFF {
            s.push_str(&format!(
                "    key <{}> {{ type[Group1] = \"FOUR_LEVEL\", [ U{:04X}, U{:04X}, VoidSymbol, VoidSymbol ] }};\n",
                kc_name, l1, l2
            ));
        } else {
            s.push_str(&format!(
                "    key <{}> {{ type[Group1] = \"FOUR_LEVEL\", [ U{:04X}, U{:04X}, U{:04X}, U{:04X} ] }};\n",
                kc_name, l1, l2, l3, l4
            ));
        }
    }
    s.push_str("  };\n");
    s.push_str("};\n");
    s
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
    fn slots_under_x11_ceiling() {
        let max_kc = BASE_EVDEV + slots_needed() as u32 - 1;
        assert!(max_kc <= 255,
            "Vietnamese slots overflowed X11 ceiling: max kc = {max_kc}");
    }

    #[test]
    fn vietnamese_lookup_returns_correct_level() {
        // First lowercase Vietnamese: slot 0, level 1, no mods.
        let s = char_to_emit('à').unwrap();
        assert_eq!(s, EmitSpec { keycode: BASE_EVDEV, mods: 0 });
        // Its uppercase: slot 0, level 2, Shift only.
        let s = char_to_emit('À').unwrap();
        assert_eq!(s, EmitSpec { keycode: BASE_EVDEV, mods: MOD_SHIFT });
        // Second lowercase: slot 0, level 3, AltGr only.
        let s = char_to_emit('á').unwrap();
        assert_eq!(s, EmitSpec { keycode: BASE_EVDEV, mods: MOD_LEVEL3 });
        // Its uppercase: slot 0, level 4, Shift+AltGr.
        let s = char_to_emit('Á').unwrap();
        assert_eq!(s, EmitSpec { keycode: BASE_EVDEV, mods: MOD_SHIFT | MOD_LEVEL3 });
    }

    #[test]
    fn ascii_lowercase_uses_standard_evdev() {
        let s = char_to_emit('a').unwrap();
        assert_eq!(s, EmitSpec { keycode: 30, mods: 0 });
        let s = char_to_emit('n').unwrap();
        assert_eq!(s, EmitSpec { keycode: 49, mods: 0 });
        let s = char_to_emit('t').unwrap();
        assert_eq!(s, EmitSpec { keycode: 20, mods: 0 });
        let s = char_to_emit(' ').unwrap();
        assert_eq!(s, EmitSpec { keycode: 57, mods: 0 });
    }

    #[test]
    fn ascii_uppercase_uses_shift() {
        let s = char_to_emit('A').unwrap();
        assert_eq!(s, EmitSpec { keycode: 30, mods: MOD_SHIFT });
        let s = char_to_emit('N').unwrap();
        assert_eq!(s, EmitSpec { keycode: 49, mods: MOD_SHIFT });
    }

    #[test]
    fn keymap_parses() {
        let km = build().expect("daklak keymap must parse");
        assert!(km.size > 100);
    }

    #[test]
    fn telex_coverage_under_ceiling() {
        // Engine emits these in common Vietnamese words; their keycodes
        // must all be <= 255 so XWayland clients receive them.
        for c in ['à', 'ầ', 'ế', 'ờ', 'ữ', 'ặ', 'đ', 'Ầ', 'Ế', 'Đ'] {
            let s = char_to_emit(c).expect(&format!("missing: {c}"));
            assert!(s.keycode <= 255, "{c} keycode {} > 255", s.keycode);
        }
    }
}
