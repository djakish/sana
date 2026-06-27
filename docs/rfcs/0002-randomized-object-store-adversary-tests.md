# RFC 0002: Randomized object-store adversary tests

- **Status:** Draft (for review)
- **Author:** djakish (AI-assisted)
- **Created:** 2026-06-27
- **Tracking:** `docs/TODO.md` → "P1: Add randomized object-store adversary
  tests"
- **Touches invariants:** durability, consistency, crash safety. This is a
  testing-infrastructure RFC; it ships no production behavior change but is meant
  to *find* durability bugs in everything else.

## 1. Summary

Add a seeded, deterministic fault-injecting `ObjectStore` decorator
(`FaultingObjectStore`) plus a property-test harness that drives random
write/flush/compact/query episodes against it and checks a fixed set of
invariants after every step. Add codec property/fuzz coverage for the frame,
WAL, manifest, and SST decoders. The goal is to search the interleaving space
Sana's hand-written crash-window tests cannot enumerate, the way FoundationDB,
TigerBeetle, and Jepsen search theirs.

## 2. Motivation

Sana already has good *targeted* crash and race tests, but each one encodes a
single scenario an author thought of:

- `tests/write.rs::PauseFirstCommitReadStore` — pause a read at one seam.
- `tests/namespace.rs::FailWalCommitCasStore` — fail one specific CAS.
- `tests/indexer.rs::PromoteOrphanBeforeFinalGcScanStore` — one GC publish race.
- `tests/query.rs::CountingStore`, `tests/sst.rs::ByteCountingStore` — counting,
  not faulting.

These are valuable but cannot find the interleaving nobody scripted. The whole
correctness argument of Sana rests on a small object-store contract (8 methods)
and on ordering rules ("publish immutable objects before CAS-publishing the
catalog"). That is exactly the kind of surface that deterministic
simulation testing was invented for:

> "A deterministic simulation … lets you find bugs that would take centuries of
> real-world running to encounter, and reproduce them perfectly."
> — Will Wilson, FoundationDB, *Testing Distributed Systems w/ Deterministic
> Simulation* (Strange Loop 2014)

turbopuffer states the guarantees we must not violate (`docs/guarantees.md`):
durable writes on `200`, atomic batches, consistent reads by default, "object
storage is the only stateful dependency … all concurrency control is delegated
to object storage." Those become the invariants the harness asserts.

## 3. Current state in Sana

- The contract under test: `ObjectStore` (`src/object_store.rs`) —
  `get`, `get_range`, `put`, `put_if_absent`, `compare_and_set`, `list`,
  `delete`, returning content-addressed `ObjectVersion`s.
- Backends: `FsObjectStore`, `S3ObjectStore`; decorators `MeteredObjectStore`,
  `CachingObjectStore`. The S3 backend already models ambiguous success
  (a write whose `2xx` was lost) and reconciles it (D73), so the harness must be
  able to *produce* ambiguous success to exercise that path.
- Dev-dependencies today: `tempfile`, `tower` only. No `proptest`, no
  `arbitrary`, no fuzz target.

## 4. Design

### 4.1 `FaultingObjectStore`

A decorator wrapping any `Arc<dyn ObjectStore>`, driven by a seeded PRNG so every
run is reproducible from a single `u64` seed (printed on failure).

```text
FaultingObjectStore {
    inner: Arc<dyn ObjectStore>,
    rng:   deterministic, seed-derived, per-operation
    policy: FaultPolicy { per-method probabilities }
}
```

Fault modes, chosen to match real object-store failure surfaces and Sana's own
retry/reconcile logic:

1. **Transient error** — return a retryable error (the classes
   `S3ObjectStore::send_retrying` already retries). Asserts the retry path.
2. **Ambiguous success** — perform the write on `inner`, then return an error
   anyway (the byte landed; the caller doesn't know). This is the single most
   valuable mode: it directly attacks conditional-write reconciliation (D73) and
   WAL/manifest idempotency.
3. **Latency / reorder** — delay an operation so concurrent operations
   interleave differently. With a deterministic async runtime this enumerates
   orderings.
4. **Lost-then-found** — make a `put` visible only after N subsequent
   operations (models eventual visibility; S3 is strongly consistent today, so
   this mode is **off by default** and used only to assert we don't *depend* on
   it).
5. **Crash** — stop issuing operations from a task at a random point
   (process-crash analog), then reopen the namespace and continue.

Determinism note: scripts cannot use wall-clock or RNG that breaks replay. The
harness threads the seed explicitly and derives all randomness from it, so a
failing seed reproduces byte-for-byte.

### 4.2 Episode harness (stateful property test)

A `proptest` (or hand-rolled) state machine that generates a random program over
one or more namespaces:

```text
op ::= upsert(doc) | delete(id) | patch_by_filter | conditional_write
     | flush | tier | compact | maintain_vectors | query(filter|ann|text)
     | branch | crash+reopen
```

After **every** step, check the invariants in §4.3 against a **reference model**:
an in-memory `BTreeMap<Id, Document>` updated by the same accepted operations.
Indexed query results must equal a brute-force scan of the model (this is the
"oracle" technique: differential testing against a trivially-correct
implementation).

### 4.3 Invariants checked after each step

From `docs/TODO.md` plus turbopuffer's guarantees:

- **I1 — committed cursor never regresses.** `wal_commit/current` is
  monotonic per epoch.
- **I2 — accepted writes are readable.** Any operation that returned success is
  visible to a subsequent strongly-consistent read (durable-writes guarantee).
- **I3 — manifests parse and are internally consistent.**
  `indexed_cursor ≤ commit`, every referenced object key is well-formed.
- **I4 — no dangling references.** Every immutable object named by
  `manifest/current` exists in the store.
- **I5 — index == scan.** Indexed query results (attribute, BM25, ANN-with-exact-
  recheck) equal the reference model's brute-force answer. ANN is approximate, so
  assert the candidate set after the live-document recheck, not raw ANN order.
- **I6 — idempotency.** Replaying a conditional/filter write with the same
  idempotency key yields the original outcome (atomic-batch guarantee).
- **I7 — branch isolation.** Writes to a branch never appear in the source and
  vice versa (see RFC 0001).

A violated invariant prints the seed, the operation log, and the failing step so
it is replayable and minimizable.

### 4.4 Codec fuzzing

Add `cargo-fuzz`/`proptest` targets for the frame, WAL, manifest, and SST
decoders. Property: **decode must never panic, over-read, or silently
truncate** — it returns `Ok` or a typed `Error` (this enforces the
"reject corruption explicitly" invariant). Round-trip property: `decode(encode(x))
== x` for generated values, and golden fixtures stay byte-stable (the existing
compatibility fixtures are the seed corpus).

## 5. Dependency and determinism choices

The repo prizes a minimal dependency set (D10). Proposed posture:

- Add `proptest` and `cargo-fuzz` as **dev-dependencies only** — zero impact on
  the shipped crate.
- For deterministic async interleaving, evaluate `madsim` / `turmoil` (Rust
  deterministic runtimes) but do **not** require them for v1: a single-threaded
  `tokio` runtime plus the `FaultingObjectStore`'s explicit latency/reorder mode
  already gives reproducible orderings for the object-store contract, which is
  the only concurrency surface that matters here.

Open question Q1: is adding `proptest` + `cargo-fuzz` to dev-deps acceptable, or
should v1 hand-roll a seeded generator to keep even dev-deps minimal? (Proposed:
use `proptest`; it is the standard and its shrinker pays for itself.)

## 6. Alternatives considered

1. **More hand-written scenario tests.** Doesn't scale; misses unknown
   interleavings — the entire motivation.
2. **Full deterministic-simulation runtime (madsim) from day one.** Highest
   power, but a large dependency and rewrite of test scaffolding. Defer until the
   `FaultingObjectStore` + property harness shows it is insufficient.
3. **Jepsen-style external black-box testing.** Powerful for distributed
   systems with real clocks/networks, but Sana's coordination is entirely the
   object-store contract; an in-process faulting decorator tests the same surface
   far faster and reproducibly. Borrow Jepsen's *checker* mindset (Elle-style
   consistency checking) without its operational weight.

## 7. Rollout / milestones

1. `FaultingObjectStore` with modes 1–2 (transient, ambiguous-success) + seed
   plumbing; retrofit two existing scenario tests onto it to prove parity.
2. Episode harness with invariants I1–I4 over a single namespace.
3. Add I5 (index==scan) across flush/tier/compact.
4. Add crash+reopen (mode 5) and I6 (idempotency).
5. Add branch ops and I7 (pairs with RFC 0001).
6. Codec fuzz targets + corpus from golden fixtures.

## 8. References

- Will Wilson / FoundationDB, *Testing Distributed Systems with Deterministic
  Simulation* (Strange Loop 2014):
  https://www.youtube.com/watch?v=4fFDFbi3toc ;
  FoundationDB testing: https://apple.github.io/foundationdb/testing.html
- TigerBeetle VOPR (deterministic simulator) and "Viewstamped Replication made
  simulation-testable": https://github.com/tigerbeetle/tigerbeetle
- Antithesis — autonomous deterministic testing of whole systems:
  https://antithesis.com/
- Jepsen and Elle (Kyle Kingsbury) — consistency checking / anomaly detection:
  https://jepsen.io , https://github.com/jepsen-io/elle
- `proptest` (Rust stateful property testing):
  https://github.com/proptest-rs/proptest ; `cargo-fuzz`:
  https://github.com/rust-fuzz/cargo-fuzz
- `madsim` / `turmoil` — deterministic async runtimes for Rust:
  https://github.com/madsim-rs/madsim , https://github.com/tokio-rs/turmoil
- turbopuffer guarantees: `sources/turbopuffer-export/docs/guarantees.md`.
- Sana internals: `src/object_store.rs`, the existing decorators in
  `tests/{write,namespace,indexer,query,sst}.rs`, `docs/PROGRESS.md` D73
  (S3 ambiguous-success reconciliation).
