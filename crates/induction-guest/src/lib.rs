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

mod cancel;

pub use cancel::CancelToken;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use induction_protocol::{
    Arm, CallFrame, Encoding, FrameKind, Hello, HostFrame, InterruptFrame, Reply, ResultFrame,
    SCHEMA_EVENT, Schema,
};
/// The environment variable a guest reads to learn which codec its parent DECLARED
/// (`INDUCTION_ENCODING`). Re-exported because a guest that reports its own capability (a
/// booth telling its venue what it can produce) must read the same name the parent wrote —
/// and inventing the literal in a second crate is how two spellings of one contract happen.
pub use induction_protocol::ENCODING_ENV;
use serde_json::Value;

// ------------------------------------------------------------ ctx --

/// The typed ctx facade. Every method is one typed host frame plus a wait on
/// that call's own `host_reply` — the reply is matched by id, so a handler can
/// be one of several in flight; [`Ctx::emit`] is the fire-and-forget exception
/// (the parent answers NOTHING by contract).
pub struct Ctx<'a> {
    io: &'a Channel,
    /// This call's cancel flag (ADR-0047 stage two). Raised by the reader loop when
    /// the parent stops waiting; never raised by anything else, and never reset — one
    /// call, one token.
    cancel: CancelToken,
    /// The request this ctx belongs to: stamped on every host call, so a host
    /// running several of this process's handlers at once can tell which call a
    /// ctx frame serves. A serial guest never needs it; a concurrent one cannot
    /// be answered without it.
    call: u64,
}

impl Ctx<'_> {
    /// Send one host call, labelled with the request it belongs to.
    fn send(&mut self, mut frame: HostFrame) -> anyhow::Result<Value> {
        frame.set_call(self.call);
        self.io.call(frame)
    }

    /// `ctx.cancel` — this call's flag. Raised when the parent stops waiting
    /// (`{"type": "interrupt", "id": …}`), which is stage two of a stop: stage one is
    /// the control bit a handler reads at its own boundaries, and this is the same
    /// intent reaching a handler that is inside one long operation.
    ///
    /// Cooperative, honestly so: a handler that never looks cannot be stopped from
    /// inside. Bounding that is the host's job, not this token's.
    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    /// Shorthand for the usual question: `if ctx.is_cancelled() { break }`.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
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
        self.send(HostFrame::new(
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
        self.send(HostFrame {
            id: None,
            call: None,
            arm: Arm::Invoke,
            args: Value::Null,
            op: None,
            raw: serde_json::json!({ "type": "invoke", "verb": verb, "args": args }),
            bytes: None,
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

    /// `ctx.emit` for a payload that is already ENCODED — the byte-carrying twin of
    /// [`Ctx::emit`], and the whole reason the content codec is an axis rather than a
    /// second output mode: a guest assembles its content once (a `Value`) and hands it
    /// here in whichever encoding the deployment asked for.
    ///
    /// `emit` is untouched, so every existing caller keeps the exact frame it had. On a CBOR
    /// link these bytes go out as a CBOR BYTE STRING ([`cbor_payload`]'s JSON spelling is what
    /// [`CborWriter`] resolves into one); on a JSON link — the fallback, because JSON text
    /// cannot hold binary — they ride as `{"$cbor": "<base64>"}`.
    ///
    /// The receiver tells the two apart with no out-of-band configuration: encoded content
    /// arrives as an object whose ONLY key is [`CBOR_PAYLOAD_KEY`], and a CBOR peer's lift
    /// says the same thing with the bytes in hand.
    pub fn emit_bytes(&mut self, event: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.emit(event, cbor_payload(bytes))
    }

    /// `ctx.store` — one collection instruction as data (the guest never parses
    /// its semantics; the host resolves it against the declared collections).
    pub fn store(&mut self, op: Value) -> anyhow::Result<Value> {
        self.send(HostFrame::store(op))
    }

    /// `ctx.resource` — durable data, asked for at RUN time: `{"op": "get", "kind", "seq"}`
    /// answers a path this process can open, `{"op": "publish", ...}` answers the reference the
    /// store minted. The credential stays on the host side of this call; a script that knew it
    /// could take the data anywhere.
    pub fn resource(&mut self, op: Value) -> anyhow::Result<Value> {
        self.send(HostFrame::resource(op))
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
        self.send(HostFrame::new(
            Arm::IterateStart,
            serde_json::json!({
                "type": target_type, "key": target_key, "handler": handler, "args": args,
            }),
        ))
    }

    /// `ctx.iterate.next` — pull one envelope round by stream id. The realm's
    /// registry names the producer; the id is the route.
    pub fn iterate_next(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.send(HostFrame::new(
            Arm::IterateNext,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.iterate.dispose` — abandon a stream (idempotent per the realm's
    /// routing entry; the mandatory dual of Start). No envelope reply.
    pub fn iterate_dispose(&mut self, stream_id: &str) -> anyhow::Result<Value> {
        self.send(HostFrame::new(
            Arm::IterateDispose,
            serde_json::json!({ "stream_id": stream_id }),
        ))
    }

    /// `ctx.schema` — the persisted declaration copy held by the host.
    pub fn schema(&mut self) -> anyhow::Result<Value> {
        self.send(HostFrame::new(Arm::Schema, Value::Null))
    }

    /// `ctx.timer.register` → the timer id. `at_ms` is a delay FROM NOW, and `durable`
    /// asks the host to keep the schedule where a restart can find it (aura ADR-0016 §1:
    /// ns 27 rows the driver rebuilds the wheel from at boot) instead of only in the
    /// wheel, which dies with the process. Whether a host keeps such a table is the
    /// host's business — the guest cannot tell, so a wake is as durable as the host it
    /// asked, and the row is the only evidence either way.
    pub fn timer_register(&mut self, at_ms: u64, tag: &str, durable: bool) -> anyhow::Result<u64> {
        let v = self.send(HostFrame::new(
            Arm::TimerRegister,
            timer_args(at_ms, tag, durable),
        ))?;
        Ok(v.get("timer_id").and_then(Value::as_u64).unwrap_or(0))
    }

    /// `ctx.timer.cancel` — idempotent (unknown = already fired).
    pub fn timer_cancel(&mut self, timer_id: u64) -> anyhow::Result<()> {
        self.send(HostFrame::new(
            Arm::TimerCancel,
            serde_json::json!({ "timer_id": timer_id }),
        ))?;
        Ok(())
    }

    /// `ctx.queue.depth` — the backlog point-read.
    pub fn queue_depth(&mut self, event: &str) -> anyhow::Result<u64> {
        let v = self.send(HostFrame::new(
            Arm::QueueDepth,
            Value::String(event.into()),
        ))?;
        Ok(v.as_u64().unwrap_or(0))
    }

    /// `ctx.stop.poll` — stage one of a stop (aura ADR-0047): has someone asked
    /// THIS activation to stop? The answer is a bit, so this is a cheap read at a
    /// boundary the guest already has — between chunks, between calls — and the
    /// guest decides what stopping means (a turn ends its own stream and closes
    /// nothing).
    pub fn stop_poll(&mut self) -> anyhow::Result<bool> {
        // No identity in the payload: the HOST knows whose activation is calling
        // (one process per instance), and a guest-supplied identity would only
        // invite a guest to ask about somebody else.
        let v = self.send(HostFrame::new(Arm::StopPoll, Value::Null))?;
        Ok(v.as_bool().unwrap_or(false))
    }

    /// `ctx.stop.clear` — the activation answered; the host forgets the request
    /// rather than letting it stop the next activation of the same instance.
    pub fn stop_clear(&mut self) -> anyhow::Result<()> {
        self.send(HostFrame::new(Arm::StopClear, Value::Null))?;
        Ok(())
    }

    /// `ctx.queue.skip_to_head` — the relief valve (a discarded backlog never
    /// revives).
    pub fn queue_skip_to_head(&mut self, event: &str) -> anyhow::Result<()> {
        self.send(HostFrame::new(
            Arm::QueueSkipToHead,
            Value::String(event.into()),
        ))?;
        Ok(())
    }
}

// ---------------------------------------------------- content codec --

// The payload spelling (the `$cbor` key, its base64 JSON mapping, and the reverse read)
// lives in the protocol crate now — one home, because gravity's booth, prism's funnel and
// this facade all read it. A CBOR link writes a real BYTE STRING for it; JSON text cannot
// hold binary, so a JSON link keeps the base64 mapping. Same payload, two spellings,
// chosen by the codec the deployment declared — never by configuration.
pub use induction_protocol::{CBOR_PAYLOAD_KEY, cbor_payload, payload_bytes};

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
        // Read the tree with bytes intact, then put an opaque payload back into its JSON
        // spelling: a `Value`-typed reader has no other way to hold binary, and this is the
        // exact shape a JSON link carries. Reading a frame a CBOR peer wrote therefore yields
        // the frame its JSON peer would have written — the codec is a transport fact, not a
        // difference the frame's consumers can see.
        let doc = ciborium::de::from_reader::<ciborium::Value, _>(&mut self.reader).ok()?;
        let (mut frame, bytes) = induction_protocol::split_bytes(doc).ok()?;
        if let Some(bytes) = bytes
            && !bytes.embed(&mut frame)
        {
            return None;
        }
        Some(frame)
    }
}

struct CborWriter<W: Write> {
    out: W,
}

impl<W: Write + Send> FrameWriter for CborWriter<W> {
    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        // A frame with an opaque payload writes the BYTE STRING, not its base64 JSON spelling
        // ([`induction_protocol::cbor_frame`] does the substitution). This is the whole reason
        // `emit_bytes` needs no new API: the payload the caller builds — `cbor_payload`, the
        // JSON mapping — becomes real bytes on a CBOR link and stays text on a JSON one. A
        // frame with no payload skips the tree rebuild entirely and goes out exactly as before.
        if induction_protocol::carries_payload(v) {
            let doc = induction_protocol::cbor_frame(v).map_err(anyhow::Error::msg)?;
            ciborium::ser::into_writer(&doc, &mut self.out)?;
        } else {
            ciborium::ser::into_writer(v, &mut self.out)?;
        }
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

    /// Register a reserved name the HOST delivers, never the venue: `__sleep` /
    /// `__wake` (residency) and `__on_timer` (a due timer, which aura delivers as an
    /// ordinary queue job whose handler is that reserved name — ADR-0016 §1). Lands in
    /// the schema's `hooks` array, never in `receives`, because a name the host owns
    /// must not also be a name somebody can emit.
    pub fn hook(
        mut self,
        name: impl Into<String>,
        h: impl Fn(&mut Ctx, Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        // One array, defined where the declaration surface lives (induction-protocol):
        // every carrier reads the same names, so a hook cannot be accepted here and refused
        // by the carrier that ships the same guest.
        assert!(
            induction_protocol::schema::RESERVED_HOOKS.contains(&name.as_str()),
            "hook name `{name}` is not reserved (allowed: {:?})",
            induction_protocol::schema::RESERVED_HOOKS
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
    /// the link ends — teardown or eviction).
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
        // The call table for stage two (ADR-0047): raised by the loop, read by the
        // handler. Nothing here is a delivery — the handler is a plain function.
        let cancels: Arc<Mutex<HashMap<u64, CancelToken>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // The serial worker. Even a serial guest needs one: the reader loop must
        // stay free to deliver replies while a handler waits on its id.
        let (job_tx, job_rx) = mpsc::channel::<CallFrame>();
        if !concurrent {
            let worker = Worker {
                channel: Arc::clone(&channel),
                registry: Arc::clone(&registry),
                streams: Arc::clone(&streams),
                cancels: Arc::clone(&cancels),
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
            // STAGE TWO OF A STOP (ADR-0047): the parent stopped waiting for a call,
            // so the call's flag goes up. The loop is free to receive this precisely
            // because it never runs a handler — that is the structural reason the
            // interrupt can exist at all.
            if let Some(interrupt) = InterruptFrame::from_value(&frame) {
                let mut map = cancels
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match map.get(&interrupt.id) {
                    Some(token) => token.cancel(),
                    // Not started yet (serial guest, queued): leave it raised so the
                    // call begins cancelled rather than late.
                    None => {
                        map.insert(interrupt.id, CancelToken::cancelled());
                    }
                }
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
                // and keep the link: a host's stray frame must not kill a
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
                    cancels: Arc::clone(&cancels),
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
    /// id → the call's cancel flag. The loop raises what it finds; a worker takes
    /// the token it is about to run under, which is also how a call that was
    /// interrupted while still queued starts cancelled instead of starting late.
    cancels: Arc<Mutex<HashMap<u64, CancelToken>>>,
}

impl Worker {
    fn handle(&self, req: CallFrame) {
        let cancel = {
            let mut map = self
                .cancels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.entry(req.id).or_default().clone()
        };
        let ctx = &mut Ctx {
            io: &self.channel,
            call: req.id,
            cancel,
        };
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
        // call), never a torn link.
        let result = match result {
            Ok(v) => v,
            Err(e) => Value::String(format!("{e:#}")),
        };
        // The call is over. An interrupt that arrives AFTER this leaves one raised
        // token behind (the loop has no way to know the call just finished), so the map
        // is bounded by "interrupts whose call was already done" — cancels, not traffic.
        self.cancels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&req.id);
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

/// The `timer.register` argument object, built in one place so the wire shape is
/// assertable without a pipe: the host reads `at_ms` as a delay from now, `tag` as the
/// string it hands back with the wake, and `durable` as "keep this where a restart can
/// find it". A host that knows only the first two ignores the third rather than failing —
/// which is why the guest cannot report durability, only ask for it.
fn timer_args(at_ms: u64, tag: &str, durable: bool) -> Value {
    serde_json::json!({ "at_ms": at_ms, "tag": tag, "durable": durable })
}

/// Re-export for guests that want the arm vocabulary.
pub use induction_protocol::{Arm as CtxArm, SCHEMA_EVENT as SCHEMA_EVENT_NAME};

// -------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
    //! The codec axis, proved where it is built: one content value, sent through BOTH
    //! `emit` paths over a REAL CborWriter/CborReader pair (the pipe a deployed guest
    //! has), and the two decoded contents compared for EQUALITY — not merely for parsing.

    use super::*;
    use std::io::Cursor;

    /// A writer whose bytes the test can read back: the pipe, minus the process.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// One content value with the shapes gravity's frames actually carry.
    fn content() -> Value {
        serde_json::json!({
            "name": "ui:turn.done",
            "island": "gravity-model",
            "turn": "7",
            "data": [{ "action": "append", "slot": "messages", "data": { "id": "7_1", "type": "frame" } }],
        })
    }

    /// Drive one ctx over a fresh channel, run `f`, and hand back every frame the pipe
    /// received, decoded the way the guest's own reader decodes them.
    fn emit_through_cbor_pipe(f: impl FnOnce(&mut Ctx)) -> Vec<Value> {
        let sink = Sink::default();
        let channel = Channel::new(Box::new(CborWriter {
            out: sink.clone(),
        }));
        {
            let mut ctx = Ctx {
                io: &channel,
                cancel: CancelToken::default(),
                call: 1,
            };
            f(&mut ctx);
        }
        let bytes = sink.0.lock().expect("sink").clone();
        let mut reader = CborReader {
            reader: Cursor::new(bytes),
        };
        let mut frames = Vec::new();
        while let Some(frame) = reader.read() {
            frames.push(frame);
        }
        frames
    }

    /// THE POINT OF THE WHOLE EXERCISE: the same content, encoded both ways, decodes to the
    /// SAME value. The CBOR frame carries `{"$cbor": …}`; the JSON frame carries the envelope
    /// itself. Neither needs to be told which it is — the shape says.
    #[test]
    fn both_encodings_decode_to_the_same_content() {
        let content = content();
        let cbor_document = {
            let mut buf = Vec::new();
            ciborium::into_writer(&content, &mut buf).expect("cbor document");
            buf
        };

        let frames = emit_through_cbor_pipe(|ctx| {
            ctx.emit("ui:turn.done", content.clone()).expect("emit");
            ctx.emit_bytes("ui:turn.done", &cbor_document)
                .expect("emit_bytes");
        });
        assert_eq!(frames.len(), 2, "one frame each: {frames:?}");

        let plain = frames[0]["host"]["args"].clone();
        let encoded = frames[1]["host"]["args"].clone();
        assert_eq!(plain["event"], "ui:turn.done");
        assert_eq!(encoded["event"], "ui:turn.done");

        // The discriminator: a plain envelope is a map with `name`/`island`/`data`; encoded
        // content is a map whose ONLY key is the reserved one.
        assert!(
            super::payload_bytes(&plain["data"]).is_none(),
            "the JSON path must not look encoded: {}",
            plain["data"]
        );
        let bytes = super::payload_bytes(&encoded["data"]).expect("the byte path is tagged");
        assert_eq!(bytes, cbor_document, "the bytes survive the wrapper verbatim");

        // …and the two decoded contents are EQUAL.
        let from_bytes: Value = ciborium::from_reader(bytes.as_slice()).expect("decode");
        assert_eq!(from_bytes, content);
        assert_eq!(
            from_bytes, plain["data"],
            "the CBOR path decodes to exactly what the JSON path sent"
        );
    }

    /// NO BASE64 ON THE CBOR PATH: the payload's own bytes are on the wire, contiguously, and
    /// the wrapper's text is nowhere in them. This is the measurement the whole change exists
    /// for — the wrapper costs a third of the payload, and a CBOR link does not need it.
    #[test]
    fn a_cbor_link_carries_the_payload_as_a_byte_string() {
        // Long enough that a 4- or 8-character base64 fragment could not appear by accident,
        // and not text, so no codec could have chosen the readable spelling.
        let document: Vec<u8> = (0u16..96).map(|i| (i * 7 % 251) as u8).collect();
        let sink = Sink::default();
        let channel = Channel::new(Box::new(CborWriter {
            out: sink.clone(),
        }));
        {
            let mut ctx = Ctx {
                io: &channel,
                cancel: CancelToken::default(),
                call: 1,
            };
            ctx.emit_bytes("ui:x", &document).expect("emit_bytes");
        }
        let wire = sink.0.lock().expect("sink").clone();

        assert!(
            wire.windows(document.len()).any(|w| w == document.as_slice()),
            "the payload's bytes are on the wire"
        );
        let base64 = super::cbor_payload(&document)[super::CBOR_PAYLOAD_KEY]
            .as_str()
            .expect("the mapping holds base64 text")
            .to_string();
        assert!(
            !wire
                .windows(base64.len())
                .any(|w| w == base64.as_bytes()),
            "a CBOR link must not carry the base64 spelling"
        );

        // And the frame a `Value`-typed reader sees is the frame a JSON peer would have sent:
        // the lift is a transport fact, not a shape its consumers can observe.
        let mut reader = CborReader {
            reader: Cursor::new(wire),
        };
        let frame = reader.read().expect("one frame");
        assert_eq!(
            super::payload_bytes(&frame["host"]["args"]["data"]),
            Some(document),
            "the reader puts the payload back in the only spelling a `Value` has"
        );
    }

    /// The axis is orthogonal to the PIPE encoding: `emit_bytes` is legal on a JSON-lines
    /// link too, because its payload is JSON-shaped. A deployment that switches content
    /// codec must not be forced to switch the frame codec with it.
    #[test]
    fn the_byte_path_crosses_a_json_link_unchanged() {
        let sink = Sink::default();
        let channel = Channel::new(Box::new(JsonWriter {
            out: sink.clone(),
        }));
        let document = vec![0x81, 0x01];
        {
            let mut ctx = Ctx {
                io: &channel,
                cancel: CancelToken::default(),
                call: 1,
            };
            ctx.emit_bytes("ui:x", &document).expect("emit_bytes");
        }
        let bytes = sink.0.lock().expect("sink").clone();
        let mut reader = JsonReader {
            reader: std::io::BufReader::new(Cursor::new(bytes)),
        };
        let frame = reader.read().expect("one line");
        let data = frame["host"]["args"]["data"].clone();
        assert_eq!(super::payload_bytes(&data), Some(document.clone()));
        assert_eq!(super::cbor_payload(&document), data, "one wrapper, both codecs");
    }
    /// The durable flag is part of the arm, not a host detail: a booth asks for a schedule
    /// that outlives its own process, and the frame must say so (aura ADR-0016 §1 — ns 27
    /// rows; the wheel alone dies with the process).
    #[test]
    fn the_timer_arm_carries_the_durable_ask() {
        let args = timer_args(600_000, "compress:7", true);
        assert_eq!(args["at_ms"], serde_json::json!(600_000));
        assert_eq!(args["tag"], serde_json::json!("compress:7"));
        assert_eq!(args["durable"], serde_json::json!(true));
        // The default the caller writes out: a memory-tier wake says so rather than omitting.
        assert_eq!(timer_args(1, "t", false)["durable"], serde_json::json!(false));
    }

}
