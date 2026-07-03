use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub name: String,
    pub set_path: PathBuf,
    pub unset_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    Applied(String),
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedHooks {
    pub hooks: Vec<HookSpec>,
}

impl AppliedHooks {
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.hooks.iter().map(|h| h.name.clone()).collect()
    }
}

pub trait HookCommandRunner: Send + Sync {
    fn run(&self, path: &Path, phase: &str, name: &str) -> Result<i32>;
}

pub struct ProcessHookRunner;

impl HookCommandRunner for ProcessHookRunner {
    fn run(&self, path: &Path, phase: &str, name: &str) -> Result<i32> {
        let status = Command::new(path)
            .env("DAKLAK_HOOK_NAME", name)
            .env("DAKLAK_HOOK_PHASE", phase)
            .env("DAKLAK_UINPUT_NAME", "daklak")
            .env("DAKLAK_UINPUT_VENDOR", "56001")
            .env("DAKLAK_UINPUT_PRODUCT", "44033")
            .status()
            .map_err(|e| anyhow!("running hook {} for {phase}: {e}", path.display()))?;
        Ok(exit_code(status))
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

fn require_hook_pair(hook: &HookSpec) -> Result<()> {
    if !hook.set_path.is_file() {
        return Err(anyhow!("evdev hook {} missing set script {}", hook.name, hook.set_path.display()));
    }
    if !hook.unset_path.is_file() {
        return Err(anyhow!("evdev hook {} missing unset script {}", hook.name, hook.unset_path.display()));
    }
    Ok(())
}

fn run_setup_hooks_inner(
    hooks: &[HookSpec],
    runner: &dyn HookCommandRunner,
    check_files: bool,
) -> Result<(AppliedHooks, Vec<SetupOutcome>)> {
    if check_files {
        for hook in hooks {
            require_hook_pair(hook)?;
        }
    }

    let mut applied = Vec::new();
    let mut outcomes = Vec::new();

    for hook in hooks {
        match runner.run(&hook.set_path, "set", &hook.name)? {
            0 => {
                applied.push(hook.clone());
                outcomes.push(SetupOutcome::Applied(hook.name.clone()));
            }
            10 => {
                outcomes.push(SetupOutcome::Skipped(hook.name.clone()));
            }
            code => {
                let applied_hooks = AppliedHooks { hooks: applied };
                let _ = run_cleanup_hooks_inner(&applied_hooks, runner);
                return Err(anyhow!("evdev hook {} set failed with exit code {code}", hook.name));
            }
        }
    }

    Ok((AppliedHooks { hooks: applied }, outcomes))
}

pub fn run_setup_hooks(
    hooks: &[HookSpec],
    runner: &dyn HookCommandRunner,
) -> Result<(AppliedHooks, Vec<SetupOutcome>)> {
    run_setup_hooks_inner(hooks, runner, true)
}

fn run_cleanup_hooks_inner(applied: &AppliedHooks, runner: &dyn HookCommandRunner) -> Result<()> {
    let mut first_error = None;
    for hook in applied.hooks.iter().rev() {
        match runner.run(&hook.unset_path, "unset", &hook.name) {
            Ok(0) => {}
            Ok(code) => {
                let msg = format!("evdev hook {} unset failed with exit code {code}", hook.name);
                tracing::warn!(msg);
                if first_error.is_none() {
                    first_error = Some(anyhow!(msg));
                }
            }
            Err(e) => {
                tracing::warn!(%e, hook = %hook.name, "evdev hook unset failed");
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub fn run_cleanup_hooks(applied: &AppliedHooks, runner: &dyn HookCommandRunner) -> Result<()> {
    run_cleanup_hooks_inner(applied, runner)
}

pub fn validate_hook_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("evdev hook name must not be empty"));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(anyhow!(
            "evdev hook name {name:?} must contain only ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok(())
}

pub fn hook_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("daklak").join("hooks"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".config").join("daklak").join("hooks"))
}

pub fn resolve_hooks(config: &Config) -> Result<Vec<HookSpec>> {
    resolve_hooks_in(config, &hook_dir()?)
}

pub(crate) fn resolve_hooks_in(config: &Config, dir: &Path) -> Result<Vec<HookSpec>> {
    let mut out = Vec::new();
    for name in &config.evdev_grab_hooks {
        validate_hook_name(name)?;
        let set_path = dir.join(format!("{name}-set"));
        let unset_path = dir.join(format!("{name}-unset"));
        out.push(HookSpec {
            name: name.clone(),
            set_path,
            unset_path,
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize)]
struct RollbackMarker {
    applied_hooks: Vec<String>,
}

pub fn rollback_marker_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR not set"))?;
    Ok(PathBuf::from(runtime).join("daklak-evdev-hooks.json"))
}

pub fn write_rollback_marker(applied: &AppliedHooks) -> Result<()> {
    if applied.is_empty() {
        return Ok(());
    }
    let marker = RollbackMarker {
        applied_hooks: applied.names(),
    };
    let path = rollback_marker_path()?;
    let text = serde_json::to_string_pretty(&marker)?;
    std::fs::write(&path, text)
        .map_err(|e| anyhow!("write evdev rollback marker {}: {e}", path.display()))
}

pub fn clear_rollback_marker() {
    if let Ok(path) = rollback_marker_path() {
        let _ = std::fs::remove_file(path);
    }
}

pub fn recover_stale_rollback(config: &Config, runner: &dyn HookCommandRunner) -> Result<()> {
    let path = match rollback_marker_path() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    if !path.exists() {
        return Ok(());
    }

    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("read evdev rollback marker {}: {e}", path.display()))?;
    let marker: RollbackMarker = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse evdev rollback marker {}: {e}", path.display()))?;

    let all_hooks = resolve_hooks(config)?;
    let mut applied = Vec::new();
    for name in marker.applied_hooks {
        if let Some(hook) = all_hooks.iter().find(|h| h.name == name) {
            applied.push(hook.clone());
        } else {
            tracing::warn!(hook = %name, "evdev stale rollback hook no longer configured; skipping");
        }
    }

    let result = run_cleanup_hooks(&AppliedHooks { hooks: applied }, runner);
    if result.is_ok() {
        clear_rollback_marker();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_names_allow_safe_ascii() {
        assert!(validate_hook_name("sway").is_ok());
        assert!(validate_hook_name("kde-6").is_ok());
        assert!(validate_hook_name("x11_custom").is_ok());
    }

    #[test]
    fn hook_names_reject_paths_and_shell_syntax() {
        assert!(validate_hook_name("").is_err());
        assert!(validate_hook_name("../x11").is_err());
        assert!(validate_hook_name("foo/bar").is_err());
        assert!(validate_hook_name("foo;rm").is_err());
        assert!(validate_hook_name("foo bar").is_err());
    }

    #[test]
    fn resolves_hook_pairs_from_config_array() {
        let mut cfg = Config::default();
        cfg.evdev_grab_hooks = vec!["sway".into(), "kde".into()];
        let hooks = resolve_hooks_in(&cfg, Path::new("/tmp/daklak-hooks")).unwrap();
        assert_eq!(hooks[0].set_path, PathBuf::from("/tmp/daklak-hooks/sway-set"));
        assert_eq!(hooks[0].unset_path, PathBuf::from("/tmp/daklak-hooks/sway-unset"));
        assert_eq!(hooks[1].set_path, PathBuf::from("/tmp/daklak-hooks/kde-set"));
        assert_eq!(hooks[1].unset_path, PathBuf::from("/tmp/daklak-hooks/kde-unset"));
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeRunner {
        codes: HashMap<(String, String), i32>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeRunner {
        fn new(codes: &[((&str, &str), i32)]) -> Self {
            Self {
                codes: codes.iter().map(|((n, p), c)| ((n.to_string(), p.to_string()), *c)).collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HookCommandRunner for FakeRunner {
        fn run(&self, _path: &Path, phase: &str, name: &str) -> Result<i32> {
            self.calls.lock().unwrap().push((name.to_string(), phase.to_string()));
            Ok(*self.codes.get(&(name.to_string(), phase.to_string())).unwrap_or(&0))
        }
    }

    fn hook(name: &str) -> HookSpec {
        HookSpec {
            name: name.into(),
            set_path: PathBuf::from(format!("/tmp/{name}-set")),
            unset_path: PathBuf::from(format!("/tmp/{name}-unset")),
        }
    }

    fn run_setup_hooks_without_file_checks_for_test(
        hooks: &[HookSpec],
        runner: &dyn HookCommandRunner,
    ) -> Result<(AppliedHooks, Vec<SetupOutcome>)> {
        run_setup_hooks_inner(hooks, runner, false)
    }

    #[test]
    fn setup_records_applied_and_skipped_hooks() {
        let hooks = vec![hook("sway"), hook("kde")];
        let runner = FakeRunner::new(&[(("sway", "set"), 10), (("kde", "set"), 0)]);
        let (applied, outcomes) = run_setup_hooks_without_file_checks_for_test(&hooks, &runner).unwrap();
        assert_eq!(applied.names(), vec!["kde"]);
        assert_eq!(outcomes, vec![SetupOutcome::Skipped("sway".into()), SetupOutcome::Applied("kde".into())]);
    }

    #[test]
    fn failure_cleans_already_applied_hooks_in_reverse_order() {
        let hooks = vec![hook("first"), hook("second"), hook("third")];
        let runner = FakeRunner::new(&[
            (("first", "set"), 0),
            (("second", "set"), 0),
            (("third", "set"), 2),
            (("second", "unset"), 0),
            (("first", "unset"), 0),
        ]);
        let err = run_setup_hooks_without_file_checks_for_test(&hooks, &runner).unwrap_err();
        assert!(err.to_string().contains("third"));
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec![
                ("first".into(), "set".into()),
                ("second".into(), "set".into()),
                ("third".into(), "set".into()),
                ("second".into(), "unset".into()),
                ("first".into(), "unset".into()),
            ]
        );
    }
}
