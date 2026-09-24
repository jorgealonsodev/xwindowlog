# Phase 1 Unit 17 — CLI forget

## Objective

Expose the existing destructive forget operations through the real CLI for
exactly one selector: either a paired `--from`/`--to` RFC3339 range or a
`--window <id>`, with safe confirmation, existing error mappings, and no new
database-writing boundary in `src/main.rs`.

## Problem

`Store::forget_range` and `Store::forget_window` already provide the storage
operations, but the CLI composition layer must validate selectors, protect
destructive execution, preserve output-stream and RF-60 behavior, and report
vacuum exhaustion without duplicating SQL or changing Store semantics.

## Authorized files

- `src/main.rs`
- `tests/cli_control.rs`
- `odd/tasks/phase-1-unit-17-cli-forget.md`

## Explicit exclusions

- Do not modify `src/store.rs`, `src/config.rs`, `src/control.rs`, Cargo files,
  OpenSpec source files, or remote state.
- Do not change Store behavior around deleting an open interval.
- Do not add direct SQL to `src/main.rs`.
- Do not change unrelated pause, resume, prune, or daemon behavior.
- Do not push, merge, or create a pull request.

## Delivery constraints

- Branch: `phase-1-unit-17-pause-expiry`.
- Strict-TDD mode is active: observe RED, implement the smallest GREEN change,
  then refactor and run the complete checks.
- Use one coherent work-unit commit with a Conventional Commit message.
- Keep prompts and diagnostics on stderr; keep success output on stdout.
- Prompt for both range and window deletion unless `--yes` is supplied.
- Accept only `y` or `yes`, case-insensitively. Blank input, EOF, and every
  other input cancel without calling either destructive Store API.
- Do not print stored titles or other sensitive data in the prompt.

## Acceptance criteria

- [x] Dispatch `Command::Forget` through the existing `open_report_store`,
      `ReportError`, deletion-outcome reporting, RF-60 mappings, and
      vacuum-exhausted behavior.
- [x] Require exactly one selector: paired `--from` and `--to`, or
      `--window <id>`; reject mixed, incomplete, or absent selectors.
- [x] The original CLI slice parsed whole-second RFC3339/ISO timestamps into
      `WallTs` and rejected non-zero fractions, malformed, equal, and reversed
      ranges before Store access. The fraction rejection was a historical
      behavior and is superseded by `phase-1-unit-17-forget-precision.md`;
      malformed, equal, and reversed ranges remain invalid.
- [x] Require confirmation for both selector forms unless `--yes` is present;
      cancellation must perform no destructive Store call.
- [x] Cover range and window deletion, selector validation, confirmation,
      output streams/status, affirmative execution, cancellation, and
      preservation of non-selected rows with real-binary tests using existing
      Store/test helpers.
- [x] Leave Store behavior for open-interval deletion unchanged.

## Verification commands

1. `cargo test --test cli_control -- forget` (RED before implementation, then
   GREEN after implementation).
2. `cargo test --lib store::`
3. `cargo test --all-targets`
4. `cargo clippy --all-targets -- -D warnings`
5. `cargo fmt -- --check`

## Route evidence

The intended route is confined to `src/main.rs`: validate the selected CLI
arguments and timestamps before `open_report_store`; prompt on stderr unless
`--yes`; call only `Store::forget_range` or `Store::forget_window`; then reuse
the existing deletion outcome, `ReportError`, RF-60, and vacuum-exhausted
reporting paths. Tests will seed and inspect fixtures through existing Store
and test helpers rather than production-only SQL.

## Confirmation semantics

Both range and window selectors are destructive forms and therefore require
confirmation when `--yes` is absent. Only case-insensitive `y` or `yes`
confirms. EOF, blank input, and any other response cancel without invoking
either destructive Store API. `--yes` bypasses the prompt. Prompts do not
include stored titles or other sensitive data.

## TDD evidence

### RED

Command: `cargo test --test cli_control -- forget`

Observed result: **RED** — test compilation succeeded, then all 8 new
`forget_*` tests failed against the existing generic `not_yet_implemented`
dispatch (`0 passed; 8 failed; 14 filtered out`). Selector cases received
`xwindowlog: this subcommand's behavior lands in Phase 16/17` instead of
validation diagnostics, and execution cases returned status `1` instead of
the expected destructive-command or cancellation result. This is the
intended pre-implementation failure boundary.

### GREEN

Command: `cargo test --test cli_control -- forget`

Observed result: **GREEN** — `8 passed, 14 filtered out`.

The implementation validates before opening the report store, prompts on
stderr for both selectors, accepts only case-insensitive `y`/`yes`, maps
cancellation to a no-op success, and routes confirmed deletion through the
existing Store APIs and outcome reporter.

### Checks

Recorded results:

- `cargo test --lib store::` — `27 passed, 170 filtered out`.
- `cargo test --all-targets` — `328 passed` across 11 suites.
- `cargo clippy --all-targets -- -D warnings` — no issues found.
- `cargo fmt -- --check` — passed with no diff.

### Historical correction note — R3-fractional-forget-boundary

The correction recorded in this historical lineage rejected non-zero
fractional nanoseconds with a usage diagnostic before Store access; its
real-binary test was RED against the pre-correction binary (status `Some(0)`,
expected `Some(1)`), then GREEN with `9 passed, 14 filtered out`. That
whole-second-only behavior is superseded by the new precision-preserving ODD
work unit in `phase-1-unit-17-forget-precision.md`, which accepts valid
fractional RFC3339 input and maps it exactly to whole-second Store bounds.

Historical review status is unchanged: native review transaction
`review-0378350da479c28d` ended escalated after rejecting a correction, and
this lineage remains escalated/unapproved. This note and the separate new work
unit do not approve or reuse that review transaction. The earlier test and
check results above record what was actually verified at that historical
point; they are not current acceptance criteria for fractional input.

### Precision-preserving successor verification

The separate bounded work unit `phase-1-unit-17-forget-precision.md` replaces
that fraction-rejection behavior at the CLI boundary without changing Store
semantics.

- RED on `ff218b5`: `cargo test --test cli_control -- forget` compiled and ran;
  `8 passed, 2 failed, 14 filtered out`. Both new fractional-range tests
  observed status `Some(1)` instead of `Some(0)` against the old rejection.
- GREEN: the same command completed with `10 passed, 14 filtered out`. For
  `[1000.5, 1100.5)`, only `[1000,1001)` and `[1100,1101)` were deleted; the
  adjacent `[900,1000)` and `[1101,1102)` rows survived. The pre-epoch
  `[-0.5, 0.5)` scenario also deleted exactly its two overlapping rows.
- `cargo test --lib store::` — `27 passed, 170 filtered out`.
- `cargo test --all-targets` — `330 passed` across 11 suites.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, and
  `git diff --check` — passed.
- Successor work-unit commit: `9d50a2bca85593d4c5bf46258a46352e29cffb0e`
  (`fix(cli): preserve fractional forget precision`).
- Its Engram task-ledger mirror is pending because Engram could not confirm
  session registration; the local ledger is preserved.

The successor work does not change the historical review result: transaction
`review-0378350da479c28d` remains escalated and unapproved.

## Changed files

- `src/main.rs` — dispatches `Command::Forget`, validates exclusive selectors
  and RFC3339 timestamps before store access, confirms destructive execution,
  and reuses Store/RF-60/outcome reporting.
- `tests/cli_control.rs` — adds eight real-binary tests plus Store-backed
  fixtures for deletion, preservation, validation, confirmation, cancellation,
  output streams, and exit status.
- `odd/tasks/phase-1-unit-17-cli-forget.md` — records scope and TDD evidence.

## Work-unit commit

`25c5b12d0288745295482bcedea710e7eab8a736 feat(cli): wire forget deletion
command`

## Review-size risk

The cohesive implementation/test/ledger commit contains 579 changed lines
(571 additions and 8 deletions, including this 147-line ledger), over the
default 400-line review budget. This is recorded as a size exception because
splitting the selector behavior, destructive routing, and its real-binary proof
would leave an incomplete work unit; no unrelated files are included.

## Unresolved issues

None known after the required checks. The existing Store tests remain the
evidence for the typed exhausted-VACUUM path; this CLI slice does not alter
Store behavior or add a slow process-level contention fixture.
