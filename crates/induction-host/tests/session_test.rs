//! Session tests: the two halves of the contract an in-memory fake can prove,
//! plus one live child (the `loop` example) proving the spawn+PDEATHSIG path,
//! the handshake and a real round trip.
//!
//! Run: `cargo test -p induction-host`.

use std::time::Duration;

use induction_host::{
    Budgets, HostOps, InvokeCall, OpsFuture, Outcome, Routed, Spec, route_line, run_driver, spawn,
};
use induction_protocol::{CallFrame, FrameKind, GuestFrame, Reply, parse_guest_line};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Serve one guest line and flatten it to "the line to write back, or nothing".
async fn serve(line: &str, ops: &dyn HostOps) -> String {
    match route_line(ops, line).await {
        Routed::Reply(r) => r.to_line(),
        _ => String::new(),
    }
}

// ======================= in-memory session =======================

/// A fake guest on the other end of a duplex: answers the handshake, then
/// echoes whatever request it receives as a result frame.
async fn fake_guest(side: tokio::io::DuplexStream) {
    let (r, mut w) = tokio::io::split(side);
    let mut reader = BufReader::new(r);
    // handshake: read initialize, answer hello
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["type"], "initialize");
    w.write_all(br#"{"type":"hello","name":"fake","schema":{"receives":{"ping":{}}}}"#)
        .await
        .unwrap();
    w.write_all(b"\n").await.unwrap();
    // one round trip
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    let answer = json!({ "id": v["id"], "result": { "saw": v["event"] } });
    w.write_all(answer.to_string().as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
}

#[tokio::test]
async fn handshake_and_round_trip_over_a_duplex() {
    let (host_side, guest_side) = tokio::io::duplex(4096);
    let guest = tokio::spawn(fake_guest(guest_side));

    let (r, w) = tokio::io::split(host_side);
    let mut session = induction_host::Session::new(r, w);
    let budgets = Budgets {
        handshake: Duration::from_secs(1),
        write: Duration::from_secs(1),
        read: Duration::from_secs(1),
    };

    let hello = session.handshake(budgets.handshake).await.expect("hello");
    assert_eq!(hello.name, "fake");
    assert!(hello.schema.receives("ping"));

    assert!(
        session
            .writer
            .write_line(&CallFrame::call(1, "ping", json!(null)).to_line(), budgets.write)
            .await
    );
    let line = session.reader.next_line(budgets.read).await.expect("result line");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => {
            assert_eq!(r.id, json!(1));
            assert_eq!(r.result, json!({ "saw": "ping" }));
        }
        other => panic!("expected a result frame, got {other:?}"),
    }

    guest.await.unwrap();
}

#[tokio::test]
async fn a_missing_handshake_refuses_the_session() {
    let (host_side, guest_side) = tokio::io::duplex(256);
    drop(guest_side); // peer gone before hello
    let (r, w) = tokio::io::split(host_side);
    let mut session = induction_host::Session::new(r, w);
    assert!(
        session
            .handshake(Duration::from_millis(200))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_write_that_misses_its_budget_reports_false() {
    // A peer that never reads: the write must give up rather than hang.
    let (a, b) = tokio::io::duplex(64);
    let (r, w) = tokio::io::split(a);
    let mut session = induction_host::Session::new(r, w);
    let big = "x".repeat(4096);
    assert!(
        !session
            .writer
            .write_line(&big, Duration::from_millis(50))
            .await,
        "an unbounded write must not be what a stalled peer gets"
    );
    drop(b);
}

// ============================ arms ============================

/// A stub host: `invoke` answers, everything else is the default refusal.
struct Stub;

impl HostOps for Stub {
    fn invoke<'a>(&'a self, call: &'a InvokeCall) -> OpsFuture<'a> {
        let call = call.clone();
        Box::pin(async move {
            Ok(match call {
                InvokeCall::Verb { verb, args } => json!({ "verb": verb, "args": args }),
                InvokeCall::Booth { handler, .. } => json!({ "handler": handler }),
            })
        })
    }
}

#[tokio::test]
async fn invoke_arm_is_routed_in_both_spellings() {
    let booth = r#"{"host":{"type":"invoke","args":{"type":"mudra","key":"w","handler":"run","args":{}}}}"#;
    let reply = serve(booth, &Stub).await;
    assert_eq!(reply, Reply::Ok(json!({ "handler": "run" })).to_line());

    let verb = r#"{"host":{"type":"invoke","verb":"pages","args":{"filter":"x"}}}"#;
    let reply = serve(verb, &Stub).await;
    assert_eq!(
        reply,
        Reply::Ok(json!({ "verb": "pages", "args": { "filter": "x" } })).to_line()
    );
}

#[tokio::test]
async fn unimplemented_arms_fail_explicitly() {
    // A guest asking for an arm the host lacks gets an error value back —
    // never a silent Null.
    for frame in [
        r#"{"host":{"type":"store","op":{}}}"#,
        r#"{"host":{"type":"timer.register","args":{"at_ms":1,"tag":"t"}}}"#,
        r#"{"host":{"type":"queue.depth","args":"e"}}"#,
        r#"{"host":{"type":"schema"}}"#,
        r#"{"host":{"type":"iterate.start","args":{}}}"#,
    ] {
        let reply = serve(frame, &Stub).await;
        assert!(
            reply.contains(r#""error":"arm `"#),
            "must be an explicit refusal: {reply}"
        );
    }
}

#[tokio::test]
async fn emit_carries_no_reply_and_undecodable_frames_are_surfaced() {
    assert_eq!(
        serve(r#"{"host":{"type":"emit","args":{"event":"e","data":{}}}}"#, &Stub).await,
        String::new()
    );
    assert_eq!(serve("not a frame", &Stub).await, String::new());
    // a delivered call's answer is a Result frame: the host consumes it
    assert_eq!(
        serve(r#"{"id":3,"result":{"n":1}}"#, &Stub).await,
        String::new()
    );
}

// ========================= live child =========================

/// The fixture guest: a bin of THIS package, so cargo links it for these
/// integration tests and exports its path — no manual build step, and the test
/// can never run against a stale fixture. (`raw_guest` is written straight
/// against induction-protocol; the ergonomic guest face is proven by
/// `induction-guest`'s pair test.)
fn fixture_guest() -> &'static str {
    env!("CARGO_BIN_EXE_raw_guest")
}

#[tokio::test]
async fn a_real_child_handshakes_and_answers() {
    let spec = Spec::new("loop", [fixture_guest().to_string()]);
    let mut process = spawn(&spec).expect("spawn the extension");
    let budgets = Budgets {
        handshake: Duration::from_secs(5),
        write: Duration::from_secs(5),
        read: Duration::from_secs(5),
    };

    let hello = process
        .session
        .handshake(budgets.handshake)
        .await
        .expect("a live child answers the handshake");
    assert_eq!(hello.name, "loop", "the guest declares its own name");
    assert!(hello.schema.receives("echo"));
    assert!(hello.schema.hooks.contains(&"__wake".to_string()));

    // one real round trip: the request into the child, its answer back out
    assert!(
        process
            .session
            .writer
            .write_line(&CallFrame::call(1, "echo", json!({"n": 1})).to_line(), budgets.write)
            .await
    );
    let line = process
        .session
        .reader
        .next_line(budgets.read)
        .await
        .expect("result line");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert_eq!(r.result, json!({ "echo": { "n": 1 } })),
        other => panic!("expected a result frame, got {other:?}"),
    }

    // a failing handler rides back as an error VALUE, not a broken session
    assert!(
        process
            .session
            .writer
            .write_line(&CallFrame::call(2, "boom", json!(null)).to_line(), budgets.write)
            .await
    );
    let line = process
        .session
        .reader
        .next_line(budgets.read)
        .await
        .expect("error result line");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => assert!(
            r.result.as_str().unwrap_or_default().contains("deliberate failure"),
            "got {r:?}"
        ),
        other => panic!("expected a result frame, got {other:?}"),
    }

    // the ctx arm crosses the process boundary: the child asks, the host answers
    assert!(
        process
            .session
            .writer
            .write_line(&CallFrame::call(3, "ask_host", json!(null)).to_line(), budgets.write)
            .await
    );
    let line = process
        .session
        .reader
        .next_line(budgets.read)
        .await
        .expect("the child's ctx frame");
    let reply = induction_host::route_line(&Stub, &line).await;
    match reply {
        induction_host::Routed::Reply(r) => {
            assert!(r.to_value().to_string().contains(r#""handler":"pages""#), "got {r:?}");
            assert!(
                process
                    .session
                    .writer
                    .write_line(&r.to_line(), budgets.write)
                    .await
            );
        }
        other => panic!("expected a host arm, got {other:?}"),
    }
    let line = process
        .session
        .reader
        .next_line(budgets.read)
        .await
        .expect("the child's answer after the host_reply");
    match parse_guest_line(&line) {
        Some(GuestFrame::Result(r)) => {
            assert_eq!(r.id, json!(3));
            assert_eq!(r.result["from_host"]["handler"], "pages");
        }
        other => panic!("expected a result frame, got {other:?}"),
    }

    // iterate: start opens the stream, next pulls rounds, exhaustion is terminal
    assert!(
        process
            .session
            .writer
            .write_line(
                &CallFrame::iterate(4, FrameKind::IterateStart, "list", json!(null), "s1").to_line(),
                budgets.write
            )
            .await
    );
    let line = process.session.reader.next_line(budgets.read).await.expect("start");
    assert!(line.contains(r#""done":false"#), "got {line}");

    process.kill();
}

#[tokio::test]
async fn a_driver_run_ends_when_the_event_source_closes() {
    let (host_side, guest_side) = tokio::io::duplex(1024);
    tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(guest_side);
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        let _ = reader.read_line(&mut line).await;
        let _ = w
            .write_all(b"{\"type\":\"hello\",\"name\":\"fake\",\"schema\":{\"receives\":{}}}\n")
            .await;
        // hold the channel open so the driver's outcome is the event source
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let (r, w) = tokio::io::split(host_side);
    let mut session = induction_host::Session::new(r, w);
    let budgets = Budgets::default();
    assert!(session.handshake(budgets.handshake).await.is_some());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallFrame>(1);
    drop(tx); // the host's event source is gone
    let outcome = run_driver(&mut session, &Stub, &mut rx, budgets).await;
    assert_eq!(outcome, Outcome::ShuttingDown);
}

#[tokio::test]
async fn a_silent_guest_is_not_a_dead_guest() {
    // An extension with nothing to say sends no frames; a read budget must not
    // turn that silence into an outcome (it would respawn a healthy extension
    // once per window).
    let (host_side, guest_side) = tokio::io::duplex(1024);
    let guest = tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(guest_side);
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        let _ = reader.read_line(&mut line).await;
        let _ = w
            .write_all(b"{\"type\":\"hello\",\"name\":\"quiet\",\"schema\":{\"receives\":{}}}\n")
            .await;
        // silent, channel held open: alive, with nothing to say
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(w); // only now does it go away
    });

    let (r, w) = tokio::io::split(host_side);
    let mut session = induction_host::Session::new(r, w);
    let budgets = Budgets {
        handshake: Duration::from_secs(1),
        write: Duration::from_millis(50),
        read: Duration::from_millis(50), // deliberately far below the silence
    };
    assert!(session.handshake(budgets.handshake).await.is_some());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallFrame>(1);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            run_driver(&mut session, &Stub, &mut rx, budgets)
        )
        .await
        .is_err(),
        "a quiet guest must not be reported as gone"
    );

    // the guest really goes away; now — and only now — the driver ends
    guest.await.unwrap();
    assert_eq!(
        run_driver(&mut session, &Stub, &mut rx, budgets).await,
        Outcome::GuestGone
    );
    drop(tx);
}
