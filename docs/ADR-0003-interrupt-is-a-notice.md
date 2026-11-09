# ADR-0003 — An interrupt is a notice, and the token is the only honest form of it

**Status:** Accepted (2026-11-09), landed. Supersedes the "cancellation per request" line in
ADR-0002's *Not decided here*.

## Context

ADR-0002 built the channel so a reader loop never runs a handler: it answers the handshake,
hands replies to the call waiting on their id, and dispatches requests to workers. It then
recorded that per-request cancellation did not exist, and did not rule it out.

Two things made the question concrete. Aura ruled that a stop is a control bit and that its
second stage is the realm lifting an in-flight operation (aura ADR-0047). Gravity ruled that a
cancelled turn must not be a stored marker (gravity ADR-0008). Between them sits this channel,
and the question is exact: *the parent stops waiting for a call — how does the guest find out?*

## Decision

**1. The shape is a notice, not a request and not an arm.** `{"type": "interrupt", "id": N}`.
It is not an `Arm` because an arm is a guest→host capability and this runs the other way; it
is not a request because nothing is answered. The reader loop claims a frame in a fixed order —
`initialize` → reply → **interrupt** → call — and the shapes are disjoint, so the order is a
reading, not a race (`an_interrupt_is_a_notice_the_reader_claims_before_the_call_arm`).

**2. Receipt is possible because the loop is free.** The frame is claimed by the loop, which
never runs a handler; that is the structural reason this decision can exist at all. A handler
blocked inside one long operation does not need to be reading — the loop is.

**3. Inside the guest it becomes a `CancelToken`.** A flag with a `wait(timeout)`, on the ctx.
Not a delivery: a handler is a plain function on a worker thread, and there is nothing to
deliver to. Not an abort: a running thread cannot be aborted safely, and a runtime that
pretended otherwise would be selling the guest a lie.

**4. The bound is named, not hidden.** A handler that never looks at the token cannot be
stopped from inside. The host's clock — killing the process — is the outer bound, and it is
the carrier's business, not this crate's. The token's job is to make the cheap case cheap; it
must not be documented as a guarantee it cannot keep.

**5. Early interrupts are not lost.** A serial guest queues requests; an interrupt can arrive
for a call that has not started. The loop leaves a raised token for that id, and the worker
takes it with `entry().or_default()`, so the call begins cancelled rather than late. A late
interrupt — for a call that already answered — leaves one raised token behind; the table is
bounded by cancels, not by traffic.

## Consequences

- **No vocabulary change.** `Arm::ALL` is untouched; the frozen-arm test in `wire.rs` does not
  move. What grows is a frame type and the host's `Writer::interrupt(id, budget)`.
- **Old guests ignore it.** A guest that predates this decodes the frame as undecodable, logs
  it, and keeps the link — which is the behaviour a stray host frame already had.
- **Nothing on the wire says whether it worked.** The parent's own terminal value is the
  answer; the guest's result, if it ever produces one, is discarded. This is the same choice
  aura made at the call boundary, and it is what keeps the guest from having to report on a
  stop it may not have noticed.