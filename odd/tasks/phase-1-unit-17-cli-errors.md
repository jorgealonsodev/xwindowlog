# Phase 1 Unit 17 — CLI control error paths

## Objective

Close the pause/resume control-command error paths so state errors and an
unavailable daemon are observable through the real CLI without weakening the
daemon-only SQLite writer boundary.

## Problem

`run_pause` and `run_resume` already contain the intended RF-60 mappings for
`AlreadyPaused` and `NotPaused`, but the real-binary CLI suite does not prove
those paths. The control client also applies its I/O deadlines only after
`UnixStream::connect`, so a reachable-but-unresponsive socket path can leave
the CLI connect phase unbounded.

## Why

The next Phase 17 slice should close the control-command contract before adding
destructive CLI operations. Users need deterministic state errors and a
bounded failure when the daemon is absent or cannot accept a connection.

## Scope

### Authorized files

- `src/main.rs` — preserve or complete the central pause/resume error mapping
  and no-daemon diagnostic/exit behavior.
- `src/control.rs` — bound the client connection attempt using the existing
  control timeout policy; do not change the wire format.
- `tests/cli_control.rs` — real-binary coverage for already-paused,
  not-paused, and no-daemon pause/resume behavior.
- `odd/tasks/phase-1-unit-17-cli-errors.md` — this task ledger and evidence.

### Explicitly out of scope

- Changes to `src/reactor.rs` or control-response acknowledgement ordering.
- `prune`, `forget`, completions, man-page generation, and pause-expiry
  follow-ups.
- Changes to the control protocol wire format, SQLite schema, or store logic.
- Remote operations, merge, pull request creation, or unrelated refactors.

## Constraints and decisions

- Keep `AlreadyPaused` and `NotPaused` as RF-60 state errors with exit code 2.
- Keep a missing/unreachable daemon in the existing environment-error policy
  with exit code 3; the lifecycle requirement is non-zero and the repository's
  central startup/error policy already assigns environment failures to 3.
- Bound connection establishment with the existing one-second control-client
  deadline; preserve the already-bounded read/write behavior.
- Keep primary command output on stdout and diagnostics on stderr.
- The daemon remains the sole writer of interval state.
- Technical artifacts remain in English.

## Route

- **Implementation route:** delegated direct.
- **Mapping evidence:** CodeGraph plus a read-only mapper traced `main.rs`,
  `control.rs`, `tests/cli_control.rs`, the Phase 17 roadmap, and existing
  pause/resume ledgers. The implementation spans three non-trivial files.
- **Branch boundary:** `phase-1-unit-17-pause-expiry`, continuing after the
  acknowledged pause-expiry work unit.

## Testing mode

- Strict TDD: enabled by project policy.
- Runner: `cargo test`.
- Required order: observed RED, GREEN, then REFACTOR/checks.

## Checklist

- [x] U17.7 RED: prove pause while already paused and resume while active exit
      2 with the matching daemon diagnostic. Added both real-binary tests;
      their pre-implementation focused runs were already GREEN because the
      RF-60 mappings were present.
- [x] U17.8 GREEN: preserved the central `AlreadyPaused`/`NotPaused` mapping
      and proved the real CLI behavior with explicit stdout/stderr and exit
      assertions.
- [x] U17.9 RED: prove pause and resume with no daemon fail promptly and
      report a non-zero environment error rather than hanging or succeeding.
      Added both real-binary tests; their pre-implementation focused runs were
      already GREEN under the existing environment-error policy.
- [x] U17.10 GREEN: bound control-client connection establishment with the
      existing one-second deadline and preserved the read/write timeout
      behavior. The new connection regression test first failed after 1.06s
      because the unbounded stub returned `Ok(UnixStream)`, then passed after
      the deadline wrapper was implemented.
- [x] U17.11 REFACTOR: kept the timeout policy centralized in `CLIENT_DEADLINE`
      and avoided protocol, reactor, and storage coupling.

## Acceptance criteria

1. `pause` against an already-paused daemon exits 2 with the daemon's state
   diagnostic.
2. `resume` against an active daemon exits 2 with the daemon's state
   diagnostic.
3. `pause` and `resume` without a reachable daemon fail promptly, emit only
   diagnostics on stderr, and exit non-zero under the existing environment
   policy.
4. The CLI does not open or mutate SQLite.
5. Focused and full applicable checks pass.

## Verification commands

```text
cargo test --lib control::
cargo test --test cli_control
cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Progress

- Status: implementation complete; delivery is the single work-unit commit
  containing this ledger, the control-client fix, and the integration tests.
- Work-unit commit identity: `fix(cli): bound control connection and cover
  error paths` on `phase-1-unit-17-pause-expiry`; the exact commit hash is
  reported with delivery because the ledger is part of that same commit.
- RED evidence: the four required real-binary tests were added first and each
  passed against the existing mappings/policy. The missing connection bound was
  then isolated by `cargo test --lib control::`, which failed only
  `control::tests::cli_client_connect_times_out_before_io` with
  `Ok(UnixStream)` after 1.06s.
- GREEN/check evidence: `cargo test --lib control::` (12 passed),
  `cargo test --test cli_control` (7 passed),
  `cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store`
  (1 passed), `cargo test --all-targets` (313 passed),
  `cargo clippy --all-targets -- -D warnings` passed, and
  `cargo fmt -- --check` passed.
- `src/main.rs` required no code change: the existing RF-60 state mappings,
  no-daemon diagnostic, and exit-code 3 environment policy were verified by
  the real-binary tests.
- Native risk assessment: medium executable change, 284 changed lines, and
  `review_due=false` with reason `under_budget` for the committed-only slice
  from `23b10cd`; the reviewed boundary advanced without a review transaction.
- Next step: none within this authorized work unit; do not expand into the
  explicitly excluded follow-ups or remote operations.
