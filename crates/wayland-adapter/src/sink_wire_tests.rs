use super::*;
use crate::KeyState;

#[test]
fn v1_text_and_key_channels_are_independent_even_when_vk_is_selected() {
    for with_vk in [false, true] {
        for commit_string_functional in [false, true] {
            let (mut h, im) = Harness::v1(with_vk);
            let ctx = h.activate(&im);
            h.context_event(&ctx, zwp_input_method_context_v1::Event::CommitState { serial: 53 });
            let wire = &h.app.state.wire;
            let key_id = match wire.key_output(53).unwrap() {
                KeyOutput::VirtualKeyboard { vk, .. } => vk.id().protocol_id(),
                KeyOutput::V1Context { ctx, .. } => ctx.id().protocol_id(),
            };
            assert_eq!(wire.has_keysym_commit(), !with_vk);
            assert!(!wire.supports_focus_session(), "VK does not give IMv1 a global grab");
            h.requests();

            let mut adapter = AdapterCtx { state: &mut h.app.state };
            adapter.with_sink((1, 2, 4, 3), commit_string_functional, |sink| {
                sink.delete_surrounding_text(3, 1, 2, 1);
                sink.commit_string("ầ");
                sink.commit(999); // IMv1 has no batch-commit request.
                sink.vk_key(71, 14, KeyState::Pressed);
                sink.vk_key(71, 14, KeyState::Released);
                // Explicit argument must not override the context's current serial.
                assert!(sink.commit_key_channel_text(999, 72, "ậb"));
            });
            let requests = h.requests();
            assert!(is_request(&requests[0], &ctx, "delete_surrounding_text"));
            assert_eq!(requests[0].args, [(-3i32) as u32, 5]);
            assert!(is_request(&requests[1], &ctx, "commit_string"));
            assert_eq!(requests[1].args[0], 53);
            assert_eq!(requests[1].args[1], 4); // three UTF-8 bytes + NUL
            assert_eq!(requests[1].args[2].to_ne_bytes(), [0xe1, 0xba, 0xa7, 0]);
            assert!(requests[2..].iter().all(|r| r.object == key_id));

            let key_requests: Vec<_> = requests[2..].iter().filter(|r| {
                if with_vk {
                    ZwpVirtualKeyboardV1::interface().requests[r.opcode as usize].name == "key"
                } else {
                    ZwpInputMethodContextV1::interface().requests[r.opcode as usize].name == "key"
                }
            }).collect();
            if with_vk {
                assert_eq!(key_requests.len(), 6); // Backspace, ậ, b press/release
                assert_eq!(key_requests[0].args, [71, 14, 1]);
                assert_eq!(key_requests[1].args, [71, 14, 0]);
                assert_eq!(requests[2].args, [0, 0, 0, 3]);
                assert_eq!(requests[5].args, [1, 2, 4, 3]);
            } else {
                assert_eq!(key_requests.len(), 2); // text goes via keysym, not key
                assert_eq!(key_requests[0].args, [53, 71, 14, 1]);
                assert_eq!(key_requests[1].args, [53, 71, 14, 0]);
                assert_eq!(requests[2].args, [53, 0, 0, 0, 3]);
                assert_eq!(requests[5].args, [53, 1, 2, 4, 3]);
                let syms: Vec<_> = requests.iter().filter(|r| is_request(r, &ctx, "keysym"))
                    .map(|r| r.args.clone()).collect();
                assert_eq!(syms, [
                    vec![53, 72, 0x0100_1ead, 1, 0], vec![53, 72, 0x0100_1ead, 0, 0],
                    vec![53, 72, 0x62, 1, 0], vec![53, 72, 0x62, 0, 0],
                ]);
            }
        }
    }
}

#[test]
fn unsupported_middle_character_emits_nothing_before_whole_text_fallback() {
    for (with_vk, text) in [(true, "a🙂b"), (false, "a\0b")] {
        let (mut h, im) = Harness::v1(with_vk);
        h.activate(&im);
        h.requests();
        let mut adapter = AdapterCtx { state: &mut h.app.state };
        adapter.with_sink((0, 0, 0, 0), true, |sink| {
            assert!(!sink.commit_key_channel_text(0, 7, text));
        });
        assert!(h.requests().is_empty(), "no supported prefix may escape before fallback");
    }
}

#[test]
fn passthrough_keymap_cannot_be_used_for_synthetic_slots() {
    let (mut h, im) = Harness::v1(true);
    h.activate(&im);
    if let WireKind::V1 { vk: Some(vk), .. } = &mut h.app.state.wire.kind {
        vk.synthetic_keymap = false;
    }
    h.requests();
    let mut adapter = AdapterCtx { state: &mut h.app.state };
    adapter.with_sink((0, 0, 0, 0), true, |sink| {
        assert!(!sink.vk_commit_char(1, 'ậ'));
        assert!(!sink.commit_key_channel_text(0, 1, "ậb"));
    });
    assert!(h.requests().is_empty());
}

#[test]
fn focus_sessions_need_a_global_grab_and_a_synthetic_keymap() {
    let mut h = Harness::new(ImProtocol::ImV2);
    assert!(h.app.state.wire.supports_focus_session());
    assert!(!h.app.state.wire.has_keysym_commit());
    // A compositor-keymap VK can pass keys through but cannot type Vietnamese.
    if let WireKind::V2 { vk, .. } = &mut h.app.state.wire.kind {
        vk.synthetic_keymap = false;
    }
    assert!(!h.app.state.wire.supports_focus_session());
    assert!(!Harness::v1(false).0.app.state.wire.supports_focus_session());
}
