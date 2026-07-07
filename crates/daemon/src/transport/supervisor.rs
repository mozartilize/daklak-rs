use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, watch};

use crate::backend::{BackendTarget, InputBackend};
use crate::config::Config;
use crate::control::{CmdKind, Command, ControlReply};
use crate::handler::Daemon;

/// What to do after the current transport stops.
enum PendingAction {
    SwitchTo(InputBackend),
    Quit,
}

/// Transport supervisor. Owns the active transport and dispatches control
/// commands (toggle, enable, disable, status, backend switch, quit).
///
/// Runs ALL transports inline (no `tokio::spawn`) to avoid Send requirements
/// from Wayland / evdev types. Uses `tokio::select!` to multiplex the command
/// channel with the transport's own event loop.
pub struct Supervisor {
    config: Config,
    enabled: Arc<AtomicBool>,
    backend_tx: watch::Sender<InputBackend>,
    state_tx: watch::Sender<bool>,
    config_change_rx: watch::Receiver<crate::control::ConfigChange>,
    /// Reference to the IBus daemon's suspend flag. Captured before the daemon
    /// is consumed by `IbusRuntime`. Kept alive across evdev switches so the
    /// supervisor can gate IBus passthrough without dropping the IBus
    /// connection (which would lose input-context binding).
    #[cfg_attr(not(all(feature = "ibus", feature = "evdev_grab")), allow(dead_code))]
    ibus_suspend: Option<Arc<AtomicBool>>,
    /// Set by handle_cmd when the transport needs to stop and switch.
    pending_action: Option<PendingAction>,
}

fn backend_supported_at_build(backend: InputBackend) -> Result<()> {
    match backend {
        InputBackend::Ibus => {
            #[cfg(feature = "ibus")]
            return Ok(());
            #[cfg(not(feature = "ibus"))]
            return Err(anyhow!("ibus support was not compiled in"));
        }
        InputBackend::Evdev => {
            #[cfg(feature = "evdev_grab")]
            return Ok(());
            #[cfg(not(feature = "evdev_grab"))]
            return Err(anyhow!("evdev_grab support was not compiled in"));
        }
        InputBackend::Wayland | InputBackend::Auto => Ok(()),
    }
}

impl Supervisor {
    pub fn new(
        config: Config,
        enabled: Arc<AtomicBool>,
        state_tx: watch::Sender<bool>,
        backend_tx: watch::Sender<InputBackend>,
        config_change_rx: watch::Receiver<crate::control::ConfigChange>,
    ) -> Self {
        Self {
            config,
            enabled,
            backend_tx,
            state_tx,
            config_change_rx,
            ibus_suspend: None,
            pending_action: None,
        }
    }

    pub async fn run(mut self, mut rx: mpsc::Receiver<Command>) -> Result<()> {
        // Recover from a prior run that died with evdev hooks applied: a stale
        // rollback marker on disk means its cleanup hooks never ran. Replay
        // them once at startup before we (possibly) apply hooks again.
        #[cfg(feature = "evdev_grab")]
        {
            use crate::evdev_hooks::{self, ProcessHookRunner};
            if let Err(e) = evdev_hooks::recover_stale_rollback(&self.config, &ProcessHookRunner) {
                tracing::warn!("evdev stale rollback recovery failed: {e}");
            }
        }

        let mut current = InputBackend::startup_from_config(&self.config);
        if current == InputBackend::Auto {
            return Err(anyhow!(
                "no input backend enabled; set enable_wayland=true or enable_evdev_grab=true"
            ));
        }

        // If a native backend starts and evdev is enabled, switch to evdev.
        // For ibus, evdev layers on top (ibus connection stays alive).
        // For wayland, evdev replaces wayland (wayland reconnects on switch
        // back to native).
        if self.config.enable_evdev_grab && current != InputBackend::Evdev {
            self.pending_action = Some(PendingAction::SwitchTo(InputBackend::Evdev));
        }

        loop {
            let result = self.run_single(current, &mut rx).await;

            match self.pending_action.take() {
                Some(PendingAction::SwitchTo(next)) => {
                    tracing::info!(from = %current, to = %next, "backend switch");
                    current = next;
                    continue;
                }
                Some(PendingAction::Quit) => {
                    tracing::info!("quit requested via control");
                    std::process::exit(0);
                }
                None => {
                    // Evdev failed (hooks, preflight, grab, …): fall back to
                    // the native backend instead of crashing the daemon.
                    if current == InputBackend::Evdev {
                        if let Err(ref e) = result {
                            let native = InputBackend::native_from_config(&self.config);
                            if native != InputBackend::Auto {
                                tracing::warn!(%e, to = %native, "evdev failed — falling back to native backend");
                                current = native;
                                continue;
                            }
                        }
                    }
                    return result;
                }
            }
        }
    }

    fn resolve_target(&self, requested: BackendTarget) -> Result<InputBackend> {
        let target = match requested {
            BackendTarget::Native => InputBackend::native_from_config(&self.config),
            BackendTarget::Evdev => InputBackend::Evdev,
            BackendTarget::Toggle => {
                if self.current_backend() == InputBackend::Evdev {
                    InputBackend::native_from_config(&self.config)
                } else {
                    InputBackend::Evdev
                }
            }
        };
        if target == InputBackend::Auto {
            return Err(anyhow!("no configured native backend available"));
        }
        backend_supported_at_build(target)?;
        Ok(target)
    }

    fn current_backend(&self) -> InputBackend {
        *self.backend_tx.borrow()
    }

    // ── single-backend runners ───────────────────────────────────────────────

    async fn run_single(
        &mut self,
        backend: InputBackend,
        rx: &mut mpsc::Receiver<Command>,
    ) -> Result<()> {
        match backend {
            InputBackend::Wayland => self.run_wayland_loop(rx).await,
            InputBackend::Ibus => {
                #[cfg(feature = "ibus")]
                {
                    return self.run_ibus_loop(rx).await;
                }
                #[cfg(not(feature = "ibus"))]
                return Err(anyhow!("ibus support was not compiled in"));
            }
            InputBackend::Evdev => self.run_evdev_loop(rx).await,
            InputBackend::Auto => Err(anyhow!("auto must be resolved before run")),
        }
    }

    async fn run_wayland_loop(&mut self, rx: &mut mpsc::Receiver<Command>) -> Result<()> {
        let daemon = Daemon::new(
            self.config.clone(),
            self.enabled.clone(),
            self.config_change_rx.clone(),
        );
        let mut wayland = viet_ime_wayland_adapter::connect(daemon)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let transport =
            crate::main_loop::core_loop_with_wayland_shutdown(&mut wayland, shutdown_rx);
        tokio::pin!(transport);
        // Publish the active backend so `daklak backend` and the tray reflect
        // the switch. Without this, a prior evdev announcement stays latched and
        // the reported backend is wrong after switching back to native.
        let _ = self.backend_tx.send(InputBackend::Wayland);

        loop {
            if self.pending_action.is_some() {
                let _ = shutdown_tx.send(true);
                let _ = (&mut transport).await;
                return Ok(());
            }
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let reply = self.handle_cmd(cmd.kind);
                    let _ = cmd.resp.send(reply);
                }
                result = &mut transport => {
                    return result;
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "ibus")]
    async fn run_ibus_loop(&mut self, rx: &mut mpsc::Receiver<Command>) -> Result<()> {
        let daemon = Daemon::new(
            self.config.clone(),
            self.enabled.clone(),
            self.config_change_rx.clone(),
        );
        self.ibus_suspend = Some(daemon.suspend_flag());
        let runtime =
            viet_ime_ibus_adapter::IbusAdapter::connect(daemon, self.enabled.clone()).await?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let transport = runtime.run_until_shutdown(shutdown_rx);
        tokio::pin!(transport);
        let _ = self.backend_tx.send(InputBackend::Ibus);

        loop {
            if let Some(action) = self.pending_action.take() {
                match action {
                    // Layer the evdev grab ON TOP of the live IBus connection
                    // instead of dropping it: the engine keeps its input-context
                    // binding and only goes to passthrough while the grab owns
                    // the keyboard. Rebuilding the connection would not rebind
                    // the context, so keys would stop routing on the way back.
                    PendingAction::SwitchTo(InputBackend::Evdev) => {
                        self.run_evdev_in_ibus_slot(rx).await;
                        match self.pending_action.take() {
                            // Back to native IBus (explicit switch, evdev's own
                            // stream end, or emergency escape): resume the SAME
                            // live connection by looping again on `transport`.
                            None | Some(PendingAction::SwitchTo(InputBackend::Ibus)) => {
                                let _ = self.backend_tx.send(InputBackend::Ibus);
                                continue;
                            }
                            // A different backend or quit: tear the IBus
                            // connection down and let the outer loop take over.
                            Some(other) => {
                                self.pending_action = Some(other);
                                let _ = shutdown_tx.send(true);
                                return Ok(());
                            }
                        }
                    }
                    // Switch to a different native backend, or quit: drop the
                    // connection and hand control back to the outer loop.
                    other => {
                        self.pending_action = Some(other);
                        let _ = shutdown_tx.send(true);
                        return Ok(());
                    }
                }
            }
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let reply = self.handle_cmd(cmd.kind);
                    let _ = cmd.resp.send(reply);
                }
                _result = &mut transport => {
                    tracing::info!("ibus connection closed");
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    #[cfg(all(feature = "ibus", feature = "evdev_grab"))]
    async fn run_evdev_in_ibus_slot(&mut self, rx: &mut mpsc::Receiver<Command>) {
        use crate::evdev_hooks::{self, ProcessHookRunner};

        if let Err(e) = crate::evdev_preflight::check_or_notify() {
            tracing::warn!(%e, "evdev preflight failed");
            return;
        }

        let hooks = match evdev_hooks::resolve_hooks(&self.config) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(%e, "evdev hooks resolve failed");
                return;
            }
        };
        let mut adapter = match viet_ime_evdev_adapter::EvdevAdapter::prepare() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(%e, "evdev adapter prepare failed");
                return;
            }
        };

        if let Err(e) = adapter.grab_keyboards() {
            tracing::warn!(%e, "evdev grab failed");
            return;
        }

        let setup_handle = tokio::task::spawn_blocking(move || {
            let (applied, outcomes) = evdev_hooks::run_setup_hooks(&hooks, &ProcessHookRunner)?;
            evdev_hooks::write_rollback_marker(&applied)?;
            Ok::<_, anyhow::Error>((applied, outcomes))
        });

        // Flip IBus to passthrough: the still-connected engine forwards keys
        // raw (no compose, no output) while the evdev grab owns the keyboard,
        // so the two paths never both act on a keystroke.
        if let Some(ref suspend) = self.ibus_suspend {
            suspend.store(true, Ordering::Release);
        }
        let _ = self.backend_tx.send(InputBackend::Evdev);
        tracing::info!("evdev grab backend active; use `daklak backend native` to release grabs");

        let mut daemon = Daemon::new(
            self.config.clone(),
            self.enabled.clone(),
            self.config_change_rx.clone(),
        );
        daemon.activate_evdev();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let transport = async {
            let run = adapter.run_until_shutdown(&mut daemon, shutdown_rx);
            tokio::pin!(run);
            let mut setup_handle = setup_handle;
            let mut setup_done = false;
            let mut applied_for_cleanup = None;

            let result = loop {
                tokio::select! {
                    result = &mut run => break result,
                    setup = &mut setup_handle, if !setup_done => {
                        setup_done = true;
                        let (applied, outcomes) = setup
                            .map_err(|e| anyhow!("evdev hooks setup spawn failed: {e}"))??;
                        tracing::info!(?outcomes, "evdev keymap hooks processed");
                        applied_for_cleanup = Some(applied);
                    }
                }
            };

            if !setup_done {
                let (applied, outcomes) = setup_handle
                    .await
                    .map_err(|e| anyhow!("evdev hooks setup spawn failed: {e}"))??;
                tracing::info!(?outcomes, "evdev keymap hooks processed");
                applied_for_cleanup = Some(applied);
            }

            let cleanup = if let Some(applied_for_cleanup) = applied_for_cleanup {
                tokio::task::spawn_blocking(move || {
                    let cleanup = evdev_hooks::run_cleanup_hooks(&applied_for_cleanup, &ProcessHookRunner);
                    if cleanup.is_ok() {
                        evdev_hooks::clear_rollback_marker();
                    }
                    cleanup
                })
                .await
                .map_err(|e| anyhow!("evdev hooks cleanup spawn failed: {e}"))?
            } else {
                Ok(())
            };
            result.and(cleanup)
        };
        tokio::pin!(transport);

        loop {
            if self.pending_action.is_some() {
                let _ = shutdown_tx.send(true);
                let _ = (&mut transport).await;
                break;
            }
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let reply = self.handle_cmd(cmd.kind);
                    let _ = cmd.resp.send(reply);
                }
                _result = &mut transport => {
                    break;
                }
            }
        }

        // Evdev stopped or switching away. Take the engine out of passthrough
        // so the still-bound IBus connection resumes composing when the caller
        // loops back to it. Backend status is updated by the IBus loop on
        // resume (or by the outer loop when switching to a different backend).
        if let Some(ref suspend) = self.ibus_suspend {
            suspend.store(false, Ordering::Release);
        }
    }

    #[cfg(all(feature = "ibus", not(feature = "evdev_grab")))]
    async fn run_evdev_in_ibus_slot(&mut self, _rx: &mut mpsc::Receiver<Command>) {
        tracing::error!("ibus→evdev switch requested but evdev_grab not compiled");
    }

    #[cfg(feature = "evdev_grab")]
    async fn run_evdev_loop(&mut self, rx: &mut mpsc::Receiver<Command>) -> Result<()> {
        use crate::evdev_hooks::{self, ProcessHookRunner};

        crate::evdev_preflight::check_or_notify()?;
        let mut adapter = viet_ime_evdev_adapter::EvdevAdapter::prepare()?;
        let hooks = evdev_hooks::resolve_hooks(&self.config)?;
        adapter.grab_keyboards()?;

        let setup_handle = tokio::task::spawn_blocking(move || {
            let (applied, outcomes) = evdev_hooks::run_setup_hooks(&hooks, &ProcessHookRunner)?;
            evdev_hooks::write_rollback_marker(&applied)?;
            Ok::<_, anyhow::Error>((applied, outcomes))
        });

        let mut daemon = Daemon::new(
            self.config.clone(),
            self.enabled.clone(),
            self.config_change_rx.clone(),
        );
        daemon.activate_evdev();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let transport = async {
            let run = adapter.run_until_shutdown(&mut daemon, shutdown_rx);
            tokio::pin!(run);
            let mut setup_handle = setup_handle;
            let mut setup_done = false;
            let mut applied_for_cleanup = None;

            let result = loop {
                tokio::select! {
                    result = &mut run => break result,
                    setup = &mut setup_handle, if !setup_done => {
                        setup_done = true;
                        let (applied, outcomes) = setup
                            .map_err(|e| anyhow!("evdev hooks setup spawn failed: {e}"))??;
                        tracing::info!(?outcomes, "evdev keymap hooks processed");
                        applied_for_cleanup = Some(applied);
                    }
                }
            };

            if !setup_done {
                let (applied, outcomes) = setup_handle
                    .await
                    .map_err(|e| anyhow!("evdev hooks setup spawn failed: {e}"))??;
                tracing::info!(?outcomes, "evdev keymap hooks processed");
                applied_for_cleanup = Some(applied);
            }

            let cleanup = if let Some(applied_for_cleanup) = applied_for_cleanup {
                tokio::task::spawn_blocking(move || {
                    let cleanup = evdev_hooks::run_cleanup_hooks(&applied_for_cleanup, &ProcessHookRunner);
                    if cleanup.is_ok() {
                        evdev_hooks::clear_rollback_marker();
                    }
                    cleanup
                })
                .await
                .map_err(|e| anyhow!("evdev hooks cleanup spawn failed: {e}"))?
            } else {
                Ok(())
            };
            result.and(cleanup)
        };
        tokio::pin!(transport);

        let _ = self.backend_tx.send(InputBackend::Evdev);
        tracing::info!("evdev grab backend active; use `daklak backend native` to release grabs");

        loop {
            if self.pending_action.is_some() {
                let _ = shutdown_tx.send(true);
                let _ = (&mut transport).await;
                return Ok(());
            }
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break Ok(()) };
                    let reply = self.handle_cmd(cmd.kind);
                    let _ = cmd.resp.send(reply);
                }
                result = &mut transport => {
                    return result;
                }
            }
        }
    }

    #[cfg(not(feature = "evdev_grab"))]
    async fn run_evdev_loop(&mut self, _rx: &mut mpsc::Receiver<Command>) -> Result<()> {
        Err(anyhow!(
            "evdev_grab mode requested, but the daemon was built without the evdev_grab feature"
        ))
    }

    /// Dispatch a control command and return a reply.
    /// Sets `self.pending_action` if the transport should stop and switch.
    fn handle_cmd(&mut self, kind: CmdKind) -> ControlReply {
        match kind {
            CmdKind::Toggle => {
                let v = !self.enabled.load(Ordering::Acquire);
                self.enabled.store(v, Ordering::Release);
                let _ = self.state_tx.send(v);
                ControlReply::Enabled(v)
            }
            CmdKind::Enable => {
                self.enabled.store(true, Ordering::Release);
                let _ = self.state_tx.send(true);
                ControlReply::Enabled(true)
            }
            CmdKind::Disable => {
                self.enabled.store(false, Ordering::Release);
                let _ = self.state_tx.send(false);
                ControlReply::Enabled(false)
            }
            CmdKind::Status => ControlReply::Enabled(self.enabled.load(Ordering::Acquire)),
            CmdKind::BackendStatus => ControlReply::Backend(self.current_backend()),
            CmdKind::SetBackend(target) => {
                let target = match self.resolve_target(target) {
                    Ok(t) => t,
                    Err(e) => {
                        return ControlReply::Error(format!("backend {target} unavailable: {e}"))
                    }
                };
                if self.current_backend() == target {
                    return ControlReply::Ok(format!("backend {target}"));
                }
                #[cfg(feature = "evdev_grab")]
                if target == InputBackend::Evdev {
                    if let Err(e) = crate::evdev_preflight::check_or_notify() {
                        tracing::warn!(%e, "evdev preflight failed");
                        return ControlReply::Error(format!("backend evdev unavailable: {e}"));
                    }
                }
                tracing::info!(from = %self.current_backend(), to = %target, "IPC requested backend switch");
                self.pending_action = Some(PendingAction::SwitchTo(target));
                ControlReply::Ok(format!("backend {target}"))
            }
            CmdKind::Quit => {
                self.pending_action = Some(PendingAction::Quit);
                let cur = self.enabled.load(Ordering::Acquire);
                ControlReply::Enabled(cur)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_resolves_from_config_even_when_evdev_startup_is_enabled() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = true;
        let enabled = Arc::new(AtomicBool::new(true));
        let (state_tx, _) = watch::channel(true);
        let (backend_tx, _) = watch::channel(InputBackend::Auto);
        let (_cfg_tx, cfg_rx) = watch::channel(crate::control::ConfigChange::default());
        let s = Supervisor::new(cfg, enabled, state_tx, backend_tx, cfg_rx);
        assert_eq!(
            s.resolve_target(BackendTarget::Native).unwrap(),
            InputBackend::Wayland
        );
    }

    #[test]
    fn toggle_resolves_opposite_current_backend() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        let enabled = Arc::new(AtomicBool::new(true));
        let (state_tx, _) = watch::channel(true);
        let (backend_tx, _) = watch::channel(InputBackend::Wayland);
        let (_cfg_tx, cfg_rx) = watch::channel(crate::control::ConfigChange::default());
        let s = Supervisor::new(cfg.clone(), enabled.clone(), state_tx.clone(), backend_tx, cfg_rx.clone());
        assert_eq!(
            s.resolve_target(BackendTarget::Toggle).unwrap(),
            InputBackend::Evdev
        );

        let (backend_tx, _) = watch::channel(InputBackend::Evdev);
        let s = Supervisor::new(cfg, enabled, state_tx, backend_tx, cfg_rx);
        assert_eq!(
            s.resolve_target(BackendTarget::Toggle).unwrap(),
            InputBackend::Wayland
        );
    }
}
