//! Daemon-side composition policy. Implements `AdapterHandler` from
//! `wayland-adapter`. Owns the engine, strategy, killer-feature seeding,
//! modifier-shortcut detection, idle-reset, and per-window routing decisions.

use std::time::{Duration, Instant};

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, ModifierState, SurroundingFrame,
};
use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, FrameSnapshot, KeyDecision};

use crate::config::Config;
use crate::window::WindowState;

// Linux evdev code for Backspace.
const KEY_BACKSPACE: u32 = 14;
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
        .rfind(|c: char| c.is_whitespace() || c == '\0')
        .map(|i| i + 1)
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

    /// Last printable user keystroke fed to `handle_char`. Tracks user
    /// intent independently of the shadow buffer — the shadow can drop
    /// just-forwarded chars when a stale surrounding_text echo arrives
    /// before the compositor commits the new state. Used to gate the
    /// word-boundary seed: skip seeding when the previous keystroke was
    /// a separator (whitespace/punct/etc.), because a new word is
    /// starting and seeding from the prior word would poison the
    /// engine state.
    pub last_input_char: Option<char>,
}

impl Daemon {
    pub fn new(config: Config) -> Self {
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
            last_input_char: None,
        }
    }

    fn detect_capability(&self, frame: &FrameSnapshot) -> BackspaceMethod {
        let probe = CapabilityProbe {
            purpose: frame.purpose,
            surrounding_text_seen: frame.surrounding_text.as_ref().map(|(text, cursor)| {
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
    fn on_done_frame(&mut self, _ctx: &mut AdapterCtx<'_>, frame: &FrameSnapshot) {
        // Real compositor activate always wins over a synthetic
        // (FocusBackend-driven) session. Tear synthetic down first.
        if frame.activate && self.synthetic_active {
            tracing::info!(
                "real Activate received while synthetic session active → drop synthetic"
            );
            self.current_active = false;
            self.synthetic_active = false;
            self.window = None;
            self.focused_app_id = None;
            self.last_input_char = None;
        }

        let activate = frame.activate && !self.current_active;
        let deactivate = frame.deactivate && self.current_active;

        if activate {
            let app_id = frame.app_id.clone();
            tracing::info!(app_id = ?app_id, "activate");
            self.current_active = true;
            self.focused_app_id = app_id;
            self.last_input_char = None;

            let method = self.detect_capability(frame);
            // UInput requires /dev/uinput; the adapter demotes silently via
            // its UinputDevice::open failure path (no fd → vk_only-like
            // emit, but adapter-side that's not modeled). Keep the daemon
            // honest: log here and fall back to ForwardKey when adapter
            // didn't open uinput. The adapter still tracks `uinput == None`
            // — we'd need a ctx accessor to query it for perfect parity.
            // For now we rely on adapter's existing semantics (uinput_key
            // is a no-op when uinput is None) — that effectively makes the
            // method behave like ForwardKey for Tier 3 BS.
            let effective_method = method;

            tracing::info!("capability detected: {:?}", effective_method);
            let ws = WindowState::new(self.config.method.to_engine(), effective_method);
            self.window = Some(ws);
            // Sync current modifier mask into the new window's strategy
            // (Tier 3 modifier guard reads this).
            if let Some(w) = self.window.as_mut() {
                w.strategy.set_modifiers(self.modifiers);
            }
        } else if deactivate {
            tracing::debug!("deactivate");
            self.current_active = false;
            self.window = None;
            self.focused_app_id = None;
            self.last_input_char = None;
        }

        // Re-sync shadow on every surrounding_text frame. Re-seed engine
        // ONLY on activate or when there's no recent daemon action (= user
        // moved the cursor by clicking).
        if let Some((text, cursor)) = frame.surrounding_text.as_ref() {
            if let Some(w) = self.window.as_mut() {
                w.strategy.on_surrounding_text(text, *cursor);

                let recent_action = self.last_action_at.elapsed() < Duration::from_millis(150);
                let should_reseed = activate || !recent_action;
                if should_reseed {
                    let word = current_word_before_cursor(text, *cursor);
                    w.engine.reset();
                    // Lowercase-only seed gate: Vietnamese typing is
                    // lowercase. Capitalized words signal English content
                    // (e.g. thunar's "Folder") where seeding poisons the
                    // engine and prevents subsequent compose triggers.
                    if !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase()) {
                        tracing::debug!(word, "re-seed engine (activate or cursor jump)");
                        w.engine.feed_context(word);
                    }
                }
            }
        }
    }

    fn on_key_pressed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        key: u32,
        ch: Option<char>,
    ) -> KeyDecision {
        // 2-second idle reset
        if let Some(w) = self.window.as_mut() {
            w.check_idle_reset();
        }

        // Modifier shortcuts (Ctrl/Alt/Super + key): bypass engine, forward
        // raw. Shift is NOT included — Shift+letter is just uppercase.
        // Modifier+key may move cursor (Ctrl+arrow, Ctrl+Home, etc.) — leave
        // last_action_at alone so the resulting surrounding_text frame is
        // treated as a user cursor move (re-seed enabled).
        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.modifiers.intersects(shortcut_mods) {
            tracing::debug!(key, mods = ?self.modifiers,
                "modifier shortcut → bypass engine + forward");
            if let Some(w) = self.window.as_mut() {
                w.full_reset();
            }
            self.last_input_char = None;
            // Forward without stamping last_forwarded_key — shortcut keys
            // don't participate in the Path A held-key dance.
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        // Navigation keys: reset shadow, forward key. Do NOT touch
        // last_action_at — the resulting cursor move must trigger re-seed.
        if NAV_KEYS.contains(&key) {
            tracing::debug!(key, "key: nav → reset shadow + forward");
            if let Some(w) = self.window.as_mut() {
                w.full_reset();
            }
            self.last_input_char = None;
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        // Mark this as a daemon action: subsequent surrounding_text frames
        // arriving within 150ms are treated as compositor echoes (expected),
        // not as user mouse clicks. Done AFTER nav/shortcut handling so those
        // paths leave the timestamp alone.
        self.last_action_at = Instant::now();

        // No active window → pass through.
        if self.window.is_none() {
            tracing::trace!(key, "key: no active window → forward");
            return KeyDecision::ForwardRaw;
        }

        if key == KEY_BACKSPACE {
            return self.handle_backspace();
        }

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
            ctx.with_sink(raw_mods, held_user_kc, |sink| {
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
        _ctx: &mut AdapterCtx<'_>,
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

        // `force_uinput_apps` is the deny override for synthetic VkOnly.
        // Chromium-class XWayland apps crash on evdev-200+ keycodes.
        let auto_xwayland_match =
            self.config.auto_vk_only_for_xwayland && is_xwayland && app_id.is_some();
        let matched = !in_force_uinput && (in_force_vk_only || auto_xwayland_match);

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
            let ws = WindowState::new(self.config.method.to_engine(), BackspaceMethod::VkOnly);
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
        } else {
            tracing::trace!(?app_id, matched, synthetic = self.synthetic_active,
                current_active = self.current_active, "focus change ignored");
        }
    }

}

impl Daemon {
    fn handle_backspace(&mut self) -> KeyDecision {
        let Some(w) = self.window.as_mut() else {
            return KeyDecision::ForwardRaw;
        };
        let r = w.engine.process_backspace();
        tracing::debug!(consumed = r.consumed, bs = r.backspaces, "engine.process_backspace");

        if r.consumed {
            w.last_keystroke_at = Instant::now();
            let method = w.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            // Engine didn't consume — forward raw backspace + shadow.pop().
            tracing::trace!("BS not consumed → forward");
            w.strategy.shadow.text_mut().pop();
            w.last_keystroke_at = Instant::now();
            KeyDecision::ForwardRaw
        }
    }

    fn handle_char(&mut self, _key: u32, ch: char) -> KeyDecision {
        // Capture prior keystroke before overwriting — used to gate the
        // word-boundary seed below. Read off self before borrowing window.
        // None means "no prior keystroke in this session" (fresh activate,
        // nav reset, etc.) — treat as seed-eligible so first-char
        // retroactive composition still fires.
        let prev_was_separator = matches!(
            self.last_input_char,
            Some(c) if !c.is_ascii_alphabetic()
        );
        self.last_input_char = Some(ch);

        let Some(w) = self.window.as_mut() else {
            return KeyDecision::ForwardRaw;
        };

        // Killer feature for end-of-word: when engine has no pending
        // composition (fresh after idle_reset, focus change, or anywhere
        // we cleared state), seed from the current word in the shadow so
        // retroactive composition fires. e.g. user types "tran", pauses
        // (idle reset clears engine), types `af` — engine seeded with
        // "tran" turns `a` into `bs=2 commit="ân"` → "trân".
        //
        // Gated by `!prev_was_separator`: if the prior keystroke was a
        // separator (space/punct/etc.), a new word is starting and the
        // shadow's previous word is no longer the composition context.
        // Necessary because surrounding_text echoes from the compositor
        // race ahead of our local shadow push and can clobber the
        // separator before the next key arrives.
        if w.engine.at_word_beginning() && !prev_was_separator {
            let shadow_text = w.strategy.shadow.text().to_owned();
            let word = current_word_before_cursor(&shadow_text, shadow_text.len() as u32);
            if !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase()) {
                tracing::debug!(word, "seed engine from shadow at word boundary");
                w.engine.feed_context(word);
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

        if r.consumed {
            let method = w.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            // CRITICAL: engine remembers the key internally even when not
            // claimed. When a later key DOES trigger consumed (e.g. second
            // 'a' of "aa" → "â"), engine returns bs=N counting back into
            // these forwarded chars. Shadow must track them so Tier 1 gets
            // correct byte counts.
            w.strategy.shadow.text_mut().push(ch);
            KeyDecision::ForwardRaw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::current_word_before_cursor;

    #[test]
    fn extracts_word_at_end_of_line() {
        assert_eq!(current_word_before_cursor("tran", 4), "tran");
    }

    #[test]
    fn extracts_word_in_middle_of_line() {
        assert_eq!(current_word_before_cursor("hello tran", 10), "tran");
    }

    #[test]
    fn extracts_partial_word_at_cursor() {
        assert_eq!(current_word_before_cursor("tran", 3), "tra");
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

    // Seed-gate filter checks: only all-lowercase ASCII words feed the engine.
    #[test]
    fn seed_gate_skips_capitalized_word() {
        let word = "Folder";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_accepts_lowercase_vietnamese_precursor() {
        let word = "tran";
        assert!(word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_skips_word_with_digit() {
        let word = "abc1";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }
}
