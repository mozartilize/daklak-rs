#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

//! Synthetic xkb keymap data + text generator, plus a thin xkbcommon
//! wrapper (`XkbState`) shared across adapters.
//!
//! Pure data crate — no Wayland or evdev dependencies. Used by both:
//!
//! - `viet-ime-wayland-adapter` ForwardKey synthetic-keymap channel (uploads
//!   `keymap_text()` to `zwp_virtual_keyboard_v1.keymap()` via memfd).
//! - `viet-ime-daemon` evdev mode (writes `keymap_text()` to a
//!   `.xkb` file, points sway/scroll at it via `swaymsg input … xkb_file`).
//! - KWin / Mutter sessions, through the `daklak:vn` xkb option
//!   (`symbols_text()` + `rules_text()`), where the compositor resolves the
//!   keysyms daklak sends against the session keymap.
//!
//! Vietnamese precomposed chars live on **levels 5-8 of ordinary typing
//! keys** (letters plus a few punctuation keys; see `SLOTS`). Levels 1-4 stay
//! whatever the user's layout puts there. Each slot key holds two Vietnamese
//! lowercase letters and their uppercase forms:
//!
//! | Level | Modifier             | Wayland mod bits | Content          |
//! |-------|----------------------|------------------|------------------|
//! | 1-4   | (layout's own)       |                  | untouched        |
//! | 5     | LevelFive            | 0x20 (Mod3)      | first lowercase  |
//! | 6     | Shift + LevelFive    | 0x21             | first uppercase  |
//! | 7     | LevelThree+LevelFive | 0xa0             | second lowercase |
//! | 8     | Shift+L3+L5          | 0xa1             | second uppercase |
//!
//! **Why ordinary typing keys.** Earlier designs parked the letters on
//! keycodes nobody types (Japanese/Korean IME keys, F13-F19, Sun editing
//! keys). Every client has its own idea of what such keys are: Chromium
//! drops codes missing from its DomCode table, physical-key terminals
//! (Ghostty ≥ 1.4) encode F13+ as escape sequences instead of the produced
//! text, desktop daemons grab others. Letter and punctuation keys are the
//! most-exercised path in every toolkit and never mean anything but text.
//!
//! **Key type.** Slot keys use the stock `EIGHT_LEVEL` type: Lock is not
//! consumed, so Caps Lock reaches clients as libxkbcommon's capitalization
//! transform (Caps + `ô` → `Ô`, Caps + `,` → `,`). That is correct for both
//! letters and for punctuation that sits on a "letter" position in other
//! layouts (AZERTY `,` on `<AB07>`, Dvorak `'` on `<AD01>`). The one
//! difference from an alphabetic key type: Shift + Caps Lock on a slot key
//! yields uppercase instead of lowercase.

pub mod xkb;

/// One Vietnamese slot: an ordinary typing key whose levels 5 and 7 carry
/// two lowercase letters (levels 6 and 8 carry their uppercase forms).
struct Slot {
    /// xkb key name (`evdev+aliases(qwerty)`).
    name: &'static str,
    /// Linux evdev keycode of that key.
    keycode: u16,
    /// Lowercase letters at level 5 and level 7.
    lower: [Option<char>; 2],
}

const fn slot(name: &'static str, keycode: u16, l5: char, l7: char) -> Slot {
    Slot { name, keycode, lower: [Some(l5), Some(l7)] }
}

/// The Vietnamese slot table.
///
/// Each family's base vowel key carries two letters of that family
/// (`â ă` on A, `ô ơ` on O, `ư ù` on U, `đ` on D, …), so anything that
/// ignores levels 5-8 degrades to the plain base letter for the most
/// common letters. The remaining tone-marked letters fill the other letter
/// keys in inventory order, then eight punctuation keys (`; ' [ ] , . / \`).
///
/// Constraints every slot key satisfies:
///
/// - **Standard typing key** present on every PC keyboard, with a stable
///   Chromium DomCode and no physical-key encoding in terminals.
/// - **evdev kc ≤ 247** so XWayland (8-bit X11 keycodes) can deliver it.
/// - **Not a digit key**: some layouts (AZERTY) put digits on Shift, and
///   digits carry layout-specific Caps Lock semantics.
/// - Not `<HENK>`, which is reserved for `ISO_Level5_Shift`.
const SLOTS: &[Slot] = &[
    // Family base keys.
    slot("AC01", 30, 'â', 'ă'), // a
    slot("AD03", 18, 'ê', 'è'), // e
    slot("AD08", 23, 'ì', 'í'), // i
    slot("AD09", 24, 'ô', 'ơ'), // o
    slot("AD07", 22, 'ư', 'ù'), // u
    slot("AD06", 21, 'ỳ', 'ý'), // y
    Slot { name: "AC03", keycode: 32, lower: [Some('đ'), None] }, // d
    // Remaining letters on the other letter keys.
    slot("AD01", 16, 'à', 'á'), // q
    slot("AD02", 17, 'ả', 'ã'), // w
    slot("AD04", 19, 'ạ', 'ầ'), // r
    slot("AD05", 20, 'ấ', 'ẩ'), // t
    slot("AD10", 25, 'ẫ', 'ậ'), // p
    slot("AC02", 31, 'ằ', 'ắ'), // s
    slot("AC04", 33, 'ẳ', 'ẵ'), // f
    slot("AC05", 34, 'ặ', 'é'), // g
    slot("AC06", 35, 'ẻ', 'ẽ'), // h
    slot("AC07", 36, 'ẹ', 'ề'), // j
    slot("AC08", 37, 'ế', 'ể'), // k
    slot("AC09", 38, 'ễ', 'ệ'), // l
    slot("AB01", 44, 'ỉ', 'ĩ'), // z
    slot("AB02", 45, 'ị', 'ò'), // x
    slot("AB03", 46, 'ó', 'ỏ'), // c
    slot("AB04", 47, 'õ', 'ọ'), // v
    slot("AB05", 48, 'ồ', 'ố'), // b
    slot("AB06", 49, 'ổ', 'ỗ'), // n
    slot("AB07", 50, 'ộ', 'ờ'), // m
    // Punctuation keys.
    slot("AC10", 39, 'ớ', 'ở'), // ;
    slot("AC11", 40, 'ỡ', 'ợ'), // '
    slot("AD11", 26, 'ú', 'ủ'), // [
    slot("AD12", 27, 'ũ', 'ụ'), // ]
    slot("AB08", 51, 'ừ', 'ứ'), // ,
    slot("AB09", 52, 'ử', 'ữ'), // .
    slot("AB10", 53, 'ự', 'ỷ'), // /
    slot("BKSL", 43, 'ỹ', 'ỵ'), // \
];

/// Evdev keycode of the first slot. Kept as a stable symbol for downstream
/// re-exports and telemetry.
pub const BASE_EVDEV: u32 = SLOTS[0].keycode as u32;

/// Evdev keycode bound to `ISO_Level5_Shift` in our keymap via
/// `modifier_map Mod3 { <HENK> }` (KEY_HENKAN). Wayland mode addresses Mod3
/// via `vk.modifiers(0x20, …)`; evdev mode emits a synthetic press of this kc
/// through uinput before the slot keycode. Never a slot key.
pub const LEVEL5_SHIFT_EVDEV: u16 = 92; // KEY_HENKAN

/// Wayland modifier bits (mods_depressed) used to address xkb levels.
pub const MOD_SHIFT: u32 = 0x01;
/// LevelThree is bound to Mod5 (AltGr) by the standard `pc` symbols we
/// include. Setting this bit selects xkb level 3.
pub const MOD_LEVEL3: u32 = 0x80;
/// LevelFive is bound to Mod3 (`<LVL5>` in `pc`, plus `<HENK>` in our
/// symbols). Setting this bit selects xkb level 5+ on the slot keys. Real
/// keyboard apps generally do not use Mod3, and GTK does not report it, so
/// clients see plain text, not a shortcut.
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

/// Vietnamese lowercase precomposed inventory.
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

/// Every evdev keycode daklak emits for Vietnamese chars. `UinputDevice`
/// passes each through `UI_SET_KEYBIT` — kernel drops emits for codes
/// not in the device capability set.
pub fn daklak_slot_keycodes() -> impl Iterator<Item = u16> {
    SLOTS.iter().map(|s| s.keycode)
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

/// Uppercase form of a Vietnamese lowercase letter from the inventory.
fn vn_upper(lower: char) -> Option<char> {
    let i = VN_LOWER.iter().position(|&x| x == lower)?;
    VN_UPPER.get(i).copied()
}

/// Locate `c` in the slot table and return (slot, level 5..=8).
fn locate_vn(c: char) -> Option<(&'static Slot, u8)> {
    for slot in SLOTS {
        for (j, lower) in slot.lower.iter().enumerate() {
            let Some(lower) = *lower else { continue };
            let base = 5 + (j as u8) * 2; // 5 or 7
            if c == lower {
                return Some((slot, base));
            }
            if Some(c) == vn_upper(lower) {
                return Some((slot, base + 1));
            }
        }
    }
    None
}

/// Map plain ASCII to its evdev keycode in the standard pc/us layout.
/// Caller adds `MOD_SHIFT` for uppercase / shifted punctuation. None
/// for chars outside the ASCII printable range.
fn ascii_evdev(c: char) -> Option<(u32, u32)> {
    ASCII_EVDEV
        .binary_search_by_key(&c, |row| row.0)
        .ok()
        .map(|i| (ASCII_EVDEV[i].1, ASCII_EVDEV[i].2))
}

const ASCII_EVDEV: &[(char, u32, u32)] = &[
    (' ', 57, 0),
    ('!', 2, MOD_SHIFT),
    ('"', 40, MOD_SHIFT),
    ('#', 4, MOD_SHIFT),
    ('$', 5, MOD_SHIFT),
    ('%', 6, MOD_SHIFT),
    ('&', 8, MOD_SHIFT),
    ('\'', 40, 0),
    ('(', 10, MOD_SHIFT),
    (')', 11, MOD_SHIFT),
    ('*', 9, MOD_SHIFT),
    ('+', 13, MOD_SHIFT),
    (',', 51, 0),
    ('-', 12, 0),
    ('.', 52, 0),
    ('/', 53, 0),
    ('0', 11, 0),
    ('1', 2, 0),
    ('2', 3, 0),
    ('3', 4, 0),
    ('4', 5, 0),
    ('5', 6, 0),
    ('6', 7, 0),
    ('7', 8, 0),
    ('8', 9, 0),
    ('9', 10, 0),
    (':', 39, MOD_SHIFT),
    (';', 39, 0),
    ('<', 51, MOD_SHIFT),
    ('=', 13, 0),
    ('>', 52, MOD_SHIFT),
    ('?', 53, MOD_SHIFT),
    ('@', 3, MOD_SHIFT),
    ('A', 30, MOD_SHIFT),
    ('B', 48, MOD_SHIFT),
    ('C', 46, MOD_SHIFT),
    ('D', 32, MOD_SHIFT),
    ('E', 18, MOD_SHIFT),
    ('F', 33, MOD_SHIFT),
    ('G', 34, MOD_SHIFT),
    ('H', 35, MOD_SHIFT),
    ('I', 23, MOD_SHIFT),
    ('J', 36, MOD_SHIFT),
    ('K', 37, MOD_SHIFT),
    ('L', 38, MOD_SHIFT),
    ('M', 50, MOD_SHIFT),
    ('N', 49, MOD_SHIFT),
    ('O', 24, MOD_SHIFT),
    ('P', 25, MOD_SHIFT),
    ('Q', 16, MOD_SHIFT),
    ('R', 19, MOD_SHIFT),
    ('S', 31, MOD_SHIFT),
    ('T', 20, MOD_SHIFT),
    ('U', 22, MOD_SHIFT),
    ('V', 47, MOD_SHIFT),
    ('W', 17, MOD_SHIFT),
    ('X', 45, MOD_SHIFT),
    ('Y', 21, MOD_SHIFT),
    ('Z', 44, MOD_SHIFT),
    ('[', 26, 0),
    ('\\', 43, 0),
    (']', 27, 0),
    ('^', 7, MOD_SHIFT),
    ('_', 12, MOD_SHIFT),
    ('`', 41, 0),
    ('a', 30, 0),
    ('b', 48, 0),
    ('c', 46, 0),
    ('d', 32, 0),
    ('e', 18, 0),
    ('f', 33, 0),
    ('g', 34, 0),
    ('h', 35, 0),
    ('i', 23, 0),
    ('j', 36, 0),
    ('k', 37, 0),
    ('l', 38, 0),
    ('m', 50, 0),
    ('n', 49, 0),
    ('o', 24, 0),
    ('p', 25, 0),
    ('q', 16, 0),
    ('r', 19, 0),
    ('s', 31, 0),
    ('t', 20, 0),
    ('u', 22, 0),
    ('v', 47, 0),
    ('w', 17, 0),
    ('x', 45, 0),
    ('y', 21, 0),
    ('z', 44, 0),
    ('{', 26, MOD_SHIFT),
    ('|', 43, MOD_SHIFT),
    ('}', 27, MOD_SHIFT),
    ('~', 41, MOD_SHIFT),
];

/// Compute the (keycode, mods) needed to deliver `c` through
/// `zwp_virtual_keyboard_v1::key()` or uinput. Tries Vietnamese slots
/// first, then falls through to the standard ASCII layout. `None` only for
/// chars outside both inventories.
pub fn char_to_emit(c: char) -> Option<EmitSpec> {
    if let Some((slot, level)) = locate_vn(c) {
        return Some(EmitSpec {
            keycode: slot.keycode as u32,
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
/// the evdev mode `swaymsg input … xkb_file <path>` activation.
pub fn keymap_text() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("xkb_keymap {\n");
    // No new keycodes: every slot is a standard key name already defined by
    // `evdev+aliases(qwerty)`.
    s.push_str("  xkb_keycodes \"evdev+daklak\" {\n");
    s.push_str("    include \"evdev+aliases(qwerty)\"\n");
    s.push_str("  };\n");
    // ── xkb types & compat (EIGHT_LEVEL + LevelFive come from complete) ───
    s.push_str("  xkb_types \"complete\" { include \"complete\" };\n");
    s.push_str("  xkb_compat \"complete\" { include \"complete\" };\n");
    // ── symbols — standard pc+us layout + Vietnamese levels 5-8 ──────────
    s.push_str("  xkb_symbols \"pc+us+daklak\" {\n");
    append_basic_body(&mut s, "  ");
    s.push_str("  };\n");
    s.push_str("};\n");
    s
}

/// Build a standalone xkb_symbols file for installation under
/// `<xkb base>/symbols/daklak_vn` (system `<datadir>/X11/xkb/symbols/` or
/// user-local `~/.config/xkb/symbols/`).
///
/// Emits three sections:
/// - **`basic`** (default): self-contained — includes `pc+us+inet(evdev)`
///   then applies the daklak additions. Used when `daklak_vn` is selected
///   as a *layout*.
/// - **`level_mods`**: binds `<RALT>` to `ISO_Level3_Shift` and `<HENK>` to
///   `ISO_Level5_Shift`, the physical keys evdev mode presses through uinput
///   to select levels 3 and 5. Merged in *override* mode by the `daklak:vn`
///   option.
/// - **`overlay`**: the Vietnamese levels 5-8 only. Merged in *augment*
///   mode by the `daklak:vn` option, so it fills the empty levels of the
///   user's own layout and never replaces a symbol the layout defines.
pub fn symbols_text() -> String {
    let mut s = String::with_capacity(12 * 1024);
    s.push_str("default partial alphanumeric_keys modifier_keys\n");
    s.push_str("xkb_symbols \"basic\" {\n");
    append_basic_body(&mut s, "");
    s.push_str("};\n\n");
    s.push_str("partial modifier_keys\n");
    s.push_str("xkb_symbols \"level_mods\" {\n");
    append_level_mods(&mut s, "");
    s.push_str("};\n\n");
    s.push_str("partial alphanumeric_keys\n");
    s.push_str("xkb_symbols \"overlay\" {\n");
    append_slot_rows(&mut s, "");
    s.push_str("};\n");
    s
}

/// Build the user-local xkb rules overlay for installation at
/// `~/.config/xkb/rules/evdev`. It pulls in the stock `evdev` ruleset and
/// registers the `daklak:vn` option (see `append_option_rules`). Works with
/// every libxkbcommon version.
pub fn rules_text() -> String {
    let mut s = String::with_capacity(768);
    s.push_str("// daklak xkb rules overlay — install at ~/.config/xkb/rules/evdev\n");
    s.push_str("// Pulls in the stock evdev ruleset, then registers the daklak:vn option.\n");
    s.push_str("! include %S/evdev\n");
    append_option_rules(&mut s);
    s
}

/// The `daklak:vn` option: applies `level_mods` (override) and `overlay`
/// (augment) to **every** configured layout, using the classic per-layout
/// `layout[N] option` sections (the same mechanism xkeyboard-config uses for
/// `misc:typo`). Option symbols are merged after the model's `inet(evdev)`,
/// so nothing re-clobbers them.
fn append_option_rules(s: &mut String) {
    s.push('\n');
    s.push_str("! layout option = symbols\n");
    s.push_str("  *  daklak:vn  =  +daklak_vn(level_mods)|daklak_vn(overlay)\n");
    for n in 1..=4 {
        s.push('\n');
        s.push_str(&format!("! layout[{n}] option = symbols\n"));
        s.push_str(&format!(
            "  *  daklak:vn  =  +daklak_vn(level_mods):{n}|daklak_vn(overlay):{n}\n"
        ));
    }
}

/// Body of the self-contained `basic` section (also used by the keymap
/// blob): the us base layout, a group name, the level selectors and the
/// Vietnamese levels.
fn append_basic_body(s: &mut String, indent: &str) {
    s.push_str(&format!("{indent}  include \"pc+us+inet(evdev)\"\n"));
    // Group name override — lets evdev mode probe whether scroll/sway
    // actually loaded our keymap (vs falling back to its default "English
    // (US)" for daklak's uinput device).
    s.push_str(&format!("{indent}  name[Group1] = \"Daklak Vietnamese\";\n"));
    append_level_mods(s, indent);
    append_slot_rows(s, indent);
}

fn append_level_mods(s: &mut String, indent: &str) {
    // RightAlt selects Level3 (Mod5). The us layout binds RALT to Alt_R
    // (Mod1), which would make evdev mode's level-7/8 emit look like
    // Alt+key (a hotkey) instead of selecting level 3.
    s.push_str(&format!("{indent}  replace key <RALT> {{ type[Group1] = \"ONE_LEVEL\", [ ISO_Level3_Shift ] }};\n"));
    s.push_str(&format!("{indent}  modifier_map Mod5 {{ <RALT> }};\n"));
    // LevelFive selector — HENK (KEY_HENKAN, evdev kc 92) is bound to
    // ISO_Level5_Shift and routed to Mod3, so evdev mode can press it
    // through uinput. (`pc` already maps the virtual `<LVL5>` key to Mod3.)
    s.push_str(&format!("{indent}  replace key <HENK> {{ type[Group1] = \"ONE_LEVEL\", [ ISO_Level5_Shift ] }};\n"));
    s.push_str(&format!("{indent}  modifier_map Mod3 {{ <HENK> }};\n"));
}

/// One `key` row per slot: `NoSymbol` on levels 1-4 (keeps the layout's own
/// symbols in every merge mode) and the Vietnamese pairs on levels 5-8.
fn append_slot_rows(s: &mut String, indent: &str) {
    for slot in SLOTS {
        let mut row = String::from("NoSymbol, NoSymbol, NoSymbol, NoSymbol");
        for lower in slot.lower {
            match lower.and_then(|l| Some((l, vn_upper(l)?))) {
                Some((l, u)) => row.push_str(&format!(", U{:04X}, U{:04X}", l as u32, u as u32)),
                None => row.push_str(", NoSymbol, NoSymbol"),
            }
        }
        s.push_str(&format!(
            "{indent}  key <{}> {{ type[Group1] = \"EIGHT_LEVEL\", symbols[Group1] = [ {row} ] }};\n",
            slot.name
        ));
    }
}

/// Plan the modifier dance for one `emit_char` invocation. Public so both
/// `AdapterSink::vk_commit_char` and the evdev sink reuse the same
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
    use std::collections::BTreeSet;
    use xkbcommon::xkb::{
        self as x, Context, Keycode, Keymap, State, CONTEXT_NO_DEFAULT_INCLUDES,
        CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1,
    };

    /// Every lowercase + uppercase Vietnamese char.
    fn all_vn() -> impl Iterator<Item = char> {
        VN_LOWER.iter().chain(VN_UPPER.iter()).copied()
    }

    #[test]
    fn slots_cover_inventory_exactly_once() {
        let mut seen = BTreeSet::new();
        for slot in SLOTS {
            for l in slot.lower.iter().flatten() {
                assert!(VN_LOWER.contains(l), "{l} is not in the inventory");
                assert!(seen.insert(*l), "{l} assigned twice");
            }
        }
        assert_eq!(seen.len(), VN_LOWER.len(), "every Vietnamese letter has a slot");
    }

    #[test]
    fn slot_keys_are_unique_ordinary_typing_keys() {
        // Letter rows (QWERTY…ASDF…ZXCV) plus `[ ] ; ' , . / \`. No digits,
        // no function/editing/IME keys, nothing above the X11 8-bit ceiling.
        let typing: BTreeSet<u16> = (16..=27).chain(30..=40).chain(43..=53).collect();
        let mut codes = BTreeSet::new();
        let mut names = BTreeSet::new();
        for slot in SLOTS {
            assert!(typing.contains(&slot.keycode), "{} kc {} is not a typing key", slot.name, slot.keycode);
            assert_ne!(slot.keycode, LEVEL5_SHIFT_EVDEV);
            assert!(codes.insert(slot.keycode), "kc {} used twice", slot.keycode);
            assert!(names.insert(slot.name), "{} used twice", slot.name);
        }
    }

    // ── compiled keymap checks (need the system xkeyboard-config data) ──

    const LOCK: u32 = 0x02;

    fn sym_char(state: &State, kc: u32) -> Option<char> {
        char::from_u32(x::keysym_to_utf32(state.key_get_one_sym(Keycode::new(kc + 8))))
            .filter(|c| *c != '\0')
    }

    fn type_char(keymap: &Keymap, kc: u32, depressed: u32, locked: u32, group: u32) -> Option<char> {
        let mut state = State::new(keymap);
        state.update_mask(depressed, 0, locked, 0, 0, group);
        sym_char(&state, kc)
    }

    fn blob_keymap() -> Keymap {
        let ctx = Context::new(CONTEXT_NO_FLAGS);
        Keymap::new_from_string(&ctx, keymap_text(), KEYMAP_FORMAT_TEXT_V1, KEYMAP_COMPILE_NO_FLAGS)
            .expect("keymap_text() compiles")
    }

    #[test]
    fn blob_delivers_every_char_from_its_emit_spec() {
        let keymap = blob_keymap();
        for c in all_vn().chain("az09AZ;'[],./\\".chars()) {
            let spec = char_to_emit(c).unwrap();
            assert_eq!(type_char(&keymap, spec.keycode, spec.mods, 0, 0), Some(c), "{c}");
        }
    }

    #[test]
    fn blob_keeps_us_levels_and_caps_lock() {
        let keymap = blob_keymap();
        assert_eq!(type_char(&keymap, 24, 0, 0, 0), Some('o'));
        assert_eq!(type_char(&keymap, 24, MOD_SHIFT, 0, 0), Some('O'));
        assert_eq!(type_char(&keymap, 51, MOD_SHIFT, 0, 0), Some('<'));
        // Caps Lock: letters capitalise, punctuation is untouched.
        assert_eq!(type_char(&keymap, 24, 0, LOCK, 0), Some('O'));
        assert_eq!(type_char(&keymap, 51, 0, LOCK, 0), Some(','));
        // Vietnamese levels follow the same capitalization transform.
        assert_eq!(type_char(&keymap, 24, MOD_LEVEL5, LOCK, 0), Some('Ô'));
        assert_eq!(type_char(&keymap, 24, MOD_SHIFT | MOD_LEVEL5, LOCK, 0), Some('Ô'));
    }

    /// Install `daklak gen-keymap --symbols/--rules` into a scratch xkb root
    /// and compile `layout(variant)` + `daklak:vn` through the real rules.
    struct OptionRoot(std::path::PathBuf);

    impl OptionRoot {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "daklak-keymap-test-{}-{tag}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("symbols")).unwrap();
            std::fs::create_dir_all(root.join("rules")).unwrap();
            std::fs::write(root.join("symbols/daklak_vn"), symbols_text()).unwrap();
            std::fs::write(root.join("rules/evdev"), rules_text()).unwrap();
            Self(root)
        }

        fn compile(&self, layout: &str, variant: &str, options: Option<String>) -> Keymap {
            let mut ctx = Context::new(CONTEXT_NO_DEFAULT_INCLUDES);
            assert!(ctx.include_path_append(&self.0));
            // System xkb paths only: the developer's own ~/.config/xkb or
            // ~/.xkb (possibly holding an older daklak install) must not leak
            // into the result.
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let defaults = Context::new(CONTEXT_NO_FLAGS);
            for path in defaults.include_paths() {
                let path = std::path::Path::new(path);
                if home.as_deref().is_none_or(|h| !path.starts_with(h)) {
                    assert!(ctx.include_path_append(path));
                }
            }
            Keymap::new_from_names(&ctx, "evdev", "pc105", layout, variant, options, KEYMAP_COMPILE_NO_FLAGS)
                .unwrap_or_else(|| panic!("{layout}({variant}) compiles"))
        }
    }

    impl Drop for OptionRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// KWin's `Xkb::keycodeFromKeysym`: first key (by keycode, then level)
    /// in `group` whose symbols contain `sym`.
    fn kwin_lookup(keymap: &Keymap, group: u32, sym: x::Keysym) -> Option<(u32, u32)> {
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();
        (min..max).find_map(|kc| {
            let key = Keycode::new(kc);
            (0..keymap.num_levels_for_key(key, group)).find_map(|level| {
                keymap
                    .key_get_syms_by_level(key, group, level)
                    .contains(&sym)
                    .then_some((kc - 8, level))
            })
        })
    }

    fn levels(keymap: &Keymap, kc: u32, group: u32, n: u32) -> Vec<Vec<x::Keysym>> {
        let key = Keycode::new(kc + 8);
        (0..n).map(|l| keymap.key_get_syms_by_level(key, group, l).to_vec()).collect()
    }

    #[test]
    fn option_reaches_every_char_in_every_layout_and_keeps_user_levels() {
        option_layouts_check(&OptionRoot::new("layouts"));
    }

    fn option_layouts_check(root: &OptionRoot) {
        for (layout, variant) in [
            ("us", ""),
            ("fr", ""),
            ("de", ""),
            ("gb", ""),
            ("es", ""),
            ("us", "dvorak"),
            ("us", "colemak"),
            ("us,ru", ""),
            ("ru,us", ""),
            ("de,fr,us", ""),
        ] {
            let plain = root.compile(layout, variant, None);
            let daklak = root.compile(layout, variant, Some("daklak:vn".into()));
            for group in 0..daklak.num_layouts() {
                let what = format!("{layout}({variant}) group {group}");
                for slot in SLOTS {
                    let kc = slot.keycode as u32;
                    assert_eq!(
                        levels(&plain, kc, group, 4),
                        levels(&daklak, kc, group, 4),
                        "{what}: {} levels 1-4 changed",
                        slot.name
                    );
                }
                for c in all_vn() {
                    let sym = xkb::commit_keysym(c).unwrap();
                    let found = kwin_lookup(&daklak, group, sym)
                        .unwrap_or_else(|| panic!("{what}: {c} not in keymap"));
                    // Our slot, or a key of the layout's own that already
                    // produces the char (fr `à`, `é`, …) — whichever comes
                    // first in keycode order, exactly as KWin picks.
                    let spec = char_to_emit(c).unwrap();
                    let level = if spec.mods & MOD_LEVEL3 != 0 { 6 } else { 4 }
                        + u32::from(spec.mods & MOD_SHIFT != 0);
                    let ours = (spec.keycode, level);
                    let own = kwin_lookup(&plain, group, sym);
                    assert!(found == ours || Some(found) == own, "{what}: {c} at {found:?}");
                }
            }
        }
    }

    #[test]
    fn option_never_replaces_existing_level5_symbols() {
        // de(neo) already uses levels 5-8 on letter keys; augment mode must
        // keep them (only empty levels get Vietnamese letters).
        let root = OptionRoot::new("neo");
        let plain = root.compile("de", "neo", None);
        let daklak = root.compile("de", "neo", Some("daklak:vn".into()));
        for slot in SLOTS {
            let kc = slot.keycode as u32;
            let before = levels(&plain, kc, 0, 8);
            let after = levels(&daklak, kc, 0, 8);
            for (l, (b, a)) in before.iter().zip(&after).enumerate() {
                if !b.is_empty() {
                    assert_eq!(b, a, "{} level {} replaced", slot.name, l + 1);
                }
            }
        }
    }

    #[test]
    fn option_binds_level_selector_keys() {
        let root = OptionRoot::new("mods");
        let keymap = root.compile("us,ru", "", Some("daklak:vn".into()));
        for group in 0..2 {
            assert_eq!(
                levels(&keymap, 100, group, 1),
                vec![vec![x::Keysym::ISO_Level3_Shift]],
                "RALT group {group}"
            );
            assert_eq!(
                levels(&keymap, LEVEL5_SHIFT_EVDEV as u32, group, 1),
                vec![vec![x::Keysym::ISO_Level5_Shift]],
                "HENK group {group}"
            );
        }
        // The Wayland modifier bits daklak uses select the intended levels.
        assert_eq!(type_char(&keymap, 24, MOD_LEVEL5, 0, 0), Some('ô'));
        assert_eq!(type_char(&keymap, 24, MOD_LEVEL3 | MOD_LEVEL5, 0, 1), Some('ơ'));
    }

    #[test]
    fn ascii_evdev_is_sorted_for_binary_search() {
        assert!(ASCII_EVDEV.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn symbols_and_keymap_share_slot_rows() {
        fn slot_rows(text: &str) -> BTreeSet<String> {
            text.lines()
                .map(str::trim_start)
                .filter(|l| l.starts_with("key <") || l.starts_with("replace key <"))
                .map(str::to_owned)
                .collect()
        }
        assert_eq!(slot_rows(&keymap_text()), slot_rows(&symbols_text()));
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

}
