use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub name: String,
    pub set_path: PathBuf,
    pub unset_path: PathBuf,
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
