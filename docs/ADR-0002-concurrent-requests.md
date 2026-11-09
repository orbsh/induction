# 0002 — One frame at a time is the loop's choice, not the protocol's shape

> **Languages:** [English](ADR-0002-concurrent-requests.md) (primary) · [中文](ADR-0002-concurrent-requests.zh-CN.md)

**Status:** Accepted (2026-11-02) — steps 1–3 are landed; the channel carries
concurrent requests at both ends.
**Date:** 2026-10-31 (accepted 2026-11-02)

## Context

A guest is a **resident** process, and often a single one for a whole booth type.
ADR-0001 fixed the frame format and said nothing about what the process does
while a request is outstanding — whether it may take a second one, whether two
handlers may run at once, whether replies must come back in the order the
requests arrived. It does not have to: the protocol's business is the bytes, and
a process's own threading is the process's business. What it does say is that the
guest is resident, and a resident extension that cannot take the next request
until the previous one returns is a per-request process wearing a resident's
name.

The channel as built is serial at BOTH ends:

- **Host.** `supervise.rs` is one loop — read a line, `route_line(ops, &line).await`,
  write the reply line — and `ops.invoke(&call).await` sits inline in that loop. A
  call that takes minutes blocks every later frame of the link, including
  frames that have nothing to do with it.
- **Guest.** `read_host_reply` takes the NEXT frame off the channel as the answer
  to the request it just wrote (`induction-guest/src/lib.rs`, "no host_reply" is
  its only failure mode beyond a malformed frame). It never looks at an id.

The correlation field already exists, in one direction, and the end that could use
it ignores it: `CallFrame` — the host's request — carries `id: u64`
(`induction-protocol/src/frame.rs`), and the guest's answer is
`ResultFrame {"id": N, "result": …}`. That is the pairing vocabulary, declared
from the first slice, and `read_host_reply` never reads it. The reverse direction
has no vocabulary at all: a guest's ctx call is
`{"host": {"type": …, "args": …}}` and the host answers
`{"host_reply": {"ok": …}}` — no id on either side, because until now neither
side could have two calls in flight.

**The case that surfaced it.** Gravity's turn serves several requests at once: a
compression pass runs on its own thread while the user's answer streams, and a
turn that long outlives a device's next keystroke has to accept the next round
while it is still running. Under the old loop a second request was not even READ
until the first one's reply returned, and if it were, the guest's reply reader
would hand request B the answer written for request A. Neither failure is a
protocol defect; both are the loop assuming it is alone.

## Decision

**The channel carries concurrent requests, and `id` pairs them.** Three changes,
in this order. None of them alters a frame's shape; they add two fields, both on
the guest's own call envelope — the `id` the reply must echo (step 2) and the
request the call belongs to (`call`, step 3).

**1. Host: one route per request, one owner of the writer.** The read loop
starts a route for each frame and keeps reading; a finished route's reply is
written with the request's own `id`. The writer stays single-owner because a line
is the unit on the wire — two writers would interleave bytes, not requests. (As
landed the routes are in-flight futures in the same loop rather than spawned
tasks: `HostOps` is a borrowed `dyn`, so spawning would force `Arc` + `'static`
on every host for nothing. Concurrency is the same either way; the difference is
where the scheduler lives.)

**2. Guest: a reader of its own, and replies matched by id.** The channel splits
in two: a reader thread (or task) demultiplexes replies into a
`HashMap<id, oneshot>`; a round trip waits on ITS id and nothing else. A guest
that never issues a concurrent call is unaffected — the map always has exactly
one entry, and the program's shape does not change. The guest's outbound call is
the one envelope that gains a field: it carries the `id` it will wait on, and the
host echoes that `id` inside `host_reply`. Additive, not a shape change — both
parsers ignore keys they do not read, so an old host still serves a new guest; it
just cannot answer two of its calls at once.

**3. Only then** may one booth serve two requests at once. Nothing else in the
protocol moves: `iterate` pulls already address a `stream_id`, and concurrency is
a property of the process, not of a stream.

**A ctx frame names the request it serves (`host.call`).** The `id` above pairs a
reply with its caller; it says nothing about WHICH request is being served, and a
host that multiplexes several calls into one child has to know that: the ctx
functions a handler reaches for belong to that handler's call (its scope, its
bridge), and answering round A's tool call with round B's context is wrong in a
way no error surfaces. So the guest stamps the request id it is serving onto every
ctx call it writes. Absent (a serial guest, an older guest) the host falls back to
the single armed call — unambiguous exactly while one call is in flight.

**The alternative — declare FIFO reply ordering as part of the protocol — is
rejected.** It would make every long call a stall for the whole link, which
contradicts what resident means, and it would push a scheduling policy into a
format that has no business having one.

## Consequences

- **Replies may arrive out of order.** Anything that assumed "the next frame is my
  answer" is precisely what step 2 removes; a guest that still assumes it is a
  guest with a latent mispairing.
- **A slow handler stops stalling its siblings.** It also stops being a natural
  backpressure signal: the host can now queue arbitrary work into a guest, so a
  per-request timeout is needed where today only the write is budgeted.
- **The host must keep the writer single-owner**, including on shutdown — the
  task spawning is the easy half.
- **Tests have to interleave.** The existing pair tests issue one request at a
  time; they prove nothing about this. The first test worth writing is two
  requests with the LATER one answered FIRST.
- **The failure's blast radius shrinks.** A handler that returns an error is
  already answered, not fatal (`Routed::Reply(Reply::Error)`), and an undecodable
  guest frame is logged and skipped. What is fatal today is a handler that
  PANICS: it panics the loop, and the link dies with it. With one task per
  request, the panic is confined to that request's task — the caller sees a
  channel that answered nothing, the link lives on, and the isolation question
  becomes "what does a request's caller do when its task dies", which is a
  per-request answer instead of a per-link one.

## Landed (2026-11-02)

- **Host.** `run_driver` keeps the in-flight routes in a vector it polls itself —
  no futures crate, no `'static` bound — and writes every finished reply with the
  id `route_line_tagged` recovered from the call. It stops reading at 64 live
  routes, so a guest that never waits throttles itself instead of growing the
  host's memory. Nothing but replies and host event frames reaches the writer:
  one owner, as decided.
- **Guest.** The run loop never executes a handler. It answers the handshake,
  hands replies to the call waiting on their id (a `HashMap<u64, Sender<Reply>>`
  behind a mutex), and dispatches requests to a worker. A serial guest — the
  default — keeps ONE worker thread in arrival order; `Guest::concurrent(true)`
  gives each request its own thread. A reply without an id still resolves when
  exactly one call is pending: that is a pre-ADR host, and it is why the change is
  additive rather than breaking.
- **Wire.** A guest's ctx call is `{"host": {"type": …, "id": N, "call": R}}` —
  `id` is the ctx call's own correlation id, `call` is the request the handler is
  serving — and the host answers `{"host_reply": {"id": N, "ok"|"error": …}}`.
  Every one of those keys is ignored by the parsers that predate it.
- **Tests.** `two_requests_run_at_once_and_each_waits_on_its_own_reply` (guest
  crate, real process): two handlers, two live ctx calls, the LATER request
  answers FIRST, and each call carries a distinct id.
  `the_driver_routes_two_guest_calls_at_once` (host crate, duplex): a slow route
  does not delay a fast one, and both replies echo their caller's id.

## Not decided here

- **Who drives a stream nobody pulls.** `iterate` is consumer-driven: no pull, no
  progress. A resident guest holding a live stream between turns still needs a
  driver that outlives the requesting entry (a resident loop, a timer, a queue).
  That is the caller's design, not the channel's.
- **Fairness and priorities.** None exist; none are ruled out. A request that never
  answers costs its slot until the caller gives up.
- **Cancellation per request** was on this list and is no longer: ADR-0003 rules the
  frame (`{"type": "interrupt", "id": N}`) and the guest-side token, both landed.
- **Whether a guest should BE concurrent.** This ADR makes it POSSIBLE. A guest
  that wants one-at-a-time behaviour keeps it by holding its own lock, which is
  the right place for that choice.