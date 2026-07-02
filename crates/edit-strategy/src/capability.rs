use crate::BackspaceMethod;

/// Data collected by the daemon during the focus-enter probe window (~50ms).
/// The daemon fills this from surrounding_text events; Stage 2 only owns the
/// decision logic.
pub struct CapabilityProbe {
    /// Whether a surrounding_text frame was received after activate.
    /// `false` means none arrived within the probe window (VSCode path).
    /// Only presence is needed for capability detection; the frame text/cursor
    /// stay in the transport/composer path so probing does not clone text.
    pub surrounding_text_seen: bool,
}

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
/// Rule: `SurroundingText` if the app sent a surrounding_text frame at
/// activate, else `ForwardKey`. Terminals are not special-cased here — a
/// terminal (foot on KWin) that advertises an empty surrounding_text frame is
/// initially picked as `SurroundingText` and then caught by the runtime ST→FK
/// liveness downgrade (`note_surrounding_liveness`) during the first
/// non-destructive keystrokes, before any `delete_surrounding_text` is issued.
/// On wlroots, terminals send no surrounding_text at all, so they resolve to
/// `ForwardKey` here directly.
pub fn detect_method(probe: &CapabilityProbe) -> BackspaceMethod {
    // XXX Some/None here conflates "explicit unsupported" with "no evidence
    // within probe window" (delayed frame, focus race, async client).
    // If a Tier 1 ↔ Tier 2 flap is ever observed on an app, promote
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

    fn probe(surrounding_text_seen: bool) -> CapabilityProbe {
        CapabilityProbe {
            surrounding_text_seen,
        }
    }

    #[test]
    fn gedit_non_empty_surrounding_is_tier1() {
        assert_eq!(detect_method(&probe(true)), BackspaceMethod::SurroundingText);
    }

    #[test]
    fn non_terminal_no_surrounding_is_forward_key() {
        // VSCode / chromium / Electron-class path: doesn't proactively send
        // surrounding_text at activate. Falls through to ForwardKey default.
        assert_eq!(detect_method(&probe(false)), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn empty_surrounding_is_tier1() {
        // Empty surrounding_text at activate = app supports the protocol
        // but widget is just empty (e.g. fresh gedit document). The runtime
        // liveness watchdog downgrades to ForwardKey if the frames stay dead
        // (terminals such as foot on KWin).
        assert_eq!(detect_method(&probe(true)), BackspaceMethod::SurroundingText);
    }
}
