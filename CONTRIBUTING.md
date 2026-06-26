# Contributing to Sana

Sana is an educational database project, but changes should still be treated as
database changes: write down the decision, preserve invariants, add focused
tests, and keep the docs in sync.

## Before changing code

1. Read [`README.md`](README.md) for the current public shape.
2. Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the current design.
3. Read [`docs/PROGRESS.md`](docs/PROGRESS.md) before making architectural
   changes. It is the durable decision log.
4. Read [`docs/TODO.md`](docs/TODO.md) for known production-readiness gaps.
5. Check the worktree with `git status --short`. Do not overwrite unrelated
   local changes.

## Agentic Task Workflow

Use the same loop for human-led and agent-led work. This follows OpenAI's
current Codex guidance: give the task explicit context and done conditions,
store durable repository guidance in `AGENTS.md`, validate the result, and
review the final diff before accepting it.

1. **Frame the task.** Write down the goal, relevant files or evidence,
   architectural constraints, and concrete done conditions.
2. **Inspect before editing.** Trace callers, tests, persisted formats, failure
   paths, and the relevant decisions in this repository.
3. **Plan when needed.** Use a short, current plan for multi-file,
   architectural, or ambiguous work. Skip ceremony for a trivial change.
4. **Implement one coherent task.** Keep the diff scoped, but finish the
   behavior, tests, and documentation needed for that task.
5. **Verify from narrow to broad.** Run the closest regression test first,
   followed by the repository checks below.
6. **Review the diff.** Use [`docs/CODE_REVIEW.md`](docs/CODE_REVIEW.md) as an
   adversarial pass over correctness, crash behavior, distributed ownership,
   storage compatibility, and missing tests. Fix confirmed findings.
7. **Update project state.** Update architecture, progress decisions, TODO
   checkboxes, examples, and operational docs when their evidence changed.
8. **Retrospect repeated mistakes.** If an agent makes the same class of error
   twice, add a concise rule to `AGENTS.md` or the review checklist.

For architecture inspired by turbopuffer, fidelity is a standing constraint:
read the matching checked-in exports first, preserve their published semantics
as far as Sana's scope allows, and document every intentional deviation. Do not
replace a published distributed protocol with a merely similar local design.
Default to turbopuffer's published shape when the exports cover the subsystem;
choose a different coordination, storage, queue, or deployment shape only when
the reason is explicit in [`docs/PROGRESS.md`](docs/PROGRESS.md) and the current
behavior is reflected in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

OpenAI references:

- [Codex best practices](https://developers.openai.com/codex/learn/best-practices)
- [Custom instructions with AGENTS.md](https://developers.openai.com/codex/guides/agents-md)
- [Review changes in the Codex app](https://developers.openai.com/codex/app/review)

## Decision Log Workflow

Use [`docs/PROGRESS.md`](docs/PROGRESS.md) for decisions that change durable
behavior, storage layout, API semantics, concurrency, deployment shape, or major
implementation policy.

- Add a new `D#` entry for new decisions. Do not renumber old decisions.
- Keep decision entries short, concrete, and causal: what changed, why it is the
  chosen tradeoff, and what limitation remains.
- If a later change supersedes an earlier decision, leave the earlier decision in
  place and add a clearly marked `Superseded` note or a later `D#`.
- Update the top status snapshot when the change affects what is done, what is
  next, test status, or the "Last updated" date.
- Keep [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) as current-state truth.
  `PROGRESS.md` explains how the repo got there.

Good decision-log candidates:

- New object-store keys, manifest fields, WAL/SST/frame format changes.
- Any CAS, lease, fencing, GC, or queue behavior change.
- HTTP/CLI contract changes.
- Query semantics, scoring semantics, or limit/default changes.
- Dependency additions and why they are worth their cost.

Small refactors, spelling fixes, and narrow test additions usually do not need a
new decision entry.

## Documentation Checklist

Update docs based on the surface area touched:

| Change | Docs to check |
|---|---|
| User-facing feature, CLI, route, or example | [`README.md`](README.md), [`docs/guide.md`](docs/guide.md), examples |
| Architecture or invariant | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/PROGRESS.md`](docs/PROGRESS.md) |
| Known unresolved risk | [`docs/TODO.md`](docs/TODO.md) |
| Deployment, roles, probes, S3/MinIO | [`docs/guide.md`](docs/guide.md), [`docs/kubernetes-roles.yaml`](docs/kubernetes-roles.yaml), [`docker-compose.yml`](docker-compose.yml) |
| Performance result | [`docs/benchmarks.md`](docs/benchmarks.md) |
| Conceptual tutorial change | `docs/book/` |
| Public project-page copy | [`docs/index.html`](docs/index.html) |

When docs disagree, fix the current-state docs first:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/guide.md`](docs/guide.md). Then update the historical decision log only
where it needs a new decision or a superseded note.

## Code Principles

- Keep the object-store boundary small. Durable coordination should flow through
  `ObjectStore` primitives: `put_if_absent`, `compare_and_set`, exact-key reads,
  and explicit lists only for maintenance/recovery.
- Do not add a hot-path dependency on `list`. Query paths should read manifests
  and exact object keys.
- Publish catalog-last: write immutable objects first, then CAS the mutable
  pointer that makes them reachable.
- Treat content-addressed immutable objects as immutable. A key collision must
  verify bytes, not silently overwrite.
- Mutable objects such as manifest pointers, WAL commit state, queue state, and
  pinning state must bypass the immutable-object cache.
- Preserve deterministic serialization. Prefer `BTreeMap` for persisted maps and
  stable ordering in golden tests.
- Keep read snapshots coherent: query code should capture the manifest and
  committed WAL cursor, then read the overlay for that snapshot.
- Do not hide corruption or lossy conversions. Return an explicit `Error` rather
  than silently changing data.
- Add abstractions only when they match an existing boundary or remove real
  duplication.

## Tests And Checks

Run the narrow test first, then the broader suite before pushing.

Required for most changes:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets
```

Use the default lint groups for the current Rust toolchain, the stricter
`[lints.clippy]` warnings in [`Cargo.toml`](Cargo.toml), and the repo's local
[`clippy.toml`](clippy.toml) thresholds. `clippy.toml` tunes configurable lint
behavior; it does not enable or deny lint groups. If a lint must be allowed,
keep the `#[allow(...)]` local to the smallest item and make the reason obvious
from the code.

Formatting is controlled by [`rustfmt.toml`](rustfmt.toml), including
`style_edition = "2024"`.

Additional checks when relevant:

```sh
# S3/MinIO backend
docker compose up -d
export AWS_ACCESS_KEY_ID=sana AWS_SECRET_ACCESS_KEY=sana-secret
export SANA_S3_ENDPOINT=http://127.0.0.1:9000 SANA_S3_PATH_STYLE=1
SANA_S3_TEST_ENDPOINT=$SANA_S3_ENDPOINT cargo test --test s3_object_store

# Examples
cargo run --release --example usage
cargo run --release --example hybrid
cargo run --release --example conditional

# Benchmark harness, only when performance or metrics changed
cargo run --release --example latency
```

If you change Kubernetes or Compose YAML, at least run:

```sh
docker compose config
```

If you cannot run an expected check, say so in the final note or PR description.

## Serialization And Golden Fixtures

Files under `tests/fixtures/` are compatibility fixtures. Treat updates to them
as storage-format changes unless proven otherwise.

- Add or update fixtures only intentionally.
- Explain the compatibility impact in `docs/PROGRESS.md`.
- Keep old-format read tests when older data should remain readable.
- Prefer additive formats and explicit format versions over inferred migrations.

## Commit Style

Use small, reviewable commits. A good commit has one reason to exist.

Preferred commit message style:

```text
add readiness drain lifecycle
fix f16 json round trip
document deployment TODOs
```

Guidelines:

- Use an imperative, concise subject line.
- Keep unrelated changes out of the commit.
- Do not commit generated caches, local data, or editor artifacts.
- Mention important verification in the commit body when the change is risky.
- If a commit changes durable behavior, include the matching docs/decision update
  in the same commit.

## Review Checklist

Before asking for review or pushing:

- Did you run the task loop and the repository review in
  [`docs/CODE_REVIEW.md`](docs/CODE_REVIEW.md)?
- Does the change preserve the object-store invariants?
- Is every new publisher fenced or otherwise safe under retries?
- Does any turbopuffer-inspired subsystem follow the checked-in turbopuffer
  shape, or document the intentional deviation?
- Are readers safe across concurrent writes, compaction, and maintenance?
- Are error cases explicit and tested?
- Are query/API limits bounded by default?
- Are docs and examples still truthful?
- Did you run the relevant checks and record any skipped ones?
