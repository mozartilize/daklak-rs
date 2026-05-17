/// Find the currently focused window's `app_id`, `name`, and whether it is
/// XWayland-backed, via Sway IPC.
///
/// Returns `None` if not on Sway, `swaymsg` is unavailable, or no focused
/// window exists. **Blocks for 3-5ms** (fork + exec of swaymsg + JSON
/// parse) — use `tokio::task::spawn_blocking` when calling from inside
/// the async runtime. The 300ms focus poller in `wayland/mod.rs`
/// already does this and populates `AppState::last_focused_app_id`;
/// prefer that cache over direct calls on the Wayland dispatch hot path.
///
/// `is_xwayland` is derived from Sway's `shell` field on the focused node
/// (`"xwayland"`), with a fallback to the presence of `window_properties`
/// (Sway exposes `window_properties.class` only for XWayland clients).
///
/// For non-Sway wlroots compositors, we'd use
/// `zwlr_foreign_toplevel_management_v1` — TODO.
pub fn focused_app_info() -> Option<(String, String, bool)> {
    let output = std::process::Command::new("swaymsg")
        .args(["-t", "get_tree"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    find_focused(&json)
}

fn find_focused(node: &serde_json::Value) -> Option<(String, String, bool)> {
    if node.get("focused").and_then(|v| v.as_bool()) == Some(true) {
        // Native Wayland: `app_id` is the canonical identifier.
        // XWayland: Sway sets `app_id` to null/empty and exposes the
        //   WM_CLASS class in `window_properties.class`. Falling through
        //   lets force_vk_only_apps match XWayland clients (Chromium-via-X,
        //   VS Code-via-X, JetBrains IDEs in their X mode, etc.) by their
        //   X class name. Match in the user-supplied list stays
        //   case-insensitive so users can write "chromium" not "Chromium".
        let has_wm_class = node
            .get("window_properties")
            .and_then(|wp| wp.get("class"))
            .and_then(|v| v.as_str())
            .is_some();
        let shell_is_xwayland = node
            .get("shell")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("xwayland"))
            .unwrap_or(false);
        let is_xwayland = shell_is_xwayland || has_wm_class;
        let app_id = node
            .get("app_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                node.get("window_properties")
                    .and_then(|wp| wp.get("class"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
            .to_owned();
        let name = node
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        return Some((app_id, name, is_xwayland));
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(arr) = node.get(key).and_then(|v| v.as_array()) {
            for child in arr {
                if let Some(r) = find_focused(child) {
                    return Some(r);
                }
            }
        }
    }
    None
}
