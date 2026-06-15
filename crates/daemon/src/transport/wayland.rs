//! Wayland transport glue: `AdapterHandler` impl. Translates text_input_v1/v2
//! frames into `Composer` calls. Owns the wayland-only frame bookkeeping (the
//! late-tier-upgrade probe, the duplicate-frame guard's activate/deactivate
//! framing, `AdapterCtx` plumbing). The reseed gate itself lives once on
//! `Composer::observe_surrounding_bytes`.

use std::sync::atomic::Ordering;

use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, ModifierState, SurroundingFrame,
};
use viet_ime_wayland_adapter::{AdapterCtx, AdapterHandler, FrameSnapshot, KeyDecision};

use crate::composer::{ByteCursor, Composer};
use crate::handler::{Daemon, KEY_BACKSPACE, NAV_KEYS};

impl AdapterHandler for Daemon {
    fn on_done_frame(&mut self, ctx: &mut AdapterCtx<'_>, frame: &FrameSnapshot) {
        if frame.activate && self.synthetic_active {
            tracing::info!(
                "real Activate received while synthetic session active → drop synthetic"
            );
            self.current_active = false;
            self.synthetic_active = false;
            self.composer = None;
            self.focused_app_id = None;
        }

        let activate = frame.activate && !self.current_active;
        let deactivate = frame.deactivate && self.current_active;

        if activate {
            let app_id = frame.app_id.clone();
            tracing::info!(app_id = ?app_id, "activate");
            self.current_active = true;
            self.focused_app_id = app_id;

            // VkOnly→UInput when the transport has no vk keyboard is clamped
            // inside detect_method now (no inline backend-name check — #3).
            let method = self.detect_capability(frame, ctx.profile().has_vk_keyboard);
            tracing::info!("capability detected: {:?}", method);
            let mut c = Composer::new(
                self.config.method.to_engine(),
                method,
                self.config.bracket_shortcuts,
            );
            let chars_for_delete = self
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
            // firefox needs both the char-count delete AND the post-apply
            // debounce; they are independent quirks that this app
            // happens to require together.
            c.set_window_quirks(chars_for_delete, chars_for_delete);
            if chars_for_delete {
                tracing::info!(
                    app_id = ?self.focused_app_id,
                    "force_chars_delete_apps match → char-count delete + debounce barrier"
                );
            }
            c.set_modifiers(self.modifiers);
            self.composer = Some(c);
        } else if deactivate {
            tracing::debug!("deactivate");
            self.current_active = false;
            self.composer = None;
            self.focused_app_id = None;
        }

        // After deactivate, composer is None — nothing more to do.
        let focused_app_id = self.router.focused_app_id.clone();
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
                raw_word = %w.raw_word,
                "on_done_frame surrounding_text"
            );

            // Duplicate-frame guard. KWin re-emits the same SurroundingText
            // 2-3 times per keystroke. We've already processed this state —
            // running the reseed logic again wipes raw_word and burns engine
            // context. Skip if nothing changed (but never on activate/deactivate
            // framing, which must always re-evaluate).
            if !activate && !deactivate && w.is_duplicate_frame(text, *cursor, *anchor) {
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
                    surrounding_text_seen: Some(SurroundingFrame {
                        text: text.clone(),
                        cursor: *cursor,
                    }),
                    app_id: focused_app_id.clone(),
                    force_uinput_apps: self.config.force_uinput_apps.clone(),
                    force_vk_only_apps: self.config.force_vk_only_apps.clone(),
                    terminal_override: self.terminal_override,
                    vk_keyboard_available: ctx.profile().has_vk_keyboard,
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
        // A key REPEAT (wl_keyboard state=2) is not a fresh keystroke: it must
        // never mutate the compose engine or re-Apply a commit. We still run the
        // forward decision so nav / modifier-shortcut repeats reach the client
        // (forwarded with value=2 via the ctx helpers), but skip every engine
        // side effect and swallow repeats of compose keys below.
        let repeat = ctx.is_repeat();

        if !repeat {
            if let Some(w) = self.composer.as_mut() {
                w.check_idle_reset();
            }
        }

        let now_enabled = self.enabled.load(Ordering::Acquire);
        if !repeat {
            if self.last_enabled && !now_enabled {
                if let Some(w) = self.composer.as_mut() {
                    w.full_reset();
                }
            }
            self.last_enabled = now_enabled;
        }
        if !now_enabled {
            return KeyDecision::ForwardRaw;
        }

        let shortcut_mods = ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.modifiers.intersects(shortcut_mods) {
            tracing::debug!(key, mods = ?self.modifiers,
                "modifier shortcut → bypass engine + forward");
            // Same rationale as NAV: shortcuts (Ctrl+V paste, Ctrl+A select,
            // etc.) end the current composition. Roll the action clock back so
            // the next surrounding frame can re-seed from whatever word the
            // cursor lands on. (A repeat already did this on the initial press.)
            if !repeat {
                if let Some(w) = self.composer.as_mut() {
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
                if let Some(w) = self.composer.as_mut() {
                    w.full_reset();
                    w.defer_action();
                }
            }
            ctx.vk_key_press_unstamped(time, key);
            return KeyDecision::Consumed;
        }

        if self.composer.is_none() {
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

        if let Some(w) = self.composer.as_mut() {
            w.mark_action();
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
        if let Some(w) = self.composer.as_mut() {
            tracing::debug!(method = ?w.method, backspaces, commit, "strategy.apply");
            let delete_in_chars = w.delete_in_chars;
            ctx.with_sink(raw_mods, held_user_kc, delete_in_chars, |sink| {
                w.apply_to_sink(backspaces, commit, serial, time, sink);
            });
        }
    }

    fn on_modifiers(&mut self, _ctx: &mut AdapterCtx<'_>, m: ModifierState) {
        self.modifiers = m;
        if let Some(w) = self.composer.as_mut() {
            w.set_modifiers(m);
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
        let vk_only_matched = !in_force_uinput && (in_force_vk_only || auto_xwayland_vk_only);
        let vk_available = ctx.profile().has_vk_keyboard;
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
            let mut c = Composer::new(
                self.config.method.to_engine(),
                BackspaceMethod::VkOnly,
                self.config.bracket_shortcuts,
            );
            c.set_modifiers(self.modifiers);
            self.composer = Some(c);
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
            self.composer = None;
            self.focused_app_id = None;
        } else if !self.synthetic_active && matched {
            tracing::trace!("focus_changed skipped: already active via IM");
        }
    }

    fn window_debounce_barrier(&self) -> bool {
        self.composer.as_ref().map(|w| w.debounce_barrier).unwrap_or(false)
    }
}
