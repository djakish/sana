# Sana — Architecture

This document describes the engine **as it currently stands**. It is the place
to look for "how does Sana work today"; [`PROGRESS.md`](PROGRESS.md) is the
dated build log (every decision `D1`–`D73`, in the order it happened) and is
deliberately *not* rewritten when a decision is later superseded.

Sana is an object-storage-native search database. Every durable byte lives in an
object store — a local directory or S3 — behind one minimal interface. There is
no separate storage tier, no attached disk that must survive a node, no
replication protocol of its own: durability, and (on S3) cross-node mutual
exclusion, are delegated to the object store. The engine is a single Rust crate,
one module per subsystem, usable as a library, a CLI, or an HTTP service.

---

## 1. The object-store boundary

Everything is expressed against one trait (`src/object_store.rs`):

```rust
trait ObjectStore {
    async fn get(&self, key) -> GetResult;             // bytes + observed version
    async fn get_range(&self, key, range) -> Bytes;    // ranged read
    async fn put(&self, key, bytes) -> ObjectVersion;
    async fn put_if_absent(&self, key, bytes) -> ObjectVersion;            // create-only
    async fn compare_and_set(&self, key, expected, bytes) -> ObjectVersion; // CAS
    async fn list(&self, prefix) -> Vec<ObjectMeta>;   // recovery/tooling, not hot path
    async fn delete(&self, key);                       // idempotent
}
```

Two primitives carry the whole design: **`put_if_absent`** (immutable objects
are written exactly once) and **`compare_and_set`** (the few mutable objects
advance by optimistic concurrency).

### Content-addressed versions

An `ObjectVersion` is an opaque token. For content-addressed objects it is the
**SHA-256** of the bytes (`sha256-<hex>`); Sana also recognizes its earlier
fixed-key **SipHash-1-3** content hash (`<16 hex>`) so objects written before the
digest change still validate (`ObjectVersion::matches_content`). CAS only ever
needs version *equality*, and every recovery path that re-verifies an immutable
object compares **bytes**, never a token, so a version produced by a different
backend (e.g. an S3 ETag) simply mismatches — which is the correct CAS answer.

> The SipHash path exists because `std`'s `DefaultHasher` does not guarantee a
> stable algorithm across Rust *versions*; pinning the algorithm via the
> `siphasher` crate froze the legacy key strings, while new content moved to
> SHA-256. (This resolves the concern flagged in PROGRESS.md's Stage 3–5
> follow-ups under "version_of still uses DefaultHasher".)

ABA — the hazard of content versioning — does not arise in normal operation:
every mutable CAS target embeds a strictly increasing number (manifest
generation, queue job-id counter, pinning revision), so its content never
repeats.

### Backends and decorators

| Layer | Role |
|---|---|
| `FsObjectStore` | local filesystem; CAS via an in-process lock + atomic temp-file rename. Crash-safe, but **single-process only** (D4). |
| `S3ObjectStore` | presigned SigV4 over `reqwest`; CAS is S3-native (`If-None-Match: *`, `If-Match: <etag>`), so it holds **across processes and nodes**. |
| `MeteredObjectStore` | decorator that counts requests, bytes, and CAS rejections — placed *below* the cache so hits don't inflate backend counts. |
| `CachingObjectStore` | byte-bounded LRU that admits **only immutable** objects (manifest bodies, `index/g/...`); mutable pointers/cursors/queue always bypass. |

`sana serve` composes them as `Caching(Metered(backend))`.

---

## 2. On-disk object layout

Everything for a namespace lives under `namespaces/{ns}/`. Immutable index
objects carry a content-version suffix in their filename (shown as `-<ver>`).

```
namespaces/{ns}/
  manifest/current                      # ManifestPointer  → live generation   (MUTABLE, CAS)
  manifest/g/{gen}.json                 # immutable manifest body
  manifest/g/{gen}-<ver>.json           # content-keyed body (CAS-loser-safe)

  wal_commit/current                    # committed cursor + cumulative bytes
                                        #   + ≤1 pending reservation            (MUTABLE, CAS)
  wal_staging/{epoch}/{seq}-<ver>.wal   # immutable staged batch (pre-publish)
  wal/{epoch}/{seq}.wal                 # durable committed batch
  idempotency/{hex(key)}.json           # request fingerprint → cursor
  idempotency/{hex(key)}.outcome-<ver>.json   # conditional/filter write outcome

  index/g/{gen}/doc/flush-{seq}-<ver>.sst       # one flush  (LSM L0)
  index/g/{gen}/doc/tier-*-<ver>.sst            # size-tiered merged run
  index/g/{gen}/doc/compacted-<ver>.sst         # full compaction
  index/g/{gen}/attr/*-<ver>.sst                # attribute postings (delta/tiered)
  index/g/{gen}/fts/*-<ver>.sst                 # BM25 text postings
  index/g/{gen}/vector/{col}/ivf.bin            # IVF base (centroids + postings + filter bitmaps)
  index/g/{gen}/vector/{col}/append-{gen}.ivf.bin     # incremental vector delta
  index/g/{gen}/vector/{col}/{reassign,local-rebuild}-*.ivf.bin  # SPFresh maintenance
  index/g/{gen}/vector/{col}/*.rabitq.bin       # RaBitQ companion (per segment)
  index/g/{gen}/vector/{col}/versions.bin       # id → live vector version

  routing/pinning.json                  # leased replica routing control        (MUTABLE, CAS)

jobs/indexing_queue.json                # store-global indexing notification queue (MUTABLE, CAS)
```

Reading a namespace is always: `get manifest/current` → `get` the named body →
the body names every index object to read. The store is never `list`ed on the
query path.

---

## 3. The manifest — per-namespace catalog

`manifest/current` is a tiny `ManifestPointer` naming the live generation and its
immutable body. The body (`NamespaceManifest`, pretty JSON, deterministic via
`BTreeMap`) carries:

- `schema` — inferred column types (strict; see §6).
- `wal_commit_cursor` *(snapshot)* and **`indexed_cursor`** — the WAL position
  folded into these indexes. The gap `(indexed_cursor, commit]` is the **overlay**.
- `indexed_wal_bytes` — cumulative committed WAL bytes absorbed, paired with the
  commit state's counter so overlay *size* needs no WAL listing (§6, backpressure).
- `doc_ssts`, `attr_ssts`, `text_ssts`, `vector_indexes` — the index families (§4).
- `branch_parent`, approximate row/byte stats, timestamps.

Indexing publishes by writing immutable objects first, then **CAS-advancing**
`manifest/current` to a new generation. A lost CAS leaves orphaned immutable
objects that operator GC can report and reclaim after an external safety check —
never corruption.

---

## 4. Index families

All four share the immutable, generation-addressed SST shape (`src/sst.rs`:
prefix-compressed blocks, per-block CRC, a checksummed footer; whole-object load
for scans, `ranged_get` of footer+index+one block for point lookups).

| Family | Module | Shape | Read role |
|---|---|---|---|
| **Documents** | `indexer`, `doc`, `sst` | size-tiered LSM SSTs of *whole* resolved docs (or tombstones); ordered newest-first; `[min_id,max_id]` per run | base layer; first run containing a key wins |
| **Attributes** | `attr` | delta + size-tiered postings keyed `column+encoded scalar → sorted id list` | **candidate generation** (a *superset*) for `Eq`/`Range`/`And`/`Or` |
| **Full-text** | `text` | BM25 field stats + term postings in target-256 blocks with block-local max scores | BM25 top-k via rank-safe block-skipping MAXSCORE |
| **Vector** | `vector`, `vector/{filter,maintenance,kernels}`, `rabitq` | IVF base + append/reassign/local-rebuild delta segments; per-segment RaBitQ companion; embedded filter bitmaps; `versions.bin` liveness map | ANN probe → masked scan → estimate-prune → exact rerank |

Two invariants make the non-document families safe to keep as *approximate
supersets* (see §7): the attribute index need only over-approximate because every
candidate is rechecked against the live document; the vector index may carry
stale copies because the version map decides liveness at query time.

---

## 5. Write path

`Namespace::append_with_options` (and the upsert/delete/patch convenience
wrappers) drive one protocol (`src/namespace.rs`). WAL ops are `Upsert`,
`Patch` (null clears a field), and `Delete`.

1. **Schema infer + validate.** If the batch introduces columns, a *schema-only*
   manifest generation is published first; matching writes touch no manifest.
2. **Stage** the encoded batch as an immutable `wal_staging/...` object.
3. **CAS-reserve** the next sequence by writing a `pending` reservation into
   `wal_commit/current`. The projected post-commit unindexed byte count is checked
   *inside* this CAS loop, so concurrent handles share one namespace budget.
4. **Publish** the canonical `wal/{epoch}/{seq}.wal`, write the durable
   idempotency record, then **CAS-advance** the committed cursor (and cumulative
   byte counter).

The reservation is the linearization point. Any writer that finds a `pending`
reservation **finishes it** (verifies staged bytes, publishes, advances) before
reserving its own sequence, so a crash mid-protocol and concurrent namespace
handles can neither lose nor overwrite an accepted batch. Write durability lives
entirely in `wal_commit/current`; the manifest moves only when indexing runs —
this is the write-durability / index-freshness separation.

**Conditional writes** linearize on the same reservation: conditions are
evaluated against the committed snapshot and the exact cursor is CAS-reserved; a
CAS loser re-reads and re-evaluates. **Patch/delete-by-filter** are two-phase Read
Committed: phase one captures matching ids from a strong snapshot, phase two
turns them into conditional ops rechecked under the reservation.

---

## 6. Read & query path

`query::execute_with_options` captures one snapshot — manifest pointer → body,
plus the commit cursor — then builds the **overlay** by reading the WAL range
`(indexed_cursor, commit]` (its size is bounded by the same byte budget as
writes). All ranking runs against that consistent base+overlay, giving
**read-after-write** consistency. Then the planner picks a path:

- **Text** (`execute_text`) — BM25 MAXSCORE when the index is current
  (`indexed_cursor == commit`), else exhaustive scoring of the strong snapshot.
- **ANN fast path** (`execute_ann_vector`) — only when unfiltered & non-aggregate:
  probe nearest centroids → masked posting scan → drop stale/WAL-shadowed ids via
  the version map → RaBitQ estimate, prune by the lower-bound test → exact rerank
  the survivors → merge exact-scored overlay vectors.
- **Otherwise** (`materialize_candidates`) — attribute index yields a superset of
  candidate ids (or full scan for `Not`/unsupported filters); `resolve_ids` reads
  each doc SST **once**, applies the overlay, then the filter is **rechecked**
  against each live document. Aggregates, exact-vector scoring or order-by, and
  `limit` follow.

Point lookups (`Namespace::lookup`) skip the planner and use ranged SST point
reads with `[min_id,max_id]` pruning.

Backpressure: `unindexed_wal_bytes = committed_wal_bytes − indexed_wal_bytes`
(two small reads, no listing). Writes past the limit (default **2 GiB**) get
`Backpressure` → HTTP `429`; strong queries/recall reject oversized overlays too.

---

## 7. Core invariants

These hold everywhere and are the reason the moving parts compose:

- **Always-recheck superset.** Index lookups need only over-approximate the
  answer; the materialize step rechecks every candidate against the live
  document. So delta/tiered attribute postings, stale value keys, and
  newest-wins overlays are all harmless false positives. (`Not` is the lone
  exception — complement needs exact membership, so it falls back to full scan.)
- **Version-map liveness.** A vector copy is live iff `versions.bin` maps its id
  to that copy's version. Append/reassign/rebuild segments simply write newer
  versions; old copies become invisible without being deleted.
- **Content-addressed immutability.** Immutable objects are written with
  `put_if_absent` at content-derived keys; re-running an interrupted publish is a
  no-op, and a CAS-loser cannot overwrite the winner's body.
- **Catalog-last publication.** Immutable objects are written before the manifest
  CAS that references them; `export` writes its `catalog.json` last. A reader
  therefore never sees a catalog pointing at a missing object.
- **GC fail-closed.** Automatic deletion is disabled by default. `sana gc`
  reports orphaned immutable objects as a dry-run, and `sana gc --apply` remains
  an operator action for controlled quiescent deployments. The legacy two-pass
  maintenance GC can be explicitly enabled in-process, but production
  multi-pod reclamation needs durable reader/publisher watermarks before delete.
- **Epoch fail-closed.** WAL epoch rotation is unimplemented; overlay/flush/GC
  compare full cursors and reject cross-epoch ranges rather than risk a silent
  stale read.

---

## 8. Indexing & maintenance

| Operation | Module | Effect |
|---|---|---|
| `flush` | `indexer` | fold the WAL delta into a new L0 doc SST + attribute delta + text snapshot + vector append (or base) and version map; CAS a new generation; advance `indexed_cursor` |
| tiering | `indexer` | when a level reaches the run threshold, fold it into one run at the next level |
| `compact` | `indexer` | merge all runs into one, drop tombstones, rebuild clean attribute/text snapshots, rebuild the IVF base & clear the append chain |
| `maintain_vectors` | `indexer`, `vector/maintenance` | publish SPFresh split/merge/reassign delta segments from manifest-planned tasks |
| `gc` | `indexer` | `list` the namespace prefix, delete anything the live manifest no longer references (plus already-folded WAL) |

**Coordination.** `jobs/indexing_queue.json` is a *notification* layer (WAL +
manifest stay authoritative): each commit best-effort enqueues its cursor;
**fenced, leased** workers claim one namespace at a time (one live publisher per
namespace). `reconcile_unindexed` repairs missed notifications by comparing
authoritative cursors and doubles as the per-namespace lag metric.

`sana serve --role all` runs this itself: an embedded indexing worker (poll,
lease, heartbeat, retry; reconcile every 30 s) plus a maintenance loop (every
60 s: threshold-driven compaction *or* vector maintenance; automatic GC is off
by default). Multi-pod deployments use `sana serve-api` for HTTP-only query/write
pods, `sana work-indexing --loop` for leased indexing workers, and
`sana maintain --loop` for all-namespace maintenance. API-only pods do not claim
jobs or scan namespaces, so query scaling no longer multiplies background object
store traffic.

---

## 9. Operations

- **Branch** — requires a fully indexed source; flattens one generation into the
  child manifest, reusing the parent's immutable objects and resetting child WAL
  cursors. GC treats foreign references into a namespace as live, so a parent
  compaction can't reclaim objects a branch still owns.
- **Copy** — physically streams every referenced object to a destination store
  under fresh generation-0 keys with an independent WAL.
- **Export** — writes content-checksummed objects under an arbitrary prefix and a
  deterministic `catalog.json` **last**.
- **Pin** — `routing/pinning.json` holds a leased, fenced replica assignment per
  slot; a replica becomes routable only after warming the exact current
  generation; routing hashes namespace+key over ready replicas.

---

## 10. Service, caching & observability

The HTTP surface (`src/api.rs`) is a thin Axum adapter over the same library
methods the CLI uses — not a second implementation. Routes: write
(`POST /v2/namespaces/{ns}`), single/multi query, metadata, `_debug/recall`,
`hint_cache_warm`, `/metrics`, `/livez`, `/readyz`, `/healthz`. Errors map to stable
`400/404/409/429/500` JSON envelopes; per-namespace query concurrency is bounded
by a weighted semaphore.

Kubernetes lifecycle: liveness is process-local and does not depend on S3;
readiness fails during startup, drain, local query-slot overload, or a bounded
backend-list failure. Ctrl-C/SIGTERM flips readiness off before graceful HTTP
shutdown so load balancers can stop routing while in-flight requests drain.

Metrics (`src/metrics.rs`) are a dependency-free in-process registry rendered as
Prometheus text. Because the meter sits *below* the cache, object-store counters
measure true backend egress; latency histograms record dominant phase seams of
the write and query paths; gauges cover cache temperature and per-namespace index
lag.

> **Not yet present:** authentication and TLS on the HTTP service (bind to
> localhost or front it with a reverse proxy), WAL epoch rotation, IAM-role S3
> credentials (env-vars only), and a turbopuffer wire-compatibility layer.

---

## 11. Selected limits

| Limit | Value | Where |
|---|---|---|
| Unindexed-WAL backpressure | 2 GiB (configurable) | `backpressure.rs` |
| HTTP request body | 64 MiB | `api.rs` |
| Max query results / default `limit` | 10,000 | `query.rs` |
| Queries per multi-query | 16 | `query.rs` |
| Full-text query length | 1 KiB | `query.rs` |
| Patch / delete-by-filter defaults | 50k / 5M rows | `write.rs` |
| Per-namespace query slots | 16 | `api.rs` |
| Idempotency key | 1–256 bytes | `namespace.rs` |
| Vector columns / dimensions | 2 / 10,752 | `schema.rs` |

---

For the module-by-module file map and the full decision history, see
[`PROGRESS.md`](PROGRESS.md).
