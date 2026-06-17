# Sana

An object-storage-native search database: vectors, full-text, and attribute
filters over documents whose only durable home is an object store — a local
directory or S3. A [turbopuffer](https://turbopuffer.com)-inspired open-source
clone, built in staged, documented commits.

> **AI disclaimer:** this is an AI-assisted project (built with Claude and Codex).
> It exists to learn from, not to run your production on.

**What works:**

- Durable writes — WAL with a CAS-advanced commit cursor in object storage;
  strongly consistent reads through the unindexed overlay; idempotency keys,
  conditional writes, patch/delete-by-filter, write backpressure.
- Indexes — LSM document SSTs, delta-tiered attribute postings, BM25 full-text
  with rank-safe block MAXSCORE, IVF vectors with faithful RaBitQ quantization
  (SIMD kernels) and SPFresh-style local split/merge maintenance.
- One binary — a CLI, API-only serving, looped indexing/maintenance roles,
  all-in-one dev serving, operator GC dry-runs, and a Prometheus `/metrics`
  endpoint.
- Backends — local filesystem for dev; S3-compatible stores with
  server-enforced conditional writes (verified against MinIO).
- Operations — namespace branch, cross-store copy, deterministic export,
  leased replica pinning, cache warming, ANN recall endpoint.

## Quick start

```sh
cargo run --release -- demo ./data        # tiny end-to-end demo
cargo run --release -- serve ./data       # HTTP service on 127.0.0.1:8080
cargo run --release --example usage       # library API tour
cargo run --release --example latency     # benchmark harness
```

[User guide](docs/guide.md) ·
[Architecture](docs/ARCHITECTURE.md) ·
[Build log & decisions](docs/PROGRESS.md) ·
[Benchmarks](docs/benchmarks.md)

## License

MIT — see [LICENSE](LICENSE).
