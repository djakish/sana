# RFC 0001: Guarded namespace drop lifecycle

- **Status:** Draft (for review)
- **Author:** djakish (AI-assisted)
- **Created:** 2026-06-27
- **Tracking:** `docs/TODO.md` → "P1: Add a guarded namespace drop lifecycle"
- **Touches invariants:** durability, distributed coordination, safe object
  reclamation. Treat every part of this as a durability change until proven
  otherwise.
- **Related decisions:** D84 (GC liveness recheck before delete), D85 (durable
  reader leases), and the P0 "Safe object reclamation" backlog.

## 1. Summary

Add a first-class operation that retires a namespace — `sana drop <store>
<namespace>`, a library entry point, and (optionally) `DELETE
/v2/namespaces/:namespace` — that removes the namespace's manifest pointer, WAL,
idempotency records, indexes, and per-namespace coordination state as one
lifecycle action, while **refusing by default to delete immutable objects that a
branch still references**. Physical reclamation of immutable objects flows
through the same safe-GC protocol Sana is building for P0, not through a blind
prefix delete.

## 2. Motivation

Today a namespace can be created, branched, copied, exported, written, and
queried, but there is no supported way to delete one. Operators can delete rows,
but not the namespace itself. The only "drop" available is a manual object-store
prefix wipe, which is unsafe:

1. A namespace can be created accidentally or become obsolete and there is no
   way to reclaim its storage.
2. Deleting `namespaces/{ns}/` by hand breaks any branch whose manifest still
   references this namespace's immutable index objects (copy-on-write sharing).
3. A manual wipe ignores store-global state that names the namespace: queued
   indexing jobs, reader leases, and pinning state.

turbopuffer exposes this as a core, total operation: `DELETE
/v2/namespaces/:namespace` "deletes the namespace and all its documents
entirely. There is no way to recover a deleted namespace," and the name becomes
reusable immediately after `200`. Branch isolation is explicit:

> Both the source and branched namespaces are fully independent after creation.
> Deleting a branch does not affect the source namespace, and deleting the
> source does not affect any branches.
> — turbopuffer, `docs/branching.md`

Because turbopuffer branches share underlying storage (copy-on-write), that
guarantee forces a reference-aware reclamation: the shared bytes must outlive
whichever namespace is deleted first. Sana must preserve this semantic.

## 3. Current state in Sana

Object layout (`src/namespace.rs`, `src/indexer.rs`):

| Kind | Key | Mutability |
| --- | --- | --- |
| Manifest pointer | `namespaces/{ns}/manifest/current` | mutable (CAS) |
| Manifest bodies | `namespaces/{ns}/manifest/g/{gen}.json` | immutable |
| WAL commit cursor | `namespaces/{ns}/wal_commit/current` | mutable (CAS) |
| WAL entries | `namespaces/{ns}/wal/{epoch}/{seq}.wal` | immutable |
| Idempotency records | `namespaces/{ns}/idempotency/{key}.json` | immutable |
| Index objects | `namespaces/{ns}/index/g/{gen}/{doc,attr,fts,vector}/…` | immutable |

Store-global state that *names* a namespace (lives outside its prefix):

- `jobs/indexing_queue.json` — jobs carry a `namespace` field
  (`src/index_queue.rs`).
- `jobs/readers/{owner}.json` — reader leases record the namespace and the exact
  manifest body key (`src/reader_lease.rs`, D85).
- pinning state — `pinning_key(namespace)` (`src/pinning.rs`).
- `jobs/maintenance_leader.json` — store-global, **not** per-namespace; a drop
  must not touch it.

Branch references already have machinery:

- `operations::branch` (`src/operations.rs`) writes a child manifest with
  `branch_parent: Some(BranchParent { … })` whose `referenced_index_keys()`
  point into the **parent's** prefix (copy-on-write; no bytes copied).
- `indexer::foreign_references_into_namespace` (`src/indexer.rs:1023`) lists all
  namespaces, loads their manifests, and collects every `referenced_index_key`
  that points into the target namespace's prefix. This is exactly the
  "is anyone else using my objects?" query a safe drop needs.

Constraints from the non-negotiable invariants (`CLAUDE.md`):

- Automatic object deletion stays off until reader and publisher safety points
  are durable and tested.
- Never add object-store `list` to a query or write hot path (drop is a control
  operation, so listing is acceptable there).
- Reject stale ownership / corruption explicitly.

## 4. Design

### 4.1 Two phases: tombstone, then reclaim

A drop separates **logical removal** (the namespace stops existing and its name
is reusable) from **physical reclamation** (immutable bytes are deleted). This
mirrors Iceberg's distinction between a metadata-only `DROP TABLE` and `DROP
TABLE … PURGE`, and Delta Lake's "remove the table, VACUUM reclaims files
later."

**Phase A — tombstone (always safe, fast, durable):**

1. Open the namespace and load `manifest/current`.
2. Compute `foreign_references_into_namespace`. If non-empty and `--force` is not
   set, **refuse** with a structured error listing the dependent namespaces
   (branches that still share this namespace's objects).
3. Write a durable tombstone marker
   `namespaces/{ns}/manifest/dropped.json` (immutable, `put_if_absent`)
   recording: dropped-at timestamp, last generation, the operator/owner id, and
   whether `--force` purge was requested. This is the audit record and the
   idempotency anchor for a resumed drop.
4. CAS-delete the mutable coordination objects so the namespace immediately
   stops serving and its name is reusable:
   - `manifest/current` (this is the point of no return; once gone, opens fail
     with `NotFound` and a fresh `create` can reuse the name).
   - `wal_commit/current`.
   - Remove this namespace's jobs from `jobs/indexing_queue.json` via the
     `QueueClient` mutation boundary (a queued flush for a dropped namespace must
     not run).
   - Fence pinning state (`pinning_key`).

After Phase A returns, the namespace is gone for all readers and writers, the
name is reusable, and only immutable objects (manifest bodies, WAL entries,
idempotency records, index objects) plus the tombstone remain.

**Phase B — reclaim (deferred, reference-aware):**

Physical deletion of the immutable objects is **not** done inline by default,
because Sana has no publisher safety point yet (P0). Instead:

- The drop enqueues the namespace's immutable objects as GC candidates, minus
  the set returned by `foreign_references_into_namespace` (objects a branch
  still needs) and minus anything an unexpired reader lease pins (D85).
- The existing safe-GC path reclaims them once it can prove no reader/publisher
  references remain — the same protocol P0 is hardening. Until that protocol is
  durable, Phase B runs only in the opt-in/quiescent mode that today's
  `maintenance gc` and `gc --apply` already require (D84 re-checks liveness
  immediately before each delete).

### 4.2 The `--force` matrix

| Situation | Default | `--force` |
| --- | --- | --- |
| No foreign references | Tombstone + schedule reclaim | Tombstone + purge now (quiescent) |
| Branches reference objects | **Refuse** | Tombstone only; shared objects survive for the branches; non-shared objects scheduled for reclaim |

`--force` never deletes an object a branch references — that would violate
turbopuffer's branch-isolation guarantee. It only relaxes the "refuse if
*anything* depends on me" gate into "tombstone me now, reclaim what is provably
mine." This makes `--force` safe-by-construction rather than a foot-gun.

### 4.3 Ordering and crash safety

The publish-before-pointer invariant runs in reverse for deletion: **delete the
pointer that names objects before deleting the objects.** Sequence:

1. Tombstone marker (`put_if_absent`) — durable intent.
2. Delete `manifest/current` (CAS) — namespace becomes unreachable.
3. Delete other mutable coordination state (WAL commit, queue jobs, pinning).
4. Schedule/perform immutable reclamation last.

A crash between any two steps is recoverable: the tombstone marker lets a resumed
drop (same idempotency anchor) detect partial progress and continue. A namespace
whose `manifest/current` is gone but whose immutable objects linger is exactly an
orphan set the normal GC already understands — it is not corruption. This is the
property TiKV relies on for `unsafe_destroy_range`: the range is logically gone
immediately; physical space returns asynchronously.

### 4.4 Surfaces

- **Library:** `operations::drop_namespace(store, name, DropOptions { force,
  purge })` returning a `DropReport { tombstoned, foreign_blockers,
  reclaim_scheduled, reclaimed }`.
- **CLI:** `sana drop <store> <namespace> [--force] [--purge]`.
- **HTTP (optional, gated):** `DELETE /v2/namespaces/:namespace` mapping to the
  default (non-purge) path, behind the same trust boundary as other write
  routes (`docs/TODO.md` P1 HTTP boundary). Returns `{ "status": "ok" }` to match
  turbopuffer's contract.

## 5. Alternatives considered

1. **Blind prefix delete (`list namespaces/{ns}/` + `delete`).** Simplest, but
   corrupts branches and ignores store-global state. Rejected — violates branch
   isolation.
2. **Reference counting per immutable object.** Maintain a refcount so the last
   namespace to release an object deletes it. Powerful but introduces a new
   durable, contended counter object per shared object and a new failure mode
   (lost decrements). Rejected for now in favor of mark-and-sweep via
   `foreign_references_into_namespace`, which is stateless and already exists.
   Revisit if branch fan-out makes full-scan reference checks too expensive.
3. **Inline synchronous purge always.** Matches the "200 means gone" feel but
   requires the publisher safety point Sana does not have, so it cannot be the
   default without risking the "unpublished object" race in the P0 backlog.

## 6. Testing plan

- Drop a namespace with no branches; assert pointer/WAL/idempotency/index/queue
  state gone and the name is immediately reusable by `create`.
- Drop a source namespace that has a live branch; assert it is **refused** by
  default, and that with `--force` the branch still resolves every shared object
  (`branch.scan()` unchanged) while non-shared objects are scheduled.
- Branch-of-branch chains (A→B→C): drop B; assert A and C remain correct.
- Crash injection (see RFC 0002): kill between tombstone and pointer delete,
  between pointer delete and queue cleanup, and mid-reclaim; assert a resumed
  drop converges and never deletes a branch-referenced object.
- Concurrent writer/reader during drop: a write racing Phase A either commits
  before the pointer delete or fails cleanly with `NotFound`; an in-flight query
  holding a reader lease keeps its objects until the lease expires.
- Idempotency: re-running `drop` after a partial drop returns the same
  `DropReport` shape and makes no further changes.

## 7. Risks and open questions

- **R1 — interaction with P0 safe GC.** Phase B depends on the publisher safety
  point. Until that lands, `--purge` is quiescent-only. Is shipping Phase A
  (tombstone + name reuse) alone, with reclamation deferred to manual GC,
  acceptable for a first cut? (Proposed: yes.)
- **R2 — `foreign_references` cost.** It lists `namespaces/` and loads every
  other manifest. Fine as a control-plane op; quantify at thousands of
  namespaces and consider a cached branch-parent index if needed.
- **Q1 — tombstone retention.** How long do we keep `manifest/dropped.json`
  after reclamation completes — forever (audit) or GC it too? (Proposed: keep
  as a small audit record; revisit with `docs/audit-logs` parity.)
- **Q2 — HTTP exposure.** Should `DELETE` ship before the P1 auth boundary, or
  stay library/CLI-only until then? (Proposed: CLI/library first.)
- **Q3 — name-reuse vs in-flight reader.** A reader lease can still name a
  just-dropped generation; create-then-reuse must not collide. Body keys are
  content/generation addressed, so reuse writes new generations — confirm no key
  collision with a lingering lease's body key.

## 8. References

- turbopuffer: `docs/delete-namespace.md`, `docs/branching.md`,
  `docs/guarantees.md` (branch isolation), `docs/backups.md`
  (`copy_from_namespace` for full isolation).
- Apache Iceberg, "DROP TABLE [PURGE]" — metadata drop vs physical purge:
  https://iceberg.apache.org/docs/latest/spark-ddl/#drop-table
- Delta Lake VACUUM — deferred reclamation with a concurrent-reader retention
  window: https://docs.delta.io/latest/delta-utility.html#vacuum
- TiKV `unsafe_destroy_range` — logical range deletion, asynchronous space
  reclamation: https://docs.pingcap.com/tidb/stable/garbage-collection-overview/
- Sana internals: `src/operations.rs` (branch/copy), `src/indexer.rs`
  (`foreign_references_into_namespace`), `src/reader_lease.rs` (D85),
  `docs/PROGRESS.md` D84–D85, `docs/ARCHITECTURE.md`.
