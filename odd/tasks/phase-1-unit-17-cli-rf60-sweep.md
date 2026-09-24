# Phase 1 Unit 17 — RF-60 CLI reporting sweep

## Objective

Close the remaining generic CLI-usage failure gap in the RF-60 reporting
contract, without changing command behavior that already reports through the
real binary.

## Scope

### Authorized files

- `src/main.rs` — change argument-error reporting only if the real-binary
  contract test proves the current behavior violates RF-60.
- `tests/cli_control.rs` and/or `tests/cli_status_today.rs` — add only missing
  real-binary regression coverage for CLI usage errors and successful help.
- `odd/tasks/phase-1-unit-17-cli-rf60-sweep.md` — this task ledger and evidence.

### OpenSpec tasks

- **17.15 RED:** complete the RF-60 stream/exit-code sweep. Existing coverage
  already exercises primary output and diagnostics for the applicable CLI
  paths, the second-daemon state error (2), and the no-X11 startup environment
  error (3). Add only the missing invalid/unknown-argument contract test.
- **17.16 GREEN:** correct only a subcommand/CLI behavior demonstrated by the
  sweep to violate its exit-code or stream contract. Preserve successful
  `--help` and `--version` output.

## Route

- **Implementation route:** direct, local, test-first.
- **Behavior path:** compiled `xwindowlog` binary → Clap parsing in
  `src/main.rs::main` → process exit status and stdout/stderr. No daemon or
  database setup is required for malformed arguments.
- **Boundary:** the mapper identified `Cli::parse()` as the probable source of
  Clap's default usage-error status 2. Confirm that hypothesis with the real
  binary before touching `src/main.rs`; leave the source unchanged if the test
  does not fail.
- **Existing evidence reused:** `tests/daemon_e2e.rs` covers second-daemon
  status 2 and no-X11 startup status 3; existing CLI integration tests cover
  pause/resume state and no-daemon paths, prune, forget, and command output.
  Do not duplicate those scenarios here.

## Testing mode

- Strict TDD: **enabled**; runner: `cargo test`.
- Required order: add the real-binary regression test, observe the specified
  RED with `cargo test --test cli_control -- cli_usage_errors_exit_one`, apply
  the smallest demonstrated fix, observe GREEN, then run the verification
  commands below.

## Acceptance criteria

1. An invalid or unknown CLI argument exits with RF-60 generic failure code 1,
   writes a Clap diagnostic to stderr, and writes no primary output to stdout.
2. `xwindowlog --help` remains a successful code-0 invocation with help text on
   stdout and no usage failure on stderr. Successful version output remains
   intact as well.
3. No source change is made unless the focused real-binary test observes the
   invalid-argument contract failing for the exit-code reason.
4. Existing RF-60 state/environment and command-output evidence is reused; no
   daemon, state, store, or command behavior is broadened.
5. All applicable focused and full checks pass; behavior, tests, and this
   ledger are included in one Conventional Commit. Record its exact hash in a
   documentation-only closure follow-up because the immutable hash is known
   only after that work-unit commit exists.

## Verification commands

```text
cargo test --test cli_control -- cli_usage_errors_exit_one
cargo test --test cli_control
cargo test --test cli_status_today
cargo test --test daemon_e2e -- second_daemon_instance_exits_nonzero_while_the_first_holds_the_lock
cargo test --test daemon_e2e -- daemon_exits_with_environment_status_when_the_first_x11_connect_fails
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

## Exclusions

- OpenSpec source files, daemon/control/store semantics, and broad RF-60
  scenario rewrites.
- Completions (17.17–17.18), man-page/build packaging (17.19), and subcommand
  help wiring (17.20–17.21).
- Remote/delivery state, push, merge, PR creation, and Gentle AI review
  lifecycle commands.

## Progress

- Status: the focused real-binary contract is green; all listed Cargo checks
  passed. Commit and final delivery closure remain.
- Engram mirror: pending — `mem_save` could not confirm session registration;
  no authoritative session ID was available to provide, so the mirror was not
  confirmed.
- **RED:** `cargo test --test cli_control -- cli_usage_errors_exit_one`
  compiled and failed at the contract assertion: observed child exit
  `Some(2)`, expected `Some(1)`. Child stderr contained Clap's `unexpected
  argument` message and usage text. This confirmed the parser gap before any
  source change.
- **GREEN:** after the parser fix, `cargo test --test cli_control --
  cli_usage_errors_exit_one` passed (1 passed); the help/version regression
  passed both before and after the fix (1 passed each run).
- **Verification:** `cargo test --test cli_control` (26 passed);
  `cargo test --test cli_status_today` (12 passed);
  the second-daemon and no-X11 `daemon_e2e` filters (1 passed each);
  `cargo test --all-targets` (332 passed, 11 suites);
  `cargo clippy --all-targets -- -D warnings` (no issues); and
  `cargo fmt -- --check` (passed). `git diff --check` passed for the source/test
  changes, and `git diff --cached --check` passed including all three staged
  artifacts.
- **Runtime harness:** the compiled binary is exercised by `cli_control` (26
  passed) and the two required daemon E2E scenarios (1 passed each); no
  separate manual runtime command was needed.
- **Work-unit commit identity:** pending. The content-addressed behavior commit
  cannot include its own hash, so the exact hash will be recorded in a
  documentation-only closure commit after the behavior commit is created.
- Rollback boundary: revert this ledger and the associated CLI usage test; if
  the RED proves a parser change is necessary, revert the `main.rs` parser
  error handling with them. No unrelated behavior depends on this slice.
