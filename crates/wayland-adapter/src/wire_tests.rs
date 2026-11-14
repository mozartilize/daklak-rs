//! Client-side wire regression tests. A socket peer records actual requests;
//! events are injected explicitly, without needing a running compositor.

#[path = "sink_wire_tests.rs"]
mod output_tests;

use std::io::Read;
use wayland_client::{Dispatch, EventQueue};
use wayland_protocols::wp::input_method::zv1::client::{zwp_input_method_context_v1, zwp_input_method_v1};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2;

use super::*;
use crate::{AdapterCtx, AdapterState, FrameSnapshot, KeyDecision, ModifierState, OutputSink};

#[derive(Default)]
struct Handler {
    serials: Vec<u32>,
    commit: bool,
}

impl AdapterHandler for Handler {
    fn on_done_frame(&mut self, ctx: &mut AdapterCtx<'_>, _: &FrameSnapshot) {
        let serial = ctx.serial();
        self.serials.push(serial);
        if self.commit {
            ctx.with_sink((0, 0, 0, 0), true, |sink| sink.commit(serial));
        }
    }
    fn on_key_pressed(&mut self, _: &mut AdapterCtx<'_>, _: u32, _: u32, _: Option<char>) -> KeyDecision { KeyDecision::ForwardRaw }
    fn apply_pending(&mut self, _: &mut AdapterCtx<'_>, _: u32, _: usize, _: &str, _: (u32, u32, u32, u32)) {}
    fn on_modifiers(&mut self, _: &mut AdapterCtx<'_>, _: ModifierState) {}
    fn on_focus_changed(&mut self, _: &mut AdapterCtx<'_>, _: Option<String>, _: bool) {}
}

struct Harness {
    conn: Connection,
    peer: TestPeer,
    _queue: EventQueue<WaylandAdapter<Handler>>,
    qh: QueueHandle<WaylandAdapter<Handler>>,
    app: WaylandAdapter<Handler>,
}

#[derive(Debug)]
struct Request { object: u32, opcode: u16, args: Vec<u32> }

impl Harness {
    fn new(protocol: ImProtocol) -> Self {
        let (state, peer) = AdapterState::for_test(protocol);
        peer.socket.set_nonblocking(true).unwrap();
        let conn = state.wire.conn.clone();
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        Self { conn, peer, _queue: queue, qh, app: WaylandAdapter { handler: Handler::default(), state } }
    }

    /// An IMv1 wire, optionally with a virtual keyboard as its key channel.
    fn v1(with_vk: bool) -> (Self, ZwpInputMethodV1) {
        let mut h = Self::new(ImProtocol::ImV1);
        let WireKind::V1 { _im, vk, .. } = &mut h.app.state.wire.kind else { unreachable!() };
        *vk = with_vk.then(|| h.peer.test_vk());
        let im = _im.clone();
        (h, im)
    }

    fn activate(&mut self, im: &ZwpInputMethodV1) -> ZwpInputMethodContextV1 {
        let ctx = self.peer.bind::<ZwpInputMethodContextV1>();
        <WaylandAdapter<Handler> as Dispatch<ZwpInputMethodV1, ()>>::event(
            &mut self.app, im, zwp_input_method_v1::Event::Activate { id: ctx.clone() }, &(), &self.conn, &self.qh,
        );
        ctx
    }

    fn context_event(&mut self, ctx: &ZwpInputMethodContextV1, event: zwp_input_method_context_v1::Event) {
        <WaylandAdapter<Handler> as Dispatch<ZwpInputMethodContextV1, ()>>::event(
            &mut self.app, ctx, event, &(), &self.conn, &self.qh,
        );
    }

    fn deactivate(&mut self, im: &ZwpInputMethodV1, ctx: &ZwpInputMethodContextV1) {
        <WaylandAdapter<Handler> as Dispatch<ZwpInputMethodV1, ()>>::event(
            &mut self.app, im, zwp_input_method_v1::Event::Deactivate { context: ctx.clone() }, &(), &self.conn, &self.qh,
        );
    }

    fn requests(&mut self) -> Vec<Request> {
        self.conn.flush().unwrap();
        self.received()
    }

    /// Requests that already reached the peer, without flushing for the wire.
    fn received(&mut self) -> Vec<Request> {
        let mut bytes = Vec::new();
        match (&self.peer.socket).read_to_end(&mut bytes) {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
            Err(e) => panic!("read wire requests: {e}"),
        }
        let words: Vec<_> = bytes.as_chunks::<4>().0.iter().map(|b| u32::from_ne_bytes(*b)).collect();
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < words.len() {
            let size = (words[offset + 1] >> 16) as usize / 4;
            assert!(size >= 2);
            result.push(Request { object: words[offset], opcode: words[offset + 1] as u16, args: words[offset + 2..offset + size].to_vec() });
            offset += size;
        }
        result
    }
}

fn is_request<I: Proxy>(request: &Request, proxy: &I, name: &str) -> bool {
    request.object == proxy.id().protocol_id()
        && I::interface().requests[request.opcode as usize].name == name
}

#[test]
fn v1_context_serial_is_not_incremented_and_stale_contexts_cannot_replace_it() {
    let (mut h, im) = Harness::v1(false);
    let old = h.activate(&im);
    h.context_event(&old, zwp_input_method_context_v1::Event::CommitState { serial: 73 });
    assert_eq!(h.app.handler.serials, [73]);

    let current = h.activate(&im);
    h.context_event(&current, zwp_input_method_context_v1::Event::CommitState { serial: 19 });
    h.deactivate(&im, &old);
    h.context_event(&old, zwp_input_method_context_v1::Event::CommitState { serial: 900 });
    assert_eq!(h.app.handler.serials, [73, 19]);
    assert_eq!(h.app.state.serial, 19);
    assert!(h.app.state.wire.is_active_context(&current));

    h.requests();
    h.app.state.emit_forward_key(42, 14, 1);
    let requests = h.requests();
    assert_eq!(requests.len(), 1);
    assert!(is_request(&requests[0], &current, "key"));
    assert_eq!(requests[0].args, [19, 42, 14, 1]);

    h.deactivate(&im, &current);
    assert!(h.app.state.wire.outputs(19).is_none());
    let requests = h.requests();
    assert_eq!(requests.iter().filter(|r| is_request(r, &current, "destroy")).count(), 1);
}

#[test]
fn v1_pending_batch_is_consumed_once_and_cannot_leak_into_next_context() {
    let (mut h, im) = Harness::v1(false);
    let old = h.activate(&im);
    h.context_event(&old, zwp_input_method_context_v1::Event::SurroundingText { text: "đỏ".into(), cursor: 5, anchor: 5 });
    assert!(h.app.state.wire.take_pending_commit());
    assert!(!h.app.state.wire.take_pending_commit());
    h.context_event(&old, zwp_input_method_context_v1::Event::ContentType { hint: 0, purpose: 12 });
    h.deactivate(&im, &old);
    let current = h.activate(&im);
    assert!(!h.app.state.wire.take_pending_commit());
    assert!(h.app.state.pending_frame.surrounding_text.is_none());
    assert_eq!(h.app.state.pending_frame.purpose, 0);
    h.context_event(&current, zwp_input_method_context_v1::Event::CommitState { serial: 8 });
    assert_eq!(h.app.state.serial, 8);
}

#[test]
fn v2_done_wraps_serial_and_acks_exactly_once_with_or_without_handler_output() {
    let mut h = Harness::new(ImProtocol::ImV2);
    let WireKind::V2 { im, .. } = &h.app.state.wire.kind else { unreachable!() };
    let im = im.clone();
    h.app.state.serial = u32::MAX;
    h.requests();
    for commit in [false, true, false] {
        h.app.handler.commit = commit;
        <WaylandAdapter<Handler> as Dispatch<ZwpInputMethodV2, ()>>::event(
            &mut h.app, &im, zwp_input_method_v2::Event::Done, &(), &h.conn, &h.qh,
        );
    }
    assert_eq!(h.app.handler.serials, [0, 1, 2]);
    let requests = h.requests();
    assert_eq!(requests.len(), 3);
    for (request, serial) in requests.iter().zip([0, 1, 2]) {
        assert!(is_request(request, &im, "commit"));
        assert_eq!(request.args, [serial]);
    }
}

#[test]
fn dropping_the_wire_releases_every_protocol_object_and_flushes() {
    // IMv2: the grab must be handed back without a later flush by anyone else.
    let mut h = Harness::new(ImProtocol::ImV2);
    let WireKind::V2 { im, grab, vk, .. } = &h.app.state.wire.kind else { unreachable!() };
    let (im, grab, vk) = (im.clone(), grab.clone(), vk.proxy.clone());
    h.requests();
    let (wire, _peer) = Wire::for_test(ImProtocol::ImV1);
    drop(std::mem::replace(&mut h.app.state.wire, wire));
    let requests = h.received();
    assert_eq!(requests.len(), 3);
    assert!(is_request(&requests[0], &grab, "release"));
    assert!(is_request(&requests[1], &im, "destroy"));
    assert!(is_request(&requests[2], &vk, "destroy"));

    // IMv1: an active context is destroyed exactly once.
    let (mut h, im) = Harness::v1(false);
    let ctx = h.activate(&im);
    h.requests();
    let (wire, _peer) = Wire::for_test(ImProtocol::ImV1);
    drop(std::mem::replace(&mut h.app.state.wire, wire));
    let requests = h.received();
    assert_eq!(requests.iter().filter(|r| is_request(r, &ctx, "destroy")).count(), 1);
}
