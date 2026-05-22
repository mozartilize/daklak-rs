// Minimal zwp_input_method_v1 probe — used to verify whether KWin
// actually forwards key events to a v1 IME grab. Logs EVERY event on the
// IM context and the grabbed wl_keyboard. Self-contained; no daklak deps.
//
// Run as: kwin_wayland --inputmethod /path/to/im_v1_probe
// or copy daklak-wrap.sh and replace the daemon line.

use std::io::{stderr, Write};
use std::time::Instant;

use wayland_client::event_created_child;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use wayland_protocols::wp::input_method::zv1::client::{
    zwp_input_method_context_v1::{self, ZwpInputMethodContextV1},
    zwp_input_method_v1::{self, ZwpInputMethodV1},
};

struct State {
    im: Option<ZwpInputMethodV1>,
    ctx: Option<ZwpInputMethodContextV1>,
    kbd: Option<WlKeyboard>,
    start: Instant,
}

impl State {
    fn t(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

fn log(s: &State, args: std::fmt::Arguments) {
    let _ = writeln!(stderr(), "[{:>8.3}] {}", s.t(), args);
}

macro_rules! L { ($s:expr, $($arg:tt)*) => { log($s, format_args!($($arg)*)) } }

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            L!(st, "global name={name} iface={interface} ver={version}");
            if interface == "zwp_input_method_v1" {
                let im = registry.bind::<ZwpInputMethodV1, _, _>(name, version.min(1), qh, ());
                L!(st, "bound zwp_input_method_v1@{:?}", im.id());
                st.im = Some(im);
            }
        }
    }
}

impl Dispatch<ZwpInputMethodV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwpInputMethodV1,
        ev: zwp_input_method_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match ev {
            zwp_input_method_v1::Event::Activate { id } => {
                L!(st, "IM Activate ctx={:?}", id.id());
                let kbd = id.grab_keyboard(qh, ());
                L!(st, "grab_keyboard → kbd={:?}", kbd.id());
                // fcitx5/daklak modifiers_map (24 bytes incl. trailing NUL)
                let mod_map = b"Shift\0Control\0Mod1\0Mod4\0".to_vec();
                id.modifiers_map(mod_map);
                L!(st, "modifiers_map sent (24 bytes)");
                st.ctx = Some(id);
                st.kbd = Some(kbd);
            }
            zwp_input_method_v1::Event::Deactivate { context } => {
                L!(st, "IM Deactivate ctx={:?}", context.id());
                st.ctx = None;
                st.kbd = None;
            }
            other => L!(st, "IM ?? {:?}", other),
        }
    }

    event_created_child!(State, ZwpInputMethodV1, [
        0 => (ZwpInputMethodContextV1, ()),
    ]);
}

impl Dispatch<ZwpInputMethodContextV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwpInputMethodContextV1,
        ev: zwp_input_method_context_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwp_input_method_context_v1::Event::SurroundingText { text, cursor, anchor } => {
                L!(st, "ctx SurroundingText cursor={cursor} anchor={anchor} len={}", text.len());
            }
            zwp_input_method_context_v1::Event::ContentType { hint, purpose } => {
                L!(st, "ctx ContentType hint={hint} purpose={purpose}");
            }
            zwp_input_method_context_v1::Event::CommitState { serial } => {
                L!(st, "ctx CommitState serial={serial}");
            }
            zwp_input_method_context_v1::Event::Reset => L!(st, "ctx Reset"),
            zwp_input_method_context_v1::Event::PreferredLanguage { language } => {
                L!(st, "ctx PreferredLanguage {language}");
            }
            zwp_input_method_context_v1::Event::InvokeAction { button, index } => {
                L!(st, "ctx InvokeAction button={button} index={index}");
            }
            other => L!(st, "ctx ?? {:?}", other),
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_keyboard::Event::Keymap { format, fd: _, size } => {
                L!(st, "KBD Keymap fmt={:?} size={size}", format);
            }
            wl_keyboard::Event::Enter { serial, surface, .. } => {
                L!(st, "KBD Enter serial={serial} surface={:?}", surface.id());
            }
            wl_keyboard::Event::Leave { serial, surface } => {
                L!(st, "KBD Leave serial={serial} surface={:?}", surface.id());
            }
            wl_keyboard::Event::Key { serial, time, key, state } => {
                L!(st, ">>> KBD KEY <<< serial={serial} time={time} key={key} state={:?}", state);
            }
            wl_keyboard::Event::Modifiers { serial, mods_depressed, mods_latched, mods_locked, group } => {
                L!(st, "KBD Modifiers serial={serial} dep={mods_depressed:#x} lat={mods_latched:#x} lock={mods_locked:#x} grp={group}");
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                L!(st, "KBD RepeatInfo rate={rate} delay={delay}");
            }
            other => L!(st, "KBD ?? {:?}", other),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let display = conn.display();
    let mut queue: EventQueue<State> = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut state = State {
        im: None,
        ctx: None,
        kbd: None,
        start: Instant::now(),
    };

    L!(&state, "im_v1_probe started; WAYLAND_DISPLAY={:?} WAYLAND_SOCKET={:?}",
        std::env::var("WAYLAND_DISPLAY").ok(),
        std::env::var("WAYLAND_SOCKET").ok());

    // initial roundtrip to populate registry
    queue.roundtrip(&mut state)?;

    if state.im.is_none() {
        L!(&state, "FATAL zwp_input_method_v1 not advertised");
    }

    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}
