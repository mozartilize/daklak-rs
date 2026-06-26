//! Synthetic xkb keymap data + text generator, plus a thin xkbcommon
//! wrapper (`XkbState`) shared across adapters.
//!
//! Pure data crate — no Wayland or evdev dependencies. Used by both:
//!
//! - `viet-ime-wayland-adapter` Tier 4 (uploads `keymap_text()` to
//!   `zwp_virtual_keyboard_v1.keymap()` via memfd).
//! - `viet-ime-daemon` evdev-only mode (writes `keymap_text()` to a
//!   `.xkb` file, points sway/scroll at it via `swaymsg input … xkb_file`).
//!
//! Vietnamese precomposed chars live in custom slots packed across 17
//! evdev keycodes (all ≤ 255 so XWayland clients can receive them; see
//! `SAFE_KEYCODES`) using **EIGHT_LEVEL** xkb type. Each slot holds 4
//! Vietnamese lowercase letters interleaved with their uppercase mirrors:
//!
//! | Level | Modifier            | Wayland mod bits  |
//! |-------|---------------------|-------------------|
//! | 1     | (none)              | 0                 |
//! | 2     | Shift               | 0x01              |
//! | 3     | LevelThree (AltGr)  | 0x80 (Mod5)       |
//! | 4     | Shift + LevelThree  | 0x81              |
//! | 5     | LevelFive           | 0x20 (Mod3)       |
//! | 6     | Shift + LevelFive   | 0x21              |
//! | 7     | LevelThree+LevelFive| 0xa0              |
//! | 8     | Shift+L3+L5         | 0xa1              |
//!
//! Standard QWERTY layout is preserved via `include "pc+us+inet(evdev)"`,
//! so daklak can emit plain ASCII via standard evdev codes through the
//! same keymap.

pub mod xkb;

/// First evdev keycode in the slot inventory. Kept as a stable symbol
/// for tests + telemetry; the actual keycode of slot N is
/// `SAFE_KEYCODES[N]` (non-contiguous mapping).
pub const BASE_EVDEV: u32 = SAFE_KEYCODES[0] as u32;

/// Evdev keycodes for the 17 Vietnamese precomposed EIGHT_LEVEL slots.
///
/// **Why all ≤ 255**: fits the X11 8-bit keycode field, so XWayland
/// clients (firefox-x11, IntelliJ, anything Xorg-only) receive the
/// keysyms our custom keymap binds. The previous KEY_MACRO range
/// (656-699) silently dropped on the XWayland boundary; verified via
/// `tools/xkb-probe/probe-eight-level.{xkb,py}` and `vk_xkb_probe_eight`.
///
/// **Why this kc set**: two zones with no userspace handler and no
/// presence on US/EU keyboards:
///
/// - **IME zone** (kc 85,86,89-95): `KEY_ZENKAKUHANKAKU`, `KEY_102ND`,
///   `KEY_RO`, `KEY_KATAKANA`, `KEY_HIRAGANA`, `KEY_KATAKANAHIRAGANA`,
///   `KEY_MUHENKAN`, `KEY_KPJPCOMMA`. `KEY_HENKAN` (92) is reserved
///   below as the `ISO_Level5_Shift` modifier_map binding.
///
/// - **F13-F21** (kc 183-191): extended-function range. Default
///   desktops have no bindings. F22-F24 (192-194) intentionally
///   excluded for users with extended keyboards.
///
/// **Hostile-code dodge** (still relevant — only the rationale shifted
/// from "above 255" to "skip these specific kc"):
///
/// - 205 `KEY_SUSPEND` → systemd-logind hibernates the host.
/// - 224/225 `KEY_BRIGHTNESS{DOWN,UP}` → screen brightness.
/// - 227 `KEY_SWITCHVIDEOMODE` → display swap.
/// - 236 `KEY_BATTERY` → upower power query.
/// - 237 `KEY_BLUETOOTH` → bluez radio toggle.
/// - 238 `KEY_WLAN` → NetworkManager/rfkill disables WiFi.
/// - 245 `KEY_DISPLAY_OFF` → DPMS blank.
/// - 246 `KEY_WWAN`, 247 `KEY_RFKILL` → wireless toggles.
///
/// **Tradeoff — real-keyboard collision**: Japanese keyboards produce
/// kc 85-95; some EU keyboards have `KEY_102ND` (86); extended kbs
/// have F13+. If a user fires one of these on their real device, the
/// real-kb's keymap decodes it (their pc+us+inet etc., NOT daklak's
/// per-device keymap). Daklak's emit windows are guarded by EVIOCGRAB
/// (evdev-only mode) or the wayland keyboard grab (wayland-mode), so
/// no race during commit.
pub const SAFE_KEYCODES: &[u16] = &[
    // IME zone (kc 85,86,89-95 with 92=HENKAN reserved for L5 mod binding).
    85, 86, 89, 90, 91, 93, 94, 95,
    // F13-F19 = 183..189. F20 (190) and F21 (191) excluded — empirically
    // some X11 sessions (observed on xfce4) silently filter X11 keycode
    // 198/199 even when a matching keysym is bound. We don't know which
    // daemon swallows them, so we steer clear; the slot count budget is
    // still met using two non-F slots below.
    183, 184, 185, 186, 187, 188, 189,
    // KEY_HANGEUL (122) and KEY_HANJA (123) — Korean IME keys, no US
    // keyboard produces them, no system daemon listens. Replace the
    // dropped F20/F21 slots one-for-one.
    122, 123,
];

/// Standard xkb key names for each slot in `SAFE_KEYCODES`, parallel by
/// index. The `evdev+aliases(qwerty)` include already binds these names
/// at the same X11 keycodes — we use them directly in `xkb_symbols`
/// (overriding the include's default bindings) instead of inventing new
/// `<DK##>` aliases. Reusing the existing names avoids the redefinition
/// warnings `xkbcomp … $DISPLAY` emits when loading session-wide on X11.
pub const SAFE_KEYCODE_NAMES: &[&str] = &[
    "ZEHA", "LSGT", "AB11", "KATA", "HIRA", "HKTG", "MUHE", "JPCM",
    "FK13", "FK14", "FK15", "FK16", "FK17", "FK18", "FK19",
    "HNGL", "HJCV",
];

const _: () = assert!(SAFE_KEYCODES.len() == SAFE_KEYCODE_NAMES.len());

/// Evdev keycode bound to `ISO_Level5_Shift` in our keymap via
/// `modifier_map Mod3 { <HENK> }`. Daklak never physically presses this
/// — wayland-mode addresses Mod3 via `vk.modifiers(0x20, …)`, evdev-only
/// mode emits a synthetic press of this kc through uinput before the
/// slot keycode. Reserved to avoid SAFE_KEYCODES collision.
pub const LEVEL5_SHIFT_EVDEV: u16 = 92; // KEY_HENKAN

/// Wayland modifier bits (mods_depressed) used to address xkb levels.
pub const MOD_SHIFT: u32 = 0x01;
/// LevelThree is bound to Mod5 (AltGr) by the standard `pc+us+inet(evdev)`
/// layout we include. Setting this bit selects xkb level 3.
pub const MOD_LEVEL3: u32 = 0x80;
/// LevelFive is bound to Mod3 in our keymap via `LEVEL5_SHIFT_EVDEV` →
/// `ISO_Level5_Shift`. Setting this bit selects xkb level 5+ in our
/// EIGHT_LEVEL custom slots. Real-keyboard apps generally do not use
/// Mod3, so it stays free for daklak.
pub const MOD_LEVEL5: u32 = 0x20;

/// Specification for emitting `c` through `zwp_virtual_keyboard_v1::key()`
/// or `uinput`. `mods` is the OR-mask the caller arranges (vk.modifiers
/// or uinput modifier press) before pressing `keycode`. Caller restores
/// the previous modifier state afterwards.
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

/// Number of EIGHT_LEVEL slots needed to pack all Vietnamese chars.
/// `ceil(VN_LOWER.len() / 4)` since each slot holds 4 lowercase letters
/// (at L1, L3, L5, L7) plus their uppercase mirrors (L2, L4, L6, L8).
pub const fn slots_needed() -> usize {
    (VN_LOWER.len() + 3) / 4
}

const _: () = assert!(SAFE_KEYCODES.len() == slots_needed());

/// Evdev keycode for slot `slot`. Indexes into `SAFE_KEYCODES`.
#[inline]
pub const fn slot_keycode(slot: usize) -> u16 {
    SAFE_KEYCODES[slot]
}

/// Every evdev keycode daklak emits for Vietnamese chars. `UinputDevice`
/// passes each through `UI_SET_KEYBIT` — kernel drops emits for codes
/// not in the device capability set.
pub fn daklak_slot_keycodes() -> &'static [u16] {
    SAFE_KEYCODES
}

/// Mods to set on `vk.modifiers()` to address level `lv` (1..=8).
const fn level_mods(lv: u8) -> u32 {
    match lv {
        1 => 0,
        2 => MOD_SHIFT,
        3 => MOD_LEVEL3,
        4 => MOD_SHIFT | MOD_LEVEL3,
        5 => MOD_LEVEL5,
        6 => MOD_SHIFT | MOD_LEVEL5,
        7 => MOD_LEVEL3 | MOD_LEVEL5,
        8 => MOD_SHIFT | MOD_LEVEL3 | MOD_LEVEL5,
        _ => 0,
    }
}

/// Locate `c` in the Vietnamese inventory and return (slot index, level).
/// Packing: 4 lowercase chars per slot at L1, L3, L5, L7 (odd levels),
/// uppercase mirrors at L2, L4, L6, L8 (even levels).
fn locate_vn(c: char) -> Option<(usize, u8)> {
    if let Some(i) = VN_LOWER.iter().position(|&x| x == c) {
        let slot = i / 4;
        let level: u8 = ((i % 4) as u8) * 2 + 1; // 1, 3, 5, 7
        Some((slot, level))
    } else if let Some(i) = VN_UPPER.iter().position(|&x| x == c) {
        let slot = i / 4;
        let level: u8 = ((i % 4) as u8) * 2 + 2; // 2, 4, 6, 8
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
/// `zwp_virtual_keyboard_v1::key()` or uinput. Tries Vietnamese custom
/// slots first, then falls through to the standard ASCII layout. `None`
/// only for chars outside both inventories.
pub fn char_to_emit(c: char) -> Option<EmitSpec> {
    if let Some((slot, level)) = locate_vn(c) {
        return Some(EmitSpec {
            keycode: slot_keycode(slot) as u32,
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

/// Build the daklak synthetic xkb keymap text. Used both for the
/// wayland-mode `zwp_virtual_keyboard_v1` keymap upload (memfd) and for
/// the evdev-only mode `swaymsg input … xkb_file <path>` activation.
pub fn keymap_text() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("xkb_keymap {\n");
    // ── keycodes — reuse the standard evdev keycode names ────────────────
    // No new keycodes are declared: every slot in SAFE_KEYCODES reuses an
    // xkb name already defined by `evdev+aliases(qwerty)` (see
    // SAFE_KEYCODE_NAMES). The symbols block below overrides those keys
    // with our EIGHT_LEVEL Vietnamese bindings.
    s.push_str("  xkb_keycodes \"evdev+daklak\" {\n");
    s.push_str("    include \"evdev+aliases(qwerty)\"\n");
    s.push_str("  };\n");
    // ── xkb types & compat (LevelThree+LevelFive bindings via complete) ───
    s.push_str("  xkb_types \"complete\" { include \"complete\" };\n");
    s.push_str("  xkb_compat \"complete\" { include \"complete\" };\n");
    // ── symbols — standard pc+us layout + EIGHT_LEVEL custom slots ────────
    append_symbols_section(&mut s, "  ", "pc+us+daklak");
    s.push_str("};\n");
    s
}

/// Build a standalone xkb_symbols fragment for installation under
/// `<datadir>/X11/xkb/symbols/daklak_vn`.
pub fn symbols_text() -> String {
    let mut s = String::with_capacity(8 * 1024);
    s.push_str("default partial alphanumeric_keys\n");
    append_symbols_section(&mut s, "", "basic");
    s
}

fn append_symbols_section(s: &mut String, indent: &str, section: &str) {
    s.push_str(&format!("{indent}xkb_symbols \"{section}\" {{\n"));
    s.push_str(&format!("{indent}  include \"pc+us+inet(evdev)\"\n"));
    // Group name override — lets evdev-only mode probe whether scroll/sway
    // actually loaded our keymap (vs falling back to its default "English
    // (US)" for daklak's uinput device).
    s.push_str(&format!("{indent}  name[Group1] = \"Daklak Vietnamese\";\n"));
    // Bake lv3:ralt_switch into the keymap so RightAlt selects Level3
    // (Mod5). The standard us-evdev include binds RALT to Mod1 (= Alt),
    // which makes daklak's emit dance for Vietnamese L3 chars appear to
    // apps as Alt+keycode (a hotkey) instead of selecting L3 of the
    // EIGHT_LEVEL DK slot. Without this, 'â', 'á', 'ấ', etc. silently
    // drop because the compositor never enters Level3 mode for the emit.
    s.push_str(&format!("{indent}  replace key <RALT> {{ type[Group1] = \"ONE_LEVEL\", [ ISO_Level3_Shift ] }};\n"));
    s.push_str(&format!("{indent}  modifier_map Mod5 {{ <RALT> }};\n"));
    // LevelFive selector — HENK (KEY_HENKAN, evdev kc 92) is bound to
    // ISO_Level5_Shift and routed to Mod3. Daklak addresses levels 5-8
    // via Mod3 (= 0x20 in vk.modifiers depressed mask), or by pressing
    // <HENK> through uinput before pressing the slot keycode in
    // evdev-only mode. Real users with Japanese keyboards may hit
    // KEY_HENKAN physically — that fires through their real-kb keymap,
    // which doesn't have this binding.
    s.push_str(&format!("{indent}  replace key <HENK> {{ type[Group1] = \"ONE_LEVEL\", [ ISO_Level5_Shift ] }};\n"));
    s.push_str(&format!("{indent}  modifier_map Mod3 {{ <HENK> }};\n"));
    append_symbol_rows(s, indent);
    s.push_str(&format!("{indent}}};\n"));
}

fn append_symbol_rows(s: &mut String, indent: &str) {
    for i in 0..slots_needed() {
        let base = i * 4;
        // Collect up to 4 lower/upper pairs, padding shortfall with VoidSymbol.
        let mut keysyms: [u32; 8] = [0xFFFFFF; 8];
        for j in 0..4 {
            let idx = base + j;
            if idx < VN_LOWER.len() {
                keysyms[j * 2] = VN_LOWER[idx] as u32;
                keysyms[j * 2 + 1] = VN_UPPER[idx] as u32;
            }
        }
        let kc_name = SAFE_KEYCODE_NAMES[i];
        // Format each keysym slot: VoidSymbol for the padding sentinel,
        // U+XXXX literal otherwise.
        let mut row = String::with_capacity(96);
        for (k, &sym) in keysyms.iter().enumerate() {
            if k > 0 {
                row.push_str(", ");
            }
            if sym == 0xFFFFFF {
                row.push_str("VoidSymbol");
            } else {
                row.push_str(&format!("U{:04X}", sym));
            }
        }
        // `override` ensures the include's default symbol (e.g. F13 for
        // <FK13>) is replaced rather than merged — without it xkbcomp
        // emits "Multiple symbols for level 1/group 1" warnings.
        s.push_str(&format!(
            "{indent}  replace key <{}> {{ type[Group1] = \"EIGHT_LEVEL\", [ {} ] }};\n",
            kc_name, row
        ));
    }
}

/// Plan the modifier dance for one `emit_char` invocation. Public so both
/// `AdapterSink::vk_commit_char` and the evdev-only sink reuse the same
/// helper.
///
/// - `dep` is the user's currently-depressed modifier mask.
/// - `spec_mods` is the level-selecting mask the synthetic keymap demands.
///
/// Returns `Some((emit_mask, restore_mask))` if a dance is needed; `None`
/// when the target char sits at L1 of its key.
pub fn plan_mod_dance(dep: u32, spec_mods: u32) -> Option<(u32, u32)> {
    if spec_mods == 0 {
        None
    } else {
        Some((dep | spec_mods, dep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_under_kernel_key_max() {
        // KEY_MAX = 0x2FF = 767 in <linux/input-event-codes.h>.
        let max_kc = *SAFE_KEYCODES.iter().max().unwrap() as u32;
        assert!(max_kc <= 767,
            "Vietnamese slots overflowed KEY_MAX: max kc = {max_kc}");
    }

    #[test]
    fn safe_keycodes_skip_known_hostile_codes() {
        // Every code below 255 has a userspace handler that grabs it
        // independent of xkb. We sidestep all of them by living in the
        // KEY_MACRO range; this test guards against accidental fallback.
        for hostile in [
            116u16, 142, 143, // power/sleep/wakeup
            205,              // suspend
            224, 225,         // brightness
            227,              // switch video
            236, 237, 238, 239, // battery / bluetooth / wlan / uwb
            245, 246, 247,    // display-off / wwan / rfkill
        ] {
            assert!(
                !SAFE_KEYCODES.contains(&hostile),
                "hostile keycode {hostile} leaked into SAFE_KEYCODES"
            );
        }
    }

    #[test]
    fn safe_keycodes_under_x11_8bit_ceiling() {
        // The whole point of B3: every slot kc must fit the X11 keycode
        // field (8 bits, so kc + 8 ≤ 255). With max evdev kc = 191
        // (F21), XWayland kc = 199, comfortably under.
        for &kc in SAFE_KEYCODES {
            assert!(
                kc <= 247,
                "keycode {kc} exceeds X11 8-bit reachable range (kc+8 must be ≤ 255)"
            );
        }
    }

    #[test]
    fn safe_keycodes_in_expected_zones() {
        // IME zone (85-95 excluding 92), F13-F19 (183-189), or Korean IME
        // pair (KEY_HANGEUL=122, KEY_HANJA=123). F20/F21 deliberately
        // dropped — see SAFE_KEYCODES doc.
        for &kc in SAFE_KEYCODES {
            let in_ime = (85..=95).contains(&kc) && kc != 92;
            let in_f_keys = (183..=189).contains(&kc);
            let in_korean = kc == 122 || kc == 123;
            assert!(
                in_ime || in_f_keys || in_korean,
                "keycode {kc} outside expected IME / F13-F19 / Korean safe zones"
            );
        }
    }

    #[test]
    fn level5_shift_evdev_not_in_safe_keycodes() {
        // The Mod3/ISO_Level5_Shift binding kc must NOT also be a viet slot.
        assert!(
            !SAFE_KEYCODES.contains(&LEVEL5_SHIFT_EVDEV),
            "LEVEL5_SHIFT_EVDEV ({LEVEL5_SHIFT_EVDEV}) leaked into SAFE_KEYCODES"
        );
    }

    #[test]
    fn vietnamese_lookup_returns_correct_level() {
        // Slot 0 packs VN_LOWER[0..4] at levels 1, 3, 5, 7. Verify each
        // level dispatches to the correct mod mask.
        let base = BASE_EVDEV;
        let s = char_to_emit(VN_LOWER[0]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: 0 });
        let s = char_to_emit(VN_UPPER[0]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_SHIFT });
        let s = char_to_emit(VN_LOWER[1]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_LEVEL3 });
        let s = char_to_emit(VN_UPPER[1]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_SHIFT | MOD_LEVEL3 });
        let s = char_to_emit(VN_LOWER[2]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_LEVEL5 });
        let s = char_to_emit(VN_UPPER[2]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_SHIFT | MOD_LEVEL5 });
        let s = char_to_emit(VN_LOWER[3]).unwrap();
        assert_eq!(s, EmitSpec { keycode: base, mods: MOD_LEVEL3 | MOD_LEVEL5 });
        let s = char_to_emit(VN_UPPER[3]).unwrap();
        assert_eq!(
            s,
            EmitSpec { keycode: base, mods: MOD_SHIFT | MOD_LEVEL3 | MOD_LEVEL5 }
        );
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

    const SHIFT: u32 = 0x01;
    const CTRL: u32 = 0x04;
    const ALT: u32 = 0x08;
    const LEVEL5: u32 = 0x20;
    const ALTGR: u32 = 0x80;
    const SHIFT_ALTGR: u32 = SHIFT | ALTGR;

    #[test]
    fn no_dance_when_spec_mods_zero() {
        assert_eq!(plan_mod_dance(0, 0), None);
        assert_eq!(plan_mod_dance(SHIFT, 0), None);
        assert_eq!(plan_mod_dance(CTRL | ALT, 0), None);
    }

    #[test]
    fn dance_l2_shift_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, SHIFT), Some((SHIFT, 0)));
    }

    #[test]
    fn dance_l3_altgr_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, ALTGR), Some((ALTGR, 0)));
    }

    #[test]
    fn dance_l4_shift_altgr_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, SHIFT_ALTGR), Some((SHIFT_ALTGR, 0)));
    }

    #[test]
    fn dance_or_combines_user_shift_with_spec_altgr() {
        assert_eq!(plan_mod_dance(SHIFT, ALTGR), Some((SHIFT_ALTGR, SHIFT)));
    }

    #[test]
    fn dance_preserves_ctrl_in_emit_and_restore() {
        assert_eq!(
            plan_mod_dance(CTRL, SHIFT_ALTGR),
            Some((CTRL | SHIFT_ALTGR, CTRL))
        );
    }

    #[test]
    fn dance_ctrl_shift_held_with_spec_altgr() {
        assert_eq!(
            plan_mod_dance(CTRL | SHIFT, ALTGR),
            Some((CTRL | SHIFT | ALTGR, CTRL | SHIFT))
        );
    }

    #[test]
    fn dance_alt_held_with_spec_shift() {
        assert_eq!(plan_mod_dance(ALT, SHIFT), Some((ALT | SHIFT, ALT)));
    }

    #[test]
    fn dance_user_already_holding_spec_mods_still_dances() {
        assert_eq!(
            plan_mod_dance(SHIFT_ALTGR, SHIFT_ALTGR),
            Some((SHIFT_ALTGR, SHIFT_ALTGR))
        );
    }

    #[test]
    fn telex_coverage_under_x11_ceiling() {
        // Every Vietnamese char engine commits in common Telex words must
        // resolve to a keycode XWayland can deliver. With B3 low-kc, the
        // X11 keycode field (8 bits, so evdev kc + 8 ≤ 255) covers
        // every slot in SAFE_KEYCODES — XWayland clients now receive
        // these chars alongside native-Wayland ones.
        for c in ['à', 'ầ', 'ế', 'ờ', 'ữ', 'ặ', 'đ', 'Ầ', 'Ế', 'Đ'] {
            let s = char_to_emit(c).expect(&format!("missing: {c}"));
            assert!(s.keycode <= 247, "{c} keycode {} > 247 (X11 ceiling)", s.keycode);
        }
    }

    #[test]
    fn dance_l5_levelfive_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, LEVEL5), Some((LEVEL5, 0)));
    }

    #[test]
    fn dance_l8_all_three_mods() {
        let l8 = SHIFT | LEVEL5 | ALTGR;
        assert_eq!(plan_mod_dance(0, l8), Some((l8, 0)));
    }

    #[test]
    fn dance_preserves_user_ctrl_with_l5_spec() {
        assert_eq!(plan_mod_dance(CTRL, LEVEL5), Some((CTRL | LEVEL5, CTRL)));
    }

    #[test]
    fn symbols_text_contains_the_expected_wrapper() {
        let text = symbols_text();
        assert!(text.starts_with("default partial alphanumeric_keys\n"));
        assert!(text.contains("xkb_symbols \"basic\" {\n"));
        assert!(text.contains("name[Group1] = \"Daklak Vietnamese\";"));
    }

    #[test]
    fn symbols_and_keymap_share_slot_rows() {
        fn slot_rows(text: &str) -> Vec<String> {
            text.lines()
                .map(str::trim_start)
                .filter(|l| l.starts_with("replace key <"))
                .map(str::to_owned)
                .collect()
        }

        assert_eq!(slot_rows(&keymap_text()), slot_rows(&symbols_text()));
    }
}
