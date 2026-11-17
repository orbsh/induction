//! The resource arm (ADR-0003 §7): the guest asks by `{op, kind, remote}` and the host answers a
//! locator. Two cases, and the second one is the interesting half — a host that cannot serve the
//! arm must SAY SO rather than answer something that looks like a path.

use induction_host::{InvokeCall, HostOps, OpsFuture, Routed, route_line};
use induction_protocol::Reply;
use serde_json::{Value, json};

/// A host that serves resources and nothing else.
struct Grants;

impl HostOps for Grants {
    fn invoke<'a>(&'a self, _call: &'a InvokeCall) -> OpsFuture<'a> {
        Box::pin(async move { Ok(Value::Null) })
    }

    fn resource<'a>(&'a self, op: &'a Value) -> OpsFuture<'a> {
        Box::pin(async move {
            let remote = op.get("remote").and_then(Value::as_str).unwrap_or_default();
            Ok(json!({ "path": format!("/work/resources/{remote}") }))
        })
    }
}

/// A host with no resource capability at all.
struct Verbs;

impl HostOps for Verbs {
    fn invoke<'a>(&'a self, _call: &'a InvokeCall) -> OpsFuture<'a> {
        Box::pin(async move { Ok(Value::Null) })
    }
}

#[tokio::test]
async fn the_arm_reaches_the_host_and_its_answer_goes_back() {
    let line = json!({
        "host": {
            "type": "resource",
            "id": 9,
            "op": { "op": "get", "kind": "git", "remote": "org/repo" }
        }
    })
    .to_string();

    match route_line(&Grants, &line).await {
        Routed::Reply(r) => assert_eq!(
            r,
            Reply::ok(json!({ "path": "/work/resources/org/repo" })),
            "the host's locator is the answer, decoded for the guest"
        ),
        other => panic!("expected a reply, got {other:?}"),
    }
}

#[tokio::test]
async fn a_host_without_the_capability_refuses_in_words() {
    let line = json!({
        "host": {
            "type": "resource",
            "id": 10,
            "op": { "op": "get", "kind": "git", "remote": "org/repo" }
        }
    })
    .to_string();

    match route_line(&Verbs, &line).await {
        Routed::Reply(r) => {
            let text = r.to_value().to_string();
            assert!(
                text.contains("not implemented") || text.contains("resource"),
                "a declining default is a refusal the guest can read: {text}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}