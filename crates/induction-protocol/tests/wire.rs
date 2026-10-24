//! Wire-contract tests. The decode/shape half of the contract is testable
//! without a pipe — that is exactly what this file is for (the loop itself is
//! covered in `induction-host`'s session tests).
//!
//! Migrated from mudra's `host_test.rs` (the hello-decode and subscription
//! cases were its baseline) and extended to the shapes both shipped
//! implementations put on the wire.

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

use induction_protocol::{
    Arm, CallFrame, Encoding, FrameKind, GuestFrame, Hello, HostFrame, InvokeCall, IterateKind,
    PROTOCOL, Reply, ResultFrame, SCHEMA_EVENT, Schema, initialize_line, parse_guest_line,
    parse_host_reply,
};

// ======================== handshake ========================

#[test]
fn initialize_declares_the_protocol_revision() {
    let v: serde_json::Value = serde_json::from_str(&initialize_line()).unwrap();
    assert_eq!(v["type"], "initialize");
    assert_eq!(v["protocol"], PROTOCOL);
}

#[test]
fn hello_carries_name_and_the_receives_set() {
    let h = Hello::parse_line(
        r#"{"type":"hello","name":"k10r-sync","schema":{"receives":{"mudra:page_open":{},"mudra:tag_set":{}},"wildcard_receives":["mudra:*"]}}"#,
    )
    .expect("valid hello");
    assert_eq!(h.name, "k10r-sync");
    assert!(h.schema.receives("mudra:page_open"));
    assert!(h.schema.receives("mudra:tag_set"));
    assert_eq!(h.schema.wildcard_receives, vec!["mudra:*".to_string()]);
}

#[test]
fn hello_accepts_the_flat_on_spelling() {
    // The pre-Induction spelling mudra's host writes today: `schema.on` as a
    // flat list. Accepted on decode so the migration window does not break.
    let h = Hello::parse_line(
        r#"{"type":"hello","name":"k10r-sync","schema":{"on":["mudra:page_open","mudra:tag_set"]}}"#,
    )
    .expect("flat hello");
    assert_eq!(h.schema.on().len(), 2);
    assert!(h.schema.receives("mudra:page_open"));
}

#[test]
fn bad_hello_shape_rejects_at_decode() {
    // Decode failure IS the version/contract-mismatch signal — never a
    // partial accept: wrong type, missing schema, missing name, junk.
    for line in [
        r#"{"type":"call","name":"x","schema":{"on":[]}}"#,
        r#"{"type":"hello","name":"x"}"#,         // no schema
        r#"{"type":"hello","schema":{"on":[]}}"#, // no name
        r#"{"type":"hello","name":7,"schema":{"on":[]}}"#, // wrong type
        r#"{"type":"hello","name":"x","schema":{"on":[7]}}"#, // bad element
        "not json",
    ] {
        assert!(Hello::parse_line(line).is_none(), "must reject: {line}");
    }
    // an EMPTY declaration is a valid hello (listens to nothing; the shape is
    // what decode checks)
    assert!(Hello::parse_line(r#"{"type":"hello","name":"x","schema":{"on":[]}}"#).is_some());
}

// ====================== request frames ======================

#[test]
fn call_frame_shape_is_the_shipped_one() {
    let f = CallFrame::call(7, "mudra:page_open", serde_json::json!({"event_id":7}));
    let v: serde_json::Value = serde_json::from_str(&f.to_line()).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["kind"], "call");
    assert_eq!(v["event"], "mudra:page_open");
    assert!(v.get("op").is_none(), "a plain call carries no op tag");
    assert!(v.get("stream_id").is_none());
}

#[test]
fn iterate_frames_carry_kind_op_and_stream_id() {
    // The shipped host writes `op` beside `kind`; JSON itself compares equal
    // regardless of key order, so this asserts the set, not the order.
    for (kind, tag) in [
        (IterateKind::Start, "start"),
        (IterateKind::Next, "next"),
        (IterateKind::Dispose, "dispose"),
    ] {
        let f = CallFrame::iterate(1, kind, "list_pages", serde_json::json!(null), "s1");
        let v: serde_json::Value = serde_json::from_str(&f.to_line()).unwrap();
        assert_eq!(v["kind"], kind.as_str());
        assert_eq!(v["op"], tag);
        assert_eq!(v["stream_id"], "s1");
    }
}

#[test]
fn iterate_kind_converts_both_ways_and_rejects_call() {
    // The inbound direction is what a carrier holding a decoded `FrameKind`
    // needs (effector maps its own `StreamOp` through it); the outbound one is
    // what `CallFrame::iterate` uses. `Call` is a decode-failure-free value
    // that still must not become a stream kind — it comes back as the error.
    for kind in IterateKind::ALL {
        let frame_kind = FrameKind::from(kind);
        assert!(IterateKind::try_from(frame_kind).is_ok());
        assert_eq!(frame_kind.as_str(), kind.as_str());
        assert!(frame_kind.op_tag().is_some());
    }
    assert_eq!(IterateKind::try_from(FrameKind::Call), Err(FrameKind::Call));
}

#[test]
fn request_frames_round_trip_and_reject_junk() {
    let f = CallFrame::iterate(
        3,
        IterateKind::Next,
        "list_pages",
        serde_json::json!({}),
        "s1",
    );
    assert_eq!(CallFrame::from_value(&f.to_value()), Some(f));
    assert_eq!(
        CallFrame::from_value(&serde_json::json!({"kind":"nope","event":"e"})),
        None
    );
    assert_eq!(
        CallFrame::from_value(&serde_json::json!({"kind":"call"})),
        None
    );
}

// ============================ arms ============================

#[test]
fn arm_vocabulary_is_the_frozen_adr_0043_set() {
    // Locked deliberately: one vocabulary across hosts, no synonyms.
    let want = [
        "invoke",
        "iterate.start",
        "iterate.next",
        "iterate.dispose",
        "store",
        "emit",
        "timer.register",
        "timer.cancel",
        "queue.depth",
        "queue.skip_to_head",
        "schema",
    ];
    assert_eq!(Arm::ALL.len(), want.len());
    for (arm, name) in Arm::ALL.into_iter().zip(want) {
        assert_eq!(arm.as_str(), name);
        assert_eq!(Arm::parse(name), Some(arm));
    }
    assert_eq!(Arm::parse("invoke_verb"), None);
}

#[test]
fn only_emit_carries_no_reply() {
    for arm in Arm::ALL {
        assert_eq!(arm.has_reply(), arm != Arm::Emit, "arm {arm:?}");
    }
}

#[test]
fn host_frames_encode_the_shipped_shapes() {
    let invoke = HostFrame::new(
        Arm::Invoke,
        serde_json::json!({"type":"mudra","key":"","handler":"run","args":{}}),
    );
    let v: serde_json::Value = serde_json::from_str(&invoke.to_line()).unwrap();
    assert_eq!(v["host"]["type"], "invoke");
    assert_eq!(v["host"]["args"]["handler"], "run");

    // store: the op rides beside `type`, never under `args` (schema-blind rule)
    let store = HostFrame::store(serde_json::json!({"get": {"collection":"pages"}}));
    let v: serde_json::Value = serde_json::from_str(&store.to_line()).unwrap();
    assert_eq!(v["host"]["type"], "store");
    assert!(v["host"]["op"].is_object());
    assert!(v["host"].get("args").is_none());

    // schema: no args at all
    let schema = HostFrame::new(Arm::Schema, serde_json::Value::Null);
    let v: serde_json::Value = serde_json::from_str(&schema.to_line()).unwrap();
    assert_eq!(v["host"]["type"], "schema");
    assert!(v["host"].get("args").is_none());
}

#[test]
fn invoke_payload_decodes_both_shipped_spellings() {
    // canonical: the nested booth call (ADR-0043 §5)
    let booth: serde_json::Value = serde_json::json!({
        "host": {"type":"invoke","args":{"type":"mudra","key":"w","handler":"run","args":{"v":1}}}
    });
    let h = HostFrame::from_value(&booth).unwrap();
    assert_eq!(
        InvokeCall::from_host(&h.raw),
        Some(InvokeCall::Booth {
            target_type: "mudra".into(),
            target_key: "w".into(),
            handler: "run".into(),
            args: serde_json::json!({"v":1}),
        })
    );

    // pre-Induction: mudra's flat `verb` beside `type`
    let verb: serde_json::Value =
        serde_json::json!({"host":{"type":"invoke","verb":"pages","args":{"filter":"x"}}});
    let h = HostFrame::from_value(&verb).unwrap();
    assert_eq!(
        InvokeCall::from_host(&h.raw),
        Some(InvokeCall::Verb {
            verb: "pages".into(),
            args: serde_json::json!({"filter":"x"}),
        })
    );

    assert_eq!(
        InvokeCall::from_host(&serde_json::json!({"type":"invoke"})),
        None
    );
}

#[test]
fn unknown_arm_is_a_decode_failure() {
    let v = serde_json::json!({"host": {"type": "queue.peek"}});
    assert_eq!(HostFrame::from_value(&v), None);
}

// =========================== replies ===========================

#[test]
fn reply_shapes_and_the_error_key() {
    assert_eq!(
        parse_host_reply(r#"{"host_reply":{"ok":{"n":1}}}"#),
        Some(Reply::Ok(serde_json::json!({"n":1})))
    );
    // canonical error key
    assert_eq!(
        parse_host_reply(r#"{"host_reply":{"error":"boom"}}"#),
        Some(Reply::Error("boom".into()))
    );
    // pre-Induction `err` spelling is tolerated on decode
    assert_eq!(
        parse_host_reply(r#"{"host_reply":{"err":"boom"}}"#),
        Some(Reply::Error("boom".into()))
    );
    // neither arm: rejected — a swallowed Null hid the parent's failure
    assert_eq!(parse_host_reply(r#"{"host_reply":{}}"#), None);
    assert_eq!(parse_host_reply(r#"{"result":1}"#), None);
    // canonical encode always writes `error`
    assert_eq!(
        Reply::error("boom").to_line(),
        r#"{"host_reply":{"error":"boom"}}"#
    );
    // the Value form is the same shape (a CBOR host writes this)
    assert_eq!(
        Reply::ok(serde_json::json!({"n":1})).to_value(),
        serde_json::json!({"host_reply": {"ok": {"n": 1}}})
    );
    assert_eq!(
        Reply::error("boom").to_value(),
        serde_json::json!({"host_reply": {"error": "boom"}})
    );
}

// ====================== guest-side lines ======================

#[test]
fn guest_lines_decode_to_host_or_result() {
    assert!(matches!(
        parse_guest_line(r#"{"host":{"type":"schema"}}"#),
        Some(GuestFrame::Host(_))
    ));
    let rf = ResultFrame::new(serde_json::json!(5), serde_json::json!({"ok":true}));
    assert_eq!(
        parse_guest_line(&rf.to_line()),
        Some(GuestFrame::Result(ResultFrame {
            id: serde_json::json!(5),
            result: serde_json::json!({"ok":true}),
        }))
    );
    assert_eq!(parse_guest_line("garbage"), None);
    assert_eq!(parse_guest_line(r#"{"kind":"call"}"#), None);
}

// =========================== schema surface ===========================

#[test]
fn schema_event_name_is_frozen() {
    assert_eq!(SCHEMA_EVENT, "interface_schema");
    let f = CallFrame::call(1, SCHEMA_EVENT, serde_json::Value::Null);
    assert_eq!(f.event, SCHEMA_EVENT);
}

#[test]
fn schema_rich_form_round_trips_and_subscription_is_exact() {
    let s = Schema {
        receives: [("a:b".to_string(), serde_json::json!({}))]
            .into_iter()
            .collect(),
        wildcard_receives: vec!["a:*".into()],
        hooks: vec!["__wake".into()],
        storage: Some(serde_json::json!({"collections": {}})),
        auth: Some(serde_json::json!({"a:b": true})),
    };
    let back = Schema::from_value(&s.to_value()).unwrap();
    assert_eq!(back, s);
    // exact membership only: a wildcard pattern is NOT a match
    assert!(back.receives("a:b"));
    assert!(!back.receives("a:c"));
    assert_eq!(back.wildcard_receives, vec!["a:*".to_string()]);
    // storage is the ctx.store plan source, carried verbatim
    assert!(back.storage.is_some());
}

#[test]
fn encoding_declaration_is_wire_visible() {
    // The guest selects its codec from the env var only — never from argv.
    assert_eq!(Encoding::from_env(), Encoding::Json); // unset in the test env
    assert_eq!(Encoding::Cbor.as_str(), "cbor");
    // …and it is a SERIALIZED declaration: hosts carry it in their own
    // control frames (effector's ToolCall, aura's persisted booth record),
    // so the serde spelling is frozen here — one enum, no second copy.
    assert_eq!(
        serde_json::to_value(Encoding::Json).unwrap(),
        serde_json::json!("json")
    );
    assert_eq!(
        serde_json::to_value(Encoding::Cbor).unwrap(),
        serde_json::json!("cbor")
    );
    assert_eq!(
        serde_json::from_value::<Encoding>(serde_json::json!("cbor")).unwrap(),
        Encoding::Cbor
    );
}
