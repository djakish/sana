# Sana — Build Progress & Task Tracker

This is the durable task log. Work pauses and resumes across sessions, so this
file is the source of truth for "what's done, what's next, and why". (The
pre-development design document and research material were retired once the
build caught up with them; this log carries the decisions forward.)

**How to resume:** read this file → run `cargo test` (should be green) → pick up
the next unchecked task under "Current milestone" / "Next up".

---

## Status snapshot

- **Current stage:** post-Stage-13 production hardening. The engine remains
  feature-complete for its educational goal; work now follows the prioritized
  multi-pod gaps in `docs/TODO.md`.
- **Next up:** per-namespace maintenance ownership and safe object reclamation:
  publisher safety points, durable maintenance jobs, and durable GC candidates
  before any online GC.
- **Done:** Stage 0 (Skeleton), Stage 1 (Durable Documents), Stage 2 (SST/LSM),
  Stage 3 (Attributes & Exact Search), Stage 4 (ANN v0), Stage 5 (Native
  Filtering), Stage 6 (SPFresh local rebuild), Stage 7 (Full-text search),
  Stage 8 (RaBitQ & kernels), Stage 9 (Object-store operations), Stage 10
  (Durability hardening and write semantics).
- **Tests:** `cargo test` and `cargo clippy --all-targets` should be green before
  each push.
- **Note:** post-Stage-2 and Stage-3–5 code-review fixes applied; remaining
  findings tracked under "Stage 2 — code review follow-ups" and "Stages 3–5 —
  code review follow-ups". Recently fixed limitations: Stage 2 ranged
  point-lookup (`sst::ranged_get`), LSM levels for `doc_ssts` (`tier_doc_ssts`),
  and orphaned-object GC (`indexer::gc`, `sana gc [--apply]`); Stage 3 attribute
  write-amplification via delta + tiered `attr_ssts` (`tier_attr_ssts`); vector
  read path fetches append deltas concurrently; Stage 7 now has tokenizer,
  block-shaped full-snapshot text postings, BM25 query support, rank-safe
  batched block MAXSCORE top-k, and consistent-snapshot multi-query for hybrid
  retrieval. Stage 8 now has portable scalar batch distance kernels plus a
  faithful, persisted RaBitQ companion per vector segment; L2 ANN scans codes,
  applies the paper's confidence bound, and exact-reranks only candidates that
  can still enter top-k. Runtime-selected NEON/AVX2 f32 distance kernels now
  accelerate full-precision scoring while preserving the scalar reference;
  4-bit stochastic query packing reduces RaBitQ estimation to AND+popcount.
  Stage 9 now has a durable global indexing queue with fenced leases,
  heartbeats, retries, at-least-once workers, brokered group commit, and
  WAL/manifest reconciliation for missed advisory notifications. Queue
  mutations now share a `QueueClient` boundary, and all-in-one serve mode routes
  HTTP writers, reconciliation, and its worker through one in-process broker.
  Multi-pod roles use a standalone broker whose address and owner generation
  live in the queue JSON; stale overlapping brokers are fenced by CAS and
  clients rediscover replacements from object storage. Query nodes can wrap any
  backend in a byte-bounded immutable-object LRU and warm one
  manifest generation under an explicit byte/concurrency budget. Fully indexed
  generations support zero-copy branches, independent cross-store copies, and
  deterministic catalog-last exports. Namespace pinning now uses durable,
  leased replica slots with fenced claims, exact-generation warm readiness,
  deterministic routing, and utilization metadata. New SSTs use a checksummed
  v2 footer and fully checked metadata arithmetic while v1 files remain
  readable. WAL commits now use recoverable CAS reservations, and durable
  per-key records make exact retries return the original cursor while rejecting
  conflicting payloads. Per-ID conditional writes are serializable against the
  WAL cursor; patch/delete-by-filter use a candidate snapshot followed by an
  atomic recheck for Read Committed behavior. WAL commit state now maintains a
  cumulative byte counter and indexed manifests publish the matching absorbed
  watermark, giving exact unindexed-byte metadata without object listings.
  Projected writes and strong query/recall snapshots enforce a configurable
  2 GiB default limit; bulk bypass is limited to unconditional upsert/delete.
  An Axum HTTP service now exposes write, query/multi-query, metadata, recall,
  and cache-warm routes over the same library methods, with structured
  400/404/409/429/500 errors and a cache-backed `sana serve` command. The
  service runs a durable indexing worker and periodic reconciliation loop, so
  ordinary deployments index accepted writes without a second process. A
  dependency-free in-process metrics registry now counts true object-store
  traffic through a `MeteredObjectStore` decorator wrapped *below* the cache, and
  `sana serve` exposes it as Prometheus text at `GET /metrics`. Dependency-free
  fixed-bucket latency histograms now time backend object-store round trips and
  the write/query paths at their dominant phase seams (write plan/commit/notify,
  query plan/candidates/overlay/rank/materialize), attached per-request via
  `Namespace::with_metrics`. Store-global indexing queue metrics now cover live,
  available, and claimed jobs, oldest-job age, queue CAS
  attempts/successes/retries, broker group-commit batch sizes, and claim wait;
  the standalone queue broker exposes its own `/metrics` endpoint. Automatic
  maintenance loops now acquire a store-global object-store CAS lease before
  scanning namespaces. Background flush, compaction, and vector-maintenance
  publishers re-check their queue claim or maintenance lease immediately before
  manifest publication; per-namespace maintenance jobs and manual publisher
  fencing remain open. Apply-mode GC and opt-in maintenance GC now re-scan
  namespace liveness immediately before deleting candidate objects. API
  query/recall snapshots now publish durable per-process reader leases so GC
  keeps their manifest bodies, referenced index objects, and WAL overlay ranges
  live; publisher safety points and durable GC candidate state remain open.
- **Last updated:** 2026-06-26.

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
- [x] **Stage 3 — Attributes & exact search.** Schema inference/checking,
      attribute inverted indexes (eq/range), filters, order-by, count/sum,
      exact vector kNN over filtered candidates.
- [x] **Stage 4 — ANN v0.** KMeans/IVF per column, immutable vector postings,
      probe + scan + rerank, recall endpoint.
- [x] **Stage 5 — Native filtering.** Cluster-level summaries, row-level
      bitmaps, filter-aware ANN traversal, filtered recall.
- [x] **Stage 6 — SPFresh local rebuild.** Mutable posting append, version map,
      split/merge/reassign background jobs.
- [x] **Stage 7 — Full-text search.** Tokenizer, BM25, block postings,
      vectorized MAXSCORE, hybrid multi-query.
- [x] **Stage 8 — RaBitQ & kernels.** Per-cluster codes, quantized query path,
      portable then SIMD kernels.
- [x] **Stage 9 — Object-store operations.** Brokered indexing queue, warm-cache
      endpoint, branch/copy/export, pinning.
- [x] **Stage 10 — Durability hardening and write semantics.** Corruption-safe
      SST metadata, idempotent/conditional writes, bounded write backpressure,
      and a service-facing HTTP/metadata surface.
- [x] **Stage 11 — Observability.** In-process metrics registry, object-store
      traffic metering and latency, phase latency histograms, cache stats,
      per-namespace index-lag gauges, store-global queue metrics, ANN/FTS
      counters, Prometheus `/metrics`.
- [x] **Stage 12 — S3 backend.** `S3ObjectStore` over presigned SigV4 requests
      with native conditional writes (`If-None-Match: *` / `If-Match: etag`),
      env-gated MinIO conformance tests, and `s3://bucket[/prefix]` locations
      accepted by every CLI verb and `sana serve`.
- [x] **Stage 13 — Automatic maintenance.** Policy-driven background
      compaction and vector maintenance inside `sana serve`; GC remains an
      explicit/dry-run-first operator action by default.

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
namespaces/{ns}/wal_commit/current      # committed cursor + pending reservation
namespaces/{ns}/wal_staging/{epoch}/*   # immutable pending WAL bytes
namespaces/{ns}/wal/{epoch}/{seq}.wal   # durable batches
namespaces/{ns}/idempotency/{key}.json  # request fingerprint -> cursor
```

Stage 1 decisions / notes:

- **D12 — Lightweight commit cursor separate from the manifest.** The write
  path CAS-advances `wal_commit/current` per commit; ordinary writes do not
  move the manifest. Indexing publishes files through the manifest, and Stage 3
  schema evolution can publish metadata-only manifest generations. This still
  realizes Principle 2 (write durability vs. indexing freshness). Manifest's
  own `wal_commit_cursor`/`indexed_cursor` are snapshots set at index-publish
  time (Stage 2+).
- **D13 — One durable pending WAL reservation per namespace.** An in-process
  append lock reduces local contention, but correctness comes from
  `wal_commit/current`: writers stage immutable WAL bytes, CAS-reserve the next
  sequence, and only then publish the canonical WAL and advance the committed
  cursor. Any later writer can finish a pending reservation after a crash.
  Concurrent namespace handles therefore cannot overwrite each other's WAL.
  The filesystem backend still has D4's cross-process CAS limitation.
- **D14 — Patch = create-or-update; null clears a field.** Patch onto a missing
  id creates a partial doc; a `Value::Null` attribute removes the field.

Known limitations to fix in later stages:

- `replay`/`lookup` are O(WAL) — full scan per call. Stage 2 SSTs fix this.
- ~~No idempotency-key dedup.~~ **Done.** Exact-key records survive WAL GC and
  process restart; equal payloads return the original cursor and unequal
  payloads return `IdempotencyConflict`.
- ~~Conditional and filter-based writes.~~ **Done for literal `FilterExpr`
  conditions.** Known-ID writes evaluate and commit under one WAL reservation;
  patch/delete-by-filter use two-phase candidate discovery and recheck.
  `$ref_new` condition operands remain an API-expression extension.
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

- ~~No LSM levels yet — `doc_ssts` is a flat newest-first list; compaction is
  all-or-nothing.~~ **Done (size-tiered).** `SstMeta.level` tags each run; flush
  writes L0 and, when a level reaches `TIER_TRIGGER` (4) runs, `tier_doc_ssts`
  folds it into one run at the next level (newest-wins, tombstones retained since
  older levels may still hold the key). `doc_ssts` stays ordered by read
  precedence (lower level / newer first), so the read path is unchanged. The full
  `compact` still merges everything and drops tombstones. Remaining refinement:
  leveling is by run *count*, not bytes, and old runs are orphaned until GC. The
  `level` field is omitted from JSON when 0, so old manifests/goldens are stable.
- ~~Orphaned SSTs from superseded generations are not GC'd~~ **Done.**
  `indexer::gc(ns, apply)` lists everything under the namespace prefix and
  removes anything the current manifest no longer references — superseded
  doc/attr runs (now plentiful with tiering + attr deltas), stale vector objects,
  old manifest bodies, and WAL batches already folded into the index — keeping
  the pointer, current body, cursor, referenced runs, and the unindexed WAL
  overlay `(indexed_cursor, commit]`. Dry-run by default (`sana gc`), deletes
  with `--apply`. Assumes single-writer quiescence (D4). A proper concurrent
  sweep would gate deletion on a reader watermark; left as future work.
- ~~No automatic flush trigger.~~ **Done (durable notification queue).** Every
  committed WAL batch best-effort enqueues its cursor in
  `jobs/indexing_queue.json`; `sana work-indexing` claims and flushes one job.
  Queue failure never changes a successful WAL result, and
  `sana reconcile-indexing` repairs missed notifications by comparing
  authoritative commit/indexed cursors.
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
- **Concurrency — manifest body overwrite race.** `ManifestPointer` can now
  name a content-derived manifest body key. `Namespace::publish_manifest` writes
  immutable bodies at `manifest/g/{generation}-{body_version}.json` and CASes
  the pointer to that exact body, so a CAS loser cannot overwrite the winner's
  body.
- **Empty-batch / empty-SST churn.** `append` rejects empty batches, and `flush`
  skips emitting a zero-row SST if it encounters an old empty WAL batch.
- **Dead manifest field partially retired.** Index publishes now maintain
  `wal_commit_cursor` as a snapshot of the commit cursor at flush/compaction
  time. Per-write durability still uses `wal_commit/current`.
- **Manifest load duplication.** `Namespace::load_manifest_snapshot` is now the
  shared pointer→body helper used by reads, schema evolution, and indexer
  publication.
- **Manifest serde test gap.** `tests/manifest_codec.rs` now round-trips
  populated `doc_ssts` metadata and content-keyed manifest pointers.

**Stage 2 review findings:**

- [x] **Epoch-blind reads.** *Guarded explicitly.* Epoch rotation is still not
      implemented, but overlay reads, flush, and GC now reject a manifest cursor
      ahead of commit or crossing epochs instead of constructing keys from the
      wrong epoch or comparing sequence numbers alone. Same-epoch sequence
      increments are checked. A malformed-manifest regression covers all three
      paths. A future rotation feature must add durable epoch-boundary metadata.
- [x] **Point lookups load whole SSTs.** *Done.* `sst::ranged_get` reads only
      the footer, the index, and the one candidate block (using the manifest's
      `size_bytes` to find the footer — no extra `head`), so `Namespace::lookup`
      no longer transfers whole objects. The whole-object `SstReader` still backs
      scans and the batch `resolve_ids` path. Whole-object and ranged paths now
      share one set of footer/index/block decoders. A counting-store test asserts
      a point lookup makes ≤3 requests and reads under a quarter of the object.
- [x] **SST footer not checksummed.** *Done.* Writers emit a v2 36-byte footer
      whose CRC covers the index handle, index CRC, version, and magic. Readers
      retain v1 compatibility and both whole/ranged paths require the index to
      end exactly at the footer.
- [x] **`u32` size/offset fields.** *Done.* Writer fields use checked
      conversions and return `InvalidWrite` on format-limit overflow. Readers
      use checked conversions/arithmetic for footer, index, block, restart,
      varint, key, and value metadata and reject malformed regions before
      slicing or allocating.
- [x] **Test gap.** *Done.* `point_lookup_prunes_ssts_by_id_range`
      (`tests/namespace.rs`) builds two un-compacted doc SSTs with disjoint id
      ranges and asserts, via a key-recording store, that a lookup issues zero
      reads against the SST its `[min_id, max_id]` excludes.

---

## Stage 3 — Attributes & Exact Search (done)

Goal: typed schema + filtering + ordering + simple aggregation, then exact
vector kNN over filtered candidates. This is the first stage that makes Sana a
*search* engine rather than a key-value log.

Planned tasks (refine when started):

- [x] Schema inference/checking: infer column types from upserts/patches,
      validate on write, evolve `Schema.version`. Decision: strict validation,
      no coercion. Null is patch-only clear, arrays are homogeneous scalar
      arrays, vectors have fixed dimension/encoding and finite values.
- [x] Attribute inverted index as a new SST family: full-snapshot sorted
      postings at `index/g/{generation}/attr/*.sst`, keyed by
      `column + encoded scalar value`, with sorted id postings. This is correct
      but write-amplifying; delta/levelled attr LSM files are still future work.
- [x] Filter expressions (Eq, range, And/Or/Not) evaluated with attribute SST
      candidate generation when possible, then rechecked against the strong
      materialized snapshot including the WAL overlay.
- [x] Order-by (primary key or one attribute) and count/sum aggregation.
- [x] Exact vector kNN: brute-force distance over a filtered candidate set
      (L2/cosine/dot), top-k via deterministic score sort. Reference scalar
      kernels only (SIMD is Stage 8).
- [x] A library query entry point: `src/query.rs` logical request/response types
      and `Namespace::query`. Integration tests cover filters, order-by,
      aggregation, kNN, and invalid vector queries.
- [x] A `query` CLI verb over the library query entry point
      (`sana query <dir> <ns> [json-query]`), plus the Stage 10 Axum query and
      multi-query route.

Known limitations to improve later:

- ~~Attribute postings are full-snapshot SSTs, not a levelled/delta attribute
  LSM. This is correct but write-amplifying.~~ **Done (delta + size-tiered).**
  A flush now appends an attribute delta of only the *touched* live docs
  (`attr_ssts` carries `level` like `doc_ssts`) instead of rewriting every id's
  postings, and `tier_attr_ssts` unions overflowing levels. The query path reads
  all levels and unions per leaf; since every candidate is rechecked against the
  live document, stale value-keyed postings are harmless false positives.
  `Eq`/`Range`/`And`/`Or` are served from the index; **`Not` falls back to the
  full-scan recheck** (complement needs exact membership, which delta levels
  can't give). The full `compact` still rebuilds one clean, stale-free snapshot.
- Query execution still materializes candidate documents for predicate recheck,
  ordering, aggregates, and exact kNN. This is acceptable for Stage 3; later
  stages should push more work into index families and vector postings.
  (Update: the O(candidates) *round trips* this caused are fixed — candidate
  resolution now reads each SST once via `Namespace::resolve_ids`; see "Stages
  3–5 — code review follow-ups". The remaining work is pushing predicate/agg
  evaluation into the index families themselves.)
- The CLI query accepts the internal serde JSON shape for `Query`. The HTTP
  surface wraps it in a stable tagged single/multi request envelope; broader
  turbopuffer wire compatibility remains a future compatibility layer.

Stage 3 decisions / notes so far:

- **D21 — Strict inferred schema at write time.** Writes infer missing columns
  from non-null upsert/patch values and reject later type changes before the
  WAL cursor advances. Attribute columns default to `filterable=true,
  indexed=true`; vector columns default to `indexed=true`, `filterable=false`,
  and `DistanceMetric::Cosine`.
- **D22 — Schema manifest updates are separate from WAL durability.** A write
  that introduces columns first publishes a schema-only manifest generation,
  then appends the WAL. Writes that match the schema do not touch the manifest.
  This keeps ordinary write durability on `wal_commit/current` while making the
  schema durable for later validators.
- **D23 — Query semantics before index acceleration.** `Namespace::query`
  executes against the strong materialized snapshot for now, which gives correct
  filter/order/aggregate/exact-kNN behavior before attribute SSTs exist.
  Attribute indexes should become a candidate-generation optimization under the
  same logical `Query` API, not a separate user-facing path.
- **D24 — Full-snapshot attribute postings for Stage 3.** Each flush/compaction
  writes one attribute postings SST for all live indexed documents. Queries use
  it to narrow candidate ids for supported filters, then always re-run the
  predicate after applying the WAL overlay. This favors correctness and simple
  delete/update semantics over write amplification; a proper attribute LSM can
  replace it later.
- **D24b — Delta + size-tiered attribute postings (supersedes the D24 write
  path).** A flush now appends a delta of only the touched live docs and tiers
  overflowing levels (`tier_attr_ssts`), rather than rewriting the whole index.
  This rests on the always-recheck invariant from D23: the candidate set need
  only be a *superset*, so unioning value-keyed postings across delta levels is
  correct for `Eq`/`Range`/`And`/`Or` even though older levels hold stale
  postings for since-changed/deleted ids — the materialize step rechecks each
  candidate against the live document and drops the false positives. `Not` is the
  one exception: complement needs exact membership, so it falls back to the
  full-scan recheck. The full `compact` still rebuilds one stale-free snapshot.

---

## Stage 4 — ANN v0 (done)

Goal: build the first approximate vector index while preserving exact rerank and
strong-read overlay semantics. This stage validates the object-store-native ANN
shape before SPFresh-style incremental updates and RaBitQ compression.

Planned tasks:

- [x] KMeans/IVF per vector column: deterministic full-snapshot KMeans builds
      from live indexed documents during flush/compaction.
- [x] Immutable vector postings: each vector column publishes one encoded
      `vector/{column}/ivf.bin` object with centroids and full-vector postings,
      referenced by `manifest.vector_indexes`.
- [x] Probe + scan + rerank: `ApproxVectorQuery` probes nearest centroids,
      scans selected postings, scores full vectors, and returns deterministic
      top-k.
- [x] Strong overlay merge: ANN results skip ids touched in unindexed WAL and
      exact-score overlay vectors before final top-k merge.
- [x] Recall checks in tests: full-probe ANN is compared against exact kNN via
      `vector::recall_at`.
- [x] Debug recall CLI/API endpoint over sampled vectors:
      `Namespace::recall` and `sana recall <dir> <ns> [json-request]` compare
      ANN against exhaustive exact search, report averages, and include
      per-sample ids for mismatch debugging.

Known limitations to improve later:

- IVF files are full-snapshot rebuilds, not incremental vector postings.
- The current index stores full f32 vectors in postings; f16 storage,
  contiguous vector blocks, quantization, and SIMD kernels are later stages.
- Filtering with ANN and filtered recall were intentionally deferred to Stage 5
  so the first ANN version could land with unfiltered semantics and overlay
  correctness first.

Stage 4 decisions / notes so far:

- **D25 — Deterministic full-snapshot IVF v0.** Build KMeans with stable initial
  centroids from id-sorted vectors and a small fixed iteration count. This keeps
  tests reproducible and makes object output deterministic enough for review.
- **D26 — ANN is an optimization under `Query`.** `ApproxVectorQuery` uses IVF
  only for unfiltered, no-aggregate vector queries. If filters/order/aggregates
  are present, it falls back to exact scoring over the strong candidate set.
  This avoids incorrect filtered ANN before Stage 5 native filtering.
- **D27 — Exact rerank always uses stored full vectors.** The IVF posting scan
  narrows candidates but final scores come from the full vector values in the
  posting object, with unindexed WAL vectors exact-scored and merged.
- **D28 — Recall endpoint measures the actual ANN path.** `Namespace::recall`
  requires a published vector index, samples vectors from the strong snapshot
  deterministically for reproducible tests, and runs exact-vs-ANN queries
  through the public `Query` executor so overlay behavior is measured too.

---

## Stage 5 — Native Filtering (done)

Goal: make attribute filtering cooperate with vector clustering instead of
falling back to exact pre-filtered kNN or post-filtering ANN results.

Planned tasks:

- [x] Persist vector addresses: `{cluster_id, local_id, row_id}` for each
      indexed vector entry so attribute indexes can point into vector postings.
- [x] Add cluster-level attribute summaries for equality/range-friendly
      filters, first as full-snapshot metadata tied to the current IVF index.
- [x] Add row-level local-ID bitmaps per attribute value and cluster.
- [x] Compile filters into cluster masks and posting-local row masks.
- [x] Make ANN traversal use cluster masks before probing/scanning postings.
- [x] Enable filtered recall in `Namespace::recall` and add tests that catch
      pre-filter/post-filter recall failures.

Known limitations to improve later:

- Stage 5 should start with full-snapshot summaries keyed to the current IVF
  generation; incremental maintenance belongs with Stage 6 SPFresh updates.
- Bitmap compression can start simple. Roaring/bitpacking and block-level range
  summaries can follow once the semantics are correct.
- Native filter metadata is embedded in the full-snapshot IVF object for now.
  That keeps cold reads to one vector object and avoids a separate filter family
  until vector postings become incremental.
- Exact array-equality filters remain outside the native mask compiler; normal
  queries fall back to exact filtered kNN for unsupported filters, and recall
  requires native support.

Stage 5 decisions / notes so far:

- **D29 — Native filtering rides with the IVF object for v0.** Vector entries
  now persist `{cluster_id, local_id, row_id}` addresses plus attribute-derived
  cluster summaries and row bitmaps in the immutable IVF object. This avoids an
  extra object-store round trip while the vector index is still full-snapshot.
- **D30 — Query owns filter semantics, vector owns bitmaps.** `query.rs`
  compiles `FilterExpr` into `VectorFilterMask` using the same Eq/Range/And/Or/
  Not semantics used for exact filtering. `vector.rs` exposes bitmap union,
  intersection, complement, and masked posting scans without depending on query
  types.
- **D31 — Filtered recall must be native.** Ordinary filtered ANN queries can
  fall back to exact filtered kNN for unsupported filters, preserving
  correctness. The recall endpoint rejects unsupported filters so it cannot
  report perfect recall from an exact fallback path.

---

## Stages 3–5 — code review follow-ups

A high-recall review of the Stage 3–5 diff (`462f44b..HEAD`) ran after it landed.

**Fixed:**

- **Numeric `Eq` cross-type miss (soundness).** `attr::ids_for_eq` built a
  type-tagged exact key (`VALUE_INT` vs `VALUE_FLOAT`, and distinct +0.0/−0.0
  bits), but the query-path recheck (`scalar_eq`) coerces numerically. So an
  indexed `Eq{score, Float(10.0)}` against an Int column returned no candidates
  while the full-scan path matched — results changed across a flush. Fixed by
  generating a numeric `Eq` as an inclusive degenerate range (decode + numeric
  compare, the machinery `ids_for_range` already uses); Bool/String keep the
  exact point lookup. Regression test `indexed_numeric_eq_matches_cross_type_query_value`.
- **ANN fast path ignored `query.limit`.** A `limit` smaller than `k` was
  honored by the exact and order_by/aggregate paths but dropped by the
  approx-vector early return. Threaded `limit` through `execute_ann_vector` /
  `exact_vector_fallback` via a shared `keep_count(k, limit)`. Regression test
  `approx_vector_query_honors_limit_below_k`.
- **Centroid-less index panic.** `search`'s `clamp(1, centroids.len())` panics on
  a zero-centroid index. `build` never emits one, but a corrupt-yet-CRC-valid
  object could; `VectorIndex::decode` now rejects it.

**Simplifications applied (behavior-preserving):**

- Extracted `live_documents(records)` in `indexer.rs` (deduped flush/compact and
  removed a dead `Deleted` arm).
- Extracted `finish_exact_vector` in `query.rs` (deduped the exact and
  approx-fallback scoring arms).
- Removed `vector_to_f32`/`vector_score` pass-through shims in `query.rs`; call
  `vector::` directly.
- Added `VectorIndex::cluster_row_counts()` to collapse five copies of the
  per-cluster row-count iterator.

**Follow-up fixed later (Stage 3 limitation — query materialization round trips):**

- **Per-id `ns.lookup` in the query path eliminated.** `materialize_candidates`
  and `execute_ann_vector` resolved each matched id with its own `ns.lookup`,
  and every `lookup` re-read the manifest pointer + body, the commit cursor,
  the doc SSTs, and the overlay — O(candidates) round trips for data already in
  hand. Added `Namespace::resolve_ids(manifest, overlay, ids)`, which loads each
  doc SST once, point-gets the candidates newest-first, and applies the
  already-read overlay. A `CountingStore` test asserts the read count no longer
  scales with candidate count: a 20-row filtered query dropped from **84 object
  reads to a handful**. Behavior is unchanged (all prior query tests pass);
  `Namespace::lookup` stays for single-key access.

**Code-quality hardening (no behavior change):**

- **One scalar comparator.** Filter equality/range, attribute-index range scans,
  and order-by previously each had their own value comparison (`scalar_eq` +
  `compare_bound_value` in `query.rs`, `compare_scalar_values` in `attr.rs`).
  Unified behind `value::compare_scalars` / `value::scalar_eq` so the three paths
  can't drift on cross-type cases. Bonus: integers now compare exactly instead of
  via `f64`, so large `i64` values keep full precision.
- **No bare `unwrap` in `src`.** The 21 `slice.try_into().unwrap()` codec sites
  (all infallible after a length check) are now `.expect("slice is a fixed-size
  window")`, and the `recall` double-lookup `.expect("checked above")` was
  refactored away with a `let Some(index_meta) = … else`.
- **`half` crate for f16.** Replaced the hand-rolled `f16_to_f32` bit-twiddling
  (and `f16_is_non_finite`) with `half::f16`, the canonical vetted crate.

**Reviewed and intentionally left as-is (design, not bugs):**

- **Per-query `metric` override on ANN.** Inference always yields `Cosine`, and
  the ANN/recall tests deliberately query Cosine-clustered indexes with `L2`.
  Centroid selection under a differing metric can lower recall on large data,
  but this is the intended "cluster once, rerank at query time" compromise; the
  recall endpoint exists to measure it. Real fix is column-level metric at
  create time (future).
- **`vector_index_generations` and `wal_commit_cursor` are write-only** —
  derivable/dead. Removing them touches the manifest schema; left with this note.
- `approx_logical_bytes` over-counts retained doc SSTs after repeated flushes
  (approximate stat; compact resets it). ANN v0 quality knobs (default probes,
  empty-cluster centroids) are by design for the full-snapshot index.
- **Content-address hash (`object_store::version_of`) still uses `DefaultHasher`.**
  Not "hand-rolled," but the wrong tool: std does not guarantee its algorithm is
  stable across Rust *versions*, so persisted content-addressed keys could shift
  on a toolchain upgrade. A vetted stable digest (e.g. `blake3`) is the right
  fix, but it changes on-disk key strings + a golden fixture and lives in the
  object-store module — left for a deliberate format change rather than folded
  into this cleanup. (The `splitmix64`/FNV recall sampler is hand-rolled but
  intentional and isolated; left as-is.)
  - **Update (superseded, commit `stabilize-content-addressed-object-versions`):**
    done, but not via blake3. `version_of` now emits **SHA-256** (`sha256-…`),
    and the old fixed-key output is reproduced explicitly by `legacy_version_of`
    via the `siphasher` crate (`std`'s `DefaultHasher` *is* SipHash-1-3) so
    pre-existing keys keep validating through `ObjectVersion::matches_content`.
    The cross-Rust-version stability concern is closed. See
    [ARCHITECTURE.md](ARCHITECTURE.md) §1.

---

## Stage 6 — SPFresh Local Rebuild (done)

Goal: move from full-snapshot vector rebuilds to SPFresh-style local updates:
append new vectors into nearby postings, drop stale versions at query time, and
rebalance postings with background split/merge/reassign work.

Planned tasks:

- [x] Add versioned vector entries and a vector version map keyed by document id
      and vector column.
- [x] Add mutable posting append objects for new vectors instead of rebuilding
      the whole IVF object on every flush.
- [x] Make ANN drop stale/deleted vector versions using the version map.
- [x] Add posting-level split/merge thresholds and local rebuild planning.
- [x] Implement bounded-neighborhood reassignment after split/merge.
- [x] Add tests for insert/delete churn preserving recall without global
      rebuild.
- [x] Execute posting-local split/merge rebuilds that change centroids/posting
      layout, then publish reassignment deltas from the new local topology.

Known limitations to improve later:

- Flushes publish append vector delta objects once a base IVF index exists;
  compaction still rewrites a full base and clears the append chain. Append
  objects currently reuse the IVF codec and duplicate centroids, so a future
  posting-specific object format should reduce overhead and bound chain length.
  (Read-path note: `execute_ann_vector` now fetches the whole delta chain
  concurrently via a `JoinSet`, instead of one round trip per delta — same
  results, fewer serial object GETs on the vector query hot path.)
- Maintenance planning is now published in the manifest, and `maintain_vectors`
  can publish bounded-neighborhood reassignment and local split/merge rebuild
  deltas from those tasks.
- Version numbers are currently index generation numbers. Future append objects
  should use monotonic per-column vector versions or WAL positions so local
  rebuilds can CAS the map without rebuilding unrelated postings.

Stage 6 decisions / notes so far:

- **D32 — Version map is the source of truth for indexed vector liveness.**
  Each vector entry/address now carries a `version`, and each vector column
  publishes a `versions.bin` object mapping document id to the current indexed
  version. ANN asks the IVF object for candidate copies, then drops any hit
  whose `(id, version)` does not match the map before final top-k.
- **D33 — Query returns all probed posting hits before liveness filtering.**
  The query path asks the IVF scan for every candidate in the selected postings,
  not only the user-visible `k`, because stale/touched entries may be removed
  after the scan. Final truncation still applies after stale filtering and WAL
  overlay merge.
- **D34 — Flush appends vector deltas; compaction resets the base.** Ordinary
  flushes with an existing vector index load the base centroids, assign touched
  live vectors to the nearest existing posting, write a generation-scoped append
  object, and publish an updated version map. The base IVF key stays stable
  across append flushes, while stale older copies are suppressed by the version
  map. Full compaction rebuilds the base index and clears appends.
- **D35 — Split/merge planning is catalog state.** The vector planner computes
  per-cluster live, stale, and appended row counts across the base IVF object
  plus append segments, then publishes deterministic split/merge tasks in the
  vector manifest metadata. Thresholds are derived from the last full base
  posting sizes, and each task carries a bounded nearest-centroid neighborhood
  for the future reassignment worker.
- **D36 — Reassignment remains append-only.** A bounded reassignment scan looks
  only at the task cluster, optional merge partner, and planned nearest-centroid
  neighborhood. Rows whose nearest local centroid changed are written to a new
  delta object with a newer version, and the version map is advanced for those
  ids so older copies become stale. This preserves lock-free reads and defers
  garbage collection to later compaction.
- **D37 — Vector maintenance is an explicit indexer pass.** `maintain_vectors`
  runs only when WAL is fully indexed, reads manifest-published maintenance
  tasks, writes at most one vector maintenance append object per vector column,
  updates that column's version map, recomputes the maintenance plan, and
  CAS-publishes a new manifest generation. This keeps foreground flush simple
  and preserves the write/indexing freshness boundary.
- **D38 — Split/merge execution is local rebuild append objects.** A split or
  merge task gathers live vectors from the planned bounded neighborhood, writes
  a new local IVF segment with action-specific topology (two centroids for
  split, one for merge), and advances the version map for rebuilt ids. Query
  treats that local rebuild segment like any other vector segment, while stale
  copies in the old base/append objects are filtered by version.

---

## Current milestone: Stage 7 — Full-Text Search

Goal: add a first full-text search path: tokenize configured text attributes,
publish simple immutable term postings with BM25 statistics, and let queries
rank/filter by text before upgrading to fixed posting blocks and MAXSCORE.

Planned tasks:

- [x] Add text schema support and tokenizer configuration.
- [x] Build immutable full-text postings during flush/compaction.
- [x] Implement BM25 scoring over text postings.
- [x] Add text query API/CLI support and hybrid-ready score plumbing.
- [x] Add tests for tokenization, ranking, filtering, and SST persistence.
- [x] Upgrade postings to fixed-size blocks with block-local max scores.
- [x] Add rank-safe MAXSCORE over the block postings.
- [x] Add hybrid multi-query planning for combined text/vector/attribute ranks.
- [x] Vectorize/batch the MAXSCORE inner loop.

Stage 7 decisions / notes:

- **D39 — Text MVP is a full-snapshot BM25 family.** Flush/compaction now publish
  `text_ssts` containing per-field document-length stats and term postings
  `(id, tf, doc_len)` for string and string-array attributes. Query uses the
  text SST only when `indexed_cursor == commit`; if WAL is ahead it falls back
  to scoring the strong replayed snapshot. This keeps read-after-write
  correctness simple while Stage 7 moves toward fixed posting blocks and
  MAXSCORE.
- **D40 — Text postings are block-shaped before MAXSCORE.** Each term now has a
  metadata record (`doc_freq`, `block_count`, default-BM25 `max_score`) plus
  fixed target-256 posting blocks, each with its own local max score. The query
  path still exhaustively consumes blocks, but the on-disk shape now matches the
  FTS v2 direction and can support block skipping without another manifest
  family change.
- **D41 — MAXSCORE is rank-safe and conservative.** Unfiltered, non-aggregate
  text top-k queries use block upper bounds to skip posting blocks whose maximum
  possible final contribution cannot reach the current heap threshold. Filtered
  or aggregate text queries still use exhaustive scoring until filters become
  native to the text planner. Custom BM25 parameters also fall back to exhaustive
  scoring because block maxima are stored for the default BM25 parameters.
- **D42 — Hybrid retrieval uses batched independent subqueries.** `MultiQuery`
  executes several ordinary `Query` plans against one captured manifest and WAL
  commit snapshot, matching the turbopuffer-style "batch text/vector reads,
  fuse/rerank client-side" model without mixing BM25, vector, and attribute
  score semantics inside the executor yet. Empty batches are rejected.
- **D43 — MAXSCORE scoring is batched and thresholded incrementally.** The text
  top-k path now precomputes BM25 term constants once per term, scores decoded
  posting blocks in contiguous 64-posting batches, and maintains the heap
  threshold with a small ordered top-k tracker instead of sorting all accumulated
  document scores after every block. This keeps the block-skip rule rank-safe
  while matching the FTS v2 guidance to favor sequential per-list work.

---

## Stage 8 — RaBitQ & Kernels (done)

Goal: add a compressed vector distance-estimation layer and isolate CPU kernels
behind a portable reference implementation before adding SIMD-specialized
paths.

Planned tasks:

- [x] Add portable scalar batch distance kernels for L2, dot, and cosine.
- [x] Add per-cluster RaBitQ code generation (faithful: rotation + estimator).
- [x] Add portable bitwise RaBitQ L2 estimator (unbiased, recall-tested).
- [x] Persist a RaBitQ object and wire the quantized query path + rerank.
- [x] Add SIMD f32 kernels with feature detection.
- [x] Add packed/SIMD RaBitQ estimation.
- [x] Benchmark f32 cache/memory/CPU bottlenecks.

Stage 8 decisions / notes:

- **D44 — Kernel boundary starts with the existing vector layout.**
  `DistanceKernel` and `ScalarDistanceKernel` batch over slices of `f32`
  candidates, so the current `Vec<f32>`-per-entry index remains unchanged while
  exact query scoring and ANN centroid/posting scans share one batch scoring
  API. RaBitQ can add code-oriented kernels behind this boundary without
  changing query semantics.
- **D45 — RaBitQ persistence is a separate companion object.** Stage 8 first
  proved `RabitqIndex` in memory, then kept the IVF object format unchanged and
  added a separately framed `.rabitq.bin` object per base/append/maintenance
  segment. Manifest fields are optional for backward compatibility; old
  manifests take the exact posting path.
- **D46 — RaBitQ is faithful to the paper, in its own module.** The first cut
  was a placeholder: a per-dimension sign flip (recoverable, so it decorrelated
  nothing) with no normalization, no correction factor, and no estimator — bits
  nobody decoded. `src/rabitq.rs` now follows Gao & Long (SIGMOD 2024): quantize
  the *normalized* residual after a fast pseudo-orthonormal rotation (random ±1
  diagonal then a Walsh–Hadamard transform, padded to a power of two), and store
  `‖o − c‖` plus the correction factor `⟨ō_q, ō'⟩`. The unbiased estimator
  `⟨ō, q_r⟩ ≈ ⟨ō_q, q'⟩ / ⟨ō_q, ō'⟩` yields an L2 distance whose ranking is
  recall-tested against exact L2. Scope is L2 (RaBitQ's design metric); dot and
  cosine remain exact posting scans. Extracting it out of
  the 1.5k-line `vector.rs` is the first step of splitting that module along its
  natural seams (filter bitmaps and maintenance are the next candidates).
- **D47 — One framed-object envelope (`src/frame.rs`).** The 20-byte header
  (magic, format version, body length, CRC32) was hand-rolled three times — WAL
  batches, the vector index, and the vector version map — with identical byte
  twiddling and `try_into().expect(...)` boilerplate. `frame::encode`/`decode`
  now own it; callers pass their magic, version, and a label for error messages.
  Pure dedup: the byte layout and every error string are unchanged, so the
  golden WAL fixture still matches.
- **D48 — `vector.rs` split along its seams into a directory module.** The
  1.3k-line file became `vector/{mod,filter,maintenance}.rs`: `filter.rs` owns
  the per-value cluster/row bitmap and the mask algebra (and the three
  `VectorIndex` mask methods); `maintenance.rs` owns LIRE split/merge planning,
  reassignment, and local rebuilds; `mod.rs` keeps the IVF core (build, k-means,
  framing, search) and the shared `assemble_index`. Pure code movement — no
  on-disk or API change. Submodules reach `mod.rs`'s private helpers (`score`,
  `assemble_index`, …) because child modules can see ancestors' private items;
  only `VectorFilterIndex::build`, which the parent's `assemble_index` calls,
  needed `pub(super)`. Public paths (`sana::vector::*`) are preserved by
  re-exports.
- **D49 — Quantized pruning happens after liveness, before local top-k.** Every
  published vector segment gets a framed RaBitQ companion because query-time L2
  may override the cosine clustering metric. L2 queries fetch each IVF/companion
  pair concurrently across base and append segments; cosine/dot skip companion
  reads. Native filter masks, the vector version map, and WAL-shadowed IDs are
  applied before estimate sorting so stale rows cannot consume a segment's
  local top-k. Candidates are ordered by the unbiased estimate, pruned only when
  their Equation-14 lower bound exceeds the current exact kth distance, and
  surviving rows are exact-reranked. Companion bytes count toward logical size
  and remain live through maintenance, compaction, and GC.
- **D50 — f32 kernels dispatch once, then run contiguous SIMD loops.**
  `vector/kernels.rs` keeps `ScalarDistanceKernel` as the reference and caches a
  runtime choice of NEON (AArch64), AVX2 (x86_64), or scalar. L2/dot use one
  vector pass; cosine computes candidate dot product and norm together instead
  of making the scalar path's two passes. Randomized parity tests cover vector
  widths 1–769, including SIMD tails, and both ARM64 and x86_64 branches
  compile. The dependency-free `cargo bench --bench distance` benchmark on the
  ARM64 development host measured 1.63–2.25x for hot-cache L2/dot and
  2.26–3.37x for cosine across 128/768/1536 dimensions. A 64 MiB working set at
  768 dimensions stayed near the same ~6 GiB/s runtime throughput, so this API
  is not saturating DRAM yet; validation and per-vector allocation/layout remain
  profiling targets.
- **D51 — Query quantization is four bit planes with an explicit error term.**
  Each rotated query is stochastically quantized to 4-bit unsigned values as in
  RaBitQ Section 3.3, then decomposed into four `u64` bit planes. A code/query
  inner product becomes four AND+popcount passes plus the affine terms from
  Equation 20. AArch64 processes two words at once with NEON byte popcount;
  other targets use portable `u64::count_ones`. The pruning radius adds the
  Equation-66 Hoeffding term for query quantization to the original estimator
  bound. Packed recall, explicit dequantization parity, SIMD/portable count
  parity, and invalid Walsh–Hadamard dimensions are tested. On the ARM64
  development host, 768-D estimation improved from 1.1 to 48.0 million codes/s
  (45.6x) while preserving the deterministic recall/rerank fixture.

---

## Stage 9 — Object-Store Operations (done)

Goal: move expensive maintenance off the write/query path and add the
operational primitives needed by an object-store-first service.

Planned tasks:

- [x] Add a durable brokered indexing queue with bounded worker claims/leases.
- [x] Add warm-cache planning and an explicit prewarm endpoint.
- [x] Add branch/copy/export operations over immutable manifest generations.
- [x] Add namespace pinning/read-replica controls after cache behavior is measured.

Stage 9 decisions / notes:

- **D52 — Queue jobs carry a WAL target and coalesce only while pending.**
  `jobs/indexing_queue.json` is versioned pretty JSON and remains a notification
  layer; WAL + manifest are authoritative. Repeated writes advance one pending
  namespace job to the highest cursor. A write behind an active claim creates a
  follow-up job, avoiding the lost-wakeup race where the worker flushes an older
  snapshot and then completes the only notification.
- **D53 — Leases are fenced by monotonically increasing claim attempts.**
  Claim/heartbeat/complete/fail are queue-file CAS transitions. One live worker
  per namespace prevents concurrent manifest publishers; lease expiry allows
  takeover, and `{job_id, worker_id, attempt}` rejects stale heartbeat or
  completion after reassignment. Workers heartbeat at one-third of the lease,
  verify the published manifest reached the target cursor, and retry failures.
  Repeating a flush after publication but before queue completion is a tested
  no-op, providing at-least-once execution.
- **D54 — Queue durability cannot weaken write durability.** WAL commit succeeds
  independently of queue availability. Post-commit enqueue is best-effort;
  advisory queue I/O runs after releasing the namespace append lock, so a slow
  queue does not serialize later WAL commits.
  `reconcile_unindexed` scans exact namespace manifest pointers, compares
  `indexed_cursor` with `wal_commit/current`, and broker-enqueues lagging
  namespaces. `IndexQueueBroker` is stateless and drains buffered mixed
  operations into one CAS; tests prove 32 pushes in one CAS, per-request error
  isolation, replacement-broker recovery, and correctness with overlapping
  brokers.
- **D55 — Only immutable generation-addressed objects enter the memory cache.**
  `CachingObjectStore` admits manifest bodies and `index/g/...` objects, keyed
  by object path with the observed object version and a content checksum.
  Mutable manifest pointers, WAL commit cursors, queue state, and operation
  records always bypass. The cache is byte-bounded LRU, rejects individually
  oversized objects, serves ranged reads from resident full bytes, writes
  through successful immutable publications, and invalidates before and after
  deletion to close the concurrent refill race.
- **D56 — Cache warming is one budgeted manifest-snapshot operation.**
  `Namespace::hint_cache_warm` captures one generation and deterministically
  prioritizes its manifest body, vector indexes/version maps/RaBitQ companions,
  then text, attribute, and document SSTs. It selects exact manifest-named
  objects under a byte budget and fetches them with bounded concurrency.
  Backends may treat this as a read hint; `CachingObjectStore` retains the
  bytes. An end-to-end test removes every warmed immutable backing object and
  still serves L2 ANN plus document materialization from cache.
- **D57 — Branches flatten one indexed generation into the child manifest.**
  A branch requires `indexed_cursor == wal_commit/current`, directly reuses the
  source generation's immutable doc/attr/text/vector keys, resets child WAL
  cursors to zero, and records `branch_parent` for lineage. Existing read and
  indexing code therefore needs no parent-chain merge: child writes form a
  normal WAL overlay and later flushes publish child-local deltas. GC scans all
  current manifests and treats foreign references into its namespace as live,
  so source compaction cannot reclaim objects still owned by a branch.
- **D58 — Physical copy/export share a bounded snapshot-transfer primitive.**
  `copy_to` streams every manifest-referenced object to destination-local
  `index/g/0/copy` keys, rewrites the new generation-0 manifest, and gives the
  copy an independent WAL. `export_to` writes content-checksummed objects under
  an arbitrary target-store prefix and publishes a deterministic versioned
  `catalog.json` last. Both require a fully indexed source, use bounded
  concurrency, and verify existing bytes for idempotent retries. Tests remove
  every source object after a cross-store copy and verify the destination is
  still readable and writable.
- **D59 — Pinning state is a leased, generation-aware routing control file.**
  `namespaces/{ns}/routing/pinning.json` is updated with versioned CAS and
  contains a configured replica count plus one fenced assignment per slot.
  Query nodes claim, heartbeat, warm, and release slots; expired leases can be
  reassigned without allowing stale owners to mutate the replacement claim.
  A replica becomes routable only after warming the exact current manifest
  generation. Routing hashes namespace plus request key over live ready
  replicas, while metadata reports configured, assigned, ready, and average
  utilization. Scaling down removes excess slots, unpinning clears all
  assignments, and namespace GC preserves the control file.

---

## Current milestone: Stage 10 — Durability Hardening And Write Semantics

Goal: close correctness gaps in durable formats and writes, then expose the
engine through a small service API without weakening the object-store model.

Planned tasks:

- [x] Add an SST footer checksum, checked offset arithmetic, and explicit size
      bounds with corruption/oversize regression tests.
- [x] Make WAL idempotency keys durable and reject conflicting retries.
- [x] Add compare-and-set conditional writes and patch/delete-by-filter against
      one validated snapshot.
- [x] Add unindexed-WAL byte accounting and configurable write/query
      backpressure.
- [x] Add HTTP write/query/metadata/recall/warm-cache endpoints over the same
      library contracts used by the CLI.

Stage 10 decisions / notes:

- **D60 — SST v2 adds footer integrity without stranding v1 objects.** The
  footer grows from 32 to 36 bytes and checksums every footer field except the
  checksum itself. Readers identify v1/v2 from the stable trailing
  version+magic and parse either format, so manifests need no migration.
  Whole-object and ranged readers share strict layout validation: contiguous
  block handles cover exactly the pre-index data region, the index ends exactly
  at the footer, keys are ordered, restart metadata is bounded, varints reject
  overflow, and decoded ranges use checked arithmetic. `SstWriter::finish`
  returns `Result`, making representational limits explicit. Tests retain the
  v1 golden, add a v2 golden, and cover footer tampering, validly rechecksummed
  invalid handles, restart corruption, and both v1 read paths.
- **D61 — Idempotency is coupled to a recoverable WAL commit reservation.**
  `wal_commit/current` is now a versioned state containing the committed cursor
  and at most one pending staged WAL. A writer first validates the request,
  writes immutable staging bytes, and CAS-reserves the next sequence. Any
  writer that sees that reservation verifies the staged bytes, finishes schema
  evolution, publishes the canonical WAL, writes the immutable per-key dedup
  record, and CAS-advances the committed cursor. This closes both marker-order
  crash windows and makes ambiguous successful CAS responses retry-safe.
  Existing bare `WalCursor` JSON is read as legacy state and migrates on the
  next append. Idempotency keys are 1–256 UTF-8 bytes and hex-encoded in object
  paths; records store a content version, byte length, and CRC of the operation
  payload. GC preserves all dedup records and an active staging object while
  reclaiming completed staging orphans. Records currently have no expiry,
  favoring retry correctness over bounded metadata count until retention
  semantics are defined.
- **D62 — Conditional writes linearize on the WAL reservation.** A known-ID
  conditional request carries one optional `FilterExpr` per upsert, patch, or
  delete. The writer reads the committed snapshot, evaluates every condition,
  and CAS-reserves that exact cursor; a CAS loser re-reads and re-evaluates.
  Missing upserts apply unconditionally, while missing patches/deletes skip.
  Duplicate IDs are rejected so all conditions observe one pre-batch snapshot.
  A zero-row result still commits an empty WAL batch, giving the read a durable
  serialization point. Applied/skipped IDs and operation counts are stored in a
  content-addressed outcome object referenced by the small commit state and
  idempotency record.
- **D63 — Filter mutations use two-phase Read Committed semantics.** Phase one
  captures matching IDs from a strong snapshot. Phase two turns those IDs into
  conditional operations with the original filter and rechecks them under the
  WAL reservation. Rows that stopped matching are skipped; rows that became
  eligible after phase one are intentionally missed. Patch and delete defaults
  match the public limits (50k and 5M); `allow_partial` truncates deterministic
  ID order and reports `rows_remaining`, while the strict mode changes nothing
  when over limit. Request-level idempotency persists the original outcome even
  after matches or limits change. Patch payloads are schema-validated even when
  phase one finds no rows. Literal query filters are shared directly; `$ref_new`
  operands remain future work.
- **D64 — Backpressure uses paired cumulative byte watermarks.** Every committed
  WAL reservation records its encoded size and advances a cumulative byte
  counter in `wal_commit/current`; each flush publishes the cumulative value it
  absorbed as `manifest.indexed_wal_bytes`. Their checked difference is the
  exact outstanding overlay size using two small reads, with no WAL listing on
  normal writes, queries, or metadata. Legacy commit states migrate once by
  using the manifest watermark as a baseline and listing only the canonical
  unindexed WAL range; mixed-version states with an existing watermark remain
  exact. New writes check projected post-commit bytes inside the same CAS
  reservation loop, so concurrent handles share one namespace budget. The
  default is 2 GiB and is configurable per call. `disable_backpressure` bypasses
  only unconditional upsert/delete batches; patches, conditional writes, and
  filter mutations still enforce the limit because they read strong state.
  Exact idempotent retries resolve before the limit check. Strong query,
  multi-query, and recall snapshots reject oversized overlays, and recall now
  evaluates candidates plus exact/ANN comparisons against one captured
  manifest and commit cursor.
- **D65 — HTTP is a thin Axum adapter, not a second database implementation.**
  `api::router` exposes `POST /v2/namespaces/{namespace}` for append,
  conditional, patch-by-filter, and delete-by-filter writes; one tagged
  single/multi query route; metadata, recall, cache-warm, and health endpoints.
  Write requests create namespaces on demand, while reads require an existing
  namespace. A 64 MiB body limit bounds request buffering. Engine failures map
  to structured JSON with stable 400/404/409/429/500 classes, and extractor
  failures preserve their HTTP status in the same envelope. `Namespace::metadata`
  combines one manifest snapshot with WAL-byte and pinning control state.
  `sana serve <dir> [address] [cache-bytes]` wraps the filesystem backend in the
  immutable-object LRU before serving HTTP/1. Router-level tests cover every
  write variant, single/multi queries, exact lag metadata, backpressure, recall,
  cache warming, and error envelopes.
- **D66 — Unsupported WAL epoch rotation fails closed.** `WalCursor` retains an
  epoch field for a future rotation protocol, but commit state currently writes
  one epoch only. Overlay reads, index flush, and GC now compare full cursors,
  reject cross-epoch ranges, and use checked sequence increments. This prevents
  latent epoch fields from turning a malformed manifest or partial future
  rollout into silent stale reads, skipped indexing, or live-WAL deletion.
- **D67 — The HTTP service owns a local durable index worker.** `api::serve`
  starts one leased queue worker after binding succeeds, polls idle queues at
  100 ms, retries failures with backoff, and reconciles authoritative
  manifest/WAL lag every 30 seconds. Queue state remains durable and fenced, so
  external workers can still be added for scale; the embedded worker simply
  makes the default single-process service self-indexing. A live socket smoke
  test observed metadata move from `updating` with unindexed bytes to
  `up-to-date` after the background flush.

---

## Current milestone: Stage 11 — Observability

Goal: make the engine measurable. The architecture lists required metrics "from
the beginning"; this stage adds the in-process plumbing and a scrape surface,
starting with the dominant cost in an object-store-native database — backend
traffic — and growing into latency, lag, and cache metrics.

Planned tasks:

- [x] Add a dependency-free in-process metrics registry (`src/metrics.rs`):
      atomic counters, a `Copy` snapshot, and a Prometheus text renderer.
- [x] Count true object-store traffic with a `MeteredObjectStore` decorator
      placed below the cache, so cache hits never inflate backend counts.
- [x] Expose a Prometheus `GET /metrics` endpoint and wire the metered stack
      into `sana serve`.
- [x] Write/query latency histograms split by phase: write plan/commit/notify,
      query plan/candidates/overlay/rank/materialize, plus a backend
      object-store request histogram in the metered decorator (cache-read
      timing rides with the cache-stats task below).
- [x] Index-lag and unindexed-byte gauges per namespace (`reconcile_unindexed`
      now reports exact per-namespace lag; the serve worker records it).
- [x] Surface cache hit ratio / temperature: the cache mirrors its stats into
      `CacheMetrics` after every operation (hits/misses/bypasses/evictions as
      counters, capacity/resident/entries as gauges).
- [x] Vector candidate/estimate/rerank/prune counts (from `RabitqSearchStats`)
      and FTS blocks-read/skipped counters (from `TextSearchStats`).

Stage 11 decisions / notes:

- **D68 — Metrics are an in-process registry, not a dependency.** `src/metrics.rs`
  holds plain `AtomicU64` counters behind an `Arc<Metrics>`, exposes a `Copy`
  `MetricsSnapshot`, and renders the Prometheus text format by hand. This keeps
  the D10 minimal-dependency posture (no `prometheus`/`metrics` crate) while
  emitting the lingua-franca scrape format. Reads are `Relaxed`: counters are
  independent and a scrape never needs a consistent cross-counter view.
- **D69 — Meter below the cache.** `MeteredObjectStore` is an `ObjectStore`
  decorator that counts each request (including failures), success byte volumes,
  and every compare-and-set rejection. `sana serve` builds the stack as
  `Caching(Metered(Fs))` so a cache hit, which never reaches the backend, is not
  counted as an object-store round trip — the counters measure true egress to
  durable storage. The shared `Arc<Metrics>` is threaded into the router so
  `/metrics` reports exactly what the decorator observed. A live `sana serve`
  smoke test confirmed a write advances `puts_if_absent`/`compare_and_sets` and
  byte counters, and that the embedded indexing worker contributes traffic too.
- **D70 — Latency histograms are fixed-bucket atomics recorded at existing
  seams.** `metrics::Histogram` is eighteen `AtomicU64` buckets (100µs doubling
  to ~6.6s, plus `+Inf`) and a microsecond sum, rendered as a Prometheus
  histogram; no timer dependency, `Relaxed` ordering like the counters.
  `Namespace` carries an `Arc<Metrics>` — a fresh private registry by default,
  swapped via `with_metrics` so API handlers and the latency example attach the
  scraped one without threading a parameter through every constructor. Phases
  are recorded where the code already has seams, and they are the *dominant
  spans, not a partition*: totals are end-to-end, phases need not sum to them,
  and a phase that does not occur (e.g. `plan` for a plain append) records
  nothing. Writes record `plan` (filter-mutation candidate discovery),
  `commit` (the locked stage/CAS/publish region, excluding lock wait), and
  `notify` (advisory enqueue); queries record `plan` (manifest + commit-state
  snapshot), `candidates` (attribute/text/vector index reads), `overlay`
  (unindexed WAL read), `rank` (ANN scan, BM25, exact rerank, or sort), and
  `materialize` (id→document resolution or the full-scan fallback). The metered
  decorator times every backend round trip into
  `sana_object_store_request_seconds`, so "object reads" are measured below the
  cache; timing cache-served reads belongs to the cache-stats task. Rejected
  requests record nothing; failures inside a timed span still record it.
- **D71 — Remaining Stage 11 metrics reuse state that already exists.** The
  cache *mirrors* its mutex-guarded `CacheStats` into atomic gauges after each
  operation (the state stays the source of truth, so attach one cache per
  registry). Per-namespace lag rides the reconciliation scan: `ReconcileReport`
  now carries exact `unindexed_bytes`/`unindexed_batches` for every scanned
  namespace, and the serve worker replaces the labeled gauge map wholesale each
  pass so deleted namespaces drop out. ANN and FTS counters surface the
  `RabitqSearchStats` and `TextSearchStats` the search paths already computed;
  the only behavior change is the text MAXSCORE path calling the `_with_stats`
  variant. `MetricsSnapshot` consequently holds a map and is `Clone`, not
  `Copy`.

---

## Stage 12 — S3 Backend (done)

Goal: make "object-store-native" literal — run the whole engine against real
S3-compatible storage with server-enforced conditional writes.

- [x] `S3ObjectStore` (`src/object_store/s3.rs`): GET / ranged GET / PUT /
      conditional PUT / paginated ListObjectsV2 / DELETE over presigned SigV4
      requests; `S3Config::from_location("s3://bucket[/prefix]")` plus
      environment configuration.
- [x] Env-gated conformance suite (`tests/s3_object_store.rs`) covering the
      store contract and a full namespace lifecycle; verified against live
      MinIO (6/6 green) plus a CLI smoke (`create`/`upsert`/`flush`/`get` over
      `s3://`).
- [x] Every CLI verb and `sana serve` accept `s3://bucket[/prefix]` locations.

Stage 12 decisions / notes:

- **D72 — S3 conditional writes are the real CAS; rusty-s3 + reqwest, not the
  AWS SDK.** Put-if-absent sends `If-None-Match: *` and compare-and-set sends
  `If-Match: <etag>`, so the precondition is enforced *by the store* across
  processes and nodes — this lifts D4's single-process filesystem limitation.
  Object versions wrap unquoted ETags; CAS only ever needs version equality,
  and every recovery path that verifies immutable objects compares bytes, not
  tokens, so ETag versions are safe (a content-hash token from another backend
  simply mismatches, which is the correct CAS answer). 412 maps to
  `CasMismatch`/`AlreadyExists`, 404-on-If-Match to `CasMismatch` with no
  actual, and 409 `ConditionalRequestConflict` (a racing conditional write)
  gets a short bounded retry. Ranged reads send one `Range` header and check
  `Content-Range`'s total size so a clamped read surfaces as `InvalidRange`,
  matching filesystem semantics; empty ranges bounds-check with a HEAD. The
  dependency posture stays D10-minimal: `rusty-s3` is a tiny sans-IO SigV4
  presigner (the conditional headers are plain HTTP and ride unsigned) and
  `reqwest`/rustls is the one transport addition — the engine needs six verbs,
  not the AWS SDK's smithy stack. The trade-off, documented here on purpose:
  credentials are env-vars (+ session token), no IAM-role/SSO chain; swapping
  in `aws-sdk-s3` later touches exactly one file behind `ObjectStore`.

- **D73 — Transient S3 failures retry at the backend boundary, ambiguous-success
  safe.** Object stores throttle (`503 SlowDown`) and return transient gateway
  faults (`500`/`502`/`504`); failing the database operation on the first one is
  avoidable. Every verb now routes its presigned request through `send_retrying`,
  which retries transport errors and those retryable 5xx codes with bounded
  exponential backoff and full jitter (capped at `RETRY_CAP`, `TRANSIENT_RETRIES`
  beyond the first attempt), re-signing the URL each attempt. `404`, precondition
  failures (`412`), and `Corrupt` decode errors stay non-retryable; the `409`
  conditional-conflict retry is unchanged and composes on top. Conditional writes
  are the subtle case: a retry after a transient failure can resend `If-None-Match`
  / `If-Match` and see `412` even though the *first* attempt already committed.
  `reconcile_conditional` therefore re-reads the key after any retried conditional
  PUT and decides by **byte equality** — not ETag/content-hash equality, since S3
  ETags are not Sana content versions — reporting our own bytes as success and a
  genuine divergence as `AlreadyExists`/`CasMismatch` (now carrying the actual
  version). The common, non-ambiguous path takes no extra GET. Retry logic is
  proven by a localhost mock-HTTP server scripting per-verb `503`/dropped-connection
  sequences plus pure `backoff_delay`/`is_retryable_status` unit tests; the MinIO
  conformance suite (6/6) confirms the refactored verbs still satisfy the
  real-backend contract.

---

## Stage 13 — Automatic Maintenance (done)

Goal: a single `sana serve` keeps its index shape tidy — no operator cron jobs
for compaction or vector maintenance. Object reclamation stays dry-run/operator
driven by default until a real safe point exists.

- [x] `src/maintenance.rs`: `MaintenancePolicy` (run-count and vector-append
      thresholds, vector maintenance, GC toggles) + `run_once` pass over every
      namespace with per-namespace error isolation.
- [x] `api::serve` runs the maintenance loop beside the index worker
      (60-second interval, default policy).
- [x] Tests: threshold-triggered compaction preserving data, unindexed
      namespaces left alone, default GC-disabled behavior, and opt-in two-pass
      deferred GC.

Stage 13 decisions / notes:

- **D73 — Maintenance is deferred, prioritized, and isolated.** A pass touches
  only *fully indexed* namespaces for index-shape work (the flush worker owns
  catching up): full compaction fires when doc/attr run counts or a vector
  append chain reach the policy thresholds, otherwise manifest-published
  vector split/merge tasks run — never both in one pass, since compaction
  subsumes the append chain anyway. The legacy GC toggle is off by default:
  two-pass deletion is only an opt-in single-process/quiescent safeguard, not a
  production proof. Per-namespace failures land in `MaintenanceReport.errors`
  instead of aborting the fleet pass, and namespaces deleted between passes drop
  their candidates.

---

## Follow-ups (post Stage 13)

Review-driven polish after the engine was feature-complete.

- **Docs & examples.** A request/response cookbook and a limits table in the
  guide; `examples/hybrid.rs` (RRF fusion over `multi_query`) and
  `examples/conditional.rs` (CAS + idempotent retry); a `docker-compose.yml`
  MinIO stack so the S3 path is copy-paste, the latency harness extended to
  target `s3://`, and a local-MinIO row added to the benchmarks.

- **MIT license.** Added `LICENSE` and `license`/`description`/`repository`
  metadata to `Cargo.toml`; the README/project page already claimed
  open-source.
- **Current-state architecture doc.** `docs/ARCHITECTURE.md` describes the engine
  as it stands (object-store boundary, on-disk layout, write/read paths, core
  invariants), so this log can stay a chronological record. Two stale references
  to `DefaultHasher` (the Stage 3–5 "left as-is" note and D3) were amended in
  place — superseded, not rewritten — to point at the SHA-256 + `siphasher`
  resolution.
- **D74 — Plain-JSON wire values.** `Id`, `Value`, and `VectorValue` now
  serialize as bare JSON scalars/arrays (`1`, `4.5`, `"fantasy"`, `[0.1, 0.2]`)
  instead of serde's type-tagged enum form (`{"U64": 1}`, `{"Float": 4.5}`),
  matching how turbopuffer and peers accept documents. The type comes from the
  JSON token plus the schema. The split rides on `serializer.is_human_readable()`:
  JSON gets the plain form, while postcard (WAL/SSTs) keeps the tagged encoding
  because it is not self-describing and cannot round-trip a tag-less scalar — so
  the binary on-disk bytes and every binary golden are unchanged. A canonical
  hyphenated-UUID string round-trips to `Id::Uuid` (a 36-char hyphenated-hex
  string therefore cannot be a `String` id). Structural enums (`Upsert`/`Patch`
  operations, `Eq`/`Range`/`And` filters) keep their tags — those discriminate,
  they do not annotate a scalar. No compatibility with the old tagged JSON form;
  this is a deliberate pre-1.0 break.
- **D75 — Production-readiness hardening starts fail-closed.** Automatic online
  GC now defaults off because TiDB-style safe reclamation requires a safe point
  over active readers/transactions, and Delta Lake's VACUUM guidance similarly
  warns that concurrent readers or uncommitted files can still be live. Sana
  keeps `sana gc` as dry-run by default and leaves the legacy maintenance GC
  behind an explicit policy flag until durable reader/publisher watermarks are
  implemented. Query `limit: null` now means `MAX_QUERY_RESULTS` (10,000);
  aggregates are computed over all matches before row truncation. JSON
  attributes reject integers above `i64::MAX` rather than converting to lossy
  `f64`, while `2^53+1` and `i64::MAX` still parse as exact Rust `i64`.
  Existing F16/F32 vector columns canonicalize human JSON float arrays to the
  column encoding before WAL publication, so a queried F16 document can be
  written back unchanged through HTTP. The hybrid example's RRF helper now uses
  one-based ranks as defined by Cormack/Clarke/Buettcher.
- **D76 — Runtime roles split before the broker boundary.** `sana serve-api`
  runs HTTP only, and `sana serve --role all` preserves the single-process dev
  shape. `sana work-indexing --loop` is a standalone leased indexing worker with
  periodic reconciliation, and `sana maintain --loop` runs the all-namespace
  maintenance pass without serving traffic. This follows the turbopuffer
  query/indexer separation: API replicas can now scale horizontally without
  each pod claiming queue work or listing every namespace for maintenance. The
  standalone networked `queue-broker` remains a separate P0 because the current
  `IndexQueueBroker` is an in-process group-commit helper with no client/server
  transport yet. `docs/kubernetes-roles.yaml` shows separate API, indexer, and
  maintenance Deployments.
  - **Superseded by D79:** the standalone `sana queue-broker` role and
    object-store-discovered HTTP client now exist; the remaining queue work is
    observability, not process separation.
- **D77 — Kubernetes lifecycle uses readiness as the traffic gate.** Following
  Kubernetes probe guidance, `/livez` (and legacy `/healthz`) stays
  process-local and does not fail just because S3 is unavailable. `/readyz`
  fails during startup, drain, local query-slot overload, or a bounded backend
  list failure. Ctrl-C and SIGTERM both start drain: Sana marks itself unready,
  rejects new namespace traffic with `503 draining`, waits five seconds for
  readiness propagation, then lets Axum gracefully drain in-flight requests.
  Looped indexer and maintenance roles watch the same signals between units of
  work, so shutdown stops the next claim/pass without cancelling the current
  job or maintenance scan. The Kubernetes example sets termination grace periods
  from the five-second readiness delay plus a 30-second worker-drain budget.
- **D78 — Queue transport is separated from queue state.** `QueueClient` is the
  object-safe mutation boundary for enqueue, claim, heartbeat, completion,
  failure, and reconciliation. Direct `IndexQueue` CAS remains the library and
  recovery default; `IndexQueueBroker` implements the same contract with durable
  group commit. `sana serve --role all` injects one broker into HTTP namespace
  handles and its indexing worker, so those operations no longer contend as
  independent queue writers. Reconciliation accepts the same client and keeps
  enqueue requests concurrent so a broker can batch them.
- **D79 — Broker discovery and fencing live in `queue.json`.** Following the
  checked-in turbopuffer queue article, `sana queue-broker` registers its
  advertised HTTP address, owner id, and monotonically increasing generation in
  the same CAS-updated queue object. API writers, reconciliation, and indexer
  workers discover that registration directly from object storage. Every
  broker batch verifies the exact registration before mutation; a replacement
  therefore fences an overlapping old broker, whose client response triggers
  rediscovery. Successful responses are sent only after durable group commit.
  Transport timeouts remain ambiguous and are not blindly replayed. A bounded
  broker group-commit timeout stops the broker loop and fails `/livez`;
  Kubernetes supplies the replacement process. This is the one implementation
  detail the article leaves to deployment supervision.
- **D80 — Queue observability is measured at the queue owner.** The remaining
  global-queue scaling question is whether the single JSON file is healthy, not
  whether broad object-store traffic exists. `IndexQueue` now records queue
  jobs, available jobs, claimed jobs, oldest-job age, queue CAS attempts,
  successes, retries, and claim wait. `IndexQueueBroker` records group-commit
  batches, total batched requests, and batch-size histograms. `sana
  queue-broker` exposes the same Prometheus `/metrics` surface as the API and
  wraps its backend in `MeteredObjectStore`, so multi-pod deployments can scrape
  the queue owner directly. All-in-one `sana serve` reports these queue metrics
  on the API `/metrics`; API-only pods using a remote broker do not claim to own
  queue health. Queue sharding remains intentionally deferred until these
  measurements prove the single-object design is insufficient.
- **D81 — Automatic maintenance uses a store-global CAS lease.** turbopuffer's
  published BYOC material exposes distinct query, index, and maintenance
  capacity, but not an internal maintenance protocol. Sana stays object-store
  native by adding `jobs/maintenance_leader.json`: pretty JSON with a format
  version, revision, owner id, monotonic fencing token, and lease expiry.
  `sana serve --role all` and `sana maintain --loop` must claim that lease
  before scanning namespaces; a live owner blocks duplicates, an expired owner
  can be replaced, and stale owners cannot heartbeat or release the replacement.
  This is intentionally coarse. It prevents duplicated automatic maintenance
  loops, but it does not yet create durable per-namespace compaction/vector jobs
  or re-check a fencing token immediately before manifest publication.
- **D82 — Background publishers verify ownership at manifest publication.**
  The checked-in turbopuffer material says object storage is authoritative and
  any indexing node can run compaction, but does not expose a private
  per-namespace maintenance protocol. Sana therefore adds a local
  `ManifestPublishFence` around manifest-pointer CAS publication: indexing
  workers heartbeat their exact queue claim, and automatic maintenance
  publishers heartbeat their store-global maintenance lease immediately before
  making new immutable index objects reachable. A stale owner can still leave
  orphaned immutable files, but it cannot publish them. Manual CLI publishers
  and durable per-namespace maintenance jobs remain future work.
- **D83 — Filesystem CAS locks are scoped per root.** `FsObjectStore` still
  provides only single-process CAS, but the in-process write mutex now belongs
  to the normalized backing root instead of the whole process. Handles opened on
  the same directory still serialize `put_if_absent`, `compare_and_set`, `put`,
  and `delete`, preserving D4's local correctness model. Independent stores no
  longer block each other, which keeps parallel tests and multi-store examples
  from coupling unrelated tempdirs through one global async mutex.
- **D84 — GC deletion rechecks namespace liveness immediately before delete.**
  Direct `sana gc --apply` and the legacy opt-in maintenance GC no longer delete
  solely from the first orphan scan. They first collect candidate keys, then run
  a fresh manifest/WAL/branch-reference liveness scan and delete only the
  intersection that still proves orphaned. This closes the race where a
  concurrent publisher makes a candidate reachable between scan and delete, but
  it is not a production online-GC protocol: Sana still needs durable
  reader/publisher watermarks before automatic deletion can be enabled.
- **D85 — Query snapshots publish durable reader leases for GC.** The checked-in
  turbopuffer material says object storage is the source of truth and compaction
  eventually removes deleted data, but does not expose a private GC watermark
  protocol. Sana stays object-store-native by writing one CAS-updated
  `jobs/readers/{owner}.json` object per API process. Each active query/recall
  snapshot records the namespace, manifest generation, exact manifest body key,
  indexed WAL cursor, and committed WAL cursor. After publishing that lease, the
  query re-reads `manifest/current` and retries if the pointer moved, so a
  generation cannot become orphaned between snapshot capture and lease
  publication. GC lists those reader objects only in the maintenance/tooling
  path and treats unexpired snapshots as live by loading their manifest bodies
  and preserving their referenced index objects plus WAL overlay range. Query
  hot paths use exact-key CAS writes, never object listing. This closes the
  old-reader deletion gap for API queries and recall; active publishers, durable
  GC candidates, and read-dependent write paths still need follow-up before
  automatic online GC can be enabled.

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
  - **Superseded:** the original used `DefaultHasher`, whose algorithm `std`
    does not guarantee stable across Rust versions. `version_of` is now
    **SHA-256**; the old SipHash-1-3 output is pinned via the `siphasher` crate
    as `legacy_version_of` and still accepted. See the Stage 3–5 follow-up note
    and [ARCHITECTURE.md](ARCHITECTURE.md) §1.
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
- **D10 — Minimal dependency set.** Stage 0 used tokio, async-trait,
  serde(+json), postcard, crc32fast, bytes, thiserror, and tempfile only.
  Purpose-built HTTP dependencies were deferred until Stage 10; Axum/Hyper and
  Tower are now present for the service adapter and router tests.
- **D11 — Single crate, module-per-subsystem.** Organized as if it could become
  a workspace later, per the architecture doc.

---

## Repo map (current)

```
src/
  lib.rs                 module exports
  main.rs                CLI + cache-backed HTTP server command
  api.rs                 Axum routes, request/response envelopes, error mapping
  error.rs               shared Error / Result
  value.rs               Id, Value, VectorValue, Document
  schema.rs              ScalarType, ColumnType, Schema, ...
  object_store.rs        ObjectStore trait, ObjectVersion, version_of
  object_store/
    fs.rs                FsObjectStore (filesystem backend)
    s3.rs                S3ObjectStore (presigned SigV4, native conditional writes)
    cache.rs             immutable-object byte-bounded LRU decorator
    metered.rs           ObjectStore decorator that counts backend traffic
  metrics.rs             in-process metrics registry + Prometheus rendering
  maintenance.rs         policy-driven compaction/vector pass + opt-in GC
  manifest.rs            NamespaceManifest, ManifestPointer, SstMeta (+ codecs)
  metadata.rs            service-facing namespace/index/pinning metadata
  wal.rs                 WalCursor, WalOp, WalBatch (+ binary codec)
  sst.rs                 generic sorted-string-table writer/reader
  doc.rs                 Id key encoding (order-preserving) + DocRecord
  attr.rs                attribute postings SST encoding/query helpers
  text.rs                tokenizer, BM25 stats/scoring, text postings SST helpers
  query.rs               logical query types + exact/ANN/text/native filtering + recall
  vector.rs              IVF core: build, k-means, framing, search
  vector/
    filter.rs            per-value cluster/row bitmaps + mask algebra
    kernels.rs           scalar + runtime-SIMD distance kernels
    maintenance.rs       SPFresh split/merge/reassign + local rebuild
  namespace.rs           Namespace: create/append + SST-aware replay/lookup
  cache_warm.rs          manifest-driven warm plan and prefetch report
  index_queue.rs          durable queue, group-commit broker, leased worker
  indexer.rs             flush/compaction + attr/text/vector index publication
  operations.rs          branch, physical copy, snapshot export
  pinning.rs             leased pinned-replica control, warming, and routing
  write.rs               conditional/filter mutation request and result types
tests/
  common/mod.rs          assert_golden snapshot helper
  fs_object_store.rs     object store behavior (CAS, ranges, list, ...)
  manifest_codec.rs      manifest/pointer round-trip + golden JSON
  wal_codec.rs           WAL round-trip, corruption detection, golden bytes
  sst.rs                 SST round-trip, point get, corruption, golden bytes
  doc_codec.rs           Id key order + record round-trip
  namespace.rs           namespace lifecycle + durability across reopen
  indexer.rs             flush/compaction + SST+overlay merge semantics
  schema.rs              write-time schema inference/validation
  query.rs               filters, ordering, aggregation, exact kNN, ANN, BM25, recall
  cache_warm.rs          warm planning + cache-resident ANN integration
  operations.rs          branch isolation/GC and cross-store copy/export
  pinning tests live beside the control implementation in src/pinning.rs
  write.rs               conditional atomicity, retries, and two-phase filters
  text.rs                tokenization, BM25, text SST round-trip
  s3_object_store.rs     env-gated S3/MinIO conformance + engine lifecycle
  maintenance.rs         threshold compaction + default-off/opt-in GC
  fixtures/              committed golden snapshots
docs/
  guide.md               user guide (CLI, S3, HTTP API, metrics, benchmark)
  kubernetes-roles.yaml  separate API/indexer/maintenance Deployment sketch
  benchmarks.md          latency-harness numbers on a dev machine
  index.html             minimal project page (GitHub Pages serves /docs)
examples/
  usage.rs               end-to-end library tour (write → index → 4 query shapes)
  hybrid.rs              multi-query vector + BM25 fused client-side with RRF
  conditional.rs         conditional (CAS) writes and idempotent retries
  latency.rs             benchmark harness (filesystem or s3://) over the serve stack
```
