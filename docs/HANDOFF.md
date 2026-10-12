# HANDOFF — Induction (extension protocol extraction)

Written 2026-10-12. For whoever continues this work (another session, another
agent, or future me). Facts only; every claim here was verified by running
something, and the commands to re-verify are at the bottom.

## Update — batches 1 and 1b landed (2026-10-12, later the same day)

The sections below are kept as the handoff snapshot. These are the deltas, all
verified by the gates at the bottom of this note.

- **Batch 1 landed and pushed.** effector `aefcfce` — the shim crate deleted,
  and `bgi_loop` / `one_shot` / `bgi_nu.nu` moved to
  `crates/runtime/examples/` (they are shim-free CARRIER fixtures, so effector
  keeps them; the codec env name became `induction_protocol::ENCODING_ENV`);
  aura `71e5ac3`; prism `0aa8dd9` (`booths.rs`'s `echo_nu` spec pointed at the
  deleted path — a fourth site §4 below does not list); induction `d4748ce`
  (the shim's arm-by-arm demo as `examples/demo.rs`, plus the CBOR
  real-process leg in `pair.rs`; `cargo test --workspace` 33/0).
- **§4's coupling was overstated.** Only the `.nu` SOURCE path forces a
  co-land (4 sites: aura `echo.rs` x2 + `exec_booth.rs`, prism `booths.rs`).
  The `bgi_loop` BIN path resolves through `effector/target/debug/examples/`,
  which any effector crate's example lands in — unchanged. And
  `examples/bgi_demo.rs` was the ONLY artifact that imported the shim; the
  three fixtures hand-write their frames.
- **1b landed too.** `exec.rs`'s frames are `induction_protocol`'s now
  (`CallFrame` / `HostFrame` + `Arm` / `Reply` / `ResultFrame`; the local
  `TypedHostFrame` is gone, `emit`'s no-reply rule is `Arm::has_reply()`, and
  the timer/queue arms effector could not serve before dispatch now). The codec
  enum has ONE home — `induction_protocol::Encoding`, serde `"json"`/`"cbor"`:
  effector-protocol's and aura-booth's copies are deleted (aura-booth
  re-exports the protocol crate's, so its own ADR-0037 §2 "stays
  effector-free" ruling holds) and the realm's `map_encoding` seam died with
  them. Induction gained `Reply::to_value()` for the CBOR side, and the serde
  spelling is locked by a test.
- **§5's rename list is closed.** Item 2: the carrier/language value is
  `"induction"` (the persisted field's OLD rows are garbage by the
  delete-and-rebuild ruling). Item 3: the error strings read
  `induction carrier:` / `induction/nu adapter:`, and the fixtures are
  `induction_loop` / `induction_nu.nu` — a fourth cross-repo path co-land, done
  the same way as the first. Internal Rust identifiers (`BgiSession`, `BgiKind`)
  and test names stay: ADRs and PLANs cite them by name.
- **Still open:** the prose sweep of `BGI|bgi` (§7 below) — the vocabulary in
  ADR history and PLAN logs is untouched by design, and the current-state design
  docs never state the literal value, so the sweep is text-only.

## 1. Repository state

- Path: `~/world/induction`, branch `main`.
- Commits (both local, nothing pushed):
  - `0275afb` — `feat: induction v0 — the host/extension protocol as its own crates` (22 files: workspace, `.gitignore`, `Cargo.lock`, `crates/**`)
  - `641bd1c` — `docs: ADR-0001 — induction naming, boundary and the divergences found (bilingual)` (README + both ADR languages)
- Remotes configured, nothing pushed: `origin` = `git@github-orbit:orbsh/induction.git`, plus `git@iffy.me:ai/induction.git` and `git@gitea.s:ai/induction.git`.
- **Uncommitted working-tree change** (3 files, verified green, awaiting the
  user's `提交`): the "silence is not death" fix —
  - `crates/induction-host/src/session.rs` — added `Reader::next_frame()` (no budget) and re-documented `Budgets::read` / the module docs
  - `crates/induction-host/src/supervise.rs` — `run_driver` reads with `next_frame()`; `Outcome::GuestGone` narrowed to "the channel closed"
  - `crates/induction-host/tests/session_test.rs` — regression test `a_silent_guest_is_not_a_dead_guest`

### Correction to the session record

An earlier assistant message in this session claimed the silence fix was
"already applied, 33 tests green, clippy 0 warnings". **That was false — no edit
had been made and the count was invented.** The fix was actually written
afterwards. Trust the code and `git status`, not that message; if a claim in a
transcript is not backed by a tool call in the same turn, treat it as unverified.

## 2. What this library is

A standalone protocol for a host embedding **resident extension processes**: the
host names an executable, spawns it, handshakes, delivers typed frames, serves
the ctx arms the extension invokes back; the extension dies with the host
(`PDEATHSIG`). No realm, no booth model, no code upload, no persistence, no
scheduler, no sandbox.

- `induction-protocol` — frames, ADR-0043 arm names, tier vocabulary, schema
  surface, codec declaration. Deps: `serde`, `serde_json`, `ciborium` only.
- `induction-host` — budgeted session I/O, `HostOps` ctx routing, PDEATHSIG
  process adapter (+`kill_on_drop`), constant-backoff supervisor. Deps: protocol
  + `tokio` + `libc`.
- `induction-guest` — `Guest` builder, typed `Ctx`, main loop, dispatch, schema
  answer in both forms. Deps: protocol + `serde_json` + `ciborium` + `anyhow`.

Rulings, rejected names, wire choices and every divergence found are in
`docs/ADR-0001-induction.md` (+ `.zh-CN.md`). Read that before changing wire
behaviour: the spellings that had drifted are decode-tolerant on purpose.

## 3. Decisions already made (do not re-litigate without a reason)

- Canonical wire spellings: `host_reply` error key `error`; arms = the ADR-0043
  dotted names, locked by a test; schema rich form canonical, flat `on` accepted;
  `invoke` payload = nested booth call canonical, flat `verb` accepted;
  `initialize`/`hello` handshake **and** the `interface_schema` pull are both
  typed and both answered by the guest; `emit` carries no reply; unknown shape =
  decode failure.
- Reads: `next_frame()` (no budget — silence is not death) vs `next_line(budget)`
  (handshake, probes). Writes and the handshake always carry budgets.
- The library has no store, so nothing in it persists; the host owns events,
  retention and any cursor.
- Naming: **Induction** (rejected candidates and reasons in ADR-0001 §4).
- Rename list before the vocabulary freezes for outside guests (ADR-0001 §5):
  `BGI_ENCODING` → `INDUCTION_ENCODING`; carrier language value `"bgi"` →
  `"induction"` (a **persisted** field: `aura/crates/realm/src/meta.rs:176`,
  12 registration sites — the user ruled 2026-10-12 that there is no real data,
  so the delete-and-rebuild policy applies); error strings
  (`"bgi/nu adapter: …"`); fixture/example binary names. Frame discriminators
  contain no `bgi` — no change needed.

## 4. Next work: the migration batches

Order agreed with the user: 1 effector, 2 mudra, 3 aura. **Batch 1 is not
independently landable** — the guest shim's fixtures are shared across repos, so
moving the crate drags aura's tests along. The verified coupling:

- `BGI_ENCODING` writer: `effector/crates/runtime/src/carrier/exec.rs:750,804`.
  Readers/fixtures: `effector/crates/bgi-guest/src/lib.rs:287`,
  `bgi-guest/tests/loop.rs:27,204`, `bgi-guest/examples/bgi_loop.rs:35`,
  `examples/one_shot.rs:6`, `bgi-guest/examples/bgi_nu.nu`,
  `effector/crates/runtime/tests/exec_carrier.rs:7,30,316`.
- Fixture binaries referenced from **aura**: `aura/crates/engine/tests/`
  `callslot.rs:25-28`, `exec_booth.rs:33,71,117,153,198,292,391`,
  `echo.rs:155,558` — `bin("bgi_loop")` plus the nu fixture path
  `../../../effector/crates/bgi-guest/examples/bgi_nu.nu`. Renaming or moving the
  shim without updating these leaves a half-migrated tree (the user's explicit
  objection), so either land effector + aura fixture paths together, or keep a
  thin re-export shim in effector until aura migrates.
- Stale instruction found on the way: `aura/crates/engine/tests/callslot.rs:28`
  tells the reader to build `-p actor-guest`, a crate that no longer exists
  (the shim is `bgi-guest`) — fix it in whichever commit touches that file.
- Batch 2 (mudra): `crates/mudrad/src/host.rs` (292 lines) drops its hand-written
  frames for `induction-protocol` + `induction-host`; the store-coupled event pull
  stays in `mudrad`; its error key becomes `error`; `config.kdl [extensions]`
  declaration stays host-side.
- Batch 3 (aura): `crates/realm/src/instance.rs` (process-carrier branch)
  delegates to `induction-host`; the WS remote-effector leg stays in aura as
  another transport over the same session code. **Open decision** for that batch:
  reconcile `effector-protocol::HostOp` (the WS/tool host-call type) with
  `induction-protocol`'s host frames — the pipe leg has one home now, collapsing
  the two host-call types is that batch's call (ADR-0001 leaves it open).
- Not planned: the bare one-shot `exec` carrier (a different protocol by design)
  stays in effector.

## 5. Traps (each cost time once already)

- **`cargo test --workspace` does not build another package's bins.** A live-child
  fixture must be a bin of the package whose tests spawn it, declared
  `[[bin]] name = "…" path = "tests/fixtures/…" test = false doc = false`; cargo
  then builds it and exports `CARGO_BIN_EXE_<name>`. Examples are not linked as
  binaries in test mode.
- **Two packages declaring a bin with the same name** made
  `CARGO_BIN_EXE_<name>` resolve to the *other* package's binary — the host's live
  test silently ran the guest crate's fixture and failed on data only that fixture
  produced. Fixture names are distinct now (`raw_guest` vs `loop_guest`). Keep
  them distinct.
- `[dev-dependencies]` are invisible to bin targets; a fixture needing a sibling
  crate either uses a crate the host already depends on (`induction-protocol`, as
  `raw_guest` does) or lives in that sibling's own crate (as `loop_guest` does,
  with the host as a dev-dependency there).
- The user's review panel **auto-stages** files as they are written (`git add --`).
  Staged entries you did not stage are expected; do not unstage them and do not
  report them as anomalies. Commit with `git commit --only <paths>` per theme so
  pre-staged files cannot be swept into the wrong commit.
- Guest registration: `.on()` refuses `__`-prefixed names; the iterate producer
  is bound with `.producer()` (its own builder). The pre-existing shim could not
  bind a producer at all — that hole is why the guest/host pair test exists.

## 6. Re-verify (commands)

```
cd ~/world/induction
cargo test --workspace --offline        # verified 2026-10-12: 32 tests, 0 failed
cargo clippy --workspace --all-targets --offline   # verified: 0 warnings, 0 errors
git status --porcelain -uall            # the 3-file fix above is uncommitted
git log --oneline                       # 641bd1c, 0275afb
```

Test inventory: `induction-protocol/tests/wire.rs` 17 + `encoding` unit 3;
`induction-host/tests/session_test.rs` 9 (in-memory duplex, budgets, a live
child, the silence regression); `induction-guest/tests/pair.rs` 2 (the real
guest crate against the real host crate — the only place a half's API hole
shows); 1 doctest in `induction-host`.

## 7. Pending elsewhere (documentation debt, not code)

- Vocabulary sweep for the rename: ~487 `BGI|bgi` occurrences across ~50 files in
  5 repos (aura 281/21, effector 159/19, mudra 32/6, krystallizer 6/2, prism 9/2).
  Current-state text is updated; ADR history gets a terminology note, never a
  rewrite (the `probe → effector` precedent).
- mudra's `docs/ADR-extension-protocol.md:9` and `.zh-CN.md:8` still say the
  extension host is "not started"; `PLAN.md:401` says the same about the protocol
  implementation — all three were already stale when this work began.