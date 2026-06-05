# Sana — Build Progress & Task Tracker

This is the durable task log. Work pauses and resumes across sessions, so this
file is the source of truth for "what's done, what's next, and why". Read this
plus `docs/wiki/architecture.md` before continuing.

**How to resume:** read this file → run `cargo test` (should be green) → pick up
the next unchecked task under "Current milestone" / "Next up".

---

## Status snapshot

- **Current stage:** Stage 0 (Skeleton) — **complete**.
- **Next stage:** Stage 1 (Durable Documents).
- **Tests:** `cargo test` green (21 tests); `cargo clippy --all-targets` clean.
- **Last updated:** 2026-06-05.

---

## Milestones (mapped to architecture stages)

- [x] **Stage 0 — Skeleton decisions.** Internal value/schema types, `ObjectStore`
      trait + filesystem backend, manifest + WAL formats, golden serialization
      tests.
- [ ] **Stage 1 — Durable documents.** Namespace lifecycle: create, append WAL,
      CAS-advance manifest, replay WAL → documents, strong primary-key lookup.
      Small CLI/local API.
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

## Current milestone: Stage 1 — Durable Documents

Goal: end-to-end on the filesystem object store — create a namespace, append a
WAL batch, CAS-advance the manifest, replay into documents, and look up by key.

Planned tasks (subject to refinement when started):

- [ ] `namespace.rs`: `Namespace` handle over an `Arc<dyn ObjectStore>` with the
      object-key layout from the architecture doc (helpers for manifest pointer,
      manifest body, and `wal/{epoch}/{seq}.wal` keys).
- [ ] Create namespace: write generation-0 manifest + `manifest/current`
      pointer via `put_if_absent`.
- [ ] Append WAL batch: allocate next `WalCursor`, write `wal/{epoch}/{seq}.wal`,
      CAS-advance the manifest's `wal_commit_cursor`. (Group-commit *shape* even
      though batches are committed one at a time at first.)
- [ ] Replay: read manifest, stream WAL from `indexed_cursor`..`wal_commit`,
      fold ops (upsert/patch/delete) into an in-memory document map.
- [ ] Strong lookup by primary key via replay overlay.
- [ ] Wire a tiny CLI in `main.rs` (create / upsert / get / list) over a local
      store dir, plus integration tests.

Open question to settle in Stage 1: WAL epoch/seq allocation & group-commit loop
structure (single-writer per namespace vs. per-process). Lean single-writer
per namespace, committing batches sequentially, with the loop structured so
multiple in-flight writes can later coalesce into one WAL object.

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
  main.rs                Stage-0 banner stub (CLI lands in Stage 1)
  error.rs               shared Error / Result
  value.rs               Id, Value, VectorValue, Document
  schema.rs              ScalarType, ColumnType, Schema, ...
  object_store/
    mod.rs               ObjectStore trait, ObjectVersion, version_of
    fs.rs                FsObjectStore (filesystem backend)
  manifest.rs            NamespaceManifest, ManifestPointer (+ codecs)
  wal.rs                 WalCursor, WalOp, WalBatch (+ binary codec)
tests/
  common/mod.rs          assert_golden snapshot helper
  fs_object_store.rs     object store behavior (CAS, ranges, list, ...)
  manifest_codec.rs      manifest/pointer round-trip + golden JSON
  wal_codec.rs           WAL round-trip, corruption detection, golden bytes
  fixtures/              committed golden snapshots
```
