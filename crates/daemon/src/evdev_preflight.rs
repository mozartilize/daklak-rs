use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};

use anyhow::{anyhow, Result};

const UINPUT_PATH: &str = "/dev/uinput";
const INPUT_DIR: &str = "/dev/input";

pub fn check_or_notify() -> Result<()> {
    match check() {
        Ok(()) => Ok(()),
        Err(e) => {
            notify_failure(&e.to_string());
            Err(e)
        }
    }
}

fn check() -> Result<()> {
    let mut problems = Vec::new();

    if let Err(e) = require_active_group("input") {
        problems.push(e.to_string());
    }
    if let Err(e) = require_active_group("uinput") {
        problems.push(e.to_string());
    }
    if let Err(e) = check_uinput_access() {
        problems.push(e.to_string());
    }
    if let Err(e) = check_input_access() {
        problems.push(e.to_string());
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "evdev grab preflight failed: {}. Add your user to input and uinput, ensure /dev/uinput exists, then log out/in.",
            problems.join("; ")
        ))
    }
}

fn require_active_group(name: &str) -> Result<()> {
    let gid = group_gid(name).ok_or_else(|| anyhow!("group {name:?} does not exist"))?;
    if active_gids().contains(&gid) {
        Ok(())
    } else {
        Err(anyhow!("current process is not in group {name:?}"))
    }
}

fn group_gid(name: &str) -> Option<u32> {
    let groups = fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut parts = line.split(':');
        let group_name = parts.next()?;
        let _passwd = parts.next()?;
        let gid = parts.next()?.parse().ok()?;
        if group_name == name {
            Some(gid)
        } else {
            None
        }
    })
}

#[cfg(unix)]
fn active_gids() -> HashSet<u32> {
    let mut gids = HashSet::new();
    gids.insert(unsafe { libc::getegid() });

    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count <= 0 {
        return gids;
    }

    let mut raw = vec![0 as libc::gid_t; count as usize];
    let got = unsafe { libc::getgroups(raw.len() as i32, raw.as_mut_ptr()) };
    if got > 0 {
        gids.extend(raw.into_iter().take(got as usize));
    }
    gids
}

#[cfg(not(unix))]
fn active_gids() -> HashSet<u32> {
    HashSet::new()
}

fn check_uinput_access() -> Result<()> {
    OpenOptions::new()
        .write(true)
        .open(UINPUT_PATH)
        .map(|_| ())
        .map_err(|e| anyhow!("cannot open {UINPUT_PATH} for writing: {e}"))
}

fn check_input_access() -> Result<()> {
    let mut saw_event = false;
    for entry in fs::read_dir(INPUT_DIR).map_err(|e| anyhow!("cannot read {INPUT_DIR}: {e}"))? {
        let entry = entry.map_err(|e| anyhow!("cannot read {INPUT_DIR} entry: {e}"))?;
        let path = entry.path();
        if !path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        saw_event = true;
        match OpenOptions::new().read(true).open(&path) {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }

    if saw_event {
        Err(anyhow!("no readable {INPUT_DIR}/event* device found"))
    } else {
        Err(anyhow!("no {INPUT_DIR}/event* devices found"))
    }
}

fn notify_failure(message: &str) {
    if let Err(e) = notify_failure_dbus(message) {
        tracing::warn!(error = %e, "desktop notification failed for evdev preflight failure");
    }
}

fn notify_failure_dbus(message: &str) -> Result<()> {
    use std::collections::HashMap;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::Value;

    let body = format!("{message}\n\nTry: sudo usermod -aG input,uinput $USER; then log out and back in.");
    let connection = Connection::session()?;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let _: u32 = proxy.call(
        "Notify",
        &(
            "Daklak",
            0_u32,
            "input-keyboard",
            "Daklak evdev grab unavailable",
            body.as_str(),
            Vec::<&str>::new(),
            HashMap::<&str, Value<'_>>::new(),
            -1_i32,
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn group_gid_reads_standard_group_file_shape() {
        // Smoke-test that the host group parser can read /etc/group without
        // panicking. Actual membership is machine-specific and tested by the
        // preflight at runtime.
        let _ = group_gid("input");
    }

    #[test]
    fn active_gids_includes_effective_gid() {
        let gids = active_gids();
        #[cfg(unix)]
        assert!(gids.contains(&unsafe { libc::getegid() }));
    }

    #[test]
    fn event_name_filter_matches_kernel_event_devices() {
        let path = Path::new("/dev/input/event0");
        assert!(path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("event")));
    }
}
