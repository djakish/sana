# Sana — Build Progress & Task Tracker

This is the durable task log. Work pauses and resumes across sessions, so this
file is the source of truth for "what's done, what's next, and why". Read this
plus `docs/wiki/architecture.md` before continuing.

**How to resume:** read this file → run `cargo test` (should be green) → pick up
the next unchecked task under "Current milestone" / "Next up".

---

## Status snapshot

- **Current stage:** Stage 7 (Full-text search) — **in progress**.
- **Next up:** Tokenizer, BM25 stats, and simple text postings.
- **Done:** Stage 0 (Skeleton), Stage 1 (Durable Documents), Stage 2 (SST/LSM),
  Stage 3 (Attributes & Exact Search), Stage 4 (ANN v0), Stage 5 (Native
  Filtering), Stage 6 (SPFresh local rebuild).
- **Tests:** `cargo test` green (89 tests); `cargo clippy --all-targets` clean.
- **Note:** post-Stage-2 and Stage-3–5 code-review fixes applied; remaining
  findings tracked under "Stage 2 — code review follow-ups" and "Stages 3–5 —
  code review follow-ups". Stage 2's ranged point-lookup limitation is now fixed
  (`sst::ranged_get`).
- **Last updated:** 2026-06-06.

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
  path CAS-advances `wal_commit/current` per commit; ordinary writes do not
  move the manifest. Indexing publishes files through the manifest, and Stage 3
  schema evolution can publish metadata-only manifest generations. This still
  realizes Principle 2 (write durability vs. indexing freshness). Manifest's
  own `wal_commit_cursor`/`indexed_cursor` are snapshots set at index-publish
  time (Stage 2+).
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
- Idempotency-key dedup and conditional writes are still future work.
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

**Outstanding (address before/within Stage 3 — none fire under the current
single-process, epoch-0, trusted-storage assumptions, but they are real):**

- [ ] **Epoch-blind reads.** `read_overlay_ops` builds WAL keys with `to.epoch`
      and `flush`'s `from_seq >= commit.seq` compares only `seq`; both break if
      the WAL epoch ever rotates. Make the overlay range epoch-aware when epoch
      rotation is implemented.
- [x] **Point lookups load whole SSTs.** *Done.* `sst::ranged_get` reads only
      the footer, the index, and the one candidate block (using the manifest's
      `size_bytes` to find the footer — no extra `head`), so `Namespace::lookup`
      no longer transfers whole objects. The whole-object `SstReader` still backs
      scans and the batch `resolve_ids` path. Whole-object and ranged paths now
      share one set of footer/index/block decoders. A counting-store test asserts
      a point lookup makes ≤3 requests and reads under a quarter of the object.
- [ ] **SST footer not checksummed.** Unlike blocks/index, footer fields aren't
      CRC'd, so accidental corruption can overflow/panic instead of erroring;
      add a footer checksum and use checked arithmetic on parsed offsets.
- [ ] **`u32` size/offset fields** (restart offsets, index key length) silently
      truncate for >4 GiB blocks/keys — widen or bound.
- [ ] **Test gap.** No test directly exercises the point-lookup `min_id/max_id`
      prune path.

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
      (`sana query <dir> <ns> [json-query]`). HTTP/API surface is still future
      work.

Known limitations to improve later:

- Attribute postings are full-snapshot SSTs, not a levelled/delta attribute LSM.
  This is correct but write-amplifying.
- Query execution still materializes candidate documents for predicate recheck,
  ordering, aggregates, and exact kNN. This is acceptable for Stage 3; later
  stages should push more work into index families and vector postings.
  (Update: the O(candidates) *round trips* this caused are fixed — candidate
  resolution now reads each SST once via `Namespace::resolve_ids`; see "Stages
  3–5 — code review follow-ups". The remaining work is pushing predicate/agg
  evaluation into the index families themselves.)
- The CLI query accepts the internal serde JSON shape for `Query`; a polished
  public HTTP/API shape is still future work.

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

- [ ] Add text schema support and tokenizer configuration.
- [ ] Build immutable full-text postings during flush/compaction.
- [ ] Implement BM25 scoring over text postings.
- [ ] Add text query API/CLI support and hybrid-ready score plumbing.
- [ ] Add tests for tokenization, ranking, filtering, and SST persistence.

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
  main.rs                CLI (create/upsert/get/delete/list/query/recall/flush/compact/demo)
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
  attr.rs                attribute postings SST encoding/query helpers
  query.rs               logical query types + exact/ANN/native filtering + recall
  vector.rs              IVF vector index, version map, native filter bitmaps
  namespace.rs           Namespace: create/append + SST-aware replay/lookup
  indexer.rs             flush/compaction + attr/vector index publication
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
  query.rs               filters, ordering, aggregation, exact kNN, ANN, recall
  fixtures/              committed golden snapshots
```
