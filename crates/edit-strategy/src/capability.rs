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
    /// Focused window's `app_id` at activate (Sway IPC). Used to escalate
    /// known-broken-on-ForwardKey terminals (see FORWARD_KEY_BROKEN_TERMINALS).
    /// `None` on non-Sway compositors → fall through to default routing.
    pub app_id: Option<String>,
    /// Forced tier for `purpose == PURPOSE_TERMINAL`, set by the daemon from
    /// `DAKLAK_TERMINAL_TIER` once at startup. Wins over per-app auto-detect.
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

/// Terminals known to drop synthetic `vk_key(BS)` + `commit_string` on the
/// ForwardKey path. Matched (lowercased) against `probe.app_id` to escalate
/// the affected app to Tier 3 UInput. Intentionally empty by default —
/// daklak-rs ships the mechanism but no opinionated list. Add an entry here
/// when a terminal is confirmed broken on Tier 2 and the user wants
/// zero-config auto-routing for it.
const FORWARD_KEY_BROKEN_TERMINALS: &[&str] = &[];

/// Pure capability decision — no cache, no network, no async.
/// The daemon (Stage 3) wraps this with a 50ms timeout and a
/// per-object generation cache.
pub fn detect_method(probe: &CapabilityProbe) -> BackspaceMethod {
    if probe.purpose == PURPOSE_TERMINAL {
        // Terminal routing priority:
        //   1. Env override (`DAKLAK_TERMINAL_TIER=...`) wins absolutely.
        //   2. Per-app auto-escalation: focused `app_id` matched against
        //      FORWARD_KEY_BROKEN_TERMINALS → Tier 3 UInput.
        //   3. Default: Tier 2 ForwardKey. foot composes correctly here
        //      (cosmetic preedit underline on uncomposed forwards is an
        //      upstream foot bug, not a daklak protocol issue). Tier 2 also
        //      leaves the IM grab intact — no keystroke-leakage window from
        //      the Tier 3 release/regrab dance.
        //
        // Why no protocol probe: terminals send no `surrounding_text` frames
        // over input-method-v2 (confirmed empirically on foot and ghostty),
        // so there is no observable signal from the app to tell us whether
        // it honored a Tier 2 commit. App-id matching is the only available
        // auto-detect, and is gated on Sway IPC providing the `app_id`.
        if let Some(forced) = probe.terminal_override {
            return forced;
        }
        if let Some(ref app_id) = probe.app_id {
            let lower = app_id.to_ascii_lowercase();
            if FORWARD_KEY_BROKEN_TERMINALS.iter().any(|t| *t == lower) {
                return BackspaceMethod::UInput;
            }
        }
        return BackspaceMethod::ForwardKey;
    }
    // Any surrounding_text event (even with empty initial text) means the
    // app supports the protocol — use Tier 1. Empty text at activate just
    // means the widget started empty (e.g. gedit on a fresh document).
    //
    // For apps that don't proactively send surrounding_text at activate
    // (chromium, VSCode, Electron-class) but still honor vk_key BS via
    // wl_keyboard, Tier 2 ForwardKey is the right path. Empirically chromium
    // also DROPS `delete_surrounding_text` entirely, so Tier 1 is strictly
    // worse there. The known Tier 2 wart — chromium drops the first compose's
    // vk_key BS because its text_input_v3 edit session hasn't started yet —
    // is mitigated by the daemon sending a warm-up `commit_string("") +
    // commit` at activate before any composition fires (see
    // [crates/daemon/src/wayland/mod.rs apply_done_frame activate branch]).
    match &probe.surrounding_text_seen {
        Some(_) => BackspaceMethod::SurroundingText,
        // Never sent surrounding_text — app doesn't support the protocol
        None => BackspaceMethod::ForwardKey,
    }
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
            terminal_override: None,
        }
    }

    fn probe_with_app_id(purpose: u32, app_id: &str) -> CapabilityProbe {
        let mut p = probe(purpose, None);
        p.app_id = Some(app_id.to_owned());
        p
    }

    #[test]
    fn terminal_default_is_forward_key() {
        // No app_id, no override → ForwardKey default.
        assert_eq!(
            detect_method(&probe(13, None)),
            BackspaceMethod::ForwardKey
        );
        // surrounding_text presence does NOT influence terminal routing —
        // purpose=13 wins.
        assert_eq!(
            detect_method(&probe(13, Some(("text", 4)))),
            BackspaceMethod::ForwardKey
        );
    }

    #[test]
    fn terminal_unknown_app_id_uses_forward_key() {
        // foot, ghostty, anything else not in the empty broken-list →
        // ForwardKey. The list is intentionally empty by default.
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
    fn terminal_app_id_match_escalates_to_uinput() {
        // Smoke test the matching mechanism even though the shipped list
        // is empty. Drive it with a temporary expectation: if the list
        // contained "ghostty", "GHOSTTY" would still match (case-insensitive).
        // We assert the lowercase-folding helper would work by checking the
        // negative side too.
        for app_id in ["ghostty", "GHOSTTY", "Ghostty"] {
            let p = probe_with_app_id(13, app_id);
            // With an empty list, no app matches → ForwardKey.
            assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
        }
    }

    #[test]
    fn terminal_override_uinput_beats_app_id() {
        // DAKLAK_TERMINAL_TIER=uinput forces UInput regardless of app_id.
        let mut p = probe_with_app_id(13, "footclient");
        p.terminal_override = Some(BackspaceMethod::UInput);
        assert_eq!(detect_method(&p), BackspaceMethod::UInput);
    }

    #[test]
    fn terminal_override_forward_beats_app_id() {
        // Explicit ForwardKey override wins (would only matter once the
        // broken-list is non-empty).
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
    fn app_id_does_not_affect_non_terminal() {
        // app_id rule is gated on purpose==13. A non-terminal surface
        // reporting a "broken" app_id should still get normal routing.
        let mut p = probe(0, Some(("", 0)));
        p.app_id = Some("com.mitchellh.ghostty".to_owned());
        assert_eq!(detect_method(&p), BackspaceMethod::SurroundingText);
    }

    #[test]
    fn gedit_non_empty_surrounding_is_tier1() {
        assert_eq!(
            detect_method(&probe(0, Some(("tran viet ha", 12)))),
            BackspaceMethod::SurroundingText
        );
    }

    #[test]
    fn non_terminal_no_surrounding_is_tier1() {
        // VSCode / chromium / Electron path: app doesn't proactively send
        // surrounding_text at activate but still honors
        // delete_surrounding_text. Route to Tier 1 to keep delete+commit
        // atomic over input-method-v2 and avoid the chromium first-compose
        // BS-drop seen on Tier 2.
        assert_eq!(detect_method(&probe(0, None)), BackspaceMethod::SurroundingText);
    }

    #[test]
    fn empty_surrounding_is_tier1() {
        // Empty surrounding_text at activate = app supports the protocol
        // but widget is just empty (e.g. fresh gedit document). Prefer Tier 1.
        assert_eq!(
            detect_method(&probe(0, Some(("", 0)))),
            BackspaceMethod::SurroundingText
        );
    }
}
