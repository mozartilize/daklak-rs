use crate::BackspaceMethod;

/// Data collected by the daemon during the focus-enter probe window (~50ms).
/// The daemon fills this from surrounding_text / content_type events; Stage 2
/// only owns the decision logic.
pub struct CapabilityProbe {
    /// content_type `purpose` from the compositor (zwp_input_method_v2 event).
    pub purpose: u32,
    /// Whether a surrounding_text frame was received after activate.
    /// `false` means none arrived within the probe window (VSCode path).
    /// Only presence is needed for capability detection; the frame text/cursor
    /// stay in the transport/composer path so probing does not clone text.
    pub surrounding_text_seen: bool,

    /// Forced tier for `purpose == PURPOSE_TERMINAL`, set by the daemon from
    /// `DAKLAK_TERMINAL_TIER` once at startup.
    pub terminal_override: Option<BackspaceMethod>,
}

/// PURPOSE_TERMINAL from zwp_text_input_unstable_v3 (value 13).
/// Terminals reliably identify themselves; no further probing needed.
/// See docs/protocol-behavior.md:60-72,84-86.
const PURPOSE_TERMINAL: u32 = 13;

/// Pure capability decision — no cache, no network, no async.
/// The daemon (Stage 3) wraps this with a 50ms timeout and a
/// per-object generation cache.
///
/// Clients that never advertise `text_input_v3` at all (Qt5,
/// XWayland-via-vk, terminals such as Ghostty on wlroots) never reach this
/// function: they fire no `Activate`, so the daemon synthesizes a
/// key-channel `ForwardKey` session from focus metadata instead (see
/// `on_focus_changed`). This decision only runs for real text-input
/// activations.
///
/// Priority:
/// 1. Env override (terminal-only): `DAKLAK_TERMINAL_TIER` wins for purpose=13.
/// 2. Purpose-based default:
///    - Terminals → ForwardKey (foot composes correctly here; cosmetic
///      upstream preedit bug aside).
///    - Non-terminals → SurroundingText if app sent a surrounding_text frame
///      at activate, else ForwardKey.
pub fn detect_method(probe: &CapabilityProbe) -> BackspaceMethod {
    if probe.purpose == PURPOSE_TERMINAL {
        if let Some(forced) = probe.terminal_override {
            return forced;
        }
        // Terminals: default to ForwardKey regardless of surrounding_text
        // presence. SurroundingText would self-emit-loop and drop commits on
        // foot/ghostty. ForwardKey is the only safe default; users can
        // override via DAKLAK_TERMINAL_TIER.
        return BackspaceMethod::ForwardKey;
    }
    // XXX Some/None here conflates "explicit unsupported" with "no evidence
    // within probe window" (delayed frame, focus race, async client).
    // Currently safe because the terminal-purpose branch short-circuits every
    // terminal we have empirical data on. Only non-terminal apps land here.
    // If a Tier 1 ↔ Tier 2 flap is ever observed on such an app, promote
    // `surrounding_text_seen` to:
    //   enum SurroundingSupport { Confirmed, TimedOut, ExplicitlyUnsupported }
    // and route TimedOut → keep last decision (or default), ExplicitlyUnsupported
    // → ForwardKey, Confirmed → SurroundingText.
    if probe.surrounding_text_seen {
        BackspaceMethod::SurroundingText
    } else {
        BackspaceMethod::ForwardKey
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(purpose: u32, surrounding_text_seen: bool) -> CapabilityProbe {
        CapabilityProbe {
            purpose,
            surrounding_text_seen,
            terminal_override: None,
        }
    }

    #[test]
    fn terminal_default_is_forward_key() {
        // No override → ForwardKey.
        assert_eq!(
            detect_method(&probe(13, false)),
            BackspaceMethod::ForwardKey
        );
        // surrounding_text presence does NOT influence terminal routing.
        assert_eq!(detect_method(&probe(13, true)), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn terminal_override_forward() {
        let mut p = probe(13, false);
        p.terminal_override = Some(BackspaceMethod::ForwardKey);
        assert_eq!(detect_method(&p), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn terminal_override_surrounding() {
        let mut p = probe(13, true);
        p.terminal_override = Some(BackspaceMethod::SurroundingText);
        assert_eq!(detect_method(&p), BackspaceMethod::SurroundingText);
    }

    #[test]
    fn gedit_non_empty_surrounding_is_tier1() {
        assert_eq!(
            detect_method(&probe(0, true)),
            BackspaceMethod::SurroundingText
        );
    }

    #[test]
    fn non_terminal_no_surrounding_is_forward_key() {
        // VSCode / chromium / Electron-class path: doesn't proactively send
        // surrounding_text at activate. Falls through to ForwardKey default.
        assert_eq!(detect_method(&probe(0, false)), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn empty_surrounding_is_tier1() {
        // Empty surrounding_text at activate = app supports the protocol
        // but widget is just empty (e.g. fresh gedit document).
        assert_eq!(
            detect_method(&probe(0, true)),
            BackspaceMethod::SurroundingText
        );
    }
}
