//! The live-child fixture for the host's session tests: a guest written
//! straight against `induction-protocol` (no guest crate — the host crate must
//! not depend on the guest crate, and this keeps the fixture a bin of THIS
//! package, which is what makes cargo build it for these tests).
//!
//! It is intentionally raw: the point is the host's spawn/handshake/IO path,
//! not the guest ergonomics. The ergonomic guest face is proven separately, by
//! `induction-guest`'s own pair test against this same protocol.
//!
//! Handlers: `echo`, `ask_host` (a ctx round trip), `boom` (a failing handler),
//! `list` (an iterate producer), plus the `initialize` handshake and the
//! `interface_schema` pull.

use induction_protocol::{
    Arm, CallFrame, Hello, HostFrame, Reply, ResultFrame, SCHEMA_EVENT, Schema,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut out = stdout.lock();

    let mut receives = BTreeMap::new();
    for event in ["echo", "ask_host", "boom", "list"] {
        receives.insert(event.to_string(), Value::Object(serde_json::Map::new()));
    }
    let schema = Schema {
        receives,
        wildcard_receives: Vec::new(),
        hooks: vec!["__wake".to_string()],
        storage: None,
    };

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return, // parent closed the channel: session over
            Ok(_) => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };

        // The load-time handshake.
        if v.get("type").and_then(Value::as_str) == Some("initialize") {
            let hello = Hello {
                name: "loop".to_string(),
                schema: schema.clone(),
            };
            let _ = writeln!(out, "{}", hello.to_line());
            let _ = out.flush();
            continue;
        }

        let Some(req) = CallFrame::from_value(&v) else {
            continue;
        };
        let result = if req.event == SCHEMA_EVENT {
            schema.to_value()
        } else {
            match req.event.as_str() {
                "echo" => json!({ "echo": req.args }),
                "ask_host" => {
                    // One ctx round trip across the process boundary.
                    let frame = HostFrame::new(
                        Arm::Invoke,
                        json!({
                            "type": "mudra", "key": "w",
                            "handler": "pages", "args": { "filter": "rust" },
                        }),
                    );
                    let _ = writeln!(out, "{}", frame.to_line());
                    let _ = out.flush();
                    let mut reply = String::new();
                    let _ = reader.read_line(&mut reply);
                    match serde_json::from_str::<Value>(reply.trim())
                        .ok()
                        .and_then(|v| Reply::from_value(&v))
                    {
                        Some(Reply::Ok(value)) => json!({ "from_host": value }),
                        Some(Reply::Error(e)) => Value::String(format!("host error: {e}")),
                        None => Value::String("no host_reply arrived".to_string()),
                    }
                }
                "boom" => Value::String("deliberate failure for the error path".to_string()),
                "list" => json!({ "done": false, "total": 3 }),
                other => Value::String(format!("no such handler: {other}")),
            }
        };
        let _ = writeln!(
            out,
            "{}",
            ResultFrame::new(Value::from(req.id), result).to_line()
        );
        let _ = out.flush();
    }
}
