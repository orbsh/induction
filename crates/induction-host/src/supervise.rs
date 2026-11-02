//! Driving sessions: the select loop, a convenience runner, and the supervisor.
//!
//! The loop shape is the one mudra's host already runs, with the store coupling
//! removed: two live sources (host-supplied event frames, the guest's channel),
//! every write budgeted, and a failed write ending the session rather than
//! silently losing frames. A host with a different event model (mudra pulls a
//! log window when a signal arrives) uses [`crate::session`] directly and keeps
//! its own loop — the library does not force an event model on the host.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use induction_protocol::CallFrame;
use tokio::sync::mpsc;

use crate::ops::{HostOps, Routed, route_line_tagged};
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
    /// The child closed its channel (or the read errored) — the only
    /// termination signal a live session produces on its own.
    GuestGone,
    /// A frame write missed its budget: the guest is not reading. The frame did
    /// not land — a host keeping a cursor must not advance it.
    WriteStalled,
    /// The host's event source closed: the host is shutting down.
    ShuttingDown,
}

/// One route in flight: the reply's correlation id (as the guest tagged the
/// call) and what routing produced.
type RouteFuture<'a> = Pin<Box<dyn Future<Output = (Option<u64>, Routed)> + Send + 'a>>;

/// The in-flight set. A vector rather than a stream type on purpose: the driver
/// polls it itself (see [`first_done`]) so no futures crate is needed and the
/// routes keep borrowing `ops`.
type InFlight<'a> = Vec<RouteFuture<'a>>;

/// Drive one live session: deliver host frames, serve guest arms.
///
/// The guest's calls are routed CONCURRENTLY (ADR-0002): a frame starts a route
/// future, the loop keeps reading, and each finished route is written with the
/// id its caller tagged. Writes still have one owner — this loop — because a
/// line is the unit on the wire, so two writers would interleave bytes rather
/// than requests.
///
/// The driver keeps reading while fewer than [`MAX_INFLIGHT`] routes are live;
/// past that it stops reading until one finishes. Without the cap a guest that
/// never waits would grow the vector without bound, and the memory would be the
/// host's, not the guest's.
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
    /// One step of the loop: a route finished, a host event arrived, or the
    /// guest sent a line. Kept as a value so the select's arms never borrow the
    /// in-flight vector twice.
    enum Step {
        Done((Option<u64>, Routed)),
        Event(Option<CallFrame>),
        Line(Option<String>),
    }

    let Session { reader, writer } = session;
    let mut inflight: InFlight<'_> = Vec::new();
    loop {
        let any = !inflight.is_empty();
        let room = inflight.len() < MAX_INFLIGHT;
        let step = tokio::select! {
            Some(done) = first_done(&mut inflight), if any => Step::Done(done),
            incoming = events.recv() => Step::Event(incoming),
            line = reader.next_frame(), if room => Step::Line(line),
        };
        match step {
            Step::Done((id, routed)) => match routed {
                // The id echoed here is the guest's, not a host sequence: the
                // guest picked it and is waiting on exactly that one.
                Routed::Reply(reply) => {
                    let line = match id {
                        Some(id) => reply.to_line_with_id(id),
                        None => reply.to_line(),
                    };
                    if !writer.write_line(&line, budgets.write).await {
                        return Outcome::WriteStalled;
                    }
                }
                // `emit` carries no reply by contract; a `result` frame is the
                // guest answering a delivered call (a host with no intercept
                // plane has nothing to consume it with).
                Routed::Accepted | Routed::Result(_) => {}
                Routed::DecodeFailed => eprintln!("[induction] undecodable guest frame"),
            },
            Step::Event(incoming) => match incoming {
                Some(frame) => {
                    if !writer.write_line(&frame.to_line(), budgets.write).await {
                        return Outcome::WriteStalled;
                    }
                }
                None => return Outcome::ShuttingDown,
            },
            Step::Line(line) => match line {
                Some(line) => inflight.push(Box::pin(async move {
                    route_line_tagged(ops, &line).await
                })),
                None => return Outcome::GuestGone,
            },
        }
    }
}

/// How many guest calls the driver keeps in flight at once.
const MAX_INFLIGHT: usize = 64;

/// Poll every in-flight route and hand back the first that finished.
///
/// Hand-rolled because the workspace's dependency whitelist has no futures
/// crate, and `JoinSet` would demand `'static` futures — these borrow `ops` for
/// exactly as long as the driver lives. Every future is polled with the caller's
/// waker, so a wake re-enters the select and lands here again.
async fn first_done<'a, T>(
    futs: &mut Vec<Pin<Box<dyn Future<Output = T> + Send + 'a>>>,
) -> Option<T> {
    poll_fn(|cx| {
        let mut ready = None;
        for (i, fut) in futs.iter_mut().enumerate() {
            if let Poll::Ready(v) = fut.as_mut().poll(cx) {
                ready = Some((i, v));
                break;
            }
        }
        let Some((i, v)) = ready else {
            return Poll::Pending;
        };
        drop(futs.swap_remove(i));
        Poll::Ready(Some(v))
    })
    .await
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
