# Sana

An object-storage-native search database: vectors, full-text, and attribute
filters over documents whose only durable home is an object store (a local
directory or S3). A [turbopuffer](https://turbopuffer.com)-inspired open-source
clone, built in staged, documented commits.

> **AI disclaimer:** this is an AI-assisted project (built with Claude and Codex).
> It exists to learn from, not to run your production on.

**What works:**

- Durable writes: a WAL with a CAS-advanced commit cursor in object storage,
  strongly consistent reads through the unindexed overlay, idempotency keys,
  conditional writes, patch/delete-by-filter, and write backpressure.
- Indexes: LSM document SSTs, delta-tiered attribute postings, BM25 full-text
  with rank-safe block MAXSCORE, IVF vectors with faithful RaBitQ quantization
  (SIMD kernels) and SPFresh-style local split/merge maintenance.
- One binary: a CLI, API, object-store-discovered queue broker, looped
  indexing/maintenance roles, all-in-one dev serving, operator GC dry-runs, and
  Prometheus `/metrics` endpoints for the API and queue broker.
- Backends: local filesystem for dev, plus S3-compatible stores with
  server-enforced conditional writes (verified against MinIO).
- Operations: namespace branch, cross-store copy, deterministic export, leased
  replica pinning, cache warming, and an ANN recall endpoint.

## Quick start

```sh
cargo run --release -- demo ./data        # tiny end-to-end demo
cargo run --release -- serve ./data       # HTTP service on 127.0.0.1:8080
cargo run --release --example usage       # library API tour
cargo run --release --example latency     # benchmark harness
```

## HTTP API

The wire format tracks turbopuffer's `/v2` API. With `serve` running (see Quick
start), write a document (the namespace is created on first write):

```sh
curl -s localhost:8080/v2/namespaces/books -H 'content-type: application/json' -d '{
  "kind": "append",
  "operations": [
    {"Upsert": {"id": 1, "document": {
      "id": 1,
      "attributes": {"title": "Dune", "genre": "scifi", "rating": 4.5},
      "vectors": {"embedding": [0.9, 0.1]}
    }}}
  ]
}'
```

Query it. Here an approximate vector search narrowed by an attribute filter:

```sh
curl -s localhost:8080/v2/namespaces/books/query -H 'content-type: application/json' -d '{
  "kind": "single",
  "query": {
    "filter": {"Eq": {"column": "genre", "value": "scifi"}},
    "approx_vector": {"column": "embedding", "vector": [1.0, 0.0], "k": 5}
  }
}'
```

Hybrid search runs several rankings over one consistent snapshot with a
multi-query, then fuses them client-side (for example with Reciprocal Rank
Fusion). Here a vector ranking and a BM25 ranking together:

```sh
curl -s localhost:8080/v2/namespaces/books/query -H 'content-type: application/json' -d '{
  "kind": "multi",
  "query": {"queries": [
    {"approx_vector": {"column": "embedding", "vector": [1.0, 0.0], "k": 10}},
    {"text": {"column": "title", "query": "dune", "k": 10}}
  ]}
}'
```

The response holds one ranked result per subquery. The `hybrid` example
(`cargo run --example hybrid`) shows the RRF fusion in full.

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/v2/namespaces/{ns}` | writes: append / conditional / patch- & delete-by-filter |
| `POST` | `/v2/namespaces/{ns}/query` | single or multi query (vector / BM25 / filter) |
| `GET` | `/v1/namespaces/{ns}/metadata` | index freshness, sizes, pinning |
| `POST` | `/v1/namespaces/{ns}/_debug/recall` | ANN recall vs exact |
| `GET` | `/metrics` | Prometheus text |

The full write/query cookbook, limits, multi-pod deployment, and the Rust
library API live in the [user guide](docs/guide.md).

## Docs

[User guide](docs/guide.md) ·
[Architecture](docs/ARCHITECTURE.md) ·
[Build log & decisions](docs/PROGRESS.md) ·
[Contributing](CONTRIBUTING.md) ·
[Benchmarks](docs/benchmarks.md)

## License

MIT, see [LICENSE](LICENSE).
