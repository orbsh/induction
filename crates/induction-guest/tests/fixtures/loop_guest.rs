//! The live-child fixture for the host's session tests: a real guest process
//! built from the guest crate, so the tests exercise spawn + PDEATHSIG +
//! handshake + a genuine pipe round trip instead of a mock.
//!
//! Handlers: `echo`, `ask_host` (a ctx round trip), `boom` (a failing handler),
//! `list` (an iterate producer). The human-facing showcase of the same shape
//! lives at `induction-guest/examples/loop.rs`.

use induction_guest::{Ctx, Guest};
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    Guest::new()
        .named("loop")
        .on("echo", |_ctx: &mut Ctx, args: Value| Ok(json!({ "echo": args })))
        .on("ask_host", |ctx: &mut Ctx, _args: Value| {
            let answer = ctx.invoke_verb("pages", json!({ "filter": "rust" }))?;
            Ok(json!({ "from_host": answer }))
        })
        .on("boom", |_ctx: &mut Ctx, _args: Value| {
            anyhow::bail!("deliberate failure for the error path")
        })
        .on("list", |_ctx: &mut Ctx, _args: Value| Ok(json!({ "done": false, "total": 3 })))
        .producer(|_ctx: &mut Ctx, args: Value| {
            let round = args.get("round").and_then(Value::as_u64).unwrap_or(0);
            Ok(json!({ "done": false, "item": round }))
        })
        .hook("__wake", |_ctx: &mut Ctx, _args: Value| Ok(json!({ "woke": true })))
        .run()
}