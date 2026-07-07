use std::fmt;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    Auto,
    #[cfg_attr(not(feature = "ibus"), allow(dead_code))]
    Ibus,
    Wayland,
    Evdev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTarget {
    Native,
    Evdev,
    Toggle,
}

impl BackendTarget {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "native" | "auto" => Some(Self::Native),
            "evdev" | "evdev-grab" | "evdev_grab" => Some(Self::Evdev),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
    }
}

impl InputBackend {
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
        if config.ibus_requested {
            return Self::Ibus;
        }

        if config.enable_wayland {
            return Self::Wayland;
        }

        Self::Auto
    }

    pub fn startup_from_config(config: &Config) -> Self {
        // IBus is special: when requested, start the IBus connection first so
        // evdev can layer on top while IBus remains connected in passthrough.
        #[cfg(feature = "ibus")]
        if config.ibus_requested {
            return Self::Ibus;
        }

        // Otherwise, honor the persisted evdev startup switch directly.
        if config.enable_evdev_grab {
            return Self::Evdev;
        }

        if config.enable_wayland {
            return Self::Wayland;
        }

        Self::Auto
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
            Self::Toggle => f.write_str("toggle"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default, clippy::uninlined_format_args)]
    use super::*;
    use crate::config::Config;

    #[test]
    fn parses_backend_targets_and_rejects_direct_native_backend_names() {
        assert_eq!(BackendTarget::parse("native"), Some(BackendTarget::Native));
        assert_eq!(BackendTarget::parse("auto"), Some(BackendTarget::Native));
        assert_eq!(
            BackendTarget::parse("evdev-grab"),
            Some(BackendTarget::Evdev)
        );
        assert_eq!(BackendTarget::parse("toggle"), Some(BackendTarget::Toggle));
        assert_eq!(BackendTarget::parse("ibus"), None);
        assert_eq!(BackendTarget::parse("wayland"), None);
    }

    #[test]
    fn startup_honors_evdev_enabled_over_wayland() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = true;
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Evdev);
    }

    #[test]
    fn startup_falls_back_to_evdev_when_no_native() {
        let mut cfg = Config::default();
        cfg.enable_wayland = false;
        cfg.enable_evdev_grab = true;
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Evdev);
    }

    #[test]
    fn native_config_prefers_ibus_over_wayland() {
        let mut cfg = Config::default();
        cfg.enable_wayland = true;
        cfg.ibus_requested = true;
        #[cfg(feature = "ibus")]
        assert_eq!(InputBackend::native_from_config(&cfg), InputBackend::Ibus);
        #[cfg(not(feature = "ibus"))]
        assert_eq!(
            InputBackend::native_from_config(&cfg),
            InputBackend::Wayland
        );
    }

    // ── startup priority matrix tests ──────────────────────────────────

    #[test]
    fn matrix_ibus_x_wayland_x_evdev_x() {
        // --ibus, wayland, evdev → ibus (native) then evdev layered
        let mut cfg = Config::default();
        cfg.ibus_requested = true;
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = true;
        #[cfg(feature = "ibus")]
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Ibus,
            "with --ibus, ibus is the startup backend"
        );
        #[cfg(not(feature = "ibus"))]
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Evdev,
            "without ibus feature, enable_evdev_grab makes evdev active"
        );
    }

    #[test]
    fn matrix_ibus_dash_wayland_dash_evdev_x() {
        // no --ibus, no wayland, evdev → evdev startup backend
        let mut cfg = Config::default();
        cfg.enable_wayland = false;
        cfg.enable_evdev_grab = true;
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Evdev);
    }

    #[test]
    fn matrix_ibus_x_wayland_dash_evdev_dash() {
        // --ibus, no wayland, no evdev → ibus only
        let mut cfg = Config::default();
        cfg.enable_wayland = false;
        cfg.ibus_requested = true;
        cfg.enable_evdev_grab = false;
        #[cfg(feature = "ibus")]
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Ibus);
        #[cfg(not(feature = "ibus"))]
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Auto,
            "no ibus feature + no wayland + no evdev → Auto (error)"
        );
    }

    #[test]
    fn matrix_ibus_x_wayland_dash_evdev_x() {
        // --ibus, no wayland, evdev → ibus (native) then evdev layered
        let mut cfg = Config::default();
        cfg.enable_wayland = false;
        cfg.ibus_requested = true;
        cfg.enable_evdev_grab = true;
        #[cfg(feature = "ibus")]
        assert_eq!(InputBackend::startup_from_config(&cfg), InputBackend::Ibus);
        #[cfg(not(feature = "ibus"))]
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Evdev,
            "no ibus feature: falls back to evdev"
        );
    }

    #[test]
    fn matrix_ibus_dash_wayland_x_evdev_dash() {
        // no --ibus, wayland, no evdev → wayland only
        let mut cfg = Config::default();
        cfg.ibus_requested = false;
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = false;
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Wayland
        );
    }

    #[test]
    fn matrix_ibus_dash_wayland_x_evdev_x() {
        // no --ibus, wayland, evdev → evdev active on startup
        let mut cfg = Config::default();
        cfg.ibus_requested = false;
        cfg.enable_wayland = true;
        cfg.enable_evdev_grab = true;
        assert_eq!(
            InputBackend::startup_from_config(&cfg),
            InputBackend::Evdev,
            "config enable_evdev_grab=true makes evdev active on startup"
        );
    }
}
