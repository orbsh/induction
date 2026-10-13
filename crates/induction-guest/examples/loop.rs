//! A minimal Induction guest — the fixture the host tests spawn, and the
//! smallest complete example of the guest face.
//!
//! Handlers: `echo` (returns its args), `ask_host` (a ctx round trip back into
//! the host), `list` (an iterate producer with the guard counter in this
//! process's memory — the residency IS the extension state). The schema the
//! host learns is assembled from these registrations, not hand-written.

use induction_guest::{Ctx, Guest};
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    Guest::new()
        .named("loop")
        .on("echo", |_ctx: &mut Ctx, args: Value| {
            Ok(json!({ "echo": args }))
        })
        .on("ask_host", |ctx: &mut Ctx, _args: Value| {
            let answer = ctx.invoke_verb("pages", json!({ "filter": "rust" }))?;
            Ok(json!({ "from_host": answer }))
        })
        .on("boom", |_ctx: &mut Ctx, _args: Value| {
            anyhow::bail!("deliberate failure for the error path")
        })
        .on("list", |_ctx: &mut Ctx, _args: Value| {
            Ok(json!({ "done": false, "total": 3 }))
        })
        .producer(|_ctx: &mut Ctx, args: Value| {
            let round = args.get("round").and_then(Value::as_u64).unwrap_or(0);
            Ok(json!({ "done": false, "item": round }))
        })
        .hook("__wake", |_ctx: &mut Ctx, _args: Value| {
            Ok(json!({ "woke": true }))
        })
        .run()
}
