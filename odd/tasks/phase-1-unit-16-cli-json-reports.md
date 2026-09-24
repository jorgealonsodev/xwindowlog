# Phase 1 Unit 16 — CLI JSON reports

## Objective

Implement versioned machine-readable output for `xwindowlog status --json` and
`xwindowlog today --json` without changing the existing plain-text reports or
their persisted-data-only behavior.

## Problem

The CLI accepts `--json`, but both JSON variants still route to the Phase 16/17
placeholder. RF-61 requires an explicit, tested schema version so scripts can
reject incompatible report payloads deliberately.

## Authorized scope

- `src/main.rs`
- `tests/cli_status_today.rs`
- This task document and its Engram mirror

Out of scope: plain-text output changes, Phase 17 control commands, project
attribution, schema migrations, new dependencies, remote operations, and
changes to the existing store query path.

## Settled decisions and constraints

- Both report types use the numeric JSON field `schema_version` with value `1`.
- Both roots include `report` (`"status"` or `"today"`) so consumers can
  validate the endpoint as well as the version.
- `today --json` returns `{"schema_version":1,"report":"today","intervals":[...]}`.
  Each interval contains `start`, `end`, and `duration_seconds` as integer
  Unix seconds, plus the persisted sanitized `app_id`, `title`, and `state`.
- `status --json` returns `{"schema_version":1,"report":"status",...}`
  with `state`, nullable `app_id`, and `duration_seconds`. The optional
  `title` field is omitted by default and included only when
  `status_show_title = true`.
- With no persisted open interval, status uses `state: "not_tracking"`,
  `app_id: null`, and `duration_seconds: 0`.
- A persisted non-active state remains visible in `state`; it is not rendered
  as an active app.
- JSON is the only stdout payload for successful JSON commands. Diagnostics
  stay on stderr and RF-60 exit-code mapping remains unchanged.
- Existing `serde`/`serde_json` dependencies are sufficient; no dependency
  change is authorized.
- Strict TDD is active. RED must be observed before implementation; the runner
  is `cargo test`.

## Route

- Route: delegated direct.
- Trigger evidence: implementation touches the CLI report layer and its
  real-binary integration tests.
- One writer owns source/test changes and one work-unit commit; no parallel
  writers.

## Tasks

- [x] U16.7 RED — add real-binary JSON tests for both commands that parse the
  output, assert `schema_version` exists and equals `1`, and prove status title
  privacy plus today persisted-data output.
      Evidence: `cargo test --test cli_status_today` → 7 passed and 5 failed against the JSON placeholder before implementation.
      Commit: `c69f885` (`feat(cli): add versioned JSON reports`)
- [x] U16.8 GREEN — dispatch both `--json` variants and serialize the settled
  versioned report DTOs while reusing `query_today_intervals`.
      Evidence: `cargo test --test cli_status_today` → 12 passed; build, clippy with warnings denied, all targets (302 passed), and fmt check also passed.
      Commit: `c69f885` (`feat(cli): add versioned JSON reports`)

## Acceptance criteria

- `status --json` and `today --json` no longer use `not_yet_implemented()`.
- Both payloads contain numeric `schema_version: 1` and the correct `report`
  discriminator.
- JSON tests fail if the version field is removed or changed.
- `today --json` exposes clipped persisted intervals with integer epoch-second
  bounds and sanitized dictionary values.
- `status --json` hides titles by default, includes them only by explicit
  configuration, and represents paused/non-active/no-open states visibly.
- Successful JSON commands write valid JSON only to stdout; diagnostics stay
  on stderr and existing exit codes remain intact.
- Plain-text `status` and `today` output remains unchanged.

## Applicable checks

```text
cargo test --test cli_status_today
cargo build
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo fmt -- --check
```

## Progress

- Status: complete on `rf-32-recovery-correction`.
- Baseline: clean after the prior Unit 16 evidence commit `29503a4`.
- Verification evidence: real-binary JSON tests assert numeric `schema_version: 1`, report discriminators, persisted interval fields, title privacy, non-active state, and JSON-only stdout.
- Required checks: `cargo test --test cli_status_today` — 12 passed; `cargo build` — passed; `cargo clippy --all-targets -- -D warnings` — passed; `cargo test --all-targets` — 302 passed; `cargo fmt -- --check` — passed after `cargo fmt`.
- Work-unit commit: `c69f88525c946ab79b7df666f775f98efc6bed70` — `feat(cli): add versioned JSON reports`.

## Next step

Unit 16 reporting is complete. Continue to Phase 17 CLI controls when
authorized.
