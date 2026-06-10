//! Daemon: the orchestration / policy layer. Owns config, the on/off gate,
//! modifier + focus lifecycle, capability detection, and key routing. The
//! composition brain lives in [`crate::composer::Composer`]; the per-transport
//! wire glue lives in [`crate::transport`]. `Daemon` keeps a single live
//! `Composer` and exposes transport-neutral entry points that the glue calls.

use std::sync::atomic::AtomicBool;
#[cfg(feature = "ibus")]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, ModifierState, SurroundingFrame,
};
use viet_ime_wayland_adapter::{FrameSnapshot, KeyDecision};

#[cfg(feature = "ibus")]
use crate::composer::CharCursor;
use crate::composer::Composer;
use crate::config::Config;

// Linux evdev code for Backspace.
pub(crate) const KEY_BACKSPACE: u32 = 14;
// Navigation keys that move the cursor — trigger shadow reset.
pub(crate) const NAV_KEYS: &[u32] = &[
    105, 106, 103, 108, // Left, Right, Up, Down
    102, 107, // Home, End
    104, 109, // PageUp, PageDown
];

/// Orchestration state. Owns config + the single live `Composer` + policy flags.
pub struct Daemon {
    pub config: Config,

    pub modifiers: ModifierState,
    pub current_active: bool,

    /// True when `current_active` was synthesized by daklak (Path C —
    /// FocusBackend reported a focused toplevel matching `force_vk_only_apps`
    /// / `auto_vk_only_for_xwayland`) rather than driven by a compositor
    /// `zwp_input_method_v2::Activate` event. Real activate always wins.
    pub synthetic_active: bool,

    /// Single live composition state. Shape unchanged from the old
    /// `window: Option<WindowState>` — only the type name changed. The
    /// per-session HashMap is the deferred follow-up (see plan81 §Deferred),
    /// not here.
    pub composer: Option<Composer>,

    /// Forced tier for `purpose == PURPOSE_TERMINAL`, read once from
    /// `DAKLAK_TERMINAL_TIER` at startup. None → detect_method default.
    pub terminal_override: Option<BackspaceMethod>,

    /// Focused window's `app_id` captured at activate. Threaded into the
    /// capability probe so known-broken-on-ForwardKey terminals can
    /// auto-escalate. None outside an active session.
    pub focused_app_id: Option<String>,

    /// Shared on/off flag — written by the control task, read each keystroke.
    pub enabled: Arc<AtomicBool>,
    /// Previous value of `enabled`; used for edge-detection (on→off triggers
    /// a lazy full_reset on the next keystroke instead of from the control task).
    pub(crate) last_enabled: bool,
}

impl Daemon {
    pub fn new(config: Config, enabled: Arc<AtomicBool>) -> Self {
        let terminal_override = match std::env::var("DAKLAK_TERMINAL_TIER")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("uinput") => {
                tracing::info!("DAKLAK_TERMINAL_TIER=uinput → terminals route to Tier 3 UInput");
                Some(BackspaceMethod::UInput)
            }
            Some("surrounding") | Some("surrounding_text") | Some("tier1") => {
                tracing::info!(
                    "DAKLAK_TERMINAL_TIER=surrounding → terminals route to Tier 1 SurroundingText"
                );
                Some(BackspaceMethod::SurroundingText)
            }
            Some("forward") | Some("forward_key") | Some("tier2") | Some("") | None => None,
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "DAKLAK_TERMINAL_TIER unrecognized; falling back to default (ForwardKey)"
                );
                None
            }
        };

        Self {
            config,
            modifiers: ModifierState::empty(),
            current_active: false,
            synthetic_active: false,
            composer: None,
            terminal_override,
            focused_app_id: None,
            enabled,
            last_enabled: true,
        }
    }

    pub(crate) fn detect_capability(&self, frame: &FrameSnapshot) -> BackspaceMethod {
        let probe = CapabilityProbe {
            purpose: frame.purpose,
            surrounding_text_seen: frame.surrounding_text.as_ref().map(|(text, cursor, _anchor)| {
                SurroundingFrame {
                    text: text.clone(),
                    cursor: *cursor,
                }
            }),
            app_id: self.focused_app_id.clone(),
            force_uinput_apps: self.config.force_uinput_apps.clone(),
            force_vk_only_apps: self.config.force_vk_only_apps.clone(),
            terminal_override: self.terminal_override,
        };
        detect_method(&probe)
    }

    // ── transport-neutral key routing (non-wayland: ibus, evdev) ────────────

    /// Like wayland's `on_key_pressed` but without `AdapterCtx`. NAV and
    /// modifier-shortcut keys return `ForwardRaw` instead of `Consumed` (the
    /// caller just passes through). Uses the v1/raw_word path — IBus intercepts
    /// before the client receives the key, same shape as the v1/KWin path.
    #[cfg(feature = "ibus")]
    pub fn process_key(&mut self, key: u32, ch: Option<char>) -> KeyDecision {
        if let Some(w) = self.composer.as_mut() {
            w.check_idle_reset();
        }
        let now_enabled = self.enabled.load(Ordering::Acquire);
        if self.last_enabled && !now_enabled {
            if let Some(w) = self.composer.as_mut() {
                w.full_reset();
            }
        }
        self.last_enabled = now_enabled;
        if !now_enabled {
            return KeyDecision::ForwardRaw;
        }
        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.modifiers.intersects(shortcut_mods) {
            if let Some(w) = self.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        }
        if NAV_KEYS.contains(&key) {
            if let Some(w) = self.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        }
        if self.composer.is_none() {
            return KeyDecision::ForwardRaw;
        }
        if key == KEY_BACKSPACE {
            return self.handle_backspace();
        }
        if let Some(w) = self.composer.as_mut() {
            w.mark_action();
        }
        let Some(ch) = ch else {
            // Non-printable key (Enter, Tab, Escape, …) ends the current word.
            // Clear the composition so the next char starts fresh and isn't
            // seeded with this line's keystrokes (e.g. "hiếu"⏎ then typing must
            // not re-seed raw_word="hieeus" → "hieeushi…").
            if let Some(w) = self.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        };
        // IBus is a client-side-insertion + observe-surrounding transport, the
        // same shape as the v1/KWin path: use the raw_word seed so the engine
        // keeps vowel-cluster context across transforms (after `ee`→ê it would
        // otherwise forget `iê` and mis-place a later tone, e.g. hiêu+s → hiêí
        // instead of hiếu). NOT the v2 grab path, which relies on the engine's
        // running state that ibus keystroke gaps + reseeds disturb.
        self.handle_char(key, ch)
    }

    // ── thin delegations to the Composer brain (trait surface + tests) ──────

    pub fn handle_char(&mut self, _key: u32, ch: char) -> KeyDecision {
        match self.composer.as_mut() {
            Some(w) => w.feed_key(ch),
            None => KeyDecision::ForwardRaw,
        }
    }

    /// Test-only entry exposing the raw seed-path selector. `true` = v1/raw_word
    /// path, `false` = v2/wlroots key-grab path. Production always uses `true`
    /// (via [`handle_char`]); the `false` path is exercised only by the
    /// raw_word_reset characterization tests.
    #[cfg(test)]
    pub(crate) fn handle_char_inner(
        &mut self,
        _key: u32,
        ch: char,
        shadow_already_has_ch: bool,
    ) -> KeyDecision {
        match self.composer.as_mut() {
            Some(w) => w.feed_key_inner(ch, shadow_already_has_ch),
            None => KeyDecision::ForwardRaw,
        }
    }

    pub fn handle_backspace(&mut self) -> KeyDecision {
        match self.composer.as_mut() {
            Some(w) => w.feed_backspace(),
            None => KeyDecision::ForwardRaw,
        }
    }

    /// Apply a pending edit to an arbitrary sink (IBus, tests).
    #[cfg(feature = "ibus")]
    pub fn apply_with_sink<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        time: u32,
        sink: &mut S,
    ) {
        if let Some(w) = self.composer.as_mut() {
            w.apply(backspaces, commit, time, sink);
        }
    }

    /// Update shadow + engine seed from surrounding text (IBus
    /// `SetSurroundingText`; cursor in chars). Skips reseed within 150 ms of a
    /// daklak action (our own echo) and on mid-word 1-char insertions.
    #[cfg(feature = "ibus")]
    pub fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) {
        if let Some(w) = self.composer.as_mut() {
            if w.is_duplicate_frame(text, cursor, anchor) {
                return;
            }
            w.observe_surrounding_chars(text, CharCursor(cursor), CharCursor(anchor));
        }
    }

    // ── focus / session lifecycle ──────────────────────────────────────────

    /// Bootstrap a synthetic session for evdev-only mode. Sets up a composer
    /// with VkOnly routing so `handle_char` / `handle_backspace` work without a
    /// Wayland compositor.
    pub fn activate_evdev(&mut self) {
        let mut c = Composer::new(
            self.config.method.to_engine(),
            BackspaceMethod::VkOnly,
            self.config.bracket_shortcuts,
        );
        c.set_modifiers(self.modifiers);
        self.current_active = true;
        self.synthetic_active = true;
        self.composer = Some(c);
    }

    /// Create a composer session for a non-Wayland transport (IBus). Idempotent.
    #[cfg(feature = "ibus")]
    pub fn activate_ibus(
        &mut self,
        method: viet_ime_edit_strategy::BackspaceMethod,
        chars_for_delete: bool,
    ) {
        if self.current_active {
            return;
        }
        let mut c = Composer::new(
            self.config.method.to_engine(),
            method,
            self.config.bracket_shortcuts,
        );
        c.set_chars_for_delete(chars_for_delete);
        c.set_modifiers(self.modifiers);
        self.composer = Some(c);
        self.current_active = true;
    }

    /// Tear down the IBus session. Clears composition state.
    #[cfg(feature = "ibus")]
    pub fn deactivate_ibus(&mut self) {
        if let Some(w) = self.composer.as_mut() {
            w.full_reset();
        }
        self.composer = None;
        self.current_active = false;
    }

    /// React to an IBus routing change while a session is live.
    #[cfg(feature = "ibus")]
    pub fn update_ibus_method(&mut self, want: BackspaceMethod) {
        if let Some(w) = self.composer.as_mut() {
            if w.method() != want {
                tracing::info!(?want, "ibus: method updated");
                w.set_method(want);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    mod surrounding_anchor_regression {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyState, OutputSink};
        use viet_ime_engine::InputMethod;
        use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, AdapterState, FrameSnapshot};

        #[derive(Default)]
        struct DeleteCaptureSink {
            deletes: Vec<(u32, u32, u32, u32)>,
            vk_keys: Vec<(u32, u32, KeyState)>,
            commits: Vec<String>,
        }

        impl OutputSink for DeleteCaptureSink {
            fn delete_surrounding_text(
                &mut self,
                before_bytes: u32,
                before_chars: u32,
                after_bytes: u32,
                after_chars: u32,
            ) {
                self.deletes
                    .push((before_bytes, before_chars, after_bytes, after_chars));
            }

            fn commit_string(&mut self, text: &str) {
                self.commits.push(text.to_owned());
            }
            fn commit(&mut self, _serial: u32) {}
            fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
                self.vk_keys.push((time, key_code, state));
            }
            fn vk_modifiers(&mut self, _depressed: u32, _latched: u32, _locked: u32, _group: u32) {}
            fn uinput_key(&mut self, _key_code: u16, _value: i32) {}
            fn vk_commit_char(&mut self, _time: u32, _c: char) -> bool {
                false
            }
        }

        fn frame(text: &str, cursor: u32, anchor: u32) -> FrameSnapshot {
            FrameSnapshot {
                activate: false,
                deactivate: false,
                surrounding_text: Some((text.to_owned(), cursor, anchor)),
                purpose: 0,
                app_id: None,
                is_xwayland: false,
            }
        }

        #[test]
        fn anchor_only_surrounding_update_must_not_be_dropped() {
            // Regression capture: Chromium omnibox + Google search provider
            // can inline-autocomplete from history (e.g. suggest
            // `translate.google.com`) and report surrounding_text like
            // "translate" with an active tail selection (cursor=3,
            // anchor=9). We must not drop anchor-only updates — the
            // selection detection triggers a ForwardKey fallback in Tier 1
            // (virtual keyboard backspaces instead of delete_surrounding_text)
            // to avoid the Chromium race condition where the key release
            // arrives via the fast wl_keyboard path and changes Chrome's
            // selection state before our text edit arrives.
            let mut daemon = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)));
            daemon.current_active = true;
            daemon.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            let mut state = AdapterState::new();
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 3));
            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 9));

            let mut sink = DeleteCaptureSink::default();
            let w = daemon.composer.as_mut().expect("composer state exists");
            w.strategy.apply(1, "â", 1, 0, &mut sink);

            // Selection present → no delete_surrounding_text (would race),
            // instead ForwardKey BS: 2 BSes (1 for selection + 1 for 'a')
            assert!(
                sink.deletes.is_empty(),
                "must NOT use delete_surrounding_text when selection is active"
            );
            assert_eq!(
                sink.vk_keys.len(),
                4, // 2 backspaces × (press + release)
                "anchor-only frame change must produce ForwardKey BS fallback"
            );
            assert_eq!(sink.commits, vec!["â".to_owned()]);
        }
    }

    // ── Ctrl+BS / NAV reset: raw_word lives on the Composer, must be cleared
    //    alongside the rest of compose state. Bug is v1/KWin only — v2/wlroots
    //    path (shadow_already_has_ch=false) never reads or writes raw_word.
    mod raw_word_reset {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::BackspaceMethod;
        use viet_ime_engine::InputMethod;
        use viet_ime_evdev_adapter::EvdevHandler;

        fn v1_daemon() -> Daemon {
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)));
            d.current_active = true;
            d.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));
            d
        }

        fn raw_word(d: &Daemon) -> &str {
            d.composer
                .as_ref()
                .map(|w| w.raw_word.as_str())
                .unwrap_or("")
        }

        #[test]
        fn v1_path_builds_raw_word_from_alpha_chars() {
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, true);
            }
            assert_eq!(raw_word(&d), "xaxax");
        }

        #[test]
        fn full_reset_window_clears_raw_word() {
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, true);
            }
            assert_eq!(raw_word(&d), "xaxax");
            d.full_reset_window();
            assert_eq!(raw_word(&d), "", "full_reset_window must clear raw_word");
        }

        #[test]
        fn next_alpha_after_reset_does_not_carry_stale_seed() {
            // Repro of the gedit Ctrl+BS bug: type `xaxax`, simulate the
            // word-delete reset path, type `x`. raw_word must be just "x".
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, true);
            }
            d.full_reset_window();
            d.handle_char_inner(0, 'x', true);
            assert_eq!(
                raw_word(&d),
                "x",
                "post-reset keystroke must not re-seed from deleted word"
            );
        }

        #[test]
        fn v2_path_never_touches_raw_word() {
            // wlroots / v2: shadow_already_has_ch=false. raw_word remains
            // untouched — bug is v1-specific.
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, false);
            }
            assert_eq!(
                raw_word(&d),
                "",
                "v2 path must not write raw_word — bug is v1-only"
            );
        }

        #[test]
        fn deactivate_drops_raw_word_via_window_drop() {
            // raw_word lives on the Composer — when composer = None, it dies
            // with it. No explicit clear needed in deactivate.
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, true);
            }
            d.composer = None;
            assert_eq!(raw_word(&d), "");
        }

        /// Regression: "work púh" + BS×2 + retype "ush" must produce "púh"
        /// again, not "uúh". Root cause: after Telex 's' (tone mark) produces
        /// bs=1 commit='ú', raw_word held "pus" for screen "pú". BS×1 popped
        /// only 'h', BS×2 popped only 's' (not 'u'), leaving raw_word="pu"
        /// for screen "p". Engine re-seeded with "pu" instead of "p".
        #[test]
        fn v1_bs_after_tone_pops_correct_raw_width() {
            // Type "push" in v1 path:
            //   'p' → ForwardRaw                raw_word="p",    widths=[1]
            //   'u' → ForwardRaw                raw_word="pu",   widths=[1,1]
            //   's' → Consumed (bs=1 commit='ú') raw_word="pus",  widths=[1,2]
            //   'h' → ForwardRaw                raw_word="push", widths=[1,2,1]
            let mut d = v1_daemon();
            d.handle_char_inner(0, 'p', true);
            d.handle_char_inner(0, 'u', true);
            d.handle_char_inner(0, 's', true);
            d.handle_char_inner(0, 'h', true);
            assert_eq!(raw_word(&d), "push");

            // BS×2: deletes screen chars 'h' then 'ú'.
            // BS#1: pop width=1 → pop 1 raw ('h')   → raw_word="pus"
            // BS#2: pop width=2 → pop 2 raw ('s','u') → raw_word="p"
            d.handle_backspace();
            d.handle_backspace();

            assert_eq!(
                raw_word(&d),
                "p",
                "after BS×2 over 'h' and 'ú' (raw 'u'+'s'), raw_word must be 'p' not 'pu'"
            );
        }

        /// After the correct BS, retyping "ush" must seed with "p" not "pu",
        /// so 's' fires bs=1 commit='ú' (not bs=2 commit='úu').
        #[test]
        fn v1_retype_after_bs_over_tone_char_seeds_correctly() {
            let mut d = v1_daemon();
            d.handle_char_inner(0, 'p', true);
            d.handle_char_inner(0, 'u', true);
            d.handle_char_inner(0, 's', true); // 'ú'
            d.handle_char_inner(0, 'h', true);
            d.handle_backspace(); // delete 'h'
            d.handle_backspace(); // delete 'ú' (pops both 'u' and 's')

            // Re-type 'u': engine seeded with "p" → process_key('u') → ForwardRaw
            let r_u = d.handle_char_inner(0, 'u', true);
            // 'u' after bare 'p' is ForwardRaw (no vowel-modify rule here)
            assert!(
                matches!(r_u, viet_ime_wayland_adapter::KeyDecision::ForwardRaw),
                "re-typed 'u' must be ForwardRaw (seed was 'p', not 'pu')"
            );

            // Re-type 's': engine seeded with "pu" → process_key('s') → bs=1 commit='ú'
            let r_s = d.handle_char_inner(0, 's', true);
            match r_s {
                viet_ime_wayland_adapter::KeyDecision::Apply { backspaces, ref commit, .. } => {
                    assert_eq!(backspaces, 1, "'s' must produce exactly 1 backspace");
                    assert_eq!(commit, "ú", "'s' after 'pu' must commit 'ú'");
                }
                _other => panic!("expected Apply for 's', got something else"),
            }
        }
    }
}
