pub mod dispatch;
pub mod frame;
pub mod keymap;
pub mod xkb;

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, QueueHandle,
};

use crate::config::Config;
use crate::protocols::{
    input_method_v2::{
        zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::ZwpInputMethodV2,
    },
    virtual_keyboard_v1::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
};
use crate::sink::WaylandSink;
use crate::window::WindowState;
use viet_ime_edit_strategy::uinput_device::UinputDevice;
use viet_ime_edit_strategy::{
    detect_method, BackspaceMethod, CapabilityProbe, ModifierState, SurroundingFrame,
};

use frame::DoneFrame;
use keymap::DaklakKeymap;
use xkb::XkbState;

// Linux evdev code for Backspace.
const KEY_BACKSPACE: u32 = 14;
// Navigation keys that move the cursor — trigger shadow reset.
const NAV_KEYS: &[u32] = &[
    105, 106, 103, 108, // Left, Right, Up, Down
    102, 107,           // Home, End
    104, 109,           // PageUp, PageDown
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

/// Entire application state. Owns all Wayland proxy objects and per-window
/// composition state. The `Dispatch` trait impls (in dispatch.rs) call methods
/// here.
pub struct AppState {
    pub config: Config,

    // Wayland proxies (set after globals binding + setup)
    pub seat: Option<WlSeat>,
    pub im_manager: Option<ZwpInputMethodManagerV2>,
    pub vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    pub im: Option<ZwpInputMethodV2>,
    pub grab: Option<ZwpInputMethodKeyboardGrabV2>,
    pub vk: Option<ZwpVirtualKeyboardV1>,

    /// Connection clone for grab-release/regrab flush around Tier 3 uinput
    /// emission. With this in place we can prove whether BS escapes the grab
    /// AND whether the focused surface honors it.
    pub conn: Option<Connection>,
    pub qh: Option<QueueHandle<AppState>>,


    // xkb state (set on first Keymap event from the grab)
    pub xkb: Option<XkbState>,
    pub keymap_init: bool,

    /// Synthetic xkb keymap with Vietnamese precomposed chars at evdev
    /// 200+. Handed to `vk.keymap()` on the first Keymap event so vk_key
    /// events can deliver Vietnamese chars without `commit_string` / a
    /// `zwp_text_input_v3` activate (Qt5/XWayland path). `None` if
    /// xkbcommon rejects the synthesized keymap — daklak then falls back
    /// to forwarding the compositor's keymap to `vk.keymap()`.
    pub daklak_keymap: Option<DaklakKeymap>,

    // Protocol state
    pub serial: u32,
    pub modifiers: ModifierState,
    pub raw_mods: (u32, u32, u32, u32), // depressed, latched, locked, group

    // Pending double-buffered frame (applied at Done)
    pub pending_frame: DoneFrame,
    pub current_active: bool,

    /// Timestamp of the last user-keystroke daemon action — used to
    /// distinguish "compositor echo of our action" (recent) from "user
    /// clicked mid-word" (not recent) in surrounding_text frames.
    pub last_action_at: Instant,

    // Per-text-input composition state (one at a time on wlroots)
    pub window: Option<WindowState>,

    // uinput device for Tier 3 — None if /dev/uinput is not accessible
    pub uinput: Option<UinputDevice>,

    /// Queue of (keycode, value, emitted_at) for kernel events daklak just
    /// synthesized via /dev/uinput. Each entry is round-tripped through the
    /// IM grab; on_key_pressed / on_key_released match and drop the matching
    /// entry so we don't re-process our own emissions.
    pub pending_self_emits: VecDeque<(u16, i32, Instant)>,

    /// Forced tier for `purpose == PURPOSE_TERMINAL`, read once from
    /// `DAKLAK_TERMINAL_TIER` at startup. None → use the detect_method
    /// default (ForwardKey).
    pub terminal_override: Option<BackspaceMethod>,

    /// Focused window's `app_id` captured at activate via Sway IPC
    /// (`focused_app_info()`). Threaded into the capability probe so
    /// known-broken-on-ForwardKey terminals can auto-escalate. `None`
    /// outside an active session or when Sway IPC is unavailable.
    pub focused_app_id: Option<String>,

    /// True when `current_active` was synthesized by daklak (Path C —
    /// Sway IPC focus matched `force_vk_only_apps`) rather than driven
    /// by a compositor `zwp_input_method_v2::Activate` event. Real
    /// activate always wins: if it fires, this flips back to false and
    /// the synthetic window is replaced by the normal capability path.
    pub synthetic_active: bool,

    pub should_exit: bool,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let uinput = match UinputDevice::open() {
            Ok(d) => {
                tracing::info!("uinput device opened (Tier 3 available)");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("uinput unavailable ({e}); Tier 3 demoted to ForwardKey");
                None
            }
        };

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
            seat: None,
            im_manager: None,
            vk_manager: None,
            im: None,
            grab: None,
            vk: None,
            conn: None,
            qh: None,
            xkb: None,
            keymap_init: false,
            daklak_keymap: match keymap::build() {
                Ok(km) => {
                    tracing::info!(
                        size = km.size,
                        chars = keymap::inventory_len(),
                        "synthetic Vietnamese keymap built (Path C)"
                    );
                    Some(km)
                }
                Err(e) => {
                    tracing::warn!(
                        "daklak keymap synthesis failed → falling back to compositor passthrough: {e:#}"
                    );
                    None
                }
            },
            serial: 0,
            modifiers: ModifierState::empty(),
            raw_mods: (0, 0, 0, 0),
            pending_frame: DoneFrame::default(),
            current_active: false,
            last_action_at: Instant::now() - std::time::Duration::from_secs(60),
            window: None,
            uinput,
            pending_self_emits: VecDeque::new(),
            terminal_override,
            focused_app_id: None,
            synthetic_active: false,
            should_exit: false,
        }
    }

    // ── Activation / deactivation ───────────────────────────────────────────

    /// Called when the Done event fires — apply the accumulated pending frame.
    pub fn apply_done_frame(&mut self) {
        self.serial = self.serial.wrapping_add(1);

        // Real compositor activate always wins over a synthetic (Sway-IPC-driven)
        // session. If one fires while we hold a synthetic VkOnly session,
        // tear the synthetic state down first so the regular capability
        // path can rebuild a real WindowState below.
        if self.pending_frame.pending_activate && self.synthetic_active {
            tracing::info!(
                "real Activate received while synthetic session active → drop synthetic"
            );
            self.current_active = false;
            self.synthetic_active = false;
            self.window = None;
            self.focused_app_id = None;
        }

        let activate = self.pending_frame.pending_activate && !self.current_active;
        let deactivate = self.pending_frame.pending_deactivate && self.current_active;

        tracing::debug!(
            serial = self.serial,
            activate,
            deactivate,
            purpose = self.pending_frame.purpose,
            has_surrounding = self.pending_frame.surrounding_text.is_some(),
            "Done frame"
        );

        if activate {
            let focused = crate::focused_app::focused_app_info();
            let (app_id, title) = focused
                .clone()
                .unwrap_or_else(|| ("?".to_owned(), "?".to_owned()));
            tracing::info!(app_id = %app_id, title = %title, "activate");
            self.current_active = true;
            // Stash for the capability probe + future per-app routing.
            // None on non-Sway compositors (focused_app_info returned None).
            self.focused_app_id = focused.map(|(id, _)| id);

            // Detect capability from this frame's surrounding_text.
            let method = self.detect_capability();
            let effective_method = if method == BackspaceMethod::UInput && self.uinput.is_none() {
                tracing::debug!("UInput requested but unavailable → ForwardKey");
                BackspaceMethod::ForwardKey
            } else {
                method
            };

            tracing::info!("capability detected: {:?}", effective_method);
            let ws = WindowState::new(self.config.method.to_engine(), effective_method);
            self.window = Some(ws);
        } else if deactivate {
            tracing::debug!("deactivate");
            self.current_active = false;
            self.window = None;
            self.focused_app_id = None;
            // Clear sticky purpose so the next activate doesn't inherit it
            // (e.g. focusing chromium right after foot must not carry
            // purpose=13).
            self.pending_frame.end_session();
        }

        // Re-sync shadow on every surrounding_text frame. Re-seed engine
        // ONLY on activate or when there's no recent daemon action (= user
        // moved the cursor by clicking).
        //
        // Why not on every frame: vnkey-engine's feed_context resets on
        // non-ASCII chars. After we commit `â`, the next surrounding_text
        // echoes "trâ" → feed_context would wipe engine state, losing the
        // tone applied to `â`. Normal typing requires engine state to
        // accumulate naturally across keystrokes.
        if let Some(ref st) = self.pending_frame.surrounding_text.clone() {
            if let Some(ref mut w) = self.window {
                w.strategy.on_surrounding_text(&st.text, st.cursor);

                let recent_action = self.last_action_at.elapsed()
                    < std::time::Duration::from_millis(150);
                let should_reseed = activate || !recent_action;
                if should_reseed {
                    let word = current_word_before_cursor(&st.text, st.cursor);
                    w.engine.reset();
                    // Only seed with all-lowercase ASCII: Vietnamese typing
                    // is lowercase. Capitalized words signal English content
                    // (e.g. thunar's "Folder") where seeding poisons the
                    // engine and prevents subsequent compose triggers.
                    if !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase()) {
                        tracing::debug!(word, "re-seed engine (activate or cursor jump)");
                        w.engine.feed_context(word);
                    }
                }
            }
        }

        self.pending_frame.reset();
    }

    /// Sway IPC reported the focused window changed. Decide whether to
    /// synthesize an activate/deactivate for Path C (Tier 4 VkOnly).
    ///
    /// Synthetic activate fires when:
    ///   - no real compositor session is active, AND
    ///   - the new focused app_id matches `config.force_vk_only_apps`.
    ///
    /// Synthetic deactivate fires when we're in a synthetic session AND
    /// focus moves away to a non-matching app. Real activates take
    /// precedence — if they ever fire, we let them tear down the
    /// synthetic state first.
    pub fn on_focus_changed(&mut self, app_id: Option<String>) {
        let matched = app_id
            .as_deref()
            .map(|id| {
                let lower = id.to_ascii_lowercase();
                self.config
                    .force_vk_only_apps
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(&lower))
            })
            .unwrap_or(false);

        if matched && !self.current_active {
            // Bootstrap a synthetic VkOnly session.
            let id = app_id.clone().unwrap_or_default();
            tracing::info!(app_id = %id, "synthetic activate (force_vk_only_apps → VkOnly)");
            self.current_active = true;
            self.synthetic_active = true;
            self.focused_app_id = app_id;
            let effective_method = BackspaceMethod::VkOnly;
            let ws = WindowState::new(self.config.method.to_engine(), effective_method);
            self.window = Some(ws);
        } else if self.synthetic_active && !matched {
            // Synthetic session — focus moved away, tear it down.
            tracing::info!(
                old = ?self.focused_app_id,
                new = ?app_id,
                "synthetic deactivate (focus left force_vk_only_apps)"
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

    fn detect_capability(&self) -> BackspaceMethod {
        let probe = CapabilityProbe {
            purpose: self.pending_frame.purpose,
            surrounding_text_seen: self.pending_frame.surrounding_text.as_ref().map(|st| {
                SurroundingFrame {
                    text: st.text.clone(),
                    cursor: st.cursor,
                }
            }),
            app_id: self.focused_app_id.clone(),
            force_uinput_apps: self.config.force_uinput_apps.clone(),
            force_vk_only_apps: self.config.force_vk_only_apps.clone(),
            terminal_override: self.terminal_override,
        };
        detect_method(&probe)
    }

    // ── Key handling ────────────────────────────────────────────────────────

    /// Check whether the incoming grab key event matches a recent self-emit
    /// from /dev/uinput. Drains expired entries (>50ms old) before matching.
    /// Returns true if the event should be suppressed (i.e. dropped silently).
    fn suppress_self_emit(&mut self, key: u32, value: i32) -> bool {
        const SELF_EMIT_WINDOW: Duration = Duration::from_millis(50);
        while let Some(&(_, _, t)) = self.pending_self_emits.front() {
            if t.elapsed() > SELF_EMIT_WINDOW {
                self.pending_self_emits.pop_front();
            } else {
                break;
            }
        }
        if let Some(idx) = self
            .pending_self_emits
            .iter()
            .position(|&(k, v, _)| k as u32 == key && v == value)
        {
            self.pending_self_emits.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn on_key_pressed(&mut self, time: u32, key: u32) {
        // Suppress self-emit round-trips from /dev/uinput (Tier 3 BS / modifier
        // restore). Kernel round-trip is sub-millisecond; 50ms window absorbs
        // scheduler jitter without swallowing real user input (reaction time
        // >150ms).
        if self.suppress_self_emit(key, 1) {
            tracing::trace!(key, value = 1, "self-emit suppressed");
            return;
        }

        let Some(ref vk) = self.vk else { return };

        // 2-second idle reset
        if let Some(ref mut w) = self.window {
            w.check_idle_reset();
        }

        // Modifier shortcuts (Ctrl/Alt/Super + key): bypass engine, forward
        // raw. Shift is NOT included — Shift+letter is just uppercase.
        // Modifier+key may move cursor (Ctrl+arrow, Ctrl+Home, etc.) — leave
        // last_action_at alone so the resulting surrounding_text frame is
        // treated as a user cursor move (re-seed enabled).
        let shortcut_mods =
            ModifierState::CTRL | ModifierState::ALT | ModifierState::SUPER;
        if self.modifiers.intersects(shortcut_mods) {
            tracing::debug!(key, mods = ?self.modifiers,
                "modifier shortcut → bypass engine + forward");
            if let Some(ref mut w) = self.window {
                w.full_reset();
            }
            vk.key(time, key, 1);
            return;
        }

        // Navigation keys: reset shadow, forward key. Do NOT touch
        // last_action_at — the resulting cursor move must trigger re-seed
        // (killer feature: typing mid-word picks up new word context).
        if NAV_KEYS.contains(&key) {
            tracing::debug!(key, "key: nav → reset shadow + forward");
            if let Some(ref mut w) = self.window {
                w.full_reset();
            }
            vk.key(time, key, 1);
            return;
        }

        // Mark this as a daemon action: subsequent surrounding_text frames
        // arriving within 150ms are treated as compositor echoes (expected),
        // not as user mouse clicks. Done AFTER nav/shortcut handling so those
        // paths leave the timestamp alone.
        self.last_action_at = Instant::now();

        // If no active window, just forward
        if self.window.is_none() {
            tracing::trace!(key, "key: no active window → forward");
            vk.key(time, key, 1);
            return;
        }

        // Backspace
        if key == KEY_BACKSPACE {
            self.handle_backspace(time);
            return;
        }

        // Char conversion
        let ch = match self.xkb.as_ref().and_then(|x| x.key_to_char(key)) {
            Some(c) => c,
            None => {
                tracing::trace!(key, "key: no xkb char → forward raw");
                vk.key(time, key, 1);
                return;
            }
        };

        self.handle_char(time, key, ch);
    }

    pub fn on_key_released(&mut self, time: u32, key: u32) {
        if self.suppress_self_emit(key, 0) {
            tracing::trace!(key, value = 0, "self-emit suppressed");
            return;
        }
        if let Some(ref vk) = self.vk {
            vk.key(time, key, 0);
        }
    }

    fn handle_backspace(&mut self, time: u32) {
        let w = self.window.as_mut().unwrap();
        let r = w.engine.process_backspace();
        tracing::debug!(consumed = r.consumed, bs = r.backspaces, "engine.process_backspace");

        if r.consumed {
            let serial = self.serial;
            let im = self.im.as_ref().unwrap();
            let vk = self.vk.as_ref().unwrap();
            let mut sink = WaylandSink {
                im,
                vk,
                uinput: self.uinput.as_mut(),
                pending_self_emits: &mut self.pending_self_emits,
                serial,
            };
            tracing::debug!(method = ?self.window.as_ref().unwrap().method,
                bs = r.backspaces, commit = %r.commit, "strategy.apply (BS)");
            self.window
                .as_mut()
                .unwrap()
                .strategy
                .apply(r.backspaces, &r.commit, serial, time, &mut sink);
            drop(sink);
        } else {
            // Engine didn't consume — forward raw backspace evdev keycode (14).
            // Update shadow to match: app will delete the last char.
            tracing::trace!("BS not consumed → forward");
            self.window.as_mut().unwrap().strategy.shadow.text_mut().pop();
            if let Some(ref vk) = self.vk {
                vk.key(time, KEY_BACKSPACE, 1);
            }
        }

        if let Some(ref mut w) = self.window {
            w.last_keystroke_at = Instant::now();
        }
    }

    fn handle_char(&mut self, time: u32, key: u32, ch: char) {
        let w = self.window.as_mut().unwrap();

        // Killer feature for end-of-word: when engine has no pending
        // composition (fresh after idle_reset, focus change, or anywhere
        // we cleared state), seed from the current word in the shadow so
        // retroactive composition fires. e.g. user types "tran", pauses
        // (idle reset clears engine), types `af` — engine seeded with
        // "tran" turns `a` into `bs=2 commit="ân"` → "trân", then `f` into
        // `bs=2 commit="ần"` → "trần".
        if w.engine.at_word_beginning() {
            let shadow_text = w.strategy.shadow.text().to_owned();
            let word = current_word_before_cursor(&shadow_text, shadow_text.len() as u32);
            // Lowercase-only seed gate (see apply_done_frame comment above).
            if !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase()) {
                tracing::debug!(word, "seed engine from shadow at word boundary");
                w.engine.feed_context(word);
            }
        }

        let r = w.engine.process_key(ch);

        w.last_keystroke_at = Instant::now();

        tracing::debug!(key, ch = %ch, consumed = r.consumed,
            bs = r.backspaces, commit = %r.commit,
            shadow = %self.window.as_ref().unwrap().strategy.shadow.text(),
            "engine.process_key");

        if r.consumed {
            let serial = self.serial;
            let method = self.window.as_ref().unwrap().strategy.method();
            let uinput_path = method == BackspaceMethod::UInput;

            // Tier 3 race-free grab dance: release grab + flush + brief
            // sleep so compositor processes the release before kernel BS
            // arrives. Window must be wide enough for the compositor to
            // process our release request, but narrow enough that user
            // keystrokes can't slip through (worst case ~80ms/key for fast
            // typists — empirically a 30ms window let `n` of `traanf` leak
            // past daklak while we composed `â`). 3ms before + 3ms after
            // = 6ms total dance, well below human keystroke interval.
            if uinput_path {
                if let Some(g) = self.grab.take() {
                    g.release();
                }
                if let Some(ref c) = self.conn {
                    let _ = c.flush();
                }
                std::thread::sleep(Duration::from_millis(3));
            }

            {
                let im = self.im.as_ref().unwrap();
                let vk = self.vk.as_ref().unwrap();
                let mut sink = WaylandSink {
                    im,
                    vk,
                    uinput: self.uinput.as_mut(),
                    pending_self_emits: &mut self.pending_self_emits,
                    serial,
                };
                tracing::debug!(method = ?method, "strategy.apply (char)");
                self.window
                    .as_mut()
                    .unwrap()
                    .strategy
                    .apply(r.backspaces, &r.commit, serial, time, &mut sink);
            }

            if uinput_path {
                if let Some(ref c) = self.conn {
                    let _ = c.flush();
                }
                std::thread::sleep(Duration::from_millis(3));
                if let (Some(im), Some(qh)) = (self.im.as_ref(), self.qh.as_ref()) {
                    self.grab = Some(im.grab_keyboard(qh, ()));
                    if let Some(ref c) = self.conn {
                        let _ = c.flush();
                    }
                }
            }
            // Don't forward — engine consumed the key
        } else {
            // Forward original evdev keycode (NOT ch as u32 — that's the char
            // codepoint and would land on a wildly different evdev keycode,
            // e.g. '\r'=13=KEY_EQUAL='=').
            //
            // CRITICAL: even when engine doesn't claim the key, the engine
            // remembers it internally. When a later key DOES trigger consumed
            // (e.g. second 'a' of "aa" → "â"), engine returns `bs=N` counting
            // back into THESE forwarded chars. So the shadow must track them
            // too, otherwise delete_surrounding_text() gets byte_count=0 and
            // the daemon appends "â" without deleting "a" → "traâ" not "trâ".
            tracing::trace!(key, ch = %ch, "char not consumed → forward + shadow.push");
            self.window.as_mut().unwrap().strategy.shadow.text_mut().push(ch);
            if let Some(ref vk) = self.vk {
                vk.key(time, key, 1);
            }
        }
    }

    // ── Modifier handling ───────────────────────────────────────────────────

    pub fn on_modifiers(
        &mut self,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        // Update xkb state
        if let Some(ref mut xkb) = self.xkb {
            xkb.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
        }

        // Track our modifier bitmask for Tier 3 modifier guard
        let mut m = ModifierState::empty();
        if mods_depressed & 0x01 != 0 { m |= ModifierState::SHIFT; }
        if mods_depressed & 0x04 != 0 { m |= ModifierState::CTRL; }
        if mods_depressed & 0x08 != 0 { m |= ModifierState::ALT; }
        if mods_depressed & 0x40 != 0 { m |= ModifierState::SUPER; }
        self.modifiers = m;

        if let Some(ref mut w) = self.window {
            w.strategy.set_modifiers(m);
        }

        self.raw_mods = (mods_depressed, mods_latched, mods_locked, group);

        // Mirror to virtual keyboard (kime pattern: state.rs:716-720)
        if let Some(ref vk) = self.vk {
            vk.modifiers(mods_depressed, mods_latched, mods_locked, group);
        }
    }
}

// ── Wrapper for fd used with tokio AsyncFd ───────────────────────────────────

struct WlRawFd(RawFd);
impl AsRawFd for WlRawFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// ── Async event loop ─────────────────────────────────────────────────────────

pub async fn run_event_loop(
    conn: Connection,
    mut event_queue: EventQueue<AppState>,
    mut app: AppState,
) -> Result<()> {
    use tokio::signal;

    // Wrap raw fd for tokio readability notifications (WlRawFd does not own the fd)
    let raw = event_queue.as_fd().as_raw_fd();
    let wl_fd = AsyncFd::with_interest(WlRawFd(raw), Interest::READABLE)
        .context("AsyncFd on Wayland socket")?;

    let ipc_server = crate::ipc::IpcServer::bind().await;

    // Path C focus poller: when `force_vk_only_apps` is non-empty, spawn a
    // task that polls Sway IPC for focus changes every ~300ms and
    // forwards app_id transitions into AppState. swaymsg is fork+exec
    // (~5ms) — wrapped in spawn_blocking to keep the runtime fluid.
    // TODO: switch to a real Sway IPC subscription (single Unix socket +
    // SUBSCRIBE message) — this poll is good enough for v0 demo of Path
    // C. See focused_app.rs for current swaymsg shell-out.
    let mut focus_rx = if !app.config.force_vk_only_apps.is_empty() {
        let (tx, rx) = mpsc::unbounded_channel::<Option<String>>();
        tokio::spawn(async move {
            let mut last: Option<String> = None;
            loop {
                let info = tokio::task::spawn_blocking(crate::focused_app::focused_app_info)
                    .await
                    .ok()
                    .flatten();
                let app_id = info.map(|(id, _)| id);
                if app_id != last {
                    if tx.send(app_id.clone()).is_err() {
                        break; // receiver dropped
                    }
                    last = app_id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        });
        Some(rx)
    } else {
        None
    };

    loop {
        // Flush queued outgoing requests
        event_queue.flush().ok();

        let read_guard = event_queue.prepare_read();

        tokio::select! {
            biased;

            // Ctrl-C / SIGTERM
            _ = signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                drop(read_guard);
                break;
            }

            // IPC accept (optional; errors ignored)
            accepted = async {
                match &ipc_server {
                    Some(s) => Some(s.accept().await),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(Ok(stream)) = accepted {
                    tokio::spawn(crate::ipc::handle_connection(stream));
                }
                drop(read_guard);
            }

            // Wayland socket readable
            ready = wl_fd.readable() => {
                let mut guard = ready.context("AsyncFd poll")?;
                guard.clear_ready();
                if let Some(rg) = read_guard {
                    rg.read().ok();
                }
                event_queue.dispatch_pending(&mut app)
                    .context("Wayland dispatch_pending")?;

                if app.should_exit {
                    tracing::info!("compositor sent Unavailable — exiting");
                    break;
                }
            }

            // Sway IPC focus change (Path C — only present when
            // force_vk_only_apps is non-empty).
            Some(app_id) = async {
                match focus_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                drop(read_guard);
                tracing::debug!(?app_id, "sway IPC: focused app changed");
                app.on_focus_changed(app_id);
            }
        }
    }

    // Clean up Wayland objects
    if let Some(grab) = app.grab.take() {
        grab.release();
    }
    if let Some(im) = app.im.take() {
        im.destroy();
    }
    if let Some(vk) = app.vk.take() {
        vk.destroy();
    }
    event_queue.flush().ok();

    Ok(())
}

// ── Globals binding ──────────────────────────────────────────────────────────

/// Connect to the Wayland compositor, bind globals, create input method + vk.
pub fn connect(config: Config) -> Result<(Connection, EventQueue<AppState>, AppState)> {
    let conn = Connection::connect_to_env().context("connect to Wayland display")?;

    let (globals, mut event_queue) =
        registry_queue_init::<AppState>(&conn).context("registry_queue_init")?;

    let qh = event_queue.handle();
    let mut app = AppState::new(config);

    // Bind wl_seat
    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=8, ())
        .context("bind wl_seat")?;
    app.seat = Some(seat.clone());

    // Bind input method manager (required)
    let im_manager = globals
        .bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
        .context("bind zwp_input_method_manager_v2 — requires wlroots compositor")?;
    app.im_manager = Some(im_manager.clone());

    // Bind virtual keyboard manager (required for Tier 2)
    let vk_manager = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .context("bind zwp_virtual_keyboard_manager_v1")?;
    app.vk_manager = Some(vk_manager.clone());

    // Initial roundtrip to process registry events
    event_queue.roundtrip(&mut app).context("initial roundtrip")?;

    // Create input method + grab
    let im = im_manager.get_input_method(&seat, &qh, ());
    let grab = im.grab_keyboard(&qh, ());
    let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());

    tracing::info!("input method and virtual keyboard created");

    app.im = Some(im);
    app.grab = Some(grab);
    app.vk = Some(vk);
    app.conn = Some(conn.clone());
    app.qh = Some(qh.clone());

    // Second roundtrip: receive Keymap + initial state from grab
    event_queue.roundtrip(&mut app).context("second roundtrip")?;

    Ok((conn, event_queue, app))
}

#[cfg(test)]
mod tests {
    use super::current_word_before_cursor;

    #[test]
    fn extracts_word_at_end_of_line() {
        // cursor at end of "tran" → entire word
        assert_eq!(current_word_before_cursor("tran", 4), "tran");
    }

    #[test]
    fn extracts_word_in_middle_of_line() {
        // cursor mid-word: take chars from last-space to cursor
        assert_eq!(current_word_before_cursor("hello tran", 10), "tran");
    }

    #[test]
    fn extracts_partial_word_at_cursor() {
        // Regression: user clicks between 'a' and 'n' of "tran".
        // cursor=3 → word is "tra".
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
        // "trâ" = 4 bytes (t=1, r=1, â=2). cursor=4 lands on char boundary.
        assert_eq!(current_word_before_cursor("trâ", 4), "trâ");
    }

    #[test]
    fn handles_cursor_inside_multibyte_char() {
        // cursor=3 lands MID-byte of â (which spans bytes 2-3).
        // The fn must snap back to a valid char boundary.
        let r = current_word_before_cursor("trâ", 3);
        // Should be "tr" — the largest prefix ending on a char boundary <= 3
        assert_eq!(r, "tr");
    }

    #[test]
    fn cursor_beyond_text_clamps() {
        // Cursor larger than text length — clamp to len
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
    // Mirrors the `word.chars().all(|c| c.is_ascii_lowercase())` predicate at
    // both seed call sites (apply_done_frame and handle_char).

    #[test]
    fn seed_gate_skips_capitalized_word() {
        // Thunar "New Folder" → word_before_cursor = "Folder" → uppercase F
        // → gate rejects → engine starts clean for new Vietnamese typing.
        let word = "Folder";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_accepts_lowercase_vietnamese_precursor() {
        // Killer feature: user types `tran`, idles, types `f` → retroactive
        // `trần`. Seed must fire on "tran".
        let word = "tran";
        assert!(word.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn seed_gate_skips_word_with_digit() {
        // "abc1" is mixed; not Vietnamese-composable.
        let word = "abc1";
        assert!(!word.chars().all(|c| c.is_ascii_lowercase()));
    }
}
