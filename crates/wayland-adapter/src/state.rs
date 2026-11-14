use std::collections::VecDeque;
use std::time::{Duration, Instant};

use viet_ime_edit_strategy::ModifierState;
use viet_ime_key_emitter::HeldKeys;
use crate::focus::{FocusEvent, FocusTracker};
use crate::frame::DoneFrame;
use crate::wire::Wire;
use viet_ime_keymap::xkb::XkbState;

/// Adapter-side state. Owns Wayland proxies + emit-history. The handler
/// (daemon) never sees these directly — it interacts via `AdapterCtx`.
#[derive(Debug, Clone, Copy)]
struct HeldRepeat {
    key: u32,
    wayland_time: u32,
    pressed_at: Instant,
    deadline: Instant,
}

/// Client-side wl_keyboard repeat state. A positive compositor rate means
/// the keyboard client must synthesize repeats; rate zero means the
/// compositor supplies state=2 events itself.
#[derive(Debug)]
pub(crate) struct ClientRepeat {
    rate: i32,
    delay: i32,
    held: Option<HeldRepeat>,
}

impl Default for ClientRepeat {
    fn default() -> Self {
        Self {
            // Input-method-v1 may omit repeat_info. Match kime's conservative
            // fallback until the compositor supplies its actual settings.
            rate: 20,
            delay: 400,
            held: None,
        }
    }
}

impl ClientRepeat {
    pub(crate) fn set_info(&mut self, rate: i32, delay: i32) {
        self.rate = rate;
        self.delay = delay;
        if rate <= 0 {
            self.held = None;
        }
    }

    pub(crate) fn arm(&mut self, key: u32, wayland_time: u32, now: Instant) {
        if self.rate <= 0 {
            return;
        }
        self.held = Some(HeldRepeat {
            key,
            wayland_time,
            pressed_at: now,
            deadline: now + Duration::from_millis(self.delay.max(0) as u64),
        });
    }

    pub(crate) fn release(&mut self, key: u32) {
        if matches!(self.held, Some(held) if held.key == key) {
            self.held = None;
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.held = None;
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.held.map(|held| held.deadline)
    }

    pub(crate) fn fire(&mut self, now: Instant) -> Option<(u32, u32)> {
        let held = self.held.as_mut()?;
        if now < held.deadline || self.rate <= 0 {
            return None;
        }
        let elapsed_ms = now.duration_since(held.pressed_at).as_millis() as u32;
        let event = (held.key, held.wayland_time.wrapping_add(elapsed_ms));
        let interval = Duration::from_secs_f64(1.0 / self.rate as f64);
        held.deadline = now + interval;
        Some(event)
    }
}

pub struct AdapterState {
    pub(crate) wire: Wire,

    // Arrives asynchronously from the keyboard grab.
    pub xkb: Option<XkbState>,

    // Protocol state
    pub serial: u32,
    pub modifiers: ModifierState,
    pub raw_mods: (u32, u32, u32, u32), // depressed, latched, locked, group

    // Pending double-buffered frame (applied at Done)
    pub pending_frame: DoneFrame,

    pub(crate) echo: ModifierEcho,

    /// User keys whose press daklak forwarded to the focused client and whose
    /// release it has not forwarded yet (tail-char-drop fix, see
    /// [`HeldKeys`]). Cleared on deactivate: the grab that would deliver the
    /// releases is gone.
    pub held_forwarded: HeldKeys,
    /// True while dispatching a key-REPEAT event (wl_keyboard state=2) rather
    /// than a fresh press. Forward helpers read this to emit `value=2` so the
    /// focused client sees a proper repeat. Without it, rate-0 clients that
    /// rely on the compositor's server-side repeat (Chromium/Electron on KWin)
    /// get no continuous-key behavior through the IM grab. See dispatch_key_repeat.
    pub forwarding_repeat: bool,
    /// Local repeat timer state used when wl_keyboard repeat_info has rate>0.
    /// Only IM-consumed keys are armed; forwarded held keys repeat in the
    /// focused client itself.
    pub(crate) client_repeat: ClientRepeat,

    pub should_exit: bool,

    /// Optional metadata for focus-driven sessions; text-input works without it.
    pub(crate) focus: Option<FocusTracker>,
}

/// Suppress only expected modifier echoes; unrelated physical updates survive.
#[derive(Default)]
pub(crate) struct ModifierEcho {
    pub pending: u32,
    pub expected: VecDeque<(u32, u32, u32, u32)>,
    pub emitted_at: Option<Instant>,
}

impl ModifierEcho {
    pub fn suppress(&mut self, incoming: (u32, u32, u32, u32)) -> bool {
        if self.pending > 0 && self.emitted_at.is_some_and(|t| t.elapsed() > Duration::from_millis(50)) {
            *self = Self::default();
        }
        if self.expected.front() != Some(&incoming) {
            return false;
        }
        self.expected.pop_front();
        self.pending = self.expected.len() as u32;
        if self.pending == 0 {
            self.emitted_at = None;
        }
        true
    }

    pub fn emission(&mut self) -> viet_ime_key_emitter::SyntheticMods<'_> {
        viet_ime_key_emitter::SyntheticMods {
            pending: &mut self.pending,
            expected: &mut self.expected,
            emitted_at: &mut self.emitted_at,
        }
    }
}

impl AdapterState {
    /// A consumed physical key never reaches the focused client, which
    /// therefore cannot self-repeat it. For positive repeat_info rates the
    /// keyboard client (daklak) must synthesize repeats itself.
    pub(crate) fn arm_consumed_key_repeat(&mut self, key: u32, time: u32) {
        let repeatable = self.xkb.as_ref().is_none_or(|xkb| xkb.key_repeats(key));
        if repeatable {
            self.client_repeat.arm(key, time, Instant::now());
        }
    }

    pub(crate) fn new(wire: Wire, focus: Option<FocusTracker>) -> Self {
        Self {
            wire,
            xkb: None,
            serial: 0,
            modifiers: ModifierState::empty(),
            raw_mods: (0, 0, 0, 0),
            pending_frame: DoneFrame::default(),
            echo: ModifierEcho::default(),
            held_forwarded: HeldKeys::default(),
            forwarding_repeat: false,
            client_repeat: ClientRepeat::default(),
            should_exit: false,
            focus,
        }
    }

    /// A state on a real wire whose requests go to the returned peer instead
    /// of a compositor. Keep the peer alive for the duration of the test.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(protocol: crate::ImProtocol) -> (Self, crate::wire::TestPeer) {
        let (wire, peer) = Wire::for_test(protocol);
        (Self::new(wire, None), peer)
    }

    pub fn focus_snapshot(&self) -> Option<FocusEvent> {
        self.focus.as_ref().and_then(FocusTracker::current)
    }

    /// The wl_keyboard key state for a press: `2` (Repeated) while dispatching
    /// a repeat, else `1` (Pressed). Release is always `0` (passed explicitly).
    pub fn press_value(&self) -> u32 {
        if self.forwarding_repeat {
            2
        } else {
            1
        }
    }

    /// Raw passthrough through the connection's selected key channel.
    pub fn emit_forward_key(&mut self, time: u32, key: u32, value: u32) {
        self.wire.forward_key(self.serial, time, key, value);
    }

    /// Single funnel for input-method events that mutate `pending_frame`.
    /// Backend dispatchers translate their protocol enum into `FrameEvent`
    /// and call this. Centralizes tracing, field assignment, and v1's
    /// `pending_commit` bookkeeping so changes can't drift between v1/v2.
    pub fn apply_event(&mut self, event: crate::frame::FrameEvent) {
        use crate::frame::{FrameEvent, SurroundingText};
        match event {
            FrameEvent::Activate => {
                tracing::trace!("im: Activate");
                self.pending_frame.pending_activate = true;
                // v1 resets pending_commit at activate (CommitState
                // batch starts fresh). v2 doesn't track this field.
                self.wire.mark_pending_commit(false);
            }
            FrameEvent::Deactivate => {
                tracing::trace!("im: Deactivate");
                self.pending_frame.pending_deactivate = true;
            }
            FrameEvent::SurroundingText { text, cursor, anchor } => {
                tracing::trace!(text = %text, cursor, anchor, "im: SurroundingText");
                self.pending_frame.surrounding_text =
                    Some(SurroundingText { text, cursor, anchor });
                self.wire.mark_pending_commit(true);
            }
            FrameEvent::Purpose(purpose) => {
                tracing::trace!(purpose, "im: ContentType");
                self.pending_frame.purpose = purpose;
                self.wire.mark_pending_commit(true);
            }
            FrameEvent::ChangeCause(cause) => {
                tracing::trace!(cause, "im: TextChangeCause");
                self.pending_frame.change_cause = Some(cause);
            }
            FrameEvent::Unavailable => {
                tracing::error!("compositor sent Unavailable — another IM is registered");
                self.should_exit = true;
            }
        }
    }
}

#[cfg(test)]
mod repeat_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn consumed_key_arms_repeat_after_compositor_delay() {
        let now = Instant::now();
        let mut repeat = ClientRepeat::default();
        repeat.set_info(25, 600);

        repeat.arm(24, 1234, now);

        assert_eq!(repeat.deadline(), Some(now + Duration::from_millis(600)));
        assert_eq!(repeat.fire(now + Duration::from_millis(600)), Some((24, 1834)));
        assert_eq!(repeat.deadline(), Some(now + Duration::from_millis(640)));
    }

    #[test]
    fn release_and_zero_rate_cancel_repeat() {
        let now = Instant::now();
        let mut repeat = ClientRepeat::default();
        repeat.set_info(25, 600);
        repeat.arm(24, 1234, now);

        repeat.release(24);
        assert_eq!(repeat.deadline(), None);

        repeat.arm(24, 1234, now);
        repeat.set_info(0, 600);
        assert_eq!(repeat.deadline(), None);
    }

    #[test]
    fn release_of_another_key_does_not_cancel_repeat() {
        let now = Instant::now();
        let mut repeat = ClientRepeat::default();
        repeat.set_info(25, 600);
        repeat.arm(24, 1234, now);

        repeat.release(38);

        assert_eq!(repeat.deadline(), Some(now + Duration::from_millis(600)));
    }
}
