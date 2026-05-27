//! Synthetic xkb keymap memfd builder.
//!
//! Lives here (not in `viet-ime-keymap`) because memfd-backed keymap
//! upload is a wire-protocol concern: both `zwp_virtual_keyboard_v1.keymap()`
//! and `ei_keyboard.keymap` take an fd + size. The pure xkb text and
//! Vietnamese inventory remain in `viet-ime-keymap::keymap_text()`.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use viet_ime_keymap::keymap_text;
use xkbcommon::xkb::{
    Context as XkbCtx, Keymap, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS,
    KEYMAP_FORMAT_TEXT_V1,
};

/// Memfd-backed handle to the synthesised daklak keymap.
pub struct DaklakKeymap {
    pub fd: OwnedFd,
    pub size: u32,
}

/// Build the keymap, validate via libxkbcommon parse, dump into a memfd.
/// Compositor reads the fd via `zwp_virtual_keyboard_v1::keymap()`.
pub fn build() -> Result<DaklakKeymap> {
    let text = keymap_text();

    let ctx = XkbCtx::new(CONTEXT_NO_FLAGS);
    let _ = Keymap::new_from_string(
        &ctx,
        text.clone(),
        KEYMAP_FORMAT_TEXT_V1,
        KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow!("xkbcommon rejected synthesized daklak keymap"))?;

    let name = CString::new("daklak-keymap").unwrap();
    // SAFETY: memfd_create is a stable Linux syscall (>= 3.17).
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: raw is a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.extend_from_slice(text.as_bytes());
    buf.push(0);

    let mut offset = 0;
    while offset < buf.len() {
        // SAFETY: writing into our own freshly-created fd.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                buf[offset..].as_ptr() as *const _,
                buf.len() - offset,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error())
                .context("write daklak keymap into memfd");
        }
        offset += n as usize;
    }

    Ok(DaklakKeymap { fd, size: buf.len() as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_parses() {
        let km = build().expect("daklak keymap must parse");
        assert!(km.size > 100);
    }
}
