use crate::BackspaceMethod;

/// Data collected by the daemon during the focus-enter probe window (~50ms).
/// The daemon fills this from surrounding_text / content_type events; Stage 2
/// only owns the decision logic.
pub struct CapabilityProbe {
    /// content_type `purpose` from the compositor (zwp_input_method_v2 event).
    pub purpose: u32,
    /// The first surrounding_text frame received after activate.
    /// `None` means none arrived within the probe window (VSCode path).
    pub surrounding_text_seen: Option<SurroundingFrame>,
    /// Focused window's `app_id` at activate (Sway IPC). Matched
    /// case-insensitively against `force_uinput_apps` to escalate any
    /// known-broken app (terminal or otherwise) to Tier 3 UInput. `None` on
    /// non-Sway compositors → fall through to purpose-based default.
    pub app_id: Option<String>,
    /// Apps whose `app_id` forces Tier 3 UInput regardless of purpose.
    /// Loaded by the daemon from config (`force_uinput_apps` in
    /// config.toml) and the env var `DAKLAK_FORCE_UINPUT_APPS`.
    /// Daklak ships with no opinionated default.
    pub force_uinput_apps: Vec<String>,
    /// Apps whose `app_id` forces Tier 4 VkOnly (Path C). Loaded from
    /// `force_vk_only_apps` in config.toml and env var
    /// `DAKLAK_FORCE_VK_ONLY_APPS`. Wins over the purpose default but
    /// loses to `force_uinput_apps` (UInput is the older, more
    /// battle-tested escape hatch). Daklak ships with no default — both
    /// lists are user-curated.
    pub force_vk_only_apps: Vec<String>,
    /// Forced tier for `purpose == PURPOSE_TERMINAL`, set by the daemon from
    /// `DAKLAK_TERMINAL_TIER` once at startup. Wins over the app_id list.
    pub terminal_override: Option<BackspaceMethod>,
}

pub struct SurroundingFrame {
    pub text: String,
    pub cursor: u32,
}

/// PURPOSE_TERMINAL from zwp_text_input_unstable_v3 (value 13).
/// Terminals reliably identify themselves; no further probing needed.
/// See docs/protocol-behavior.md:60-72,84-86.
const PURPOSE_TERMINAL: u32 = 13;

/// Pure capability decision — no cache, no network, no async.
/// The daemon (Stage 3) wraps this with a 50ms timeout and a
/// per-object generation cache.
///
/// Priority:
/// 1. Env override (terminal-only): `DAKLAK_TERMINAL_TIER` wins for purpose=13.
/// 2. App-id list — `force_uinput_apps` (any purpose) → Tier 3 UInput.
/// 3. App-id list — `force_vk_only_apps` (any purpose) → Tier 4 VkOnly
///    (Path C). Routes everything through `zwp_virtual_keyboard_v1::key()`
///    using daklak's synthesized Vietnamese keymap — bypasses
///    `text_input_v3` entirely. Safe target: clients with NO
///    `text_input_v3` at all (Qt5/XWayland-via-vk). Unsafe target:
///    Chromium-class apps — their renderer has hard-coded
///    `LinuxKeyCode → DomCode` tables and crashes when fed evdev 200+
///    attached to Unicode keysyms. For those, use `force_uinput_apps`.
/// 4. Purpose-based default:
///    - Terminals → ForwardKey (foot composes correctly here; cosmetic
///      upstream preedit bug aside).
///    - Non-terminals → SurroundingText if app sent a surrounding_text frame
///      at activate, else ForwardKey.
pub fn detect_method(probe: &CapabilityProbe) -> BackspaceMethod {
    if probe.purpose == PURPOSE_TERMINAL {
        if let Some(forced) = probe.terminal_override {
            return forced;
        }
    }
    if let Some(ref app_id) = probe.app_id {
        if app_id_matches(app_id, &probe.force_uinput_apps) {
            return BackspaceMethod::UInput;
        }
        if app_id_matches(app_id, &probe.force_vk_only_apps) {
            return BackspaceMethod::VkOnly;
        }
    }
    // Terminals: default to ForwardKey regardless of surrounding_text
    // presence. SurroundingText would self-emit-loop and drop commits on
    // foot/ghostty (see memory/project_terminal_tier.md). UInput would
    // race the terminal's own read loop. ForwardKey is the only safe
    // default; users can override via DAKLAK_TERMINAL_TIER.
    if probe.purpose == PURPOSE_TERMINAL {
        return BackspaceMethod::ForwardKey;
    }
    // XXX Some/None here conflates "explicit unsupported" with "no evidence
    // within probe window" (delayed frame, focus race, async client).
    // Currently safe because branches 1-4 (terminal_override, force_uinput_apps,
    // force_vk_only_apps, terminal-purpose) short-circuit every app we have
    // empirical data on. Only unknown apps with `purpose != PURPOSE_TERMINAL`
    // land here. If a Tier 1 ↔ Tier 2 flap is ever observed on such an app,
    // promote `surrounding_text_seen` to:
    //   enum SurroundingSupport { Confirmed, TimedOut, ExplicitlyUnsupported }
    // and route TimedOut → keep last decision (or default), ExplicitlyUnsupported
    // → ForwardKey, Confirmed → SurroundingText.
    match &probe.surrounding_text_seen {
        Some(_) => BackspaceMethod::SurroundingText,
        None => BackspaceMethod::ForwardKey,
    }
}

/// Case-insensitive match of `app_id` against a user-supplied list of
/// apps. Used by both `force_uinput_apps` and `force_vk_only_apps`.
///
/// `app_id` is trimmed defensively — the source is either Sway IPC (which
/// shouldn't pad) or the `WM_CLASS` fallback for XWayland (which can have
/// trailing nulls or whitespace on some clients). List entries are
/// canonicalized at load time (`Config::load`), so we only need to trim
/// the input side here.
fn app_id_matches<S: AsRef<str>>(app_id: &str, list: &[S]) -> bool {
    let lower = app_id.trim().to_ascii_lowercase();
    list.iter().any(|t| t.as_ref() == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(purpose: u32, surrounding: Option<(&str, u32)>) -> CapabilityProbe {
        CapabilityProbe {
            purpose,
            surrounding_text_seen: surrounding.map(|(t, c)| SurroundingFrame {
                text: t.to_owned(),
                cursor: c,
            }),
            app_id: None,
            force_uinput_apps: Vec::new(),
            force_vk_only_apps: Vec::new(),
            terminal_override: None,
        }
    }

    fn probe_with_app_id(purpose: u32, app_id: &str) -> CapabilityProbe {
        let mut p = probe(purpose, None);
        p.app_id = Some(app_id.to_owned());
        p
    }

    fn probe_with_app_id_and_list(
        purpose: u32,
        app_id: &str,
        list: &[&str],
    ) -> CapabilityProbe {
        let mut p = probe_with_app_id(purpose, app_id);
        p.force_uinput_apps = list.iter().map(|s| (*s).to_owned()).collect();
        p
    }

    #[test]
    fn terminal_default_is_forward_key() {
        // No app_id, no override, empty broken list → ForwardKey.
        assert_eq!(detect_method(&probe(13, None)), BackspaceMethod::ForwardKey);
        // surrounding_text presence does NOT influence terminal routing.
        assert_eq!(
            detect_method(&probe(13, Some(("text", 4)))),
            BackspaceMethod::ForwardKey
        );
    }

    #[test]
    fn terminal_unknown_app_id_uses_forward_key() {
        assert_eq!(
            detect_method(&probe_with_app_id(13, "footclient")),
            BackspaceMethod::ForwardKey
        );
        assert_eq!(
            detect_method(&probe_with_app_id(13, "com.mitchellh.ghostty")),
            BackspaceMethod::ForwardKey
        );
    }

    #[test]
    fn terminal_override_uinput_beats_app_id() {
        let mut p = probe_with_app_id(13, "footclient");
        p.terminal_override = Some(BackspaceMethod::UInput);
        assert_eq!(detect_method(&p), BackspaceMethod::UInput);
    }

    #[test]
    fn terminal_override_forward_beats_app_id() {
        let mut p = probe_with_app_id(13, "com.mitchellh.ghostty");
        p.terminal_override = Some(BackspaceMethod::ForwardKey);
        assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn terminal_override_surrounding() {
        let mut p = probe(13, Some(("", 0)));
        p.terminal_override = Some(BackspaceMethod::SurroundingText);
        assert_eq!(detect_method(&p), BackspaceMethod::SurroundingText);
    }

    #[test]
    fn app_id_match_helper_is_case_insensitive() {
        let list = &["chromium", "com.mitchellh.ghostty"];
        for input in ["chromium", "Chromium", "CHROMIUM", "ChRoMiUm"] {
            assert!(app_id_matches(input, list), "expected match for {input}");
        }
        for input in ["com.mitchellh.ghostty", "COM.MITCHELLH.GHOSTTY"] {
            assert!(app_id_matches(input, list), "expected match for {input}");
        }
        for input in ["chrome", "footclient", "foot", "gedit", ""] {
            assert!(!app_id_matches(input, list), "did not expect match for {input}");
        }
    }

    #[test]
    fn app_id_match_helper_empty_list_matches_nothing() {
        let empty: &[&str] = &[];
        for input in ["chromium", "ghostty", "footclient"] {
            assert!(!app_id_matches(input, empty), "empty list should never match {input}");
        }
    }

    #[test]
    fn non_terminal_app_id_match_escalates_to_uinput() {
        // Config-driven list: chromium with purpose=0 escalates to UInput.
        let p = probe_with_app_id_and_list(0, "chromium", &["chromium"]);
        assert_eq!(detect_method(&p), BackspaceMethod::UInput);
        // Not in list → falls through to ForwardKey default for purpose=0.
        let p = probe_with_app_id_and_list(0, "gedit", &["chromium"]);
        assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn terminal_app_id_match_escalates_to_uinput() {
        // Config-driven list: ghostty with purpose=13 escalates to UInput.
        let p = probe_with_app_id_and_list(13, "com.mitchellh.ghostty", &["com.mitchellh.ghostty"]);
        assert_eq!(detect_method(&p), BackspaceMethod::UInput);
    }

    #[test]
    fn force_vk_only_routes_to_vk_only() {
        // Chromium scenario: app advertises text_input_v3 so real Activate
        // fires, but user listed it in force_vk_only_apps because commit
        // delivery is flaky on Tier 1/2. force_vk_only_apps wins over the
        // purpose-based default.
        let mut p = probe_with_app_id(0, "chromium");
        p.force_vk_only_apps = vec!["chromium".to_owned()];
        assert_eq!(detect_method(&p), BackspaceMethod::VkOnly);

        // Even when surrounding_text frame is present (would otherwise
        // route to Tier 1) — force_vk_only_apps still wins.
        let mut p = probe(0, Some(("tran", 4)));
        p.app_id = Some("chromium".to_owned());
        p.force_vk_only_apps = vec!["chromium".to_owned()];
        assert_eq!(detect_method(&p), BackspaceMethod::VkOnly);
    }

    #[test]
    fn force_uinput_beats_force_vk_only() {
        // Both lists contain the app — UInput wins (older, more
        // battle-tested escape hatch). Documented in detect_method's
        // priority comment.
        let mut p = probe_with_app_id(0, "chromium");
        p.force_uinput_apps = vec!["chromium".to_owned()];
        p.force_vk_only_apps = vec!["chromium".to_owned()];
        assert_eq!(detect_method(&p), BackspaceMethod::UInput);
    }

    #[test]
    fn empty_list_never_escalates() {
        // Default config (no force_uinput_apps): chromium stays on ForwardKey
        // for purpose=0, ghostty stays on ForwardKey for purpose=13.
        let p = probe_with_app_id(0, "chromium");
        assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
        let p = probe_with_app_id(13, "com.mitchellh.ghostty");
        assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn gedit_non_empty_surrounding_is_tier1() {
        assert_eq!(
            detect_method(&probe(0, Some(("tran viet ha", 12)))),
            BackspaceMethod::SurroundingText
        );
    }

    #[test]
    fn non_terminal_no_surrounding_is_forward_key() {
        // VSCode / chromium / Electron-class path: doesn't proactively send
        // surrounding_text at activate. Falls through to ForwardKey default.
        // (chromium's first-compose BS-drop is mitigated by adding its app_id
        // to FORWARD_KEY_BROKEN_APPS to force UInput.)
        assert_eq!(detect_method(&probe(0, None)), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn empty_surrounding_is_tier1() {
        // Empty surrounding_text at activate = app supports the protocol
        // but widget is just empty (e.g. fresh gedit document).
        assert_eq!(
            detect_method(&probe(0, Some(("", 0)))),
            BackspaceMethod::SurroundingText
        );
    }
}
