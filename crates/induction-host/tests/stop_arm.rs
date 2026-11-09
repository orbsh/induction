//! The stop arm (aura ADR-0047 stage one): `ctx.stop.poll` is a bit the host
//! answers, and it is an ARM rather than an event precisely because a delivery
//! addressed to a busy instance queues behind the work it means to stop.
//!
//! These tests lock the vocabulary and the routing. The interesting part is the
//! negative: a host that has no stop bits must SAY SO instead of answering
//! `false`, because a false "no" is a stop that silently never lands.

use induction_host::{route_line_tagged, HostOps, InvokeCall, OpsFuture, Routed};
use induction_protocol::{Arm, Reply};
use serde_json::{Value, json};

/// A host whose stop bit is whatever the test set it to.
struct Bit(bool);

impl HostOps for Bit {
    fn invoke<'a>(&'a self, _call: &'a InvokeCall) -> OpsFuture<'a> {
        Box::pin(async move { Ok(Value::Null) })
    }

    fn stop_poll<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        let answer = self.0;
        Box::pin(async move { Ok(json!(answer)) })
    }

    fn stop_clear<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        Box::pin(async move { Ok(Value::Null) })
    }
}

/// A host that implements nothing but `invoke`.
struct Verbs;

impl HostOps for Verbs {
    fn invoke<'a>(&'a self, _call: &'a InvokeCall) -> OpsFuture<'a> {
        Box::pin(async move { Ok(Value::Null) })
    }
}

#[test]
fn the_arm_is_in_the_vocabulary_with_its_dotted_name() {
    assert_eq!(Arm::parse("stop.poll"), Some(Arm::StopPoll));
    assert_eq!(Arm::parse("stop.clear"), Some(Arm::StopClear));
    assert_eq!(Arm::StopPoll.as_str(), "stop.poll");
    assert_eq!(
        Arm::ALL.len(),
        13,
        "the arm set is enumerated: adding one is a protocol decision, not a name"
    );
    assert!(Arm::StopPoll.has_reply(), "a bit has to come back as a value");
}

#[tokio::test]
async fn the_host_answers_the_bit_it_holds() {
    let line = json!({ "host": { "type": "stop.poll", "id": 7, "args": Value::Null } }).to_string();

    let (id, routed) = route_line_tagged(&Bit(true), &line).await;
    assert_eq!(id, Some(7), "the correlation id rides back");
    assert_eq!(routed, Routed::Reply(Reply::ok(json!(true))));

    let (_, routed) = route_line_tagged(&Bit(false), &line).await;
    assert_eq!(routed, Routed::Reply(Reply::ok(json!(false))));
}

#[tokio::test]
async fn a_host_without_stop_bits_says_so_instead_of_answering_no() {
    let line = json!({ "host": { "type": "stop.poll", "id": 1, "args": Value::Null } }).to_string();
    match route_line_tagged(&Verbs, &line).await.1 {
        Routed::Reply(Reply::Error(message)) => {
            assert!(
                message.contains("stop.poll"),
                "the refusal names the arm: {message}"
            );
        }
        other => panic!("an unimplemented arm must refuse, not answer {other:?}"),
    }
}