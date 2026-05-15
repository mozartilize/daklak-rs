// Wayland protocol probe for viet-ime development (step 0.5)
//
// Connects to the Wayland compositor and logs protocol events to verify
// compositor behavior. Tests surrounding_text ordering, done-frame collection,
// virtual keyboard permissions, and keyboard grab.
//
// Usage: probe [--timeout SECS] [--grab] [-v]

#![allow(non_camel_case_types, unused_unsafe, unused_variables)]
#![allow(non_upper_case_globals, non_snake_case, missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_registry,
        wl_seat::{self, WlSeat},
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};

// ─── Generated protocol bindings ──────────────────────────────────────────────

mod input_method_v2 {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports, clippy::all)]

    pub mod __interfaces {
        use wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "protocols/zwp-input-method-unstable-v2.xml"
        );
    }
    use self::__interfaces::*;
    use wayland_backend;
    use wayland_client;
    use wayland_client::protocol::*;

    wayland_scanner::generate_client_code!(
        "protocols/zwp-input-method-unstable-v2.xml"
    );
}

mod virtual_keyboard_v1 {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports, clippy::all)]

    pub mod __interfaces {
        use wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "protocols/zwp-virtual-keyboard-unstable-v1.xml"
        );
    }
    use self::__interfaces::*;
    use wayland_backend;
    use wayland_client;
    use wayland_client::protocol::*;

    wayland_scanner::generate_client_code!(
        "protocols/zwp-virtual-keyboard-unstable-v1.xml"
    );
}

use input_method_v2::{
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::ZwpInputMethodV2,
};
use virtual_keyboard_v1::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

// ─── ANSI helpers ─────────────────────────────────────────────────────────────

extern "C" {
    fn isatty(fd: i32) -> i32;
}

fn stdout_is_tty() -> bool {
    use std::os::unix::io::AsRawFd;
    unsafe { isatty(std::io::stdout().as_raw_fd()) != 0 }
}

struct Color {
    tty: bool,
}

impl Color {
    fn new() -> Self {
        Color { tty: stdout_is_tty() }
    }
    fn cyan(&self, s: &str) -> String {
        if self.tty { format!("\x1b[36m{s}\x1b[0m") } else { s.to_owned() }
    }
    fn green(&self, s: &str) -> String {
        if self.tty { format!("\x1b[32m{s}\x1b[0m") } else { s.to_owned() }
    }
    fn red(&self, s: &str) -> String {
        if self.tty { format!("\x1b[31m{s}\x1b[0m") } else { s.to_owned() }
    }
    fn yellow(&self, s: &str) -> String {
        if self.tty { format!("\x1b[33m{s}\x1b[0m") } else { s.to_owned() }
    }
    fn bold(&self, s: &str) -> String {
        if self.tty { format!("\x1b[1m{s}\x1b[0m") } else { s.to_owned() }
    }
    fn dim(&self, s: &str) -> String {
        if self.tty { format!("\x1b[2m{s}\x1b[0m") } else { s.to_owned() }
    }
}

// ─── CLI options ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ProbeOpts {
    timeout_secs: Option<u64>,
    grab: bool,
    verbose: bool,
}

impl ProbeOpts {
    fn parse() -> Result<Self, String> {
        let mut opts = ProbeOpts { timeout_secs: None, grab: false, verbose: false };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--timeout" => {
                    i += 1;
                    if i >= args.len() {
                        return Err("--timeout requires a value".into());
                    }
                    opts.timeout_secs = Some(
                        args[i]
                            .parse::<u64>()
                            .map_err(|_| format!("invalid timeout: {}", args[i]))?,
                    );
                }
                "--grab" => opts.grab = true,
                "-v" | "--verbose" => opts.verbose = true,
                "--help" | "-h" => {
                    println!(
                        "Usage: probe [OPTIONS]\n\
                         \n  --timeout SECS   Stop after N seconds (default: run until Ctrl-C)\
                         \n  --grab           Grab keyboard (intercepts ALL keys from apps)\
                         \n  -v               Verbose (log extra detail)\
                         \n  -h / --help      Show this help"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
            i += 1;
        }
        Ok(opts)
    }
}

// ─── Done-frame accumulator ───────────────────────────────────────────────────

#[derive(Default)]
struct DoneFrame {
    surrounding: Option<(String, u32, u32)>,
    change_cause: Option<u32>,
    content_type: Option<(u32, u32)>,
    received_at: Vec<(&'static str, Instant)>,
}

impl DoneFrame {
    fn record(&mut self, name: &'static str) {
        self.received_at.push((name, Instant::now()));
    }

    fn take(&mut self) -> DoneFrame {
        std::mem::take(self)
    }
}

// ─── Probe state ──────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct Probe {
    t0: Instant,
    color: Color,

    seat: Option<WlSeat>,
    im_manager: Option<ZwpInputMethodManagerV2>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,

    input_method: Option<ZwpInputMethodV2>,
    kbd_grab: Option<ZwpInputMethodKeyboardGrabV2>,
    #[allow(dead_code)]
    virtual_keyboard: Option<ZwpVirtualKeyboardV1>,

    frame: DoneFrame,

    last_surrounding_at: Option<Instant>,

    surrounding_text_count: u64,
    done_count: u64,
    key_count: u64,

    opts: ProbeOpts,
}

impl Probe {
    fn ts(&self) -> String {
        format!("[+{:.3}s]", self.t0.elapsed().as_secs_f64())
    }

    fn log(&self, category: &str, msg: &str) {
        println!("{} {} {}", self.color.dim(&self.ts()), self.color.cyan(category), msg);
    }

    fn log_ok(&self, msg: &str) {
        println!("{} {}", self.color.dim(&self.ts()), self.color.green(msg));
    }

    fn log_err(&self, msg: &str) {
        println!("{} {}", self.color.dim(&self.ts()), self.color.red(msg));
    }
}

// ─── Dispatch: WlRegistry ────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

// ─── Dispatch: WlSeat ────────────────────────────────────────────────────────

impl Dispatch<WlSeat, ()> for Probe {
    fn event(
        state: &mut Self,
        _proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let caps = match capabilities {
                wayland_client::WEnum::Value(c) => c,
                wayland_client::WEnum::Unknown(_) => return,
            };
            let kbd = caps.contains(wl_seat::Capability::Keyboard);
            let ptr = caps.contains(wl_seat::Capability::Pointer);
            let tch = caps.contains(wl_seat::Capability::Touch);
            state.log(
                "SEAT",
                &format!("capabilities: keyboard={kbd} pointer={ptr} touch={tch}"),
            );
        }
    }
}

// ─── Dispatch: ZwpInputMethodManagerV2 (no events) ───────────────────────────

delegate_noop!(Probe: ZwpInputMethodManagerV2);

// ─── Dispatch: ZwpVirtualKeyboardManagerV1 (no events) ───────────────────────

delegate_noop!(Probe: ZwpVirtualKeyboardManagerV1);

// ─── Dispatch: ZwpVirtualKeyboardV1 (no events) ──────────────────────────────

delegate_noop!(Probe: ZwpVirtualKeyboardV1);

// ─── Dispatch: ZwpInputMethodV2 ──────────────────────────────────────────────

impl Dispatch<ZwpInputMethodV2, ()> for Probe {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputMethodV2,
        event: input_method_v2::zwp_input_method_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use input_method_v2::zwp_input_method_v2::Event;
        match event {
            Event::Activate => {
                state.log("IM", "activate");
            }
            Event::Deactivate => {
                state.log("IM", "deactivate — resetting frame");
                state.frame = DoneFrame::default();
            }
            Event::SurroundingText { text, cursor, anchor } => {
                state.surrounding_text_count += 1;
                state.last_surrounding_at = Some(Instant::now());
                state.frame.record("surrounding_text");
                state.frame.surrounding = Some((text.clone(), cursor, anchor));
                state.log(
                    "IM",
                    &format!("surrounding_text text={text:?} cursor={cursor} anchor={anchor}"),
                );
            }
            Event::TextChangeCause { cause } => {
                let cause_name = match cause {
                    0 => "InputMethod",
                    1 => "Other",
                    _ => "Unknown",
                };
                state.frame.record("text_change_cause");
                state.frame.change_cause = Some(cause);
                state.log("IM", &format!("text_change_cause cause={cause} ({cause_name})"));
            }
            Event::ContentType { hint, purpose } => {
                state.frame.record("content_type");
                state.frame.content_type = Some((hint, purpose));
                state.log("IM", &format!("content_type hint={hint} purpose={purpose}"));
            }
            Event::Done => {
                state.done_count += 1;
                let frame = state.frame.take();

                let event_names: Vec<&str> = {
                    let mut names: Vec<&str> =
                        frame.received_at.iter().map(|(n, _)| *n).collect();
                    names.dedup();
                    names
                };

                let surrounding_present =
                    frame.received_at.iter().any(|(n, _)| *n == "surrounding_text");
                let order_note = if surrounding_present {
                    state.color.green("[ORDER OK: surrounding before done]")
                } else {
                    state.color.yellow("[no surrounding_text in this frame]")
                };

                let event_list = if event_names.is_empty() {
                    "(empty frame)".to_owned()
                } else {
                    event_names.join(", ")
                };

                println!(
                    "{} {} done — frame summary:",
                    state.color.dim(&state.ts()),
                    state.color.cyan("IM"),
                );
                println!(
                    "         events in frame: {} {}",
                    state.color.bold(&event_list),
                    order_note,
                );
                if let Some((text, cur, anc)) = &frame.surrounding {
                    println!("         surrounding: {text:?} cur={cur} anc={anc}");
                }
                if let Some(cause) = frame.change_cause {
                    let name = match cause {
                        0 => "InputMethod",
                        1 => "Other",
                        _ => "Unknown",
                    };
                    println!("         change_cause: {cause} ({name})");
                }
                if let Some((hint, purpose)) = frame.content_type {
                    println!("         content_type: hint={hint} purpose={purpose}");
                }
            }
            Event::Unavailable => {
                state.log_err("IM unavailable — compositor revoked input method");
            }
        }
    }
}

// ─── Dispatch: ZwpInputMethodKeyboardGrabV2 ──────────────────────────────────

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for Probe {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputMethodKeyboardGrabV2,
        event: input_method_v2::zwp_input_method_keyboard_grab_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use input_method_v2::zwp_input_method_keyboard_grab_v2::Event;
        match event {
            Event::Keymap { format, fd, size } => {
                let fmt_name = match format {
                    0 => "no_keymap",
                    1 => "xkb_v1",
                    _ => "unknown",
                };
                state.log("KBD", &format!("keymap format={fmt_name} size={size}"));
                // Close the fd to avoid leaks — fd is OwnedFd, drop closes it
                drop(fd);
            }
            Event::Key { serial, time, key, state: key_state } => {
                state.key_count += 1;
                // key_state is u32: 0=released, 1=pressed (wl_keyboard.key_state)
                let state_name = match key_state {
                    0 => "RELEASED",
                    1 => "PRESSED",
                    _ => "UNKNOWN",
                };
                let key_name = keycode_name(key);
                state.log(
                    "KBD",
                    &format!("key serial={serial} time={time} key={key} ({key_name}) state={state_name}"),
                );
            }
            Event::Modifiers { serial, mods_depressed, mods_latched, mods_locked, group } => {
                let verbose = state.opts.verbose;
                if verbose || mods_depressed != 0 || mods_latched != 0 || mods_locked != 0 {
                    state.log(
                        "KBD",
                        &format!(
                            "modifiers serial={serial} depressed={mods_depressed:#010x} \
                             latched={mods_latched:#010x} locked={mods_locked:#010x} group={group}"
                        ),
                    );
                }
            }
            Event::RepeatInfo { rate, delay } => {
                state.log("KBD", &format!("repeat_info rate={rate}/s delay={delay}ms"));
            }
        }
    }
}

// ─── Key code → name ─────────────────────────────────────────────────────────

fn keycode_name(code: u32) -> &'static str {
    match code {
        1 => "ESC",
        2 => "1",
        3 => "2",
        4 => "3",
        5 => "4",
        6 => "5",
        7 => "6",
        8 => "7",
        9 => "8",
        10 => "9",
        11 => "0",
        12 => "-",
        13 => "=",
        14 => "BACKSPACE",
        15 => "TAB",
        16 => "q",
        17 => "w",
        18 => "e",
        19 => "r",
        20 => "t",
        21 => "y",
        22 => "u",
        23 => "i",
        24 => "o",
        25 => "p",
        26 => "[",
        27 => "]",
        28 => "ENTER",
        29 => "CTRL",
        30 => "a",
        31 => "s",
        32 => "d",
        33 => "f",
        34 => "g",
        35 => "h",
        36 => "j",
        37 => "k",
        38 => "l",
        39 => ";",
        40 => "'",
        41 => "`",
        42 => "LSHIFT",
        43 => "\\",
        44 => "z",
        45 => "x",
        46 => "c",
        47 => "v",
        48 => "b",
        49 => "n",
        50 => "m",
        51 => ",",
        52 => ".",
        53 => "/",
        54 => "RSHIFT",
        56 => "LALT",
        57 => "SPACE",
        58 => "CAPSLOCK",
        100 => "RALT",
        _ => "?",
    }
}

// ─── Ctrl-C handler ──────────────────────────────────────────────────────────

const SIGINT: i32 = 2;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

static CTRLC_FLAG_PTR: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

extern "C" fn ctrlc_handler(_sig: i32) {
    let ptr = CTRLC_FLAG_PTR.load(Ordering::SeqCst);
    if ptr != 0 {
        // SAFETY: pointer set from Arc::into_raw in install_ctrlc
        let flag = unsafe { &*(ptr as *const AtomicBool) };
        flag.store(false, Ordering::SeqCst);
    }
}

fn install_ctrlc(flag: Arc<AtomicBool>) {
    // SAFETY: we store the raw pointer and restore it in ctrlc_handler
    let ptr = Arc::into_raw(flag) as usize;
    CTRLC_FLAG_PTR.store(ptr, Ordering::SeqCst);
    unsafe { signal(SIGINT, ctrlc_handler as usize) };
}

// ─── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let opts = ProbeOpts::parse().unwrap_or_else(|e| {
        eprintln!("probe: error: {e}");
        std::process::exit(1);
    });

    let color = Color::new();
    println!("{}", color.bold("[probe] connecting to Wayland display..."));

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}\n  {e}\n\n  Ensure:\n  \
                 • $WAYLAND_DISPLAY is set\n  \
                 • A Wayland compositor is running\n  \
                 • The socket at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY exists",
                color.red("[probe] ERROR: could not connect to Wayland"),
            );
            std::process::exit(1);
        }
    };

    let (globals, mut event_queue): (_, EventQueue<Probe>) =
        registry_queue_init::<Probe>(&conn).expect("failed to init registry queue");

    let qh = event_queue.handle();

    // ── Global discovery ──────────────────────────────────────────────────────
    println!("{}", color.bold("[probe] globals discovered:"));

    let mut found_im_manager = false;
    let mut found_vk_manager = false;
    let mut found_seat = false;

    for g in globals.contents().clone_list() {
        match g.interface.as_str() {
            "zwp_input_method_manager_v2" => {
                found_im_manager = true;
                println!("  {} zwp_input_method_manager_v2 (v{})", color.green("✓"), g.version);
            }
            "zwp_virtual_keyboard_manager_v1" => {
                found_vk_manager = true;
                println!(
                    "  {} zwp_virtual_keyboard_manager_v1 (v{})",
                    color.green("✓"),
                    g.version
                );
            }
            "wl_seat" => {
                found_seat = true;
                if opts.verbose {
                    println!("  {} wl_seat (v{})", color.green("✓"), g.version);
                }
            }
            other if opts.verbose => {
                println!("  {} {other} (v{})", color.dim("-"), g.version);
            }
            _ => {}
        }
    }

    if !found_im_manager {
        eprintln!(
            "  {} NOT FOUND: zwp_input_method_manager_v2\n\n{}\n  \
             This protocol requires a wlroots-based compositor (e.g. sway, river, labwc).\n  \
             GNOME Shell and KDE Plasma do not expose this protocol by default.",
            color.red("✗"),
            color.red("[probe] ERROR: required protocol not available — cannot continue"),
        );
        std::process::exit(1);
    }

    if !found_vk_manager {
        println!(
            "  {} NOT FOUND: zwp_virtual_keyboard_manager_v1 (virtual keyboard tests skipped)",
            color.yellow("✗"),
        );
    }

    if !found_seat {
        eprintln!("{}", color.red("[probe] ERROR: no wl_seat found — cannot continue"));
        std::process::exit(1);
    }

    // ── Bind globals ──────────────────────────────────────────────────────────
    let seat: WlSeat =
        globals.bind::<WlSeat, _, _>(&qh, 1..=8, ()).expect("failed to bind wl_seat");

    let im_manager: ZwpInputMethodManagerV2 =
        globals
            .bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
            .expect("failed to bind zwp_input_method_manager_v2");

    let vk_manager: Option<ZwpVirtualKeyboardManagerV1> = if found_vk_manager {
        Some(
            globals
                .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
                .expect("failed to bind zwp_virtual_keyboard_manager_v1"),
        )
    } else {
        None
    };

    // ── Build probe state ─────────────────────────────────────────────────────
    let mut probe = Probe {
        t0: Instant::now(),
        color: Color::new(),
        seat: Some(seat.clone()),
        im_manager: Some(im_manager.clone()),
        vk_manager: vk_manager.clone(),
        input_method: None,
        kbd_grab: None,
        virtual_keyboard: None,
        frame: DoneFrame::default(),
        last_surrounding_at: None,
        surrounding_text_count: 0,
        done_count: 0,
        key_count: 0,
        opts: opts.clone(),
    };

    event_queue.roundtrip(&mut probe).expect("initial roundtrip failed");

    // ── Create input method ───────────────────────────────────────────────────
    let input_method = im_manager.get_input_method(&seat, &qh, ());
    probe.log_ok("IM created input_method for seat");
    probe.input_method = Some(input_method.clone());

    // ── Grab keyboard ─────────────────────────────────────────────────────────
    if opts.grab {
        let kbd_grab = input_method.grab_keyboard(&qh, ());
        probe.log_ok("KBD grabbed keyboard");
        probe.kbd_grab = Some(kbd_grab);
    } else {
        probe.log("KBD", "keyboard not grabbed (use --grab to intercept keys)");
    }

    // ── Try virtual keyboard ──────────────────────────────────────────────────
    if let Some(ref vkm) = vk_manager {
        let vk = vkm.create_virtual_keyboard(&seat, &qh, ());
        probe.log_ok("VKBD created virtual keyboard — permissions OK");
        probe.virtual_keyboard = Some(vk);
    } else {
        probe.log("VKBD", "skipped (manager not available)");
    }

    println!(
        "\n{}\n",
        probe.color.bold(
            "[probe] ready — keys pass through to apps normally (use --grab to intercept)\n         focus any app to see surrounding_text events — Ctrl-C to stop"
        )
    );

    // ── Ctrl-C handler ────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    install_ctrlc(running.clone());

    let deadline =
        opts.timeout_secs.map(|s| Instant::now() + std::time::Duration::from_secs(s));

    // ── Event loop ────────────────────────────────────────────────────────────
    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                probe.log("probe", "timeout reached — exiting");
                break;
            }
        }
        event_queue.blocking_dispatch(&mut probe).expect("event queue error");
    }

    // ── Summary ───────────────────────────────────────────────────────────────
    println!(
        "\n{} surrounding_text={} done={} keys={}",
        probe.color.bold("[probe] done."),
        probe.surrounding_text_count,
        probe.done_count,
        probe.key_count,
    );

    // Clean up
    if let Some(grab) = probe.kbd_grab.take() {
        grab.release();
    }
    if let Some(im) = probe.input_method.take() {
        im.destroy();
    }
    if let Some(vk) = probe.virtual_keyboard.take() {
        vk.destroy();
    }
    let _ = event_queue.flush();
}
