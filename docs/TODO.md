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

**Status:** automatic GC is not safe for a multi-process deployment.

Current code:

- [`maintenance.rs`](../src/maintenance.rs#L65-L151) remembers orphan
  candidates only in process memory and deletes an object after two scans.
- [`indexer.rs`](../src/indexer.rs#L822-L913) computes liveness from only the
  current manifest and explicitly assumes quiescence.
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

- [ ] Disable automatic online GC by default. Keep dry-run reporting available.
- [ ] Add a durable reader-generation lease or watermark per query pod.
- [ ] Include active publishers in the reclamation safety calculation.
- [ ] Delete only below a computed safe generation, never only because two
      timer-based scans agreed.
- [ ] Make GC candidates durable or safely reconstructable across leader failover.
- [ ] Add tests that pause readers and publishers across multiple GC passes.
- [ ] Add a final reference and safety-point check immediately before deletion.

A conservative age threshold can be an interim safeguard, but
[`ObjectMeta`](../src/object_store.rs#L56-L61) currently has no creation or
modification time. Age retention is still weaker than a real reader watermark.

Related designs:

- [TiDB GC safe points and protection for active transactions](https://docs.pingcap.com/tidb/stable/garbage-collection-overview/)
- [Delta Lake VACUUM retention and concurrent-reader warning](https://docs.delta.io/delta-utility/#remove-files-no-longer-referenced-by-a-delta-table)
- [Apache Iceberg snapshot expiration](https://iceberg.apache.org/docs/latest/maintenance/#expire-snapshots)

## P0: Separate process roles

**Status:** every API pod also becomes an indexer and maintenance worker.

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

- [ ] Add explicit roles, for example:

  ```text
  sana serve-api
  sana queue-broker
  sana work-indexing --loop
  sana maintain --loop
  ```

- [ ] Keep `sana serve --role all` only as a convenient single-node/dev mode.
- [ ] Give each role separate concurrency, cache, metrics, and shutdown config.
- [ ] Make API replicas horizontally scalable without starting background work.
- [ ] Add deployment examples with separate Kubernetes Deployments.

Turbopuffer's published architecture similarly shows separate query and indexer
binaries: [turbopuffer architecture](https://turbopuffer.com/docs/architecture).

## P0: Coordinate all maintenance publishers

**Status:** indexing jobs are leased, but compaction and vector maintenance are
not distributed jobs.

[`maintenance::run_once`](../src/maintenance.rs#L65-L151) independently scans
the whole store in every process. Manifest CAS prevents two stale manifests
from both becoming current, but it does not prevent duplicate expensive work,
conflicting maintenance decisions, or unsafe interaction with GC.

The queue lease also does not cover manual `flush`, `compact`,
`maintain-vectors`, or the automatic maintenance loop.

### Required work

- [ ] Initially run one maintenance leader using a Kubernetes
      `coordination.k8s.io/Lease`, or an object-store CAS lease if Sana should
      remain independent of Kubernetes.
- [ ] Add per-namespace ownership for compaction and vector maintenance.
- [ ] Prefer durable maintenance jobs with job kind, namespace, target
      generation, attempts, lease, and fencing token.
- [ ] Make every publishing entry point use the same ownership mechanism,
      including CLI operations.
- [ ] Re-check the source generation and ownership immediately before manifest
      publication.
- [ ] Test leader death, lease expiry, duplicate execution, and stale workers.

Kubernetes uses Lease objects for leader election in its own HA components:
[Kubernetes Leases](https://kubernetes.io/docs/concepts/architecture/leases/).

## P0: Finish the global queue broker

**Status:** the queue data model is useful; the deployed broker topology is
missing.

Sana already has:

- One global CAS-updated queue object:
  [`IndexQueue`](../src/index_queue.rs#L367-L531).
- Leased claims, attempts, heartbeats, retries, and at-least-once work:
  [`run_worker_once`](../src/index_queue.rs#L827-L884).
- An in-process group-commit broker:
  [`IndexQueueBroker`](../src/index_queue.rs#L583-L595).

But normal operations bypass that broker:

- Writes enqueue directly through `IndexQueue`:
  [`namespace.rs`](../src/namespace.rs#L953-L959).
- Workers claim and heartbeat directly through `IndexQueue`:
  [`index_queue.rs`](../src/index_queue.rs#L827-L846).
- Reconciliation creates a temporary broker local to one process:
  [`index_queue.rs`](../src/index_queue.rs#L751-L759).

### Why direct CAS stops scaling

1. Every API and indexer pod reads the same queue object.
2. Each pod modifies its private copy.
3. Only one CAS succeeds.
4. Losers download the new queue and retry.
5. Heartbeats, claims, completions, and enqueues contend on the same object.
6. More pods produce more retries rather than more queue throughput.

### Required work

- [ ] Introduce a `QueueClient` boundary used by all enqueue, claim, heartbeat,
      complete, fail, and reconcile operations.
- [ ] Implement a standalone broker service that owns the group-commit loop.
- [ ] Route all normal queue mutations through the broker.
- [ ] Acknowledge a request only after its batched CAS is durable.
- [ ] Add broker discovery and an epoch/lease for failover.
- [ ] Permit brief overlapping brokers; CAS remains the final correctness guard.
- [ ] Keep direct object-store CAS as a dev/recovery backend, not the normal
      multi-pod path.
- [ ] Measure queue size, CAS rate, batch size, retries, claim latency, and
      oldest-job age before considering sharding.

Do **not** shard the queue preemptively. Turbopuffer reports that a single JSON
queue with a stateless, HA, brokered group-commit loop handles its indexing
traffic:

- [How to build a distributed queue in a single JSON file on object storage](https://turbopuffer.com/blog/object-storage-queue)
- [Amazon S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)

## P0: Kubernetes lifecycle and health

**Status:** shutdown and health checks are not sufficient for Kubernetes.

- [`shutdown_signal`](../src/main.rs#L431-L438) handles Ctrl-C but not SIGTERM.
- [`/healthz`](../src/api.rs#L368-L370) always reports success.
- There is no separate readiness state or drain state.

### Failure

1. Kubernetes sends SIGTERM during rollout or eviction.
2. Sana does not observe it.
3. The process remains alive until Kubernetes sends SIGKILL.
4. HTTP requests and background jobs are interrupted without controlled drain.

### Required work

- [ ] Handle both Ctrl-C and SIGTERM.
- [ ] Add `/livez` for process health and `/readyz` for traffic readiness.
- [ ] Do not make liveness fail only because S3 is temporarily unavailable.
- [ ] Make readiness fail during startup, backend failure, overload, and drain.
- [ ] On shutdown, become unready first and stop accepting new work.
- [ ] Drain HTTP requests and stop claiming jobs.
- [ ] Finish, fail, or release the current job before the termination deadline.
- [ ] Set Kubernetes `terminationGracePeriodSeconds` from the maximum drain time.

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

**Status:** `MAX_QUERY_RESULTS` applies only when the caller supplies `limit`.

[`execute_with_snapshot`](../src/query.rs#L330-L419) rejects
`limit > 10_000`, but `limit: null` returns every matching document.
`Query::all()` uses `limit: None`.

### Failure

1. A client sends an unfiltered query without `limit`.
2. Sana materializes and sorts the full namespace.
3. The HTTP response contains every document.
4. A large namespace can exhaust pod memory or exceed practical response size.

### Required work

- [ ] Define an effective default limit, initially `MAX_QUERY_RESULTS`.
- [ ] Apply it to every query path, including ordinary, text, vector, and
      multi-query responses.
- [ ] Keep aggregate semantics explicit: aggregate over all matches or only the
      returned page.
- [ ] Add cursor-based pagination before advertising full scans over HTTP.
- [ ] Test omitted limits and multi-query worst cases.

## P1: Do not silently lose integer precision

**Status:** JSON integers above `i64::MAX` silently become `f64`.

[`ValueVisitor::visit_u64`](../src/value.rs#L270-L279) converts an out-of-range
`u64` with `v as f64`. Many such integers cannot be represented exactly.

### Failure

1. Client sends an integer attribute larger than `i64::MAX`.
2. Deserialization changes its type from integer to float.
3. Values above the exact binary64 range can change numerically.
4. Equality filters, ordering, serialization, and stored data use the changed
   value without reporting an error.

### Required work

- [ ] Choose one explicit contract:
      add `Value::UInt(u64)`, reject values above `i64::MAX`, or require a
      string representation.
- [ ] Never silently convert an integer to a lossy float.
- [ ] Add boundary tests around `2^53`, `i64::MAX`, and `u64::MAX`.

RFC 8259 identifies `[-(2^53)+1, (2^53)-1]` as the interoperable exact integer
range for common JSON implementations:
[RFC 8259, Numbers](https://www.rfc-editor.org/rfc/rfc8259#section-6).

## P1: Make F16 JSON writes schema-aware

**Status:** F16 vectors serialize as plain floats but deserialize as F32.

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

- [ ] Parse HTTP vectors into a neutral wire representation such as `Vec<f32>`.
- [ ] Convert to F16 or F32 using the existing column schema before validation.
- [ ] Infer F32 only when creating a new vector column without an explicit schema.
- [ ] Add HTTP round-trip tests for both F16 and F32 columns.

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

- [ ] Use `1.0 / (k + rank as f64 + 1.0)`.
- [ ] Add a small deterministic unit test.

Source: [Cormack, Clarke, and Buettcher, Reciprocal Rank Fusion](https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf).

## Suggested order

1. Disable automatic GC.
2. Fix unbounded results, integer precision, and F16 round trips.
3. Split API, indexer, maintenance, and broker roles.
4. Route every queue mutation through the standalone broker.
5. Add maintenance leader election and per-namespace ownership.
6. Implement reader/publisher watermarks, then re-enable GC.
7. Add Kubernetes lifecycle, probes, renewable AWS credentials, and the HTTP
   trust boundary.
8. Add cache-aware routing after the basic multi-pod deployment is correct.
