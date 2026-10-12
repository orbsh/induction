//! The pair test: the real guest crate against the real host crate, over a real
//! pipe in a real process. Everything else in this workspace tests one half;
//! this file is what catches a half's API hole that only shows up when the
//! other half exercises it (the iterate producer binding was exactly that: the
//! guard lookup and `.on()`'s reserved-name rule could not both be satisfied,
//! and no single-crate test could see it).
//!
//! Run: `cargo test -p induction-guest`.

use std::time::Duration;

use induction_host::{
    Budgets, HostOps, InvokeCall, OpsFuture, Routed, Spec, route_line, spawn,
};
use induction_protocol::{CallFrame, FrameKind, GuestFrame, parse_guest_line};
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
    let line = process.session.reader.next_line(b.read).await.expect("schema");
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
            .write_line(&CallFrame::call(2, "echo", json!({"n": 1})).to_line(), b.write)
            .await
    );
    let line = process.session.reader.next_line(b.read).await.expect("echo");
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
    let line = process.session.reader.next_line(b.read).await.expect("boom");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert!(
            r.result.as_str().unwrap_or_default().contains("deliberate failure"),
            "a failing handler rides back as an error value: {r:?}"
        ),
        other => panic!("expected a result frame, got {other:?}"),
    }

    // ---- the ctx arm crosses the boundary, in the guest's own spelling ----
    assert!(
        process
            .session
            .writer
            .write_line(&CallFrame::call(4, "ask_host", Value::Null).to_line(), b.write)
            .await
    );
    let line = process.session.reader.next_line(b.read).await.expect("ctx frame");
    let reply = match route_line(&Verbs, &line).await {
        Routed::Reply(r) => r,
        other => panic!("expected a host arm, got {other:?}"),
    };
    assert!(reply.contains(r#""verb":"pages""#), "the guest used the verb spelling: {reply}");
    assert!(process.session.writer.write_line(&reply, b.write).await);
    let line = process.session.reader.next_line(b.read).await.expect("answer");
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
                &CallFrame::iterate(5, FrameKind::IterateStart, "list", Value::Null, "s1").to_line(),
                b.write
            )
            .await
    );
    let line = process.session.reader.next_line(b.read).await.expect("start");
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
                    &CallFrame::iterate(round, FrameKind::IterateNext, "list", Value::Null, "s1")
                        .to_line(),
                    b.write
                )
                .await
        );
        let line = process.session.reader.next_line(b.read).await.expect("pull");
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
                &CallFrame::iterate(8, FrameKind::IterateNext, "list", Value::Null, "s1").to_line(),
                b.write
            )
            .await
    );
    let line = process.session.reader.next_line(b.read).await.expect("exhaustion");
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
            .write_line(&CallFrame::call(1, "__wake", Value::Null).to_line(), b.write)
            .await
    );
    let line = process.session.reader.next_line(b.read).await.expect("hook answer");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "woke": true })),
        other => panic!("expected a result frame, got {other:?}"),
    }
    process.kill();
}