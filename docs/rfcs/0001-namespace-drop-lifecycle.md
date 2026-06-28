# RFC 0001: Guarded namespace drop lifecycle

- **Status:** Draft, revision 2 (revised after review; not ready to implement —
  see §9)
- **Author:** djakish (AI-assisted)
- **Created:** 2026-06-27
- **Revised:** 2026-06-28 (rev 2: namespace incarnation model)
- **Tracking:** `docs/TODO.md` → "P1: Add a guarded namespace drop lifecycle"
- **Touches invariants:** durability, distributed coordination, safe object
  reclamation, **persisted-key layout / storage compatibility**. Treat every
  part of this as a durability change until proven otherwise.
- **Related decisions:** D84 (GC liveness recheck before delete), D85 (durable
  reader leases), and the P0 "Safe object reclamation" backlog.

## 0. Revision history

- **rev 1 (2026-06-27):** Two-phase drop (tombstone, then deferred reclaim) with
  immediate name reuse, `--force`/`--purge`, deferred GC.
- **rev 2 (2026-06-28):** Reworked after a review found rev 1 unsafe against
  Sana's current keyspace. The core change is a **namespace incarnation model**;
  drop is a **CAS state transition**, not a blind delete; reclamation runs from a
  **self-describing tombstone** instead of the live-manifest GC; queue cleanup
  goes through a **new brokered mutation**; the foreign-reference helper becomes a
  **dependency map**. See §9 for the point-by-point response to the review.

## 1. Summary

Add a first-class operation that retires a namespace — `sana drop <store>
<namespace>`, a library entry point, and (optionally) `DELETE
/v2/namespaces/:namespace`. Retiring a namespace must satisfy turbopuffer's
contract: after the call returns, "you can reuse the same namespace name by
writing to it again" (`docs/delete-namespace.md`), while branch isolation is
preserved (a branch that still references the dropped namespace's immutable
objects keeps working).

Rev 1 tried to deliver immediate name reuse by deleting the manifest pointer and
leaving immutable objects behind. The review showed that is unsafe with Sana's
current keys: a re-created namespace reuses the same WAL/idempotency keys at
`epoch 0` and collides with the dropped namespace's lingering immutable bytes,
which Sana treats as **corruption**. Rev 2 introduces a per-name **incarnation
id** embedded in every durable per-namespace key, so a reused name writes into a
fresh, disjoint keyspace and the old incarnation's bytes are reclaimed
independently. Drop becomes a single durable **CAS transition** of the namespace
pointer to a `Dropped` state plus a self-describing tombstone that a
dropped-namespace GC pass consumes.

## 2. Motivation

Today a namespace can be created, branched, copied, exported, written, and
queried, but there is no supported way to delete one. The only "drop" available
is a manual object-store prefix wipe, which is unsafe:

1. There is no way to reclaim an obsolete namespace's storage.
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
whichever namespace is deleted first. Sana must preserve both semantics —
immediate name reuse **and** branch isolation — and the review showed they
cannot both hold without an incarnation id.

## 3. Current state in Sana (the constraints rev 1 missed)

Object layout (`src/namespace.rs`, `src/indexer.rs`):

| Kind | Key | Mutability |
| --- | --- | --- |
| Manifest pointer | `namespaces/{ns}/manifest/current` | mutable (CAS) |
| Manifest bodies | `namespaces/{ns}/manifest/g/{gen}.json` | immutable |
| WAL commit cursor | `namespaces/{ns}/wal_commit/current` | mutable (CAS) |
| WAL entries | `namespaces/{ns}/wal/{epoch}/{seq}.wal` | immutable |
| Idempotency records | `namespaces/{ns}/idempotency/{hex}.json` | immutable |
| Index objects | `namespaces/{ns}/index/g/{gen}/{doc,attr,fts,vector}/…` | immutable |

Store-global state that *names* a namespace (lives outside its prefix):

- `jobs/indexing_queue.json` — jobs carry a `namespace` field
  (`IndexJob`, `src/index_queue.rs:69`).
- `jobs/readers/{owner}.json` — reader leases record the namespace and the exact
  manifest body key (`src/reader_lease.rs`, D85).
- pinning state — `pinning_key(namespace)` (`src/pinning.rs`).
- `jobs/maintenance_leader.json` — store-global, **not** per-namespace; a drop
  must not touch it.

### 3.1 Why immediate reuse is unsafe with today's keys

These four facts, all verified in the current tree, are the binding constraints:

1. **A fresh create resets the WAL cursor to `epoch 0, seq 0`.**
   `Namespace::create_from_manifest` writes
   `WalCommitState::new(WalCursor::new(0, 0))` (`src/namespace.rs:454`). So a
   re-created namespace begins writing `namespaces/{ns}/wal/0/0.wal` again.
2. **Immutable keys reject conflicting bytes as corruption.**
   `put_immutable_if_absent` returns `Error::Corrupt("immutable object key has
   conflicting bytes…")` when the key exists with different bytes
   (`src/namespace.rs:352`–`367`). WAL entries and idempotency records are
   written this way, keyed only by `epoch/seq` / the user's idempotency key — not
   content-addressed. A reused name whose first write differs from the dropped
   namespace's lingering `wal/0/0.wal` therefore **fails as corruption**, not as
   a clean overwrite.
3. **GC needs a *live* manifest to compute liveness.** `scan_gc` starts with
   `ns.load_manifest_snapshot()` (`src/indexer.rs:950`–`951`); once the pointer
   is gone, `Namespace::open` returns `NotFound` (`src/namespace.rs:476`–`482`)
   and there is no way to enumerate what was live. Rev 1's claim that lingering
   objects are "an orphan set the normal GC already understands" was wrong.
4. **There is no CAS-delete primitive.** `ObjectStore` offers
   `compare_and_set(key, expected_version, bytes)` (sets *to bytes*,
   `src/object_store.rs:80`) and unconditional idempotent `delete`
   (`src/object_store.rs:91`). "CAS-delete the pointer" (rev 1) is not
   expressible; an unconditional delete cannot linearize drop against
   create / resumed-drop / racing writers.

Two more surface gaps the review flagged:

5. **Queue cleanup has no API.** `QueueClient` exposes only `enqueue`, `claim`,
   `heartbeat`, `complete`, `fail` (`src/index_queue.rs:49`–`59`). Removing a
   namespace's jobs is not expressible without a direct queue-file CAS side path,
   which the brokered-queue design exists specifically to avoid.
6. **Foreign-reference helper can't name blockers.**
   `foreign_references_into_namespace` returns a flat `BTreeSet<String>` of
   referenced keys (`src/indexer.rs:1023`, `:1058`), not which namespace
   references them — so rev 1's "structured error listing dependent namespaces"
   could not be built from it.

Constraints from the non-negotiable invariants (`CLAUDE.md`):

- Automatic object deletion stays off until reader and publisher safety points
  are durable and tested.
- Never add object-store `list` to a query or write hot path (drop is a control
  operation, so listing is acceptable there).
- Reject stale ownership / corruption explicitly.
- Preserve deterministic persisted serialization and compatibility fixtures —
  the key-layout change in §4.1 is a storage-format change and needs versioning.

## 4. Design

### 4.1 Namespace incarnation model

Introduce a per-name **incarnation id** (`incarnation: u64`, monotonic per
name). Every *per-namespace* durable key gains the incarnation as a path
segment:

```text
namespaces/{ns}/{incarnation}/manifest/current      (mutable pointer body)
namespaces/{ns}/{incarnation}/manifest/g/{gen}.json
namespaces/{ns}/{incarnation}/wal_commit/current
namespaces/{ns}/{incarnation}/wal/{epoch}/{seq}.wal
namespaces/{ns}/{incarnation}/idempotency/{hex}.json
namespaces/{ns}/{incarnation}/index/g/{gen}/…
```

The name-level existence sentinel stays at a stable, incarnation-independent key
so create / open / drop can find "the current incarnation". It evolves from
today's `manifest/current` into a small **head pointer**:

```text
namespaces/{ns}/head        (mutable, CAS)

NamespaceHead ::= {
  incarnation: u64,
  state: Active { pointer: ManifestPointer }     // pointer's body keys are
       | Dropped { tombstone_key, dropped_at_ms } // incarnation-scoped
}
```

- **create:** `put_if_absent` a head with `incarnation = 1, Active`, **or** CAS a
  `Dropped` head to `Active` with `incarnation + 1`. Then write the
  incarnation-scoped manifest body and WAL cursor. Because every durable key is
  under the new incarnation, it is **disjoint** from any lingering bytes of prior
  incarnations — fact (2) can no longer fire.
- **open:** read `head`; `NotFound` if missing or `state = Dropped`; otherwise
  open the incarnation named by the head.
- **drop:** CAS `head` from `Active{incarnation = N}` to `Dropped`. This single
  CAS is the **linearization point** and uses the primitive Sana already has
  (`compare_and_set`), resolving fact (4). A racing writer's commit that loses
  this CAS fails cleanly; a resumed drop that finds the head already `Dropped` is
  a no-op.

This is the smallest change consistent with turbopuffer's published "name is
immediately reusable" contract; turbopuffer's internal generation scheme is not
exported, so the incarnation is stated here as an explicit assumption (per
`CLAUDE.md`: when the exports are silent, choose the smallest design consistent
with the published shape).

**Storage compatibility.** This changes the per-namespace key layout and the
sentinel payload — a persisted-format change. It must ship behind a format
version with new compatibility fixtures, and a one-time migration that treats
existing namespaces as `incarnation = 0` under the legacy (segment-less) keys.
Detailed migration mechanics are an open question (Q4).

### 4.2 Drop as a CAS transition, with a self-describing tombstone

Drop separates **logical removal** (the head flips to `Dropped`; the name is
reusable) from **physical reclamation** (immutable bytes are deleted later).
This mirrors Iceberg's `DROP TABLE` vs `DROP TABLE … PURGE` and Delta Lake's
"remove the table, VACUUM reclaims files later."

**Sequence (each step durable and idempotent):**

1. **Read the head and the live manifest.** If `state = Dropped`, the drop has
   already happened — return the existing report (idempotent).
2. **Compute the dependency map** (§4.4). If any branch references this
   incarnation's objects and `--force` is not set, **refuse** with a structured
   error naming the dependent namespaces.
3. **Write the tombstone** `namespaces/{ns}/{incarnation}/manifest/dropped.json`
   (immutable, `put_if_absent`) — the audit record, the resume anchor, **and the
   reclamation snapshot**. It embeds everything the dropped-namespace GC needs
   *without re-opening the namespace* (resolving fact (3)):
   - `incarnation`, `dropped_at_ms`, operator/owner id, `--force`/`--purge` flags;
   - the last manifest body key and `referenced_index_keys()` (this
     incarnation's live object set);
   - the last committed WAL cursor (so WAL entries are enumerable);
   - the dependency map computed at step 2 (advisory; re-checked at delete time).
4. **CAS the head** `Active{N}` → `Dropped{tombstone_key, dropped_at_ms}`. After
   this returns, the namespace is unreachable and the name is reusable (a new
   `create` allocates incarnation `N + 1`).
5. **Brokered queue cleanup:** call the new `remove_namespace_jobs(ns,
   incarnation)` broker mutation (§4.3); fence pinning state.
6. **Schedule reclamation** of the tombstone's object set via the
   dropped-namespace GC pass (§4.5).

A crash between any two steps converges: between 3 and 4 a resumed drop finds the
tombstone and re-issues the head CAS; after 4 the head is `Dropped` and the
tombstone drives GC independently of any reuse. A namespace whose head is
`Dropped` but whose immutable objects linger is exactly the asynchronous
space-reclamation TiKV relies on for `unsafe_destroy_range`: logically gone
immediately, physically reclaimed later.

### 4.3 Brokered queue mutation

Add to `QueueClient` (and the broker request enum) a single mutation:

```rust
async fn remove_namespace_jobs(&self, namespace: &str, incarnation: u64)
    -> Result<usize>;   // returns jobs removed
```

Routing it through the broker keeps all `jobs/indexing_queue.json` writes on the
one CAS boundary (no direct side path in multi-pod deployments), exactly as
`enqueue`/`claim` do today. **Defense in depth:** because `IndexJob` will carry
the `incarnation` (alongside its existing `namespace` field,
`src/index_queue.rs:69`), a worker that claims a stale job whose incarnation no
longer matches the head simply skips it — so a queued flush for a dropped
incarnation can never run even if cleanup has not yet executed.

### 4.4 Dependency map (foreign references)

Replace the flat helper with one that records *who* depends on the target:

```rust
async fn foreign_references_into_incarnation(store, ns, incarnation)
    -> Result<BTreeMap<String /*dependent ns*/, BTreeSet<String> /*keys*/>>;
```

It lists `namespaces/`, loads each *other* namespace's manifest, and groups every
`referenced_index_key` that points into `namespaces/{ns}/{incarnation}/` by the
referencing namespace. The drop refusal error uses the map keys to name blockers;
the existing GC call site flattens it (`.values().flatten()`), so GC behavior is
unchanged. The flat `foreign_references_into_namespace` can remain as a thin
wrapper to avoid churn at the GC site.

### 4.5 Dropped-namespace reclamation (Phase B)

Physical deletion runs in a **dropped-namespace GC pass** that takes the
*tombstone*, not a live `Namespace`:

```text
scan_gc_for_dropped(store, ns, incarnation, tombstone) -> GcScan
```

It lists `namespaces/{ns}/{incarnation}/` and marks every object an orphan
**except**:

- objects still named by a live branch (re-check the dependency map at delete
  time, per D84 — never trust the tombstone's snapshot for the final delete);
- objects an unexpired reader lease pins (D85).

Because Sana has no publisher safety point yet (P0), this pass deletes only in
the opt-in/quiescent mode that `maintenance gc` / `gc --apply` already require.
Until that protocol is durable, the default drop tombstones and **defers** all
deletion; `--purge` (inline reclaim) is rejected unless the quiescent safety
conditions hold. The tombstone itself is the last object reclaimed.

### 4.6 The `--force` / `--purge` matrix (now orthogonal)

`--force` and `--purge` are independent flags with separate jobs:

- **`--force` — bypass the branch-reference *refusal* only.** It never deletes a
  branch-referenced object; it relaxes "refuse if anything depends on me" into
  "tombstone now; reclaim only what is provably mine." Branch-referenced objects
  survive for the branches.
- **`--purge` — request inline physical reclamation.** Allowed only under
  quiescent / publisher-safety-point conditions (no active reader leases on this
  incarnation, P0 protocol satisfied). Orthogonal to `--force`.

| Flags | Foreign refs present? | Behavior |
| --- | --- | --- |
| (none) | no | Tombstone + flip head + schedule deferred reclaim |
| (none) | yes | **Refuse**, error names dependent namespaces |
| `--force` | yes | Tombstone + flip head; shared objects survive; provably-unreferenced scheduled |
| `--purge` | no, quiescent | Tombstone + flip head + reclaim provably-mine **now** |
| `--purge` | not quiescent | **Reject** `--purge`; suggest deferred drop |
| `--force --purge` | yes, quiescent | Bypass refusal **and** purge provably-unreferenced now |

### 4.7 Surfaces

- **Library:** `operations::drop_namespace(store, name, DropOptions { force,
  purge })` returning `DropReport { incarnation, tombstoned, foreign_blockers:
  BTreeMap<String, BTreeSet<String>>, reclaim_scheduled, reclaimed }`.
- **CLI:** `sana drop <store> <namespace> [--force] [--purge]`.
- **HTTP (optional, gated):** `DELETE /v2/namespaces/:namespace` mapping to the
  default (non-purge) path, behind the same trust boundary as other write routes.
  Returns `{ "status": "ok" }` to match turbopuffer's contract.

## 5. Alternatives considered

1. **Blind prefix delete (`list` + `delete`).** Corrupts branches and ignores
   store-global state. Rejected — violates branch isolation.
2. **Defer name reuse until physical purge completes (no incarnation).** Keep
   rev 1's delete-the-pointer model but refuse to re-create the name until the
   old objects are fully reclaimed. Safe with today's keys and a much smaller
   change, but it **breaks turbopuffer's "immediately reusable" contract** and
   couples name availability to GC latency. Viable as a *phase 0* if the
   incarnation migration is judged too large for a first cut; recorded here as
   the explicit fallback.
3. **Reference counting per immutable object.** A durable refcount per shared
   object; last releaser deletes. Powerful but adds a contended counter object
   and a lost-decrement failure mode. Rejected for now in favor of stateless
   mark-and-sweep via the dependency map. Revisit if branch fan-out makes
   full-scan reference checks too expensive.
4. **Inline synchronous purge always.** Matches the "200 means gone" feel but
   requires the publisher safety point Sana lacks (P0). Cannot be the default.

## 6. Testing plan

- **Name reuse correctness (the rev-1 bug):** drop a namespace that has written
  WAL/idempotency records, immediately `create` the same name, write a
  *different* first batch; assert it succeeds (new incarnation) and never raises
  `Error::Corrupt` from a colliding `wal/0/0.wal` or idempotency key.
- **Linearization:** concurrent `drop` + `create` + writer commit on one name;
  assert exactly one head transition wins each CAS and no operation observes a
  torn state.
- **Branch isolation:** drop a source with a live branch — **refused** by
  default; with `--force` the branch still resolves every shared object
  (`branch.scan()` unchanged) while non-shared objects are scheduled. Chains
  A→B→C: drop B; assert A and C remain correct.
- **Dependency-map error:** assert the refusal error lists the exact dependent
  namespaces, not just keys.
- **Tombstone-driven GC:** with the namespace head `Dropped` (un-openable),
  assert `scan_gc_for_dropped` reclaims the incarnation's objects from the
  tombstone alone and re-checks the dependency map at delete time.
- **Queue + incarnation:** a job queued for incarnation N is removed by
  `remove_namespace_jobs`, and a worker that races and claims it skips on
  incarnation mismatch.
- **Crash injection (RFC 0002):** kill between tombstone-write and head-CAS,
  between head-CAS and queue cleanup, and mid-reclaim; assert a resumed drop
  converges and never deletes a branch-referenced object.
- **Idempotency / migration:** re-running `drop` returns the same `DropReport`;
  a legacy (`incarnation = 0`, segment-less) namespace migrates and then drops
  cleanly.

## 7. Risks and open questions

- **R1 — migration scope.** Embedding the incarnation in every durable key is a
  storage-format change touching WAL, idempotency, index, manifest, and the
  sentinel. It needs a version bump, new fixtures, and a legacy-`incarnation = 0`
  read path. This is the largest cost of rev 2.
- **R2 — interaction with P0 safe GC.** Phase B still depends on the publisher
  safety point; until it lands, `--purge` is quiescent-only and the default drop
  defers all deletion. Shipping logical drop (incarnation flip + tombstone) with
  reclamation deferred to manual GC is the proposed first cut.
- **R3 — `foreign_references` cost.** Still lists `namespaces/` and loads every
  other manifest. Fine as a control-plane op; quantify at thousands of namespaces
  and consider a cached branch-parent index.
- **Q1 — incarnation id shape.** Monotonic `u64` (needs a durable allocator —
  the head CAS provides it) vs a content/time-derived token. Proposed: `u64` in
  the head, incremented under the create/reuse CAS.
- **Q2 — HTTP exposure.** Ship `DELETE` before or after the P1 auth boundary?
  Proposed: CLI/library first.
- **Q3 — tombstone retention.** Keep `dropped.json` forever (audit) or GC it
  after reclamation? Proposed: keep as a small audit record; revisit with
  `docs/audit-logs` parity.
- **Q4 — migration mechanics.** Lazy (treat absent segment as `incarnation = 0`
  on read, rewrite on next compaction) vs an explicit one-shot migration tool.
  Proposed: lazy read-compat + opportunistic rewrite; spell out before coding.

## 8. References

- turbopuffer: `docs/delete-namespace.md` (name reusable after `200`),
  `docs/branching.md` and `docs/guarantees.md` (branch isolation),
  `docs/backups.md` (`copy_from_namespace`).
- Apache Iceberg, "DROP TABLE [PURGE]" — metadata drop vs physical purge:
  https://iceberg.apache.org/docs/latest/spark-ddl/#drop-table
- Delta Lake VACUUM — deferred reclamation with a concurrent-reader retention
  window: https://docs.delta.io/latest/delta-utility.html#vacuum
- TiKV `unsafe_destroy_range` — logical range deletion, asynchronous space
  reclamation: https://docs.pingcap.com/tidb/stable/garbage-collection-overview/
- Sana internals: `src/namespace.rs` (`create_from_manifest:454`,
  `put_immutable_if_absent:352`, `open:476`), `src/indexer.rs` (`scan_gc:950`,
  `foreign_references_into_namespace:1023`), `src/object_store.rs`
  (`compare_and_set:80`, `delete:91`), `src/index_queue.rs` (`QueueClient:49`,
  `IndexJob:69`), `src/reader_lease.rs` (D85), `docs/PROGRESS.md` D84–D85.

## 9. Response to review (rev 1 → rev 2)

| # | Review finding | Resolution in rev 2 |
| --- | --- | --- |
| P0 | Immediate name reuse collides with existing WAL/idempotency keys (`namespace.rs:81,85,352`); WAL resets to epoch 0 on create (`:454`) | §4.1 incarnation segment makes every reused-name key disjoint; §3.1 fact (2) can no longer fire |
| P0 | Phase B can't use current GC after deleting `manifest/current` (`indexer.rs:950`, `namespace.rs:475`) | §4.5 `scan_gc_for_dropped` runs from the §4.2 self-describing tombstone, never re-opening the namespace |
| P0 | "CAS-delete" is not a primitive (`object_store.rs:80,91`) | §4.1/§4.2 drop is a `compare_and_set` of the head `Active`→`Dropped`; no delete needed to linearize |
| P1 | Queue cleanup specified through an API that can't do it (`index_queue.rs:46`) | §4.3 new brokered `remove_namespace_jobs`; incarnation on `IndexJob` for defense in depth |
| P1 | Foreign-reference helper can't name dependents (`indexer.rs:1023`) | §4.4 returns a `BTreeMap<dependent ns, keys>`; refusal error names blockers |
| P2 | `--force`/`--purge` semantics muddled | §4.6 makes them orthogonal: `--force` bypasses branch refusal only; `--purge` is quiescent inline reclaim |

The reviewer's verdict ("do not implement yet") stands: rev 2 is a design
revision, not an implementation. The status remains **Draft** pending a second
review of the incarnation model and the migration plan (Q4 / R1).
