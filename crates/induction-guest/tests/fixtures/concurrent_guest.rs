//! The concurrency fixture (ADR-0002): two handlers that each do a ctx round
//! trip, one of them after a deliberate delay.
//!
//! With `.concurrent(true)` the guest dispatches both requests on their own
//! threads, so `fast` answers before `slow` even though `slow` arrived first.
//! Each handler waits on the reply to ITS call — two calls are in flight at
//! once, so a host that answers in order (or without echoing ids) stalls one of
//! them, and the pair test hangs instead of passing by luck.

use std::time::Duration;

use induction_guest::{Ctx, Guest};
use serde_json::{Value, json};

fn probe(ctx: &mut Ctx, who: &str) -> anyhow::Result<Value> {
    let answer = ctx.invoke_verb("probe", json!({ "who": who }))?;
    Ok(json!({ "who": who, "from_host": answer }))
}

fn main() -> anyhow::Result<()> {
    Guest::new()
        .named("concurrent")
        .concurrent(true)
        .on("slow", |ctx: &mut Ctx, _args: Value| {
            std::thread::sleep(Duration::from_millis(200));
            probe(ctx, "slow")
        })
        .on("fast", |ctx: &mut Ctx, _args: Value| probe(ctx, "fast"))
        .run()
}