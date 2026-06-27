# Sana RFCs

Design proposals for substantial, not-yet-implemented work. An RFC lays out the
problem, Sana's current state, turbopuffer fidelity and prior art, a proposed
design, alternatives, a testing plan, and open questions — enough for a human or
an agent (e.g. Codex) to review, push back on, or implement from.

These are **proposals, not current behavior**. `docs/ARCHITECTURE.md` remains the
source of truth for what Sana does today; `docs/PROGRESS.md` records decisions
once they are implemented. An RFC graduates by being implemented and recording
its decisions in `docs/PROGRESS.md` (next id is D87), then flipping the relevant
`docs/TODO.md` checkboxes.

## Index

| RFC | Title | Status | TODO section |
| --- | --- | --- | --- |
| [0001](0001-namespace-drop-lifecycle.md) | Guarded namespace drop lifecycle | Draft | P1: namespace drop |
| [0002](0002-randomized-object-store-adversary-tests.md) | Randomized object-store adversary tests | Draft | P1: adversary tests |
| [0003](0003-incremental-compaction-planning.md) | Incremental, byte-aware compaction planning | Draft | P2: compaction write spikes |

## Reviewing

- Each RFC has numbered open questions (`Q1`, …) and risks (`R1`, …). Reply to
  those by number.
- Check proposals against the non-negotiable invariants in the project
  `CLAUDE.md` and against the checked-in turbopuffer material under
  `sources/turbopuffer-export/`.
- RFC 0001 and 0002 are coupled (drop needs crash/branch fault tests); 0002 and
  0003 are coupled (the property harness is how compaction planning is verified).

## Template

New RFCs follow the structure of the existing three: Summary, Motivation,
Current state in Sana, Design, Alternatives, Testing plan, Risks/open questions,
References. Number them sequentially (`NNNN-kebab-title.md`).
