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

## Tests

```
cargo test --workspace
```

31 tests: the wire contract (decode strictness, arm vocabulary, reply shapes,
schema forms), in-memory session behaviour (handshake, budgets, write stalls,
driver outcomes), two live-child process tests (spawn + PDEATHSIG + handshake +
round trip + ctx routing + iterate rounds), and a doctest.

The live tests spawn real children. Each lives in the package that owns the
fixture binary — cargo builds a package's own bins for its integration tests and
exports `CARGO_BIN_EXE_<name>`, so no manual build step is ever needed and a test
can never run against a stale fixture. Fixture bin names are distinct across
packages (`raw_guest` in `induction-host`, `loop_guest` in `induction-guest`):
two bins sharing a name across packages made `CARGO_BIN_EXE` pick the wrong
binary.

## Status

First slice: protocol, host and guest land here; the workspace's existing hosts
(`mudrad`) and the shared substrate (`effector`) migrate in later batches, then
`aura`'s realm delegates its process-carrier branch. The migration order and the
wire-visible rename list are in ADR-0001.