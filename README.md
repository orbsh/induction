# induction

A standalone protocol for embedding **resident extension processes** in a host
program: the host names an executable, spawns it, handshakes, delivers typed
frames, and serves the ctx arms the extension invokes back. The extension lives
as long as its host does (`PDEATHSIG`), keeps its own state and memory, and is
separated from the host by process boundaries — the coupling is mediated, not
shared.

Nothing here persists, and nothing here is a scheduler, a sandbox or a service
registry: the host owns its events, its storage, its policies. See
`docs/ADR-0001-induction.md` for the naming and boundary rulings, and for the
divergences this crate set was extracted to end.

## Crates

| crate | what it carries | dependencies |
| --- | --- | --- |
| `induction-protocol` | frames, ctx arm names, tier vocabulary, schema surface, codec declaration | `serde`, `serde_json`, `ciborium` |
| `induction-host` | session I/O with budgets, ctx-arm routing (`HostOps`), process adapter (PDEATHSIG + `kill_on_drop`), constant-backoff supervisor | protocol + `tokio` + `libc` |
| `induction-guest` | the Rust guest face: `Guest` builder, typed `Ctx`, main loop, dispatch, schema answer | protocol + `serde_json` + `ciborium` + `anyhow` |

## The minimal host

```rust
use induction_host::{Budgets, HostOps, InvokeCall, OpsFuture, Spec, run_driver, spawn};
use serde_json::json;

struct Verbs;

impl HostOps for Verbs {
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

async fn drive(spec: Spec) {
    let mut process = spawn(&spec).expect("spawn the extension");
    if process.session.handshake(Budgets::default().handshake).await.is_none() {
        process.kill();
        return;
    }
    let (_tx, mut rx) = tokio::sync::mpsc::channel(16);
    run_driver(&mut process.session, &Verbs, &mut rx, Budgets::default()).await;
    process.kill();
}
```

## The minimal guest

```rust
use induction_guest::{Ctx, Guest};
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    Guest::new()
        .named("echo")
        .on("echo", |_ctx: &mut Ctx, args: Value| Ok(json!({ "echo": args })))
        .producer(|_ctx: &mut Ctx, args: Value| {
            let round = args.get("round").and_then(Value::as_u64).unwrap_or(0);
            Ok(json!({ "done": false, "item": round }))
        })
        .run()
}
```

`cargo build --example loop -p induction-guest` builds a working guest
(`crates/induction-guest/examples/loop.rs`) that shows the handshake, a ctx
round trip, an iterate producer and a lifecycle hook.

## What the protocol actually carries

- **Process lifecycle** (`induction-host/src/process.rs`): the host spawns the
  declared executable (`Spec`: path + env). `PDEATHSIG=SIGTERM` rides
  `pre_exec` — a host killed with SIGKILL still takes its children down;
  `kill_on_drop` covers the normal path. stdin/stdout are piped; **stderr
  passes through** — the guest's own logs reach the host's terminal without
  riding the frame protocol.
- **Handshake** (`schema.rs`): the host writes the `initialize` frame (carrying
  `PROTOCOL`; no negotiation — the vocabulary is static and both sides are
  installed together), the guest answers `Hello` with its schema
  (`receives` / `wildcard_receives` / `hooks` / `storage`). The schema stays
  pullable at any time via a `call` to `interface_schema`.
- **Frame vocabulary** (`frame.rs`): parent → child
  `{"id","kind","event","args"}` with kind ∈ `call | iterate_start |
  iterate_next | iterate_dispose` (the iterate kinds add `op` + `stream_id`);
  child → parent in two directions — the guest invokes ctx arms as
  `{"host":{"type":<arm>,…}}` answered by `{"host_reply":{"ok"|"error"}}`, and
  answers host requests with `{"id","result"}`. The arms are the ADR-0043 ctx
  members as dotted method paths (invoke/store/store.frame/timer.*/queue.*/
  iterate.*/emit/schema).
- **The envelope**: `{item, done:false}` / `{done:true, value?}`. A plain call
  IS a stream whose first reply is terminal (ADR-0036) — one wire shape for
  both, stream association derived from `done`, not from a `stream_id` field.
- **Codec declaration** (`encoding.rs`): `INDUCTION_ENCODING` env (`json` |
  `cbor`; absent or unknown falls back to json — a bad value never fails a
  session), one encoding per session's life, never per frame. It crosses as an
  env, never as an argv: fifo-shaped spawn specs pass exactly the author's
  own arguments, and a runtime-appended argv would break the author's arity.
- **Budgets and the driver** (`session.rs`, `supervise.rs`): handshake and
  writes always carry a budget (defaults 5s; a write stall = the frame did
  not land — a host keeping a cursor must not advance it). Reads have two
  forms on purpose: the handshake and peer probes budget their read, the
  driver loop reads with NONE — a streaming guest may legitimately stay
  silent for long stretches, and a stalled peer stalls only itself. The
  select loop runs two live sources (the host's event frames + the child's
  stdout); a failed write ends the session rather than silently losing
  frames. A constant-backoff supervisor reaps and respawns.
- **The guest face** (`induction-guest`): the `Guest` builder (`.on` /
  `.on_wildcard` / `.hook` / `.storage` — declaration IS schema, and the
  phase gate is structural: post-run registration is unrepresentable), a
  typed `Ctx` mirroring the ctx arms 1:1, and the producer guard table
  (exhaustion is terminal — the producer writes `done`).
- **The host face**: the `HostOps` trait and `route_frame` — codec- and
  transport-independent arm dispatch with ONE home, so each host shapes its
  own codec on top (WS id-paired, pipe in-band).

## Can stdout carry binary? (CBOR)

Yes — CBOR is a shipped encoding, not an aspiration. `CborCodec` writes
`ciborium` documents straight to stdout and reads stdin the same way. CBOR is
**self-delimiting**: each document carries its own length information, so the
parser knows where it ends — no newline framing (that is the JSON mode's
device) and no length prefix. Documents are simply written back to back. The
host declares `Encoding::Cbor` (crossing to the child via the env), both
sides use the same codec. For a high-frequency small-frame stream (token
deltas), CBOR skips the per-frame JSON quoting/escaping overhead.

## Tests

```
cargo test --workspace
```

33 tests plus a doctest: the wire contract (decode strictness, arm vocabulary,
reply shapes, schema forms), in-memory session behaviour (handshake, budgets,
write stalls, driver outcomes), two live-child process tests (spawn + PDEATHSIG
+ handshake + round trip + ctx routing + iterate rounds), and the minimal-host
doctest.

The live tests spawn real children. Each lives in the package that owns the
fixture binary — cargo builds a package's own bins for its integration tests and
exports `CARGO_BIN_EXE_<name>`, so no manual build step is ever needed and a test
can never run against a stale fixture. Fixture bin names are distinct across
packages (`raw_guest` in `induction-host`, `loop_guest` in `induction-guest`):
two bins sharing a name across packages made `CARGO_BIN_EXE` pick the wrong
binary.

## The panic surface

The workspace denies the escape hatches instead of trusting review. In the root
manifest, applied through each member's `[lints] workspace = true`:

```
clippy::unwrap_used       clippy::expect_used         clippy::panic
clippy::todo              clippy::unimplemented       clippy::unreachable
clippy::unwrap_in_result  clippy::panic_in_result_fn  clippy::indexing_slicing
```

What each lint actually covers decides whether the gate is a wall or a curtain:
`panic` catches `panic!` only — `todo!`, `unimplemented!` and `unreachable!` are
three separate lints, and no clippy lint sees `assert!` at all. `unwrap_or*` is
not flagged, because it cannot panic. Tests keep the vocabulary: unit tests
through one `#![cfg_attr(test, allow(..))]` per crate root, and the `tests/`
files through the same list at their own root — a `tests/` file is its own
crate, so `cfg(test)` is false there.

Two hatches therefore remain in production code, both declared rather than
discovered in a crash dump:

- **Three `assert!`s in `induction-guest`'s builders** — `on` refuses a
  `__`-prefixed event, `on_wildcard` requires the `prefix.*` form, `hook`
  accepts only `__sleep`/`__wake`. The input is the extension author's own
  registration code, so these are startup failures with a message rather than
  runtime error paths. No lint can gate them (see above), and a fallible
  builder chain would push `?` into every extension for a check that can only
  fire on a typo — a review rule, kept deliberately.
- **One `unsafe` block** — `prctl(PR_SET_PDEATHSIG)` in `pre_exec`, so a dead
  host never orphans its extensions. `unsafe_code = "deny"` is workspace-wide
  and the single site carries a commented `#[allow(unsafe_code)]`.

Everything else on the surface is translation: `CallFrame::iterate` takes an
`IterateKind` instead of a `FrameKind`, so a `call`-shaped stream frame is
unrepresentable rather than debug-asserted away in release builds, and the wire
constructors fill a JSON object field by field instead of indexing into it.

## Status

The extraction is complete. Landed: protocol, host and guest crates (batches
1/1b/1c); `effector` rides `induction-protocol` for frames and the codec enum
and routes host frames through the `route_frame` dispatcher (batch 3);
`mudra`'s host is re-based on `induction-protocol` + `induction-host` with the
store-coupled event pull kept in `mudrad` (batch 2); aura's WS remote-effector
leg needed zero changes — `HostOp` and the pipe `HostFrame` are two transport
encodings of the one arm vocabulary, and the shared piece is the dispatcher,
not the session (resolved in ADR-0001 §Consequences). The internal Rust
identifiers were renamed in a later pass (`BgiSession` → `InductionSession`,
`BgiKind` → `SessionKind`, `bgi_*`/`nu_bgi_*` tests → `induction_*`);
historical ADR/PLAN passages citing the old names stay as written.