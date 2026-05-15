// Generated Wayland protocol bindings — same pattern as tools/probe/src/main.rs.
// Uses wlroots-flavor XMLs (not public wayland-protocols) per docs/protocol-behavior.md.

pub mod input_method_v2 {
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

pub mod virtual_keyboard_v1 {
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
