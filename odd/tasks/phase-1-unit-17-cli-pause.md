# Phase 1 Unit 17 — CLI pause

## Objective

Expose the existing daemon control-socket pause operation through the
`xwindowlog pause` CLI command, preserving the daemon as the sole SQLite
writer and the RF-60 stream/exit-code contract.

## Problem

Phase 15/13 already provides the daemon control channel and the thin
`control::send_pause` client, but `pause` is still dispatched as an
unimplemented subcommand. Users cannot request the already-implemented pause
state transition from the CLI.

## Why

Phase 17 is the next planned implementation area after the completed Unit 16
reports. Starting with the smallest coherent slice keeps the control protocol
and its tests reviewable before adding resume, destructive commands, and
completion generation.

## Scope

### Authorized files

- `src/main.rs` — dispatch `Command::Pause` through the control-socket client.
- `tests/cli_control.rs` — real-binary integration coverage for pause.
- `odd/tasks/phase-1-unit-17-cli-pause.md` — this task ledger and evidence.

### Explicitly out of scope

- `resume`, `pause --minutes`, `prune`, `forget`, completions, and man-page
  generation.
- Direct SQLite writes from the CLI.
- Remote operations, merge, pull request creation, or unrelated refactors.

## Constraints and decisions

- The daemon remains the only writer of interval state.
- Reuse `control::send_pause`; do not duplicate the wire protocol in the CLI.
- Resolve the socket using the existing `$XDG_RUNTIME_DIR/xwindowlog.sock`
  convention.
- Preserve existing plain-text commands and map failures through the central
  RF-60 exit-code policy.
- Technical artifacts remain in English.

## Route

- **Implementation route:** delegated direct.
- **Mapping evidence:** roadmap understanding required the PRD, OpenSpec task
  ledger, existing ODD ledgers, and multiple Rust modules; a read-only mapper
  identified Phase 17 pause as the next pending slice.
- **Writer evidence:** the slice changes two non-trivial files, so one bounded
  writer is required; no parallel writers.

## Testing mode

- Strict TDD: enabled by project policy.
- Runner: `cargo test`.
- Required order: observed RED, GREEN, then REFACTOR/checks.

## Checklist

- [x] U17.1 RED: add a real-binary CLI test proving `pause` reaches a running
      daemon and records the paused state without a direct SQLite write.
      Observed RED: the placeholder dispatch exited `1` with the expected
      unimplemented-subcommand diagnostic.
- [x] U17.2 GREEN: dispatch `pause` to `control::send_pause`, map the response,
      and preserve RF-60 stdout/stderr behavior. Observed GREEN: the real
      binary pause test passed and the daemon recorded the paused interval.
- [x] U17.3 REFACTOR: keep the command path bounded and reuse existing control
      and exit-code helpers without duplicating protocol logic.

## Acceptance criteria

1. `xwindowlog pause` succeeds against a running daemon.
2. The daemon closes the current interval and opens `paused`.
3. The CLI does not open or mutate SQLite.
4. Primary output is on stdout, diagnostics are on stderr, and success exits
   `0`.
5. Focused and full applicable checks pass.

## Verification commands

```text
cargo test --test cli_control
cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Progress

- Status: implemented and reviewed; delivery closure pending.
- Work-unit commit: `b35a1ee feat(cli): expose pause control command`.
- Verification evidence: `cargo test --test cli_control`,
  `cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store`,
  `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt -- --check` all passed.
- Native review: lineage `review-eb1f892defd74369` approved and acknowledged;
  no blockers. Three non-blocking follow-ups remain: make the SQLite observer
  read-only, add failure-path coverage for the new CLI mapping, and clean up
  the Xvfb readiness-timeout child.
- Parent spot-check: `cargo test --test cli_control` passed after review.
- Next step: deliver this coherent pause slice; begin the next Phase 17
  command as a separate work unit afterward.
