# Benchmarks

Honest numbers from the bundled harness — they exist to track regressions and
to show the engine's cost shape, not to win comparisons. Local filesystem is
not S3: real object-store latency moves writes from milliseconds to tens of
milliseconds and makes the cache the whole story for reads.

```sh
cargo run --release --example latency
cargo run --release --example latency -- <dir> <writes> <dim> <queries>
```

The harness runs the same decorator stack as `sana serve`
(`Caching(Metered(Fs))`) and reports per-operation percentiles plus the true
backend traffic the run generated.

## Apple M1 (8 GB), macOS 26.2, local SSD — 2026-06-10

5,000 docs, 64-dim vectors, 1,000 queries per shape, release build.

| Operation | p50 | p90 | p99 | throughput |
|---|---|---|---|---|
| write, 1 doc / WAL commit | 60.0 ms | 67.7 ms | 75.9 ms | 17 commits/s |
| write, 100 docs / WAL commit | 67.6 ms | 71.2 ms | 74.8 ms | **1,481 docs/s** (0.68 ms/doc) |
| flush (index 10,000 docs) | — | — | — | 533 ms total |
| point lookup | 0.075 ms | 0.084 ms | 0.103 ms | 13,046 ops/s |
| ANN vector query (k=10) | 9.9 ms | 10.2 ms | 10.6 ms | 101 ops/s |
| filter query (eq, limit 10) | 3.0 ms | 3.1 ms | 3.3 ms | 328 ops/s |

Object-store traffic for the whole run: 46,404 gets (14.1 MiB), 10,112
put-if-absent + 15,152 compare-and-sets (18.2 MiB), zero CAS conflicts.

## Reading the numbers

- **A WAL commit costs a fixed number of durable round trips** (stage,
  reserve, publish, advance — each fsynced), so single-document writes are
  commit-bound at ~60 ms while batching 100 docs into one commit amortizes to
  0.68 ms/doc. Batch your writes; the API takes whole operation lists.
- **Point lookups are cache-resident** after the first touch: manifest body
  and SST blocks come from the immutable-object LRU, so the p50 is memory
  speed, not disk.
- **ANN latency is scan-dominated** at this scale (one IVF generation, full
  postings in one object); RaBitQ's packed estimation shows up at larger
  dimensions and corpus sizes — see the `cargo bench --bench distance` kernel
  numbers in `docs/PROGRESS.md` (D50/D51: up to 45× on 768-dim estimation).
- **Zero `cas_mismatches`** is the single-writer happy path; the protocol's
  value is what happens when that stops being true (crash recovery, fenced
  retries), which the test suite covers.
