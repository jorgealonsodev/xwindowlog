# Phase 1 Unit 17 — Preserve CLI forget timestamp precision

## Objective

Accept valid RFC3339 timestamps with fractional seconds for `forget --from/--to`
without widening the destructive selection beyond the requested half-open
range. Preserve the existing integer-second `WallTs` and
`Store::forget_range` boundary.

## Scope and exclusions

Authorized paths for this bounded work unit:

- `src/main.rs`
- `tests/cli_control.rs`
- `odd/tasks/phase-1-unit-17-cli-forget.md`
- `odd/tasks/phase-1-unit-17-forget-precision.md`

Do not modify Store semantics or any other production/test paths. Do not push,
merge, create a PR, or run Gentle AI review commands. The parent context owns
the new native assessment, consent, and review lifecycle.

## Route and boundary mapping

Parse each input as an `OffsetDateTime` and compare the exact instants before
opening the Store. Map the requested interval `[from, to)` to the existing
integer-second Store API as follows:

| Bound | Store value | Reason under `start < to && end > from` |
|-------|-------------|----------------------------------------|
| `from` | `floor(from)` (`unix_timestamp()`) | For integer `end`, `end > floor(from)` selects exactly the values satisfying `end > from`, including when `from` is fractional. |
| `to` | `floor(to)` when integral; otherwise checked `floor(to) + 1` | For integer `start`, `start < ceil(to)` selects exactly the values satisfying `start < to`. |

`unix_timestamp()` is the floor-second boundary, including before the Unix
epoch. Use checked arithmetic for the fractional upper-bound adjustment and
preserve the existing error path if it overflows. Pass only the resulting
whole-second `WallTs` values to `Store::forget_range`; do not truncate both
bounds or change the Store query.

## Decisions

- Parse and order-compare full `OffsetDateTime` values so a reversed fractional
  range cannot appear valid merely because its rounded Store bounds overlap.
- Keep the destructive selection exact for integer-second interval endpoints;
  the Store deletes whole rows that overlap the requested range.
- Cover a pre-epoch fractional case if the real-binary fixture can express it
  without adding production or Store changes.
- Replace the prior test that expected valid fractional input to be rejected;
  valid fractions are accepted in this work unit.

## Strict TDD plan

1. Add/replace focused real-binary tests first, seeding and reading whole-second
   rows through the existing Store-backed fixture.
2. Run `cargo test --test cli_control -- forget` against the current
   whole-second-only behavior and record the observed RED.
3. Implement the smallest CLI-boundary change in `src/main.rs`; do not modify
   `src/store.rs`.
4. Re-run the focused tests for GREEN, then execute the complete verification
   suite below.

## Acceptance tests

- A range `[1000.5, 1100.5)` deletes `[1000,1001)` and `[1100,1101)` while
  preserving `[900,1000)` and `[1101,1102)`.
- Valid fractional RFC3339 input succeeds and invokes the existing destructive
  route after confirmation/`--yes`; it is not rejected for its fraction.
- A pre-epoch fractional boundary is covered if practical, proving floor and
  ceiling behavior around negative Unix timestamps.
- Existing whole-second selector, confirmation, output, and RF-60 behavior
  remains intact.
- No row outside the true requested interval is deleted, and no Store semantic
  change is needed.

## Verification

- `cargo test --test cli_control -- forget` — RED: `8 passed, 2 failed,
  14 filtered out`; GREEN: `10 passed, 14 filtered out`.
- `cargo test --lib store::` — `27 passed, 170 filtered out`.
- `cargo test --all-targets` — `330 passed` across 11 suites.
- `cargo clippy --all-targets -- -D warnings` — passed; no issues found.
- `cargo fmt -- --check` — passed with no output.
- `git diff --check` — passed with no output.

Runtime harness: `cargo test --test cli_control -- forget` runs the compiled
CLI binary against Store-seeded whole-second rows. GREEN result: `10 passed,
14 filtered out`, including the exact positive-epoch and pre-epoch scenarios.

### RED

Command: `cargo test --test cli_control -- forget` on `ff218b5`.

Observed result: **RED** — the tests compiled and ran; `8 passed, 2 failed,
14 filtered out`. The two new fractional-range tests expected status `Some(0)`
but got `Some(1)`, confirming the current CLI rejects both valid fractional
ranges before deletion. The tests assert exact row preservation/deletion and
include the pre-epoch case.

### GREEN

Command: `cargo test --test cli_control -- forget`.

Observed result: **GREEN** — `10 passed, 14 filtered out`. Both the positive
epoch `[1000.5, 1100.5)` fixture and the pre-epoch `[-0.5, 0.5)` fixture
deleted exactly the two overlapping whole-second rows and preserved both
outside rows. The implementation stayed in the CLI boundary; `src/store.rs`
was not changed.

## Prior review-escalation context

This is a new bounded ODD work unit. Do not continue or reuse native review
transaction `review-0378350da479c28d`; it ended escalated after rejecting a
correction. That historical lineage remains escalated and unapproved. This
precision-preserving implementation supersedes the rejected whole-second-only
behavior; it does not retroactively approve or alter the old review result.

## Delivery record

- Branch: `phase-1-unit-17-pause-expiry`.
- Work-unit commit: one Conventional Commit, subject
  `fix(cli): preserve fractional forget precision`; full object ID is included
  in the completion record.
- Rollback boundary: revert that one commit to remove the CLI mapping, its
  real-binary tests, and the two associated task-ledger updates; no Store
  semantics or unrelated behavior are included.
- Engram mirror topic: `odd/phase-1-unit-17-forget-precision/tasks`.
- Engram mirror status: pending; the initial `mem_save` could not confirm
  Engram session registration. The runtime supplied no authoritative session
  ID, so the local ledger remains the current copy.
- Unresolved issues: Engram mirror remains pending because the server could not
  confirm session registration and no authoritative runtime session ID was
  supplied. No functional issues remain after the required checks.
