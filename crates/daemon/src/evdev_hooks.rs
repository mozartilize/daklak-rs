use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

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

    #[allow(dead_code)]
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
        #[cfg(unix)]
        let exec_path = validate_hook_script_path(path)?;
        #[cfg(not(unix))]
        let exec_path = path.to_path_buf();

        let mut command = Command::new(&exec_path);
        command
            .env("DAKLAK_HOOK_NAME", name)
            .env("DAKLAK_HOOK_PHASE", phase)
            .env(
                "DAKLAK_BIN",
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("daklak")),
            )
            .env("DAKLAK_UINPUT_NAME", "daklak")
            .env("DAKLAK_UINPUT_VENDOR", "56001")
            .env("DAKLAK_UINPUT_PRODUCT", "44033");
        if let Some(dir) = builtin_xkb_dir() {
            command.env("DAKLAK_XKB_DIR", dir);
        }
        let status = command
            .status()
            .map_err(|e| anyhow!("running hook {} for {phase}: {e}", exec_path.display()))?;
        Ok(exit_code(status))
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

fn hook_scripts_present(hook: &HookSpec) -> bool {
    hook.set_path.is_file() && hook.unset_path.is_file()
}

fn run_setup_hooks_inner(
    hooks: &[HookSpec],
    runner: &dyn HookCommandRunner,
    check_files: bool,
) -> Result<(AppliedHooks, Vec<SetupOutcome>)> {
    let mut applied = Vec::new();
    let mut outcomes = Vec::new();

    for hook in hooks {
        // A configured hook whose scripts aren't installed (e.g. a
        // multi-desktop `evdev_grab_hooks` list where only one desktop's
        // scripts exist) is skipped, not fatal. Aborting here propagates up
        // through the transport and terminates the daemon — fatal when daklak
        // runs as KWin's managed virtual-keyboard input method.
        if check_files && !hook_scripts_present(hook) {
            tracing::warn!(
                hook = %hook.name,
                set = %hook.set_path.display(),
                "evdev hook scripts missing — skipping (not installed / not applicable)"
            );
            outcomes.push(SetupOutcome::Skipped(hook.name.clone()));
            continue;
        }

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
                return Err(anyhow!(
                    "evdev hook {} set failed with exit code {code}",
                    hook.name
                ));
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
            Ok(10) => {}
            Ok(code) => {
                let msg = format!(
                    "evdev hook {} unset failed with exit code {code}",
                    hook.name
                );
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
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(anyhow!(
            "evdev hook name {name:?} must contain only ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
pub(crate) fn validate_hook_script_path(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow!("canonicalize hook {}: {e}", path.display()))?;
    let meta = std::fs::metadata(&canonical)
        .map_err(|e| anyhow!("stat hook {}: {e}", canonical.display()))?;
    if !meta.is_file() {
        return Err(anyhow!("hook {} is not a regular file", canonical.display()));
    }
    let mode = meta.mode();
    if mode & 0o022 != 0 {
        return Err(anyhow!(
            "hook {} is group/other writable; refusing to execute",
            canonical.display()
        ));
    }
    let owner = meta.uid();
    let euid = current_euid();
    if owner != euid && owner != 0 {
        return Err(anyhow!(
            "hook {} owner uid {} is neither daemon uid {} nor root",
            canonical.display(),
            owner,
            euid
        ));
    }
    Ok(canonical)
}

pub fn hook_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("daklak").join("hooks"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("daklak")
        .join("hooks"))
}

pub fn resolve_hooks(config: &Config) -> Result<Vec<HookSpec>> {
    resolve_hooks_in_dirs(config, &hook_search_dirs())
}

#[cfg(test)]
pub(crate) fn resolve_hooks_in(config: &Config, dir: &Path) -> Result<Vec<HookSpec>> {
    resolve_hooks_in_dirs(config, &[dir.to_path_buf()])
}

/// Ordered runtime search path for hook scripts. First match wins, so user
/// overrides in `~/.config/daklak/hooks` shadow the packaged copies:
/// 1. `$XDG_CONFIG_HOME/daklak/hooks` (user overrides)
/// 2. `~/.local/libexec/daklak/hooks` (per-user install)
/// 3. `/usr/libexec/daklak/hooks` (system install)
///
/// An explicit `DAKLAK_BUILTIN_HOOK_DIR` (env / Meson-embedded) is appended for
/// custom-prefix installs. Runtime discovery means a cargo-built binary (no
/// embedded path) still finds installed hooks.
fn hook_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = hook_dir() {
        dirs.push(d);
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(
            PathBuf::from(home)
                .join(".local")
                .join("libexec")
                .join("daklak")
                .join("hooks"),
        );
    }
    dirs.push(PathBuf::from("/usr/libexec/daklak/hooks"));
    if let Some(d) = builtin_hook_dir() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn builtin_hook_dir() -> Option<PathBuf> {
    resolve_builtin_dir(
        "DAKLAK_BUILTIN_HOOK_DIR",
        option_env!("DAKLAK_BUILTIN_HOOK_DIR"),
    )
}

/// Directory holding the generated xkb artifacts (`daklak.xkb`, `daklak_vn`,
/// `evdev`) that the hooks consume via `DAKLAK_XKB_DIR`. Resolved at runtime:
/// an explicit `DAKLAK_BUILTIN_XKB_DIR` (env / Meson-embedded) wins, else the
/// first existing standard location.
///
/// Search order mirrors the hook-script search in [`hook_search_dirs()`]:
/// 1. `$XDG_CONFIG_HOME/daklak/xkb` — user config override
/// 2. `~/.local/share/daklak/xkb`   — per-user install
/// 3. `/usr/share/daklak/xkb`       — system install
fn builtin_xkb_dir() -> Option<PathBuf> {
    if let Some(d) = resolve_builtin_dir(
        "DAKLAK_BUILTIN_XKB_DIR",
        option_env!("DAKLAK_BUILTIN_XKB_DIR"),
    ) {
        return Some(d);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(config).join("daklak").join("xkb"));
    } else if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".config")
                .join("daklak")
                .join("xkb"),
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("daklak")
                .join("xkb"),
        );
    }
    candidates.push(PathBuf::from("/usr/share/daklak/xkb"));
    candidates.into_iter().find(|d| d.is_dir())
}

/// Resolve a packaged-resource directory. A runtime env var takes precedence
/// over the compile-time (`option_env!`) value so a cargo-built binary — which
/// has no Meson-embedded path — can still find hooks / xkb artifacts installed
/// at a known location (e.g. `~/.local/libexec/daklak/hooks`).
fn resolve_builtin_dir(env_key: &str, compile_time: Option<&str>) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(env_key) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    compile_time.filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub(crate) fn resolve_hooks_in_dirs(config: &Config, dirs: &[PathBuf]) -> Result<Vec<HookSpec>> {
    let mut out = Vec::new();
    for name in &config.evdev_grab_hooks {
        validate_hook_name(name)?;
        let (set_path, unset_path) = resolve_hook_pair(name, dirs);
        out.push(HookSpec {
            name: name.clone(),
            set_path,
            unset_path,
        });
    }
    Ok(out)
}

fn resolve_hook_pair(name: &str, dirs: &[PathBuf]) -> (PathBuf, PathBuf) {
    // First directory holding a complete pair wins (user overrides first).
    for dir in dirs {
        let pair = hook_pair_paths(dir, name);
        if hook_pair_exists(&pair) {
            return pair;
        }
    }
    // None complete: prefer a directory that at least has one half (a partial
    // user override), otherwise the first search dir — either way the missing
    // check will skip it and log the path.
    for dir in dirs {
        let pair = hook_pair_paths(dir, name);
        if hook_pair_is_partial(&pair) {
            return pair;
        }
    }
    let first = dirs.first().cloned().unwrap_or_default();
    hook_pair_paths(&first, name)
}

fn hook_pair_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{name}-set")),
        dir.join(format!("{name}-unset")),
    )
}

fn hook_pair_exists(pair: &(PathBuf, PathBuf)) -> bool {
    pair.0.is_file() && pair.1.is_file()
}

fn hook_pair_is_partial(pair: &(PathBuf, PathBuf)) -> bool {
    pair.0.exists() || pair.1.exists()
}

#[derive(Debug, Serialize, Deserialize)]
struct RollbackMarker {
    version: u8,
    applied_hooks: Vec<RollbackHookRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RollbackHookRecord {
    name: String,
    set_path: PathBuf,
    unset_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RollbackMarkerFile {
    V2(RollbackMarker),
    Legacy { applied_hooks: Vec<String> },
}

fn rollback_marker_json(applied: &AppliedHooks) -> Result<String> {
    let marker = RollbackMarker {
        version: 2,
        applied_hooks: applied
            .hooks
            .iter()
            .map(|hook| RollbackHookRecord {
                name: hook.name.clone(),
                set_path: hook.set_path.clone(),
                unset_path: hook.unset_path.clone(),
            })
            .collect(),
    };
    serde_json::to_string_pretty(&marker).map_err(Into::into)
}

pub fn rollback_marker_path() -> Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| anyhow!("XDG_RUNTIME_DIR not set"))?;
    Ok(PathBuf::from(runtime).join("daklak-evdev-hooks.json"))
}

pub fn write_rollback_marker(applied: &AppliedHooks) -> Result<()> {
    if applied.is_empty() {
        return Ok(());
    }
    let path = rollback_marker_path()?;
    let text = rollback_marker_json(applied)?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&tmp)
        .map_err(|e| anyhow!("create evdev rollback marker {}: {e}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| anyhow!("write evdev rollback marker {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| anyhow!("sync evdev rollback marker {}: {e}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow!("rename evdev rollback marker {} -> {}: {e}", tmp.display(), path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| anyhow!("chmod evdev rollback marker {}: {e}", path.display()))?;
    Ok(())
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
    let marker: RollbackMarkerFile = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse evdev rollback marker {}: {e}", path.display()))?;

    let applied = match marker {
        RollbackMarkerFile::V2(marker) => marker
            .applied_hooks
            .into_iter()
            .map(|record| HookSpec {
                name: record.name,
                set_path: record.set_path,
                unset_path: record.unset_path,
            })
            .collect(),
        RollbackMarkerFile::Legacy { applied_hooks } => {
            let all_hooks = resolve_hooks(config)?;
            let mut applied = Vec::new();
            for name in applied_hooks {
                if let Some(hook) = all_hooks.iter().find(|h| h.name == name) {
                    applied.push(hook.clone());
                } else {
                    tracing::warn!(hook = %name, "evdev stale rollback hook no longer configured; skipping");
                }
            }
            applied
        }
    };

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
    fn builtin_dir_runtime_env_overrides_compile_time() {
        // Unique key so parallel tests don't collide on the process env.
        let key = "DAKLAK_TEST_BUILTIN_DIR_KEY";
        std::env::remove_var(key);

        // Runtime env wins over the compile-time (Meson-embedded) value — this
        // is how a cargo-built binary finds hooks installed at a known path.
        std::env::set_var(key, "/run/libexec/daklak/hooks");
        assert_eq!(
            resolve_builtin_dir(key, Some("/compile/libexec/daklak/hooks")),
            Some(PathBuf::from("/run/libexec/daklak/hooks"))
        );

        // Empty runtime env falls back to compile-time.
        std::env::set_var(key, "");
        assert_eq!(
            resolve_builtin_dir(key, Some("/compile/libexec/daklak/hooks")),
            Some(PathBuf::from("/compile/libexec/daklak/hooks"))
        );

        // No runtime env, no compile-time → None.
        std::env::remove_var(key);
        assert_eq!(resolve_builtin_dir(key, None), None);
    }

    #[test]
    fn hook_names_reject_paths_and_shell_syntax() {
        assert!(validate_hook_name("").is_err());
        assert!(validate_hook_name("../x11").is_err());
        assert!(validate_hook_name("foo/bar").is_err());
        assert!(validate_hook_name("foo;rm").is_err());
        assert!(validate_hook_name("foo bar").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hook_file_policy_rejects_group_or_other_writable_scripts() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "daklak-hook-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sway-set");
        std::fs::write(&path, "#!/bin/sh\nexit 10\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let err = validate_hook_script_path(&path).unwrap_err();
        assert!(err.to_string().contains("group/other writable"), "{err}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn hook_file_policy_accepts_owner_private_script() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "daklak-hook-policy-ok-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sway-set");
        std::fs::write(&path, "#!/bin/sh\nexit 10\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();

        let canonical = validate_hook_script_path(&path).unwrap();
        assert!(canonical.is_absolute());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_hook_pairs_from_config_array() {
        let mut cfg = Config::default();
        cfg.evdev_grab_hooks = vec!["sway".into(), "kde".into()];
        let hooks = resolve_hooks_in(&cfg, Path::new("/tmp/daklak-hooks")).unwrap();
        assert_eq!(
            hooks[0].set_path,
            PathBuf::from("/tmp/daklak-hooks/sway-set")
        );
        assert_eq!(
            hooks[0].unset_path,
            PathBuf::from("/tmp/daklak-hooks/sway-unset")
        );
        assert_eq!(
            hooks[1].set_path,
            PathBuf::from("/tmp/daklak-hooks/kde-set")
        );
        assert_eq!(
            hooks[1].unset_path,
            PathBuf::from("/tmp/daklak-hooks/kde-unset")
        );
    }

    #[test]
    fn resolves_user_hooks_before_packaged_hooks() {
        let root = std::env::temp_dir().join(format!(
            "daklak-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let user_dir = root.join("config-hooks");
        let packaged_dir = root.join("packaged-hooks");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&packaged_dir).unwrap();

        for path in [
            user_dir.join("sway-set"),
            user_dir.join("sway-unset"),
            packaged_dir.join("sway-set"),
            packaged_dir.join("sway-unset"),
            packaged_dir.join("kde-set"),
            packaged_dir.join("kde-unset"),
        ] {
            std::fs::write(path, "#!/bin/sh\nexit 10\n").unwrap();
        }

        let mut cfg = Config::default();
        cfg.evdev_grab_hooks = vec!["sway".into(), "kde".into()];
        let dirs = vec![user_dir.clone(), packaged_dir.clone()];
        let hooks = resolve_hooks_in_dirs(&cfg, &dirs).unwrap();

        assert_eq!(hooks[0].set_path, user_dir.join("sway-set"));
        assert_eq!(hooks[0].unset_path, user_dir.join("sway-unset"));
        assert_eq!(hooks[1].set_path, packaged_dir.join("kde-set"));
        assert_eq!(hooks[1].unset_path, packaged_dir.join("kde-unset"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_hook_from_first_dir_with_complete_pair_in_search_list() {
        let root = std::env::temp_dir().join(format!(
            "daklak-hooks3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let config = root.join("config");
        let local = root.join("local-libexec");
        let system = root.join("system-libexec");
        for d in [&config, &local, &system] {
            std::fs::create_dir_all(d).unwrap();
        }
        // `kde` only in the 2nd (local) dir; `x11` only in the 3rd (system) dir.
        for p in [
            local.join("kde-set"),
            local.join("kde-unset"),
            system.join("x11-set"),
            system.join("x11-unset"),
        ] {
            std::fs::write(p, "#!/bin/sh\nexit 10\n").unwrap();
        }

        let mut cfg = Config::default();
        cfg.evdev_grab_hooks = vec!["kde".into(), "x11".into()];
        let dirs = vec![config.clone(), local.clone(), system.clone()];
        let hooks = resolve_hooks_in_dirs(&cfg, &dirs).unwrap();

        assert_eq!(
            hooks[0].set_path,
            local.join("kde-set"),
            "kde resolves from ~/.local dir"
        );
        assert_eq!(
            hooks[1].set_path,
            system.join("x11-set"),
            "x11 resolves from /usr dir"
        );

        let _ = std::fs::remove_dir_all(root);
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
                codes: codes
                    .iter()
                    .map(|((n, p), c)| ((n.to_string(), p.to_string()), *c))
                    .collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HookCommandRunner for FakeRunner {
        fn run(&self, _path: &Path, phase: &str, name: &str) -> Result<i32> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), phase.to_string()));
            Ok(*self
                .codes
                .get(&(name.to_string(), phase.to_string()))
                .unwrap_or(&0))
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
        let (applied, outcomes) =
            run_setup_hooks_without_file_checks_for_test(&hooks, &runner).unwrap();
        assert_eq!(applied.names(), vec!["kde"]);
        assert_eq!(
            outcomes,
            vec![
                SetupOutcome::Skipped("sway".into()),
                SetupOutcome::Applied("kde".into())
            ]
        );
    }

    #[test]
    fn missing_hook_scripts_are_skipped_not_fatal() {
        // A configured hook whose scripts don't exist (e.g. a multi-desktop
        // config lists `gnome` but only `kde` scripts are installed) must NOT
        // abort evdev activation — that Err propagates up and kills the whole
        // daemon (fatal when daklak is launched as KWin's virtual keyboard).
        // Treat the missing hook like a not-applicable skip instead.
        let hooks = vec![HookSpec {
            name: "ghost".into(),
            set_path: PathBuf::from("/tmp/daklak-nonexistent-ghost-set"),
            unset_path: PathBuf::from("/tmp/daklak-nonexistent-ghost-unset"),
        }];
        let runner = FakeRunner::new(&[]);
        let (applied, outcomes) = run_setup_hooks_inner(&hooks, &runner, true).unwrap();
        assert!(applied.is_empty(), "missing hook applies nothing");
        assert_eq!(outcomes, vec![SetupOutcome::Skipped("ghost".into())]);
        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "missing hook script is never executed"
        );
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

    #[test]
    fn cleanup_treats_skip_exit_as_success() {
        let applied = AppliedHooks {
            hooks: vec![hook("gnome")],
        };
        let runner = FakeRunner::new(&[(("gnome", "unset"), 10)]);

        run_cleanup_hooks(&applied, &runner).unwrap();

        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec![("gnome".into(), "unset".into())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_marker_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "daklak-marker-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &root);

        let applied = AppliedHooks { hooks: vec![hook("sway")] };
        write_rollback_marker(&applied).unwrap();
        let path = rollback_marker_path().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_marker_records_paths_not_only_names() {
        let applied = AppliedHooks { hooks: vec![hook("sway")] };
        let text = rollback_marker_json(&applied).unwrap();
        assert!(text.contains("\"version\": 2"));
        assert!(text.contains("\"unset_path\""));
        assert!(text.contains("/tmp/sway-unset"));
    }
}
