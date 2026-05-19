// VK probe: uploads an arbitrary xkb_file via zwp_virtual_keyboard_v1.keymap(),
// then emits kc 30 sequences across all FOUR_LEVEL levels. Use with xev under
// XWayland to confirm whether vk path delivers low-kc Vietnamese chars to X11
// clients the same way the uinput+sway-xkb_file path does.
//
// Usage:
//   cargo run -p probe --bin vk_xkb_probe -- \
//     /home/mozart/Documents/workspace/daklak-rs/tools/xkb-probe/probe-four-level.xkb

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Instant;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_registry,
        wl_seat::{self, WlSeat},
    },
};

mod virtual_keyboard_v1 {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports, clippy::all)]

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "protocols/zwp-virtual-keyboard-unstable-v1.xml"
        );
    }
    use self::__interfaces::*;
    use wayland_client;
    use wayland_client::protocol::*;

    wayland_scanner::generate_client_code!(
        "protocols/zwp-virtual-keyboard-unstable-v1.xml"
    );
}

use virtual_keyboard_v1::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Create a CLOEXEC memfd holding `data` (no trailing NUL — Wayland vk
/// expects the size of the keymap text only, no terminator counted).
/// Returns (fd, size-in-bytes).
fn keymap_memfd(data: &[u8]) -> std::io::Result<(OwnedFd, u32)> {
    let name = CString::new("vk-xkb-probe").unwrap();
    // SAFETY: memfd_create is a stable Linux syscall (>= 3.17).
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh, owned fd from a successful syscall.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut f = unsafe { std::fs::File::from_raw_fd(libc::dup(raw)) };
    f.write_all(data)?;
    f.flush()?;
    Ok((fd, data.len() as u32))
}

fn main() {
    let xkb_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: vk_xkb_probe <path-to-xkb-file>");
        std::process::exit(2);
    });
    let xkb_text = fs::read_to_string(&xkb_path).expect("read xkb file");

    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY connect");
    let (globals, mut eq) =
        registry_queue_init::<State>(&conn).expect("registry init");
    let qh = eq.handle();

    let seat: WlSeat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=8, ())
        .expect("bind wl_seat");
    let vkm: ZwpVirtualKeyboardManagerV1 = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .expect("bind zwp_virtual_keyboard_manager_v1");

    let vk = vkm.create_virtual_keyboard(&seat, &qh, ());

    // Upload custom keymap text via memfd.
    let (fd, size) = keymap_memfd(xkb_text.as_bytes()).expect("memfd");
    let borrow: BorrowedFd = fd.as_fd();
    vk.keymap(1 /* xkb v1 */, borrow, size);
    eq.roundtrip(&mut State).expect("roundtrip after keymap upload");
    drop(fd);

    println!("vk created; keymap uploaded from {xkb_path} ({size} bytes)");
    println!("Focus xev (under XWayland) within 2s of each ENTER.");

    let t0 = Instant::now();
    let now_ms = || t0.elapsed().as_millis() as u32;

    // (label, expected keysym, depressed mask)
    // Mod mask convention: Shift = 0x01, Mod5 = 0x80 (matches daklak's
    // MOD_SHIFT / MOD_LEVEL3 constants in crates/keymap/src/lib.rs).
    let cases: &[(&str, &str, u32)] = &[
        ("L1 plain     ", "à 0x00e0", 0x00),
        ("L2 Shift     ", "À 0x00c0", 0x01),
        ("L3 Mod5      ", "á 0x00e1", 0x80),
        ("L4 Shift+Mod5", "Á 0x00c1", 0x81),
    ];

    for (label, expected, mods) in cases {
        let mut s = String::new();
        eprint!("ENTER then focus xev ({label} → {expected}, mods=0x{mods:02x}): ");
        std::io::stdout().flush().ok();
        std::io::stdin().read_line(&mut s).ok();
        std::thread::sleep(std::time::Duration::from_secs(2));

        // Set modifier state, then tap kc 30 (KEY_A = evdev), then restore.
        vk.modifiers(*mods, 0, 0, 0);
        vk.key(now_ms(), 30, 1);
        vk.key(now_ms(), 30, 0);
        if *mods != 0 {
            vk.modifiers(0, 0, 0, 0);
        }
        eq.roundtrip(&mut State).expect("roundtrip after emit");
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    vk.destroy();
    eq.flush().ok();
    println!("done. compare xev keysyms to the expected column above.");
}
