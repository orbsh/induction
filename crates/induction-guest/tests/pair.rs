//! The pair test: the real guest crate against the real host crate, over a real
//! pipe in a real process. Everything else in this workspace tests one half;
//! this file is what catches a half's API hole that only shows up when the
//! other half exercises it (the iterate producer binding was exactly that: the
//! guard lookup and `.on()`'s reserved-name rule could not both be satisfied,
//! and no single-crate test could see it).
//!
//! Run: `cargo test -p induction-guest`.

// Tests are allowed the unwrap/panic vocabulary the library targets are
// denied: a panicking test is a failing test with a stack trace. A `tests/`
// file is its own crate, so `cfg(test)` is false here — the allow is
// unconditional, and it is the same set the crate root grants its unit tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
)]

use std::time::Duration;

use induction_host::{Budgets, HostOps, InvokeCall, OpsFuture, Routed, Spec, route_line, spawn};
use induction_protocol::{CallFrame, GuestFrame, IterateKind, Reply, parse_guest_line};
use serde_json::{Value, json};

/// A host whose only arm is `invoke`: the minimal complete host.
struct Verbs;

impl HostOps for Verbs {
    fn invoke<'a>(&'a self, call: &'a InvokeCall) -> OpsFuture<'a> {
        let call = call.clone();
        Box::pin(async move {
            Ok(match call {
                InvokeCall::Verb { verb, args } => json!({ "verb": verb, "args": args }),
                InvokeCall::Booth { handler, args, .. } => {
                    json!({ "handler": handler, "args": args })
                }
            })
        })
    }
}

fn budgets() -> Budgets {
    Budgets {
        handshake: Duration::from_secs(5),
        write: Duration::from_secs(5),
        read: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn the_guest_crate_and_the_host_crate_speak_the_same_protocol() {
    let spec = Spec::new("loop", [env!("CARGO_BIN_EXE_loop_guest").to_string()]);
    let mut process = spawn(&spec).expect("spawn the guest fixture");
    let b = budgets();

    // ---- the load-time handshake: the declaration comes from the registry ----
    let hello = process
        .link
        .handshake(b.handshake)
        .await
        .expect("the guest answers initialize with hello");
    assert_eq!(hello.name, "loop");
    assert!(hello.schema.receives("echo"));
    assert!(hello.schema.receives("list"));
    assert!(hello.schema.hooks.contains(&"__wake".to_string()));
    assert!(
        !hello.schema.receives("__next__"),
        "the iterate producer is a mechanism, not a subscription: reserved names never enter receives"
    );
    assert!(
        !hello.schema.receives("__wake"),
        "hooks land in `hooks`, never in receives"
    );

    // ---- the pull form answers from the same registry ----
    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::call(1, "interface_schema", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("schema");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => {
            assert!(r.result["receives"]["echo"].is_object());
            assert_eq!(r.result["hooks"][0], "__wake");
        }
        other => panic!("expected a result frame, got {other:?}"),
    }

    // ---- a handler's value, a handler's failure: both are VALUES ----
    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::call(2, "echo", json!({"n": 1})).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("echo");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "echo": { "n": 1 } })),
        other => panic!("expected a result frame, got {other:?}"),
    }

    assert!(
        process
            .link
            .writer
            .write_line(&CallFrame::call(3, "boom", Value::Null).to_line(), b.write)
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("boom");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert!(
            r.result
                .as_str()
                .unwrap_or_default()
                .contains("deliberate failure"),
            "a failing handler rides back as an error value: {r:?}"
        ),
        other => panic!("expected a result frame, got {other:?}"),
    }

    // ---- the ctx arm crosses the boundary, in the guest's own spelling ----
    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::call(4, "ask_host", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("ctx frame");
    let reply = match route_line(&Verbs, &line).await {
        Routed::Reply(r) => r,
        other => panic!("expected a host arm, got {other:?}"),
    };
    assert!(
        reply.to_value().to_string().contains(r#""verb":"pages""#),
        "the guest used the verb spelling: {reply:?}"
    );
    assert!(
        process
            .link
            .writer
            .write_line(&reply.to_line(), b.write)
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("answer");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => {
            assert_eq!(r.id, json!(4));
            assert_eq!(r.result["from_host"]["verb"], "pages");
        }
        other => panic!("expected a result frame, got {other:?}"),
    }

    // ---- iterate: the producer binding, round by round, then exhaustion ----
    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::iterate(5, IterateKind::Start, "list", Value::Null, "s1").to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("start");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result["total"], 3),
        other => panic!("expected a result frame, got {other:?}"),
    }
    for (round, expect_item) in [(6u64, 1), (7, 2)] {
        assert!(
            process
                .link
                .writer
                .write_line(
                    &CallFrame::iterate(round, IterateKind::Next, "list", Value::Null, "s1")
                        .to_line(),
                    b.write
                )
                .await
        );
        let line = process
            .link
            .reader
            .next_line(b.read)
            .await
            .expect("pull");
        match parse_guest_line(&line) {
            Some(GuestFrame::Result(r)) => {
                assert_eq!(r.result["item"], expect_item, "round {round}");
                assert_eq!(r.result["done"], false);
            }
            other => panic!("expected a result frame, got {other:?}"),
        }
    }
    // the guard is exhausted: terminal, and the stream is gone afterwards
    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::iterate(8, IterateKind::Next, "list", Value::Null, "s1").to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("exhaustion");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "done": true })),
        other => panic!("expected a result frame, got {other:?}"),
    }

    process.kill();
}

/// ADR-0002, end to end: two requests, one process, and each handler waiting on
/// the reply to its own call.
///
/// Both requests are written before either can be answered, so a guest that
/// dispatches inline would answer `slow` first (and, before this change, would
/// hand `fast` the answer written for `slow`). Two ctx calls are live at once
/// here, which is also why the host must echo ids: the no-id fallback only
/// resolves a SINGLE pending call, so an old host stalls both handlers and this
/// test times out instead of passing on an accident.
#[tokio::test]
async fn two_requests_run_at_once_and_each_waits_on_its_own_reply() {
    let spec = Spec::new(
        "concurrent",
        [env!("CARGO_BIN_EXE_concurrent_guest").to_string()],
    );
    let mut process = spawn(&spec).expect("spawn the concurrent fixture");
    let b = budgets();
    let hello = process
        .link
        .handshake(b.handshake)
        .await
        .expect("handshake");
    assert!(hello.schema.receives("slow") && hello.schema.receives("fast"));

    // Both requests land before the guest can answer either: `slow` arrives
    // first but sleeps, and `fast` must not queue behind it.
    for frame in [
        CallFrame::call(1, "slow", Value::Null),
        CallFrame::call(2, "fast", Value::Null),
    ] {
        assert!(
            process
                .link
                .writer
                .write_line(&frame.to_line(), b.write)
                .await
        );
    }

    let mut answered = Vec::new();
    let mut call_ids = Vec::new();
    while answered.len() < 2 {
        let line = process
            .link
            .reader
            .next_line(b.read)
            .await
            .expect("a frame from the guest");
        match parse_guest_line(&line) {
            Some(GuestFrame::Host(h)) => {
                call_ids.push(h.id);
                // Echo the id, exactly as the driver does.
                let reply = Reply::ok(json!({ "verb": "probe", "who": h.args["who"].clone() }));
                let line = match h.id {
                    Some(id) => reply.to_line_with_id(id),
                    None => reply.to_line(),
                };
                assert!(process.link.writer.write_line(&line, b.write).await);
            }
            Some(GuestFrame::Result(r)) => answered.push(
                r.result["who"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            other => panic!("expected a result or a call, got {other:?}"),
        }
    }

    assert_eq!(
        answered,
        vec!["fast".to_string(), "slow".to_string()],
        "the later request finished first: the two handlers ran at once"
    );
    assert_eq!(call_ids.len(), 2, "one ctx call per handler");
    assert!(
        call_ids.iter().all(Option::is_some),
        "every ctx call carried an id: {call_ids:?}"
    );
    assert_ne!(call_ids[0], call_ids[1], "the ids are distinct");
    process.kill();
}

#[tokio::test]
async fn a_hook_is_reachable_by_its_reserved_name() {
    // The hook rides the same dispatch table: a host that calls `__wake` reaches
    // the registered handler (the deliver-by-reserved-name contract).
    let spec = Spec::new("loop", [env!("CARGO_BIN_EXE_loop_guest").to_string()]);
    let mut process = spawn(&spec).expect("spawn the guest fixture");
    let b = budgets();
    assert!(process.link.handshake(b.handshake).await.is_some());

    assert!(
        process
            .link
            .writer
            .write_line(
                &CallFrame::call(1, "__wake", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("hook answer");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "woke": true })),
        other => panic!("expected a result frame, got {other:?}"),
    }
    process.kill();
}

/// Stage one of a stop (aura ADR-0047), over a real process: the guest asks at a
/// boundary it already has, the HOST answers the bit, and the answer comes back
/// as a value the handler can act on. The parent here plays the host by hand, so
/// the assertion is on the frame that crosses the pipe (arm name + the boolean
/// that comes back), not on a mock.
#[tokio::test]
async fn a_guest_reads_the_stop_bit_the_host_holds() {
    let spec = Spec::new("loop", [env!("CARGO_BIN_EXE_loop_guest").to_string()]);
    let mut process = spawn(&spec).expect("spawn the guest fixture");
    let b = budgets();
    assert!(process.link.handshake(b.handshake).await.is_some());

    assert!(
        process
            .link
            .writer
            .write_line(&CallFrame::call(1, "stopped", Value::Null).to_line(), b.write)
            .await
    );

    // The guest's ctx round trip arrives as a host frame naming the arm.
    let line = process
        .link
        .reader
        .next_line(b.read)
        .await
        .expect("the guest asks the host");
    let asked = parse_guest_line(&line).expect("a decodable frame");
    let id = match &asked {
        GuestFrame::Host(h) => {
            assert_eq!(
                h.arm.as_str(),
                "stop.poll",
                "the guest asks the arm, not an event: {}",
                h.arm.as_str()
            );
            h.id.unwrap_or(0)
        }
        other => panic!("expected a host frame, got {other:?}"),
    };

    // The bit is set: answer it, and the handler reports what it read.
    assert!(
        process
            .link
            .writer
            .write_line(&Reply::ok(Value::Bool(true)).to_line_with_id(id), b.write)
            .await
    );
    let line = process.link.reader.next_line(b.read).await.expect("the result");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "stopped": true })),
        other => panic!("expected a result frame, got {other:?}"),
    }
    process.kill();
}

/// The guest's CBOR codec, over a real process. The parent here is hand-written
/// (ciborium straight onto the pipes) rather than `induction-host`: the host
/// link's writer is JSON-lines by contract, so the CBOR parent a deployment
/// uses is the CARRIER that declares the codec — this test speaks what that
/// carrier speaks. Without it the guest's CBOR loop has no real-process
/// coverage at all (`wire.rs` locks the declaration, not the loop; the
/// hand-written loop shape that rides the same body is likewise only reached
/// here).
#[test]
fn the_cbor_codec_carries_the_same_shapes_over_a_real_process() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_loop_guest"))
        .env("INDUCTION_ENCODING", "cbor")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the guest fixture in CBOR mode");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // One request document out; host frames serviced inline (the carrier's
    // exchange loop); the result value back. `emit` carries no reply.
    let mut call = |frame: Value, answer: Option<Value>| -> Value {
        ciborium::ser::into_writer(&frame, &mut stdin).expect("cbor encode");
        stdin.flush().unwrap();
        loop {
            let v: Value = ciborium::de::from_reader(&mut stdout).expect("a frame document");
            if let Some(host) = v.get("host") {
                if host.get("type").and_then(Value::as_str) == Some("emit") {
                    continue; // fire-and-forget by contract
                }
                let reply = json!({
                    "host_reply": { "ok": answer.clone().expect("unexpected ctx call") }
                });
                ciborium::ser::into_writer(&reply, &mut stdin).expect("cbor encode");
                stdin.flush().unwrap();
                continue;
            }
            return v.get("result").cloned().unwrap_or(Value::Null);
        }
    };

    // The pull form of the declaration, from the same registry, under CBOR.
    let schema = call(
        CallFrame::call(1, "interface_schema", Value::Null).to_value(),
        None,
    );
    assert!(schema["receives"]["echo"].is_object(), "{schema}");
    assert_eq!(schema["hooks"][0], "__wake", "{schema}");

    let out = call(
        CallFrame::call(2, "echo", json!({ "n": 1 })).to_value(),
        None,
    );
    assert_eq!(out, json!({ "echo": { "n": 1 } }), "{out}");

    // The ctx arm crosses the boundary in the guest's own spelling — the
    // verb form — and its host_reply comes back through the same document
    // stream that carried the request.
    let out = call(
        CallFrame::call(3, "ask_host", Value::Null).to_value(),
        Some(json!({ "verb": "pages", "args": { "filter": "rust" } })),
    );
    assert_eq!(out["from_host"]["verb"], "pages", "{out}");

    // A failing handler still rides back as an error VALUE, never a Null.
    let out = call(CallFrame::call(4, "boom", Value::Null).to_value(), None);
    assert!(
        out.as_str()
            .unwrap_or_default()
            .contains("deliberate failure"),
        "a failing handler rides back as an error value: {out}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
