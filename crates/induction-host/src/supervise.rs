//! Driving sessions: the select loop, a convenience runner, and the supervisor.
//!
//! The loop shape is the one mudra's host already runs, with the store coupling
//! removed: two live sources (host-supplied event frames, the guest's channel),
//! every write budgeted, and a failed write ending the session rather than
//! silently losing frames. A host with a different event model (mudra pulls a
//! log window when a signal arrives) uses [`crate::session`] directly and keeps
//! its own loop — the library does not force an event model on the host.

use std::future::Future;
use std::time::Duration;

use induction_protocol::CallFrame;
use tokio::sync::mpsc;

use crate::ops::{HostOps, Routed, route_line};
use crate::process::{Process, Spec, spawn};
use crate::session::{Budgets, Session};

/// Why a session ended. Every variant is a supervisor's input, not an error
/// value for the guest.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The declared executable could not be spawned.
    SpawnFailed(String),
    /// `initialize` → `hello` did not complete (missing, malformed, or late).
    HandshakeFailed,
    /// The child closed its channel (or a read budget expired on an idle peer).
    GuestGone,
    /// A frame write missed its budget: the guest is not reading. The frame did
    /// not land — a host keeping a cursor must not advance it.
    WriteStalled,
    /// The host's event source closed: the host is shutting down.
    ShuttingDown,
}

/// Drive one live session: deliver host frames, serve guest arms.
///
/// Borrows of the read and write halves are disjoint by construction, which is
/// what lets both sit in one `select!`.
pub async fn run_driver<R, W>(
    session: &mut Session<R, W>,
    ops: &dyn HostOps,
    events: &mut mpsc::Receiver<CallFrame>,
    budgets: Budgets,
) -> Outcome
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let Session { reader, writer } = session;
    loop {
        tokio::select! {
            incoming = events.recv() => {
                match incoming {
                    Some(frame) => {
                        if !writer.write_line(&frame.to_line(), budgets.write).await {
                            return Outcome::WriteStalled;
                        }
                    }
                    None => return Outcome::ShuttingDown,
                }
            }
            line = reader.next_line(budgets.read) => {
                match line {
                    Some(line) => match route_line(ops, &line).await {
                        Routed::Reply(reply) => {
                            if !writer.write_line(&reply, budgets.write).await {
                                return Outcome::WriteStalled;
                            }
                        }
                        // `emit` carries no reply by contract; a `result` frame
                        // is the guest answering a delivered call (a host with
                        // no intercept plane has nothing to consume it with).
                        Routed::Accepted | Routed::Result(_) => {}
                        Routed::DecodeFailed => {
                            eprintln!("[induction] undecodable guest frame: {line}");
                        }
                    },
                    None => return Outcome::GuestGone,
                }
            }
        }
    }
}

/// Spawn, handshake, drive. The whole life of one extension in one call —
/// for hosts whose events arrive as ready-to-send frames.
pub async fn run_process(
    spec: &Spec,
    ops: &dyn HostOps,
    events: &mut mpsc::Receiver<CallFrame>,
    budgets: Budgets,
) -> Outcome {
    let mut process = match spawn(spec) {
        Ok(p) => p,
        Err(e) => return Outcome::SpawnFailed(format!("{}: {e}", spec.command_line())),
    };
    let hello = match process.session.handshake(budgets.handshake).await {
        Some(h) => h,
        None => {
            process.kill();
            return Outcome::HandshakeFailed;
        }
    };
    if hello.name != spec.name {
        eprintln!(
            "[induction] {}: hello name mismatch ({:?} declared, {:?} answered)",
            spec.name, spec.name, hello.name
        );
    }
    let outcome = run_driver(&mut process.session, ops, events, budgets).await;
    process.kill();
    outcome
}

/// Keep every declared extension alive: one task per declaration, the supplied
/// runner retried after `backoff`.
///
/// The backoff is constant on purpose: residency accounting (exponential
/// growth, jail-like limits, metering) is a policy a host may layer on top —
/// the library only guarantees "no hot loop, no orphan".
pub async fn supervise<F, Fut>(decls: Vec<Spec>, backoff: Duration, run: F)
where
    F: Fn(Spec) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    for spec in decls {
        let run = run.clone();
        tokio::spawn(async move {
            loop {
                run(spec.clone()).await;
                tokio::time::sleep(backoff).await;
            }
        });
    }
}

/// The supervisor used by every host in this workspace: constant backoff.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(10);

/// A `Process` whose session is left un-driven: the primitive form, for hosts
/// that own their loop. Exposed so a host never has to re-implement spawn.
pub fn spawn_only(spec: &Spec) -> std::io::Result<Process> {
    spawn(spec)
}