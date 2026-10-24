//! # induction-host
//!
//! The host half of **Induction**: spawn a declared executable as a *resident
//! extension*, handshake, deliver framed calls, and serve the ctx arms it
//! invokes back. No realm, no booth model, no code upload, no storage: the
//! extension is an executable whose entry point lives in a config, and the
//! host's job is the channel and the lifetime.
//!
//! ## What this crate is not
//!
//! - not a scheduler: the host supplies the event frames it wants delivered
//!   (mudra pulls a log window when its epoch changes; another host may deliver
//!   straight from memory). There is no store here, so nothing here persists —
//!   that is structural, not a configuration.
//! - not a sandbox: `bwrap`/cgroup/jail policy belongs to the host's spawn
//!   layer. This crate spawns what it is told to spawn (with PDEATHSIG, so a
//!   dead host never leaves orphans).
//! - not a transport zoo: [`session`] is generic over the byte channel, and
//!   [`process`] is one adapter over it. A WS-backed duplex or an in-memory
//!   pair drives the same session code.
//!
//! ## Minimal host
//!
//! ```no_run
//! use induction_host::{HostOps, InvokeCall, OpsFuture, Spec, Budgets, spawn, run_driver};
//! use induction_protocol::CallFrame;
//! use serde_json::{Value, json};
//!
//! struct Verbs;
//! impl HostOps for Verbs {
//!     fn invoke<'a>(&'a self, call: &'a InvokeCall) -> OpsFuture<'a> {
//!         let call = call.clone();
//!         Box::pin(async move {
//!             match call {
//!                 InvokeCall::Verb { verb, args } => Ok(json!({ "verb": verb, "args": args })),
//!                 InvokeCall::Booth { handler, .. } => Ok(json!({ "handler": handler })),
//!             }
//!         })
//!     }
//! }
//!
//! async fn drive(spec: Spec) {
//!     // No `.expect()`: a spawn failure is a returned outcome, not a crash.
//!     let Ok(mut process) = spawn(&spec) else { return };
//!     if process.session.handshake(Budgets::default().handshake).await.is_none() {
//!         process.kill();
//!         return;
//!     }
//!     let (_tx, mut rx) = tokio::sync::mpsc::channel::<CallFrame>(16);
//!     run_driver(&mut process.session, &Verbs, &mut rx, Budgets::default()).await;
//!     process.kill();
//! }
//! ```

// Unit tests keep the unwrap/panic vocabulary the production targets are
// denied: a panicking test is a failing test with a stack trace. `cfg(test)` is
// only on for this crate's own test build — the `tests/` crates carry the same
// allow at their own root.
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
))]

pub mod ops;
pub mod process;
pub mod session;
pub mod supervise;

pub use induction_protocol::parse_guest_value;
pub use induction_protocol::{Arm, CallFrame, Encoding, FrameKind, InvokeCall, IterateKind, Reply};
pub use ops::{HostOps, OpsFuture, Routed, route_frame, route_line};
pub use process::{Process, Spec, spawn};
pub use session::{Budgets, Reader, Session, Writer};
pub use supervise::{DEFAULT_BACKOFF, Outcome, run_driver, run_process, spawn_only, supervise};

/// The protocol revision this host speaks.
pub use induction_protocol::PROTOCOL;
