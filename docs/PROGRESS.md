# Sana — Build Progress & Task Tracker

This is the durable task log. Work pauses and resumes across sessions, so this
file is the source of truth for "what's done, what's next, and why". Read this
plus `docs/wiki/architecture.md` before continuing.

**How to resume:** read this file → run `cargo test` (should be green) → pick up
the next unchecked task under "Current milestone" / "Next up".

---

## Status snapshot

- **Current stage:** Stage 2 (SST/LSM) — **complete**.
- **Next stage:** Stage 3 (Attributes & Exact Search).
- **Done:** Stage 0 (Skeleton), Stage 1 (Durable Documents), Stage 2 (SST/LSM).
- **Tests:** `cargo test` green (52 tests); `cargo clippy --all-targets` clean.
- **Note:** post-Stage-2 code-review fixes applied (efficiency + stats +
  dedup); remaining findings tracked under "Stage 2 — code review follow-ups".
- **Last updated:** 2026-06-05.

---

## Milestones (mapped to architecture stages)

- [x] **Stage 0 — Skeleton decisions.** Internal value/schema types, `ObjectStore`
      trait + filesystem backend, manifest + WAL formats, golden serialization
      tests.
- [x] **Stage 1 — Durable documents.** Namespace lifecycle: create, append WAL,
      CAS-advance commit cursor, replay WAL → documents, strong primary-key
      lookup. Small CLI.
- [x] **Stage 2 — SST/LSM.** SST writer/reader, build doc SSTs from WAL
      (flush), compaction + tombstones, query from manifest SSTs + WAL overlay.
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

## Stage 2 — SST / LSM (done)

Reads no longer replay the whole WAL: documents live in immutable sorted SSTs
named by the manifest, with a bounded recent-WAL overlay on top. Shipped in
`src/sst.rs`, `src/doc.rs`, `src/indexer.rs`, manifest `doc_ssts`, and an
SST-aware read path in `src/namespace.rs`. On-disk:

```
namespaces/{ns}/index/g/{generation}/doc/flush-{seq}.sst   # one flush
namespaces/{ns}/index/g/{generation}/doc/compacted.sst     # a compaction
```

Stage 2 decisions / notes:

- **D15 — Generic byte-keyed SST.** `bytes -> bytes`, sorted keys, prefix-
  compressed blocks + restart array, per-block CRC, a loaded index block, and a
  fixed 32-byte footer (index handle + magic + version + index CRC). Backs docs
  now; reused for attribute/FTS/vector-address families later.
- **D16 — Whole-object SST load for Stage 2.** Reads pull the whole SST in one
  round trip and parse in memory. The format already supports ranged reads
  (footer → index → only needed blocks) as a later optimization with no on-disk
  change. Block-internal binary search via restarts is also deferred (the reader
  linearly scans a ≤4 KB block today).
- **D17 — Order-preserving `Id` keys.** `doc::encode_id` = tag byte (U64 < Uuid
  < String) then big-endian u64 / raw uuid / utf-8, so lexicographic SST order
  equals `Id`'s `Ord`. `min_id`/`max_id` in `SstMeta` let point lookups skip
  files.
- **D18 — Flush writes complete documents, not deltas.** Each touched id is
  resolved (base from existing SSTs + delta ops) and written whole, or as a
  tombstone. So newest-first reads return full documents and merges stay simple.
- **D19 — SST creation stamped by generation in the path.** SSTs are immutable
  and shared across manifest generations; the path records the creating
  generation, keeping names unique without a separate counter (matches the
  doc's generation-addressed layout). `doc_ssts` is newest-first.
- **D20 — Full compaction drops tombstones.** Compaction merges *all* doc SSTs
  into one; since nothing older remains, tombstones are dropped safely.

Known limitations to fix later:

- No LSM levels yet — `doc_ssts` is a flat newest-first list; compaction is
  all-or-nothing. Introduce levels/size-tiering when flush frequency grows.
- Orphaned SSTs from superseded generations are not GC'd (need an
  unreferenced-object sweep gated on manifest watermarks).
- No automatic flush trigger (backpressure on unindexed WAL bytes) — flush is
  manual via API/CLI. Wire a trigger when the indexing queue lands (Stage 9).
- `replay` still loads all SSTs fully; fine until namespaces get large.

---

## Stage 2 — code review follow-ups

A high-effort recall review of the Stage 2 diff ran after it landed. Outcomes:

**Fixed (committed as the review-fixes change):**

- **Efficiency — flush re-loaded every SST per touched id.** `flush` now loads
  the merged SST records once (`Namespace::sst_records`) and seeds bases from
  that map, instead of O(touched × ssts) object GETs.
- **Efficiency — overlay WAL read was sequential.** `read_overlay_ops` now
  issues the (known-up-front) WAL GETs concurrently via a `JoinSet` and
  re-orders results by sequence.
- **Stats — manifest counters went stale.** `flush` now sets
  `approx_row_count` (exact, base + delta) and `approx_logical_bytes` (sum of
  SST sizes); `compact` also sets `approx_logical_bytes`. Covered by
  `tests/indexer.rs::flush_and_compact_update_stats`.
- **Altitude — merge logic was duplicated.** The "merge all doc SSTs, newest
  wins" loop now lives once in `Namespace::sst_records`, shared by `replay`,
  `compact`, and `flush`. `lookup` intentionally keeps the point-get path
  (`sst_point_get`): early-stop + min/max pruning for a single id.

**Outstanding (address before/within Stage 3 — none fire under the current
single-process, epoch-0, trusted-storage assumptions, but they are real):**

- [ ] **Concurrency: non-atomic manifest publish.** `commit_manifest` `put`s
      the new body before the pointer CAS, so two indexers at the same `new_gen`
      let the CAS loser overwrite the winner's body. Fix when concurrent
      indexers become possible (write body under a content-unique/generation
      key only after — or make publish a single CAS). Latent today (manual,
      single-process flush).
- [ ] **Epoch-blind reads.** `read_overlay_ops` builds WAL keys with `to.epoch`
      and `flush`'s `from_seq >= commit.seq` compares only `seq`; both break if
      the WAL epoch ever rotates. Make the overlay range epoch-aware when epoch
      rotation is implemented.
- [ ] **Empty-batch / empty-SST churn.** `append` accepts an empty operations
      batch (advances the commit cursor); `flush` then writes a zero-row SST
      with `min_id/max_id = None` that can never be pruned. Reject empty
      batches and/or skip emitting an empty SST.
- [ ] **Point lookups load whole SSTs.** Implement the ranged read (footer →
      index → one block) the SST format already supports (D16).
- [ ] **Dead manifest field.** `wal_commit_cursor` is serialized but never
      written; either maintain it or remove it.
- [ ] **SST footer not checksummed.** Unlike blocks/index, footer fields aren't
      CRC'd, so accidental corruption can overflow/panic instead of erroring;
      add a footer checksum and use checked arithmetic on parsed offsets.
- [ ] **`u32` size/offset fields** (restart offsets, index key length) silently
      truncate for >4 GiB blocks/keys — widen or bound.
- [ ] **Test gaps.** No test round-trips a manifest with a populated `doc_ssts`
      (SstMeta serde uncovered), and none asserts `min_id/max_id` or exercises
      the prune path.
- [ ] **`load_manifest` vs `indexer::read_manifest`** duplicate the
      pointer→generation→body load; share one helper.

---

## Current milestone: Stage 3 — Attributes & Exact Search

Goal: typed schema + filtering + ordering + simple aggregation, then exact
vector kNN over filtered candidates. This is the first stage that makes Sana a
*search* engine rather than a key-value log.

Planned tasks (refine when started):

- [ ] Schema inference/checking: infer column types from upserts, validate on
      write, evolve `Schema.version`. Decide strictness (reject vs. coerce).
- [ ] Attribute inverted index as a new SST family (`attr/{col}/{value}` →
      bitmap/posting of ids). Reuse `sst.rs`; design an order-preserving
      composite key encoding (the `encode_id` note in `doc.rs`).
- [ ] Filter expressions (Eq, range, And/Or/Not) compiled to id sets, evaluated
      against attribute SSTs + WAL overlay. Start with equality + range.
- [ ] Order-by (primary key or one attribute) and count/sum aggregation.
- [ ] Exact vector kNN: brute-force distance over a filtered candidate set
      (L2/cosine/dot), top-k heap. Reference scalar kernels (SIMD is Stage 8).
- [ ] A query entry point (logical query → plan → execute) and a `query` CLI/
      API verb. Integration tests for filters, order-by, aggregation, kNN.

Open questions for Stage 3: bitmap representation (roaring-style vs. simple
sorted-id postings — start simple); how filters compose with the WAL overlay
(re-evaluate overlay docs against the predicate, like patch/delete-by-filter).

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
  main.rs                CLI (create/upsert/get/delete/list/flush/compact/demo)
  error.rs               shared Error / Result
  value.rs               Id, Value, VectorValue, Document
  schema.rs              ScalarType, ColumnType, Schema, ...
  object_store/
    mod.rs               ObjectStore trait, ObjectVersion, version_of
    fs.rs                FsObjectStore (filesystem backend)
  manifest.rs            NamespaceManifest, ManifestPointer, SstMeta (+ codecs)
  wal.rs                 WalCursor, WalOp, WalBatch (+ binary codec)
  sst.rs                 generic sorted-string-table writer/reader
  doc.rs                 Id key encoding (order-preserving) + DocRecord
  namespace.rs           Namespace: create/append + SST-aware replay/lookup
  indexer.rs             flush (WAL -> SST) and compaction
tests/
  common/mod.rs          assert_golden snapshot helper
  fs_object_store.rs     object store behavior (CAS, ranges, list, ...)
  manifest_codec.rs      manifest/pointer round-trip + golden JSON
  wal_codec.rs           WAL round-trip, corruption detection, golden bytes
  sst.rs                 SST round-trip, point get, corruption, golden bytes
  doc_codec.rs           Id key order + record round-trip
  namespace.rs           namespace lifecycle + durability across reopen
  indexer.rs             flush/compaction + SST+overlay merge semantics
  fixtures/              committed golden snapshots
```
