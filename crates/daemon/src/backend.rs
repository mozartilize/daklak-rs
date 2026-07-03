use std::fmt;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    Auto,
    Ibus,
    Wayland,
    Evdev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTarget {
    Native,
    Evdev,
}

impl BackendTarget {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "native" | "auto" => Some(Self::Native),
            "evdev" | "evdev-grab" | "evdev_grab" => Some(Self::Evdev),
            _ => None,
        }
    }
}

impl InputBackend {
    pub fn parse_status(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "ibus" => Some(Self::Ibus),
            "wayland" => Some(Self::Wayland),
            "evdev" | "evdev-grab" | "evdev_grab" => Some(Self::Evdev),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ibus => "ibus",
            Self::Wayland => "wayland",
            Self::Evdev => "evdev",
        }
    }

    pub fn native_from_config(config: &Config) -> Self {
        #[cfg(feature = "ibus")]
        if config.enable_ibus {
            return Self::Ibus;
        }

        if config.enable_wayland {
            return Self::Wayland;
        }

        Self::Auto
    }

    pub fn startup_from_config(config: &Config) -> Self {
        if config.enable_evdev_grab {
            return Self::Evdev;
        }
        Self::native_from_config(config)
    }
}

impl fmt::Display for InputBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for BackendTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native => f.write_str("native"),
            Self::Evdev => f.write_str("evdev"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn parses_backend_targets_and_rejects_direct_native_backend_names() {
        assert_eq!(BackendTarget::parse("native"), Some(BackendTarget::Native));
        assert_eq!(BackendTarget::parse("auto"), Some(BackendTarget::Native));
        assert_eq!(BackendTarget::parse("evdev-grab"), Some(BackendTarget::Evdev));
        assert_eq!(BackendTarget::parse("ibus"), None);
        assert_eq!(BackendTarget::parse("wayland"), None);
    }

    #[test]
    fn startup_config_honors_explicit_evdev_before_native() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = true;
        cfg.enable_ibus = true;
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Evdev);
    }

    #[test]
    fn native_config_prefers_ibus_over_wayland() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        cfg.enable_ibus = true;
        #[cfg(feature = "ibus")]
        assert_eq!(InputBackend::native_from_config(&cfg), InputBackend::Ibus);
    }
}
