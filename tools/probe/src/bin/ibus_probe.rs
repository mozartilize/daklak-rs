//! Phase 0 spike for plan8 (GNOME / IBus support).
//!
//! Goal: prove the `IBusText` GVariant serialization is correct and that a
//! hand-built `CommitText` lands in a real client. NOT production code — the
//! real engine lives in `crates/ibus-adapter` (Phase 2+).
//!
//! Two modes:
//!   * `ibus_probe selftest` — build the `IBusText` value, assert its signature
//!     is `(sa{sv}sv)`, round-trip it. No bus needed; runnable anywhere.
//!   * `ibus_probe`           — connect to ibus-daemon, register a factory +
//!     engine, and emit `CommitText("xin chào")` on every key press.
//!
//! Wire formats (verified against vendors/ibus/src):
//!   IBusText     = (s a{sv} s v)   → ("IBusText",     {}, <text>, <attrlist>)
//!   IBusAttrList = (s a{sv} av)    → ("IBusAttrList", {}, [])
//!   CommitText signal arg is a single `v` wrapping the IBusText struct.
//!   DeleteSurroundingText(i,u) and ForwardKeyEvent(u,u,u) are plain (unused here).

use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result};
use zbus::object_server::{ObjectServer, SignalEmitter};
use zvariant::{Array, Dict, ObjectPath, OwnedObjectPath, Signature, StructureBuilder, Value};

// ── IBusText / IBusAttrList builders ────────────────────────────────────────

/// Empty `a{sv}` attachments dict shared by every IBusSerializable header.
fn empty_attachments() -> Dict<'static, 'static> {
    Dict::new(&Signature::Str, &Signature::Variant)
}

/// `IBusAttrList` with no attributes → ("IBusAttrList", {}, []) :: (sa{sv}av)
///
/// Built with `append_field` (not the tuple `From`/`add_field`): `Value::new`
/// treats any `Value`-typed argument as dynamically-typed and re-wraps it in a
/// variant `v`, which would corrupt the `a{sv}`/`av` container fields.
fn ibus_attr_list() -> Value<'static> {
    let empty_attrs = Array::new(&Signature::Variant); // `av`, no elements
    let s = StructureBuilder::new()
        .append_field(Value::from("IBusAttrList"))
        .append_field(Value::Dict(empty_attachments()))
        .append_field(Value::Array(empty_attrs))
        .build()
        .expect("non-empty structure");
    Value::Structure(s)
}

/// `IBusText` → ("IBusText", {}, text, <variant attrlist>) :: (sa{sv}sv)
fn ibus_text(text: &str) -> Value<'static> {
    // Last field is a D-Bus variant `v`: box the attrlist value.
    let attrs = Value::Value(Box::new(ibus_attr_list()));
    let s = StructureBuilder::new()
        .append_field(Value::from("IBusText"))
        .append_field(Value::Dict(empty_attachments()))
        .append_field(Value::from(text.to_string()))
        .append_field(attrs)
        .build()
        .expect("non-empty structure");
    Value::Structure(s)
}

// ── D-Bus: Factory + Engine ──────────────────────────────────────────────────

struct Factory {
    next_id: AtomicI32,
}

#[zbus::interface(name = "org.freedesktop.IBus.Factory")]
impl Factory {
    /// ibus calls this when the daklak engine is activated. Create an engine
    /// object, export it, and return its path (`(o)`).
    async fn create_engine(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        engine_name: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/freedesktop/IBus/Engine/{id}");
        let op = ObjectPath::try_from(path.clone())
            .map_err(|e| zbus::fdo::Error::Failed(format!("bad path: {e}")))?
            .into();
        server.at(&op, Engine).await?;
        eprintln!("[factory] CreateEngine(name={engine_name}) -> {path}");
        Ok(op)
    }
}

struct Engine;

#[zbus::interface(name = "org.freedesktop.IBus.Engine")]
impl Engine {
    /// The hot path. ibus delivers X keysym + keycode + modifier state and
    /// blocks on the bool reply (swallowed vs passthrough). We emit the commit
    /// signal BEFORE returning so it's ordered ahead of the method reply.
    async fn process_key_event(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        keyval: u32,
        keycode: u32,
        state: u32,
    ) -> bool {
        const IBUS_RELEASE_MASK: u32 = 1 << 30;
        if state & IBUS_RELEASE_MASK != 0 {
            return false; // ignore key releases
        }
        eprintln!("[engine] ProcessKeyEvent keyval={keyval:#x} keycode={keycode} state={state:#x}");
        match Engine::commit_text(&emitter, ibus_text("xin chào")).await {
            Ok(()) => {
                eprintln!("[engine] CommitText emitted");
                true // swallow the key — we produced output for it
            }
            Err(e) => {
                eprintln!("[engine] CommitText FAILED: {e}");
                false
            }
        }
    }

    /// CommitText(v) — `v` wraps the serialized IBusText.
    #[zbus(signal)]
    async fn commit_text(emitter: &SignalEmitter<'_>, text: Value<'_>) -> zbus::Result<()>;

    // ── lifecycle no-ops so ibus's calls don't return UnknownMethod ──────────
    async fn focus_in(&self) {}
    async fn focus_in_id(&self, _object_path: &str, _client: &str) {}
    async fn focus_out(&self) {}
    async fn focus_out_id(&self, _object_path: &str) {}
    async fn reset(&self) {}
    async fn enable(&self) {
        eprintln!("[engine] Enable");
    }
    async fn disable(&self) {
        eprintln!("[engine] Disable");
    }
    async fn set_capabilities(&self, caps: u32) {
        eprintln!("[engine] SetCapabilities caps={caps:#x}");
    }
    async fn set_cursor_location(&self, _x: i32, _y: i32, _w: i32, _h: i32) {}
    async fn set_surrounding_text(&self, _text: Value<'_>, cursor: u32, anchor: u32) {
        eprintln!("[engine] SetSurroundingText cursor={cursor} anchor={anchor}");
    }
    async fn property_activate(&self, _name: &str, _state: u32) {}
}

// ── ibus bus address discovery ───────────────────────────────────────────────

fn resolve_ibus_address() -> Result<String> {
    if let Ok(addr) = std::env::var("IBUS_ADDRESS") {
        if !addr.is_empty() {
            return Ok(addr);
        }
    }
    // ~/.config/ibus/bus/<machine-id>-unix-<display>
    let home = std::env::var("HOME").context("HOME unset")?;
    let dir = std::path::Path::new(&home).join(".config/ibus/bus");

    // Prefer the file matching this session's display.
    let suffix = if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
        format!("unix-{wl}")
    } else if let Ok(x) = std::env::var("DISPLAY") {
        // ":0" / ":0.0" → "unix-0"
        let n = x.trim_start_matches(':');
        let n = n.split('.').next().unwrap_or("0");
        format!("unix-{n}")
    } else {
        "unix-0".to_string()
    };

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();

    let chosen = entries
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(&suffix))
                .unwrap_or(false)
        })
        .or_else(|| entries.first())
        .with_context(|| format!("no ibus address file in {}", dir.display()))?;

    eprintln!("[addr] using {}", chosen.display());
    let content = std::fs::read_to_string(chosen)?;
    for line in content.lines() {
        if let Some(addr) = line.strip_prefix("IBUS_ADDRESS=") {
            return Ok(addr.trim().to_string());
        }
    }
    anyhow::bail!("no IBUS_ADDRESS= line in {}", chosen.display())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn selftest() -> Result<()> {
    let v = ibus_text("xin chào");
    let sig = v.value_signature().to_string();
    println!("IBusText value_signature = {sig}");
    assert_eq!(sig, "(sa{sv}sv)", "IBusText signature mismatch");

    let al = ibus_attr_list();
    let al_sig = al.value_signature().to_string();
    println!("IBusAttrList value_signature = {al_sig}");
    assert_eq!(al_sig, "(sa{sv}av)", "IBusAttrList signature mismatch");

    println!("OK — IBusText/IBusAttrList GVariant shapes are correct.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("selftest") {
        return selftest();
    }

    let addr = resolve_ibus_address()?;
    eprintln!("[main] connecting to ibus at {addr}");
    let conn = zbus::conn::Builder::address(addr.as_str())?
        .build()
        .await
        .context("connecting to ibus-daemon")?;

    conn.object_server()
        .at(
            "/org/freedesktop/IBus/Factory",
            Factory {
                next_id: AtomicI32::new(0),
            },
        )
        .await?;
    conn.request_name("org.freedesktop.IBus.Daklak")
        .await
        .context("request_name org.freedesktop.IBus.Daklak")?;

    eprintln!("[main] registered as org.freedesktop.IBus.Daklak — waiting for CreateEngine.");
    eprintln!("[main] activate with:  ibus engine daklak");
    std::future::pending::<()>().await;
    Ok(())
}
