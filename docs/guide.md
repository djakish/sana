# Sana — User Guide

Sana is an object-storage-native search database: vectors (IVF + RaBitQ),
full-text (BM25), and attribute filters over documents whose only durable home
is an object store — a local directory or S3. One binary gives you a CLI, an
HTTP service with a built-in indexing worker and automatic maintenance, and a
Rust library.

> This is an AI-assisted educational project (a turbopuffer-inspired clone).
> Don't run your production on it.

## Build & quick start

```sh
cargo build --release

# Local directory store
./target/release/sana create  ./data books
./target/release/sana upsert  ./data books 1 title="A Wizard of Earthsea" genre=fantasy rating=4.5
./target/release/sana flush   ./data books          # build indexes now (serve does this for you)
./target/release/sana get     ./data books 1
./target/release/sana query   ./data books '{"filter":{"Eq":{"column":"genre","value":{"String":"fantasy"}}}}'
```

Every CLI verb takes a store location as its first argument: a directory, or
`s3://bucket[/prefix]`.

## S3

```sh
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
export SANA_S3_ENDPOINT=http://127.0.0.1:9000   # omit for AWS; defaults to s3.<region>.amazonaws.com
export AWS_REGION=us-east-1                     # optional, default us-east-1
export SANA_S3_PATH_STYLE=1                     # optional; defaults on for non-AWS endpoints

sana serve s3://my-bucket/sana
```

Conditional writes (`If-None-Match: *`, `If-Match: <etag>`) are enforced by
the store itself, so several nodes can safely share one bucket. The
filesystem backend's CAS is single-process only — fine for dev.

## The service

```sh
sana serve ./data 127.0.0.1:8080 268435456   # store, address, cache bytes
```

One process serves HTTP, runs a durable indexing worker (your writes become
indexed without a second process), reconciles missed index notifications every
30 s, and runs background maintenance every 60 s: compaction once a namespace
accumulates enough SST runs or vector deltas, vector split/merge maintenance,
and garbage collection of superseded objects (two-pass deferred, so in-flight
readers drain first).

### Routes

| Route | Purpose |
|---|---|
| `POST /v2/namespaces/{ns}` | writes (append / conditional / patch- & delete-by-filter) |
| `POST /v2/namespaces/{ns}/query` | single or multi query |
| `GET /v1/namespaces/{ns}/metadata` | index freshness, sizes, pinning |
| `POST /v1/namespaces/{ns}/_debug/recall` | ANN recall vs exact, on sampled vectors |
| `GET /v1/namespaces/{ns}/hint_cache_warm` | prefetch one manifest generation into cache |
| `GET /metrics` | Prometheus text |
| `GET /healthz` | liveness |

Write — append two documents (creates the namespace on first write):

```sh
curl -s localhost:8080/v2/namespaces/books -d '{
  "kind": "append",
  "operations": [
    {"Upsert": {"id": {"U64": 1}, "document": {
      "id": {"U64": 1},
      "attributes": {"title": {"String": "The Dispossessed"}, "rating": {"Float": 4.8}},
      "vectors": {"embedding": {"F32": [0.95, 0.05]}}
    }}}
  ],
  "idempotency_key": "load-1"
}' -H 'content-type: application/json'
```

Query — ANN with a filter:

```sh
curl -s localhost:8080/v2/namespaces/books/query -d '{
  "kind": "single",
  "query": {
    "filter": {"Range": {"column": "rating", "lower": {"Included": {"Float": 4.0}}, "upper": null}},
    "approx_vector": {"column": "embedding", "vector": [1.0, 0.0], "k": 5}
  }
}' -H 'content-type: application/json'
```

A multi-query (`"kind": "multi"`) runs several queries against one consistent
snapshot — the building block for hybrid text+vector retrieval with
client-side fusion. Errors come back as
`{"error": {"code": "...", "message": "..."}}` with stable
400/404/409/429/500 classes; writes past the 2 GiB unindexed-WAL budget get
`429 backpressure`.

## Library

The HTTP service is a thin adapter — everything is callable as a library.
`examples/usage.rs` is the tour: create a namespace, write, `indexer::flush`,
then filtered / exact-kNN / ANN / BM25 queries and a hybrid multi-query.

```sh
cargo run --example usage
```

## Observability

`GET /metrics` exposes object-store traffic and latency (counted below the
cache, so it measures true backend round trips), write/query latency
histograms split by phase, cache hit/miss/byte gauges, per-namespace
unindexed-WAL gauges, and ANN/FTS work counters. See `src/metrics.rs` for the
full series list.

## Benchmark

```sh
cargo run --release --example latency                  # defaults: 5k writes, 64-dim, 1k queries
cargo run --release --example latency -- '' 10000 768 1000
```

Reports p50/p90/p99 for single and batched writes, point lookups, ANN and
filtered queries, plus the true object-store traffic the run generated. See
[benchmarks.md](benchmarks.md) for current numbers on a dev machine.

## More

- `docs/wiki/architecture.md` — the full design document.
- `docs/PROGRESS.md` — staged build log and every design decision (D1–D73).
- `sana --help` (no args) — the complete CLI verb list: branch, copy, export,
  pin, gc, recall, and friends.
