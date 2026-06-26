# Sana Code Review

Review the final diff as if it came from another engineer. Findings come before
summary. Use file and line references, and fix confirmed issues before calling
the task complete.

## Correctness

- Trace success, retry, crash, timeout, cancellation, and stale-owner paths.
- Check ordering and linearization points around object-store writes and CAS.
- Check arithmetic, bounds, cursor epochs, generations, attempts, and fencing.
- Confirm errors are explicit; no corruption or precision loss is hidden.

## Distributed Safety

- If the change implements a turbopuffer-inspired subsystem, compare it against
  the relevant checked-in export and identify semantic deviations. The default
  review expectation is maximum fidelity to the published turbopuffer shape
  unless `docs/PROGRESS.md` records why Sana intentionally deviates.
- Readers cannot observe a catalog that references missing objects.
- CAS losers cannot overwrite a winner's immutable data.
- Retried operations are idempotent or safely rejected.
- Leases and claims reject stale workers after reassignment.
- A process restart does not erase state required for correctness.
- Scaling a role does not multiply unrelated maintenance or coordination work.

## Storage And Query Semantics

- Persisted bytes remain deterministic and compatible, or the format change is
  versioned and documented.
- Query snapshots remain coherent across the manifest and WAL overlay.
- Approximate indexes remain supersets and live-document rechecks still occur.
- Limits, pagination, memory growth, and object-store round trips stay bounded.

## Tests And Documentation

- A regression test fails without the change and covers the important failure
  path, not only the happy path.
- The focused test and required repository checks pass.
- `docs/ARCHITECTURE.md` describes current behavior.
- `docs/PROGRESS.md` records durable decisions and current status.
- `docs/TODO.md` reflects only gaps that still exist.

## Review Result

Record one of:

- Findings fixed, with the relevant tests.
- No findings, with residual risks or checks that could not be run.
- Blocked, with the missing evidence or external dependency.
