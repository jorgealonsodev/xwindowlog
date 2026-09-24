# Phase 1 Unit 17 — CLI prune

## Objective

Expose the existing store retention operations through `xwindowlog prune`,
including explicit retention windows, the configured default, and
`--vacuum-only`, while keeping all destructive database work outside the
daemon process.

## Problem

`Command::Prune` already describes the intended CLI surface and `DaemonConfig`
already supplies `retention_days`, but `main.rs` still routes the command to
the generic unimplemented path. The store already provides `prune()` and
`vacuum_only()` with durable delete/VACUUM outcome types and RF-60 mappings.

## Why

Retention is an explicit Phase 1 user operation. Wiring the existing store API
now completes the first destructive CLI path without mixing it into the
daemon's long-lived SQLite connection or broadening the store contract.

## Scope

### Authorized files

- `src/main.rs` — parse the supported retention duration, load config/data
  paths, open the store, dispatch `prune` or `vacuum_only`, and map outcomes to
  RF-60 streams and exit codes.
- `tests/cli_control.rs` — real-binary coverage for prune dispatch, explicit
  and configured retention windows, `--vacuum-only`, and failure output.
- `odd/tasks/phase-1-unit-17-cli-prune.md` — this task ledger and evidence.

### Explicitly out of scope

- `src/store.rs` changes, including the already identified WAL checkpoint
  outcome distinction for OpenSpec 17.23–17.24; that is a separate store
  work unit.
- `forget`, completions, man-page generation, the final CLI help/exit sweep,
  and unrelated CLI error-path or pause-expiry follow-ups.
- Automatic pruning, systemd units, remote operations, merge, push, or PR
  creation.

## Constraints and decisions

- Use `Store::prune(cutoff)` and `Store::vacuum_only()`; do not duplicate SQL
  or write through the daemon control socket.
- An explicit `--older-than` duration overrides `retention_days`; when omitted,
  use the configured value, where configured `0` disables deletion safely.
  Explicit `--older-than 0d` is rejected as an invalid prune request unless
  `--vacuum-only` is selected.
- Support the documented day form such as `180d` with checked arithmetic and
  clear rejection of malformed, zero, or overflowing explicit durations.
- Compute the cutoff from the current wall clock as a `WallTs`; never use a
  monotonic instant for persisted/database timestamps.
- Map `VacuumOutcome::Exhausted` to exit code 2 and emit
  `vacuum_exhausted_message` on stderr; successful primary output belongs on
  stdout and diagnostics on stderr.
- Preserve the daemon as a separate process and the sole writer during normal
  tracking; `prune` is intentionally a CLI-only store operation.
- Technical artifacts remain in English.

## Route

- **Implementation route:** delegated direct.
- **Mapping evidence:** CodeGraph and the Phase 17 specification confirm that
  `Store::prune`, `Store::vacuum_only`, `DeletionOutcome`, `VacuumOutcome`,
  `vacuum_exhausted_message`, and `DaemonConfig::retention_days` already exist;
  only CLI composition and real-binary proof are missing.
- **Branch boundary:** `phase-1-unit-17-pause-expiry`, after the acknowledged
  pause-expiry and under-budget control-error slices.

## Testing mode

- Strict TDD: enabled by project policy.
- Runner: `cargo test`.
- Required order: observed RED, then GREEN, then REFACTOR/checks.

## Checklist

- [x] U17.11 RED: real-binary tests compiled and failed against the existing
      generic dispatch: `cargo test --test cli_control -- prune` reported
      7 failed, 0 passed, with `xwindowlog: this subcommand's behavior lands
      in Phase 16/17` and exit code 1. The tests cover explicit retention,
      configured/default/zero retention, `--vacuum-only`, invalid input, and
      missing-database stream/exit behavior.
- [x] U17.12 GREEN: implemented the prune subcommand with explicit/configured
      cutoff selection, checked `d` duration parsing, wall-clock `WallTs`, the
      existing Store APIs, and RF-60 StoreError/VacuumOutcome mappings.
- [x] U17.13 REFACTOR: kept duration parsing, path resolution, and exit-code
      mapping local to CLI composition; no store or daemon logic changed.

## Acceptance criteria

1. `xwindowlog prune --older-than 180d` deletes eligible closed intervals,
   preserves the open interval/rules/projects, and runs the existing VACUUM
   path.
2. Omitting `--older-than` uses configured `retention_days`; `0` is handled
   safely rather than silently deleting everything.
3. `xwindowlog prune --vacuum-only` runs only the existing VACUUM retry path.
4. Exhausted VACUUM returns exit code 2 with the exact actionable store
   diagnostic, while store/config/database failures use deliberate RF-60 codes.
5. Invalid duration input fails before any destructive database operation.
6. Focused and full applicable checks pass.

## Verification commands

```text
cargo test --test cli_control -- prune
cargo test --lib store::
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Progress

- Status: implemented and delivered.
- Work-unit commit: `bfec3bb feat(cli): wire prune retention command`.
- Changed files: `src/main.rs`, `tests/cli_control.rs`, and this ledger.
- GREEN evidence: `cargo test --test cli_control -- prune` reported 7 passed,
  7 filtered out.
- Verification evidence: `cargo test --lib store::` reported 27 passed and
  170 filtered out; `cargo test --all-targets` reported 320 passed across 11
  suites; `cargo clippy --all-targets -- -D warnings` reported no issues; and
  `cargo fmt -- --check` passed after formatting the implementation.
- Outcome evidence: the existing store tests cover the typed exhausted-VACUUM
  outcome and exact actionable diagnostic; the real-binary CLI tests cover the
  success, invalid-input, missing-database, configured-zero, and vacuum-only
  stream/exit paths without introducing a slow process-level lock fixture.
- Important boundary: the implementation does not change `src/store.rs`, WAL
  checkpoint semantics, the daemon, `forget`, completions, man pages, the
  final help/exit sweep, systemd, or any remote/delivery operation.
- Native risk assessment: medium executable CLI change; the committed diff is
  537 lines including the 132-line task ledger, with the behavior/test slice
  just over the 400-line review budget. The overage is a single cohesive
  destructive-command boundary and is recorded as a size exception; no
  separate behavior slice would leave a complete prune work unit.
- Native review: lineage `review-8679b934f8ace8d2` approved and acknowledged;
  no correction was required. One non-blocking suggestion remains:
  `src/main.rs:238` lacks a real-binary exhausted-VACUUM fixture, while the
  existing store tests cover the typed outcome and exact diagnostic.
- Parent spot-check: `cargo test --test cli_control -- prune` reported 7
  passed and 7 filtered out after review acknowledgement.
- Next step: none within this authorized work unit; do not expand into the
  explicitly excluded follow-ups or remote operations.
