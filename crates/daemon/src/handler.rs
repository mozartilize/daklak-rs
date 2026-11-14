//! Daemon: the orchestration / policy layer. Owns config, the on/off gate,
//! modifier + focus lifecycle, capability detection, and key routing. The
//! composition brain lives in [`crate::composer::Composer`]; the per-transport
//! wire glue lives in [`crate::transport`]. `Daemon` keeps a single live
//! `Composer` and exposes transport-neutral entry points that the glue calls.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision, ModifierState};
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

    /// The live session's composition state; `Some` exactly while a session
    /// is active. There is one session at a time.
    pub composer: Option<Composer>,

    /// True when the session was synthesized by daklak (a focused toplevel
    /// that never enabled text-input, or evdev mode) rather than started by
    /// a compositor `Activate`. A real activate always replaces it.
    pub synthetic_active: bool,

    /// Focused window's `app_id` captured when the session started; `None`
    /// outside a session. A focus-synthesized session restarts when it
    /// changes, so a half-typed word never carries over to another window.
    pub focused_app_id: Option<String>,

    /// Previous value of `enabled`; used for edge-detection (on→off triggers
    /// a lazy full_reset on the next keystroke instead of from the control task).
    pub(crate) last_enabled: bool,
}

impl Router {
    fn new() -> Self {
        Self {
            modifiers: ModifierState::empty(),
            composer: None,
            synthetic_active: false,
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

    /// Shared on/off flag — written by the control task, read each keystroke.
    pub enabled: Arc<AtomicBool>,

    /// Passthrough flag for the IBus transport: while a layered evdev grab
    /// owns the keyboard, the IBus connection stays alive and passes every key
    /// through raw so both backends never compose the same key. (The Wayland
    /// transport is torn down for evdev instead, so it never reads this.)
    /// Distinct from `enabled`, the user's on/off gate. Written by the
    /// supervisor only.
    #[cfg_attr(not(feature = "ibus"), allow(dead_code))]
    pub suspended: Arc<AtomicBool>,

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
        let initial_method = config.method;
        let initial_modern_style = config.modern_style;
        let initial_auto_capitalize = config.auto_capitalize;
        let initial_double_space_period = config.double_space_period;
        let initial_evdev_grab = config.enable_evdev_grab;

        Self {
            config,
            router: Router::new(),
            enabled,
            suspended: Arc::new(AtomicBool::new(false)),
            config_change_rx,
            last_config: control::ConfigChange {
                method: initial_method,
                modern_style: initial_modern_style,
                auto_capitalize: initial_auto_capitalize,
                double_space_period: initial_double_space_period,
                enable_evdev_grab: initial_evdev_grab,
            },
        }
    }

    /// Clone the suspended flag so the supervisor can pause/resume this
    /// transport while a layered evdev grab owns the keyboard.
    #[cfg(feature = "ibus")]
    pub fn suspend_flag(&self) -> Arc<AtomicBool> {
        self.suspended.clone()
    }

    /// A composer set up from the current config. Every transport starts its
    /// session with this, so a config option is applied in one place.
    pub(crate) fn new_composer(&self, method: BackspaceMethod) -> Composer {
        let mut c = Composer::new(
            self.config.method.to_engine(),
            method,
            self.config.bracket_shortcuts,
        );
        c.set_modern_style(self.config.modern_style);
        c.set_v2_key_replacement(self.config.v2_key_replacement);
        c.set_auto_capitalize(self.config.auto_capitalize);
        c.set_double_space_period(self.config.double_space_period);
        c
    }

    /// End the current session, whether real or synthesized.
    pub(crate) fn end_session(&mut self) {
        self.router.composer = None;
        self.router.synthetic_active = false;
        self.router.focused_app_id = None;
    }

    /// Tier at activation: `SurroundingText` if the client sent a
    /// surrounding-text frame (even an empty one), else `ForwardKey`.
    /// Clients without text-input never activate; they get a focus-synthesized
    /// `ForwardKey` session instead (`on_focus_changed`). The surrounding-text
    /// liveness check can downgrade a session at runtime; a session whose
    /// activate frame lacked surrounding text can upgrade on a later frame.
    pub(crate) fn detect_capability(&self, frame: &FrameSnapshot) -> BackspaceMethod {
        if frame.surrounding_text.is_some() {
            BackspaceMethod::SurroundingText
        } else {
            BackspaceMethod::ForwardKey
        }
    }

    // ── transport-neutral key routing (non-wayland: ibus, evdev) ────────────

    /// Apply the enabled on→off edge and report whether the IME is currently
    /// enabled. When daklak is toggled off mid-word this resets the in-flight
    /// composition so the next enable starts clean. Shared by the evdev, ibus,
    /// and wayland transports: all must forward keys raw while disabled instead
    /// of continuing to compose. Wayland skips this on key-repeat so the
    /// enabled edge is not observed twice.
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

    pub(crate) fn with_composer(&mut self, f: impl FnOnce(&mut Composer)) {
        if let Some(w) = self.router.composer.as_mut() {
            f(w);
        }
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
            if change.auto_capitalize != self.last_config.auto_capitalize {
                self.config.auto_capitalize = change.auto_capitalize;
                if let Some(c) = self.router.composer.as_mut() {
                    c.set_auto_capitalize(change.auto_capitalize);
                }
                tracing::info!(
                    auto_capitalize = change.auto_capitalize,
                    "config: auto_capitalize changed at runtime"
                );
            }
            if change.double_space_period != self.last_config.double_space_period {
                self.config.double_space_period = change.double_space_period;
                if let Some(c) = self.router.composer.as_mut() {
                    c.set_double_space_period(change.double_space_period);
                }
                tracing::info!(
                    double_space_period = change.double_space_period,
                    "config: double_space_period changed at runtime"
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
        if self.suspended.load(Ordering::Acquire) {
            if let Some(w) = self.router.composer.as_mut() {
                w.full_reset();
            }
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

    /// Bootstrap a synthetic session for evdev mode. Sets up a composer
    /// so `handle_char` / `handle_backspace` work without a Wayland
    /// compositor. The evdev adapter emits backspaces + the replacement string
    /// directly via uinput and ignores `BackspaceMethod`, so the tier here is
    /// only a label.
    #[cfg(feature = "evdev_grab")]
    pub fn activate_evdev(&mut self) {
        self.router.composer = Some(self.new_composer(BackspaceMethod::ForwardKey));
        self.router.synthetic_active = true;
    }

    /// Create a composer session for a non-Wayland transport (IBus). Idempotent.
    #[cfg(feature = "ibus")]
    pub fn activate_ibus(&mut self, method: viet_ime_edit_strategy::BackspaceMethod) {
        if self.router.composer.is_none() {
            self.router.composer = Some(self.new_composer(method));
        }
    }

    /// Tear down the IBus session. Clears composition state.
    #[cfg(feature = "ibus")]
    pub fn deactivate_ibus(&mut self) {
        self.end_session();
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

        assert!(d.router.composer.is_none());
        assert!(!d.router.synthetic_active);
        assert!(d.router.focused_app_id.is_none());
        assert!(d.router.last_enabled);
    }

    mod surrounding_anchor_regression {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision, KeyState, OutputSink};
        use viet_ime_engine::InputMethod;
        use viet_ime_wayland_adapter::{
            AdapterCtx, AdapterHandler, AdapterState, FrameSnapshot, ImProtocol,
        };

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

        /// Apply each key decision immediately, without client frame reports.
        fn typed(daemon: &mut Daemon, keys: &str) -> String {
            let mut visible = String::new();
            for ch in keys.chars() {
                match daemon.handle_char(ch) {
                    KeyDecision::ForwardRaw => visible.push(ch),
                    KeyDecision::Apply { backspaces, commit } => {
                        // As a transport does: the composer tracks its edit.
                        if let Some(w) = daemon.router.composer.as_mut() {
                            let mut sink = DeleteCaptureSink::default();
                            w.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                        }
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                    }
                    KeyDecision::Consumed => {}
                }
            }
            visible
        }

        #[test]
        fn activation_honors_legacy_tone_config() {
            let cfg = Config {
                modern_style: false,
                ..Config::default()
            };
            let mut daemon = Daemon::new(
                cfg,
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );

            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            let mut ctx = AdapterCtx { state: &mut state };
            let mut activate = frame("", 0, 0);
            activate.activate = true;
            activate.surrounding_text = None;
            daemon.on_done_frame(&mut ctx, &activate);

            assert_eq!(typed(&mut daemon, "hoaf"), "hòa");
        }

        #[test]
        fn every_session_kind_takes_its_options_from_the_config() {
            let daemon = || {
                Daemon::new(
                    Config {
                        modern_style: false,
                        auto_capitalize: true,
                        double_space_period: true,
                        ..Config::default()
                    },
                    Arc::new(AtomicBool::new(true)),
                    super::super::noop_config_rx(),
                )
            };
            // Double space → ". ", then a capitalized word with legacy tone
            // placement (`hòa`, where the modern style gives `hoà`).
            let check = |d: &mut Daemon, kind: &str| {
                assert_eq!(typed(d, "a  hoaf"), "a. Hòa", "{kind}");
            };
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            let mut ctx = AdapterCtx { state: &mut state };

            let mut d = daemon();
            let mut activate = frame("", 0, 0);
            activate.activate = true;
            activate.surrounding_text = None;
            d.on_done_frame(&mut ctx, &activate);
            check(&mut d, "text-input activate");

            let mut d = daemon();
            d.on_focus_changed(&mut ctx, Some("com.mitchellh.ghostty".to_owned()), false);
            check(&mut d, "focus without text-input");

            #[cfg(feature = "evdev_grab")]
            {
                let mut d = daemon();
                d.activate_evdev();
                check(&mut d, "evdev");
            }
            #[cfg(feature = "ibus")]
            {
                let mut d = daemon();
                d.activate_ibus(BackspaceMethod::ForwardKey);
                check(&mut d, "ibus");
            }
        }

        #[test]
        fn key_replacement_option_reaches_a_surrounding_text_session() {
            struct KeyChannel(Vec<String>);
            impl OutputSink for KeyChannel {
                fn delete_surrounding_text(&mut self, _: u32, _: u32, _: u32, _: u32) {}
                fn commit_string(&mut self, _: &str) {}
                fn commit(&mut self, _: u32) {}
                fn vk_key(&mut self, _: u32, _: u32, _: KeyState) {}
                fn prefer_key_channel_replacement(&self, _: &str) -> bool {
                    true
                }
                fn commit_key_channel_text(&mut self, _: u32, _: u32, text: &str) -> bool {
                    self.0.push(text.to_owned());
                    true
                }
            }

            for (enabled, expected) in [(true, vec!["a".to_owned()]), (false, vec![])] {
                let mut daemon = Daemon::new(
                    Config {
                        v2_key_replacement: enabled,
                        ..Config::default()
                    },
                    Arc::new(AtomicBool::new(true)),
                    super::super::noop_config_rx(),
                );
                let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
                let mut ctx = AdapterCtx { state: &mut state };
                let mut activate = frame("", 0, 0);
                activate.activate = true;
                daemon.on_done_frame(&mut ctx, &activate);

                let mut sink = KeyChannel(Vec::new());
                let composer = daemon.router.composer.as_mut().expect("composer");
                assert_eq!(composer.method(), BackspaceMethod::SurroundingText);
                composer.apply_to_sink(0, "a", 0, 0, &mut sink);
                assert_eq!(sink.0, expected, "v2_key_replacement={enabled}");
            }
        }

        #[test]
        fn deactivate_and_focus_loss_end_the_session() {
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            let mut ctx = AdapterCtx { state: &mut state };
            let app = || Some("com.mitchellh.ghostty".to_owned());
            let mut activate = frame("", 0, 0);
            activate.activate = true;
            activate.app_id = Some("org.gnome.TextEditor".to_owned());
            let mut deactivate = frame("", 0, 0);
            deactivate.deactivate = true;

            // A real activation replaces a focus-synthesized session.
            daemon.on_focus_changed(&mut ctx, app(), false);
            assert!(daemon.router.synthetic_active);
            daemon.on_done_frame(&mut ctx, &activate);
            assert!(!daemon.router.synthetic_active);
            assert_eq!(daemon.router.focused_app_id, activate.app_id);
            assert_eq!(
                daemon.router.composer.as_ref().map(Composer::method),
                Some(BackspaceMethod::SurroundingText)
            );

            daemon.on_done_frame(&mut ctx, &deactivate);
            assert!(daemon.router.composer.is_none());
            assert!(daemon.router.focused_app_id.is_none());

            // Focus leaving every toplevel ends a synthesized session.
            daemon.on_focus_changed(&mut ctx, app(), false);
            assert!(daemon.router.composer.is_some());
            daemon.on_focus_changed(&mut ctx, None, false);
            assert!(daemon.router.composer.is_none());
            assert!(!daemon.router.synthetic_active);
            assert!(daemon.router.focused_app_id.is_none());
        }

        #[test]
        fn detect_capability_present_surrounding_is_surrounding_text() {
            // A present surrounding_text frame at activate resolves to
            // SurroundingText — even for a terminal purpose (13), which is no
            // longer special-cased. Terminals whose frames stay dead (foot on
            // KWin) are caught later by the runtime ST→FK liveness downgrade
            // before any delete_surrounding_text is issued.
            let d = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let mut f = frame("", 0, 0);
            f.purpose = 13;
            assert_eq!(d.detect_capability(&f), BackspaceMethod::SurroundingText);
        }

        #[test]
        fn detect_capability_absent_surrounding_is_forward_key() {
            // No surrounding_text frame at activate → ForwardKey (wlroots
            // terminals such as foot/ghostty land here directly).
            let d = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let mut f = frame("", 0, 0);
            f.surrounding_text = None;
            assert_eq!(d.detect_capability(&f), BackspaceMethod::ForwardKey);
        }

        #[test]
        fn first_surrounding_frame_upgrades_a_forward_key_activation() {
            // Chromium on wlroots activates without surrounding text, then
            // reports it with the first keystroke.
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            let mut ctx = AdapterCtx { state: &mut state };
            let method = |d: &Daemon| d.router.composer.as_ref().map(Composer::method);

            let mut activate = frame("", 0, 0);
            activate.activate = true;
            activate.surrounding_text = None;
            daemon.on_done_frame(&mut ctx, &activate);
            assert_eq!(method(&daemon), Some(BackspaceMethod::ForwardKey));

            daemon.on_done_frame(&mut ctx, &frame("a", 1, 1));
            assert_eq!(method(&daemon), Some(BackspaceMethod::SurroundingText));
        }

        #[test]
        fn native_focus_without_text_input_synthesizes_forward_key_session() {
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_focus_changed(&mut ctx, Some("com.mitchellh.ghostty".to_owned()), false);

            assert!(daemon.router.composer.is_some());
            assert!(daemon.router.synthetic_active);
            assert_eq!(
                daemon.router.focused_app_id.as_deref(),
                Some("com.mitchellh.ghostty")
            );
            // Synthetic focus sessions use ForwardKey with a dead commit_string,
            // so replacements route through the virtual keyboard's synthetic
            // keymap channel.
            let composer = daemon.router.composer.as_ref().expect("composer");
            assert_eq!(composer.method(), BackspaceMethod::ForwardKey);
            assert!(!composer.commit_string_functional);
        }

        #[test]
        fn focus_on_imv1_does_not_synthesize_session() {
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            // IMv1 has no keyboard grab outside a text-input context, so a
            // focused client without text-input cannot be composed for.
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV1);
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_focus_changed(&mut ctx, Some("com.mitchellh.ghostty".to_owned()), false);

            assert!(daemon.router.composer.is_none());
            assert!(!daemon.router.synthetic_active);
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
            let mut daemon = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            daemon.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
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
            let mut d = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
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
            assert!(d
                .router
                .composer
                .as_ref()
                .unwrap()
                .last_input_char
                .is_some());
            // Toggle off: the on→off edge closes the gate and clears the word
            // so a later enable starts clean (mirrors what the evdev/ibus
            // transports rely on to stop composing while daklak is "off").
            d.enabled.store(false, Ordering::Release);
            assert!(!d.sync_enabled_edge());
            assert!(d
                .router
                .composer
                .as_ref()
                .unwrap()
                .last_input_char
                .is_none());
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
        use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, AdapterState, ImProtocol};

        const KEY_A: u32 = 30;
        const KEY_LEFT: u32 = 105;

        fn v1_daemon() -> Daemon {
            let mut d = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));
            d
        }

        /// Drive the trait `on_key_pressed` with `forwarding_repeat = repeat`.
        fn key(d: &mut Daemon, key: u32, ch: Option<char>, repeat: bool) -> KeyDecision {
            let (mut state, _peer) = AdapterState::for_test(ImProtocol::ImV2);
            state.forwarding_repeat = repeat;
            let mut ctx = AdapterCtx { state: &mut state };
            d.on_key_pressed(&mut ctx, 0, key, ch)
        }

        #[test]
        fn compose_key_repeat_composes() {
            let mut d = v1_daemon();
            key(&mut d, KEY_A, Some('a'), false);
            // Holding the letter composes like a real press: the second 'a'
            // transforms "aa" → "â", so the repeat returns Apply (delete 1 +
            // commit "â"), matching evdev behaviour.
            let decision = key(&mut d, KEY_A, Some('a'), true);
            assert!(
                matches!(decision, KeyDecision::Apply { .. }),
                "compose-key repeat feeds the engine"
            );
        }

        #[test]
        fn raw_forward_key_repeat_is_forwarded_raw() {
            const KEY_5: u32 = 6;
            let mut d = v1_daemon();
            // A digit isn't a Telex compose key: on repeat it forwards raw
            // (the adapter emits it as state=2 for rate-0 clients), never
            // swallowed.
            let decision = key(&mut d, KEY_5, Some('5'), true);
            assert!(
                matches!(decision, KeyDecision::ForwardRaw),
                "raw-forward repeat forwards raw"
            );
        }

        #[test]
        fn backspace_repeat_is_processed_not_swallowed() {
            const KEY_BS: u32 = 14;
            let mut d = v1_daemon();
            // Held Backspace deletes on repeat like a real press. With an empty
            // composer the engine doesn't consume it, so it forwards raw (the
            // app performs the delete) — the point is it is NOT swallowed.
            let decision = key(&mut d, KEY_BS, None, true);
            assert!(
                matches!(decision, KeyDecision::ForwardRaw),
                "held backspace is processed"
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
            assert!(NAV_KEYS.contains(&KEY_LEFT), "sanity: Left is a nav key");
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
    /// The transport retypes the already-decoded char as a keysym; with no
    /// keysym route (as here: the test context has no live im-v1 context),
    /// it commits the char as text.
    mod shifted_passthrough {
        use super::super::Daemon;
        use crate::composer::Composer;
        use crate::config::Config;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision};
        use viet_ime_engine::InputMethod;
        use viet_ime_keymap::xkb::XkbState;
        use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, AdapterState, ImProtocol};

        const KEY_L: u32 = 38;

        fn daemon() -> Daemon {
            let mut d = Daemon::new(
                Config::default(),
                Arc::new(AtomicBool::new(true)),
                super::super::noop_config_rx(),
            );
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
            let (mut state, _peer) = AdapterState::for_test(protocol);
            state.xkb = Some(XkbState::us_for_test());
            let mut ctx = AdapterCtx { state: &mut state };
            d.on_key_pressed(&mut ctx, 0, KEY_L, Some(ch))
        }

        #[test]
        fn v1_level_shifted_passthrough_without_key_channel_commits_text() {
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
        fn v1_level_shifted_text_commit_is_not_recorded_as_raw_forward() {
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
            // The tray always sends a complete `ConfigChange{method,modern_style,enable_evdev_grab}`.
            // `sync_config()` must apply the runtime fields (method, modern_style) from a single update.
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);
            assert_eq!(d.config.method, MethodConfig::Telex);
            assert!(d.config.modern_style);

            // Send VNI + legacy-style in one atomic send.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                auto_capitalize: true,
                double_space_period: true,
                enable_evdev_grab: false,
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
            assert!(d.config.auto_capitalize);
            assert!(d.config.double_space_period);
        }

        #[test]
        fn noop_update_does_not_reset_engine_inflight() {
            // Identical config after `sync_config()` must NOT trigger
            // `set_input_method` (which resets the engine word).
            let (tx, rx) = watch::channel(ConfigChange::default());
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)), rx);
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            // Build an in-flight composition state.
            let _ = d.handle_char('h');
            assert!(
                d.router
                    .composer
                    .as_ref()
                    .unwrap()
                    .last_input_char
                    .is_some(),
                "engine must have state after handle_char"
            );

            // Send the SAME config we already have.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Telex,
                modern_style: true,
                ..ConfigChange::default()
            });
            d.sync_config();

            // The in-flight state must survive (no silent reset).
            assert!(
                d.router
                    .composer
                    .as_ref()
                    .unwrap()
                    .last_input_char
                    .is_some(),
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
                ..ConfigChange::default()
            });
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Telex,
                modern_style: false,
                ..ConfigChange::default()
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
                ..ConfigChange::default()
            });
            let cfg = Config {
                modern_style: false,
                ..Config::default()
            };
            let mut d = Daemon::new(cfg, Arc::new(AtomicBool::new(true)), rx);
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            composer.set_modern_style(false);
            d.router.composer = Some(composer);

            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                ..ConfigChange::default()
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
            d.router.composer = Some(Composer::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            // Build in-flight state.
            let _ = d.handle_char('h');
            assert!(d
                .router
                .composer
                .as_ref()
                .unwrap()
                .last_input_char
                .is_some());

            // Method changes: Telex → Vni.
            let _ = tx.send(ConfigChange {
                method: MethodConfig::Vni,
                modern_style: true,
                ..ConfigChange::default()
            });
            d.sync_config();

            // Engine must have been reset by set_input_method.
            assert!(
                d.router
                    .composer
                    .as_ref()
                    .unwrap()
                    .last_input_char
                    .is_none(),
                "engine must reset on method change"
            );
            // Config field persisted.
            assert_eq!(d.config.method, MethodConfig::Vni);
        }
    }
}
