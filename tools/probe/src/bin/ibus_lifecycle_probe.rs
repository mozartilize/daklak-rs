//! IBus lifecycle probe — answers "does ibus-daemon kill a spawned engine when
//! it drops its D-Bus connection, and can that engine re-register afterwards?"
//!
//! NOT production code. Ephemeral dev spike for evaluating in-process
//! native<->evdev backend switching (whether the IBus transport can be torn
//! down and rebuilt without the process being SIGTERM'd by ibus-daemon).
//!
//! It reuses daklak's REAL registration path from `viet-ime-ibus-adapter`
//! (`EngineState` + `Factory` + `resolve_ibus_address` + `request_name`) so
//! ibus-daemon treats it exactly like the production component — same code
//! path in `bus/component.c`. Only the well-known name and engine name differ
//! (`DaklakProbe` / `daklak-probe`) so it never clashes with a real daklak
//! install.
//!
//! The engine is a no-op passthrough: `process_key` always returns
//! `ForwardRaw`, so while the engine is selected, typing works normally — that
//! is the "engine is alive" signal for a human observer.
//!
//! Signals (see tools/probe/ibus-lifecycle-probe.sh):
//!   SIGUSR1 → drop the zbus connection      (simulate switch  → evdev grab)
//!   SIGUSR2 → reconnect + re-register        (simulate switch  → native ibus)
//!   SIGTERM → log + exit                     (detects ibus killing us)
//!   SIGINT  → log + exit
//!
//! CLI:
//!   --name <dbus-name>   Well-known name to request. Defaults to
//!                        `org.freedesktop.IBus.DaklakProbe`. Pass
//!                        `org.freedesktop.IBus.Daklak` to impersonate the real
//!                        daklak component when daklak is NOT installed (the
//!                        component XML must use the same name — the driver
//!                        script handles that via its impersonation profile).
//!
//! All events are appended to `$XDG_RUNTIME_DIR/daklak-ibus-probe.log` because
//! ibus spawns engines with stdout/stderr redirected to /dev/null.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use zbus::Connection;

use viet_ime_edit_strategy::{BackspaceMethod, KeyDecision, ModifierState};
use viet_ime_ibus_adapter::bus::resolve_ibus_address;
use viet_ime_ibus_adapter::engine::{EngineState, Factory, IbusHandler};
use viet_ime_ibus_adapter::sink::IbusSink;

/// Distinct name so the probe never collides with a real daklak install while
/// still exercising the identical ibus-daemon spawn/lifecycle code path.
/// Overridable with `--name` to impersonate the real daklak component.
const DEFAULT_BUS_NAME: &str = "org.freedesktop.IBus.DaklakProbe";
const FACTORY_PATH: &str = "/org/freedesktop/IBus/Factory";

/// Parse `--name <dbus-name>` / `--name=<dbus-name>`; fall back to the default.
fn parse_name_arg() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--name" {
            if let Some(v) = args.next() {
                return v;
            }
        } else if let Some(v) = a.strip_prefix("--name=") {
            return v.to_string();
        }
    }
    DEFAULT_BUS_NAME.to_string()
}

/// No-op IBus engine: never consumes keys, so selecting it does not break
/// typing. Focus/enable transitions are logged to prove the engine activated.
struct ProbeHandler;

impl IbusHandler for ProbeHandler {
    fn process_key(&mut self, _evdev: u32, _ch: Option<char>) -> KeyDecision {
        KeyDecision::ForwardRaw
    }
    fn apply_with_sink(&mut self, _backspaces: usize, _commit: &str, _time: u32, _sink: &mut IbusSink) {}
    fn observe_surrounding(&mut self, _text: &str, _cursor: u32, _anchor: u32) {}
    fn set_modifiers(&mut self, _m: ModifierState) {}
    fn activate_ibus(&mut self, _method: BackspaceMethod) {
        plog("engine ACTIVATED (ibus enable/CreateEngine reached us)");
    }
    fn deactivate_ibus(&mut self) {
        plog("engine deactivated");
    }
    fn update_method(&mut self, _method: BackspaceMethod) {}
    fn full_reset(&mut self) {}
}

fn logfile_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("daklak-ibus-probe.log")
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append a timestamped, PID-tagged line to both stderr and the logfile.
fn plog(msg: &str) {
    use std::io::Write;
    let line = format!("[{}] pid={} {msg}\n", now_ts(), std::process::id());
    eprint!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logfile_path())
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn ppid() -> i32 {
    // SAFETY: getppid is always safe and never fails.
    unsafe { libc::getppid() }
}

/// Mirror of `viet_ime_ibus_adapter::engine::run`'s connect+register sequence,
/// minus the park-forever tail — so ibus-daemon sees an identical registration.
async fn register(
    state: Arc<Mutex<EngineState<ProbeHandler>>>,
    name: &str,
) -> Result<Connection> {
    let addr = resolve_ibus_address()?;
    let conn = zbus::conn::Builder::address(addr.as_str())?
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to ibus-daemon: {e}"))?;
    conn.object_server()
        .at(FACTORY_PATH, Factory::new(state))
        .await?;
    conn.request_name(name)
        .await
        .map_err(|e| anyhow::anyhow!("request_name {name}: {e}"))?;
    Ok(conn)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let name = parse_name_arg();
    plog(&format!(
        "=== ibus_lifecycle_probe START (name={name}, ppid={}, addr_env={:?}) ===",
        ppid(),
        std::env::var("IBUS_ADDRESS").ok().map(|s| !s.is_empty())
    ));

    let enabled = Arc::new(AtomicBool::new(true));
    let state = Arc::new(Mutex::new(EngineState::new(ProbeHandler, enabled)));

    // Hold the live connection here; dropping it disconnects from ibus-daemon.
    let mut conn: Option<Connection> = match register(state.clone(), &name).await {
        Ok(c) => {
            plog(&format!("registered {name} — awaiting CreateEngine"));
            Some(c)
        }
        Err(e) => {
            plog(&format!("initial register FAILED: {e}"));
            return Err(e);
        }
    };

    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut usr2 = signal(SignalKind::user_defined2())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut intr = signal(SignalKind::interrupt())?;

    plog("READY: USR1=drop conn (→evdev), USR2=reconnect (→native), TERM/INT=exit");

    loop {
        tokio::select! {
            _ = usr1.recv() => {
                if conn.take().is_some() {
                    plog("SIGUSR1: dropped zbus connection (simulate switch → evdev grab)");
                    plog("connection dropped; process STILL ALIVE — watching for an ibus SIGTERM…");
                } else {
                    plog("SIGUSR1: already disconnected — no-op");
                }
            }
            _ = usr2.recv() => {
                plog("SIGUSR2: reconnecting (simulate switch → native ibus)");
                match register(state.clone(), &name).await {
                    Ok(c) => {
                        conn = Some(c);
                        plog(&format!("re-registered {name} OK — switch-back path VIABLE"));
                    }
                    Err(e) => plog(&format!("re-register FAILED — switch-back path BROKEN: {e}")),
                }
            }
            _ = term.recv() => {
                plog("SIGTERM received — ibus-daemon (or user) killed the process; exiting");
                break;
            }
            _ = intr.recv() => {
                plog("SIGINT received — exiting");
                break;
            }
        }
    }

    plog("=== ibus_lifecycle_probe EXIT ===");
    Ok(())
}
