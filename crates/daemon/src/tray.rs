//! Tray icon / SNI app indicator with method and modern-style controls.
//!
//! The tray menu has:
//! - An on/off toggle (replaces `daklak toggle` / IPC enable|disable).
//! - A `Mode` submenu with Telex / VNI / ViQR radio items.
//! - A `Legacy tone placement` checkmark item (`òa` vs `oà`).
//! - An `Evdev hooks` submenu when evdev-grab support is compiled in.
//! - A `Quit` item.
//!
//! Mode, legacy-tone, and hook-list changes are written back to the config file.
//! Mode and legacy-tone changes are also forwarded to the daemon at runtime
//! through a `watch::Sender<ConfigChange>`.

#[cfg(feature = "evdev_grab")]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ksni::TrayMethods;

use tokio::sync::watch;

use crate::backend::{BackendTarget, InputBackend};
use crate::config::{Config, MethodConfig};
use crate::control::{CmdKind, CmdTx, Command, ConfigChange, StateRx};

pub struct DaklakTray {
    cmd_tx: CmdTx,
    config_change_tx: tokio::sync::watch::Sender<ConfigChange>,
    config_path: Option<PathBuf>,
    pub enabled: bool,
    pub method: MethodConfig,
    pub modern_style: bool,
    /// Whether evdev grab should be enabled at startup (persisted to config).
    pub evdev_enabled: bool,
    /// Current active backend (updated via watch channel at runtime).
    pub backend: InputBackend,
    pub evdev_grab_hooks: Vec<String>,
}

fn evdev_toggle_label(evdev_enabled: bool) -> &'static str {
    if evdev_enabled {
        "Evdev grab backend active"
    } else {
        "Enable evdev grab backend"
    }
}

impl ksni::Tray for DaklakTray {
    fn id(&self) -> String {
        "daklak".into()
    }

    fn icon_name(&self) -> String {
        if self.enabled {
            "input-keyboard".into()
        } else {
            "input-keyboard-symbolic".into()
        }
    }

    fn title(&self) -> String {
        format!("daklak — {}", if self.enabled { "on" } else { "off" })
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};

        let (label, target) = if self.enabled {
            ("Off", CmdKind::Disable)
        } else {
            ("On", CmdKind::Enable)
        };

        // Mode radio group index for the current method.
        let selected = match self.method {
            MethodConfig::Telex => 0,
            MethodConfig::Vni => 1,
            MethodConfig::Viqr => 2,
        };

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut Self| fire(&t.cmd_tx, target)),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Mode".into(),
                submenu: vec![RadioGroup {
                    selected,
                    select: Box::new(|tray: &mut Self, idx: usize| {
                        let method = match idx {
                            0 => MethodConfig::Telex,
                            1 => MethodConfig::Vni,
                            2 => MethodConfig::Viqr,
                            _ => return,
                        };
                        tray.method = method;
                        tray.apply(ConfigChange {
                            method,
                            modern_style: tray.modern_style,
                            enable_evdev_grab: tray.evdev_enabled,
                        });
                    }),
                    options: vec![
                        RadioItem {
                            label: "Telex".into(),
                            ..Default::default()
                        },
                        RadioItem {
                            label: "VNI".into(),
                            ..Default::default()
                        },
                        RadioItem {
                            label: "VIQR".into(),
                            ..Default::default()
                        },
                    ],
                }
                .into()],
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Legacy tone placement (òa vs oà)".into(),
                checked: !self.modern_style,
                activate: Box::new(|tray: &mut Self| {
                    let modern_style = !tray.modern_style;
                    tray.modern_style = modern_style;
                    tray.apply(ConfigChange {
                        method: tray.method,
                        modern_style,
                        enable_evdev_grab: tray.evdev_enabled,
                    });
                }),
                ..Default::default()
            }
            .into(),
        ];
        if cfg!(feature = "evdev_grab") {
            items.push(
                CheckmarkItem {
                    label: evdev_toggle_label(self.evdev_enabled).into(),
                    checked: self.evdev_enabled,
                    activate: Box::new(|t: &mut Self| {
                        let new_evdev = !t.evdev_enabled;
                        t.evdev_enabled = new_evdev;
                        let target = if new_evdev {
                            CmdKind::SetBackend(BackendTarget::Evdev)
                        } else {
                            CmdKind::SetBackend(BackendTarget::Native)
                        };
                        t.save_current_config();
                        fire(&t.cmd_tx, target);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            let hook_items = discover_hook_names()
                .into_iter()
                .map(|name| {
                    let checked = self.evdev_grab_hooks.iter().any(|h| h == &name);
                    CheckmarkItem {
                        label: name.clone(),
                        checked,
                        activate: Box::new(move |t: &mut Self| t.toggle_evdev_hook(&name)),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.push(
                SubMenu {
                    label: "Evdev hooks".into(),
                    submenu: hook_items,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| fire(&t.cmd_tx, CmdKind::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::debug!(?reason, "SNI watcher offline — tray icon unavailable");
        false // keep running, just without a visible icon
    }
}

impl DaklakTray {
    /// Send runtime-safe config changes to the daemon and persist the full tray config.
    fn apply(&mut self, change: ConfigChange) {
        let _ = self.config_change_tx.send(change);
        self.save_current_config();
    }

    fn toggle_evdev_hook(&mut self, name: &str) {
        if let Some(idx) = self.evdev_grab_hooks.iter().position(|h| h == name) {
            self.evdev_grab_hooks.remove(idx);
        } else {
            self.evdev_grab_hooks.push(name.to_owned());
        }
        self.save_current_config();
    }

    fn save_current_config(&self) {
        if let Some(path) = &self.config_path {
            save_config(
                path,
                ConfigChange {
                    method: self.method,
                    modern_style: self.modern_style,
                    enable_evdev_grab: self.evdev_enabled,
                },
                &self.evdev_grab_hooks,
            );
        }
    }
}

fn fire(tx: &CmdTx, kind: CmdKind) {
    let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
    let _ = tx.try_send(Command {
        kind,
        resp: resp_tx,
    });
}

/// Standard install locations for the config example file, in search order.
/// Mirrors the resource-lookup pattern used by hooks and xkb artifacts.
fn config_example_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("daklak")
                .join("config.toml.example"),
        );
    }
    dirs.push(PathBuf::from("/usr/share/daklak/config.toml.example"));
    dirs
}

/// Try to find and copy an installed config example to `target`.
/// Returns the example text on success, `None` on failure.
fn copy_config_example(target: &Path) -> Option<String> {
    let example = config_example_candidates()
        .into_iter()
        .find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&example).ok()?;
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(target, &text).is_err() {
        return None;
    }
    Some(text)
}

/// Replace the TOML value on the first line matching 'key = <old-value>'.
/// Handles scalars plus single-line or multiline arrays.
/// When `bound` is `Some(n)`, only searches `text[..n]` (the top-level
/// region before the first table header).
/// Returns 'true' if a replacement was made.
fn replace_toml_value(text: &mut String, key: &str, new_val: &str, bound: Option<usize>) -> bool {
    let search_until = bound.unwrap_or(text.len());

    // Helper: find needle in search region
    let find_in_region = |text: &str, needle: &str| -> Option<usize> {
        if let Some(pos) = text[..search_until].find(needle) {
            if pos + needle.len() <= search_until {
                return Some(pos);
            }
        }
        None
    };
    let value_end = |text: &str, val_start: usize, line_end: usize| -> usize {
        let first_line_value = text[val_start..line_end].trim_start();
        if first_line_value.starts_with('[') {
            let leading = text[val_start..line_end].find('[').unwrap_or(0);
            let array_start = val_start + leading;
            if let Some(close_rel) = text[array_start..search_until].find(']') {
                return array_start + close_rel + 1;
            }
        }
        line_end
    };
    // Search for '\nkey = ' and '\nkey=' to handle optional whitespace.
    let needle = format!("\n{} = ", key);
    let alt_needle = format!("\n{} =", key);
    for needle in [&needle, &alt_needle] {
        if let Some(pos) = find_in_region(text, needle) {
            let val_start = pos + needle.len();
            let line_end = text[val_start..]
                .find('\n')
                .map(|n| val_start + n)
                .unwrap_or(search_until);
            // Don't touch comment-only lines (value starts with #).
            if val_start < search_until && !text[val_start..line_end].trim().starts_with('#') {
                let val_end = value_end(text, val_start, line_end);
                text.replace_range(val_start..val_end, new_val);
                return true;
            }
        }
    }
    // Also check start of string (first line has no leading \n).
    for prefix in [&format!("{} = ", key), &format!("{} =", key)] {
        if text.starts_with(prefix) && prefix.len() <= search_until {
            let line_end = text[prefix.len()..]
                .find('\n')
                .map(|n| prefix.len() + n)
                .unwrap_or(search_until);
            if prefix.len() < search_until && !text[prefix.len()..line_end].trim().starts_with('#')
            {
                let val_end = value_end(text, prefix.len(), line_end);
                text.replace_range(prefix.len()..val_end, new_val);
                return true;
            }
        }
    }
    false
}

/// Find the byte offset of the first TOML table or array-of-tables header
/// (`[section]` or `[[array]]`) in the text, so that missing top-level keys
/// can be inserted before it rather than blindly appended at EOF (which would
/// put them inside the wrong table). Returns `None` when there are no table
/// headers.
fn find_first_table_pos(text: &str) -> Option<usize> {
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        // TOML table headers start with `[` at the start of a line.
        // Strip trailing comments (`# ...`) before checking for `]`.
        let without_comment = trimmed.split('#').next().unwrap_or("").trim_end();
        if without_comment.starts_with('[') && without_comment.ends_with(']') {
            // Compute offset: sum of lines up to this one + newlines
            let mut offset = 0usize;
            for (j, l) in text.lines().enumerate() {
                if j == i {
                    break;
                }
                offset += l.len() + 1; // +1 for the newline
            }
            return Some(offset);
        }
    }
    None
}

/// Format evdev_grab_hooks as a single-line TOML inline array.
fn format_hooks_array(hooks: &[String]) -> String {
    toml::Value::Array(
        hooks
            .iter()
            .map(|h| toml::Value::String(h.clone()))
            .collect(),
    )
    .to_string()
    .lines()
    .collect::<String>()
}

/// Read the config file at `path`, update tray-managed fields, and write back.
/// Logs a warning on any I/O or parse failure instead of crashing.
///
/// When the file doesn't exist, first copies the installed
/// `config.toml.example` (which carries detailed comment explanations), then
/// applies targeted text replacements so those comments survive every write.
#[allow(clippy::field_reassign_with_default, clippy::uninlined_format_args)]
fn save_config(path: &Path, change: ConfigChange, evdev_grab_hooks: &[String]) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match copy_config_example(path) {
            Some(t) => t,
            None => String::new(),
        },
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "tray: failed to read config for save");
            return;
        }
    };

    let method_str = match change.method {
        MethodConfig::Telex => "telex",
        MethodConfig::Vni => "vni",
        MethodConfig::Viqr => "viqr",
    };
    let hooks_str = format_hooks_array(evdev_grab_hooks);

    let evdev_str = change.enable_evdev_grab.to_string();
    let method_val = format!("\"{}\"", method_str);
    let modern_val = change.modern_style.to_string();

    let out = if text.is_empty() {
        // No example to copy from — generate from scratch (no comments).
        let mut cfg = Config::default();
        cfg.method = change.method;
        cfg.modern_style = change.modern_style;
        cfg.enable_evdev_grab = change.enable_evdev_grab;
        cfg.evdev_grab_hooks = evdev_grab_hooks.to_vec();
        match toml::to_string_pretty(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%e, "tray: failed to serialize config for save");
                return;
            }
        }
    } else {
        // Validate existing TOML before editing (warn + fallback to generated
        // on bad parse so broken configs don't silently corrupt).
        if let Err(e) = toml::from_str::<toml::Value>(&text) {
            tracing::warn!(path = %path.display(), %e, "tray: existing config has invalid TOML — replacing with generated config");
            let mut cfg = Config::default();
            cfg.method = change.method;
            cfg.modern_style = change.modern_style;
            cfg.enable_evdev_grab = change.enable_evdev_grab;
            cfg.evdev_grab_hooks = evdev_grab_hooks.to_vec();
            match toml::to_string_pretty(&cfg) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%e, "tray: failed to serialize config for save");
                    return;
                }
            }
        } else {
            // Apply targeted text replacements to preserve comments.
            // Only search the top-level region (before the first table header)
            // so that matches inside `[section]` are not mistaken for missing
            // top-level keys. Any field not found at the top level is appended
            // before the first table header (or at EOF if flat).
            let mut out = text;

            let mut pending = String::new();
            let mut append = |key: &str, val: &str| {
                // Recompute boundary before each call since replacements may
                // have shifted byte offsets.
                let boundary = find_first_table_pos(&out);
                if !replace_toml_value(&mut out, key, val, boundary) {
                    pending.push_str(&format!("\n{} = {}", key, val));
                }
            };
            append("method", &method_val);
            append("modern_style", &modern_val);
            append("enable_evdev_grab", &evdev_str);
            append("evdev_grab_hooks", &hooks_str);

            if !pending.is_empty() {
                // Recompute insertion point after all replacements.
                let pos = find_first_table_pos(&out).unwrap_or(out.len());
                out.insert_str(pos, &pending);
                // Ensure a newline separates the last pending entry from
                // any table header that follows.
                if pos < out.len() && !out[pos + pending.len()..].starts_with('\n') {
                    out.insert(pos + pending.len(), '\n');
                }
            }
            out
        }
    };

    // Validate the generated output before writing — if it doesn't parse
    // as valid Config, fall back to a clean generated TOML.
    let out = match toml::from_str::<toml::Value>(&out) {
        Ok(_) => out,
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "tray: save_config produced invalid TOML — falling back to generated config");
            let mut cfg = Config::default();
            cfg.method = change.method;
            cfg.modern_style = change.modern_style;
            cfg.enable_evdev_grab = change.enable_evdev_grab;
            cfg.evdev_grab_hooks = evdev_grab_hooks.to_vec();
            match toml::to_string_pretty(&cfg) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%e, "tray: failed to serialize config for save");
                    return;
                }
            }
        }
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), %e, "tray: failed to create config directory");
            return;
        }
    }
    if let Err(e) = std::fs::write(path, &out) {
        tracing::warn!(path = %path.display(), %e, "tray: failed to write config");
    }
    tracing::debug!(path = %path.display(), ?change, "tray saved config");
}

#[cfg(feature = "evdev_grab")]
fn discover_hook_names() -> Vec<String> {
    match crate::evdev_hooks::hook_dir() {
        Ok(dir) => discover_hook_names_in(&dir),
        Err(e) => {
            tracing::warn!(%e, "tray: failed to resolve evdev hook directory");
            builtin_hook_names()
        }
    }
}

#[cfg(not(feature = "evdev_grab"))]
fn discover_hook_names() -> Vec<String> {
    Vec::new()
}

#[cfg(feature = "evdev_grab")]
fn builtin_hook_names() -> Vec<String> {
    ["gnome", "kde", "sway", "x11"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[cfg(feature = "evdev_grab")]
fn discover_hook_names_in(dir: &Path) -> Vec<String> {
    let mut names: BTreeSet<String> = builtin_hook_names().into_iter().collect();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return names.into_iter().collect(),
        Err(e) => {
            tracing::warn!(path = %dir.display(), %e, "tray: failed to list evdev hooks");
            return names.into_iter().collect();
        }
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().and_then(|s| s.strip_suffix("-set")) else {
            continue;
        };
        if !hook_name_is_safe(name) {
            continue;
        }
        if dir.join(format!("{name}-unset")).is_file() {
            names.insert(name.to_owned());
        }
    }
    names.into_iter().collect()
}

#[cfg(feature = "evdev_grab")]
fn hook_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Spawn the tray icon best-effort. Logs a warning on D-Bus failure and returns
/// without aborting the daemon — IPC/CLI still work without a session bus.
pub fn spawn_tray(
    cmd_tx: CmdTx,
    mut state_rx: StateRx,
    mut backend_rx: watch::Receiver<InputBackend>,
    config_change_tx: tokio::sync::watch::Sender<ConfigChange>,
    config: &Config,
) {
    let method = config.method;
    let modern_style = config.modern_style;
    let config_path = config.config_path.clone();
    let evdev_enabled = config.enable_evdev_grab;
    let evdev_grab_hooks = config.evdev_grab_hooks.clone();

    tokio::spawn(async move {
        let tray = DaklakTray {
            cmd_tx,
            config_change_tx,
            config_path,
            enabled: *state_rx.borrow_and_update(),
            method,
            modern_style,
            evdev_enabled,
            backend: *backend_rx.borrow(),
            evdev_grab_hooks,
        };
        let handle = match tray.spawn().await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("app indicator unavailable (no session D-Bus or SNI host?): {e}");
                return;
            }
        };
        // Repaint on every state or backend change.
        loop {
            tokio::select! {
                biased;
                changed = state_rx.changed() => {
                    if changed.is_err() { break; }
                    let on = *state_rx.borrow_and_update();
                    handle.update(|t| t.enabled = on).await;
                }
                changed = backend_rx.changed() => {
                    if changed.is_err() { break; }
                    let b = *backend_rx.borrow_and_update();
                    handle.update(|t| t.backend = b).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_toggle_label_mentions_grab_backend() {
        assert_eq!(evdev_toggle_label(false), "Enable evdev grab backend");
        assert_eq!(evdev_toggle_label(true), "Evdev grab backend active");
    }

    #[test]
    fn save_config_creates_missing_config_file() {
        let root = std::env::temp_dir().join(format!(
            "daklak-tray-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("daklak").join("config.toml");

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                enable_evdev_grab: true,
            },
            &[],
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.method, MethodConfig::Vni);
        assert!(!cfg.modern_style);
        assert!(
            cfg.enable_evdev_grab,
            "enable_evdev_grab persisted on fresh file"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "evdev_grab")]
    #[test]
    fn discover_hook_names_merges_builtins_and_user_pairs() {
        let root = std::env::temp_dir().join(format!(
            "daklak-tray-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let hooks = root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for name in ["sway", "custom"] {
            std::fs::write(hooks.join(format!("{name}-set")), "#!/bin/sh\n").unwrap();
            std::fs::write(hooks.join(format!("{name}-unset")), "#!/bin/sh\n").unwrap();
        }
        std::fs::write(hooks.join("partial-set"), "#!/bin/sh\n").unwrap();

        let names = discover_hook_names_in(&hooks);

        assert_eq!(names, vec!["custom", "gnome", "kde", "sway", "x11"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_config_updates_evdev_hook_list() {
        let root = std::env::temp_dir().join(format!(
            "daklak-tray-hook-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("daklak").join("config.toml");

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Telex,
                modern_style: true,
                enable_evdev_grab: true,
            },
            &["sway".into(), "x11".into()],
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.evdev_grab_hooks, vec!["sway", "x11"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_config_replaces_multiline_evdev_hooks_without_leftover_tail() {
        let root = std::env::temp_dir().join(format!(
            "daklak-tray-multiline-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("daklak").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let old = r#"# keep this comment
method = "telex"
modern_style = true
enable_evdev_grab = true
evdev_grab_hooks = [
    "gnome",
    "kde",
    "sway",
    "x11",
]
"#;
        std::fs::write(&path, old).unwrap();

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Telex,
                modern_style: true,
                enable_evdev_grab: true,
            },
            &[],
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "comments preserved: {text}");
        assert!(text.contains("evdev_grab_hooks = []"));
        assert!(!text.contains("\"gnome\""), "old multiline array tail removed: {text}");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.evdev_grab_hooks.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_config_appends_missing_enable_evdev_grab_key() {
        // An existing config without `enable_evdev_grab` (e.g. a user config
        // from before the field was added) should get it appended on save.
        let root = std::env::temp_dir().join(format!(
            "daklak-append-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("daklak").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // Write a minimal config that predates enable_evdev_grab
        let old = r#"# My settings
method = "telex"
modern_style = true
"#;
        std::fs::write(&path, old).unwrap();

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                enable_evdev_grab: true,
            },
            &[],
        );

        let text = std::fs::read_to_string(&path).unwrap();
        // Comment from original preserved
        assert!(text.contains("# My settings"), "comments preserved");
        // New key present
        assert!(text.contains("enable_evdev_grab"), "missing key appended");
        // Parses as valid config
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.method, MethodConfig::Vni);
        assert!(!cfg.modern_style);
        assert!(cfg.enable_evdev_grab);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_toml_value_skips_nested_key_with_boundary() {
        // When a boundary is given (before the first table header),
        // replace_toml_value must not touch keys inside `[section]`.
        let mut text = String::from("method = \"telex\"\n\n[section]\nenable_evdev_grab = false\n");
        let boundary = find_first_table_pos(&text); // points to "[section]"
                                                    // enable_evdev_grab is NOT in the top-level region — replacement fails
        assert!(!replace_toml_value(
            &mut text,
            "enable_evdev_grab",
            "true",
            boundary
        ));
        // The nested key must remain untouched
        assert!(text.contains("[section]\nenable_evdev_grab = false"));
    }

    #[test]
    fn save_config_inserts_top_level_before_section_table() {
        // A config with `[section]` containing a key that matches a top-level
        // key name must NOT get its nested value replaced — the nested key
        // stays, and the missing top-level key is inserted before `[section]`.
        let root = std::env::temp_dir().join(format!(
            "daklak-nested-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("config.toml");
        std::fs::create_dir_all(&root).unwrap();

        // Top-level lacks enable_evdev_grab; it only appears inside [section].
        let content =
            "method = \"telex\"\nmodern_style = true\n\n[section]\nenable_evdev_grab = false\n";
        std::fs::write(&path, content).unwrap();

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                enable_evdev_grab: true,
            },
            &[],
        );

        let text = std::fs::read_to_string(&path).unwrap();

        // The nested key inside [section] must not be modified.
        assert!(
            text.contains("[section]\nenable_evdev_grab = false"),
            "nested enable_evdev_grab unchanged; output: {text}"
        );
        // A top-level enable_evdev_grab = true should exist before [section].
        let top = text.split("[section]").next().unwrap();
        assert!(
            top.contains("enable_evdev_grab = true"),
            "top-level enable_evdev_grab inserted before [section]"
        );
        // Output must be valid TOML (parse as Value to allow unknown tables).
        let _: toml::Value = toml::from_str(&text).unwrap();

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_toml_value_updates_in_place() {
        let mut text = String::from("# comment\nmethod = \"telex\"\nmodern_style = true\n");
        assert!(replace_toml_value(&mut text, "method", "\"vni\"", None));
        assert!(replace_toml_value(&mut text, "modern_style", "false", None));
        assert_eq!(text, "# comment\nmethod = \"vni\"\nmodern_style = false\n");
    }

    #[test]
    fn replace_toml_value_returns_false_for_missing_key() {
        let mut text = String::from("method = \"telex\"\n");
        assert!(!replace_toml_value(
            &mut text,
            "enable_evdev_grab",
            "true",
            None
        ));
        assert_eq!(text, "method = \"telex\"\n", "text unchanged");
    }

    #[test]
    fn format_hooks_array_quotes_properly() {
        // Uses TOML serialization, so strings are always properly quoted.
        let hooks: Vec<String> = vec!["sway".into(), "x11".into()];
        let formatted = format_hooks_array(&hooks);
        assert_eq!(formatted, "[\"sway\", \"x11\"]");
    }

    #[test]
    fn format_hooks_array_empty() {
        assert_eq!(format_hooks_array(&[]), "[]");
    }

    #[test]
    fn find_first_table_pos_returns_none_for_flat_config() {
        let text = "method = \"telex\"\nmodern_style = true\n";
        assert_eq!(find_first_table_pos(text), None);
    }

    #[test]
    fn find_first_table_pos_finds_table_header() {
        let text = "# header\nmethod = \"telex\"\n\n[section]\nkey = 1\n";
        let pos = find_first_table_pos(text).unwrap();
        assert!(pos > 0);
        assert_eq!(&text[pos..pos + 9], "[section]");
    }

    #[test]
    fn find_first_table_pos_handles_trailing_comment() {
        let text = "method = \"telex\"\n\n[section] # my table\nkey = 1\n";
        let pos = find_first_table_pos(text).unwrap();
        assert_eq!(&text[pos..pos + 9], "[section]");
        // Also: sure the comment is on the same line after the header
        assert!(text[pos..].contains("# my table"));
    }

    #[test]
    fn save_config_preserves_table_with_comment_on_header() {
        // A table header with a trailing `# comment` must be correctly
        // detected so that inserted keys go before it.
        let root = std::env::temp_dir().join(format!(
            "daklak-comment-header-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        let path = root.join("config.toml");
        std::fs::create_dir_all(&root).unwrap();

        let content = "method = \"telex\"\n\n[section] # important section\nenable_evdev_grab = false\n";
        std::fs::write(&path, content).unwrap();

        save_config(
            &path,
            ConfigChange {
                method: MethodConfig::Vni,
                modern_style: false,
                enable_evdev_grab: true,
            },
            &[],
        );

        let text = std::fs::read_to_string(&path).unwrap();

        // Top-level enable_evdev_grab inserted before [section]
        let top = text.split("[section]").next().unwrap();
        assert!(top.contains("enable_evdev_grab = true"));
        // Nested value unchanged
        assert!(text.contains("enable_evdev_grab = false"));
        // Comment on the table header preserved
        assert!(text.contains("# important section"));

        let _ = std::fs::remove_dir_all(root);
    }
}
