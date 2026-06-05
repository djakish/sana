# Sana — Build Progress & Task Tracker

This is the durable task log. Work pauses and resumes across sessions, so this
file is the source of truth for "what's done, what's next, and why". Read this
plus `docs/wiki/architecture.md` before continuing.

**How to resume:** read this file → run `cargo test` (should be green) → pick up
the next unchecked task under "Current milestone" / "Next up".

---

## Status snapshot

- **Current stage:** Stage 1 (Durable Documents) — **complete**.
- **Next stage:** Stage 2 (SST/LSM).
- **Done:** Stage 0 (Skeleton), Stage 1 (Durable Documents).
- **Tests:** `cargo test` green (30 tests); `cargo clippy --all-targets` clean.
- **Last updated:** 2026-06-05.

---

## Milestones (mapped to architecture stages)

- [x] **Stage 0 — Skeleton decisions.** Internal value/schema types, `ObjectStore`
      trait + filesystem backend, manifest + WAL formats, golden serialization
      tests.
- [x] **Stage 1 — Durable documents.** Namespace lifecycle: create, append WAL,
      CAS-advance commit cursor, replay WAL → documents, strong primary-key
      lookup. Small CLI.
- [ ] **Stage 2 — SST/LSM.** SST writer/reader/range iterator, build doc SSTs
      from WAL, compaction + tombstones, query from manifest + WAL overlay.
- [ ] **Stage 3 — Attributes & exact search.** Schema inference/checking,
      attribute inverted indexes (eq/range), filters, order-by, count/sum,
      exact vector kNN over filtered candidates.
- [ ] **Stage 4 — ANN v0.** KMeans/IVF per column, immutable vector postings,
      probe + scan + rerank, recall endpoint.
- [ ] **Stage 5 — Native filtering.** Cluster-level summaries, row-level
      bitmaps, filter-aware ANN traversal, filtered recall.
- [ ] **Stage 6 — SPFresh local rebuild.** Mutable posting append, version map,
      split/merge/reassign background jobs.
- [ ] **Stage 7 — Full-text search.** Tokenizer, BM25, block postings,
      vectorized MAXSCORE, hybrid multi-query.
- [ ] **Stage 8 — RaBitQ & kernels.** Per-cluster codes, quantized query path,
      portable then SIMD kernels.
- [ ] **Stage 9 — Object-store operations.** Brokered indexing queue, warm-cache
      endpoint, branch/copy/export, pinning.

---

## Stage 1 — Durable Documents (done)

End-to-end on the filesystem object store: create a namespace, append WAL
batches, CAS-advance a lightweight commit cursor, replay into documents, and
look up by key. Shipped in `src/namespace.rs` + `src/main.rs` (CLI) with
integration tests in `tests/namespace.rs`. On-disk layout matches the
architecture doc:

```
namespaces/{ns}/manifest/current        # ManifestPointer -> generation
namespaces/{ns}/manifest/g/{gen}.json   # immutable manifest body
namespaces/{ns}/wal_commit/current      # CAS commit cursor (WalCursor as JSON)
namespaces/{ns}/wal/{epoch}/{seq}.wal   # durable batches
```

Stage 1 decisions / notes:

- **D12 — Lightweight commit cursor separate from the manifest.** The write
  path CAS-advances `wal_commit/current` per commit; the manifest only changes
  when indexing publishes files. Realizes Principle 2 (write durability vs.
  indexing freshness). Manifest's own `wal_commit_cursor`/`indexed_cursor` are
  snapshots set at index-publish time (Stage 2+).
- **D13 — Single-writer-per-namespace append.** In-process append lock + cursor
  CAS. WAL object written with `put` (overwrite) before the cursor advances, so
  a crashed prior attempt at that seq is a harmless orphan we overwrite. Same
  single-process caveat as D4; cross-process append needs `put_if_absent` +
  explicit orphan reconciliation (future).
- **D14 — Patch = create-or-update; null clears a field.** Patch onto a missing
  id creates a partial doc; a `Value::Null` attribute removes the field.

Known limitations to fix in later stages:

- `replay`/`lookup` are O(WAL) — full scan per call. Stage 2 SSTs fix this.
- No idempotency-key dedup yet (field is plumbed through the WAL batch).
- No schema inference/validation yet (Stage 3); attributes are free-form.
- Single WAL epoch only; epoch rotation is unused.

---

## Current milestone: Stage 2 — SST / LSM

Goal: stop replaying the whole WAL. Build immutable sorted document files
(SSTs), fold committed WAL into them via an indexer step, and serve reads from
manifest-named SSTs plus a bounded recent-WAL overlay. Add compaction and
tombstone cleanup.

Planned tasks (refine when started):

- [ ] `sst.rs`: SST writer/reader with data blocks (prefix-compressed keys),
      a block index (min/max key + offsets), and a footer (magic, version,
      checksums, index offset). Batched range iterator (no per-item yield).
- [ ] Key encoding for the `doc/{id}` family that sorts lexicographically by
      `Id` (define a canonical byte ordering for U64/Uuid/String).
- [ ] Indexer step: read WAL `indexed_cursor`..`wal_commit`, merge into a new
      doc SST, write it, then CAS-advance the manifest (new generation naming
      the SST + updated `indexed_cursor`).
- [ ] Read path: load manifest → read SSTs named by it → merge with WAL overlay
      after `indexed_cursor`. Lookup becomes SST point-read + small overlay.
- [ ] Compaction: merge SSTs within the doc family, drop overwritten values and
      tombstones past a retention horizon, update approx stats. Never mutate in
      place.
- [ ] Golden SST format test + indexer/compaction integration tests.

Open question for Stage 2: levels/manifest representation of SST sets (start
with a single level / full list of files; introduce levels when compaction
needs them).

---

## Decision log

Decisions I (the implementer) made; the user delegated architectural calls.
"D#" are stable references.

- **D1 — Async + Tokio from day one.** The `ObjectStore` contract is async to
  match S3/GCS later; retrofitting async is painful. Tokio multi-thread runtime.
- **D2 — `async-trait` for `ObjectStore`.** Enables `Arc<dyn ObjectStore>`
  (native async-fn-in-trait isn't dyn-compatible without boxing). Boxing cost is
  irrelevant next to object-store I/O latency.
- **D3 — Content-addressed `ObjectVersion`.** `version_of` = hash of bytes.
  Gives correct CAS-by-content, survives restarts, needs no sidecar. ABA is a
  non-issue because the only CAS target (manifest pointer) strictly increases
  its generation, so content never repeats. `DefaultHasher::new()` has a fixed
  seed → versions are stable across runs.
- **D4 — FS CAS via single in-process lock + atomic rename.** Correct for
  single-process local dev (which is all early Sana needs). Crash-safe via
  temp-file + `rename`. **Limitation:** not safe across OS processes; real
  cross-writer CAS will come from S3/GCS conditional writes. Documented in
  `object_store/fs.rs`.
- **D5 — `get` returns bytes *and* version (`GetResult`).** Minor divergence
  from the doc's `get -> Bytes`. Avoids a read-modify-CAS race that a separate
  `get` + `head` would introduce.
- **D6 — Manifest = pretty JSON; WAL = binary.** Manifest is the human-readable
  catalog (doc names it `.json`). WAL is a binary envelope (magic / format
  version / body len / crc32) wrapping a `postcard` body — compact, and the CRC
  detects torn/corrupt writes.
- **D7 — `BTreeMap` for all maps in serialized types.** Deterministic byte
  output → golden tests are meaningful and diffs are stable.
- **D8 — Golden/snapshot fixtures in `tests/fixtures/`.** First run records,
  later runs compare; committed to git so format drift surfaces in review.
- **D9 — `bytes::Bytes` in the store API.** Cheap clones (caching) and cheap
  slicing (range reads), matching the production design.
- **D10 — Minimal dependency set.** tokio, async-trait, serde(+json), postcard,
  crc32fast, bytes, thiserror; tempfile (dev). No HTTP/ANN/job-queue crates yet
  (Stage 0 discipline).
- **D11 — Single crate, module-per-subsystem.** Organized as if it could become
  a workspace later, per the architecture doc.

---

## Repo map (current)

```
src/
  lib.rs                 module exports
  main.rs                Stage-1 CLI (create/upsert/get/delete/list/demo)
  error.rs               shared Error / Result
  value.rs               Id, Value, VectorValue, Document
  schema.rs              ScalarType, ColumnType, Schema, ...
  object_store/
    mod.rs               ObjectStore trait, ObjectVersion, version_of
    fs.rs                FsObjectStore (filesystem backend)
  manifest.rs            NamespaceManifest, ManifestPointer (+ codecs)
  wal.rs                 WalCursor, WalOp, WalBatch (+ binary codec)
  namespace.rs           Namespace: create/append/replay/lookup
tests/
  common/mod.rs          assert_golden snapshot helper
  fs_object_store.rs     object store behavior (CAS, ranges, list, ...)
  manifest_codec.rs      manifest/pointer round-trip + golden JSON
  wal_codec.rs           WAL round-trip, corruption detection, golden bytes
  namespace.rs           namespace lifecycle + durability across reopen
  fixtures/              committed golden snapshots
```
