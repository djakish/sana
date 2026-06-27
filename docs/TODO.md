# Sana TODO

This is the prioritized list of issues that should be resolved before Sana is
run as a multi-pod service behind a load balancer.

The intended deployment is:

```text
                         +------------------+
client -> ALB/service -> | API/query pods   | -> shared S3
                         +------------------+
                                  |
                                  v
                         +------------------+
                         | queue broker     | -> jobs/indexing_queue.json
                         +------------------+
                                  |
                                  v
                         +------------------+
                         | indexer pods     | -> shared S3
                         +------------------+

                         +------------------+
                         | maintenance      | -> shared S3
                         +------------------+
```

Keep the global queue JSON. The missing piece is a real broker and safe
coordination around maintenance and object deletion.

## P0: Safe object reclamation

**Status:** automatic deletion is disabled by default. API query and recall
snapshots now publish durable per-process reader leases under `jobs/readers/`.
After publishing the lease, the query re-reads `manifest/current` and retries if
the pointer moved, so the old-reader snapshot is not orphaned between capture
and lease publication.
Apply-mode GC and the legacy opt-in two-pass maintenance GC now re-read
namespace liveness immediately before deleting candidates and include those
active reader snapshots. The path remains unsafe for automatic multi-process
reclamation because Sana still has no publisher safety point or durable GC
candidate set.

Current code:

- [`maintenance.rs`](../src/maintenance.rs#L42-L503) keeps legacy GC behind
  `MaintenancePolicy::gc`; when enabled, it remembers orphan candidates only in
  process memory and asks the indexer GC helper to re-check candidates before
  deletion.
- [`indexer.rs`](../src/indexer.rs#L854-L926) computes liveness from the
  current manifest plus unexpired reader leases. Deletion re-runs that liveness
  calculation immediately before deleting, but still explicitly assumes
  quiescence from unpublished writers.
- [`reader_lease.rs`](../src/reader_lease.rs#L81-L247) stores one CAS-updated
  lease file per API process. Each active query/recall snapshot records the
  manifest body key and WAL overlay range that GC must keep reachable.
- [`query.rs`](../src/query.rs#L336-L370) publishes the reader lease before
  object reads, then verifies that `manifest/current` still names the same
  pointer before it uses the snapshot.
- [`reader_lease.rs`](../src/reader_lease.rs#L365-L390) lists reader leases only
  from the GC path, then loads active manifests and preserves their referenced
  objects.
- Publishers write immutable objects before publishing their manifest:
  [`flush`](../src/indexer.rs#L543-L615),
  [`compact`](../src/indexer.rs#L773-L809), and
  [`maintain_vectors`](../src/indexer.rs#L953-L1131).

### Failure: old reader

1. Query pod A reads manifest generation 10.
2. Compaction publishes generation 11.
3. GC sees generation 10's files as orphaned.
4. The files survive one 60-second interval and are then deleted.
5. Pod A resumes or retries an object read from generation 10.
6. The query fails with `NotFound`.

A fixed delay lowers the probability, but it does not prove that no reader
still holds the old generation.

### Failure: unpublished object

1. A publisher writes immutable object `K`.
2. It crashes or loses the manifest CAS, leaving `K` unreferenced.
3. GC records `K` as a candidate.
4. A retry computes the same content-addressed key and reuses `K`.
5. GC deletes `K` between the retry's existence check and manifest CAS.
6. The retry can publish a manifest that references a missing object.

### Required work

- [x] Disable automatic online GC by default. Keep dry-run reporting available.
- [x] Add a durable reader-generation lease or watermark per query pod.
- [ ] Include active publishers in the reclamation safety calculation.
- [ ] Delete only below a computed safe generation, never only because two
      timer-based scans agreed.
- [ ] Make GC candidates durable or safely reconstructable across leader failover.
- [ ] Add tests that pause publishers across multiple GC passes.
- [x] Add tests that keep an old reader snapshot active across GC and release it.
- [x] Add a final current-manifest/WAL reference check immediately before
      deletion.
- [ ] Add a durable publisher safety-point check immediately before deletion.

A conservative age threshold can be an interim safeguard, but
[`ObjectMeta`](../src/object_store.rs#L56-L61) currently has no creation or
modification time. Age retention is still weaker than a real reader watermark.

Related designs:

- [TiDB GC safe points and protection for active transactions](https://docs.pingcap.com/tidb/stable/garbage-collection-overview/)
- [Delta Lake VACUUM retention and concurrent-reader warning](https://docs.delta.io/delta-utility/#remove-files-no-longer-referenced-by-a-delta-table)
- [Apache Iceberg snapshot expiration](https://iceberg.apache.org/docs/latest/maintenance/#expire-snapshots)

## P0: Separate process roles

**Status:** API-only, queue-broker, indexing-worker, and maintenance roles now
exist. The remaining role work is per-role tuning and ownership policy, not
binary separation.

[`api::serve_with_shutdown`](../src/api.rs#L278-L297) always joins the HTTP
server, index worker, and maintenance loop. Scaling the API Deployment from one
pod to ten therefore also creates ten full-store reconcilers and ten
maintenance loops.

### Gap

1. Kubernetes scales API pods for query load.
2. Every new pod starts polling the global queue.
3. Every new pod lists all namespaces every 30 seconds for reconciliation.
4. Every new pod scans all namespaces every 60 seconds for maintenance and GC.
5. Query scaling unintentionally multiplies background S3 traffic and races.

### Required work

- [x] Add explicit API, indexing, and maintenance roles:

  ```text
  sana serve-api
  sana work-indexing --loop
  sana maintain --loop
  ```

- [x] Add the standalone `sana queue-broker` role tracked in the queue-broker
      section below.
- [x] Keep `sana serve --role all` only as a convenient single-node/dev mode.
- [ ] Give each role separate concurrency, cache, metrics, and shutdown config.
- [x] Make API replicas horizontally scalable without starting background work.
- [x] Add deployment examples with separate Kubernetes Deployments.

Turbopuffer's published architecture similarly shows separate query and indexer
binaries: [turbopuffer architecture](https://turbopuffer.com/docs/architecture).

## P0: Coordinate all maintenance publishers

**Status:** automatic maintenance loops now acquire a store-global object-store
CAS lease before scanning namespaces. Background flush, compaction, and vector
maintenance publishers now re-check their queue claim or maintenance lease
immediately before publishing `manifest/current`. Compaction and vector
maintenance still do not have per-namespace durable jobs, and manual publisher
entry points remain outside the lease.

The unleased maintenance pass ([`maintenance::run_once`](../src/maintenance.rs))
still scans the whole store when called directly; automatic loops now enter
through [`run_once_leased`](../src/maintenance.rs). Manifest CAS prevents two
stale manifests from both becoming current. The new publish fence additionally
prevents stale background owners from making already-written immutable work
reachable, but it does not prevent duplicate expensive work, conflicting
maintenance decisions, or unsafe interaction with GC unless callers use the
leader lease.

The queue lease also does not cover manual `flush`, `compact`, or
`maintain-vectors`. The maintenance leader lease only gates the automatic
all-namespace maintenance loop.

### Required work

- [x] Initially run one maintenance leader using a Kubernetes
      `coordination.k8s.io/Lease`, or an object-store CAS lease if Sana should
      remain independent of Kubernetes.
- [ ] Add per-namespace ownership for compaction and vector maintenance.
- [ ] Prefer durable maintenance jobs with job kind, namespace, target
      generation, attempts, lease, and fencing token.
- [ ] Make every publishing entry point use the same ownership mechanism,
      including CLI operations.
- [x] Re-check the source generation and ownership immediately before manifest
      publication for automatic queue and maintenance publishers.
- [ ] Test leader death, lease expiry, duplicate execution, and stale workers.

Kubernetes uses Lease objects for leader election in its own HA components:
[Kubernetes Leases](https://kubernetes.io/docs/concepts/architecture/leases/).

## P0: Finish the global queue broker

**Status:** the published turbopuffer queue shape is implemented and measured:
one JSON queue object, brokered group commit, durable acknowledgement, broker
address discovery through that object, generation fencing for overlapping
brokers, worker heartbeats, and queue-owner metrics for contention and queue
age. Kubernetes supplies replacement broker processes when liveness fails.

Sana already has:

- One global CAS-updated queue object:
  [`IndexQueue`](../src/index_queue.rs).
- Leased claims, attempts, heartbeats, retries, and at-least-once work:
  [`run_worker_once_with_client`](../src/index_queue.rs).
- An in-process group-commit broker:
  [`IndexQueueBroker`](../src/index_queue.rs).
- A transport-neutral mutation boundary implemented by both:
  [`QueueClient`](../src/index_queue.rs).
- A standalone HTTP broker and object-store-discovered client:
  [`queue_broker.rs`](../src/queue_broker.rs).

### Why direct CAS stops scaling

1. Every API and indexer pod reads the same queue object.
2. Each pod modifies its private copy.
3. Only one CAS succeeds.
4. Losers download the new queue and retry.
5. Heartbeats, claims, completions, and enqueues contend on the same object.
6. More pods produce more retries rather than more queue throughput.

### Required work

- [x] Introduce a `QueueClient` boundary used by all enqueue, claim, heartbeat,
      complete, fail, and reconcile operations.
- [x] Implement a standalone broker service that owns the group-commit loop.
- [x] Route all normal multi-pod queue mutations through the broker.
- [x] Acknowledge a broker request only after its batched CAS is durable.
- [x] Store broker discovery and a monotonically increasing owner generation in
      `queue.json`.
- [x] Permit brief overlapping brokers; CAS remains the final correctness guard.
- [x] Keep direct object-store CAS as a dev/recovery backend, not the normal
      multi-pod path.
- [x] Fail broker liveness after a bounded group-commit timeout so the process
      supervisor starts a replacement.
- [x] Measure queue size, CAS rate, batch size, retries, claim latency, and
      oldest-job age before considering sharding.

Do **not** shard the queue preemptively. Turbopuffer reports that a single JSON
queue with a stateless, HA, brokered group-commit loop handles its indexing
traffic:

- [How to build a distributed queue in a single JSON file on object storage](https://turbopuffer.com/blog/object-storage-queue)
- [Amazon S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)

## P0: Kubernetes lifecycle and health

**Status:** HTTP roles now expose Kubernetes-style liveness/readiness probes
and enter drain on Ctrl-C or SIGTERM. Workers finish the current tick/job before
observing shutdown and stopping the next claim/pass.

- [`shutdown_signal`](../src/main.rs#L431-L438) handles Ctrl-C and SIGTERM.
- [`/livez`](../src/api.rs#L368-L370) reports process liveness; `/healthz`
  remains a liveness alias.
- [`/readyz`](../src/api.rs#L368-L370) checks readiness state, overload, and a
  bounded backend list.

### Failure

1. Kubernetes sends SIGTERM during rollout or eviction.
2. Sana does not observe it.
3. The process remains alive until Kubernetes sends SIGKILL.
4. HTTP requests and background jobs are interrupted without controlled drain.

### Required work

- [x] Handle both Ctrl-C and SIGTERM.
- [x] Add `/livez` for process health and `/readyz` for traffic readiness.
- [x] Do not make liveness fail only because S3 is temporarily unavailable.
- [x] Make readiness fail during startup, backend failure, overload, and drain.
- [x] On shutdown, become unready first and stop accepting new work.
- [x] Drain HTTP requests and stop claiming jobs.
- [x] Finish, fail, or release the current job before the termination deadline.
- [x] Set Kubernetes `terminationGracePeriodSeconds` from the maximum drain time.

Sources:

- [Kubernetes pod termination and SIGTERM](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/#pod-termination)
- [Kubernetes liveness, readiness, and startup probes](https://kubernetes.io/docs/concepts/workloads/pods/probes/)

## P1: Use renewable AWS workload credentials

**Status:** the S3 backend loads credentials once from environment variables.

[`S3ObjectStore::from_env`](../src/object_store/s3.rs#L100-L108) requires
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. It does not use the AWS default
credential chain or refresh credentials supplied through EKS Pod Identity.

### Required work

- [ ] Add an asynchronous, renewable credential-provider abstraction.
- [ ] Prefer the AWS SDK default credential chain for AWS S3.
- [ ] Retain custom endpoint/path-style support for MinIO.
- [ ] Use separate least-privilege service accounts for API, broker, indexer,
      maintenance, and GC roles.
- [ ] Test credential refresh without restarting the process.

Source: [Amazon EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html).

## P1: Define the HTTP trust boundary and deadlines

**Status:** the HTTP service has no authentication or authorization middleware,
and no whole-request deadline.

[`router_with_metrics`](../src/api.rs#L251-L271) exposes writes, queries,
metadata, cache warming, recall debugging, and metrics through one router. A
public ALB would therefore expose administrative and data operations unless
another layer protects them.

### Required work

- [ ] Make the default deployment private until authentication is configured.
- [ ] Terminate TLS at the ALB or ingress and restrict pods to ALB/ingress traffic.
- [ ] Authenticate requests at the ALB, gateway, or Sana.
- [ ] Enforce namespace-level authorization inside a trusted component.
- [ ] Keep `/metrics` and debug recall routes on an internal listener.
- [ ] Add request deadlines and propagate cancellation into query and S3 work.
- [ ] Add per-tenant rate limits in addition to the current per-process query
      semaphore.

Possible building blocks:

- [ALB OIDC/Cognito authentication](https://docs.aws.amazon.com/elasticloadbalancing/latest/application/listener-authenticate-users.html)
- [Kubernetes NetworkPolicy](https://kubernetes.io/docs/concepts/services-networking/network-policies/)

## P1: Enforce a real query result bound

**Status:** `MAX_QUERY_RESULTS` is now the effective default when `limit` is
omitted. Query aggregates are computed over all matches before the returned row
page is truncated.

[`execute_with_snapshot`](../src/query.rs#L330-L419) rejects
`limit > 10_000`, but `limit: null` returns every matching document.
`Query::all()` uses `limit: None`.

### Failure

1. A client sends an unfiltered query without `limit`.
2. Sana materializes and sorts the full namespace.
3. The HTTP response contains every document.
4. A large namespace can exhaust pod memory or exceed practical response size.

### Required work

- [x] Define an effective default limit, initially `MAX_QUERY_RESULTS`.
- [x] Apply it to every query path, including ordinary, text, vector, and
      multi-query responses.
- [x] Keep aggregate semantics explicit: aggregate over all matches or only the
      returned page.
- [ ] Add cursor-based pagination before advertising full scans over HTTP.
- [x] Test omitted limits and multi-query worst cases.

## P1: Do not silently lose integer precision

**Status:** JSON integers above `i64::MAX` are rejected instead of being
silently converted to `f64`.

[`ValueVisitor::visit_u64`](../src/value.rs#L270-L279) converts an out-of-range
`u64` with `v as f64`. Many such integers cannot be represented exactly.

### Failure

1. Client sends an integer attribute larger than `i64::MAX`.
2. Deserialization changes its type from integer to float.
3. Values above the exact binary64 range can change numerically.
4. Equality filters, ordering, serialization, and stored data use the changed
   value without reporting an error.

### Required work

- [x] Choose one explicit contract:
      add `Value::UInt(u64)`, reject values above `i64::MAX`, or require a
      string representation.
- [x] Never silently convert an integer to a lossy float.
- [x] Add boundary tests around `2^53`, `i64::MAX`, and `u64::MAX`.

RFC 8259 identifies `[-(2^53)+1, (2^53)-1]` as the interoperable exact integer
range for common JSON implementations:
[RFC 8259, Numbers](https://www.rfc-editor.org/rfc/rfc8259#section-6).

## P1: Make F16 JSON writes schema-aware

**Status:** F16 vectors serialize as plain floats; JSON writes now treat that
float array as a wire vector and coerce it to the existing column's schema
before WAL publication.

- F16 JSON serialization emits a float array:
  [`value.rs`](../src/value.rs#L343-L364).
- Every human-readable vector is deserialized as `VectorValue::F32`:
  [`value.rs`](../src/value.rs#L382-L389).
- Existing F16 columns reject the inferred F32 type:
  [`schema.rs`](../src/schema.rs#L159-L169).

### Failure

1. Sana returns an F16 document through JSON as `[1.0, ...]`.
2. A client sends that document back unchanged.
3. Sana parses the vector as F32.
4. The F16 schema rejects the write.

### Required work

- [x] Parse HTTP vectors into a neutral wire representation such as `Vec<f32>`.
- [x] Convert to F16 or F32 using the existing column schema before validation.
- [x] Infer F32 only when creating a new vector column without an explicit schema.
- [x] Add HTTP round-trip tests for both F16 and F32 columns.

## P2: Connect pinning to request routing

**Status:** pinning and warm readiness exist, but the HTTP path does not use
them.

[`PinningController::route`](../src/pinning.rs#L381-L422) and
[`warm_replica`](../src/pinning.rs#L424-L468) are only library/CLI facilities.
The API router never claims a replica slot or routes a namespace to a ready pod.

With ordinary ALB distribution, repeated queries for one namespace can land on
different pods and repeatedly warm separate caches.

### Required work

- [ ] Decide whether routing lives in an application gateway or query pods.
- [ ] Route the same namespace/request key to ready replicas using rendezvous or
      consistent hashing.
- [ ] Fall back to any query pod because S3 remains authoritative.
- [ ] Report cache-hit and cold-query latency by node and namespace.
- [ ] Treat pinning as a latency optimization, not a correctness requirement.

Turbopuffer describes routing subsequent queries to the same query node for
cache locality: [turbopuffer architecture](https://turbopuffer.com/docs/architecture).

## P2: Correct the RRF example

[`examples/hybrid.rs`](../examples/hybrid.rs#L83-L91) uses zero-based
`enumerate()` directly in `1 / (k + rank)`. The published RRF formula defines
rankings as permutations over `1..|D|`, so the first result should contribute
`1 / (k + 1)`, not `1 / k`.

- [x] Use `1.0 / (k + rank as f64 + 1.0)`.
- [x] Add a small deterministic unit test.

Source: [Cormack, Clarke, and Buettcher, Reciprocal Rank Fusion](https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf).

## P1: Add a guarded namespace drop lifecycle

**Status:** namespaces can be created, branched, copied, exported, written, and
queried, but there is no `sana drop` or library equivalent.

Current code:

- [`Namespace::create`](../src/namespace.rs) and [`Namespace::open`](../src/namespace.rs)
  define the entry path.
- [`operations::branch_namespace`](../src/operations.rs) creates child manifests
  that may reference immutable objects owned by the source namespace.
- [`indexer::foreign_references_into_namespace`](../src/indexer.rs) already
  scans branch references so GC does not delete parent objects still used by a
  child branch.

### Gap

1. A namespace can be created accidentally or become obsolete.
2. Operators can delete rows, but not the namespace's manifest pointer, WAL,
   idempotency records, indexes, pinning state, and queue jobs as one lifecycle
   operation.
3. Deleting the prefix manually can break branches that still reference parent
   objects.

### Required work

- [ ] Add `sana drop <store> <namespace>` and a library/API entry point.
- [ ] Refuse by default while any other manifest references the namespace's
      immutable objects.
- [ ] Decide whether `--force` only tombstones the namespace or also schedules
      object deletion through the safe GC protocol.
- [ ] Remove or fence queue, reader-lease, pinning, and maintenance state for
      the namespace.
- [ ] Add tests for branch-held objects, concurrent readers/writers, and retry
      after partial drop progress.

## P1: Retry transient S3 failures safely

**Status:** done. Every S3 verb now routes its presigned request through
`send_retrying`, which retries transient transport errors and retryable 5xx
responses (`500`/`502`/`503` `SlowDown`/`504`) with bounded exponential backoff
and full jitter, on top of the existing 409 conditional-conflict retries.
`404`, precondition failures, and corruption errors stay non-retryable. The two
conditional verbs reconcile an ambiguous success — a write whose 2xx was lost
behind a transient failure — by re-reading the current bytes after any retry,
so an already-committed write is reported as success instead of a spurious
`AlreadyExists`/`CasMismatch`.

Current code:

- [`S3ObjectStore::send_retrying`](../src/object_store/s3.rs) owns the transient
  retry loop, re-signing the URL per attempt and reporting whether a retry
  occurred. [`is_retryable_status`](../src/object_store/s3.rs) and
  [`backoff_delay`](../src/object_store/s3.rs) are pure and unit-tested.
- [`S3ObjectStore::reconcile_conditional`](../src/object_store/s3.rs) re-reads
  the key after an ambiguous conditional write and decides the outcome by byte
  equality (S3 ETags are not Sana content versions).
- The WAL, manifest, and immutable-object protocols tolerate ambiguous success,
  so re-`put`ting identical immutable bytes or re-running a CAS is safe.

### Gap

1. S3 returns `503 SlowDown`, a transient `500`, or a temporary transport error.
2. Sana surfaces that as a hard database operation failure.
3. The object-store protocol remains safe, but operators see avoidable write,
   query, indexing, or maintenance failures.

### Required work

- [x] Add bounded exponential backoff with jitter for retryable transport
      errors and S3 `500`, `502`, `503`, and `504` responses.
- [x] Keep `404`, failed preconditions, and deterministic corruption errors
      non-retryable.
- [x] Treat conditional `PUT` and CAS retries as ambiguous-success safe:
      verify existing bytes or re-read current object state where needed.
- [x] Add tests with an injected S3/HTTP failure sequence for every object-store
      verb (mock-HTTP per-verb retry tests plus pure backoff/classification
      unit tests; MinIO conformance still covers the real-backend contract).

## P1: Add randomized object-store adversary tests

**Status:** crash-window and race tests exist for many known protocols, but
there is no seeded adversarial object-store test that searches for unknown
interleavings.

Current code:

- Tests use small custom `ObjectStore` decorators for specific scenarios, such
  as ambiguous WAL commits and GC publish races.
- There is no dev dependency such as `proptest`, no reusable fault-injecting
  store, and no fuzz target for WAL/SST/frame decoders.

### Required work

- [ ] Add a seeded `FaultingObjectStore` test decorator that can delay, fail,
      or report ambiguous success on configured operations.
- [ ] Run random write/flush/compact/query episodes and check invariants after
      every step: committed cursor never regresses, accepted writes are readable,
      manifests parse, and referenced immutable objects exist.
- [ ] Add property tests that generate documents and filter expressions, then
      assert indexed query results equal a full-scan reference across flush,
      tiering, and compaction.
- [ ] Add codec fuzzing or property tests for frame, WAL, manifest, and SST
      decoders.

## P2: Reduce full-compaction write spikes

**Status:** minor tiering exists for document and attribute SSTs, but the
operator `compact` path still rewrites a whole namespace and is the only path
that drops tombstones, rebuilds stale-free attribute/text snapshots, and resets
the vector append chain.

Current code:

- [`tier_doc_ssts`](../src/indexer.rs) and [`tier_attr_ssts`](../src/indexer.rs)
  merge runs within one level.
- [`compact`](../src/indexer.rs) merges all runs, drops tombstones, rebuilds
  attribute/text indexes, rebuilds the vector base, and clears appends.
- [`MaintenancePolicy::default`](../src/maintenance.rs) triggers full compaction
  at eight doc/attr runs or four vector appends.

### Required work

- [ ] Add a compaction plan that bounds bytes rewritten per maintenance pass.
- [ ] Separate tombstone cleanup, stale attribute cleanup, text rebuild, and
      vector-base reset so they do not always require one full rewrite.
- [ ] Make compaction thresholds byte-aware, not only run-count based.
- [ ] Expose compaction bytes in metrics before changing automatic policy.

## P2: Surface background work in metrics and logs

**Status:** mostly done. Maintenance-pass and indexing-worker outcomes are now
first-class `Metrics` counters exported on `/metrics`; failures still also print
to stderr. The only deferred item is structured `tracing`.

Current code:

- [`MaintenanceMetrics`](../src/metrics.rs) and [`WorkerMetrics`](../src/metrics.rs)
  hold the new counters; both render through `to_prometheus`.
- [`api::run_maintenance_loop`](../src/api.rs) folds each `MaintenanceReport`
  into `MaintenancePassSample` and records skipped/failed passes; the serve
  indexing worker classifies each `run_worker_once_with_client` outcome into
  claims, flushes, failures, and stale-claim rejections.
- [`MaintenanceReport`](../src/maintenance.rs) still carries the per-namespace
  detail; the loop also keeps `eprintln!` for human-readable failure context.

### Required work

- [x] Add counters for maintenance passes, skipped leased passes, compactions,
      vector-maintenance publications, GC candidates, GC deletions, and
      maintenance errors.
- [x] Add worker counters for claims, successful flushes, retries/failures, and
      stale-claim publication rejections.
- [ ] Decide whether to add feature-gated `tracing` for structured logs while
      preserving the low-dependency default. (Deferred: kept the dependency-free
      counter registry; failures still go to stderr. Revisit if operators need
      structured per-event logs.)
- [x] Document which metrics operators should alert on.

## P2: Improve public library ergonomics

**Status:** done. `From` conversions, chainable `Document` builders,
`FilterExpr`/`RangeBound` constructors, crate-root re-exports, and the
`Namespace::flush()`/`scan()` aliases now let an embedder write documents and
filters and run the common lifecycle calls without naming low-level enum
variants or reaching into the `indexer` module; `examples/usage.rs` is rewritten
to use them.

Current code:

- [`Document::new`](../src/value.rs) takes `impl Into<Id>` and has chainable
  `attr`/`vector` builders; `From` impls cover `Id`, `Value`, and
  `VectorValue` for the common scalar/vector inputs.
- [`FilterExpr`](../src/query.rs) has `eq`/`range`/`gte`/`gt`/`lte`/`lt`/
  `and`/`or`/`not` constructors and `RangeBound::included`/`excluded`.
- The crate root re-exports `Namespace`, `Document`, `Id`, `Value`,
  `VectorValue`, `Query`, `FilterExpr`, `RangeBound`, `ObjectStore`,
  `FsObjectStore`.
- [`Namespace::scan`](../src/namespace.rs) aliases `replay`, and
  [`Namespace::flush`](../src/namespace.rs) delegates to `indexer::flush`;
  both original entry points stay public for compatibility.

### Required work

- [x] Add `From` implementations for common `Id`, `Value`, and `VectorValue`
      inputs without changing persisted binary formats.
- [x] Add chainable document builders for attributes and vectors.
- [x] Add filter/query helper constructors for the common cookbook shapes.
- [x] Consider `Namespace::flush()` and `Namespace::scan()` aliases while
      keeping existing APIs for compatibility.
- [x] Re-export the public types needed by most examples from the crate root.

## Suggested order

1. Disable automatic GC.
2. Fix unbounded results, integer precision, and F16 round trips.
3. Split API, indexer, maintenance, and broker roles.
4. Route every queue mutation through the standalone broker.
5. Add maintenance leader election and per-namespace ownership.
6. Implement publisher safety points and durable GC candidates, then re-enable
   automatic GC.
7. Add Kubernetes lifecycle, probes, renewable AWS credentials, and the HTTP
   trust boundary.
8. Add cache-aware routing after the basic multi-pod deployment is correct.
9. Work through the Fable 5 follow-ups above: namespace drop, S3 retries,
   adversarial tests, compaction planning, observability, and library
   ergonomics.
