//! # induction-guest
//!
//! The Rust user face for an Induction extension: a typed `ctx` facade over the
//! frame protocol. An extension links this crate and implements its handlers as
//! plain `fn(&mut Ctx, Value) -> anyhow::Result<Value>`; the shim owns
//! everything else:
//!
//! - the main loop (JSON-lines or CBOR, chosen from [`ENCODING_ENV`] — the
//!   parent's declared codec crosses as an env, never as an argv),
//! - the typed host frames (`{"host": {"type": …}}` → `host_reply`),
//!   all framed through `induction-protocol` (one vocabulary, not a copy),
//! - the load-time handshake answer (`initialize` → `hello`) and the pull form
//!   (`interface_schema` → the assembled schema), from the SAME registry,
//! - dispatch by event name, the iterate guard table, and the phase gate:
//!   `on`/`hook` are BUILDER methods, so registration is structurally closed
//!   once `run` begins — unrepresentable, not an error value.
//!
//! Zero aura/effector dependencies: the protocol IS the ABI.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use induction_protocol::{
    Arm, CallFrame, ENCODING_ENV, Encoding, Hello, HostFrame, Reply, ResultFrame, SCHEMA_EVENT,
    Schema,
};
use serde_json::Value;

// ------------------------------------------------------------ ctx --

/// The typed ctx facade. Every method is one typed host frame plus one blocking
/// `host_reply` read (the synchronous-by-contract rule); [`Ctx::emit`] is the
/// fire-and-forget exception — the parent answers NOTHING by contract.
pub struct Ctx<'a> {
    io: &'a mut dyn Codec,
}

impl Ctx<'_> {
    fn round_trip(&mut self, frame: &HostFrame) -> anyhow::Result<Value> {
        self.io.write(&frame.to_value())?;
        read_host_reply(self.io)
    }

    /// `ctx.invoke` — the canonical (booth-call) spelling: target instance,
    /// handler, args.
    pub fn invoke(
        &mut self,
        target_type: &str,
        target_key: &str,
        handler: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::new(
            Arm::Invoke,
            serde_json::json!({
                "type": target_type, "key": target_key, "handler": handler, "args": args,
            }),
        ))
    }

    /// `ctx.invoke` — the verb spelling: hosts whose control surface is a verb
    /// directory (mudra's today) route this shape. One arm, two payload
    /// spellings; see ADR-0001.
    pub fn invoke_verb(&mut self, verb: &str, args: Value) -> anyhow::Result<Value> {
        let host = serde_json::json!({ "type": "invoke", "verb": verb, "args": args });
        self.io.write(&serde_json::json!({ "host": host }))?;
        read_host_reply(self.io)
    }

    /// `ctx.emit` — fire-and-forget by contract: no reply is read.
    pub fn emit(&mut self, event: &str, data: Value) -> anyhow::Result<()> {
        let frame = HostFrame::new(
            Arm::Emit,
            serde_json::json!({ "event": event, "data": data }),
        );
        self.io.write(&frame.to_value())?;
        Ok(())
    }

    /// `ctx.store` — one collection instruction as data (the guest never parses
    /// its semantics; the host resolves it against the declared collections).
    pub fn store(&mut self, op: Value) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::store(op))
    }

    /// `ctx.iterate.start` — open a stream on a sibling booth (the consumer
    /// face of ADR-0034). The reply carries the first envelope with the
    /// realm-minted `stream_id` merged in.
    pub fn iterate_start(
        &mut self,
        target_type: &str,
        target_key: &str,
        handler: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::new(
            Arm::IterateStart,
            serde_json::json!({
                "type": target_type, "key": target_key, "handler": handler, "args": args,
            }),
        ))
    }

    /// `ctx.iterate.next` — pull one envelope round by stream id. The realm's
    /// registry names the producer; the id is the route.
    pub fn iterate_next(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::new(
            Arm::IterateNext,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.iterate.dispose` — abandon a stream (idempotent per the realm's
    /// routing entry; the mandatory dual of Start). No envelope reply.
    pub fn iterate_dispose(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::new(
            Arm::IterateDispose,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.schema` — the persisted declaration copy held by the host.
    pub fn schema(&mut self) -> anyhow::Result<Value> {
        self.round_trip(&HostFrame::new(Arm::Schema, Value::Null))
    }

    /// `ctx.timer.register` → the timer id.
    pub fn timer_register(&mut self, at_ms: u64, tag: &str) -> anyhow::Result<u64> {
        let v = self.round_trip(&HostFrame::new(
            Arm::TimerRegister,
            serde_json::json!({ "at_ms": at_ms, "tag": tag }),
        ))?;
        Ok(v.get("timer_id").and_then(Value::as_u64).unwrap_or(0))
    }

    /// `ctx.timer.cancel` — idempotent (unknown = already fired).
    pub fn timer_cancel(&mut self, timer_id: u64) -> anyhow::Result<()> {
        self.round_trip(&HostFrame::new(
            Arm::TimerCancel,
            serde_json::json!({ "timer_id": timer_id }),
        ))?;
        Ok(())
    }

    /// `ctx.queue.depth` — the backlog point-read.
    pub fn queue_depth(&mut self, event: &str) -> anyhow::Result<u64> {
        let v = self.round_trip(&HostFrame::new(
            Arm::QueueDepth,
            Value::String(event.into()),
        ))?;
        Ok(v.as_u64().unwrap_or(0))
    }

    /// `ctx.queue.skip_to_head` — the relief valve (a discarded backlog never
    /// revives).
    pub fn queue_skip_to_head(&mut self, event: &str) -> anyhow::Result<()> {
        self.round_trip(&HostFrame::new(
            Arm::QueueSkipToHead,
            Value::String(event.into()),
        ))?;
        Ok(())
    }
}

// ------------------------------------------------------------ codec --

/// How a `Value` crosses the pipe. The shape under both codecs is identical;
/// only the framing differs (JSON = text lines, CBOR = self-delimited
/// documents, so sequential decodes on the blocking reader land
/// document-by-document).
trait Codec {
    fn read(&mut self) -> Option<Value>;
    fn write(&mut self, v: &Value) -> anyhow::Result<()>;
}

struct JsonCodec<R: BufRead, W: Write> {
    reader: R,
    out: W,
}

impl<R: BufRead, W: Write> Codec for JsonCodec<R, W> {
    fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
        }
    }

    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        writeln!(self.out, "{v}")?;
        self.out.flush()?;
        Ok(())
    }
}

struct CborCodec<R: std::io::Read, W: Write> {
    reader: R,
    out: W,
}

impl<R: std::io::Read, W: Write> Codec for CborCodec<R, W> {
    fn read(&mut self) -> Option<Value> {
        ciborium::de::from_reader(&mut self.reader).ok()
    }

    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        ciborium::ser::into_writer(v, &mut self.out)?;
        self.out.flush()?;
        Ok(())
    }
}

/// Read one `host_reply` and unpack it: `ok` carries the value, an error arm
/// becomes the Err. A swallowed Null would make the parent's failure invisible
/// in the guest's logs — this function exists so that cannot happen.
fn read_host_reply(io: &mut dyn Codec) -> anyhow::Result<Value> {
    let v = io
        .read()
        .ok_or_else(|| anyhow::anyhow!("host closed the channel mid-call (no host_reply)"))?;
    match Reply::from_value(&v) {
        Some(Reply::Ok(value)) => Ok(value),
        Some(Reply::Error(e)) => Err(anyhow::anyhow!("ctx call failed: {e}")),
        None => Err(anyhow::anyhow!("expected a host_reply frame, got {v}")),
    }
}

// ------------------------------------------------------ the guest --

type Handler = Box<dyn Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send>;

/// One iterate producer's guard: the handler drives its own stream state; the
/// shim tracks the pull count only where the protocol needs it (exhaustion is
/// terminal — the producer writes `done`).
struct StreamGuard {
    pulled: u64,
    total: u64,
}

/// The registry + run-loop owner. Assembled at construction, frozen when `run`
/// begins.
struct Registry {
    handlers: HashMap<String, Handler>,
    hooks: Vec<String>,
    wildcards: Vec<String>,
    storage: Option<Value>,
    auth: Option<Value>,
}

impl Registry {
    fn schema(&self) -> Schema {
        Schema {
            receives: self
                .handlers
                .keys()
                .filter(|k| !k.starts_with("__"))
                .map(|k| (k.clone(), Value::Object(serde_json::Map::new())))
                .collect(),
            wildcard_receives: self.wildcards.clone(),
            hooks: self.hooks.clone(),
            storage: self.storage.clone(),
            auth: self.auth.clone(),
        }
    }
}

pub struct Guest {
    /// The name this guest declares in its hello. Unset falls back to the
    /// executable's basename — the host may cross-check it against its own
    /// declaration key and only logs a mismatch.
    name: Option<String>,
    registry: Registry,
}

impl Default for Guest {
    fn default() -> Self {
        Self::new()
    }
}

impl Guest {
    pub fn new() -> Self {
        Self {
            name: None,
            registry: Registry {
                handlers: HashMap::new(),
                hooks: Vec::new(),
                wildcards: Vec::new(),
                storage: None,
                auth: None,
            },
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Register an event handler. A `__`-prefixed event here is a DECLARATION
    /// ERROR — lifecycle hooks register through [`Guest::hook`], mirroring the
    /// rule that reserved names never enter `receives`.
    pub fn on(
        mut self,
        event: impl Into<String>,
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + 'static,
    ) -> Self {
        let event = event.into();
        assert!(
            !event.starts_with("__"),
            "`{event}` is a reserved hook name — register lifecycle hooks with .hook(), never .on()"
        );
        self.registry.handlers.insert(event, Box::new(h));
        self
    }

    /// Register a wildcard (prefix pattern) subscription — singleton consumers.
    pub fn on_wildcard(mut self, pattern: impl Into<String>) -> Self {
        let pattern = pattern.into();
        assert!(
            pattern.ends_with('*'),
            "wildcard patterns are prefix form (`prefix.*`): {pattern}"
        );
        self.registry.wildcards.push(pattern);
        self
    }

    /// Register a reserved lifecycle hook (`__sleep` / `__wake` only). Lands in
    /// the schema's `hooks` array, never in `receives`.
    pub fn hook(
        mut self,
        name: impl Into<String>,
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + 'static,
    ) -> Self {
        let name = name.into();
        assert!(
            matches!(name.as_str(), "__sleep" | "__wake"),
            "hook name `{name}` is not reserved (allowed: [\"__sleep\", \"__wake\"])"
        );
        if !self.registry.hooks.contains(&name) {
            self.registry.hooks.push(name.clone());
        }
        self.registry.handlers.insert(name, Box::new(h));
        self
    }

    /// Declare the storage block (the ctx.store plan source), carried verbatim
    /// in the schema.
    pub fn storage(mut self, schema: Value) -> Self {
        self.registry.storage = Some(schema);
        self
    }

    /// Declare the per-handler auth block, carried verbatim in the
    /// schema (the host's gate reads `auth.<handler>`; the guest
    /// declares which handlers need a bound user).
    pub fn auth(mut self, auth: Value) -> Self {
        self.registry.auth = Some(auth);
        self
    }

    /// Bind the iterate producer: the handler that stream pulls call, round by
    /// round (`{stream_id, round}` in, an envelope out).
    ///
    /// This is the reserved `__next__` binding. It gets its own builder because
    /// a stream mechanism is not a lifecycle hook: `.on()` keeps refusing
    /// reserved names without making iterate unreachable (the hole this closes
    /// — the guard lookup and the `.on()` assertion could not both be satisfied).
    pub fn producer(
        mut self,
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + 'static,
    ) -> Self {
        self.registry
            .handlers
            .insert("__next__".to_string(), Box::new(h));
        self
    }

    /// The declaration this guest will answer with, both at load (hello) and on
    /// the pull form (`interface_schema`).
    pub fn declare(&self) -> Schema {
        self.registry.schema()
    }

    fn declared_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        std::env::var("INDUCTION_NAME").unwrap_or_else(|_| {
            std::env::args()
                .next()
                .as_deref()
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
                .unwrap_or_else(|| "extension".to_string())
        })
    }

    /// Run: own the stdio channels, loop until EOF (the parent closed stdin =
    /// the session ends — teardown or eviction).
    pub fn run(self) -> anyhow::Result<()> {
        let codec = Encoding::from_env();
        let cin = std::io::stdin();
        let cout = std::io::stdout();
        let _ = ENCODING_ENV;
        match codec {
            Encoding::Cbor => self.run_loop(CborCodec {
                reader: cin.lock(),
                out: cout.lock(),
            }),
            Encoding::Json => self.run_loop(JsonCodec {
                reader: cin.lock(),
                out: cout.lock(),
            }),
        }
    }

    fn run_loop<C: Codec>(self, mut io: C) -> anyhow::Result<()> {
        let name = self.declared_name();
        let schema = self.declare();
        let mut streams: HashMap<String, StreamGuard> = HashMap::new();
        while let Some(frame) = io.read() {
            // The load-time handshake: one initialize → one hello, from the
            // same registry the pull form answers with.
            if frame.get("type").and_then(Value::as_str) == Some("initialize") {
                io.write(
                    &Hello {
                        name: name.clone(),
                        schema: schema.clone(),
                    }
                    .to_value(),
                )?;
                continue;
            }
            let Some(req) = CallFrame::from_value(&frame) else {
                // Not a request we can answer (no id to key a result to). Log
                // and keep the session: a host's stray frame must not kill a
                // live extension.
                eprintln!("[induction-guest] undecodable frame: {frame}");
                continue;
            };
            let result = if req.event == SCHEMA_EVENT {
                Ok(schema.to_value())
            } else {
                let mut ctx = Ctx { io: &mut io };
                match req.kind {
                    // A plain handler's bare reply wraps to `{done:true,value}`
                    // at the host; the bare `call` kind is the primitive.
                    induction_protocol::FrameKind::Call
                    | induction_protocol::FrameKind::IterateStart => self.run_handler(
                        &mut ctx,
                        &req.event,
                        req.args,
                        &req.stream_id,
                        &mut streams,
                    ),
                    induction_protocol::FrameKind::IterateNext => self.pull(
                        &mut ctx,
                        req.stream_id.as_deref().unwrap_or(""),
                        &mut streams,
                    ),
                    induction_protocol::FrameKind::IterateDispose => {
                        streams.remove(req.stream_id.as_deref().unwrap_or(""));
                        Ok(Value::Null)
                    }
                }
            };
            let result = match result {
                Ok(v) => v,
                Err(e) => Value::String(format!("{e:#}")),
            };
            io.write(&ResultFrame::new(Value::from(req.id), result).to_value())?;
        }
        Ok(())
    }

    fn run_handler(
        &self,
        ctx: &mut Ctx,
        event: &str,
        args: Value,
        stream_id: &Option<String>,
        streams: &mut HashMap<String, StreamGuard>,
    ) -> anyhow::Result<Value> {
        let Some(h) = self.registry.handlers.get(event) else {
            anyhow::bail!("no such handler: {event}");
        };
        let out = h(ctx, args)?;
        let opened = out.get("done").and_then(Value::as_bool) == Some(false);
        if opened {
            let total = out.get("total").and_then(Value::as_u64).unwrap_or(u64::MAX);
            streams.insert(
                stream_id.clone().unwrap_or_default(),
                StreamGuard { pulled: 1, total },
            );
        }
        Ok(out)
    }

    fn pull(
        &self,
        ctx: &mut Ctx,
        stream_id: &str,
        streams: &mut HashMap<String, StreamGuard>,
    ) -> anyhow::Result<Value> {
        let Some(g) = streams.get_mut(stream_id) else {
            anyhow::bail!("unknown stream {stream_id}");
        };
        g.pulled += 1;
        if g.pulled > g.total {
            streams.remove(stream_id);
            return Ok(serde_json::json!({ "done": true }));
        }
        let h = self
            .registry
            .handlers
            .get("__next__")
            .ok_or_else(|| anyhow::anyhow!("stream {stream_id}: no __next__ producer bound"))?;
        h(
            ctx,
            serde_json::json!({ "stream_id": stream_id, "round": g.pulled - 1 }),
        )
    }
}

/// Re-export for guests that want the arm vocabulary.
pub use induction_protocol::{Arm as CtxArm, SCHEMA_EVENT as SCHEMA_EVENT_NAME};
