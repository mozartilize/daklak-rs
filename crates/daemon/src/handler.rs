//! Daemon-side composition policy. Implements `AdapterHandler` from
//! `wayland-adapter`. Owns the engine, strategy, killer-feature seeding,
//! modifier-shortcut detection, idle-reset, and per-window routing decisions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, ModifierState, SurroundingFrame,
};
use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, FrameSnapshot, ImBackend, KeyDecision};

use crate::config::Config;
use crate::window::WindowState;

// Linux evdev code for Backspace.
const KEY_BACKSPACE: u32 = 14;
// Linux evdev code for Escape — used as evdev emergency escape chord.
// Navigation keys that move the cursor — trigger shadow reset.
const NAV_KEYS: &[u32] = &[
    105, 106, 103, 108, // Left, Right, Up, Down
    102, 107, // Home, End
    104, 109, // PageUp, PageDown
];

/// Extract just the word immediately before the cursor (scan back to last
/// whitespace). For retroactive editing, the engine only needs the current
/// word's context — not the entire document.
fn current_word_before_cursor(text: &str, cursor: u32) -> &str {
    let cursor = (cursor as usize).min(text.len());
    let cursor = (0..=cursor)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace() || *c == '\0')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &before[start..]
}

/// Composition state. Owns config + per-window state + policy flags.
pub struct Daemon {
    pub config: Config,

    pub modifiers: ModifierState,
    pub current_active: bool,

    /// True when `current_active` was synthesized by daklak (Path C —
    /// FocusBackend reported a focused toplevel matching `force_vk_only_apps`
    /// / `auto_vk_only_for_xwayland`) rather than driven by a compositor
    /// `zwp_input_method_v2::Activate` event. Real activate always wins.
    pub synthetic_active: bool,

    pub window: Option<WindowState>,

    /// Timestamp of the last user-keystroke daemon action — used to
    /// distinguish "compositor echo of our action" (recent) from "user
    /// clicked mid-word" (not recent) in surrounding_text frames.
    pub last_action_at: Instant,

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
    last_enabled: bool,
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
            window: None,
            last_action_at: Instant::now() - Duration::from_secs(60),
            terminal_override,
            focused_app_id: None,
            enabled,
            last_enabled: true,
        }
    }

    fn detect_capability(&self, frame: &FrameSnapshot) -> BackspaceMethod {
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
}

impl AdapterHandler for Daemon {
    fn on_done_frame(&mut self, ctx: &mut AdapterCtx<'_>, frame: &FrameSnapshot) {
        if frame.activate && self.synthetic_active {
            tracing::info!(
                "real Activate received while synthetic session active → drop synthetic"
            );
            self.current_active = false;
            self.synthetic_active = false;
            self.window = None;
            self.focused_app_id = None;
        }

        let activate = frame.activate && !self.current_active;
        let deactivate = frame.deactivate && self.current_active;

        if activate {
            let app_id = frame.app_id.clone();
            tracing::info!(app_id = ?app_id, "activate");
            self.current_active = true;
            self.focused_app_id = app_id;

            let mut method = self.detect_capability(frame);
            // VkOnly (Tier 4) requires a separate vk keyboard, which v1
            // (KWin/Mutter) does not expose. Fall through to Tier 3 uinput.
            if method == BackspaceMethod::VkOnly && ctx.im_backend() == ImBackend::V1Kde {
                tracing::info!("VkOnly gated off on KWin (no vk) → falling back to UInput");
                method = BackspaceMethod::UInput;
            }
            tracing::info!("capability detected: {:?}", method);
            let mut ws = WindowState::new(
                self.config.method.to_engine(),
                method,
                self.config.bracket_shortcuts,
            );
            ws.chars_for_delete = self
                .focused_app_id
                .as_deref()
                .map(|id| {
                    let lower = id.trim().to_ascii_lowercase();
                    self.config
                        .force_chars_delete_apps
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(&lower))
                })
                .unwrap_or(false);
            if ws.chars_for_delete {
                tracing::info!(
                    app_id = ?self.focused_app_id,
                    "force_chars_delete_apps match → v1 delete_surrounding_text will use char count"
                );
            }
            self.window = Some(ws);
            if let Some(w) = self.window.as_mut() {
                w.strategy.set_modifiers(self.modifiers);
            }
        } else if deactivate {
            tracing::debug!("deactivate");
            self.current_active = false;
            self.window = None;
            self.focused_app_id = None;
        }

        // After deactivate, window is None — nothing more to do.
        let Some(w) = self.window.as_mut() else {
            return;
        };

        // Re-sync shadow on every surrounding_text frame. Re-seed engine
        // ONLY on activate or genuine cursor jump (user clicked elsewhere).
        // A 1-char insertion at the cursor is an ordinary keystroke — the
        // engine's running state already tracks it, re-seeding would clobber
        // that state with the post-insertion word and double-count the new
        // char when process_key() fires.
        if let Some((text, cursor, anchor)) = frame.surrounding_text.as_ref() {
            tracing::trace!(
                text = %text,
                text_len = text.len(),
                cursor = *cursor,
                prev_text = %w.prev_text,
                prev_cursor = w.prev_cursor,
                raw_word = %w.raw_word,
                "on_done_frame surrounding_text"
            );

            // Duplicate-frame guard. KWin re-emits the same SurroundingText
            // 2-3 times per keystroke. We've already processed this state —
            // running through the diff/reseed logic again wipes raw_word
            // and burns engine context. Skip if nothing changed.
            if !activate && !deactivate
                && text == &w.prev_text
                && *cursor == w.prev_cursor
                && *anchor == w.prev_anchor
            {
                return;
            }

            // Detect a 1-char insertion at the prior cursor (= keystroke).
            // Holds for end-of-text typing AND mid-text typing.
            let one_char_typed = !activate
                && !deactivate
                && detect_one_char_insertion(
                    &w.prev_text,
                    w.prev_cursor,
                    text,
                    *cursor,
                );

            let recent_action = self.last_action_at.elapsed() < Duration::from_millis(150);
            let should_reseed = activate || (!one_char_typed && !recent_action);
            // Re-init raw_word from the current word at cursor.
            // Only valid when the word is ASCII-only (we can't reconstruct
            // the raw Telex for an already-composed Vietnamese word).
            // Otherwise clear — will rebuild as user types.
            let reseed_word: Option<String> = if should_reseed {
                let word = current_word_before_cursor(text, *cursor);
                if word_qualifies_for_reseed(word) {
                    Some(word.to_owned())
                } else {
                    None
                }
            } else {
                None
            };
            // Late tier upgrade. Chromium under wlroots/sway doesn't
            // include set_surrounding_text in its enable cycle, so the
            // activate frame has has_surrounding=false and the tier
            // detector picks ForwardKey. The first real keystroke then
            // arrives with surrounding info — re-detect and upgrade to
            // Tier 1 SurroundingText so autocomplete-selection handling
            // (delete_surrounding_text + ForwardKey BS fallback) works.
            // Upgrade-only: never downgrade ST → FK here.
            if !activate
                && w.strategy.method() == BackspaceMethod::ForwardKey
            {
                let probe = CapabilityProbe {
                    purpose: frame.purpose,
                    surrounding_text_seen: Some(SurroundingFrame {
                        text: text.clone(),
                        cursor: *cursor,
                    }),
                    app_id: self.focused_app_id.clone(),
                    force_uinput_apps: self.config.force_uinput_apps.clone(),
                    force_vk_only_apps: self.config.force_vk_only_apps.clone(),
                    terminal_override: self.terminal_override,
                };
                let upgraded = detect_method(&probe);
                if upgraded == BackspaceMethod::SurroundingText {
                    tracing::info!(
                        from = ?BackspaceMethod::ForwardKey,
                        to = ?upgraded,
                        "late tier upgrade on first surrounding_text"
                    );
                    w.strategy.set_method(upgraded);
                }
            }
            w.strategy.on_surrounding_text(text, *cursor, *anchor);
            if should_reseed {
                w.engine.reset();
                if let Some(word) = &reseed_word {
                    tracing::debug!(word, "re-seed engine (activate or cursor jump)");
                    w.engine.feed_context(word);
                }
                if let Some(word) = reseed_word {
                    w.raw_word_screen_widths = vec![1u8; word.len()];
                    w.raw_word = word;
                } else {
                    w.raw_word.clear();
                    w.raw_word_screen_widths.clear();
                }
            }

            w.prev_text = text.clone();
            w.prev_cursor = *cursor;
            w.prev_anchor = *anchor;
        } else if !activate && !deactivate {
            w.prev_text.clear();
            w.prev_cursor = 0;
            w.prev_anchor = 0;
        }
    }

    fn on_key_pressed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        key: u32,
        ch: Option<char>,
    ) -> KeyDecision {
        if let Some(w) = self.window.as_mut() {
            w.check_idle_reset();
        }

        let now_enabled = self.enabled.load(Ordering::Acquire);
        if self.last_enabled && !now_enabled {
            if let Some(w) = self.window.as_mut() { w.full_reset(); }
        }
        self.last_enabled = now_enabled;
        if !now_enabled {
            return KeyDecision::ForwardRaw;
        }

        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.modifiers.intersects(shortcut_mods) {
            tracing::debug!(key, mods = ?self.modifiers,
                "modifier shortcut → bypass engine + forward");
            if let Some(w) = self.window.as_mut() {
                w.full_reset();
            }
            // Same rationale as NAV: shortcuts (Ctrl+V paste, Ctrl+A
            // select, etc.) end the current composition. Roll back so
            // the next surrounding frame can re-seed from whatever
            // word the cursor lands on.
            self.last_action_at = Instant::now() - Duration::from_secs(60);
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        if NAV_KEYS.contains(&key) {
            tracing::debug!(key, "key: nav → reset shadow + forward");
            if let Some(w) = self.window.as_mut() {
                w.full_reset();
            }
            // Roll last_action_at back so the next surrounding frame
            // bypasses the recent_action gate and re-seeds. NAV ends
            // the current composition — the cursor is now elsewhere
            // and the next char should compose against whatever word
            // the cursor lands on (killer feature, multi-hop case:
            // "bò bo|" → arrow keys → "bò bof|" should compose).
            self.last_action_at = Instant::now() - Duration::from_secs(60);
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        if self.window.is_none() {
            tracing::trace!(key, "key: no active window → forward");
            return KeyDecision::ForwardRaw;
        }

        if key == KEY_BACKSPACE {
            // Don't update last_action_at: BS deletes, doesn't compose.
            // The killer feature (retroactive tone after space+BS+'f' →
            // bò) needs surrounding-text re-seed to fire on the BS's
            // post-surrounding frame. If BS marked an action, the
            // 150ms recent_action gate would block re-seed, leaving
            // engine empty so the next 'f' is forwarded as ASCII.
            return self.handle_backspace();
        }

        self.last_action_at = Instant::now();

        let Some(ch) = ch else {
            tracing::trace!(key, "key: no xkb char → forward raw");
            return KeyDecision::ForwardRaw;
        };

        self.handle_char(key, ch)
    }

    fn apply_pending(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        method: BackspaceMethod,
        backspaces: usize,
        commit: &str,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
    ) {
        let _ = method;
        let serial = ctx.serial();
        if let Some(w) = self.window.as_mut() {
            tracing::debug!(method = ?w.method, backspaces, commit, "strategy.apply");
            let chars_for_delete = w.chars_for_delete;
            ctx.with_sink(raw_mods, held_user_kc, chars_for_delete, |sink| {
                w.strategy.apply(backspaces, commit, serial, time, sink);
            });
        }
    }

    fn on_modifiers(&mut self, _ctx: &mut AdapterCtx<'_>, m: ModifierState) {
        self.modifiers = m;
        if let Some(w) = self.window.as_mut() {
            w.strategy.set_modifiers(m);
        }
    }

    fn on_focus_changed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        app_id: Option<String>,
        is_xwayland: bool,
    ) {
        let lower = app_id.as_deref().map(str::to_ascii_lowercase);
        let in_force_uinput = lower
            .as_deref()
            .map(|id| {
                self.config
                    .force_uinput_apps
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(id))
            })
            .unwrap_or(false);
        let in_force_vk_only = lower
            .as_deref()
            .map(|id| {
                self.config
                    .force_vk_only_apps
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(id))
            })
            .unwrap_or(false);

        let auto_xwayland_vk_only =
            self.config.auto_vk_only_for_xwayland && is_xwayland && app_id.is_some();
        let vk_only_matched =
            !in_force_uinput && (in_force_vk_only || auto_xwayland_vk_only);
        let vk_available = ctx.im_backend() != ImBackend::V1Kde;
        let matched = vk_only_matched && vk_available;

        if matched && !self.current_active {
            let id = app_id.clone().unwrap_or_default();
            let reason = if in_force_vk_only {
                "force_vk_only_apps"
            } else {
                "auto_vk_only_for_xwayland"
            };

            tracing::info!(app_id = %id, reason, is_xwayland,
                "synthetic activate → VkOnly");
            self.current_active = true;
            self.synthetic_active = true;
            self.focused_app_id = app_id;
            let ws = WindowState::new(
                self.config.method.to_engine(),
                BackspaceMethod::VkOnly,
                self.config.bracket_shortcuts,
            );
            self.window = Some(ws);
            if let Some(w) = self.window.as_mut() {
                w.strategy.set_modifiers(self.modifiers);
            }
        } else if self.synthetic_active && !matched {
            tracing::info!(
                old = ?self.focused_app_id,
                new = ?app_id,
                is_xwayland,
                in_force_uinput,
                "synthetic deactivate (no longer matches VkOnly conditions)"
            );
            self.current_active = false;
            self.synthetic_active = false;
            self.window = None;
            self.focused_app_id = None;
        } else if !self.synthetic_active && matched {
            tracing::trace!("focus_changed skipped: already active via IM");
        }
    }

    fn window_chars_for_delete(&self) -> Option<bool> {
        self.window.as_ref().map(|w| w.chars_for_delete)
    }
}

impl viet_ime_evdev_adapter::EvdevHandler for Daemon {
    fn handle_char(&mut self, code: u32, ch: char) -> KeyDecision {
        Daemon::handle_char(self, code, ch)
    }

    fn handle_backspace(&mut self) -> KeyDecision {
        Daemon::handle_backspace(self)
    }

    fn clear_session(&mut self) {
        self.current_active = false;
        self.synthetic_active = false;
        self.window = None;
        self.focused_app_id = None;
    }

    fn clear_last_input_char(&mut self) {
        if let Some(w) = self.window.as_mut() {
            w.last_input_char = None;
        }
    }

    fn full_reset_window(&mut self) {
        if let Some(w) = self.window.as_mut() {
            w.full_reset();
        }
    }

    fn check_idle_reset_window(&mut self) {
        if let Some(w) = self.window.as_mut() {
            w.check_idle_reset();
        }
    }
}

impl Daemon {
    /// Bootstrap a synthetic session for evdev-only mode. Sets up a
    /// window with VkOnly routing so `handle_char` and `handle_backspace`
    /// work without a Wayland compositor.
    pub fn activate_evdev(&mut self) {
        let ws = WindowState::new(
            self.config.method.to_engine(),
            BackspaceMethod::VkOnly,
            self.config.bracket_shortcuts,
        );
        self.current_active = true;
        self.synthetic_active = true;
        self.window = Some(ws);
        if let Some(w) = self.window.as_mut() {
            w.strategy.set_modifiers(self.modifiers);
        }
    }

    pub fn handle_backspace(&mut self) -> KeyDecision {
        let Some(w) = self.window.as_mut() else {
            return KeyDecision::ForwardRaw;
        };
        let r = w.engine.process_backspace();
        tracing::debug!(consumed = r.consumed, bs = r.backspaces, "engine.process_backspace");

        // v1/KWin path: raw_word tracks raw keystrokes so backspace must
        // pop the raw entries that produced the deleted screen char.
        // raw_word_screen_widths[last] tells us how many raw chars to remove
        // (e.g. Telex 'u'+'s' produced 'ú' → width=2 → BS over 'ú' pops
        // both 's' and 'u', leaving raw_word consistent with the screen).
        let popped_width = w.raw_word_screen_widths.pop();
        {
            let width = popped_width.unwrap_or(1) as usize;
            for _ in 0..width {
                w.raw_word.pop();
            }
        }

        // If the engine restored chars (e.g. tone-undo: 'ú' → 'u', or
        // vowel-undo: 'ê' → 'ee'), push them back into raw_word so the
        // next keystroke seeds the engine with correct context.  Only do
        // this when we were actively tracking (popped_width.is_some()),
        // i.e. the v1 path — in v2 raw_word_screen_widths is always empty.
        if popped_width.is_some() && !r.commit.is_empty() {
            for ch in r.commit.chars() {
                if ch.is_ascii_alphabetic() {
                    w.raw_word.push(ch);
                    w.raw_word_screen_widths.push(1);
                }
            }
        }

        if r.consumed {
            w.last_keystroke_at = Instant::now();
            let method = w.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            tracing::trace!("BS not consumed → forward");
            w.strategy.shadow.text_mut().pop();
            w.last_keystroke_at = Instant::now();
            KeyDecision::ForwardRaw
        }
    }

    pub fn handle_char(&mut self, _key: u32, ch: char) -> KeyDecision {
        self.handle_char_inner(_key, ch, true)
    }

    /// `shadow_already_has_ch`: pass `true` when the caller is the v1/KWin
    /// surrounding-text path — kate already inserted the char, so shadow
    /// reflects post-insertion text. The word-boundary seed must skip the
    /// trailing char (else process_key double-feeds). Pass `false` for the
    /// v2/wlroots key-grab path: the IM grab intercepts the key before the
    /// client sees it, shadow does NOT contain the char yet.
    pub fn handle_char_inner(
        &mut self,
        _key: u32,
        ch: char,
        shadow_already_has_ch: bool,
    ) -> KeyDecision {
        let Some(w) = self.window.as_mut() else {
            return KeyDecision::ForwardRaw;
        };
        let prev_was_separator = matches!(
            w.last_input_char,
            Some(c) if !c.is_ascii_alphabetic()
        );
        w.last_input_char = Some(ch);

        // v1/KWin path: maintain raw_word and use it as the engine seed on
        // EVERY keystroke. Engine forgets vowel-cluster context after
        // returning a transform (e.g. after `ee → ê` engine no longer
        // recognizes `iê` as a vowel cluster when 's' tone arrives later).
        // Feeding the original raw ASCII chars sidesteps that.
        if shadow_already_has_ch {
            // Word boundary: reset raw_word.
            if !ch.is_ascii_alphabetic() {
                w.raw_word.clear();
                w.raw_word_screen_widths.clear();
            }
            let prefix = w.raw_word.clone();
            w.engine.reset();
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_alphabetic()) {
                tracing::debug!(prefix, "seed engine from raw_word (v1 path)");
                w.engine.feed_context(&prefix);
            }
            // Append `ch` AFTER seeding (engine's process_key adds it).
            if ch.is_ascii_alphabetic() {
                w.raw_word.push(ch);
            }
        } else {
            // v2/wlroots path: original shadow-based seed.
            if w.engine.at_word_beginning() && !prev_was_separator {
                let shadow_text = w.strategy.shadow.text().to_owned();
                let raw_word = current_word_before_cursor(&shadow_text, shadow_text.len() as u32);
                if !raw_word.is_empty() && raw_word.chars().all(|c| c.is_ascii_lowercase()) {
                    tracing::debug!(word = raw_word, "seed engine from shadow at word boundary");
                    w.engine.feed_context(raw_word);
                }
            }
        }

        let r = w.engine.process_key(ch);

        w.last_keystroke_at = Instant::now();

        tracing::debug!(
            ch = %ch,
            consumed = r.consumed,
            bs = r.backspaces,
            commit = %r.commit,
            shadow = %w.strategy.shadow.text(),
            "engine.process_key"
        );

        // v1/KWin path: maintain raw_word_screen_widths in sync with raw_word.
        // raw_word_screen_widths[i] = how many raw chars produced screen char i.
        // Invariant: sum(raw_word_screen_widths) == raw_word.len().
        if shadow_already_has_ch && ch.is_ascii_alphabetic() {
            if r.consumed {
                // Engine deleted r.backspaces screen chars and emitted r.commit.
                // Pop r.backspaces widths (sum = s); the new raw chars for all
                // commit screen chars together cost s + 1 (the current ch).
                let s: usize = (0..r.backspaces)
                    .map(|_| w.raw_word_screen_widths.pop().unwrap_or(1) as usize)
                    .sum();
                let total = s + 1; // raw chars to distribute across commit chars
                let m = r.commit.chars().count().max(1);
                // Push 1 for the first m-1 commit chars; all remaining raw
                // chars go to the last one (ensures sum == total == raw_word
                // growth since last clear).
                for _ in 0..m.saturating_sub(1) {
                    w.raw_word_screen_widths.push(1);
                }
                let last_width = total.saturating_sub(m.saturating_sub(1)).max(1) as u8;
                w.raw_word_screen_widths.push(last_width);
            } else {
                // ForwardRaw: one raw char → one screen char.
                w.raw_word_screen_widths.push(1);
            }
        }

        if r.consumed {
            let method = w.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            w.strategy.shadow.text_mut().push(ch);
            KeyDecision::ForwardRaw
        }
    }
}

/// Whether `word` is suitable to re-seed the engine + `raw_word` from on
/// a cursor jump.
///
/// Must accept ASCII letters of either case — Telex composes `DD→Đ`,
/// `AA→Â`, `OO→Ô`, so capitals are valid raw input. A previous version
/// gated on `is_ascii_lowercase` which silently dropped capitals,
/// breaking the `DD→Đ` transform when the cursor entered a new word
/// starting with a capital letter (e.g. `Đường\nD` then `D` again).
///
/// Must reject:
/// - empty strings (no context to seed)
/// - words containing non-ASCII (already-composed Vietnamese — we can't
///   reconstruct the raw Telex from the composed form)
/// - words containing digits, punctuation, etc. (not Telex input)
pub fn word_qualifies_for_reseed(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic())
}

/// Detects whether the transition from (`prev_text`, `prev_cursor`) to
/// (`text`, `cursor`) is exactly one character inserted at `prev_cursor`.
/// Handles end-of-text typing AND mid-text typing.
pub fn detect_one_char_insertion(
    prev_text: &str,
    prev_cursor: u32,
    text: &str,
    cursor: u32,
) -> bool {
    let prev_cur = prev_cursor as usize;
    prev_cur <= prev_text.len()
        && cursor > prev_cursor
        && cursor == prev_cursor + 1
        && text.len() > prev_text.len()
        && text.len() == prev_text.len() + 1
        && text.get(..prev_cur) == prev_text.get(..prev_cur)
        && text.get((cursor as usize)..) == prev_text.get(prev_cur..)
}

#[cfg(test)]
mod tests {
    use super::current_word_before_cursor;

    #[test]
    fn extracts_word_at_end_of_line() {
        assert_eq!(current_word_before_cursor("phow", 4), "phow");
    }

    #[test]
    fn extracts_word_in_middle_of_line() {
        assert_eq!(current_word_before_cursor("hello phow", 10), "phow");
    }

    #[test]
    fn extracts_partial_word_at_cursor() {
        assert_eq!(current_word_before_cursor("phow", 3), "pho");
    }

    #[test]
    fn empty_text_returns_empty() {
        assert_eq!(current_word_before_cursor("", 0), "");
    }

    #[test]
    fn cursor_at_start_returns_empty() {
        assert_eq!(current_word_before_cursor("hello", 0), "");
    }

    #[test]
    fn handles_multibyte_chars_at_char_boundary() {
        assert_eq!(current_word_before_cursor("trâ", 4), "trâ");
    }

    #[test]
    fn handles_cursor_inside_multibyte_char() {
        let r = current_word_before_cursor("trâ", 3);
        assert_eq!(r, "tr");
    }

    #[test]
    fn cursor_beyond_text_clamps() {
        assert_eq!(current_word_before_cursor("hi", 99), "hi");
    }

    #[test]
    fn space_separates_words() {
        assert_eq!(current_word_before_cursor("foo bar baz", 11), "baz");
    }

    #[test]
    fn tab_separates_words() {
        assert_eq!(current_word_before_cursor("foo\tbar", 7), "bar");
    }

    #[test]
    fn newline_separates_words() {
        assert_eq!(current_word_before_cursor("line1\nline2", 11), "line2");
    }

    #[test]
    fn seed_gate_skips_capitalized_word() {
        let word = "Folder";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_accepts_lowercase_vietnamese_precursor() {
        let word = "phow";
        assert!(word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_skips_word_with_digit() {
        let word = "abc1";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }

    // ── detect_one_char_insertion ──────────────────────────────────────

    use super::detect_one_char_insertion as oci;

    #[test]
    fn oci_empty_to_first_char() {
        assert!(oci("", 0, "t", 1));
    }

    #[test]
    fn oci_append_ascii() {
        assert!(oci("ti", 2, "tie", 3));
    }

    #[test]
    fn oci_append_after_vietnamese() {
        // shadow "tiê" (4 bytes, 3 chars). cursor at byte 4. Type 'n' →
        // "tiên" (5 bytes), cursor at byte 5. Must detect as keystroke.
        assert!(oci("tiê", 4, "tiên", 5));
    }

    #[test]
    fn oci_mid_text_insert() {
        // "abcd" cursor=2 ("ab|cd"). Type 'X' → "abXcd" cursor=3.
        assert!(oci("abcd", 2, "abXcd", 3));
    }

    #[test]
    fn oci_multi_char_paste_rejected() {
        assert!(!oci("ab", 2, "abcde", 5));
    }

    #[test]
    fn oci_backspace_rejected() {
        assert!(!oci("abc", 3, "ab", 2));
    }

    #[test]
    fn oci_same_text_rejected() {
        // Duplicate frame: text and cursor unchanged.
        assert!(!oci("abc", 3, "abc", 3));
    }

    #[test]
    fn oci_cursor_jump_rejected() {
        // Same text, cursor moved (user clicked elsewhere).
        assert!(!oci("abcde", 5, "abcde", 2));
    }

    #[test]
    fn oci_replace_with_same_length_rejected() {
        // ê (2 bytes) replaced with two ASCII: same byte count, no growth.
        assert!(!oci("tiê", 4, "tiab", 4));
    }

    #[test]
    fn oci_cursor_mismatch_rejected() {
        // text grew by 1 byte but cursor jumped by 2 (impossible for single
        // keystroke insert at cursor).
        assert!(!oci("ab", 2, "abc", 3) == false);
        // also: cursor at 0 in larger text (not at insertion point).
        assert!(!oci("ab", 2, "abc", 1));
    }

    #[test]
    fn oci_post_apply_echo_rejected() {
        // After daklak's `bs=2 commit="ê"` on "tiee" cursor=4:
        // kate text becomes "tiê" cursor=4. text shrank by 0 in chars but
        // -1 byte (4→4 bytes? actually tiê=4 bytes, tiee=4 bytes, same len).
        // Should NOT be one_char_typed.
        assert!(!oci("tiee", 4, "tiê", 4));
    }

    #[test]
    fn oci_duong_line_break_to_capital_d() {
        // Regression: gedit on Enter resets surrounding text. User had
        // "đường" then pressed Enter and typed 'D'. text="D" cursor=1 vs
        // prev_text="đường" prev_cursor=9. Cursor went DOWN — must NOT be
        // detected as a 1-char keystroke (would feed 'D' into engine
        // without resetting raw_word).
        assert!(!oci("đường", 9, "D", 1));
    }

    #[test]
    fn oci_second_capital_d_on_new_line() {
        // After cursor jump above, prev_text becomes "D" prev_cursor=1.
        // User types second 'D' → text="DD" cursor=2. MUST be detected as
        // 1-char keystroke so handle_char fires and `DD→Đ` rule runs.
        assert!(oci("D", 1, "DD", 2));
    }

    // ── word_qualifies_for_reseed ──────────────────────────────────────

    use super::word_qualifies_for_reseed as wqr;

    #[test]
    fn wqr_capital_d_accepted() {
        // The Đường-line-2 regression: word="D" must seed raw_word so the
        // next 'D' keystroke can fire the DD→Đ Telex rule.
        assert!(wqr("D"));
    }

    #[test]
    fn wqr_double_capital_d_accepted() {
        assert!(wqr("DD"));
    }

    #[test]
    fn wqr_capital_aa_accepted() {
        // AA→Â Telex rule needs the same capital handling.
        assert!(wqr("AA"));
    }

    #[test]
    fn wqr_mixed_case_accepted() {
        // "Folder" is alphabetic; engine just won't transform — harmless.
        assert!(wqr("Folder"));
    }

    #[test]
    fn wqr_lowercase_accepted() {
        assert!(wqr("phow"));
    }

    #[test]
    fn wqr_empty_rejected() {
        assert!(!wqr(""));
    }

    #[test]
    fn wqr_vietnamese_rejected() {
        // Already-composed Vietnamese: can't reconstruct raw Telex.
        assert!(!wqr("đường"));
        assert!(!wqr("tiếng"));
        assert!(!wqr("mộng"));
    }

    #[test]
    fn wqr_digit_rejected() {
        assert!(!wqr("abc1"));
    }

    #[test]
    fn wqr_punctuation_rejected() {
        assert!(!wqr("hello,"));
    }

    mod surrounding_anchor_regression {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use super::super::Daemon;
        use crate::config::Config;
        use crate::window::WindowState;
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
            daemon.window = Some(WindowState::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));

            let mut state = AdapterState::new();
            let mut ctx = AdapterCtx { state: &mut state };

            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 3));
            daemon.on_done_frame(&mut ctx, &frame("translate", 3, 9));

            let mut sink = DeleteCaptureSink::default();
            let w = daemon.window.as_mut().expect("window state exists");
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

    // ── Ctrl+BS / NAV reset: raw_word lives on Daemon, must be cleared
    //    alongside WindowState. Bug is v1/KWin only — v2/wlroots path
    //    (shadow_already_has_ch=false) never reads or writes raw_word.
    mod raw_word_reset {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use super::super::Daemon;
        use crate::config::Config;
        use crate::window::WindowState;
        use viet_ime_edit_strategy::BackspaceMethod;
        use viet_ime_engine::InputMethod;
        use viet_ime_evdev_adapter::EvdevHandler;

        fn v1_daemon() -> Daemon {
            let mut d = Daemon::new(Config::default(), Arc::new(AtomicBool::new(true)));
            d.current_active = true;
            d.window = Some(WindowState::new(
                InputMethod::Telex,
                BackspaceMethod::SurroundingText,
                false,
            ));
            d
        }

        fn raw_word(d: &Daemon) -> &str {
            d.window
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
            // raw_word lives on WindowState — when window = None, it dies
            // with the window. No explicit clear needed in deactivate.
            let mut d = v1_daemon();
            for ch in "xaxax".chars() {
                d.handle_char_inner(0, ch, true);
            }
            d.window = None;
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
