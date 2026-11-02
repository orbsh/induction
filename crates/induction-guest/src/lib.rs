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
//! - declaration-time guards: `on` refuses a `__`-prefixed event, `on_wildcard`
//!   requires the `prefix.*` form, and `hook` accepts only the two reserved
//!   names. These three `assert!`s are the crate's ONLY panic sites, and they
//!   stay assertions on purpose: the input is the extension author's own
//!   registration code rather than external data, no clippy lint sees an
//!   assertion at all (so they cannot be gated), and a fallible builder chain
//!   would tax every extension with `?` for a check that can only fire on a
//!   typo. README §"The panic surface" records the trade.
//!
//! ## Two requests at once (ADR-0002)
//!
//! The channel is split the same way on both ends. The **reader loop** never
//! runs a handler: it answers the handshake, hands replies to the call waiting on
//! their `id`, and dispatches every request to a worker. A ctx round trip
//! therefore waits on ITS id rather than on "the next frame", which is what lets
//! one guest process have several calls — and several host requests — live at
//! once.
//!
//! Serial is the default and stays the default: [`Guest::concurrent`] is the opt
//! in, and a serial guest runs its handlers on ONE worker thread in arrival
//! order, so nothing it could observe changes. What changes even for a serial
//! guest is only the thing that was broken: a reply is matched to its call.
//!
//! Zero aura/effector dependencies: the protocol IS the ABI.

// Unit tests keep the unwrap/panic vocabulary the production targets are
// denied: a panicking test is a failing test with a stack trace. `cfg(test)` is
// only on for this crate's own test build — the `tests/` crates carry the same
// allow at their own root.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn,
        clippy::indexing_slicing
    )
)]

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use induction_protocol::{
    Arm, CallFrame, ENCODING_ENV, Encoding, FrameKind, Hello, HostFrame, Reply, ResultFrame,
    SCHEMA_EVENT, Schema,
};
use serde_json::Value;

// ------------------------------------------------------------ ctx --

/// The typed ctx facade. Every method is one typed host frame plus a wait on
/// that call's own `host_reply` — the reply is matched by id, so a handler can
/// be one of several in flight; [`Ctx::emit`] is the fire-and-forget exception
/// (the parent answers NOTHING by contract).
pub struct Ctx<'a> {
    io: &'a Channel,
}

impl Ctx<'_> {
    /// `ctx.invoke` — the canonical (booth-call) spelling: target instance,
    /// handler, args.
    pub fn invoke(
        &mut self,
        target_type: &str,
        target_key: &str,
        handler: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        self.io.call(HostFrame::new(
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
        self.io.call(HostFrame {
            id: None,
            arm: Arm::Invoke,
            args: Value::Null,
            op: None,
            raw: serde_json::json!({ "type": "invoke", "verb": verb, "args": args }),
        })
    }

    /// `ctx.emit` — fire-and-forget by contract: no reply is read.
    pub fn emit(&mut self, event: &str, data: Value) -> anyhow::Result<()> {
        let frame = HostFrame::new(
            Arm::Emit,
            serde_json::json!({ "event": event, "data": data }),
        );
        self.io.write(&frame.to_value())
    }

    /// `ctx.store` — one collection instruction as data (the guest never parses
    /// its semantics; the host resolves it against the declared collections).
    pub fn store(&mut self, op: Value) -> anyhow::Result<Value> {
        self.io.call(HostFrame::store(op))
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
        self.io.call(HostFrame::new(
            Arm::IterateStart,
            serde_json::json!({
                "type": target_type, "key": target_key, "handler": handler, "args": args,
            }),
        ))
    }

    /// `ctx.iterate.next` — pull one envelope round by stream id. The realm's
    /// registry names the producer; the id is the route.
    pub fn iterate_next(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.io.call(HostFrame::new(
            Arm::IterateNext,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.iterate.dispose` — abandon a stream (idempotent per the realm's
    /// routing entry; the mandatory dual of Start). No envelope reply.
    pub fn iterate_dispose(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.io.call(HostFrame::new(
            Arm::IterateDispose,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.schema` — the persisted declaration copy held by the host.
    pub fn schema(&mut self) -> anyhow::Result<Value> {
        self.io.call(HostFrame::new(Arm::Schema, Value::Null))
    }

    /// `ctx.timer.register` → the timer id.
    pub fn timer_register(&mut self, at_ms: u64, tag: &str) -> anyhow::Result<u64> {
        let v = self.io.call(HostFrame::new(
            Arm::TimerRegister,
            serde_json::json!({ "at_ms": at_ms, "tag": tag }),
        ))?;
        Ok(v.get("timer_id").and_then(Value::as_u64).unwrap_or(0))
    }

    /// `ctx.timer.cancel` — idempotent (unknown = already fired).
    pub fn timer_cancel(&mut self, timer_id: u64) -> anyhow::Result<()> {
        self.io.call(HostFrame::new(
            Arm::TimerCancel,
            serde_json::json!({ "timer_id": timer_id }),
        ))?;
        Ok(())
    }

    /// `ctx.queue.depth` — the backlog point-read.
    pub fn queue_depth(&mut self, event: &str) -> anyhow::Result<u64> {
        let v = self.io.call(HostFrame::new(
            Arm::QueueDepth,
            Value::String(event.into()),
        ))?;
        Ok(v.as_u64().unwrap_or(0))
    }

    /// `ctx.queue.skip_to_head` — the relief valve (a discarded backlog never
    /// revives).
    pub fn queue_skip_to_head(&mut self, event: &str) -> anyhow::Result<()> {
        self.io.call(HostFrame::new(
            Arm::QueueSkipToHead,
            Value::String(event.into()),
        ))?;
        Ok(())
    }
}

// ------------------------------------------------------------ channel --

/// How a `Value` crosses the pipe inbound. The shape under both codecs is
/// identical; only the framing differs (JSON = text lines, CBOR =
/// self-delimited documents, so sequential decodes on the blocking reader land
/// document-by-document).
trait FrameReader {
    fn read(&mut self) -> Option<Value>;
}

/// …and outbound. `Send` because the writer lives behind the channel's lock,
/// shared by every worker that answers a request.
trait FrameWriter: Send {
    fn write(&mut self, v: &Value) -> anyhow::Result<()>;
}

struct JsonReader<R: BufRead> {
    reader: R,
}

impl<R: BufRead> FrameReader for JsonReader<R> {
    fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
        }
    }
}

struct JsonWriter<W: Write> {
    out: W,
}

impl<W: Write + Send> FrameWriter for JsonWriter<W> {
    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        writeln!(self.out, "{v}")?;
        self.out.flush()?;
        Ok(())
    }
}

struct CborReader<R: std::io::Read> {
    reader: R,
}

impl<R: std::io::Read> FrameReader for CborReader<R> {
    fn read(&mut self) -> Option<Value> {
        ciborium::de::from_reader(&mut self.reader).ok()
    }
}

struct CborWriter<W: Write> {
    out: W,
}

impl<W: Write + Send> FrameWriter for CborWriter<W> {
    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        ciborium::ser::into_writer(v, &mut self.out)?;
        self.out.flush()?;
        Ok(())
    }
}

/// The guest's whole I/O state: one writer, an id counter, and the table of
/// calls waiting for an answer.
///
/// The reader is NOT here — the run loop owns it, which is the point: replies
/// arrive on the loop's thread and are delivered to whoever waits for their id,
/// so a handler blocked on a round trip never holds the read side.
struct Channel {
    out: Mutex<Box<dyn FrameWriter>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
}

impl Channel {
    fn new(out: Box<dyn FrameWriter>) -> Self {
        Self {
            out: Mutex::new(out),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn write(&self, v: &Value) -> anyhow::Result<()> {
        self.out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(v)
    }

    /// One typed call: tag it with a fresh id, wait for the reply that carries
    /// that id. Two handlers running at once therefore cannot consume each
    /// other's answers.
    fn call(&self, mut frame: HostFrame) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, tx);
        frame.set_id(id);
        if let Err(e) = self.write(&frame.to_value()) {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(e);
        }
        match rx.recv() {
            Ok(Reply::Ok(v)) => Ok(v),
            Ok(Reply::Error(e)) => Err(anyhow::anyhow!("ctx call failed: {e}")),
            Err(_) => Err(anyhow::anyhow!(
                "host closed the channel mid-call (no host_reply for id {id})"
            )),
        }
    }

    /// Deliver one reply frame to the call waiting on its id.
    fn deliver(&self, v: &Value) {
        let Some(reply) = Reply::from_value(v) else {
            eprintln!("[induction-guest] undecodable host frame: {v}");
            return;
        };
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(id) = Reply::id_of(v) {
            match pending.remove(&id) {
                Some(tx) => {
                    let _ = tx.send(reply);
                }
                None => eprintln!("[induction-guest] reply for unknown id {id}"),
            }
            return;
        }
        // A pre-ADR host answers in order and echoes nothing. That is only
        // unambiguous while exactly one call is outstanding, which is every
        // guest that never goes concurrent.
        match pending.len() {
            1 => {
                if let Some(id) = pending.keys().next().copied()
                    && let Some(tx) = pending.remove(&id)
                {
                    let _ = tx.send(reply);
                }
            }
            0 => eprintln!("[induction-guest] stray reply: {v}"),
            n => eprintln!(
                "[induction-guest] reply without an id while {n} calls are pending — \
                 the host predates ADR-0002 and cannot serve a concurrent guest"
            ),
        }
    }
}

// ------------------------------------------------------ the guest --

type Handler = Arc<dyn Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + Sync>;

/// One iterate producer's guard: the handler drives its own stream state; the
/// shim tracks the pull count only where the protocol needs it (exhaustion is
/// terminal — the producer writes `done`).
struct StreamGuard {
    pulled: u64,
    total: u64,
}

type Streams = Arc<Mutex<HashMap<String, StreamGuard>>>;

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
    /// ADR-0002's opt-in: `true` dispatches every request on its own thread, so
    /// two host requests run at once. `false` (the default) keeps ONE worker
    /// thread in arrival order — the shape every guest had before ADR-0002.
    concurrent: bool,
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
            concurrent: false,
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

    /// Serve two host requests at once (ADR-0002). Off by default: concurrency
    /// is a property of the guest's own state, and a guest that mutates shared
    /// state assumes one handler at a time — that guest either keeps this off or
    /// takes its own lock.
    ///
    /// On or off, replies are matched by id: a ctx round trip always waits on
    /// the answer to ITS call.
    pub fn concurrent(mut self, on: bool) -> Self {
        self.concurrent = on;
        self
    }

    /// Register an event handler. A `__`-prefixed event here is a DECLARATION
    /// ERROR — lifecycle hooks register through [`Guest::hook`], mirroring the
    /// rule that reserved names never enter `receives`.
    pub fn on(
        mut self,
        event: impl Into<String>,
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    ) -> Self {
        let event = event.into();
        assert!(
            !event.starts_with("__"),
            "`{event}` is a reserved hook name — register lifecycle hooks with .hook(), never .on()"
        );
        self.registry.handlers.insert(event, Arc::new(h));
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
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        assert!(
            matches!(name.as_str(), "__sleep" | "__wake"),
            "hook name `{name}` is not reserved (allowed: [\"__sleep\", \"__wake\"])"
        );
        if !self.registry.hooks.contains(&name) {
            self.registry.hooks.push(name.clone());
        }
        self.registry.handlers.insert(name, Arc::new(h));
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
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    ) -> Self {
        self.registry
            .handlers
            .insert("__next__".to_string(), Arc::new(h));
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
        let _ = ENCODING_ENV;
        match Encoding::from_env() {
            Encoding::Cbor => self.run_loop(
                CborReader {
                    reader: std::io::stdin(),
                },
                CborWriter {
                    out: std::io::stdout(),
                },
            ),
            Encoding::Json => self.run_loop(
                JsonReader {
                    reader: std::io::BufReader::new(std::io::stdin()),
                },
                JsonWriter {
                    out: std::io::stdout(),
                },
            ),
        }
    }

    fn run_loop<R, W>(self, mut reader: R, writer: W) -> anyhow::Result<()>
    where
        R: FrameReader,
        W: FrameWriter + 'static,
    {
        let name = self.declared_name();
        let schema = self.declare();
        let concurrent = self.concurrent;
        let channel = Arc::new(Channel::new(Box::new(writer)));
        let registry = Arc::new(self.registry);
        let streams: Streams = Arc::new(Mutex::new(HashMap::new()));

        // The serial worker. Even a serial guest needs one: the reader loop must
        // stay free to deliver replies while a handler waits on its id.
        let (job_tx, job_rx) = mpsc::channel::<CallFrame>();
        if !concurrent {
            let worker = Worker {
                channel: Arc::clone(&channel),
                registry: Arc::clone(&registry),
                streams: Arc::clone(&streams),
            };
            std::thread::spawn(move || {
                for req in job_rx {
                    worker.handle(req);
                }
            });
        }

        while let Some(frame) = reader.read() {
            // The load-time handshake: one initialize → one hello, from the
            // same registry the pull form answers with.
            if frame.get("type").and_then(Value::as_str) == Some("initialize") {
                channel.write(
                    &Hello {
                        name: name.clone(),
                        schema: schema.clone(),
                    }
                    .to_value(),
                )?;
                continue;
            }
            // A reply belongs to a call some handler is waiting on — never to
            // the loop itself.
            if Reply::from_value(&frame).is_some() {
                channel.deliver(&frame);
                continue;
            }
            let Some(req) = CallFrame::from_value(&frame) else {
                // Not a request we can answer (no id to key a result to). Log
                // and keep the session: a host's stray frame must not kill a
                // live extension.
                eprintln!("[induction-guest] undecodable frame: {frame}");
                continue;
            };
            if req.event == SCHEMA_EVENT {
                channel.write(
                    &ResultFrame::new(Value::from(req.id), schema.to_value()).to_value(),
                )?;
                continue;
            }
            if concurrent {
                let worker = Worker {
                    channel: Arc::clone(&channel),
                    registry: Arc::clone(&registry),
                    streams: Arc::clone(&streams),
                };
                std::thread::spawn(move || worker.handle(req));
            } else if job_tx.send(req).is_err() {
                eprintln!("[induction-guest] worker stopped; a request was dropped");
            }
        }
        Ok(())
    }
}

/// One request's execution: runs a handler, answers with the request's own id.
struct Worker {
    channel: Arc<Channel>,
    registry: Arc<Registry>,
    streams: Streams,
}

impl Worker {
    fn handle(&self, req: CallFrame) {
        let ctx = &mut Ctx { io: &self.channel };
        let result = match req.kind {
            FrameKind::Call | FrameKind::IterateStart => {
                self.run_handler(ctx, &req.event, req.args.clone(), &req.stream_id)
            }
            FrameKind::IterateNext => self.pull(ctx, req.stream_id.as_deref().unwrap_or("")),
            FrameKind::IterateDispose => {
                self.streams
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(req.stream_id.as_deref().unwrap_or(""));
                Ok(Value::Null)
            }
        };
        // A handler's failure is an ANSWER (the host turns it into a failed
        // call), never a torn session.
        let result = match result {
            Ok(v) => v,
            Err(e) => Value::String(format!("{e:#}")),
        };
        if let Err(e) = self
            .channel
            .write(&ResultFrame::new(Value::from(req.id), result).to_value())
        {
            eprintln!(
                "[induction-guest] could not answer call {}: {e}",
                req.id
            );
        }
    }

    fn run_handler(
        &self,
        ctx: &mut Ctx,
        event: &str,
        args: Value,
        stream_id: &Option<String>,
    ) -> anyhow::Result<Value> {
        let Some(h) = self.registry.handlers.get(event) else {
            anyhow::bail!("no such handler: {event}");
        };
        let out = h(ctx, args)?;
        let opened = out.get("done").and_then(Value::as_bool) == Some(false);
        if opened {
            let total = out.get("total").and_then(Value::as_u64).unwrap_or(u64::MAX);
            self.streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    stream_id.clone().unwrap_or_default(),
                    StreamGuard { pulled: 1, total },
                );
        }
        Ok(out)
    }

    fn pull(&self, ctx: &mut Ctx, stream_id: &str) -> anyhow::Result<Value> {
        let round = {
            let mut streams = self
                .streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(g) = streams.get_mut(stream_id) else {
                anyhow::bail!("unknown stream {stream_id}");
            };
            g.pulled += 1;
            if g.pulled > g.total {
                streams.remove(stream_id);
                return Ok(serde_json::json!({ "done": true }));
            }
            g.pulled - 1
        };
        let h = self
            .registry
            .handlers
            .get("__next__")
            .ok_or_else(|| anyhow::anyhow!("stream {stream_id}: no __next__ producer bound"))?;
        h(
            ctx,
            serde_json::json!({ "stream_id": stream_id, "round": round }),
        )
    }
}

/// Re-export for guests that want the arm vocabulary.
pub use induction_protocol::{Arm as CtxArm, SCHEMA_EVENT as SCHEMA_EVENT_NAME};