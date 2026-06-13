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
./target/release/sana query   ./data books '{"filter":{"Eq":{"column":"genre","value":"fantasy"}}}'
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

### Local MinIO

`docker-compose.yml` brings up MinIO and creates the buckets, so the S3 path is
copy-paste:

```sh
docker compose up -d                                 # MinIO on :9000, console :9001
export AWS_ACCESS_KEY_ID=sana AWS_SECRET_ACCESS_KEY=sana-secret
export SANA_S3_ENDPOINT=http://127.0.0.1:9000 SANA_S3_PATH_STYLE=1

cargo run --release -- serve s3://sana-dev/books     # or any CLI verb over s3://
SANA_S3_TEST_ENDPOINT=$SANA_S3_ENDPOINT cargo test --test s3_object_store
```

The conformance suite is a no-op unless `SANA_S3_TEST_ENDPOINT` is set, and it
creates its own bucket. The same MinIO backs the S3 row in
[benchmarks.md](benchmarks.md).

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

Attribute values, vectors, and ids are plain JSON — the type comes from the
JSON token and the schema, not a wrapper. (The `Upsert`/`Patch`/`Delete` tag on
an operation is the structural discriminator and stays.)

```sh
curl -s localhost:8080/v2/namespaces/books -d '{
  "kind": "append",
  "operations": [
    {"Upsert": {"id": 1, "document": {
      "id": 1,
      "attributes": {"title": "The Dispossessed", "rating": 4.8},
      "vectors": {"embedding": [0.95, 0.05]}
    }}}
  ],
  "idempotency_key": "load-1"
}' -H 'content-type: application/json'
```

Query bodies POST to `/query` and are tagged `single` or `multi`. Every write
and query shape is in the [Cookbook](#cookbook) below. Errors come back as
`{"error": {"code": "...", "message": "..."}}` with stable 400/404/409/429/500
classes — see [Limits](#limits).

## Cookbook

Every body below POSTs to the route in its heading — same `curl … -d '<body>'`
shape as the append example above. Scalars, ids, and vectors are plain JSON; the
`Upsert` / `Eq` / `Sum` / … tags are structural discriminators.

### Writes — `POST /v2/namespaces/{ns}`

Append (upsert), with an idempotency key — a repeat returns the original cursor:

```json
{"kind":"append","operations":[
  {"Upsert":{"id":1,"document":{"id":1,
    "attributes":{"title":"Dune","genre":"scifi","rating":4.5,"year":1965},
    "vectors":{"embedding":[0.9,0.1]}}}}],
 "idempotency_key":"load-1"}
```

Patch (a `null` attribute clears the field) and delete in one atomic batch:

```json
{"kind":"append","operations":[
  {"Patch":{"id":1,"attributes":{"rating":4.8,"subtitle":null}}},
  {"Delete":{"id":2}}]}
```

Conditional write — apply each op only if its per-id condition holds:

```json
{"kind":"conditional","writes":[
  {"operation":{"Upsert":{"id":1,"document":{"id":1,"attributes":{"rating":5.0}}}},
   "condition":{"Eq":{"column":"rating","value":4.8}}}]}
```

Patch by filter — set fields on every matching row (defaults to ≤ 50k rows):

```json
{"kind":"patch_by_filter","request":{
  "filter":{"Eq":{"column":"genre","value":"scifi"}},
  "attributes":{"featured":true}}}
```

Delete by filter (defaults to ≤ 5M rows):

```json
{"kind":"delete_by_filter","request":{
  "filter":{"Range":{"column":"rating","upper":{"Excluded":3.0}}}}}
```

An append returns just the commit cursor; conditional / patch / delete also
return an outcome:

```json
{"cursor":{"epoch":0,"seq":7},
 "outcome":{"rows_affected":1,"rows_upserted":1,"rows_patched":0,
            "rows_deleted":0,"applied_ids":[1]}}
```

### Queries — `POST /v2/namespaces/{ns}/query`

Equality filter:

```json
{"kind":"single","query":{"filter":{"Eq":{"column":"genre","value":"scifi"}}}}
```

Range — either bound optional, `Included` or `Excluded`:

```json
{"kind":"single","query":{"filter":{"Range":{"column":"rating",
  "lower":{"Included":4.0},"upper":{"Excluded":5.0}}}}}
```

Boolean combinators — `And` / `Or` / `Not`:

```json
{"kind":"single","query":{"filter":{"And":[
  {"Eq":{"column":"genre","value":"scifi"}},
  {"Not":{"Eq":{"column":"year","value":1965}}}]}}}
```

Order by an attribute (or `"Id"`) and limit:

```json
{"kind":"single","query":{
  "order_by":{"target":{"Attribute":"rating"},"direction":"Desc"},"limit":10}}
```

Aggregates — `Count` and `Sum`:

```json
{"kind":"single","query":{"filter":{"Eq":{"column":"genre","value":"scifi"}},
  "aggregates":["Count",{"Sum":{"column":"rating"}}]}}
```

Exact (brute-force) kNN:

```json
{"kind":"single","query":{"exact_vector":{"column":"embedding","vector":[1.0,0.0],"k":5}}}
```

Approximate kNN with a filter, explicit probes, and an L2 override:

```json
{"kind":"single","query":{
  "filter":{"Range":{"column":"rating","lower":{"Included":4.0}}},
  "approx_vector":{"column":"embedding","vector":[1.0,0.0],"k":5,"probes":8,"metric":"L2"}}}
```

Full-text BM25:

```json
{"kind":"single","query":{"text":{"column":"title","query":"dune","k":10}}}
```

Multi-query — several rankings over one consistent snapshot, the building block
for hybrid retrieval (fuse client-side):

```json
{"kind":"multi","query":{"queries":[
  {"approx_vector":{"column":"embedding","vector":[1.0,0.0],"k":10}},
  {"text":{"column":"title","query":"dune","k":10}}]}}
```

A single response wraps rows — each with the document and, for a ranked query, a
`score` (higher is better) — plus any aggregates:

```json
{"kind":"single","result":{
  "rows":[{"id":1,
    "document":{"id":1,"vectors":{"embedding":[0.9,0.1]},
                "attributes":{"rating":4.5,"title":"Dune"}},
    "score":-0.0123}],
  "aggregates":[{"Count":2},
                {"Sum":{"column":"rating","value_count":2,"total":9.3}}]}}
```

A multi response is `{"kind":"multi","result":{"results":[<single result>, …]}}`.

### Metadata — `GET /v1/namespaces/{ns}/metadata`

```json
{"namespace":"books",
 "schema":{"columns":{
   "rating":{"column_type":{"Scalar":"Float"},"filterable":true,"indexed":true},
   "title":{"column_type":"FullText","filterable":false,"indexed":true}},
   "version":2},
 "approx_logical_bytes":81920,"approx_row_count":1000,
 "created_at_ms":1700000000000,"updated_at_ms":1700000500000,
 "index":{"status":"updating","unindexed_bytes":4096,
          "committed_cursor":{"epoch":0,"seq":12},
          "indexed_cursor":{"epoch":0,"seq":9}}}
```

`status` flips to `up-to-date` once `indexed_cursor` reaches `committed_cursor`.

## Limits

| Limit | Default | When exceeded |
|---|---|---|
| Unindexed WAL per namespace | 2 GiB (per write via `options.max_unindexed_wal_bytes`) | `429 backpressure` |
| HTTP request body | 64 MiB | `413` |
| Query result `limit` | ≤ 10,000 | `400 invalid_request` |
| Queries per multi-query | 16 | `400 invalid_request` |
| Full-text query | 1,024 bytes | `400 invalid_request` |
| Patch-by-filter match | 50,000 rows (`max_rows`) | strict: applies nothing; `allow_partial`: applies first N, sets `rows_remaining` |
| Delete-by-filter match | 5,000,000 rows (`max_rows`) | same as patch |
| Concurrent queries / namespace | 16 slots | `429 query_concurrency` |
| Idempotency key | 1–256 bytes | `400 invalid_request` |
| String id | 64 bytes | `400 invalid_request` |
| Columns / vector columns / vector dims | 1,024 / 2 / 10,752 | `400 invalid_request` |

The backing constants live in `query.rs`, `write.rs`, `api.rs`,
`backpressure.rs`, `namespace.rs`, and `schema.rs`.

## Library

The HTTP service is a thin adapter — everything is callable as a library.
Runnable examples:

- `usage` — the end-to-end tour: write → `indexer::flush` → filtered / exact-kNN
  / ANN / BM25 queries and a hybrid multi-query.
- `hybrid` — a vector ranking and a BM25 ranking over one snapshot, fused
  client-side with Reciprocal Rank Fusion (RRF).
- `conditional` — compare-and-set writes and idempotent retries.
- `latency` — the benchmark harness; takes a directory or an `s3://…` location.

```sh
cargo run --example hybrid
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

- `docs/ARCHITECTURE.md` — how the engine works today: the object-store
  boundary, on-disk layout, write/read paths, and the core invariants.
- `docs/PROGRESS.md` — staged build log and every design decision (D1–D74).
- `sana --help` (no args) — the complete CLI verb list: branch, copy, export,
  pin, gc, recall, and friends.
