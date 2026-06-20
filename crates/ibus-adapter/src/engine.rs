//! IBus Factory + Engine D-Bus interface implementations.
//!
//! The Factory creates Engine objects when ibus activates the daklak engine.
//! The Engine handles all ibus method calls (ProcessKeyEvent, FocusIn, etc.)
//! and emits signals (CommitText, DeleteSurroundingText, ForwardKeyEvent).
//!
//! Async model:
//! - All interface methods are async; zbus runs them on the caller's tokio executor.
//! - Engine state is behind Rc<RefCell<_>> — single-threaded LocalSet, no Mutex.
//! - ProcessKeyEvent emits all output signals BEFORE returning the bool so that
//!   D-Bus message ordering guarantees signals arrive before the method reply.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::Value;

use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision};

use crate::ibus_text::ibus_text;
use crate::keyval::{ibus_state_to_modifiers, keyval_to_char, keyval_to_evdev, IBUS_RELEASE_MASK};
use crate::sink::IbusSink;

/// IBus capability bits. We only care about surrounding-text.
const IBUS_CAP_SURROUNDING_TEXT: u32 = 1 << 5;

/// Delay between the forwarded BackSpace key events and the CommitText of a
/// single Telex correction on the ForwardKey tier.
///
/// Symptom (Google Docs in Firefox/Chrome): emitted back-to-back, each Telex
/// correction non-deterministically doubles or drops its diacritic
/// (`Tiếng` → `Tieng` / `Tiêến`) even though our shadow is correct — a delivery
/// race between the forwarded BackSpaces and the commit.
///
/// Root cause (confirmed in ibus + GTK im-module source): a client that hasn't
/// enabled post-process-key-event (Firefox/GTK) has ibus-daemon relay our
/// ForwardKeyEvent and CommitText as immediate, separate D-Bus signals. The
/// GTK im-module then commits text *synchronously* (inserts now) but routes a
/// ForwardKeyEvent through the GDK event queue (`gdk_event_put`), processed
/// *later* by the main loop. Emitted back-to-back, both signals arrive in one
/// client main-loop wakeup: the commit inserts the corrected letters while the
/// BackSpaces still sit queued → delete runs after insert → the word doubles.
///
/// The fix is to delay our CommitText so the client processes the
/// ForwardKeyEvent signal in an *earlier* main-loop iteration (draining the
/// queued BackSpaces) before the commit arrives. It must be a
/// `tokio::time::sleep` — even `0` ms — because that engages tokio's timer
/// driver and yields a real reactor-cycle gap; a bare `tokio::task::yield_now()`
/// adds no wall-clock gap, both signals still land in the same client wakeup,
/// and the bug returns (verified by elimination: remove → bug; `yield_now` →
/// bug; `sleep(0)` → fixed). NOT a zbus-flush issue — zbus already writes each
/// signal as a separate, fully-flushed, ordered `sendmsg(2)`.
///
/// Raise the duration if a slower client still races. The durable alternative
/// is preedit-based composition (no forwarded BackSpaces, no reorder).
const FORWARD_COMMIT_BARRIER: Duration = Duration::from_millis(0);

/// State shared between Factory and Engine instances.
/// Wrapped in Arc<Mutex> so zbus interface futures are Send.
/// Lock is never held across .await points — only during sync daemon calls.
pub struct EngineState<D> {
    pub daemon: D,
    pub enabled: Arc<AtomicBool>,
    has_surrounding: bool,
    /// Whether the client *explicitly* cleared the surrounding-text capability
    /// bit via SetCapabilities **in the current focus session**. Distinct from
    /// `has_surrounding == false`, which is also the optimistic-default state
    /// before any caps arrive and can linger as a stale focus-flap artifact.
    ///
    /// When true, a subsequent SetSurroundingText frame must NOT upgrade the
    /// tier back to SurroundingText: the client declared it has none, and some
    /// clients (Google Docs in Firefox) still emit a frame reflecting existing
    /// document text yet silently drop `delete_surrounding_text` — trusting the
    /// frame there re-breaks composition (`Tiếng` → `Tieêngếng`). Reset on
    /// focus-in so a *stale* caps=9 from a prior defocus (gedit) doesn't block
    /// that client's legitimate late upgrade.
    caps_cleared_surrounding: bool,
    /// app_id from FocusInId (lowercase). Used to detect Firefox for
    /// chars_for_delete.
    client_app_id: Option<String>,
    /// Default chars_for_delete apps list (from daemon config).
    chars_delete_apps: Vec<String>,
}

impl<D> EngineState<D> {
    pub fn new(daemon: D, enabled: Arc<AtomicBool>, chars_delete_apps: Vec<String>) -> Self {
        Self {
            daemon,
            enabled,
            has_surrounding: true, // optimistic default for GNOME
            caps_cleared_surrounding: false,
            client_app_id: None,
            chars_delete_apps,
        }
    }

    fn chars_for_delete(&self) -> bool {
        // chars_for_delete is a per-app wayland quirk (Firefox v1); for IBus
        // we always use char counts since IBus protocol uses Unicode scalars.
        // We keep the flag for future per-app workarounds if needed.
        self.client_app_id
            .as_deref()
            .map(|id| {
                self.chars_delete_apps
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(id))
            })
            .unwrap_or(false)
    }
}

/// D-Bus Factory — ibus calls CreateEngine to spawn our engine.
pub struct Factory<D> {
    state: Arc<tokio::sync::Mutex<EngineState<D>>>,
    next_id: std::sync::atomic::AtomicI32,
}

impl<D: IbusHandler + Send + 'static> Factory<D> {
    pub fn new(state: Arc<tokio::sync::Mutex<EngineState<D>>>) -> Self {
        Self {
            state,
            next_id: std::sync::atomic::AtomicI32::new(0),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.IBus.Factory")]
impl<D: IbusHandler + Send + 'static> Factory<D> {
    async fn create_engine(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        engine_name: &str,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = format!("/org/freedesktop/IBus/Engine/{id}");
        let op: zbus::zvariant::OwnedObjectPath = zbus::zvariant::ObjectPath::try_from(path.clone())
            .map_err(|e| zbus::fdo::Error::Failed(format!("bad path: {e}")))?
            .into();
        let engine = Engine {
            state: self.state.clone(),
        };
        server.at(&op, engine).await?;
        tracing::info!(engine_name, path, "CreateEngine");
        Ok(op)
    }
}

/// D-Bus Engine — handles all ibus method calls.
pub struct Engine<D> {
    state: Arc<tokio::sync::Mutex<EngineState<D>>>,
}

/// Trait that the daemon must implement to hook into the ibus adapter.
/// Implemented by crates/daemon handler::Daemon.
pub trait IbusHandler: Send {
    fn process_key(&mut self, evdev: u32, ch: Option<char>) -> KeyDecision;
    fn apply_with_sink(&mut self, backspaces: usize, commit: &str, time: u32, sink: &mut IbusSink);
    fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32);
    fn set_modifiers(&mut self, m: viet_ime_edit_strategy::ModifierState);
    fn activate_ibus(&mut self, method: BackspaceMethod, chars_for_delete: bool);
    fn deactivate_ibus(&mut self);
    /// React to a routing change mid-session.
    fn update_method(&mut self, method: BackspaceMethod);
    fn full_reset(&mut self);
}

#[zbus::interface(name = "org.freedesktop.IBus.Engine")]
impl<D: IbusHandler + Send + 'static> Engine<D> {
    /// Intercept a key press. Emit output signals BEFORE returning the bool
    /// so D-Bus message ordering ensures signals arrive first.
    /// Lock is dropped before every .await to satisfy Send bounds.
    async fn process_key_event(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
    ) -> bool {
        if state & IBUS_RELEASE_MASK != 0 {
            return false;
        }
        let evdev = keyval_to_evdev(keyval, keycode);
        let ch = keyval_to_char(keyval);
        let mods = ibus_state_to_modifiers(state);

        // Hold lock only for sync daemon calls; drop before .await.
        let (decision, sink_opt) = {
            let mut s = self.state.lock().await;
            s.daemon.set_modifiers(mods);
            let decision = s.daemon.process_key(evdev, ch);
            let sink_opt = if let KeyDecision::Apply { ref backspaces, ref commit, .. } = decision {
                let chars_for_delete = s.chars_for_delete();
                let mut sink = IbusSink::new(chars_for_delete);
                s.daemon.apply_with_sink(*backspaces, commit, 0, &mut sink);
                Some(sink)
            } else {
                None
            };
            (decision, sink_opt)
            // lock dropped here
        };

        match decision {
            KeyDecision::ForwardRaw => false,
            KeyDecision::Consumed => true,
            KeyDecision::Apply { .. } => {
                let sink = sink_opt.expect("Apply always produces sink");
                for d in &sink.deletes {
                    if let Err(e) =
                        Engine::<D>::delete_surrounding_text(&emitter, d.offset, d.n_chars).await
                    {
                        tracing::warn!("DeleteSurroundingText failed: {e}");
                    }
                }
                // Deletion must precede the commit in BOTH tiers: surrounding
                // emits delete_surrounding_text (above), ForwardKey emits
                // BackSpace key events here. Flushing commits first would
                // append the corrected text and only then backspace, dropping
                // the correction (`tieêngếng` instead of `tiếng` in foot).
                for fwd in &sink.forwards {
                    tracing::debug!(
                        keyval = format_args!("{:#06x}", fwd.keyval),
                        keycode = fwd.keycode,
                        state = fwd.state,
                        "emit ForwardKeyEvent D-Bus signal"
                    );
                    if let Err(e) = Engine::<D>::forward_key_event(
                        &emitter,
                        fwd.keyval,
                        fwd.keycode,
                        fwd.state,
                    )
                    .await
                    {
                        tracing::warn!("ForwardKeyEvent failed: {e}");
                    }
                }
                // Browser-race barrier between the forwarded BackSpaces and the
                // CommitText (see FORWARD_COMMIT_BARRIER). Without this sleep,
                // Google Docs reorders commit-before-delete and each correction
                // doubles/drops its diacritic. Must be `sleep` (timer-driver
                // round-trip), not `yield_now` — both removal and a yield_now
                // swap were verified to regress. Only a delete+commit correction
                // races; raw forwards or bare commits don't.
                if !sink.forwards.is_empty() && !sink.commits.is_empty() {
                    tokio::time::sleep(FORWARD_COMMIT_BARRIER).await;
                }
                for text in &sink.commits {
                    tracing::debug!(commit = %text, "emit CommitText D-Bus signal");
                    if let Err(e) = Engine::<D>::commit_text(&emitter, ibus_text(text)).await {
                        tracing::warn!("CommitText failed: {e}");
                    }
                }
                true
            }
        }
    }

    async fn focus_in(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        tracing::debug!("D-Bus FocusIn (no client id)");
        self.do_focus_in(None, &emitter).await;
    }

    async fn focus_in_id(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        object_path: &str,
        client: &str,
    ) {
        tracing::debug!(object_path, client, "D-Bus FocusInId");
        let app_id = if client.is_empty() { None } else { Some(client.to_ascii_lowercase()) };
        self.do_focus_in(app_id, &emitter).await;
    }

    async fn focus_out(&self) {
        self.do_focus_out().await;
    }

    async fn focus_out_id(&self, _object_path: &str) {
        self.do_focus_out().await;
    }

    async fn reset(&self) {
        tracing::debug!("Reset → full_reset");
        self.state.lock().await.daemon.full_reset();
    }

    async fn enable(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        tracing::debug!("IBus Enable");
        // Prime the client's surrounding-text machinery on enable (ibusengine.c
        // notes the client requests the initial surrounding text here).
        if let Err(e) = Engine::<D>::require_surrounding_text(&emitter).await {
            tracing::warn!("RequireSurroundingText (enable) failed: {e}");
        }
    }

    async fn disable(&self) {
        tracing::debug!("IBus Disable");
        self.state.lock().await.daemon.deactivate_ibus();
    }

    async fn set_capabilities(&self, caps: u32) {
        let has_surrounding = caps & IBUS_CAP_SURROUNDING_TEXT != 0;
        tracing::debug!(caps, has_surrounding, "SetCapabilities");
        let mut s = self.state.lock().await;
        s.has_surrounding = has_surrounding;
        // Remember an explicit "no surrounding" declaration so a later stray
        // SetSurroundingText frame can't override it (Google Docs/Firefox).
        s.caps_cleared_surrounding = !has_surrounding;
        // FocusIn may have latched ForwardKey from a transient caps=9; upgrade
        // now that we know surrounding text is available.
        s.daemon.update_method(method_for_capability(has_surrounding));
    }

    async fn set_cursor_location(&self, _x: i32, _y: i32, _w: i32, _h: i32) {}

    async fn set_surrounding_text(&self, text: Value<'_>, cursor_pos: u32, anchor_pos: u32) {
        let text_str = extract_ibus_text_string(&text);
        let Some(text_str) = text_str else {
            tracing::debug!("SetSurroundingText: could not extract string");
            return;
        };
        tracing::debug!(
            text = %text_str,
            cursor_pos,
            anchor_pos,
            "SetSurroundingText"
        );
        let mut s = self.state.lock().await;
        // Receiving surrounding text is PROOF the client supports it —
        // regardless of a transient caps=9 that may have latched ForwardKey
        // during a focus flap (gedit's defocused capability ends on caps=9, so
        // a re-focus can race activate_ibus into picking ForwardKey and get
        // stuck). Trust the evidence over the racy caps flag: flip
        // has_surrounding sticky-true and upgrade the method (no-op if already
        // SurroundingText). Mirrors the wayland "late tier upgrade on first
        // surrounding_text".
        if should_upgrade_on_surrounding(s.has_surrounding, s.caps_cleared_surrounding) {
            tracing::info!(
                "SetSurroundingText while has_surrounding=false → upgrade method (caps race)"
            );
            s.has_surrounding = true;
            s.daemon.update_method(BackspaceMethod::SurroundingText);
        } else if s.caps_cleared_surrounding {
            tracing::debug!(
                "SetSurroundingText ignored for tier: client explicitly cleared surrounding caps \
                 (Docs-style frame; delete_surrounding_text would be dropped)"
            );
        }
        s.daemon.observe_surrounding(&text_str, cursor_pos, anchor_pos);
    }

    async fn property_activate(&self, _name: &str, _state: u32) {}

    // ── Signals ──────────────────────────────────────────────────────────────

    #[zbus(signal)]
    async fn commit_text(emitter: &SignalEmitter<'_>, text: Value<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn forward_key_event(
        emitter: &SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn delete_surrounding_text(
        emitter: &SignalEmitter<'_>,
        offset: i32,
        n_chars: u32,
    ) -> zbus::Result<()>;

    /// Tells the client (GTK/Qt im-context) that this engine consumes
    /// surrounding text. Without this, GTK never primes the surrounding-text
    /// path and silently drops every `DeleteSurroundingText` we emit, so
    /// commit-on-the-fly corrections append instead of replacing. ibus's own
    /// `ibus_engine_get_surrounding_text()` emits this (ibusengine.c:2214); we
    /// emit it on focus-in/enable so the client starts sending SetSurroundingText.
    #[zbus(signal)]
    async fn require_surrounding_text(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn update_preedit_text(
        emitter: &SignalEmitter<'_>,
        text: Value<'_>,
        cursor_pos: u32,
        visible: bool,
        mode: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn show_preedit_text(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn hide_preedit_text(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl<D: IbusHandler + Send + 'static> Engine<D> {
    async fn do_focus_in(&self, app_id: Option<String>, emitter: &SignalEmitter<'_>) {
        {
            let mut s = self.state.lock().await;
            s.client_app_id = app_id;
            // New focus session: forget any prior caps=9 declaration so a stale
            // defocus artifact (gedit) doesn't block this client's legitimate
            // SetSurroundingText upgrade. The client re-declares caps after
            // FocusIn; an in-session caps=9 (Docs) re-arms the guard before its
            // surrounding frame arrives.
            s.caps_cleared_surrounding = false;
            let method = method_for_capability(s.has_surrounding);
            let chars_for_delete = s.chars_for_delete();
            tracing::debug!(
                app_id = ?s.client_app_id,
                ?method,
                chars_for_delete,
                "FocusIn → activate_ibus"
            );
            s.daemon.activate_ibus(method, chars_for_delete);
            // lock dropped before the .await below
        }
        // Re-assert that we consume surrounding text so the client (re)starts
        // pushing SetSurroundingText and honors our DeleteSurroundingText.
        if let Err(e) = Engine::<D>::require_surrounding_text(emitter).await {
            tracing::warn!("RequireSurroundingText (focus_in) failed: {e}");
        }
    }

    async fn do_focus_out(&self) {
        let mut s = self.state.lock().await;
        tracing::debug!(app_id = ?s.client_app_id, "FocusOut → deactivate_ibus");
        s.client_app_id = None;
        s.daemon.deactivate_ibus();
    }
}

fn method_for_capability(has_surrounding: bool) -> BackspaceMethod {
    if has_surrounding {
        BackspaceMethod::SurroundingText
    } else {
        BackspaceMethod::ForwardKey
    }
}

/// Whether an incoming SetSurroundingText frame should upgrade the tier back to
/// SurroundingText. Only when we currently believe there's no surrounding AND
/// the client hasn't *explicitly* cleared the capability this session. The
/// explicit-clear case is Google Docs/Firefox: it declares `caps=9` then still
/// emits a frame for existing document text while dropping
/// `delete_surrounding_text`, so trusting the frame re-breaks composition.
fn should_upgrade_on_surrounding(has_surrounding: bool, caps_cleared_surrounding: bool) -> bool {
    !has_surrounding && !caps_cleared_surrounding
}

/// Extract the plain string from an IBusText variant value.
/// IBusText is `(sa{sv}sv)` — the string is the 3rd field (index 2).
fn extract_ibus_text_string(v: &Value<'_>) -> Option<String> {
    // Unwrap outer variant if present
    let inner = match v {
        Value::Value(boxed) => boxed.as_ref(),
        other => other,
    };
    let Value::Structure(s) = inner else { return None };
    let fields = s.fields();
    // Field 0: type name "IBusText"; field 1: attachments; field 2: the string
    if fields.len() < 3 {
        return None;
    }
    match &fields[2] {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Run the ibus adapter: connect to ibus-daemon, register factory + engine,
/// and drive the async message loop until disconnected.
pub async fn run<D: IbusHandler + Send + 'static>(
    daemon: D,
    enabled: Arc<AtomicBool>,
    chars_delete_apps: Vec<String>,
) -> Result<()> {
    let addr = crate::bus::resolve_ibus_address()?;
    tracing::info!(%addr, "connecting to ibus-daemon");

    let conn = zbus::conn::Builder::address(addr.as_str())?
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to ibus-daemon: {e}"))?;

    let state = Arc::new(tokio::sync::Mutex::new(
        EngineState::new(daemon, enabled, chars_delete_apps),
    ));

    conn.object_server()
        .at("/org/freedesktop/IBus/Factory", Factory::new(state))
        .await?;

    conn.request_name("org.freedesktop.IBus.Daklak")
        .await
        .map_err(|e| anyhow::anyhow!("request_name org.freedesktop.IBus.Daklak: {e}"))?;

    tracing::info!("registered as org.freedesktop.IBus.Daklak — awaiting CreateEngine");

    // Keep the connection alive and the object server dispatching. zbus's tokio
    // integration services incoming method calls (CreateEngine, ProcessKeyEvent,
    // …) on background tasks for as long as `conn` is held and this future is
    // pending. `monitor_activity()` returns an EventListener that fires on the
    // *first* activity event — awaiting it once returns immediately after
    // register, dropping `conn` before any CreateEngine arrives. ibus owns the
    // engine lifecycle and SIGTERMs the process on disable/shutdown, so park
    // here forever while holding `conn`.
    let _conn = conn;
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_selection_prefers_surrounding() {
        assert_eq!(
            method_for_capability(true),
            BackspaceMethod::SurroundingText
        );
    }

    #[test]
    fn method_selection_falls_back_to_forward_key() {
        assert_eq!(method_for_capability(false), BackspaceMethod::ForwardKey);
    }

    #[test]
    fn surrounding_frame_upgrades_only_without_explicit_caps_clear() {
        // Stale focus-flap caps=9 (not explicitly cleared this session): a
        // surrounding frame proves the client is alive → upgrade. (gedit)
        assert!(should_upgrade_on_surrounding(false, false));

        // Client explicitly declared caps=9 this session (Google Docs/Firefox):
        // the frame is for existing doc text but deletes are dropped → DON'T
        // upgrade; stay on ForwardKey.
        assert!(!should_upgrade_on_surrounding(false, true));

        // Already on surrounding → nothing to upgrade, regardless of the flag.
        assert!(!should_upgrade_on_surrounding(true, false));
        assert!(!should_upgrade_on_surrounding(true, true));
    }
}
