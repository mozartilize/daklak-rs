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
/// 2. App-id list (any purpose): match against `probe.force_uinput_apps` →
///    UInput. List is loaded by the daemon from config.toml or env
///    (`DAKLAK_FORCE_UINPUT_APPS=app1,app2,...`).
/// 3. Purpose-based default:
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
        if app_id_forces_uinput(app_id, &probe.force_uinput_apps) {
            return BackspaceMethod::UInput;
        }
    }
    if probe.purpose == PURPOSE_TERMINAL {
        return BackspaceMethod::ForwardKey;
    }
    match &probe.surrounding_text_seen {
        Some(_) => BackspaceMethod::SurroundingText,
        None => BackspaceMethod::ForwardKey,
    }
}

/// Case-insensitive match of `app_id` against the user-supplied list of apps
/// that must route to Tier 3 UInput.
fn app_id_forces_uinput<S: AsRef<str>>(app_id: &str, broken_list: &[S]) -> bool {
    let lower = app_id.to_ascii_lowercase();
    broken_list.iter().any(|t| t.as_ref() == lower)
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
            assert!(app_id_forces_uinput(input, list), "expected match for {input}");
        }
        for input in ["com.mitchellh.ghostty", "COM.MITCHELLH.GHOSTTY"] {
            assert!(app_id_forces_uinput(input, list), "expected match for {input}");
        }
        for input in ["chrome", "footclient", "foot", "gedit", ""] {
            assert!(!app_id_forces_uinput(input, list), "did not expect match for {input}");
        }
    }

    #[test]
    fn app_id_match_helper_empty_list_matches_nothing() {
        let empty: &[&str] = &[];
        for input in ["chromium", "ghostty", "footclient"] {
            assert!(!app_id_forces_uinput(input, empty), "empty list should never match {input}");
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
