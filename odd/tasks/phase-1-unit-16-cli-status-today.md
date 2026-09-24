# Phase 1 Unit 16 — CLI `status` and `today`

## Objective

Implement the plain-text `xwindowlog status` and `xwindowlog today` commands from Phase 1, using persisted sanitized data and the existing interval-clipping mechanism.

## Problem

The CLI parses `status` and `today`, but both commands still route to `not_yet_implemented()`. Users need a store-only daily report and a single-line status-bar report without waking X11 or depending on live daemon state.

## Authorized scope

- `src/main.rs`
- `src/store.rs`
- `src/config.rs`
- `Cargo.toml` and `Cargo.lock` only when enabling the already-declared `time` local-offset feature
- `tests/cli_status_today.rs`
- This task document and its Engram mirror

Out of scope: `status --json`/`today --json` (tasks 16.7–16.8), project persistence or attribution, schema migrations, reactor/X11/control changes, README work, new dependencies, and remote operations.

## Settled decisions and constraints

- `today` uses the user's local calendar day; persisted timestamps remain UTC epoch seconds.
- Phase 1 has no project association; reports must not invent project text.
- Both commands read persisted sanitized data only; they never query X11 or live daemon state.
- `status_show_title` defaults to `false`; an explicit `true` opt-in includes the persisted title.
- A paused/non-active persisted state is shown visibly instead of being rendered as an active app.
- Missing database and other state/config failures follow the existing RF-60 exit-code mapping.
- Strict TDD is active. RED must be observed before each implementation step; the runner is `cargo test`.

## Route

- Route: delegated direct.
- Trigger evidence: understanding spans more than four files and implementation touches multiple non-trivial Rust files.
- One writer owns the source/test changes and one work-unit commit; no parallel writers.

## Tasks

- [x] U16.1 RED — `cargo test --test cli_status_today` compiled the real-binary assertion and failed against the placeholder with `stderr="xwindowlog: this subcommand's behavior lands in Phase 16/17\\n"` (1 failed).
      Evidence: `cargo test --test cli_status_today` → 1 failed against the placeholder; stderr was the Phase 16/17 placeholder message.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.2 GREEN — `cargo test --test cli_status_today` passed (1 test). Plain `today` now opens the existing database, resolves one local offset with UTC fallback, queries `Store::clipped_intervals`, and renders persisted dictionary values.
      Evidence: `cargo test --test cli_status_today` → 1 passed; `today` used persisted intervals and one resolved local offset with UTC fallback.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.3 RED — the added real-binary title test compiled; `cargo test --test cli_status_today` observed 1 passed (`today`) and 1 failed because `status` still emitted the Phase 16/17 placeholder (exit failure, empty stdout).
      Evidence: `cargo test --test cli_status_today` → 1 passed and 1 failed; `status` still emitted the placeholder with empty stdout.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.4 GREEN — `cargo test --test cli_status_today` passed (2 tests). `status` now reads `status_show_title` through the existing config loader, selects the persisted open row, and never queries X11 or the control socket.
      Evidence: `cargo test --test cli_status_today` → 2 passed; `status` reads configuration and persisted state without X11 or control-socket access.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.5 RED — `cargo test --test cli_status_today` observed 2 passed and 2 failed: the active fixture produced only `"xwindowlog: editor\\n"` instead of the app/time/`today` shape, and the paused fixture also rendered `editor` without `paused`.
      Evidence: `cargo test --test cli_status_today` → 2 passed and 2 failed; active output lacked duration/today and paused output lacked `paused`.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.6 GREEN — `cargo test --test cli_status_today` passed (4 tests). The formatter emits one `xwindowlog: app · duration today` line, optionally includes the persisted title, and renders persisted `paused`/other non-active states instead of an app name.
      Evidence: `cargo test --test cli_status_today` → 4 passed; formatter emitted the app/duration/today shape and visible non-active states.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)
- [x] U16.9 REFACTOR — `cargo test --test cli_status_today` passed (5 tests), including a source-level guard proving exactly one report-layer `clipped_intervals` call site and two consumers of `query_today_intervals`.
      Evidence: `cargo test --test cli_status_today` → 5 passed; source guard confirmed one report-layer `clipped_intervals` call site and two `query_today_intervals` consumers.
      Commit: `ac5d003` (`feat(cli): add status and today reports`)

Mutation evidence for the store-only/shared-query claims:

- Injecting `X11Source::connect(None)` into `today` made the persisted-only test fail with `X11 connection failed: $DISPLAY variable not set and no value was provided explicitly`; removing it restored that test to 1 passed.
- Injecting a second direct `clipped_intervals` call into `status` made the shared-query guard fail (`left: 2`, `right: 1`); removing it restored the guard to 1 passed.

## Acceptance criteria

- Plain `status` and `today` no longer call `not_yet_implemented()`.
- Daily bounds use one resolved local UTC offset per process, with the existing UTC fallback/diagnostic behavior.
- Open and cross-midnight intervals are clipped correctly and never counted outside the requested range.
- Output comes from sanitized persisted dictionaries and never invents project attribution.
- Default status output is one line, hides titles, and visibly reports paused/non-active state.
- `--json` remains explicitly out of scope and unchanged.
- Focused and full checks pass with recorded RED/GREEN evidence.

## Applicable checks

```text
cargo test --test cli_status_today
cargo test --lib store::tests::clipped_intervals
cargo build
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo fmt -- --check
```

## Progress

- Status: complete on `rf-32-recovery-correction`.
- Baseline: clean `rf-32-recovery-correction` at `c7eade5`.
- Verification evidence: focused CLI test is GREEN with six real-binary/store-only tests plus the shared-query guard; both targeted mutations failed as expected and were reverted.
- Required checks (final run): `cargo test --test cli_status_today` — 7 passed; `cargo test --lib store::tests::clipped_intervals` — 3 passed, 190 filtered; `cargo build` — passed; `cargo clippy --all-targets -- -D warnings` — passed; `cargo test --all-targets` — 297 passed across 10 suites; `cargo fmt -- --check` — passed.
- Formatting note: the first format check exposed only formatting in the new code; `cargo fmt` was run, and the final format check passed.
- Work-unit commit: `ac5d0038f15aa419186f34f9a70204d67d55d962` — `feat(cli): add status and today reports`.

## Next step

U16.7–U16.8 (versioned `--json`) remain explicitly queued for a separate work unit; Phase 17 CLI control commands remain future work.
