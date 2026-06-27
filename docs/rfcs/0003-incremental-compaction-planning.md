# RFC 0003: Incremental, byte-aware compaction planning

- **Status:** Draft (for review)
- **Author:** djakish (AI-assisted)
- **Created:** 2026-06-27
- **Tracking:** `docs/TODO.md` → "P2: Reduce full-compaction write spikes"
- **Touches invariants:** storage compatibility, durability, write amplification.
  Persisted SST/manifest formats must stay byte-stable; this RFC changes *when
  and how much* we rewrite, not the on-disk encoding.

## 1. Summary

Replace the single all-or-nothing `compact` path with a **compaction planner**
that bounds the bytes rewritten per maintenance pass and separates the four jobs
full compaction currently fuses (tombstone cleanup, stale-attribute cleanup,
full-text rebuild, vector-base reset). Make the trigger thresholds byte-aware
rather than purely run-count based. Ship compaction-bytes metrics first so policy
changes are driven by measurement.

## 2. Motivation

Sana already does lightweight within-level tiering, but the operator-facing
`compact` (`src/indexer.rs:782`) is the only path that:

- drops tombstones (safe only at a full merge — "nothing older remains"),
- rebuilds stale-free attribute and full-text indexes,
- rebuilds the vector base and clears the append-delta chain.

Because these are fused, any one of them forces a **whole-namespace rewrite**.
The default policy (`MaintenancePolicy`, `src/maintenance.rs`) triggers it at 8
doc/attr runs or 4 vector appends — a *count*, with no regard for the bytes
behind those counts. The result is a periodic write spike: a namespace that
crossed a count threshold rewrites everything at once, multiplying object-store
PUT traffic and competing with foreground writes.

This is the classic LSM **write-amplification vs space/read-amplification**
trade-off. The literature (RocksDB, Dostoevsky, Lucene, Cassandra) is unanimous
that a *planner* bounding work per cycle beats a single full merge:

- RocksDB bounds each compaction to a set of overlapping files between two
  levels, not the whole tree.
- Lucene's `TieredMergePolicy` merges a budgeted number of roughly-equal-size
  segments per cycle and caps the merged-segment size.
- Cassandra's size-tiered/leveled strategies (STCS/LCS) explicitly trade write
  amplification for read/space amplification per policy.

turbopuffer is LSM-shaped over object storage (WAL → async index, per the
architecture doc) and uses SPFresh for vectors precisely because a centroid
index "minimizes round-trips and write-amplification compared to graph indexes."
Bounded, incremental maintenance is consistent with that object-storage-native
posture: small, frequent rewrites beat rare massive ones when every write is an
S3 PUT.

## 3. Current state in Sana

Index objects (`src/indexer.rs`):

| Component | Objects | Compaction behavior today |
| --- | --- | --- |
| Documents | `doc/flush-{seq}.sst`, `doc/tier-*.sst`, `doc/compacted.sst` | `tier_doc_ssts` merges within a level; `compact` merges all + drops tombstones |
| Attributes | `attr/*.sst`, `attr/tier-*.sst` | `tier_attr_ssts` merges within a level; `compact` rebuilds stale-free |
| Full text | `fts/*.sst` | rebuilt only by `compact` |
| Vectors | `vector/{col}/ivf.bin` + `append-{n}.ivf.bin` + `rabitq` + `versions.bin` | `maintain_vectors` does bounded split/merge; `compact` resets base + clears appends |

Triggers: `compact_at_runs = 8`, `compact_at_vector_appends = 4`
(`MaintenancePolicy::default`). Run-count only.

Observability prerequisite (D86, this branch): maintenance now exports a
`compactions` **count**. It does **not** yet export compaction **bytes**, which
the TODO requires *before* changing automatic policy. That gap is part of this
RFC's milestone 1.

## 4. Design

### 4.1 A compaction plan, not a compaction call

Introduce `plan_compaction(&manifest, &policy) -> CompactionPlan`, a pure
function (easy to unit-test and to property-test via RFC 0002) that emits a list
of bounded **jobs** instead of one monolith:

```text
CompactionJob ::=
  | MergeDocRun   { level, inputs: [SstMeta], est_bytes }
  | MergeAttrRun  { level, inputs: [SstMeta], est_bytes }
  | DropTombstones{ scope, est_bytes }      // only when provably safe
  | RebuildText   { est_bytes }             // decoupled from doc merge
  | VectorBaseReset { column, est_bytes }   // decoupled; or defer to maintain_vectors
```

The planner returns jobs ordered by benefit/cost, and the maintenance pass
executes jobs until it hits a **per-pass byte budget** (`max_rewrite_bytes`).
Whatever does not fit waits for the next pass. This is the core change: work is
*sliced*, so the spike becomes a steady trickle.

### 4.2 Byte-aware thresholds

Replace pure run-count triggers with byte-aware ones, keeping run-count as a
secondary guard (many tiny runs still hurt read amplification even at low bytes):

```text
needs_doc_merge  = doc_run_bytes_in_level ≥ level_target_bytes
                   OR doc_run_count ≥ compact_at_runs
needs_tombstone_gc = tombstone_bytes ≥ tombstone_budget (or fraction of live)
```

`SstMeta` already carries row counts and byte sizes, so the planner can estimate
without reading object bodies (no `list`/`get` on a hot path; planning runs in
the maintenance loop).

### 4.3 Decoupling the four fused jobs

- **Tombstone cleanup** stays correctness-bounded: tombstones may be dropped only
  where no older run could resurrect the key. The planner emits `DropTombstones`
  only for the bottom level / full overlap, exactly as `compact` reasons today —
  but as its own job, not bundled with text/vector rewrites.
- **Stale-attribute cleanup** rides the attribute merge jobs.
- **Full-text rebuild** becomes its own job; it does not require the doc base to
  be fully merged first.
- **Vector-base reset** stays the most expensive; prefer incremental
  `maintain_vectors` (already bounded) and only schedule a base reset when the
  append chain's *bytes* (not count) justify it.

### 4.4 Safety and ordering (unchanged invariants)

Every job still: writes new immutable objects, then CAS-publishes a new manifest
that names them (publish-before-pointer), then leaves superseded objects as GC
candidates (never deletes inline — that is RFC 0001 / P0 territory). A job that
loses the manifest CAS or fails its publish fence is a no-op that wrote some
orphan bytes — the existing GC handles those. Bounding bytes per pass does not
weaken any of this; it only changes batch size.

### 4.5 Metrics (ship first)

Add to `MaintenanceMetrics`/`WorkerMetrics` (extending D86):

- `compaction_bytes_rewritten_total` (counter) and a per-job-kind breakdown.
- `compaction_jobs_planned` / `_executed` / `_deferred` (so a backlog is
  visible).
- `tombstone_bytes`, `append_chain_bytes` gauges per namespace (lets us *see*
  the trade-off before touching thresholds).

Only after these are live and observed should `max_rewrite_bytes` and the
byte thresholds be tuned. "Expose compaction bytes in metrics before changing
automatic policy" is an explicit TODO gate.

## 5. Alternatives considered

1. **Keep full compaction, just run it less often.** Reduces spike frequency,
   not spike size; a large namespace still stalls when it fires. Rejected.
2. **Adopt strict RocksDB-style leveled compaction.** Well-understood write
   amplification, but a larger rewrite of the SST layout and level metadata than
   Sana needs now. The planner here is closer to Lucene's tiered policy, which
   fits append-of-immutable-SSTs better. Revisit leveled if read amplification
   becomes the bottleneck.
3. **Tune only the count thresholds.** Cheapest, but never addresses the
   count-vs-bytes mismatch (8 tiny runs ≠ 8 huge runs). Rejected as the primary
   fix; retained as a secondary guard.

## 6. Testing plan

- Unit-test `plan_compaction` as a pure function: given a synthetic manifest,
  assert the job list, ordering, and that total scheduled bytes ≤ budget.
- Property test (RFC 0002): random write/flush/tier/compact episodes; assert
  index==scan (I5) holds across *incremental* compaction, tombstones eventually
  disappear, and no single pass rewrites more than `max_rewrite_bytes`.
- Regression: a tombstone-heavy namespace reclaims tombstone space without a
  full text/vector rebuild in the same pass.
- Benchmark (`examples/latency.rs`): compare object-store PUT bytes over a
  write-heavy run, full-compaction vs planned; expect a flatter write curve at
  equal or better read latency.

## 7. Risks and open questions

- **R1 — deferring tombstone GC too long** raises space and read amplification.
  The byte budget must not starve `DropTombstones`; the planner should prioritize
  it once `tombstone_bytes` crosses its budget.
- **R2 — vector base reset is lumpy.** Even byte-bounded, a base reset is large.
  Open question Q1: can the vector base itself be tiered (partial reassign) so it
  never needs a single full reset? (Likely a follow-up RFC on SPFresh-style
  incremental rebuild.)
- **Q2 — budget units.** Is `max_rewrite_bytes` per pass, per namespace, or
  store-global across the maintenance leader? (Proposed: per namespace per pass,
  with a store-global ceiling enforced by the maintenance leader.)
- **Q3 — interaction with minor tiering.** Should `tier_doc_ssts`/
  `tier_attr_ssts` be folded into the planner, or stay as a fast inline path the
  planner only supplements? (Proposed: planner supplements; keep the fast path.)

## 8. References

- RocksDB compaction (leveled & universal/tiered):
  https://github.com/facebook/rocksdb/wiki/Compaction
- Niv Dayan, Stratos Idreos, *Dostoevsky: Better Space-Time Trade-Offs for
  LSM-Tree Based Key-Value Stores*, SIGMOD 2018:
  https://stratos.seas.harvard.edu/files/stratos/files/dostoevsky.pdf
- Apache Lucene `TieredMergePolicy` — budgeted, size-capped segment merges:
  https://lucene.apache.org/core/9_0_0/core/org/apache/lucene/index/TieredMergePolicy.html
- Cassandra compaction strategies (STCS / LCS / TWCS):
  https://cassandra.apache.org/doc/latest/cassandra/operating/compaction/
- CockroachDB Pebble — LSM storage engine design notes:
  https://github.com/cockroachdb/pebble
- SPFresh (turbopuffer's vector index basis), ACM:
  https://dl.acm.org/doi/10.1145/3600006.3613166
- turbopuffer: `sources/turbopuffer-export/docs/architecture.md`,
  `sources/turbopuffer-export/blog/zero-cost.md`.
- Sana internals: `src/indexer.rs` (`compact`, `tier_doc_ssts`,
  `tier_attr_ssts`, `maintain_vectors`), `src/maintenance.rs`
  (`MaintenancePolicy`), `docs/PROGRESS.md` D86 (maintenance metrics).
