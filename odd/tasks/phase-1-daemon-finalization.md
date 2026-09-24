# Phase 1 Daemon — Finalization Work Plan

## Goal

Finish the remaining OpenSpec Phase 1 daemon tasks, reconciling every unchecked
item against implementation and executable evidence rather than treating a
checkbox as proof. Continue on `phase-1-unit-17-pause-expiry`; do not push,
merge, or open a PR.

## Starting position

- Phase 17 tasks 17.1–17.16 have implementation ledgers; the RF-60 sweep is
  closed in `odd/tasks/phase-1-unit-17-cli-rf60-sweep.md`.
- Phase 17 tasks 17.17–17.24 and Phases 18–20 remain unchecked in
  `openspec/changes/phase-1-daemon/tasks.md`; reconcile each against the tree
  before implementing it.
- The open-interval Store-preservation criterion in
  `odd/tasks/phase-1-unit-17-cli-forget.md` now records concrete evidence: the
  implementation commit excludes `src/store.rs`, and the Store test suite
  passed. The reviewed documentation correction is committed as
  `02298c0d892f757a9db4200ccb84dffd14c3bbeb`.
- Two `gentle-ai-explore` attempts failed before child tool calls. A first
  writer launch was aborted while the conversation was interrupted; a later
  bounded writer completed Unit 1, and independent verification found and
  cleared a formatting blocker. Use the available package roles for later
  bounded tasks and report any runtime fallback.

## Rules

- Strict TDD (`cargo test`): where the task has a RED requirement, capture and
  record the actual failing assertion before production changes. Do not invent
  RED evidence for behavior that already passes; record the passing baseline
  and identify the remaining gap instead.
- Before each work unit, reconcile its OpenSpec acceptance criteria with code,
  tests, docs, and available environment. Do not duplicate already-present
  functionality merely because the canonical checkbox is unchecked.
- Keep each work unit bounded and reviewable; split further if its changed
  scope risks the 400-line review budget. Keep tests/docs with their behavior.
- Keep all technical artifacts in English. Use read-only exploration before a
  bounded worker, derive narrow edit surfaces, and keep writes single-threaded.
- Close each work unit only after its checks and evidence are recorded here and
  mirrored to Engram. Preserve the user's authority over push, PR, and merge.

## Work units

1. **Complete shell completions and man-page generation (OpenSpec 17.17–17.19).**
   Reconcile `Command::Completions`, supported shells, smoke-test availability,
   and build-time man-page generation. Add only missing behavior/tests/docs.
   Record exact runner and shell/tool availability; do not claim a smoke test
   that was skipped.
2. **Complete subcommand help and exit-map review (17.20–17.22).**
   Exercise help for every Phase 1 subcommand, fix only demonstrated gaps, and
   establish a single reviewable RF-60 exit-code mapping if the source does not
   already have one.
3. **Report VACUUM truncation truthfully (17.23–17.24).**
   Prove blocked WAL truncation is distinct from successful physical file
   shrink, preserve best-effort checkpoint semantics, and report reclaimed-but-
   not-yet-truncated space without converting successful VACUUM into failure.
4. **Complete systemd/service packaging (18.1–18.7).**
   Cover the daemon unit, graphical-session lifecycle evidence, full Phase 1
   example config, prune timer/service, and the agreement between systemd
   network restrictions and runtime FD-family checks.
5. **Prove the runtime no-network FD contract (19.1–19.2).**
   Add/run the `/proc/self/fd` family check after normal operation; record
   harness limitations explicitly rather than silently omitting them.
6. **Implement RNF-1/RNF-2 measurement evidence (19.3–19.5).**
   Add the accelerated memory soak, reactor idle-wakeup/per-thread schedstat
   measurement, and CPU-budget benchmark as the spec classifies them; report
   measured values, and do not turn local-only measures into CI gates.
7. **Implement RNF-3/RNF-4 regression gates (19.6–19.7).**
   Add a justified binary-size baseline/regression check and multi-run startup
   latency statistic with the spec's hard-gate semantics.
8. **Complete MSRV and CI assembly (19.8–19.11).**
   Verify the pinned MSRV, add its CI job, assemble stable/beta, Xvfb/openbox,
   and NFR gates, and ensure CI output reports numeric measurements.
9. **Complete README examples and fixture parity (20.1–20.3).**
   Document frozen `today`, `status`, JSON and status-bar usage; test the literal
   `today` example byte-for-byte against equivalent fixture data.
10. **Complete security/PRD amendments (20.4–20.5).**
    Add the threat-model honesty section and apply the four proposal-authorized
    PRD clarifications individually, preserving traceability and recording any
    unresolved wording conflict rather than guessing.
11. **Run final Phase 1 acceptance traceability (20.6).**
    Map each proposal success criterion to named passing evidence; report gaps
    and environment-skipped checks explicitly. Do not mark the OpenSpec change
    complete or archive it without the applicable SDD phase authority.

## Progress and evidence

| Unit | Status | RED/GREEN, checks, evidence, commit |
|---|---|---|
| 1. Completions/man page | verified; commit/review pending | Existing real-binary completions passed; added build-time man-page generation. RED: `cargo test --test completions` (3 passed, man-page assertion failed because `XWINDOWLOG_MANPAGE` was unset). GREEN: 4 passed. Shell syntax smoke checks passed for bash/zsh/fish; all three shells were installed. Independent checks: `cargo build`, `cargo fmt -- --check`, and `git diff --check` passed. Changed: `Cargo.toml`, `build.rs`, `tests/completions.rs`; `Cargo.lock` unchanged. Follow-up risk: build.rs mirrors CLI definition. |
| 2. Help/exit map | pending | |
| 3. VACUUM truncation reporting | pending | |
| 4. Systemd packaging | pending | |
| 5. Runtime network-FD check | pending | |
| 6. RNF-1/RNF-2 measurements | pending | |
| 7. RNF-3/RNF-4 gates | pending | |
| 8. MSRV/CI | pending | |
| 9. README examples | pending | |
| 10. PRD/security docs | pending | |
| 11. Final traceability | pending | |

## Exclusions

- No scope outside OpenSpec `phase-1-daemon` Phase 1 completion.
- No network publication, push, merge, or PR creation.
- No weakening of acceptance criteria to make unavailable runtime checks appear
  passed.
