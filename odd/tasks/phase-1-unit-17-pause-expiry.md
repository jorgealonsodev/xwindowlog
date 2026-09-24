# Phase 1 Unit 17 — Pause expiry and `--minutes`

## Objective

Implement the wall-anchored, monotonic-waiting pause expiry path and wire
`xwindowlog pause --minutes N` to it without changing the daemon-only SQLite
writer boundary.

## Problem

The control protocol already computes a wall-clock target for `Pause { minutes
}` and the daemon arms a monotonic timer from the initial wall delta. The
target is then discarded. During a suspend, monotonic elapsed time excludes
the suspended interval while wall time advances, so a nominal 30-minute pause
can last roughly 50 minutes. The CLI also currently leaves `--minutes` on the
generic unimplemented path.

## Why

RF-49 requires `--minutes` to expire at the requested real-time target,
including across suspend. This slice must correct the dual-clock contract
before exposing the option; otherwise the CLI would advertise behavior the
daemon cannot reliably provide.

## Scope

### Authorized files

- `src/main.rs` — forward `minutes`, retain the wall-target refresh hook in
  the event loop, and remove unchecked/saturating expiry arithmetic.
- `src/reactor.rs` — retain the pause wall target, refresh the monotonic wait
  from the remaining wall duration, and preempt an already-expired target.
- `src/tracker.rs` — retain the pause target and close automatic expiry at the
  target while preserving manual resume-at-now behavior.
- `tests/cli_control.rs` — real-binary `pause --minutes` forwarding/expiry
  coverage.
- `odd/tasks/phase-1-unit-17-pause-expiry.md` — this task ledger and evidence.

### Explicitly out of scope

- Changes to the control protocol wire format, SQLite schema, or restart
  persistence for pause deadlines.
- Changing paused-across-suspend state semantics to `locked`; the current
  behavior remains paused because RF-49's explicit wall target is the chosen
  contract for this slice.
- State-error/no-daemon CLI mapping, `prune`, `forget`, completions, man-page
  generation, and the prior pause/resume review follow-ups.
- Remote operations, merge, pull request creation, or unrelated refactors.

## Constraints and decisions

- Compare wall timestamps to determine remaining duration; use monotonic time
  only for `poll(2)` waiting. Never convert a wall timestamp directly into a
  monotonic instant.
- Retain the target in memory only. The daemon remains the sole writer of
  persisted interval boundaries as `WallTs` values.
- A late automatic expiry closes at the retained wall target, while a manual
  `resume` continues to close at the actual resume time.
- Wall-clock corrections intentionally affect expiry because RF-49 defines a
  wall-anchored target; document and test the behavior.
- Preserve checked arithmetic and fail safely if a monotonic deadline cannot
  be represented.
- Technical artifacts remain in English.

## Route

- **Implementation route:** delegated direct.
- **Mapping evidence:** CodeGraph plus a read-only audit traced the control,
  reactor, tracker, clock, and integration-test paths and confirmed the
  suspended-expiry defect is real, latent only because `--minutes` is not yet
  dispatched.
- **Branch boundary:** `phase-1-unit-17-pause-expiry`, branched from the
  local resume work unit.

## Testing mode

- Strict TDD: enabled by project policy.
- Runner: `cargo test`.
- Required order: observed RED, GREEN, then REFACTOR/checks.

## Checklist

- [x] U17.3 RED: prove a late `PauseExpiry` closes at the retained wall target,
      not delivery time; prove a wall-only suspend shortens the remaining
      monotonic wait; prove an already-expired target is emitted before the
      post-suspend event; and prove `pause --minutes` is forwarded by the real
      CLI. Observed RED: all four focused cases failed before implementation.
- [x] U17.4 GREEN: retain and refresh the dual-clock pause target, preserve
      the automatic/manual close distinction, and dispatch the CLI minutes
      value through the existing control client. Observed GREEN: all targeted
      tests passed.
- [x] U17.5 REFACTOR: keep checked wall arithmetic and timer ownership local to
      the reactor/event-loop boundaries, with no protocol or storage coupling.

## Acceptance criteria

1. `xwindowlog pause --minutes 30` reaches the daemon with `minutes: 30`.
2. A pause expires at its wall-clock target even when suspend advances wall
   time without advancing the monotonic wait.
3. Automatic expiry closes at the requested target; manual resume closes at
   the actual resume instant.
4. The CLI does not open or mutate SQLite, and existing pause/resume behavior
   remains green.
5. Focused and full applicable checks pass.

## Verification commands

```text
cargo test --lib tracker::
cargo test --lib reactor::
cargo test --test cli_control
cargo test --test pipeline_integration
cargo test --test daemon_e2e -- control_socket_pause_then_resume_reaches_the_store
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Progress

- Status: implemented; delivery closure pending.
- Work-unit commit: `c2b2af2 fix(daemon): anchor pause expiry to wall time`.
- Verification evidence: tracker tests (23 passed), reactor tests (31 passed),
  CLI control tests (3 passed), pipeline integration (2 passed), daemon E2E
  filter (1 passed), and all targets (308 passed). Clippy with `-D warnings`
  and format check passed; no environmental failures occurred.
- Next step: complete the native risk assessment/review decision and deliver
  this coherent expiry slice; begin the next Phase 17 command afterward.
