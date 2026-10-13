//! The ctx arms a guest invokes back on its host.
//!
//! The protocol fixes the arm NAMES and their frame shapes; the host supplies
//! the implementations. [`HostOps`] therefore has one required method
//! ([`HostOps::invoke`] — the arm every real host can serve) and defaults for
//! the rest that fail *explicitly*: a guest asking for an arm its host does not
//! implement gets an error value back, never a silent Null.

use std::future::Future;
use std::pin::Pin;

use induction_protocol::{Arm, GuestFrame, InvokeCall, Reply, parse_guest_line};
use serde_json::Value;

/// A boxed, sendable future — keeps `HostOps` object-safe.
pub type OpsFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// What the host can do for the guests it hosts.
///
/// Arms mirror the aura ctx members (ADR-0043 §5) one-for-one. A host that
/// serves only verbs implements [`HostOps::invoke`] and nothing else — that is
/// the minimal host, and it is a complete one: `emit`/`store`/`timer`/`queue`
/// are capabilities a host may simply not have.
pub trait HostOps: Send + Sync {
    /// `ctx.invoke` — route one host call. Either spelling may arrive (see
    /// [`InvokeCall`]); a host refuses the shape it cannot serve with an error
    /// string, which rides back to the guest as a failed call.
    fn invoke<'a>(&'a self, call: &'a InvokeCall) -> OpsFuture<'a>;

    /// `ctx.emit` — fire-and-forget by contract: NO reply frame goes back
    /// (the protocol says so, not the host). A publish failure belongs in the
    /// host's log; sending it as an error value into a guest that stopped
    /// listening is the failure mode this arm exists to avoid.
    fn emit(&self, event: &str, _data: &Value) -> Result<(), String> {
        Err(format!(
            "arm `emit` is not implemented by this host (event `{event}` dropped)"
        ))
    }

    /// `ctx.store` — one okm Collection instruction as data.
    fn store<'a>(&'a self, _op: &'a Value) -> OpsFuture<'a> {
        declined("store")
    }

    /// `ctx.iterate.*` — stream arms (producer guard table lives guest-side).
    fn iterate<'a>(&'a self, arm: Arm, _args: &'a Value) -> OpsFuture<'a> {
        declined(arm.as_str())
    }

    /// `ctx.timer.register` — host-owned timers; mudra has none today.
    fn timer_register<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        declined("timer.register")
    }

    fn timer_cancel<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        declined("timer.cancel")
    }

    /// `ctx.queue.depth` — the backlog point-read (a queue host's answer).
    fn queue_depth<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        declined("queue.depth")
    }

    fn queue_skip_to_head<'a>(&'a self, _args: &'a Value) -> OpsFuture<'a> {
        declined("queue.skip_to_head")
    }

    /// `ctx.schema` — the persisted declaration copy.
    fn schema<'a>(&'a self) -> OpsFuture<'a> {
        declined("schema")
    }
}

fn declined<'a>(arm: &'static str) -> OpsFuture<'a> {
    Box::pin(async move { Err(format!("arm `{arm}` is not implemented by this host")) })
}

/// What routing one guest frame produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    /// A ctx arm was served; write this reply back. The host shapes it for
    /// its own codec — JSON-lines hosts `to_line()`, CBOR hosts `to_value()`
    /// — the arm dispatch itself is codec-blind (one core for every
    /// transport, below).
    Reply(Reply),
    /// The arm carries no reply (`emit`) — nothing to write.
    Accepted,
    /// A request answer (`result` frame) — logged or consumed by the host.
    Result(Value),
    /// Not a decodable frame: the version-mismatch signal. Never absorbed
    /// silently — the caller decides whether to log it or end the session.
    DecodeFailed,
}

/// Serve one decoded guest frame: route the arm to `ops`, hand back the
/// reply when the arm carries one. This is the codec-independent core —
/// a JSON-lines host drives it through [`route_line`], a CBOR host decodes
/// its documents to `Value`s and drives it directly.
pub async fn route_frame(ops: &dyn HostOps, frame: GuestFrame) -> Routed {
    let host = match frame {
        GuestFrame::Result(r) => return Routed::Result(r.result),
        GuestFrame::Host(h) => h,
    };
    match host.arm {
        Arm::Emit => {
            let event = host
                .args
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let data = host.args.get("data").cloned().unwrap_or(Value::Null);
            match ops.emit(&event, &data) {
                Ok(()) => Routed::Accepted,
                Err(e) => {
                    eprintln!("[induction] emit arm failed: {e}");
                    Routed::Accepted
                }
            }
        }
        Arm::Invoke => match InvokeCall::from_host(&host.raw) {
            Some(call) => Routed::Reply(ops.invoke(&call).await.into_reply()),
            None => Routed::Reply(Reply::error("invoke arm: undecodable payload")),
        },
        Arm::Store => Routed::Reply(ops.store(host.op.as_ref().unwrap_or(&Value::Null)).await.into_reply()),
        Arm::IterateStart | Arm::IterateNext | Arm::IterateDispose => {
            Routed::Reply(ops.iterate(host.arm, &host.args).await.into_reply())
        }
        Arm::TimerRegister => Routed::Reply(ops.timer_register(&host.args).await.into_reply()),
        Arm::TimerCancel => Routed::Reply(ops.timer_cancel(&host.args).await.into_reply()),
        Arm::QueueDepth => Routed::Reply(ops.queue_depth(&host.args).await.into_reply()),
        Arm::QueueSkipToHead => {
            Routed::Reply(ops.queue_skip_to_head(&host.args).await.into_reply())
        }
        Arm::Schema => Routed::Reply(ops.schema().await.into_reply()),
    }
}

/// Serve one guest line: decode it, route the arm to `ops`, and hand back the
/// reply when the arm carries one. The JSON-lines form of [`route_frame`].
pub async fn route_line(ops: &dyn HostOps, line: &str) -> Routed {
    match parse_guest_line(line) {
        Some(frame) => route_frame(ops, frame).await,
        None => Routed::DecodeFailed,
    }
}

trait IntoReply {
    fn into_reply(self) -> Reply;
}

impl IntoReply for Result<Value, String> {
    fn into_reply(self) -> Reply {
        match self {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Error(e),
        }
    }
}