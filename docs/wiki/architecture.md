# Sana Architecture

Sana is a search database for vectors, text, and attributes. The design is
inspired by turbopuffer's public docs/blogs, the SPFresh paper, and the RaBitQ
paper, but it is written as an independent implementation plan for this repo.

The database should be useful before it is clever. The first version should be
a local, durable, testable object-storage-native engine with exact search and a
simple ANN index. The long-term version should preserve the same storage and API
contracts while adding SPFresh-style incremental vector indexing, native
filtering, FTS v2-style postings, RaBitQ quantization, and distributed query
execution.

## Product Shape

The core unit is a namespace. A namespace is an isolated set of documents with
its own object-store prefix, WAL, schema, indexes, cache state, and metadata.
Small namespaces are a feature, not a limitation: tenant-level or corpus-level
namespaces keep query planning simple and make cache behavior predictable.

Documents have:

- a primary key: `u64`, UUID, or short string;
- zero or more vector columns: `[N]f32` or `[N]f16`;
- scalar and array attributes for filtering, ordering, aggregation, and return;
- optional full-text-search attributes.

The public API should converge on these operations:

- `POST /v2/namespaces/:namespace`: upsert, patch, delete, conditional write,
  patch-by-filter, delete-by-filter, copy, and branch.
- `POST /v2/namespaces/:namespace/query`: vector ANN, exact kNN, BM25, sparse
  vector, order-by, lookup, aggregation, grouped aggregation, and multi-query.
- `GET/PATCH /v1/namespaces/:namespace/metadata`: schema, approximate size,
  indexing state, pinning state, branch parent.
- `POST /v1/namespaces/:namespace/_debug/recall`: compare ANN against exact
  search over sampled queries.
- `GET /v1/namespaces/:namespace/hint_cache_warm`: prewarm the cache.
- export/list/delete namespace endpoints later.

Non-goals for the core engine:

- general-purpose transactions;
- sub-millisecond writes;
- replacing PostgreSQL;
- built-in embedding generation or second-stage reranking;
- exact top-k over huge unfiltered vector corpora by default.

Sana is a first-stage retrieval system. It should quickly reduce millions or
billions of rows to a candidate set for application-owned fusion/reranking.

## Design Principles

1. Object storage is the source of truth.
   Query and indexing nodes are replaceable. Durable state lives in object
   storage, initially a local filesystem implementation of the same API.

2. Separate write durability from indexing freshness.
   A successful write means the WAL entry is durable. Indexing is async.
   Strong reads include unindexed WAL data through a recent-write overlay.

3. Keep cold reads to a small number of large round trips.
   Object storage has high latency but good throughput. Query plans should
   fetch metadata first, then issue batched/ranged reads.

4. Use an object-storage-native LSM, not a disk-first LSM transplanted upward.
   Immutable sorted files, manifests, CAS commits, and compaction are the
   fundamental storage operations.

5. Prefer clustering over graph traversal for cold vector search.
   Cluster-based ANN bounds object-store round trips better than graph indexes.

6. Make filters aware of vector clustering.
   Filtered ANN is not "vector search plus post-filtering". Attribute indexes
   need cluster-level summaries and row-level bitmaps keyed by vector addresses.

7. Measure recall continuously.
   ANN correctness is a production metric. The system should expose recall
   measurement early, even while the index is simple.

8. Optimize the boring data path before distribution.
   Batching, contiguous blocks, SIMD-friendly loops, direct I/O, and cache
   placement matter more than premature distributed complexity.

## High-Level System

```
client
  |
  v
api/query node
  |       \
  |        \-- memory cache: manifests, schema, hot centroids, hot postings
  |
  \---------- NVMe cache: SST blocks, vector postings, FTS blocks
                 |
                 v
             object store
             / namespace prefixes
             / WAL
             / manifests
             / SSTs
             / vector postings

indexer nodes poll/claim indexing jobs, read WAL/index state, write new
immutable files, and publish new manifests with compare-and-set.
```

The repo should start as a single Rust crate, but code should be organized as if
it could become a workspace:

- `object_store`: filesystem and future S3/GCS backends, range reads, CAS puts.
- `manifest`: namespace manifests, generations, branch pointers, schema state.
- `wal`: write records, group commit, replay, strong-read overlays.
- `sst`: object-storage-native sorted string table format.
- `lsm`: levels, compaction, iterators, merge streams, tombstones.
- `schema`: typed values, vector encodings, validation, evolution rules.
- `query`: logical plans, physical plans, top-k, filters, aggregates.
- `vector`: exact search, IVF/SPFresh, RaBitQ, recall measurement.
- `text`: tokenization, BM25, postings, MAXSCORE.
- `indexer`: job queue, compaction, vector local rebuilds.
- `api`: HTTP surface and request/response types.

## Object Store Contract

Everything should be implemented against a minimal object-store interface:

```rust
trait ObjectStore {
    async fn get(&self, key: &str) -> Bytes;
    async fn get_range(&self, key: &str, range: Range<u64>) -> Bytes;
    async fn put(&self, key: &str, bytes: Bytes) -> ObjectVersion;
    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion, CasError>;
    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion, CasError>;
    async fn list(&self, prefix: &str) -> Vec<ObjectMeta>;
}
```

The production design assumes strong read-after-write and conditional writes.
For local development, the filesystem backend can emulate object versions with
sidecar metadata or atomic rename plus a generation file.

Avoid listing on the query hot path. Manifests should name the exact files to
read. Listing is acceptable for recovery, tooling, and offline repair.

## Namespace Object Layout

Object keys are append-mostly and generation-addressed:

```text
namespaces/{ns}/
  manifest/current                 # CAS pointer to current namespace generation
  manifest/g/{generation}.json     # immutable manifest body
  wal/{epoch}/{seq}.wal            # durable write batches
  wal_commit/current               # CAS commit cursor for grouped WAL entries
  index/g/{generation}/
    doc/*.sst
    attr/*.sst
    fts/*.sst
    vector/{column}/tree.bin
    vector/{column}/postings/*.vpost
    vector/{column}/*.rabitq.bin
  ops/{token}.json                 # async operation status
  branches/{child}.json            # optional branch metadata
```

Global service objects live outside namespace prefixes:

```text
jobs/indexing_queue.json           # durable brokered indexing queue
```

The current manifest records:

- namespace ID and generation;
- schema and schema version;
- WAL commit cursor and indexed cursor;
- active SST files by index family and level;
- vector index generation by column;
- branch parent, if any;
- approximate logical bytes and row count;
- compaction/indexing watermarks.

The manifest is the namespace's catalog. A query starts by loading or validating
it. An indexer publishes work by writing immutable files, then CAS-updating
`manifest/current`.

## WAL And Writes

The WAL is the only synchronous durable write path. Each WAL entry is an atomic
batch:

```text
WalBatch {
  namespace,
  sequence,
  created_at,
  idempotency_key?,
  schema_delta?,
  operations: [
    Upsert { id, full_document },
    Patch { id, partial_document },
    Delete { id },
    Conditional { op, predicate },
    BranchFrom { source_namespace, source_generation },
  ],
  payload_checksum,
}
```

Write path:

1. Parse JSON, validate IDs, infer or check schema, and normalize row/column
   formats into internal rows.
2. For conditional writes and patch/delete-by-filter, read the current snapshot
   and determine affected IDs.
3. Append the batch to a per-namespace group-commit buffer.
4. The group-commit loop writes one WAL object and advances the commit cursor
   with CAS.
5. Enqueue an indexing job.
6. Update the local write-through cache for this node.
7. Return after the WAL commit is durable.

One WAL entry per second per namespace is an acceptable early constraint. The
implementation should still be structured as group commit from the start so
throughput is not tied to object write latency.

Strong reads use the manifest's indexed cursor plus WAL entries after that
cursor. Unindexed rows are searched exhaustively and merged with indexed
results. Eventual reads may use a cached manifest and only include a bounded
recent-write overlay.

Backpressure is based on unindexed WAL bytes. Once the outstanding WAL exceeds a
limit, normal writes return `429` unless the caller explicitly disables
backpressure for bulk ingest. Strong reads may reject queries if the overlay is
too large.

## LSM Storage Engine

The LSM stores logical indexes as sorted immutable key-value files. It backs:

- document storage and primary-key lookup;
- attribute indexes;
- FTS posting blocks;
- vector address metadata;
- schema/metadata side indexes when needed.

SST file shape:

```text
SST object:
  data blocks          # prefix-compressed keys, encoded values
  block index          # min/max key, offset, size, bloom/filter metadata
  range summaries      # optional per-block stats for query planning
  footer               # magic, format version, checksums, index offset
```

The key format must sort lexicographically by index family:

```text
doc/{id} -> DocumentValue(version, deleted?, attrs, vectors_ref)
id_seq/{id} -> latest sequence
attr/{attr_id}/{encoded_value}/{cluster_id?}/{block_id?} -> postings/bitmap
fts/{field_id}/{term}/{block_id} -> weighted posting block
vector_addr/{column}/{id} -> VectorAddress(cluster_id, local_id, version)
```

Compaction:

- merges sorted files within an index family;
- removes overwritten values and tombstones past a retention horizon;
- rewrites posting blocks to maintain target sizes;
- updates approximate namespace stats;
- never mutates files in place.

Iterator design should be batched. The turbopuffer zero-cost/SIMD post makes
the important point: an iterator that yields one item at a time can prevent
unrolling and vectorization. Internal merge streams should expose batches of
keys/postings/bitmap words.

## Query Planning

Every query compiles to:

1. snapshot selection: strong or eventual;
2. logical rank/filter/aggregate expression;
3. index family selection;
4. object read plan;
5. execution pipeline;
6. recent-WAL overlay merge;
7. attribute materialization.

Supported query classes, in build order:

- lookup/order-by over primary key or one attribute;
- exact kNN with mandatory filter;
- ANN vector search;
- BM25 text search;
- filter-only and aggregation;
- grouped aggregation;
- multi-query for hybrid retrieval.

The planner should prefer namespace partitioning over complex filters, but when
filters are present it must use the inverted indexes. Returning fewer attributes
is a physical optimization: materialize only requested columns.

Strong consistency adds a metadata/CAS validation floor and the WAL overlay.
Eventual consistency can skip validation and run directly against warm cache.

## Vector Index

### MVP

Start with exact vector search over filtered candidates and a simple IVF index:

- build KMeans centroids for a vector column;
- assign vectors to leaf clusters;
- at query time, probe nearest centroids and scan vectors in those clusters;
- rerank with exact distance;
- rebuild the IVF index during compaction.

This is enough to validate storage, filters, recall measurement, and API
semantics.

### Target: Hierarchical SPFresh-Style Index

The long-term index is a centroid tree with local, incremental updates.

Data structures per vector column:

```text
VectorTree {
  distance_metric,
  dimension,
  root,
  branching_factor,
  levels,
  centroids,
}

Posting {
  cluster_id,
  generation,
  vectors: [(local_id, row_id, version, full_vector, quant_code?)],
  centroid,
  length,
  tombstone_count,
}

VectorAddress {
  cluster_id,
  local_id,
  version,
}
```

A query:

1. Load or reuse the centroid tree.
2. Walk the hierarchy with a beam/probe width.
3. If filters exist, use cluster-level filter summaries to skip clusters that
   cannot contain matches.
4. Fetch selected posting blocks in one batched read.
5. Use quantized distance estimates when available.
6. Fetch/rerank full vectors for candidates that can enter top-k.
7. Merge unindexed WAL vectors by exact search.

Cluster-based ANN is chosen because it bounds object-store round trips. Graph
indexes can be excellent in memory, but sequential graph traversal is a poor
fit for cold object-store queries.

### LIRE Updates

The SPFresh/LIRE rule is nearest partition assignment: a vector belongs in the
nearest posting centroid. Inserts append to the nearest posting. Deletes mark a
version/tombstone. Background local rebuilds preserve balance:

- split a posting when it exceeds the max length;
- merge nearby postings when they fall below the min length;
- reassign vectors in the split/merged posting and nearby postings when the
  nearest-centroid relation may have changed.

The paper's practical defaults are useful starting points:

- check a bounded local neighborhood for reassignment;
- `top64` nearby postings was enough in their parameter study;
- use a foreground/background thread ratio around 2:1 for insert/rebuild;
- keep reassign off the foreground write path.

Sana should store versions with vector entries and maintain an in-memory or
cached version map. Reassign appends a new copy with a newer version and leaves
old copies stale until garbage collection. Search drops stale versions.

Concurrency rules:

- posting writes use posting-level locks;
- posting reads are lock-free against immutable blocks plus version checks;
- reassign uses CAS on the version map to avoid moving the same vector twice;
- if a target posting disappears during a split, abort and retry that reassign.

### RaBitQ Quantization

RaBitQ is the compressed L2 distance-estimation layer. Each immutable IVF base,
append, or maintenance segment has a separately framed `.rabitq.bin` companion;
the manifest names both objects so older manifests can fall back to exact scans.

For each vector cluster:

- compute cluster centroid `c`;
- normalize each raw vector `o_r` to `o = (o_r - c) / ||o_r - c||`;
- sample/store a random orthogonal transform `P`;
- store the signs of `P^-1 o` as a `D`-bit code;
- store `||o_r - c||`, `<o_bar, o>`, and bit counts needed by the estimator.

At query time:

- normalize and transform the query for each probed cluster;
- stochastically quantize transformed query coordinates to 4-bit unsigned
  integers and decompose them into four bit planes;
- estimate inner products using bitwise AND and popcount;
- convert estimated inner products back to raw squared distances;
- use the error bound to decide which candidates require exact reranking.

Sana loads IVF/companion pairs concurrently only for L2 queries. It applies
native filter masks, version-map liveness, and WAL shadowing before segment
top-k pruning. A candidate is exact-reranked unless its confidence lower bound
is already worse than the current exact kth distance. Cosine and dot queries do
not fetch the companion. The confidence radius includes both the original
RaBitQ estimator bound and the Hoeffding term introduced by 4-bit query
quantization.

The key engineering payoff is that quantized vectors are 16-32x smaller than
full `f16`/`f32` vectors, moving scans higher in the memory hierarchy. The
system still reranks exact full vectors for final correctness.

SIMD work should be isolated behind a trait:

```rust
trait DistanceKernel {
    fn l2_f32_batch(query: &[f32], vectors: &[f32], out: &mut [f32]);
    fn cosine_f32_batch(query: &[f32], vectors: &[f32], out: &mut [f32]);
    fn rabitq_estimate_batch(query: &QuantizedQuery, codes: &[u64], out: &mut [f32]);
}
```

The f32 kernels use cached runtime dispatch to scalar, AArch64 NEON, or x86_64
AVX2 implementations, with randomized parity tests and a dependency-free
release benchmark. RaBitQ uses four AND+popcount passes; AArch64 batches two
`u64` words with NEON byte popcount and other targets use portable
`u64::count_ones`. AVX-512/VPOPCNT should only be added when x86 benchmarks
justify a separate path.

## Native Filtering

Filtered vector search must not be implemented as either pre-filter-only or
post-filter-only. The attribute index must cooperate with the vector index.

Every indexed document in a vector namespace has a vector address:

```text
{cluster_id, local_id, row_id, version}
```

Attribute indexes should provide two levels:

- cluster-level summaries: which clusters contain at least one matching row;
- row-level bitmaps: which local IDs inside a cluster match.

Example key families:

```text
attr_cluster/{attr}/{value}/{chunk} -> compressed bitset(cluster_id)
attr_row/{attr}/{value}/{cluster_id}/{block} -> compressed bitset(local_id)
```

For `filter AND ANN`:

1. Compile the filter expression into bitmap operations.
2. Evaluate cluster-level summaries first.
3. During ANN tree traversal, ignore clusters outside the cluster mask.
4. Fetch row-level bitmaps only for clusters selected by ANN.
5. Scan only matching local IDs inside each posting.

When LIRE reassigns vectors to new clusters, corresponding attribute postings
must be updated. This can be done by writing new LSM keys and tombstones; the
old entries disappear through compaction.

Glob/regex/fuzzy filters can later use trigram indexes to narrow candidates
before exact evaluation.

## Full-Text Search

FTS is an inverted index with BM25 ranking. It should share as much machinery as
possible with attribute postings.

MVP:

- lowercase/simple word tokenizer;
- term -> sorted `(doc_id, term_frequency)` postings;
- BM25 with field length and average length;
- optional filters through bitmap intersection;
- no phrase/prefix/fuzzy support initially.

Target FTS v2-style postings:

- fixed posting blocks around 256 postings;
- split blocks above 512 postings and merge below 128;
- bitpack doc deltas and term-frequency/weight data;
- store block-local max score for dynamic pruning;
- generic posting block over weight type, so filters can use zero-sized weights.

Query algorithm:

- compile `rank_by` into clauses: BM25 terms, filter boosts, attribute scores;
- use vectorized MAXSCORE rather than WAND as the default;
- batch per-posting-list work to preserve memory locality and SIMD potential;
- maintain a top-k heap and skip blocks whose max score cannot beat the heap
  minimum.

Rank expressions should support:

- `Sum`, `Max`, `Product`;
- `BM25(field, query)`;
- rank-by-filter as a 0/1 score;
- rank-by-attribute via `Attribute`, `Saturate`, `Decay`, and `Dist`.

This keeps first-stage ranking expressive while avoiding an application-specific
query language for second-stage relevance.

## Caching

Cache tiers:

- object store: durable source of truth;
- NVMe/local disk: SST blocks, vector postings, FTS blocks;
- memory: manifests, schemas, file indexes, hot centroid levels, hot bitmaps.

The cache key should include object key, byte range, object version, and
checksum. Cached bytes are immutable. Manifest changes naturally point at new
immutable objects.

Routing should prefer cache locality: route a namespace to the same query node
when possible, while preserving the invariant that any node can serve any
namespace after a cold read.

`CachingObjectStore` implements the memory tier for immutable manifest bodies
and generation-addressed index objects with byte-bounded LRU admission.
Mutable pointers/cursors always bypass it. `hint_cache_warm` captures one
manifest generation and, under an explicit byte budget, loads the manifest and
vector families first, followed by text/attribute/document SSTs. For pinned
namespaces, a future scheduler can reserve query nodes and keep the namespace's
working set on their NVMe drives.

## Indexing Queue

Indexing jobs are notifications, not the source of truth. The WAL and manifest
are authoritative.

MVP: scan namespaces for unindexed WAL and run indexers in a local process.

Implemented core: a single object-store queue file with brokered group commit:

- clients send push/claim/heartbeat/complete requests to a stateless broker;
- the broker batches changes and CAS-writes `jobs/indexing_queue.json`;
- if the broker dies, another broker takes over;
- workers heartbeat claimed jobs;
- timed-out jobs are returned to the queue;
- delivery is at least once, so index jobs must be idempotent.

Each job carries a namespace and target WAL cursor. Pending jobs for one
namespace coalesce to the highest cursor, but a write arriving behind an active
claim creates a follow-up notification. Claims are fenced by an incrementing
attempt number, so a timed-out worker cannot complete a job after takeover.
Queue publication is best-effort after WAL commit: write durability never
depends on this advisory file. A reconciliation scan compares each namespace's
commit and indexed cursors and restores missed notifications.

Index job idempotence comes from deterministic output names or generation CAS:
if a job sees that its WAL range is already indexed by the current manifest, it
exits successfully.

## Branching And Copy

Branching is a manifest operation:

```text
child manifest:
  parent_namespace = source
  parent_generation = source_generation
  overlay_wal_cursor = empty
  overlay_index = empty
```

Reads merge parent generation files with child overlay files. Writes to parent
and child are independent after branch creation. Compaction can later materialize
or flatten branch chains.

`copy_from_namespace` is a physical or logical copy operation. It is useful for
cross-region copies, re-encryption, and backups. Branching is the preferred
same-region clone.

## Guarantees

Target guarantees:

- Durable writes: success means WAL commit is durable in object storage.
- Atomic batches: all operations in one write batch become visible together.
- Strong reads by default: include all writes committed before the query starts.
- Eventual reads optional: lower latency, bounded stale overlay.
- Atomic conditional writes: evaluate predicate and write against one snapshot
  and commit with CAS.
- Patch/delete-by-filter: two phase, read committed semantics; identify IDs
  from a snapshot, then re-evaluate each matching ID before modifying.
- Any node can serve any namespace.
- Object storage is the only required stateful dependency.

## Observability

Required metrics from the beginning:

- write latency, WAL bytes/sec, group size;
- unindexed WAL bytes per namespace;
- index lag by namespace and index family;
- query latency split by planning, object reads, cache reads, scoring,
  materialization, WAL overlay;
- cache hit ratio and cache temperature;
- object-store request count, range-read bytes, CAS failures;
- ANN recall@k from explicit endpoint and sampled traffic;
- vector probe counts, candidate counts, exact rerank counts;
- FTS postings decoded, blocks skipped, heap threshold evolution;
- compaction input/output bytes and write amplification.

The recall endpoint should run exact search over sampled vectors and compare it
to ANN. It is expensive but essential.

## Staged Build Plan

### Stage 0: Skeleton Decisions

- Define internal row/value/schema types.
- Define `ObjectStore` with filesystem backend.
- Define namespace manifest and WAL batch formats.
- Add golden serialization tests.

First commits in the current crate:

```text
src/
  main.rs                 # CLI/API entry point
  lib.rs                  # module exports
  object_store/
    mod.rs                # ObjectStore trait
    fs.rs                 # filesystem backend
  manifest.rs             # NamespaceManifest, generation pointer
  wal.rs                  # WalBatch, WalOp, codec
  value.rs                # ID, Value, Document, VectorValue
  schema.rs               # inferred/declared schema
  error.rs                # shared Result/Error
tests/
  manifest_codec.rs
  wal_codec.rs
  fs_object_store.rs
```

Do not add HTTP, async job queues, or ANN in Stage 0. The first useful
milestone is: create a namespace on the filesystem object store, append a WAL
batch, CAS-advance the manifest, and replay the namespace into documents.

### Stage 1: Durable Documents

- Implement WAL append and manifest CAS.
- Implement document upsert/delete.
- Implement strong lookup by replaying WAL over snapshot.
- Expose a small local HTTP API or CLI.

### Stage 2: SST/LSM

- Implement SST writer/reader/range iterator.
- Build document SSTs from WAL.
- Implement compaction and tombstone cleanup.
- Query from manifest plus unindexed WAL overlay.

### Stage 3: Attributes And Exact Search

- Implement typed schema inference/checking.
- Implement attribute inverted indexes for equality/range basics.
- Implement filters, order-by, count/sum aggregation.
- Implement exact vector kNN over filtered candidates.

### Stage 4: ANN v0

- Implement KMeans/IVF per vector column.
- Build immutable vector postings.
- Probe nearest clusters, scan full vectors, rerank.
- Add recall endpoint.

### Stage 5: Native Filtering

- Add cluster-level attribute summaries.
- Add row-level local-ID bitmaps.
- Make ANN traversal filter-aware.
- Measure filtered recall.

### Stage 6: SPFresh Local Rebuild

- Add mutable append path to vector postings.
- Add version map and stale-vector handling.
- Implement split/merge/reassign background jobs.
- Keep index quality under insert/delete churn without global rebuild.

### Stage 7: Full-Text Search

- Implement tokenizer, BM25 stats, and simple postings.
- Add BM25 query support and hybrid multi-query.
- Upgrade postings to fixed-size blocks with block max scores.
- Add vectorized MAXSCORE.

### Stage 8: RaBitQ And Kernels

- Add per-cluster RaBitQ code generation.
- Add quantized query path and error-bound rerank selection.
- Add portable bitwise kernels, then SIMD kernels with feature detection.
- Benchmark cache/memory/CPU bottlenecks.

### Stage 9: Object Store Operations

- Implement brokered indexing queue.
- Add warm-cache endpoint and cache admission/eviction policies.
- Add branch/copy/export operations.
- Add pinning/read replicas only after single-node efficiency is proven.

## Risks

- Object-store semantics vary. Keep the backend contract explicit and test CAS,
  range reads, and list consistency separately.
- Native filtering can dominate complexity. Start with equality filters and
  cluster summaries before supporting every operator.
- SPFresh implementation can corrupt recall silently. Keep exact-search recall
  checks and version-map invariants in tests.
- FTS performance depends on posting layout. Avoid per-posting KV entries.
- SIMD can make code brittle. Keep scalar kernels as the reference.
- Branch chains can increase read amplification. Add chain-depth limits or
  background flattening before heavy branch use.

## Source Notes

Most public turbopuffer docs describe the product and operational contract:
object-storage source of truth, WAL/group commit, strong vs eventual reads,
stateless query/indexing nodes, warm cache, namespace pinning, branching,
metadata, limits, and recall measurement.

The turbopuffer architecture and ANN v3 posts drive the high-level storage and
vector-index choices: cache hierarchy, bounded object-store round trips,
hierarchical clustering, binary quantization, exact reranking, and sharding
only after single-machine efficiency is high.

The native filtering and FTS v2 posts drive the index layout: vector addresses,
cluster-aware attribute indexes, fixed-size posting blocks, bitpacking,
block-max metadata, vectorized MAXSCORE, and batched iterators.

SPFresh supplies the incremental vector update model: nearest partition
assignment, split/merge/reassign, versioned stale entries, foreground updater,
background local rebuilder, and append-optimized posting storage.

RaBitQ supplies the quantization model: normalize within clusters, randomized
binary codes, unbiased distance estimation with error bounds, bitwise/SIMD
estimation, and error-bound-driven exact reranking.
