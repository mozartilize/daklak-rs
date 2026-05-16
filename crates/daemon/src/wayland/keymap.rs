//! Synthetic xkb keymap with Vietnamese precomposed chars in spare evdev
//! slots. Wired into `zwp_virtual_keyboard_v1.keymap()` so daklak can emit
//! Vietnamese chars via raw `vk_key()` events — no `commit_string`, no
//! `zwp_text_input_v3`. Works for any client that reads `wl_keyboard`
//! (Qt5/XWayland-via-vk/tui-in-terminal — all the cases where the
//! input-method-v2 activate never fires).
//!
//! Standard QWERTY layout is preserved via `include "evdev+aliases(qwerty)"`
//! and `include "pc+us+inet(evdev)"`, so existing tiers that emit `vk_key`
//! for evdev BackSpace (14) keep working unchanged. Vietnamese chars sit
//! at evdev `BASE_EVDEV..BASE_EVDEV+N`.
//!
//! **Hazard — do not route Chromium-class apps here.** Chromium's renderer
//! has its own hard-coded `LinuxKeyCode → DomCode` table for keyboard
//! event introspection. Evdev codes 200+ are KEY_KBDILLUMUP / KEY_FN_F*
//! / etc in that table — feeding them in attached to Unicode Vietnamese
//! keysyms (e.g. evdev 207 + U+1EA7) crashes the tab. Chromium has
//! `zwp_text_input_v3` (just commit-flaky on Tier 1/2); use
//! `force_uinput_apps` for it instead — kernel BS + `commit_string`
//! delivers via the legitimate text_input_v3 session once it's hot.
//! Path C / VkOnly is only safe for clients that do NOT advertise
//! text_input_v3 at all and read `wl_keyboard` as a vanilla keyboard:
//! Qt5 (KeePassXC), most XWayland clients, tui-in-pty.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use xkbcommon::xkb::{
    Context as XkbCtx, Keymap, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS,
    KEYMAP_FORMAT_TEXT_V1,
};

/// First evdev keycode allocated to daklak's Vietnamese chars. 200 is well
/// above OEM keys (∼190) and below the KEY_FN/BTN_* ranges (240+).
pub const BASE_EVDEV: u32 = 200;

/// Characters daklak emits in commits — Vietnamese precomposed PLUS the
/// plain ASCII that engine commits routinely include (`ần` ends with `n`,
/// `ết` ends with `t`, restorations like `oow` → `ow` end with letters).
/// Order is stable; index N → evdev keycode `BASE_EVDEV + N`. Every entry
/// gets its own dedicated keycode in daklak's synthesized layout so
/// `vk_commit_char` never needs Shift modifier dancing for uppercase
/// variants or commit_string fallbacks for plain letters.
const VN_CHARS: &[char] = &[
    // ── Vietnamese precomposed — lowercase ─────────────────────────────────
    // a-family — grave/acute/hook/tilde/dot
    'à', 'á', 'ả', 'ã', 'ạ',
    'â', 'ầ', 'ấ', 'ẩ', 'ẫ', 'ậ',
    'ă', 'ằ', 'ắ', 'ẳ', 'ẵ', 'ặ',
    // e-family
    'è', 'é', 'ẻ', 'ẽ', 'ẹ',
    'ê', 'ề', 'ế', 'ể', 'ễ', 'ệ',
    // i-family (no roof/breve in Vietnamese)
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
    // ── Vietnamese precomposed — uppercase ─────────────────────────────────
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
    // ── Plain ASCII letters ────────────────────────────────────────────────
    // Engine commits often include these as restoration/non-converted
    // tails: `ần` (n), `ết` (t), `oow → ow` (w), capitalized forms, etc.
    // Each gets a dedicated daklak keycode → vk_commit_char never falls
    // through to commit_string.
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j',
    'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't',
    'u', 'v', 'w', 'x', 'y', 'z',
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J',
    'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T',
    'U', 'V', 'W', 'X', 'Y', 'Z',
    // ── Digits + common punct (rare but cheap to include) ──────────────────
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
    ' ', '.', ',', ';', ':', '!', '?', '\'', '"', '-',
    '_', '(', ')', '[', ']', '/', '\\',
];

/// evdev keycode that produces `c` via daklak's synthesized keymap, or
/// `None` if `c` isn't in the precomposed inventory (caller falls back to
/// `commit_string`).
pub fn char_to_evdev(c: char) -> Option<u32> {
    VN_CHARS.iter().position(|&v| v == c).map(|i| BASE_EVDEV + i as u32)
}

/// Total number of Vietnamese chars daklak maps to evdev slots.
pub fn inventory_len() -> usize {
    VN_CHARS.len()
}

fn keymap_text() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("xkb_keymap {\n");
    s.push_str("  xkb_keycodes \"evdev+daklak\" {\n");
    s.push_str("    include \"evdev+aliases(qwerty)\"\n");
    let max_xkb = BASE_EVDEV + VN_CHARS.len() as u32 + 8 + 8; // headroom
    s.push_str(&format!("    maximum = {};\n", max_xkb));
    for i in 0..VN_CHARS.len() {
        let xkb_kc = BASE_EVDEV + i as u32 + 8;
        s.push_str(&format!("    <DK{:03}> = {};\n", i, xkb_kc));
    }
    s.push_str("  };\n");
    s.push_str("  xkb_types \"complete\" { include \"complete\" };\n");
    s.push_str("  xkb_compat \"complete\" { include \"complete\" };\n");
    s.push_str("  xkb_symbols \"pc+us+daklak\" {\n");
    s.push_str("    include \"pc+us+inet(evdev)\"\n");
    for (i, c) in VN_CHARS.iter().enumerate() {
        s.push_str(&format!(
            "    key <DK{:03}> {{ [ U{:04X} ] }};\n",
            i, *c as u32
        ));
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

    // Validate locally — if our own libxkbcommon can't parse this, the
    // compositor's can't either. Catch typos before they trip the IM.
    let ctx = XkbCtx::new(CONTEXT_NO_FLAGS);
    let _ = Keymap::new_from_string(
        &ctx,
        text.clone(),
        KEYMAP_FORMAT_TEXT_V1,
        KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow!("xkbcommon rejected synthesized daklak keymap"))?;

    let name = CString::new("daklak-keymap").unwrap();
    // SAFETY: memfd_create is a stable Linux syscall (>= 3.17). flags=0 →
    // non-cloexec/non-sealable; that's fine since the compositor only
    // needs to mmap once.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: raw is a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // xkbcommon convention: keymap fd is NUL-terminated.
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
    fn char_to_evdev_known() {
        // First Vietnamese char anchors BASE_EVDEV — keep this stable so
        // the synthesized keymap text stays diff-friendly.
        assert_eq!(char_to_evdev('à'), Some(BASE_EVDEV));
        // Every entry resolves to a unique keycode in
        // BASE_EVDEV..BASE_EVDEV+inventory_len.
        assert!(char_to_evdev('Đ').is_some());
        let kc = char_to_evdev('Đ').unwrap();
        assert!(kc >= BASE_EVDEV && kc < BASE_EVDEV + inventory_len() as u32);
    }

    #[test]
    fn char_to_evdev_ascii_covered() {
        // Engine commits often have ASCII tails (`ần` ends `n`, `ết`
        // ends `t`). Without these in the inventory they fall through
        // to `commit_string` which Qt5/text-input-v3-missing clients
        // ignore → silently dropped letters.
        assert!(char_to_evdev('a').is_some());
        assert!(char_to_evdev('n').is_some());
        assert!(char_to_evdev('t').is_some());
        assert!(char_to_evdev('A').is_some());
        assert!(char_to_evdev(' ').is_some());
        assert!(char_to_evdev('0').is_some());
    }

    #[test]
    fn char_to_evdev_truly_unknown() {
        // Symbols outside the inventory still return None — caller logs
        // and falls back. Anything daklak's engine routinely emits is
        // in the inventory above.
        assert_eq!(char_to_evdev('☃'), None);
        assert_eq!(char_to_evdev('€'), None);
    }

    #[test]
    fn keymap_parses() {
        // build() validates via xkbcommon — if it returns Err here the
        // synthesized keymap is malformed.
        let km = build().expect("daklak keymap must parse");
        assert!(km.size > 100, "keymap looked suspiciously small");
    }

    #[test]
    fn inventory_has_full_telex_coverage() {
        // Spot-check chars vnkey-engine emits for common Telex words.
        for c in ['ầ', 'ế', 'ờ', 'ữ', 'ặ', 'đ', 'Ầ', 'Ế', 'Đ'] {
            assert!(char_to_evdev(c).is_some(), "missing: {c}");
        }
    }
}
