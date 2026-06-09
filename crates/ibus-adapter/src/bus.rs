//! IBus bus address discovery.

use anyhow::{Context, Result};

/// Resolve the ibus-daemon private bus address.
///
/// Checks `$IBUS_ADDRESS` first, then parses `~/.config/ibus/bus/<machine-id>-unix-<display>`.
pub fn resolve_ibus_address() -> Result<String> {
    if let Ok(addr) = std::env::var("IBUS_ADDRESS") {
        if !addr.is_empty() {
            return Ok(addr);
        }
    }

    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = std::path::Path::new(&home).join(".config/ibus/bus");

    let suffix = if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
        format!("unix-{wl}")
    } else if let Ok(x) = std::env::var("DISPLAY") {
        let n = x.trim_start_matches(':');
        let n = n.split('.').next().unwrap_or("0");
        format!("unix-{n}")
    } else {
        "unix-0".to_string()
    };

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();

    // Prefer the file matching this display's suffix, but stale daemons leave
    // empty `IBUS_ADDRESS=` files behind (e.g. unix-wayland-1, unix-0). Sort
    // suffix-matches first, then scan all candidates and return the first file
    // that actually carries a non-empty address — never return an empty addr.
    entries.sort_by_key(|p| {
        let matches = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(&suffix))
            .unwrap_or(false);
        !matches // false (matches) sorts before true
    });

    for path in &entries {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            if let Some(addr) = line.strip_prefix("IBUS_ADDRESS=") {
                let addr = addr.trim();
                if !addr.is_empty() {
                    tracing::debug!(path = %path.display(), %addr, "ibus address file");
                    return Ok(addr.to_string());
                }
            }
        }
    }

    anyhow::bail!(
        "no non-empty IBUS_ADDRESS= in any file under {} (is ibus-daemon running?)",
        dir.display()
    )
}
