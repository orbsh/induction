# 0001 — Induction: a standalone host↔extension protocol (naming, boundary, first slice)

> **Languages:** [English](ADR-0001-induction.md) (primary) · [中文](ADR-0001-induction.zh-CN.md)

**Status:** Accepted (2026-10-12); first slice landed — `induction-protocol`,
`induction-host`, `induction-guest`, 31 tests green (`cargo test --workspace`).
**Date:** 2026-10-12

## Context

Two programs in this workspace need the same thing: a host embeds *resident
extension processes* that speak typed frames over a byte channel (stdio in the
common case), exchange calls and effects, and die with the host. The extension is
an executable named in a config — no code upload, no introspection pass, no
storage of code, no realm, no booth model.

Both implementations existed, and so did their vocabulary — three times over:

- **guest face** — `effector/crates/bgi-guest` (416 lines, serde_json +
  ciborium only: the one piece already shaped right).
- **host side** — `effector/crates/runtime/src/carrier/exec.rs` (825 lines) +
  `carrier/session.rs` (372), welded to the WS runtime, the embedded
  steel/python carriers, `bwrap` policy, the nushell fifo adapter, and the
  separate bare-`exec` one-shot carrier.
- **a trimmed copy** — `mudra/crates/mudrad/src/host.rs` (292 lines), with
  frames hand-written as `serde_json::json!` literals.

Three spellings of one contract produced drift, and the drift produced bugs that
neither side could see alone (see *Divergences found*, below). A fourth host
would produce a fourth spelling.

## Decision

### 1. Its own repository, three crates

`~/world/induction`, workspace with `induction-protocol`, `induction-host`,
`induction-guest`.

Reason for a separate repo rather than living inside `effector`: `effector` is
the execution tier of a particular distributed system; this is an
embedding/transport ABI that any Rust program may take. Consumers must not
inherit effector's product line, release cadence or CI identity to get a frame
codec.

Dependency whitelist, enforced by review (the workspace file states it):

- `induction-protocol` — `serde`, `serde_json`, `ciborium`. Nothing else, ever.
- `induction-guest` — the above + `anyhow`.
- `induction-host` — protocol + `tokio` (rt/io/process/sync/time) + `libc`
  (one `prctl` call).

Excluded by ruling: `okm`/`fjall`, any `aura-*`, `effector-config`, `steel-core`,
`pyo3`, `sandbox-runtime-rs`, `ureq`, `tokio-tungstenite`. A transport other than
the process adapter and any sandbox policy belong to the HOST.

Three crates, not one crate with features, because the dependency weights
differ by half: a guest must be buildable without tokio or libc, and a host must
not carry the guest facade.

### 2. Boundary: the channel and the lifetime, nothing else

In: frame types, arm names, tier vocabulary, schema surface, codec declaration,
session I/O with budgets, the ctx-arm routing trait, the process adapter
(PDEATHSIG + `kill_on_drop`), the constant-backoff supervisor.

Out, by ruling: storage (there is no store here, so *nothing in this library
persists* — structural, not configurable); event sourcing (the host supplies the
frames it wants delivered; mudra pulls a log window when its epoch changes,
another host may deliver from memory); timers and queues (protocol names +
`HostOps` hooks, implementations are the host's); sandbox policy (a host-side
spawn concern); code upload/introspection stores (a prebuilt executable declares
its own schema — this is exactly why "just name an executable" works here while
aura's uploaded-booth path needs introspection collectors and a blob store).

### 3. Canonical wire choices (union of what shipped, one spelling chosen)

- **Codec** — declared by the parent through `INDUCTION_ENCODING`
  (`"json"`|`"cbor"`; absent or unknown → json; a bad value never fails a
  session). An env, not an argv: fifo-shaped specs pass the author's own
  arguments.
- **Arms** — the ADR-0043 ctx members as dotted method paths, adopted verbatim
  (`invoke`, `iterate.start/next/dispose`, `store`, `emit`,
  `timer.register/cancel`, `queue.depth/skip_to_head`, `schema`). Locked by a
  test.
- **`host_reply` error key** — canonical `error`; mudra's pre-Induction `err`
  spelling is tolerated on decode so a running pair keeps working across the
  rename.
- **Schema** — rich form (`receives` / `wildcard_receives` / `hooks` /
  `storage`) is canonical; the flat `{"on": […]}` spelling is accepted on
  decode. Both declaration paths are typed and both are answered by the guest:
  the load-time handshake (`initialize` → `hello`) and the pull form (a `call`
  whose event is `interface_schema`, which is also the form upload-time
  introspection needs).
- **`invoke` payload** — canonical is the accepted ctx definition: the nested
  booth call (`{type, key, handler, args}`). The flat `{verb, args}` spelling
  (mudra's current control surface) decodes to `InvokeCall::Verb`; a host routes
  whichever it can serve, and the migration is mudra's, not the protocol's.
- **Decode strictness** — an unknown arm, kind, or shape is a decode failure
  (the version-mismatch signal), never a defaulted dispatch. A guest's
  undecodable line is surfaced by the host, not absorbed.
- **Reply discipline** — `emit` carries NO reply (fire-and-forget by contract);
  every other arm answers. A host that lacks an arm answers an explicit error
  value, never a silent Null.

### 4. Naming: Induction

Judged by: no host-domain noun in the name; it must describe the real mechanism
(a mediated, non-contact coupling between two isolated processes); it must carry
`X`/`X-host`/`X-guest` family forms and its own version axis; it must not collide
with a standard acronym.

Chosen: **Induction** — a current in one circuit induces one in a *galvanically
isolated* circuit; both sides keep their own state and memory, the coupling is
mediated, and by mutual inductance it runs both ways. Galvanic isolation is the
process boundary; the induced current does its work in its own circuit. The
precise physics of "not tightly coupled" is resonant inductive coupling (k<1) —
the name keeps the mechanism, not the discipline.

Rejected, with reasons:

- **BGI (Booth Gateway Interface)** — carried aura's domain noun ("Booth") and a
  wrong mechanism word ("Gateway" is *host-call resolution* in this codebase —
  `aura/crates/engine/src/host_wire.rs` — not transport).
- **Actuator** — a near-synonym of `effector` (which already owns that role in
  this workspace), already rejected once as a name for `effector` itself, and,
  decisively, it names the wrong party: the extension is the decision plane and
  the host executes effects, so the actuator is the host side, not the channel
  this library is.
- **Electromagnetism / Lorentz force** — a discipline name and a law name,
  neither of which describes the coupling layer; the field is already aura's
  metaphor (aura = the field, effector = the part in it that turns decision into
  action), so naming a host-neutral library after it repeats BGI's error in
  metaphor form; and a physics phenomenon is undiscoverable for someone looking
  for an extension host.
- **Relay** — device layer (wrong party again), one-way (coil → contact) while
  the channel is bidirectional, and "relay" is already a booth type name in
  aura's own tests.
- **Yoke** — occupied by a Rust crate (`yoke`, ICU4X; present in aura's
  Cargo.lock).
- **Residency** — describes lifetime, while the load-bearing property is the
  mediated connection, control and operation.
- **Scion / Symbiont** — botanical/biological registers: the coupling layer reads
  as a gesture rather than a mechanism.

### 5. Rename list (wire-visible items first)

Before the vocabulary is frozen for outside guests:

1. `BGI_ENCODING` → `INDUCTION_ENCODING` (carrier writer + guest reader).
2. Carrier/language value `"bgi"` → `"induction"` — **not** a cosmetic string:
   `PersistedBooth.language` is a persisted field
   (`aura/crates/realm/src/meta.rs:176`, 12 registration sites). With no real
   data in the meta store (user decision 2026-10-12) this takes the
   delete-and-rebuild policy, same as mudra's prototype store.
3. Error strings (`"bgi/nu adapter: …"`), fixture/example binary names
   (`bgi_loop`, `bgi_demo`).
4. Frame discriminators (`initialize`, `hello`, `call`, `result`, `host`,
   `host_reply`) contain no `bgi` — no change needed (verified).

**Status (2026-10-12).** All four items landed. Item 1: the carrier writes
`induction_protocol::ENCODING_ENV` and the fixtures/guest read the codec through
`induction_protocol::Encoding`. The codec enum followed to its one home —
`Encoding` is the only one left (effector-protocol's and aura-booth's copies are
gone; aura-booth re-exports this crate's, so its own ADR-0037 §2 ruling stands).
Item 2: the carrier/language value is `"induction"` (effector's carrier arm, the
registration sites in aura/prism, and the control-plane examples in effector's
USAGE). Item 3: the error strings read `induction carrier:` / `induction/nu
adapter:`, and the fixtures are named `induction_loop` / `induction_nu.nu`.
Item 4 needed nothing, as recorded.

Left outside the list on purpose: internal Rust identifiers (`BgiSession`,
`BgiKind`, `bgi_bin`, test function names such as
`bgi_nu_booth_store_emit_roundtrip`) and the prose sweep — ADRs and PLANs cite
some of those by name, so renaming them would turn a vocabulary change into doc
drift. The sweep of `BGI|bgi` in prose is its own pass (HANDOFF §7).

## Divergences found while building this (each with its site)

Facts, not plans — all verified in the shipped code:

1. **`host_reply` error key disagreed.** mudra's host wrote `{"err": …}`
   (`mudrad/src/host.rs`), while the carrier and the guest shim read `"error"`
   (`carrier/exec.rs`, `bgi-guest/src/lib.rs`). A verb failure reached a
   bgi-guest extension as "host_reply carried neither ok nor error". One
   vocabulary home makes this class impossible; the tolerant decode covers the
   migration window.
2. **The handshake was one-sided.** `initialize`/`hello` exists only in mudra;
   aura/effector never send it and learn the schema through the
   `interface_schema` call. Both forms are now in the protocol, and the shipped
   guest answers both.
3. **`invoke` payload shape disagreed** (nested booth call vs flat verb). Both
   decode; the canonical form is aura's ctx definition.
4. **bgi-guest's iterate producer binding was unreachable.** `pull()` looks up a
   handler named `__next__`, but `on()` asserts that `__`-prefixed names are
   reserved — so no API call could bind a producer. Only a real pair test sees
   this: `induction-guest` adds `.producer(handler)`, its own builder beside
   `.hook()` (a stream mechanism is not a lifecycle hook).
5. **mudra's event payload is a host convention**, not wire vocabulary: it wraps
   log rows as `{event_id, at, snapshot}`. Recorded here so it is not mistaken
   for protocol when mudra migrates.
6. **A cargo trap encountered building the tests**: two packages declaring a bin
   with the same name made `CARGO_BIN_EXE_<name>` resolve to the other package's
   binary (the host's live test silently ran the guest crate's fixture). Fixture
   bin names are now distinct (`raw_guest` vs `loop_guest`).

## Consequences

- `mudra` (batch 2) replaces its hand-written frame path with
  `induction-protocol` + `induction-host`, keeps its store-coupled event pull in
  `mudrad`, and switches its error key to `error`.
- `effector` (batch 1) moves its guest shim out to `induction-guest` and takes
  `induction-protocol` for frames and the codec enum.
- `aura` (batch 3, refactor not drop-in) has the realm's process-carrier branch
  delegate to `induction-host`. Its WS remote-effector leg stays in aura as
  another transport over the same session code. **Resolved 2026-10-13 (batch 3
  as landed):** the two host-call types are NOT collapsed — `HostOp` (WS, serde-
  tagged, id-paired across concurrent streams) and the pipe `HostFrame`
  (in-band, serially paired) are two TRANSPORT encodings of one arm VOCABULARY
  (`Arm`); the collapse that matters is the dispatcher's: `induction-host`
  `route_frame` is the codec- and transport-independent core, and both hosts
  drive it (effector's process carrier over a `HostOps` bridge adapter, mudra's
  verb host directly). What is protocol is shared to the last arm name; what is
  transport stays with the transport.
- History in this workspace keeps its ADRs: historical decision text is not
  rewritten, current-state text is updated, and the rename carries a terminology
  note (the `probe → effector` precedent).

## Not decided here

- Whether the bare one-shot `exec` carrier (a different protocol by design) ever
  joins this repo.
- Whether `VirtualStorage`-style tier/backlog vocabulary is ever added: the
  library has no store, so `queue.*` is a protocol name a *host with a queue*
  serves.
- The `receives` per-handler object is today empty; it is the reserved place for
  tier/deadline declarations, which stay host policy until a guest needs to
  declare them.