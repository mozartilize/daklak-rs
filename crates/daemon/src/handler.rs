//! Daemon: the orchestration / policy layer. Owns config, the on/off gate,
//! modifier + focus lifecycle, capability detection, and key routing. The
//! composition brain lives in [`crate::composer::Composer`]; the per-transport
//! wire glue lives in [`crate::transport`]. `Daemon` keeps a single live
//! `Composer` and exposes transport-neutral entry points that the glue calls.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, KeyDecision, ModifierState,
};
use viet_ime_wayland_adapter::FrameSnapshot;

#[cfg(feature = "ibus")]
use crate::composer::CharCursor;
use crate::composer::Composer;
use crate::config::Config;
use crate::control;

// Linux evdev code for Backspace.
pub(crate) const KEY_BACKSPACE: u32 = 14;
#[cfg(feature = "ibus")]
pub(crate) const KEY_ESC: u32 = 1;
// Navigation keys that move the cursor — trigger shadow reset.
pub(crate) const NAV_KEYS: &[u32] = &[
    105, 106, 103, 108, // Left, Right, Up, Down
    102, 107, // Home, End
    104, 109, // PageUp, PageDown
];

/// Router-owned lifecycle and composition state. `Daemon` holds config/policy;
/// `Router` holds the active session, focus, modifiers, and enabled edge state.
pub struct Router {
    pub modifiers: ModifierState,
    pub current_active: bool,

    /// True when `current_active` was synthesized by daklak (a focused
    /// toplevel exposed a virtual keyboard but never enabled text-input, so
    /// no compositor `Activate` fired) rather than driven by a compositor
    /// `zwp_input_method_v2::Activate` event. Real activate always wins.
    pub synthetic_active: bool,

    /// Single live composition state. Shape unchanged from the old
    /// `window: Option<WindowState>` — only the type name changed. The
    /// per-session HashMap is a deferred follow-up, not here.
    pub composer: Option<Composer>,

    /// Focused window's `app_id` captured at activate. Threaded into the
    /// capability probe so known-broken-on-ForwardKey terminals can
    /// auto-escalate. None outside an active session.
    pub focused_app_id: Option<String>,

    /// Previous value of `enabled`; used for edge-detection (on→off triggers
    /// a lazy full_reset on the next keystroke instead of from the control task).
    pub(crate) last_enabled: bool,
}

impl Router {
    fn new() -> Self {
        Self {
            modifiers: ModifierState::empty(),
            current_active: false,
            synthetic_active: false,
            composer: None,
            focused_app_id: None,
            last_enabled: true,
        }
    }
}

/// Orchestration state. Owns config + shared policy; delegates active lifecycle
/// and key-routing state to [`Router`].
pub struct Daemon {
    pub config: Config,

    pub router: Router,

    /// Forced tier for `purpose == PURPOSE_TERMINAL`, read once from
    /// `DAKLAK_TERMINAL_TIER` at startup. None → detect_method default.
    pub terminal_override: Option<BackspaceMethod>,

    /// Shared on/off flag — written by the control task, read each keystroke.
    pub enabled: Arc<AtomicBool>,

    /// Receiver for config changes sent by the tray / IPC menu actions.
    /// Polled before each keystroke handler call to pick up method,
    /// modern_style changes and apply them to the active composer.
    config_change_rx: tokio::sync::watch::Receiver<control::ConfigChange>,
    /// Last applied config values — used to avoid re-applying identical
    /// changes (which would reset the engine mid-word on every keystroke).
    last_config: control::ConfigChange,
}

/// Create a no-op config-change receiver (always reports `None`).
/// Used by tests and code paths that don't wire a config change channel.
#[cfg(test)]
pub(crate) fn noop_config_rx() -> tokio::sync::watch::Receiver<control::ConfigChange> {
    let (tx, _) = tokio::sync::watch::channel(control::ConfigChange::default());
    tx.subscribe()
}

impl Daemon {
    pub fn new(
        config: Config,
        enabled: Arc<AtomicBool>,
        config_change_rx: tokio::sync::watch::Receiver<control::ConfigChange>,
    ) -> Self {
        let terminal_override = match std::env::var("DAKLAK_TERMINAL_TIER")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
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

        let initial_method = config.method;
        let initial_modern_style = config.modern_style;

        Self {
            config,
            router: Router::new(),
            terminal_override,
            enabled,
            config_change_rx,
            last_config: control::ConfigChange {
                method: initial_method,
                modern_style: initial_modern_style,
            },
        }
    }

    pub(crate) fn detect_capability(&self, frame: &FrameSnapshot) -> BackspaceMethod {
        let probe = CapabilityProbe {
            purpose: frame.purpose,
            surrounding_text_seen: frame.surrounding_text.is_some(),
            terminal_override: self.terminal_override,
        };
        detect_method(&probe)
    }

    // ── transport-neutral key routing (non-wayland: ibus, evdev) ────────────

    /// Apply the enabled on→off edge and report whether the IME is currently
    /// enabled. When daklak is toggled off mid-word this resets the in-flight
    /// composition so the next enable starts clean. Shared by the evdev and
    /// ibus transports: both must forward keys raw while disabled instead of
    /// continuing to compose. (The wayland path inlines the same logic with an
    /// extra key-repeat guard.)
    pub(crate) fn sync_enabled_edge(&mut self) -> bool {
        let now_enabled = self.enabled.load(Ordering::Acquire);
        if self.router.last_enabled && !now_enabled {
            if let Some(w) = self.router.composer.as_mut() {
                w.full_reset();
            }
        }
        self.router.last_enabled = now_enabled;
        now_enabled
    }

    /// Poll the config-change channel and apply pending changes (method,
    /// modern_style) to the active composer. Called at the top of every
    /// transport entry point — the change takes effect within one keystroke.
    pub(crate) fn sync_config(&mut self) {
        if self.config_change_rx.has_changed().unwrap_or(false) {
            let change = *self.config_change_rx.borrow_and_update();
            if change.no_change_from(&self.last_config) {
                return;
            }
            // Method change — reset engine.
            if change.method != self.last_config.method {
                self.config.method = change.method;
                if let Some(c) = self.router.composer.as_mut() {
                    c.set_input_method(change.method.to_engine());
                }
                tracing::info!(method = ?change.method, "config: method changed at runtime");
            }
            // Modern_style change — no engine reset needed.
            if change.modern_style != self.last_config.modern_style {
                self.config.modern_style = change.modern_style;
                if let Some(c) = self.router.composer.as_mut() {
                    c.set_modern_style(change.modern_style);
                }
                tracing::info!(
                    modern_style = change.modern_style,
                    "config: modern_style changed at runtime"
                );
            }
            self.last_config = change;
        }
    }

    /// Like wayland's `on_key_pressed` but without `AdapterCtx`. NAV and
    /// modifier-shortcut keys return `ForwardRaw` instead of `Consumed` (the
    /// caller just passes through). Runs the engine continuously and seeds from
    /// shadow at word start, same as the wayland path.
    #[cfg(feature = "ibus")]
    pub fn process_key(&mut self, key: u32, ch: Option<char>) -> KeyDecision {
        self.sync_config();
        if let Some(w) = self.router.composer.as_mut() {
            w.check_idle_reset();
        }
        if !self.sync_enabled_edge() {
            return KeyDecision::ForwardRaw;
        }
        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.router.modifiers.intersects(shortcut_mods) {
            if let Some(w) = self.router.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        }
        if NAV_KEYS.contains(&key) {
            if let Some(w) = self.router.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        }
        if self.router.composer.is_none() {
            return KeyDecision::ForwardRaw;
        }
        if key == KEY_BACKSPACE {
            return self.handle_backspace();
        }
        if key == KEY_ESC {
            if let Some(w) = self.router.composer.as_mut() {
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        }
        if let Some(w) = self.router.composer.as_mut() {
            w.mark_action();
        }
        let Some(ch) = ch else {
            // Non-printable key (Enter, Tab, Escape, …) ends the current word.
            // Clear the composition so the next char starts fresh and isn't
            // seeded with this line's word (e.g. after "hiếu"⏎ the next key must
            // start a new word, not extend "hiếu").
            if let Some(w) = self.router.composer.as_mut() {
                w.full_reset();
                w.defer_action();
            }
            return KeyDecision::ForwardRaw;
        };
        self.handle_char(ch)
    }

    // ── thin delegations to the Composer brain (trait surface + tests) ──────

    /// Feed a printable char to the composer. The engine runs continuously
    /// (no per-key reset) for every transport — wayland, IBus, evdev — building
    /// on the prior key's state, with a render-gated shadow seed at word start.
    pub fn handle_char(&mut self, ch: char) -> KeyDecision {
        self.sync_config();
        match self.router.composer.as_mut() {
            Some(w) => w.feed_key(ch),
            None => KeyDecision::ForwardRaw,
        }
    }

    pub fn handle_backspace(&mut self) -> KeyDecision {
        self.sync_config();
        match self.router.composer.as_mut() {
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
        if let Some(w) = self.router.composer.as_mut() {
            // Echo-based ST → FK downgrade for clients that advertise
            // surrounding-text but silently no-op DeleteSurroundingText (Google
            // Docs under IBus), doubling each correction (`Tiếng` →
            // `Tieêngếng`). A functional client echoes every edit back as a
            // SetSurroundingText; Docs sends nothing. Capability bits can't
            // tell them apart (Docs flaps caps=9/caps=41 in one focus
            // sequence), but the missing echo can. Check BEFORE applying so the
            // offending correction routes through ForwardKey (real BackSpace
            // ForwardKeyEvents + CommitText, both honored by Docs). Only
            // corrections that delete (`backspaces > 0`) can double a word.
            if backspaces > 0
                && w.method() == BackspaceMethod::SurroundingText
                && w.note_surrounding_correction()
            {
                tracing::info!(
                    from = ?BackspaceMethod::SurroundingText,
                    to = ?BackspaceMethod::ForwardKey,
                    "surrounding-text corrections drew no echo (delete not honored) → downgrade tier"
                );
                w.set_method(BackspaceMethod::ForwardKey);
            }
            w.apply(backspaces, commit, time, sink);
        }
    }

    /// Update shadow + engine seed from surrounding text (IBus
    /// `SetSurroundingText`; cursor in chars). Skips reseed within 150 ms of a
    /// daklak action (our own echo) and on mid-word 1-char insertions.
    #[cfg(feature = "ibus")]
    pub fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) {
        if let Some(w) = self.router.composer.as_mut() {
            // A frame arrived → the client echoed our edit, i.e. it honored
            // delete_surrounding_text. Opens the echo window so the echo-based
            // downgrade (`note_surrounding_correction`, in `apply_with_sink`)
            // won't strike the in-flight correction. Mark before any
            // early-return guard so every frame counts.
            w.mark_surrounding_frame_seen();
            // Runtime tier downgrade ST → FK. Same watchdog as the wayland
            // path: clients that advertise surrounding-text but never honor it
            // (Google Docs / Firefox contenteditable echo text="" cursor=0
            // forever and no-op delete_surrounding_text, doubling each
            // correction's commit). Must run BEFORE the duplicate-frame guard
            // because every dead frame is byte-identical. Viable on GNOME now
            // that the upstream mutter ForwardKey-drop fix is in play.
            if w.method() == BackspaceMethod::SurroundingText
                && w.note_surrounding_liveness(text, cursor)
            {
                tracing::info!(
                    from = ?BackspaceMethod::SurroundingText,
                    to = ?BackspaceMethod::ForwardKey,
                    "surrounding text non-functional (empty frames despite commits) → downgrade tier"
                );
                w.set_method(BackspaceMethod::ForwardKey);
                // Mirror the wayland path: dead ST ⟹ dead commit_string. The
                // IBus sink doesn't read this flag today, but keep the composer
                // state coherent across transports.
                w.commit_string_functional = false;
            }
            if w.should_skip_surrounding_frame(text, cursor, anchor, false, false) {
                return;
            }
            w.observe_surrounding_chars(text, CharCursor(cursor), CharCursor(anchor));
        }
    }

    // ── focus / session lifecycle ──────────────────────────────────────────

    /// Bootstrap a synthetic session for evdev-only mode. Sets up a composer
    /// so `handle_char` / `handle_backspace` work without a Wayland
    /// compositor. The evdev adapter emits backspaces + the replacement string
    /// directly via uinput and ignores `BackspaceMethod`, so the tier here is
    /// only a label.
    #[cfg(feature = "evdev_grab")]
    pub fn activate_evdev(&mut self) {
        let mut c = Composer::new(
            self.config.method.to_engine(),
            BackspaceMethod::ForwardKey,
            self.config.bracket_shortcuts,
        );
        c.set_modern_style(self.config.modern_style);
        c.set_modifiers(self.router.modifiers);
        self.router.current_active = true;
        self.router.synthetic_active = true;
        self.router.composer = Some(c);
    }

    /// Create a composer session for a non-Wayland transport (IBus). Idempotent.
    #[cfg(feature = "ibus")]
    pub fn activate_ibus(&mut self, method: viet_ime_edit_strategy::BackspaceMethod) {
        if self.router.current_active {
            return;
        }
        let mut c = Composer::new(
            self.config.method.to_engine(),
            method,
            self.config.bracket_shortcuts,
        );
        c.set_modern_style(self.config.modern_style);
        c.set_modifiers(self.router.modifiers);
        self.router.composer = Some(c);
        self.router.current_active = true;
    }

    /// Tear down the IBus session. Clears composition state.
    #[cfg(feature = "ibus")]
    pub fn deactivate_ibus(&mut self) {
        if let Some(w) = self.router.composer.as_mut() {
            w.full_reset();
        }
        self.router.composer = None;
        self.router.current_active = false;
    }

    /// React to an IBus routing change while a session is live.
    #[cfg(feature = "ibus")]
    pub fn update_ibus_method(&mut self, want: BackspaceMethod) {
        if let Some(w) = self.router.composer.as_mut() {
            if w.method() != want {
                tracing::info!(?want, "ibus: method updated");
                w.set_method(want);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Daemon;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn test_daemon() -> Daemon {
        Daemon::new(
            Config::default(),
            Arc::new(AtomicBool::new(true)),
            super::noop_config_rx(),
        )
    }

    #[test]
    fn daemon_initializes_router_lifecycle_state() {
        let d = test_daemon();

        assert!(!d.router.current_active);
        assert!(!d.router.synthetic_active);
        assert!(d.router.composer.is_none());
        assert!(d.router.focused_app_id.is_none());
        assert!(d.router.last_enabled);
    }

    mod surrounding_anchor_regression {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{
            BackspaceMethod, KeyDecision, KeyState, OutputSink,
        };
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
        fn activation_honors_legacy_tone_config() {
            let mut cfg = Config::default();
            cfg.modern_style = false;
            let mut daemon = Daemon::new(
                cfg,
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );

            let mut state = AdapterState::new();
            let mut ctx = AdapterCtx { state: &mut state };
            let mut activate = frame("", 0, 0);
            activate.activate = true;
            activate.surrounding_text = None;
            daemon.on_done_frame(&mut ctx, &activate);

            let mut visible = String::new();
            for ch in "hoaf".chars() {
                match daemon.handle_char(ch) {
                    KeyDecision::ForwardRaw => visible.push(ch),
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                    }
                    KeyDecision::Consumed => {}
                }
            }

            assert_eq!(visible, "hòa");
        }

        #[test]
        fn detect_capability_terminal_defaults_to_forward_key() {
            // Terminal purpose (13) with no override resolves to ForwardKey
            // regardless of surrounding_text presence.
            let d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), super::super::noop_config_rx());
            let mut f = frame("", 0, 0);
            f.purpose = 13;
            assert_eq!(d.detect_capability(&f), BackspaceMethod::ForwardKey);
        }

        #[test]
        fn native_focus_without_text_input_synthesizes_forward_key_session() {
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let mut state = AdapterState::new();
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_focus_changed(
                &mut ctx,
                Some("com.mitchellh.ghostty".to_owned()),
                false,
            );

            assert!(daemon.router.current_active);
            assert!(daemon.router.synthetic_active);
            assert_eq!(
                daemon.router.focused_app_id.as_deref(),
                Some("com.mitchellh.ghostty")
            );
            // The synthetic session merges the former Tier 4 VkOnly into
            // ForwardKey with a dead commit_string, so replacements route
            // through the virtual keyboard's synthetic keymap.
            let composer = daemon.router.composer.as_ref().expect("composer");
            assert_eq!(composer.method(), BackspaceMethod::ForwardKey);
            assert!(!composer.commit_string_functional);
        }

        #[test]
        fn focus_without_vk_does_not_synthesize_session() {
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let mut state = AdapterState::new();
            state.profile.has_vk_keyboard = false;
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_focus_changed(
                &mut ctx,
                Some("com.mitchellh.ghostty".to_owned()),
                false,
            );

            assert!(!daemon.router.current_active);
            assert!(!daemon.router.synthetic_active);
            assert!(daemon.router.composer.is_none());
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
            let mut daemon = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), super::super::noop_config_rx());
            daemon.router.current_active = true;
            daemon.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            let mut state = AdapterState::new();
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 3));
            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 9));

            let mut sink = DeleteCaptureSink::default();
            let w = daemon
                .router
                .composer
                .as_mut()
                .expect("composer state exists");
            w.apply_to_sink(1, "â", 1, 0, &mut sink);

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

    // ── Continuous-engine seed model: every transport (wayland, IBus, evdev)
    //    runs the engine continuously and seeds from shadow at word start.
    //    These pin that behavior at the routing surface.
    mod continuous_seed {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::BackspaceMethod;
        use viet_ime_edit_strategy::KeyDecision;
        use viet_ime_engine::InputMethod;

        fn daemon() -> Daemon {
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), super::super::noop_config_rx());
            d.router.current_active = true;
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));
            d
        }

        #[test]
        fn sync_enabled_edge_gates_and_resets_inflight() {
            use std::sync::atomic::Ordering;
            let mut d = daemon();
            // Enabled: gate open, no reset.
            assert!(d.sync_enabled_edge());
            // Build in-flight composition state.
            let _ = d.handle_char('h');
            assert!(d.router.composer.as_ref().unwrap().last_input_char.is_some());
            // Toggle off: the on→off edge closes the gate and clears the word
            // so a later enable starts clean (mirrors what the evdev/ibus
            // transports rely on to stop composing while daklak is "off").
            d.enabled.store(false, Ordering::Release);
            assert!(!d.sync_enabled_edge());
            assert!(d.router.composer.as_ref().unwrap().last_input_char.is_none());
        }

        #[test]
        fn typing_english_word_stays_raw() {
            // No surrounding text: "word" must come out verbatim, never
            // recomposed into Vietnamese on the continuous path.
            let mut d = daemon();
            let mut visible = String::new();
            for ch in "word".chars() {
                match d.handle_char(ch) {
                    KeyDecision::ForwardRaw => visible.push(ch),
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                    }
                    KeyDecision::Consumed => {}
                }
            }
            assert_eq!(visible, "word");
        }

        #[cfg(feature = "ibus")]
        #[test]
        fn ibus_escape_does_not_clear_current_word_context() {
            let mut d = daemon();
            d.process_key(35, Some('h'));
            d.process_key(24, Some('o'));
            d.process_key(49, Some('n'));

            // KEY_ESC is commonly used to dismiss autocomplete popups. It should
            // pass through without clearing the word being edited.
            d.process_key(1, None);

            match d.process_key(17, Some('w')) {
                KeyDecision::Apply {
                    backspaces,
                    ref commit,
                    ..
                } => {
                    assert_eq!(backspaces, 2);
                    assert_eq!(commit, "ơn");
                }
                _other => panic!("expected Apply for 'w' after hon + Esc"),
            }
            match d.process_key(31, Some('s')) {
                KeyDecision::Apply {
                    backspaces,
                    ref commit,
                    ..
                } => {
                    assert_eq!(backspaces, 2);
                    assert_eq!(commit, "ớn");
                }
                _other => panic!("expected Apply for 's' after honw + Esc"),
            }
        }
    }

    /// Key-REPEAT (wl_keyboard state=2) handling. A repeat must reach the
    /// client for nav / shortcut keys (so Ctrl+Arrow, hold-Arrow move the
    /// cursor on rate-0 clients like Chromium on KWin) but must never mutate
    /// the compose engine or re-type a held letter. The adapter sets
    /// `forwarding_repeat` on the state, which `ctx.is_repeat()` exposes.
    mod key_repeat {
        use super::super::{Daemon, NAV_KEYS};
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision, ModifierState};
        use viet_ime_engine::InputMethod;
        use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, AdapterState};

        const KEY_A: u32 = 30;
        const KEY_LEFT: u32 = 105;

        fn v1_daemon() -> Daemon {
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), super::super::noop_config_rx());
            d.router.current_active = true;
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));
            d
        }

        /// Drive the trait `on_key_pressed` with `forwarding_repeat = repeat`.
        fn key(d: &mut Daemon, key: u32, ch: Option<char>, repeat: bool) -> KeyDecision {
            let mut state = AdapterState::new();
            state.forwarding_repeat = repeat;
            let mut ctx = AdapterCtx { state: &mut state };
            d.on_key_pressed(&mut ctx, 0, key, ch)
        }

        #[test]
        fn compose_key_repeat_is_swallowed() {
            let mut d = v1_daemon();
            key(&mut d, KEY_A, Some('a'), false);
            // Holding the letter must NOT re-type it: the repeat is swallowed
            // (Consumed) so the engine is untouched and 'a' isn't duplicated.
            let decision = key(&mut d, KEY_A, Some('a'), true);
            assert!(
                matches!(decision, KeyDecision::Consumed),
                "compose-key repeat is swallowed"
            );
        }

        #[test]
        fn nav_key_repeat_is_consumed_and_forwarded() {
            let mut d = v1_daemon();
            key(&mut d, KEY_LEFT, None, false);
            // A nav repeat is consumed (forwarded with state=2 via the ctx
            // helper, not routed into the engine).
            let decision = key(&mut d, KEY_LEFT, None, true);
            assert!(
                matches!(decision, KeyDecision::Consumed),
                "nav repeat forwards + consumes"
            );
            assert_eq!(
                NAV_KEYS.contains(&KEY_LEFT),
                true,
                "sanity: Left is a nav key"
            );
        }

        #[test]
        fn modifier_shortcut_repeat_is_consumed() {
            let mut d = v1_daemon();
            // Ctrl held → Ctrl+Left takes the modifier-shortcut branch, which
            // sits above the compose-swallow so its repeats still forward.
            d.router.modifiers = ModifierState::CTRL;
            let decision = key(&mut d, KEY_LEFT, None, true);
            assert!(
                matches!(decision, KeyDecision::Consumed),
                "Ctrl+Left repeat forwards + consumes"
            );
        }
    }

    /// Shift-held passthrough chars on the v1/KWin ForwardKey path. KWin's
    /// notifyKeyboardKey doesn't refresh the client's modifier state for an
    /// IM-forwarded keycode, so a raw-forwarded shifted letter loses its case.
    /// The transport commits the already-decoded char as text instead.
    mod shifted_passthrough {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision};
        use viet_ime_engine::InputMethod;
        use viet_ime_keymap::xkb::XkbState;
        use viet_ime_wayland_adapter::{
            AdapterCtx, AdapterHandler, AdapterState, ImProtocol, TransportProfile,
        };

        const KEY_L: u32 = 38;

        fn daemon() -> Daemon {
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), super::super::noop_config_rx());
            d.router.current_active = true;
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::ForwardKey,
                false,
            ));
            d
        }

        /// Drive `on_key_pressed` with a real `us` keymap installed so
        /// `is_level_shifted` can compare the decoded char to its base level.
        /// `ch` is the already-decoded char the adapter would pass.
        fn key(d: &mut Daemon, protocol: ImProtocol, ch: char) -> KeyDecision {
            let mut state = AdapterState::new();
            state.profile = TransportProfile::for_protocol(protocol);
            state.xkb = Some(XkbState::us_for_test());
            let mut ctx = AdapterCtx { state: &mut state };
            d.on_key_pressed(&mut ctx, 0, KEY_L, Some(ch))
        }

        #[test]
        fn v1_level_shifted_passthrough_commits_decoded_char() {
            let mut d = daemon();
            // 'L' is a consonant the engine forwards (doesn't compose alone).
            // Its base level for key 38 is 'l', so 'L' is level-shifted → on v1
            // it must be committed as text, not forwarded as a raw keycode.
            match key(&mut d, ImProtocol::ImV1, 'L') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 0);
                    assert_eq!(commit, "L", "decoded char committed verbatim");
                }
                _ => panic!("expected Apply commit for level-shifted passthrough on v1"),
            }
        }

        #[test]
        fn v1_level_shifted_passthrough_is_not_recorded_as_raw_forward() {
            let mut d = daemon();

            let decision = key(&mut d, ImProtocol::ImV1, 'L');

            assert!(matches!(decision, KeyDecision::Apply { .. }));
            assert_eq!(
                d.router.composer.as_ref().unwrap().shadow_text(),
                "",
                "the deferred Apply will record the committed char; pre-recording it as raw input doubles the shadow"
            );
        }

        #[test]
        fn v1_base_level_passthrough_still_forwards_raw() {
            let mut d = daemon();
            // 'l' IS the base level for key 38 → not level-shifted → the common
            // path is unchanged (raw keycode forward).
            assert!(
                matches!(key(&mut d, ImProtocol::ImV1, 'l'), KeyDecision::ForwardRaw),
                "base-level passthrough still forwards the raw keycode"
            );
        }

        #[test]
        fn v2_level_shifted_passthrough_still_forwards_raw() {
            let mut d = daemon();
            // v2 (no keysym-commit) carries modifiers on the vk path, so the
            // divert must NOT apply even for a level-shifted char.
            assert!(
                matches!(key(&mut d, ImProtocol::ImV2, 'L'), KeyDecision::ForwardRaw),
                "v2 keeps raw forward; the KWin modifier quirk doesn't apply"
            );
        }
    }

    // ── Config-change coalescing regression tests ──────────────────────
    //
    // The watch-based config channel carries a full-state `ConfigChange`
    // struct so fast sequential tray clicks (method change + legacy toggle)
    // never lose an update. Further, `sync_config()` must only apply deltas
    // — identical values must NOT reset the engine (or mid-word composition
    // breaks on every keystroke).
    mod sync_config {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::{Config, MethodConfig};
        use crate::control::ConfigChange;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use tokio::sync::watch;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision};
        use viet_ime_engine::InputMethod;

        #[test]
        fn full_state_struct_applies_method_and_modern_style_together() {
            // The tray always sends a complete `ConfigChange{method,modern_style}`.
            // `sync_config()` must apply both fields from a single update.
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);
            assert_eq!(d.config.method, MethodConfig::Telex);
            assert!(d.config.modern_style);

            // Send VNI + legacy-style in one atomic send.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
            });
            d.sync_config();

            assert_eq!(
                d.config.method,
                MethodConfig::Vni,
                "method must flip from a full-state send"
            );
            assert!(
                !d.config.modern_style,
                "modern_style must flip from the same send"
            );
        }

        #[test]
        fn noop_update_does_not_reset_engine_inflight() {
            // Identical config after `sync_config()` must NOT trigger
            // `set_input_method` (which resets the engine word).
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);
            d.router.current_active = true;
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            // Build an in-flight composition state.
            let _ = d.handle_char('h');
            assert!(
                d.router.composer.as_ref().unwrap().last_input_char.is_some(),
                "engine must have state after handle_char"
            );

            // Send the SAME config we already have.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Telex,
                modern_style: true,
            });
            d.sync_config();

            // The in-flight state must survive (no silent reset).
            assert!(
                d.router.composer.as_ref().unwrap().last_input_char.is_some(),
                "no-op sync_config() must not reset the engine"
            );
        }

        #[test]
        fn sequential_sends_apply_latest_config() {
            // The actual coalescing scenario: two back-to-back sends before
            // `sync_config()` reads. The watch channel only retains the
            // latest, so the tray must always send a *full* ConfigChange.
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);

            // Two rapid sends before any poll.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: true,
            });
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Telex,
                modern_style: false,
            });

            d.sync_config();

            // Both fields must reflect the LATEST send, not an intermediate.
            assert_eq!(d.config.method, MethodConfig::Telex);
            assert!(!d.config.modern_style, "modern_style from the second send");
        }

        #[test]
        fn method_change_preserves_legacy_tone_placement() {
            // A mode switch resets the underlying engine. If legacy tone
            // placement was already active, that reset must not silently
            // restore vnkey-engine's default modern placement.
            let (tx, rx) = watch::channel(ConfigChange {
                method: MethodConfig::Telex,
                modern_style: false,
            });
            let mut cfg = Config::default();
            cfg.modern_style = false;
            let mut d = Daemon::new(cfg, Arc::new(AtomicBool::new(true)), rx);
            d.router.current_active = true;
            let mut composer = Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            );
            composer.set_modern_style(false);
            d.router.composer = Some(composer);

            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
            });
            d.sync_config();
            assert_eq!(d.config.method, MethodConfig::Vni);

            let mut visible = String::new();
            for ch in "hoa2".chars() {
                match d.handle_char(ch) {
                    KeyDecision::ForwardRaw => visible.push(ch),
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                    }
                    KeyDecision::Consumed => {}
                }
            }

            assert_eq!(visible, "hòa");
        }

        #[test]
        fn method_change_resets_engine_state() {
            // When method actually changes, `set_input_method()` must fire
            // and reset the in-flight composition.
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);
            d.router.current_active = true;
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            // Build in-flight state.
            let _ = d.handle_char('h');
            assert!(d.router.composer.as_ref().unwrap().last_input_char.is_some());

            // Method changes: Telex → Vni.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: true,
            });
            d.sync_config();

            // Engine must have been reset by set_input_method.
            assert!(
                d.router.composer.as_ref().unwrap().last_input_char.is_none(),
                "engine must reset on method change"
            );
            // Config field persisted.
            assert_eq!(d.config.method, MethodConfig::Vni);
        }
    }
}
