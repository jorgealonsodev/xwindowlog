# Phase 1 Unit 17 — CLI resume

## Objective

Expose the existing daemon control-socket resume operation through the
`xwindowlog resume` CLI command, preserving daemon-only interval writes and the
RF-60 stream/exit-code contract.

## Problem

The daemon control protocol already implements `Resume`, and the CLI argument
parser recognizes the subcommand, but dispatch still falls through to the
generic unimplemented path. A user can pause through the CLI but cannot end
that pause through the corresponding CLI command.

## Why

Resume is the smallest coherent next Phase 17 slice after the delivered pause
command: one existing client call, one CLI dispatch, and one real-binary
integration proof. Pause expiry is deliberately separate because its suspended
dual-clock behavior needs its own correction and evidence.

## Scope

### Authorized files

- `src/main.rs` — dispatch `Command::Resume` through the control-socket client.
- `tests/cli_control.rs` — real-binary integration coverage for resume.
- `odd/tasks/phase-1-unit-17-cli-resume.md` — this task ledger and evidence.

### Explicitly out of scope

- `pause --minutes`, pause-expiry/suspend correction, state-error coverage,
  no-daemon handling, `prune`, `forget`, completions, and man-page generation.
- Changes to `control.rs`, `reactor.rs`, `tracker.rs`, `clock.rs`, or the
  delivered pause review follow-ups.
- Direct SQLite writes from the CLI.
- Remote operations, merge, pull request creation, or unrelated refactors.

## Constraints and decisions

- The daemon remains the only writer of interval state.
- Reuse `control::send_resume`; do not duplicate the wire protocol in the CLI.
- Resolve the socket using the existing `$XDG_RUNTIME_DIR/xwindowlog.sock`
  convention.
- Preserve existing plain-text commands and map failures through the central
  RF-60 exit-code policy.
- Technical artifacts remain in English.

## Route

- **Implementation route:** delegated direct.
- **Mapping evidence:** CodeGraph and a read-only roadmap mapper confirmed that
  resume is parsed but undispatched, while daemon-side resume support already
  exists; the slice changes two non-trivial implementation/test files.
- **Branch boundary:** `phase-1-unit-17-resume`, branched from delivered
  `rf-32-recovery-correction` at `f43e47a`.

## Testing mode

- Strict TDD: enabled by project policy.
- Runner: `cargo test`.
- Required order: observed RED, GREEN, then REFACTOR/checks.

## Checklist

- [x] U17.5 RED: add a real-binary CLI test proving `resume` reaches a paused
      daemon, closes the paused interval, and returns to the daemon's current
      tracking state without a direct SQLite write. Observed RED: the resume
      test failed through the generic unimplemented-subcommand path.
- [x] U17.6 GREEN: dispatch `resume` to `control::send_resume`, map the
      response, and preserve RF-60 stdout/stderr behavior. Observed GREEN: the
      focused resume test passed.
- [x] U17.7 REFACTOR: keep the command path bounded and reuse existing control
      and exit-code helpers without duplicating protocol logic.

## Acceptance criteria

1. `xwindowlog resume` succeeds against a paused running daemon.
2. The daemon closes the open `paused` interval and records the resumed state
   transition according to the existing tracker behavior.
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

- Status: implemented; delivery closure pending.
- Work-unit commit: `cd105a6 feat(cli): expose resume control command`.
- Verification evidence: `cargo test --test cli_control` (2 passed),
  `cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store`
  passed, `cargo test --all-targets` (304 passed),
  `cargo clippy --all-targets -- -D warnings` passed, and
  `cargo fmt -- --check` passed.
- Native risk assessment: medium executable change; `review_due=false` with
  reason `under_budget` for the committed-only slice from `f43e47a`.
- Parent spot-check: `cargo test --test cli_control` passed after assessment.
- Next step: deliver this coherent resume slice; begin pause expiry as a
  separate work unit afterward.
