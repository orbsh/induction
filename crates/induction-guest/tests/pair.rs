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
use induction_protocol::{CallFrame, GuestFrame, IterateKind, parse_guest_line};
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
        .session
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
            .session
            .writer
            .write_line(
                &CallFrame::call(1, "interface_schema", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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
            .session
            .writer
            .write_line(
                &CallFrame::call(2, "echo", json!({"n": 1})).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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
            .session
            .writer
            .write_line(&CallFrame::call(3, "boom", Value::Null).to_line(), b.write)
            .await
    );
    let line = process
        .session
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
            .session
            .writer
            .write_line(
                &CallFrame::call(4, "ask_host", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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
            .session
            .writer
            .write_line(&reply.to_line(), b.write)
            .await
    );
    let line = process
        .session
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
            .session
            .writer
            .write_line(
                &CallFrame::iterate(5, IterateKind::Start, "list", Value::Null, "s1")
                    .to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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
                .session
                .writer
                .write_line(
                    &CallFrame::iterate(round, IterateKind::Next, "list", Value::Null, "s1")
                        .to_line(),
                    b.write
                )
                .await
        );
        let line = process
            .session
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
            .session
            .writer
            .write_line(
                &CallFrame::iterate(8, IterateKind::Next, "list", Value::Null, "s1").to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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

#[tokio::test]
async fn a_hook_is_reachable_by_its_reserved_name() {
    // The hook rides the same dispatch table: a host that calls `__wake` reaches
    // the registered handler (the deliver-by-reserved-name contract).
    let spec = Spec::new("loop", [env!("CARGO_BIN_EXE_loop_guest").to_string()]);
    let mut process = spawn(&spec).expect("spawn the guest fixture");
    let b = budgets();
    assert!(process.session.handshake(b.handshake).await.is_some());

    assert!(
        process
            .session
            .writer
            .write_line(
                &CallFrame::call(1, "__wake", Value::Null).to_line(),
                b.write
            )
            .await
    );
    let line = process
        .session
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

/// The guest's CBOR codec, over a real process. The parent here is hand-written
/// (ciborium straight onto the pipes) rather than `induction-host`: the host
/// session's writer is JSON-lines by contract, so the CBOR parent a deployment
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
