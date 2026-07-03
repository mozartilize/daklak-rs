//! IBus engine adapter for the daklak Vietnamese IME.
//!
//! Implements `org.freedesktop.IBus.Factory` + `org.freedesktop.IBus.Engine`
//! over zbus on the ibus-daemon private bus. Used as the GNOME transport
//! (Mutter does not expose zwp_input_method_v2/v1 — GNOME uses IBus D-Bus).
//!
//! Usage from daemon main:
//! ```ignore
//! IbusAdapter::run(daemon, enabled).await?;
//! ```

pub mod bus;
pub mod engine;
pub mod ibus_text;
pub mod keyval;
pub mod sink;

pub use engine::{run as run_ibus, IbusHandler, IbusRuntime};

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::Result;

pub struct IbusAdapter;

impl IbusAdapter {
    pub async fn run<D: IbusHandler + Send + 'static>(
        daemon: D,
        enabled: Arc<AtomicBool>,
    ) -> Result<()> {
        engine::run(daemon, enabled).await
    }

    pub async fn connect<D: IbusHandler + Send + 'static>(
        daemon: D,
        enabled: Arc<AtomicBool>,
    ) -> Result<IbusRuntime<D>> {
        engine::IbusRuntime::connect(daemon, enabled).await
    }
}
