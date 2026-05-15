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
pub fn detect_method(probe: &CapabilityProbe) -> BackspaceMethod {
    if probe.purpose == PURPOSE_TERMINAL {
        // Tier 3 (UInput) currently broken on foot: synth BS round-trips
        // through own grab + foot may drop commit_string on PTY mode.
        // Tier 2 (ForwardKey = vk_key BS + commit_string) is the empirical
        // fallback until Tier 3 self-emit suppression lands.
        return BackspaceMethod::ForwardKey;
    }
    // Any surrounding_text event (even with empty initial text) means the
    // app supports the protocol — use Tier 1. Empty text at activate just
    // means the widget started empty (e.g. gedit on a fresh document).
    //
    // Tier 2 (vk_key BS + commit_string) is fundamentally racy: the vk_key
    // events go through the keyboard channel while commit_string is part of
    // an atomic IM state update, and apps may process them out of order.
    // Prefer Tier 1 whenever possible.
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
        }
    }

    #[test]
    fn terminal_purpose_is_forward_key() {
        // Tier 3 (UInput) is broken on foot; terminals use Tier 2 until fixed.
        assert_eq!(detect_method(&probe(13, None)), BackspaceMethod::ForwardKey);
        // Even if terminal sends surrounding_text, purpose wins
        assert_eq!(
            detect_method(&probe(13, Some(("text", 4)))),
            BackspaceMethod::ForwardKey
        );
    }

    #[test]
    fn gedit_non_empty_surrounding_is_tier1() {
        assert_eq!(
            detect_method(&probe(0, Some(("tran viet ha", 12)))),
            BackspaceMethod::SurroundingText
        );
    }

    #[test]
    fn vscode_no_surrounding_is_forward_key() {
        assert_eq!(detect_method(&probe(0, None)), BackspaceMethod::ForwardKey);
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
