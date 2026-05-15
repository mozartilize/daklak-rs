/// Find the currently focused window's `app_id` and `name` via Sway IPC.
///
/// Returns `None` if not on Sway, `swaymsg` is unavailable, or no focused
/// window exists. Blocks for a few ms — only call when we genuinely need
/// the info (e.g. on IM activate, not per keystroke).
///
/// For non-Sway wlroots compositors, we'd use
/// `zwlr_foreign_toplevel_management_v1` — TODO.
pub fn focused_app_info() -> Option<(String, String)> {
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

fn find_focused(node: &serde_json::Value) -> Option<(String, String)> {
    if node.get("focused").and_then(|v| v.as_bool()) == Some(true) {
        let app_id = node
            .get("app_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let name = node
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        return Some((app_id, name));
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
