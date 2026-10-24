//! Frames: what the host sends, what the guest answers, and the ctx arms the
//! guest invokes back.
//!
//! Every constructor here produces the exact shape the shipped implementations
//! put on the pipe; every parser accepts the shipped spellings (including the
//! pre-Induction ones, marked as such) and rejects shape violations.

use serde_json::{Value, json};

// ---------------------------------------------------------------- kinds --

/// A parent → child request kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// One request, one answer.
    Call,
    /// Open a stream: run the handler as a producer under `stream_id`.
    IterateStart,
    /// Pull one envelope (`{item, done:false}` / `{done:true, value?}`).
    IterateNext,
    /// Abandon the stream (idempotent).
    IterateDispose,
}

impl FrameKind {
    pub const ALL: [FrameKind; 4] = [
        FrameKind::Call,
        FrameKind::IterateStart,
        FrameKind::IterateNext,
        FrameKind::IterateDispose,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Call => "call",
            FrameKind::IterateStart => "iterate_start",
            FrameKind::IterateNext => "iterate_next",
            FrameKind::IterateDispose => "iterate_dispose",
        }
    }

    /// The `op` tag written beside `kind` for the iterate kinds (the shipped
    /// host writes both fields; `kind` is what a guest dispatches on).
    pub fn op_tag(self) -> Option<&'static str> {
        match self {
            FrameKind::Call => None,
            FrameKind::IterateStart => Some("start"),
            FrameKind::IterateNext => Some("next"),
            FrameKind::IterateDispose => Some("dispose"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        FrameKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// The three stream kinds — every [`FrameKind`] except [`FrameKind::Call`].
///
/// A separate type because [`CallFrame::iterate`] can do nothing sensible with
/// `Call`: the frame it would build carries a `stream_id` and an `op` tag that
/// the peer has no route for. So the kind is part of the constructor's TYPE
/// rather than a runtime assertion. The `debug_assert!` this replaces was the
/// wrong shape twice over — it disappears in a release build, and the failure
/// it guarded (a malformed frame reaching the peer) is worse than the crash it
/// would have prevented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterateKind {
    /// Open a stream.
    Start,
    /// Pull one envelope.
    Next,
    /// Abandon the stream (idempotent).
    Dispose,
}

impl IterateKind {
    pub const ALL: [IterateKind; 3] = [IterateKind::Start, IterateKind::Next, IterateKind::Dispose];

    pub fn as_str(self) -> &'static str {
        FrameKind::from(self).as_str()
    }
}

impl From<IterateKind> for FrameKind {
    fn from(kind: IterateKind) -> Self {
        match kind {
            IterateKind::Start => FrameKind::IterateStart,
            IterateKind::Next => FrameKind::IterateNext,
            IterateKind::Dispose => FrameKind::IterateDispose,
        }
    }
}

/// The fallible direction, for a consumer holding a decoded [`FrameKind`]:
/// `Call` is not a stream kind, and the error hands back the offending kind so
/// the caller can name it.
impl TryFrom<FrameKind> for IterateKind {
    type Error = FrameKind;

    fn try_from(kind: FrameKind) -> Result<Self, Self::Error> {
        match kind {
            FrameKind::IterateStart => Ok(IterateKind::Start),
            FrameKind::IterateNext => Ok(IterateKind::Next),
            FrameKind::IterateDispose => Ok(IterateKind::Dispose),
            FrameKind::Call => Err(FrameKind::Call),
        }
    }
}

// ------------------------------------------------------------ call frame --

/// A parent → child request.
#[derive(Debug, Clone, PartialEq)]
pub struct CallFrame {
    pub id: u64,
    pub kind: FrameKind,
    pub event: String,
    pub args: Value,
    pub stream_id: Option<String>,
}

impl CallFrame {
    /// A plain request (`kind: "call"`).
    pub fn call(id: u64, event: impl Into<String>, args: Value) -> Self {
        Self {
            id,
            kind: FrameKind::Call,
            event: event.into(),
            args,
            stream_id: None,
        }
    }

    /// A stream request. The kind is an iterate kind BY TYPE ([`IterateKind`]),
    /// so no caller can build a `call`-shaped frame that carries a stream id.
    pub fn iterate(
        id: u64,
        kind: IterateKind,
        event: impl Into<String>,
        args: Value,
        stream_id: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind: kind.into(),
            event: event.into(),
            args,
            stream_id: Some(stream_id.into()),
        }
    }

    /// The frame as a value. Built field by field rather than assembled and
    /// then mutated by key: `Value`'s indexing operator is a panicking API, and
    /// a wire constructor is not the place to rely on "this literal happens to
    /// be an object".
    pub fn to_value(&self) -> Value {
        let mut v = serde_json::Map::new();
        v.insert("id".into(), Value::from(self.id));
        v.insert("kind".into(), Value::from(self.kind.as_str()));
        v.insert("event".into(), Value::from(self.event.as_str()));
        v.insert("args".into(), self.args.clone());
        if let Some(op) = self.kind.op_tag() {
            v.insert("op".into(), Value::from(op));
        }
        if let Some(s) = &self.stream_id {
            v.insert("stream_id".into(), Value::from(s.as_str()));
        }
        Value::Object(v)
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    /// Decode a request frame. `None` on a missing kind/event or an unknown
    /// kind — decode failure, not a defaulted dispatch.
    pub fn from_value(v: &Value) -> Option<Self> {
        let kind = FrameKind::parse(v.get("kind")?.as_str()?)?;
        let event = v.get("event")?.as_str()?.to_string();
        Some(Self {
            id: v.get("id").and_then(Value::as_u64).unwrap_or(0),
            kind,
            event,
            args: v.get("args").cloned().unwrap_or(Value::Null),
            stream_id: v
                .get("stream_id")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        })
    }
}

// ------------------------------------------------------------------ arms --

/// The ctx arms a guest can invoke back on its host: the CALLABLE members of
/// the aura ctx object (ADR-0043 §5), as dotted method paths. One vocabulary
/// across hosts — adopt these names verbatim rather than minting synonyms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    Invoke,
    IterateStart,
    IterateNext,
    IterateDispose,
    Store,
    Emit,
    TimerRegister,
    TimerCancel,
    QueueDepth,
    QueueSkipToHead,
    Schema,
}

impl Arm {
    pub const ALL: [Arm; 11] = [
        Arm::Invoke,
        Arm::IterateStart,
        Arm::IterateNext,
        Arm::IterateDispose,
        Arm::Store,
        Arm::Emit,
        Arm::TimerRegister,
        Arm::TimerCancel,
        Arm::QueueDepth,
        Arm::QueueSkipToHead,
        Arm::Schema,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Invoke => "invoke",
            Arm::IterateStart => "iterate.start",
            Arm::IterateNext => "iterate.next",
            Arm::IterateDispose => "iterate.dispose",
            Arm::Store => "store",
            Arm::Emit => "emit",
            Arm::TimerRegister => "timer.register",
            Arm::TimerCancel => "timer.cancel",
            Arm::QueueDepth => "queue.depth",
            Arm::QueueSkipToHead => "queue.skip_to_head",
            Arm::Schema => "schema",
        }
    }

    /// Does this arm expect a `host_reply`? `emit` is fire-and-forget by
    /// contract: the host answers NOTHING, and a publish failure lands in the
    /// host's log, never as an error value into a guest that stopped listening.
    pub fn has_reply(self) -> bool {
        self != Arm::Emit
    }

    pub fn parse(s: &str) -> Option<Self> {
        Arm::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

/// A child → parent host frame: `{"host": {"type": <arm>, …}}`.
///
/// `op` carries the `store` arm's instruction, which rides beside `type`
/// (not under `args`) — the schema-blind rule: the guest never parses the
/// instruction's semantics. `raw` keeps the original host object so arm
/// payloads that use flat spellings stay decodable.
#[derive(Debug, Clone, PartialEq)]
pub struct HostFrame {
    pub arm: Arm,
    pub args: Value,
    pub op: Option<Value>,
    pub raw: Value,
}

impl HostFrame {
    pub fn new(arm: Arm, args: Value) -> Self {
        let mut host = json!({ "type": arm.as_str(), "args": args });
        if args.is_null() {
            host = json!({ "type": arm.as_str() });
        }
        Self {
            arm,
            args,
            op: None,
            raw: host,
        }
    }

    /// The `store` arm: one okm Collection instruction as data.
    pub fn store(op: Value) -> Self {
        Self {
            arm: Arm::Store,
            args: Value::Null,
            op: Some(op.clone()),
            raw: json!({ "type": "store", "op": op }),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({ "host": self.raw })
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    /// Decode `{"host": …}`. `None` for a non-host value or an unknown arm.
    pub fn from_value(v: &Value) -> Option<Self> {
        let host = v.get("host")?;
        let arm = Arm::parse(host.get("type")?.as_str()?)?;
        Some(Self {
            arm,
            args: host.get("args").cloned().unwrap_or(Value::Null),
            op: host.get("op").cloned(),
            raw: host.clone(),
        })
    }
}

/// The `invoke` arm's payload.
///
/// Canonical form is the accepted ctx definition (ADR-0043 §5): a booth call —
/// a target instance plus a handler plus args. `Verb` is the pre-Induction
/// spelling mudra's host shipped (a flat `verb` beside `type`), decoded here so
/// the migration window does not break a running pair; hosts route whichever
/// shape they can serve.
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeCall {
    Booth {
        target_type: String,
        target_key: String,
        handler: String,
        args: Value,
    },
    Verb {
        verb: String,
        args: Value,
    },
}

impl InvokeCall {
    /// Decode from the host object (`v["host"]`), accepting both shipped
    /// spellings.
    pub fn from_host(host: &Value) -> Option<Self> {
        if let Some(verb) = host.get("verb").and_then(|v| v.as_str()) {
            return Some(InvokeCall::Verb {
                verb: verb.to_string(),
                args: host.get("args").cloned().unwrap_or(Value::Null),
            });
        }
        let args = host.get("args")?;
        let target_type = args.get("type")?.as_str()?.to_string();
        let handler = args.get("handler")?.as_str()?.to_string();
        Some(InvokeCall::Booth {
            target_key: args
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                .to_string(),
            target_type,
            handler,
            args: args.get("args").cloned().unwrap_or(Value::Null),
        })
    }

    /// The canonical encoding: the nested booth shape.
    pub fn to_value(&self) -> Value {
        match self {
            InvokeCall::Booth {
                target_type,
                target_key,
                handler,
                args,
            } => json!({
                "type": "invoke",
                "args": {
                    "type": target_type,
                    "key": target_key,
                    "handler": handler,
                    "args": args,
                }
            }),
            InvokeCall::Verb { verb, args } => json!({
                "type": "invoke",
                "verb": verb,
                "args": args,
            }),
        }
    }
}

// ---------------------------------------------------------------- replies --

/// A host's answer to a ctx arm: `{"host_reply": {"ok": …}}` /
/// `{"host_reply": {"error": …}}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok(Value),
    Error(String),
}

impl Reply {
    pub fn ok(value: Value) -> Self {
        Reply::Ok(value)
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Reply::Error(msg.into())
    }

    /// The frame as a value — what a CBOR host writes (`to_line` is the
    /// JSON-lines form of the same shape).
    pub fn to_value(&self) -> Value {
        match self {
            Reply::Ok(v) => json!({ "host_reply": { "ok": v } }),
            Reply::Error(e) => json!({ "host_reply": { "error": e } }),
        }
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    /// Decode a reply frame. The canonical error key is `error`; the
    /// pre-Induction `err` spelling is accepted on decode so a running
    /// host/guest pair keeps working across the rename. `None` when the value
    /// carries neither — a swallowed Null made a parent's failure invisible.
    pub fn from_value(v: &Value) -> Option<Self> {
        let hr = v.get("host_reply")?;
        if let Some(ok) = hr.get("ok") {
            return Some(Reply::Ok(ok.clone()));
        }
        for key in ["error", "err"] {
            if let Some(e) = hr.get(key) {
                return Some(Reply::Error(match e {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }));
            }
        }
        None
    }
}

/// A guest's answer to one request frame: `{"id":N,"result":…}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultFrame {
    pub id: Value,
    pub result: Value,
}

impl ResultFrame {
    pub fn new(id: Value, result: Value) -> Self {
        Self { id, result }
    }

    pub fn to_value(&self) -> Value {
        json!({ "id": self.id, "result": self.result })
    }

    pub fn to_line(&self) -> String {
        self.to_value().to_string()
    }

    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id").cloned().unwrap_or(Value::Null),
            result: v.get("result")?.clone(),
        })
    }
}

/// Anything the host may read off the child's channel.
#[derive(Debug, Clone, PartialEq)]
pub enum GuestFrame {
    /// A ctx call routed back to the host.
    Host(HostFrame),
    /// A request's answer.
    Result(ResultFrame),
}

/// Decode one guest frame from an already-parsed value (a CBOR host's
/// decoded document, a test's json! literal). `None` = decode failure
/// (version mismatch signal): an unrecognised shape is never silently
/// absorbed.
pub fn parse_guest_value(v: &Value) -> Option<GuestFrame> {
    if let Some(h) = HostFrame::from_value(v) {
        return Some(GuestFrame::Host(h));
    }
    ResultFrame::from_value(v).map(GuestFrame::Result)
}

/// Decode one guest line. `None` = decode failure (version mismatch signal):
/// an unrecognised shape is never silently absorbed.
pub fn parse_guest_line(line: &str) -> Option<GuestFrame> {
    let v: Value = serde_json::from_str(line).ok()?;
    parse_guest_value(&v)
}

/// Decode a `host_reply` line, if that is what it is.
pub fn parse_host_reply(line: &str) -> Option<Reply> {
    let v: Value = serde_json::from_str(line).ok()?;
    Reply::from_value(&v)
}
