//! Wayland transport glue: `AdapterHandler` impl. Translates text_input_v1/v2
//! frames into `Composer` calls. Owns the wayland-only frame bookkeeping (the
//! late-tier-upgrade probe, the duplicate-frame guard's activate/deactivate
//! framing, `AdapterCtx` plumbing). The reseed gate itself lives once on
//! `Composer::observe_surrounding_bytes`.

use std::sync::atomic::Ordering;

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, KeyDecision, ModifierState,
};
use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, FrameSnapshot};

use crate::composer::{ByteCursor, Composer};
use crate::handler::{Daemon, KEY_BACKSPACE, NAV_KEYS};

impl AdapterHandler for Daemon {
    fn on_done_frame(&mut self, _ctx: &mut AdapterCtx<'_>, frame: &FrameSnapshot) {
        self.sync_config();

        if frame.activate && self.router.synthetic_active {
            tracing::info!(
                "real Activate received while synthetic session active → drop synthetic"
            );
            self.router.current_active = false;
            self.router.synthetic_active = false;
            self.router.composer = None;
            self.router.focused_app_id = None;
        }

        let activate = frame.activate && !self.router.current_active;
        let deactivate = frame.deactivate && self.router.current_active;

        if activate {
            let app_id = frame.app_id.clone();
            tracing::info!(app_id = ?app_id, "activate");
            self.router.current_active = true;
            self.router.focused_app_id = app_id;

            let method = self.detect_capability(frame);
            tracing::info!("capability detected: {:?}", method);
            let mut c = Composer::new(
                self.config.method.to_engine(),
                method,
                self.config.bracket_shortcuts,
            );
            c.set_modern_style(self.config.modern_style);
            // `commit_string_functional` starts true (the spec default: a v3
            // client is obligated to apply commit_string). It is driven down
            // ONLY by the runtime ST-liveness probe at the ST→FK downgrade —
            // not by a purpose/capability guess, because text-input-v3 exposes
            // no per-feature bit and the breakage is per-widget. See plan91.
            c.set_modifiers(self.router.modifiers);
            self.router.composer = Some(c);
        } else if deactivate {
            tracing::debug!("deactivate");
            self.router.current_active = false;
            self.router.composer = None;
            self.router.focused_app_id = None;
        }

        // After deactivate, composer is None — nothing more to do.
        let Some(w) = self.router.composer.as_mut() else {
            return;
        };

        // Re-sync shadow on every surrounding_text frame. Re-seed engine
        // ONLY on activate or genuine cursor jump (user clicked elsewhere) —
        // the reseed gate inside `observe_surrounding_bytes` enforces that.
        if let Some((text, cursor, anchor)) = frame.surrounding_text.as_ref() {
            let (prev_text, prev_cursor) = w.prev_surrounding_for_trace();
            tracing::trace!(
                text = %text,
                text_len = text.len(),
                cursor = *cursor,
                prev_text = %prev_text,
                prev_cursor,
                "on_done_frame surrounding_text"
            );

            // Runtime tier downgrade ST → FK. An app that advertises
            // surrounding-text support but never reflects our commits (Google
            // Docs / contenteditable in Firefox report text="" cursor=0 on
            // every frame) silently no-ops delete_surrounding_text, so each
            // correction's commit doubles the word ("Tiếng" → "Tieêngếng").
            // Detect the dead-surrounding signature and fall back to ForwardKey,
            // whose real Backspace keystrokes the client does honor. Symmetric
            // to the late upgrade below but in the other direction; it must run
            // BEFORE the duplicate-frame guard because every dead frame is
            // byte-identical and would otherwise be swallowed there.
            if !activate
                && w.method() == BackspaceMethod::SurroundingText
                && w.note_surrounding_liveness(text, *cursor)
            {
                tracing::info!(
                    from = ?BackspaceMethod::SurroundingText,
                    to = ?BackspaceMethod::ForwardKey,
                    "surrounding text non-functional (empty frames despite commits) → downgrade tier"
                );
                w.set_method(BackspaceMethod::ForwardKey);
                // Dead surrounding-text ⟹ dead text-input-v3 server contract
                // ⟹ commit_string also silently dropped (common cause). Route
                // ForwardKey commits through vk/keysym instead.
                w.commit_string_functional = false;
            }

            // Duplicate-frame guard. KWin re-emits the same SurroundingText
            // 2-3 times per keystroke. We've already processed this state —
            // running the reseed logic again resets and burns engine context.
            // Let Composer keep duplicate frames that are still meaningful for
            // pending correction-echo checks.
            if w.should_skip_surrounding_frame(text, *cursor, *anchor, activate, deactivate) {
                return;
            }

            // Late tier upgrade. Chromium under wlroots/sway doesn't include
            // set_surrounding_text in its enable cycle, so the activate frame
            // has has_surrounding=false and the tier detector picks ForwardKey.
            // The first real keystroke then arrives with surrounding info —
            // re-detect and upgrade to Tier 1 SurroundingText so
            // autocomplete-selection handling works. Upgrade-only: never
            // downgrade ST → FK here.
            if !activate && w.method() == BackspaceMethod::ForwardKey {
                let probe = CapabilityProbe {
                    purpose: frame.purpose,
                    surrounding_text_seen: true,
                    terminal_override: self.terminal_override,
                };
                let upgraded = detect_method(&probe);
                if upgraded == BackspaceMethod::SurroundingText {
                    tracing::info!(
                        from = ?BackspaceMethod::ForwardKey,
                        to = ?upgraded,
                        "late tier upgrade on first surrounding_text"
                    );
                    w.set_method(upgraded);
                }
            }

            // The ONE reseed gate. force_reseed on activate (always seed from
            // the word at cursor); otherwise the gate decides via the one-char
            // / recent-action heuristics.
            w.observe_surrounding_bytes(text, ByteCursor(*cursor), ByteCursor(*anchor), activate);
        } else if !activate && !deactivate {
            w.clear_prev_surrounding();
        }
    }

    fn on_key_pressed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        key: u32,
        ch: Option<char>,
    ) -> KeyDecision {
        self.sync_config();

        // A key REPEAT (wl_keyboard state=2) is not a fresh keystroke: it must
        // never mutate the compose engine or re-Apply a commit. We still run the
        // forward decision so nav / modifier-shortcut repeats reach the client
        // (forwarded with value=2 via the ctx helpers), but skip every engine
        // side effect and swallow repeats of compose keys below.
        let repeat = ctx.is_repeat();

        if !repeat {
            if let Some(w) = self.router.composer.as_mut() {
                w.check_idle_reset();
            }
        }

        let now_enabled = self.enabled.load(Ordering::Acquire);
        if !repeat {
            if self.router.last_enabled && !now_enabled {
                if let Some(w) = self.router.composer.as_mut() {
                    w.full_reset();
                }
            }
            self.router.last_enabled = now_enabled;
        }
        if !now_enabled {
            return KeyDecision::ForwardRaw;
        }

        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.router.modifiers.intersects(shortcut_mods) {
            tracing::debug!(key, mods = ?self.router.modifiers,
                "modifier shortcut → bypass engine + forward");
            // Same rationale as NAV: shortcuts (Ctrl+V paste, Ctrl+A select,
            // etc.) end the current composition. Roll the action clock back so
            // the next surrounding frame can re-seed from whatever word the
            // cursor lands on. (A repeat already did this on the initial press.)
            if !repeat {
                if let Some(w) = self.router.composer.as_mut() {
                    w.full_reset();
                    w.defer_action();
                }
            }
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        if NAV_KEYS.contains(&key) {
            tracing::debug!(key, "key: nav → reset shadow + forward");
            // Roll the action clock back so the next surrounding frame bypasses
            // the recent-action gate and re-seeds. NAV ends the current
            // composition — the cursor is now elsewhere and the next char
            // should compose against whatever word the cursor lands on (killer
            // feature, multi-hop: "bò bo|" → arrows → "bò bof|" → composes).
            if !repeat {
                if let Some(w) = self.router.composer.as_mut() {
                    w.full_reset();
                    w.defer_action();
                }
            }
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        if self.router.composer.is_none() {
            tracing::trace!(key, "key: no active window → forward");
            return KeyDecision::ForwardRaw;
        }

        // Past here keys feed the compose engine. A repeat must not: swallow it
        // so a held letter / backspace doesn't re-type or re-delete through the
        // engine. (Cursor-nav repeats already returned above via the forward
        // paths; this only drops compose-key auto-repeat, which Vietnamese
        // typing never relies on.)
        if repeat {
            return KeyDecision::Consumed;
        }

        if key == KEY_BACKSPACE {
            // Don't mark an action: BS deletes, doesn't compose. The killer
            // feature (retroactive tone after space+BS+'f' → bò) needs the
            // surrounding-text re-seed to fire on the BS's post-surrounding
            // frame. If BS marked an action, the 150ms recent_action gate would
            // block re-seed, leaving the engine empty so the next 'f' is
            // forwarded as ASCII.
            return self.handle_backspace();
        }

        if let Some(w) = self.router.composer.as_mut() {
            w.mark_action();
        }

        let Some(ch) = ch else {
            tracing::trace!(key, "key: no xkb char → forward raw");
            return KeyDecision::ForwardRaw;
        };

        let decision = self.handle_char(ch);

        // v1/KWin ForwardKey: a passthrough char (engine didn't consume it, or
        // emitted it unchanged) is normally forwarded as a raw keycode. But
        // KWin's notifyKeyboardKey for an IM-forwarded key does NOT refresh the
        // client's modifier state — it only flushes modifiers on keysym/commit
        // — so the client re-decodes the keycode at BASE level. Any char the
        // active modifier level changed from base (Shift → `WAYLAND`, AltGr/
        // Level3 → `€`, CapsLock, …) would therefore be corrupted. daklak
        // already decoded `ch` at the correct level; commit it as text instead
        // of forwarding the bare keycode. Base-level chars (the common
        // unshifted case) keep raw-keycode forwarding; control keys (Enter/Tab)
        // are never committed as text.
        if matches!(decision, KeyDecision::ForwardRaw)
            && ctx.profile().has_keysym_commit
            && ctx.is_level_shifted(key, ch)
            && !ch.is_control()
        {
            let method = self
                .router
                .composer
                .as_mut()
                .map(|w| {
                    w.unrecord_forwarded_char();
                    w.method()
                })
                .unwrap_or(BackspaceMethod::ForwardKey);
            tracing::debug!(ch = %ch, "level-shifted passthrough → commit (v1 keycode loses modifier level)");
            return KeyDecision::Apply {
                method,
                backspaces: 0,
                commit: ch.to_string(),
            };
        }

        decision
    }

    fn apply_pending(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        time: u32,
        backspaces: usize,
        commit: &str,
        raw_mods: (u32, u32, u32, u32),
        held_user_kc: Option<u32>,
    ) {
        let serial = ctx.serial();
        if let Some(w) = self.router.composer.as_mut() {
            tracing::debug!(method = ?w.method(), backspaces, commit, "strategy.apply");
            let commit_string_functional = w.commit_string_functional;
            ctx.with_sink(raw_mods, held_user_kc, commit_string_functional, |sink| {
                w.apply_to_sink(backspaces, commit, serial, time, sink);
            });
        }
    }

    fn on_modifiers(&mut self, _ctx: &mut AdapterCtx<'_>, m: ModifierState) {
        self.router.modifiers = m;
        if let Some(w) = self.router.composer.as_mut() {
            w.set_modifiers(m);
        }
    }

    fn on_focus_changed(
        &mut self,
        ctx: &mut AdapterCtx<'_>,
        app_id: Option<String>,
        is_xwayland: bool,
    ) {
        // Native clients (wlroots terminals like Ghostty, XWayland apps like
        // OnlyOffice) focus and deliver IM keyboard-grab keys without ever
        // enabling text-input, so no compositor `Activate` fires and capability
        // detection never runs. When the transport exposes a virtual keyboard,
        // synthesize a ForwardKey session from focus metadata: its key-channel
        // fallback (`commit_string_functional = false`) emits every replacement
        // through daklak's synthetic Vietnamese keymap on
        // `zwp_virtual_keyboard_v1` — the route that reliably reaches
        // no-text-input clients (incl. XWayland). A real `Activate` always
        // wins and replaces this synthetic session (see `on_done_frame`).
        let vk_available = ctx.profile().has_vk_keyboard;
        let matched = app_id.is_some() && vk_available;

        let synthetic_target_changed = self.router.synthetic_active
            && matched
            && self.router.focused_app_id != app_id;

        if matched && (!self.router.current_active || synthetic_target_changed) {
            let id = app_id.clone().unwrap_or_default();

            tracing::info!(app_id = %id, is_xwayland,
                "synthetic activate → ForwardKey (focus without text-input)");
            self.router.current_active = true;
            self.router.synthetic_active = true;
            self.router.focused_app_id = app_id;
            let mut c = Composer::new(
                self.config.method.to_engine(),
                BackspaceMethod::ForwardKey,
                self.config.bracket_shortcuts,
            );
            c.set_modern_style(self.config.modern_style);
            c.set_modifiers(self.router.modifiers);
            // No text-input session exists for this client, so commit_string is
            // dead. Force ForwardKey to route replacements through the virtual
            // keyboard's synthetic keymap instead.
            c.commit_string_functional = false;
            self.router.composer = Some(c);
        } else if self.router.synthetic_active && !matched {
            tracing::info!(
                old = ?self.router.focused_app_id,
                new = ?app_id,
                is_xwayland,
                "synthetic deactivate (focus lost / no virtual keyboard)"
            );
            self.router.current_active = false;
            self.router.synthetic_active = false;
            self.router.composer = None;
            self.router.focused_app_id = None;
        } else if !self.router.synthetic_active && matched {
            tracing::trace!("focus_changed skipped: already active via IM");
        }
    }
}
