# Tasks: Phase 1 — Daemon

**Change:** `phase-1-daemon` · **Store:** hybrid
**Engram mirror:** topic `sdd/phase-1-daemon/tasks`
**Upstream:** `openspec/changes/phase-1-daemon/proposal.md` (read-only) ·
`openspec/changes/phase-1-daemon/design.md` (read-only) · 8 spec files under
`openspec/changes/phase-1-daemon/specs/*/spec.md` (read-only)

TDD: **strict, RED → GREEN → REFACTOR**, runner `cargo test`. The crate
bootstrap task (Phase 1) is the **only** exemption, because it creates the
test runner itself and therefore cannot be preceded by an observable failing
test (`openspec/config.yaml` `rules.apply.tdd`).

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~7,500–9,500 total across 20 slices (avg ~400/slice, several above) |
| 400-line budget risk | High |
| Chained PRs recommended | Yes |
| Suggested split | PR 1 → PR 2 → … → PR 20 (stacked-to-main, see below) |
| Delivery strategy | auto-chain |
| Chain strategy | stacked-to-main |

Decision needed before apply: No
Chained PRs recommended: Yes
Chain strategy: stacked-to-main
400-line budget risk: High

**Why 20 slices, not the proposal's estimated 14–16.** The proposal's
14–16-slice estimate predates `sdd-design`, which introduced three modules
the proposal did not name individually — `reactor.rs`, `clock.rs`,
`signals.rs`, `control.rs` — as separate files with independent test
surfaces (design §4 File Changes). Splitting the reactor into
`signals.rs`+`control.rs` (Phase 13) and `reactor.rs` (Phase 14) instead of
one "reactor" slice, and giving CLI/service/measurement/README their own
slices instead of one "polish" slice, is a more honest cut than forcing them
back together to hit a number written before the architecture existed. Per
the sizing instruction, this is stated rather than hidden, and no task below
is split artificially or has its tests dropped to fit a line count.
`x11.rs` needed the anticipated 3 slices; `store.rs` and `tracker.rs` needed
the anticipated 2 each — those three proposal estimates held exactly.

**Slices at real risk of exceeding 400 lines**, each because of test-count
density rather than implementation size, so none should be shrunk by cutting
tests: Phase 6 (`tracker.rs` full transition table + P1/P2 proptest), Phase 7
(`exclude.rs`, five independently-cased default-exclusion categories plus
allowlist and secret sanitization), Phase 13 (`signals.rs`+`control.rs`, six
threat-matrix RED tests), Phase 14 (`reactor.rs`, the design's own
"highest-risk logic"), and Phase 17 (five CLI subcommands plus the RF-60
exit-code sweep). Each is flagged `High` below and is a strong candidate for
a further split at apply time if the actual diff exceeds budget — `sdd-apply`
should re-measure before opening the PR and split further along the RED/GREEN
task boundaries already drawn, rather than compress.

### Suggested Work Units

| Unit | Goal | Likely PR | Focused test command | Runtime harness | Rollback boundary | Risk |
|---|---|---|---|---|---|---|
| 1 | Crate bootstrap; prove `cargo test`, C toolchain, SQLite ≥3.31, x11rb/zbus/signal-hook fd assumptions | PR 1 | `cargo test` | `cargo build` (proves the C toolchain link); no daemon behavior yet | Delete `Cargo.toml` + module stubs; no prior slice depends on it | Low |
| 2 | `clock.rs` — wall/monotonic discipline, backwards-jump clamp | PR 2 | `cargo test clock::` | N/A — pure functions, no I/O, no clock, no real environment | Delete `src/clock.rs`; nothing else references it yet | Low |
| 3 | `store.rs` part 1 — schema, permissions, migrations, startup recovery | PR 3 | `cargo test store::schema`, `cargo test --test sqlite_version` | `sqlite3` CLI inspection of a real `xwindowlog.db` file's mode bits after first run | Delete schema/migration code in `src/store.rs`; no writer depends on it yet | Medium |
| 4 | `store.rs` part 2 — atomic transitions, clipping CTE, `prune`, `forget`, VACUUM contention | PR 4 | `cargo test store::` (transitions, clipping, prune, forget, contention) | Two real `rusqlite` connections on a temp file for the D-11 contention cases | Delete `transition`/`prune`/`forget` functions; schema (PR 3) stays intact | Medium-High |
| 5 | `tracker.rs` part 1 — `WindowSource`/`Effect` types, core window-change rows | PR 5 | `cargo test tracker::core` | N/A — pure state machine, proven with `ScriptedSource`+`FakeClock` | Delete the core transition arms; type definitions can stay as dead code if needed | Low |
| 6 | `tracker.rs` part 2 — full §11.1 table, debounce, destroy-grace, P1/P2 proptest | PR 6 | `cargo test tracker::` + `cargo test --release invariants::p1_p2` | N/A — pure, `ScriptedSource`+`FakeClock`; the "phase closing condition" runs in ms | Revert to PR 5's core arms only; P1/P2 tests would then correctly fail closed | High |
| 7 | `exclude.rs` — config rules, default list (5 categories), `hide_app`, allowlist, secret redaction | PR 7 | `cargo test exclude::` | N/A — pure regex/redaction, no I/O | Delete `src/exclude.rs`; not yet wired into the pipeline | High |
| 8 | Pipeline integration (x11-shaped synthetic → exclude → tracker → store) + P3 proptest + §14.3 ordering check | PR 8 | `cargo test --test pipeline_integration` + `cargo test --release invariants::p3` | N/A — still synthetic `WindowSource`, no real X11 yet | Revert the wiring test/glue code; each module (PR 5-7) remains independently correct | Medium |
| 9 | `x11.rs` part 1 — connection, EWMH verification/fallback, subscription, unconditional read, XWayland warning | PR 9 | `cargo test --test x11_integration -- ewmh` | Xvfb + `openbox --sm-disable`, readiness-polled (no fixed `sleep`) | Delete connection/subscription code; PR 8's synthetic pipeline is unaffected | Medium |
| 10 | `x11.rs` part 2 — BadWindow race, destroy-grace surfacing, title decode/truncation/`comm` fallback | PR 10 | `cargo test --test x11_integration -- races` | Xvfb + `openbox --sm-disable` + in-house synthetic-window helper | Revert to PR 9's connection-only behavior | Medium-High |
| 11 | `x11.rs` part 3 — SYNC/IDLETIME + degradation chain, reconnect backoff, full E2E (3 windows ≤1s) | PR 11 | `cargo test --test x11_integration -- absence reconnect e2e` | Xvfb + `openbox --sm-disable`, EWMH readiness poll (E-3) | Revert to PR 10; absence detection degrades to "disabled" cleanly per RF-25 | Medium-High |
| 12 | `logind.rs` — `SessionMonitor` trait, `Fake`/`Zbus` impls, `LockedHint`, session resolution, suspend inhibitor, RF-65 degradation | PR 12 | `cargo test logind::` | A reachable D-Bus session bus for the `ZbusSessionMonitor` smoke test; `FakeSessionMonitor` covers the rest with no D-Bus | Delete `src/logind.rs`; not yet on the reactor's fd table | Medium-High |
| 13 | `signals.rs` + `control.rs` — self-pipe signal handling, unix control socket, 6 threat-matrix cases | PR 13 | `cargo test signals::` `cargo test control::` | Real `UnixListener` + a real signal sent to the test process | Delete both modules; not yet wired into `reactor.rs` | High |
| 14 | `reactor.rs` — `poll(2)` loop, fd table, fairness/drain-before-poll, deadlines, wakeup counter | PR 14 | `cargo test reactor::` | Synthetic fds (pipes) standing in for the four real sources; no real X11/D-Bus needed here | Revert to PR 13's isolated modules; nothing yet depends on the composed reactor | High |
| 15 | `main.rs` composition — clap skeleton, config load, `flock`, signal/shutdown wiring, SIGHUP E2E, SIGKILL-recovery E2E | PR 15 | `cargo test --test daemon_lifecycle` | Real compiled binary, real process signals, real Xvfb session for the E2E daemon run | Revert `main.rs` to a stub `main()`; every module (PR 1-14) remains independently correct | Medium-High |
| 16 | CLI — `status`, `today`, `--json` schema version | PR 16 | `cargo test --test cli_status_today` | Real binary invocation against a fixture `xwindowlog.db` | Delete the `status`/`today` clap subcommand handlers | Low-Medium |
| 17 | CLI — `pause`, `resume`, `prune`, `forget`, `completions`, RF-60 exit-code sweep | PR 17 | `cargo test --test cli_control` | Real binary invocation against the real daemon (started in-test) and a fixture DB | Delete the five subcommand handlers; PR 16 unaffected | High |
| 18 | Service packaging — `contrib/xwindowlog.service`, `config.example.toml`, `xwindowlog-prune.timer`/`.service` | PR 18 | `cargo test --test systemd_unit_assertions` | `systemd-analyze verify contrib/xwindowlog.service` (informational per RNF-13) | Delete the `contrib/` files; daemon behavior is unaffected | Low |
| 19 | Measurement & CI gates — RNF-1..4 benchmarks, RNF-5 `/proc/self/fd` runtime check, MSRV job, CI workflow assembly | PR 19 | `cargo test --test rnf5_fd_check`; benchmarks run via documented local commands | Real running daemon under `perf`/`/proc` inspection; Xvfb CI job end to end | Delete `.github/workflows/ci.yml` additions and benchmark harnesses; no behavior depends on them | Medium |
| 20 | `README.md` frozen examples + `PRD.md` amendments (§13, RNF-5, RF-49, §17/§15.6) | PR 20 | `cargo test --test readme_example_matches_today` | Real binary run against the fixture DB, output diffed against the README | Revert `README.md`/`PRD.md`; no code behavior depends on either | Low-Medium |

---

## Phase 1: Crate Bootstrap (TDD-exempt — creates the runner)

- [x] 1.1 Create `Cargo.toml`: edition 2021, `panic = "unwind"` (proposal A-1),
      explicit MSRV pin (RNF-11), release profile. Phase 1 dependencies only —
      `x11rb` (`screensaver`, `sync` features), `zbus` (default features, see
      design §2 D-3), `rusqlite` (`bundled`, `functions`, `backup`), `nix`
      (`poll`, `socket`, `fs`, `signal`), `signal-hook`, `clap` +
      `clap_complete` + `clap_mangen`, `serde` + `serde_json` + `toml`,
      `regex`, `time`. Explicitly **no `rmcp`, no `tokio`** (proposal *In
      Scope*).
- [x] 1.2 Create module skeletons: `src/main.rs`, `src/reactor.rs`,
      `src/clock.rs`, `src/x11.rs`, `src/logind.rs`, `src/control.rs`,
      `src/signals.rs`, `src/tracker.rs`, `src/exclude.rs`, `src/store.rs`,
      each with a one-line purpose comment (design §4 File Changes) and an
      empty `#[cfg(test)] mod tests {}`.
- [x] 1.3 **Acceptance check:** `cargo test` runs successfully against the
      empty skeleton with zero failures. This is what makes every later RED
      test in this document observable — nothing proceeds until this passes.
- [x] 1.4 **Acceptance check (E-1):** a C toolchain (`cc`) is present and
      `rusqlite`'s `bundled` feature links; `cargo build` succeeds. Fail
      loudly here, not at Phase 4 packaging.
- [x] 1.5 **Acceptance check (E-2, D-10 layer 2, assumption A-5):** write
      `tests/sqlite_version.rs` asserting
      `rusqlite::version_number() >= 3_031_000`. This is diagnosable-in-10-
      seconds scaffolding; the load-bearing behavioral proof is D-10 layer 1
      in Phase 3 (task 3.1).
- [x] 1.6 **Acceptance check (assumption A-1, DR-1, proposal's single
      load-bearing unverified claim):** a throwaway five-line program proves
      `x11rb`'s connection type exposes a pollable fd (`AsRawFd`/`AsFd` or a
      `stream()` accessor over one), and that the fd is genuinely poll-able.
      **If this is false, STOP before any of Phase 9-11 or Phase 14 starts**
      and apply the D-3-pattern bridge-thread-plus-`eventfd` fallback (the
      same pattern already used for `logind.rs`) instead — record the
      decision and continue; do not discover this mid-`x11.rs`.
- [x] 1.7 **Acceptance check (assumption A-2):** the same throwaway program
      proves `x11rb` offers a non-blocking event drain
      (`poll_for_event() -> Option<Event>`, distinct from a blocking
      `wait_for_event`), which D-6's drain-before-poll invariant needs. Same
      stop-and-fallback rule as 1.6 if false.
- [x] 1.8 **Acceptance check (assumption A-3):** a compile-only check that
      `signal_hook::flag::register(sig, Arc<AtomicBool>)` and
      `signal_hook::low_level::pipe::register(sig, writer)` exist with the
      shapes design §2 D-4 assumes.
- [x] 1.9 REFACTOR: run `cargo fmt` and `cargo clippy` on the skeleton and
      confirm both are clean, establishing the baseline every later task is
      diffed against.

---

## Phase 2: Clock Discipline — `src/clock.rs`

**Traces:** RF-28, design §2 D-9. Landed before `tracker.rs` per design §8's
explicit ordering constraint.

- [x] 2.1 RED: `close_at(start, requested_end)` — forward case returns
      `Close::Ok`; a backwards `requested_end < start` returns
      `Close::ClampedBackwards { to: start }` (interval-tracking "Backwards
      wall-clock jump during an open interval").
- [x] 2.2 GREEN: implement `WallTs`/`MonoInstant` newtypes (no `Sub` impl, no
      `From` in either direction between them — RF-28's "never derived from
      the other" enforced by absence of a conversion) and `close_at`.
- [x] 2.3 RED: `duration_secs(start, end)` never negative and never panics,
      including `i64::MIN`/`i64::MAX` inputs (T-2).
- [x] 2.4 GREEN: implement `duration_secs` via `checked_sub` +
      `.filter(|d| *d >= 0)` + `.unwrap_or(0)`. No bare `-` on a `WallTs`
      anywhere.
- [x] 2.5 RED: `backdated_close(start, now, idle)` clamps to `start` when
      backdating past it would produce a negative duration (interval-tracking
      "Backdated absence close clamped by a backwards jump").
- [x] 2.6 GREEN: implement `backdated_close`.
- [x] 2.7 RED: a `trybuild` compile-fail fixture asserting no `From<WallTs>
      for MonoInstant` (or the reverse) exists (interval-tracking "Monotonic
      and wall clocks are never substitutable").
- [x] 2.8 GREEN: add the `trybuild` fixture and wire it into `cargo test`.
- [x] 2.9 GREEN (no isolated RED — needed as test infrastructure for every
      later module): implement the `Clock` trait, `SystemClock`, `FakeClock`
      (design §2 D-8) — this is what makes P2 a millisecond-scale test
      instead of a 24-hour one.
- [x] 2.10 REFACTOR: grep `clock.rs` for any bare `-` on a `WallTs` value;
      confirm none exists (design's stated reviewable convention).

---

## Phase 3: Interval Storage — Schema, Migrations, Recovery — `src/store.rs` (1/2)

**Traces:** RF-10, RF-11, RF-35, RF-36, RNF-6 (consistency half).

- [x] 3.1 RED (D-10 layer 1, the behavioral proof of E-2 that does not trust
      any version string): apply the real RF-11 schema to
      `Connection::open_in_memory()`; insert one open interval, insert a
      second, assert the second fails `SQLITE_CONSTRAINT_UNIQUE` on
      `idx_intervals_one_open` (interval-storage "A second open interval is
      rejected").
- [x] 3.2 GREEN: implement the full RF-11 schema in `store.rs` — pragmas
      (`journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
      `temp_store=MEMORY`, `busy_timeout=5000`), `apps`/`titles` tables with
      sentinel rows (`apps(1,'?')`, `titles(1,'-')`), `intervals` with the
      `open_marker` generated column and `idx_intervals_one_open`, `projects`
      and `rules` tables (proposal assumption A-3: full schema at
      `user_version = 1`, including tables nothing reads/writes in Phase 1).
- [x] 3.3 RED: an invalid `state = 'activ'` fails its `CHECK`; an
      `end < start` insert fails `CHECK ("end" IS NULL OR "end" >= start)`; an
      interval referencing a nonexistent `apps.id` fails its foreign-key
      constraint (interval-storage, three "rejected" scenarios).
- [x] 3.4 GREEN: fix any constraint definition gap 3.3 surfaces.
- [x] 3.5 RED: with ambient umask `022`, `xwindowlog.db`,
      `xwindowlog.db-wal`, `xwindowlog.db-shm` are all created `0600`; data
      and config directories are created `0700` (RF-10, both scenarios).
- [x] 3.6 GREEN: implement `store::open()` calling `umask(0o077)` before any
      `Connection::open`, and explicit-mode directory creation.
- [x] 3.7 RED: `store::open()` returns a typed error mapped to exit code 3
      (environment error, cli-reporting RF-60) rather than a panic, when the
      linked SQLite fails the D-10 layer-1 behavioral assertion at runtime.
- [x] 3.8 GREEN: implement the runtime guard (D-10 layer 3).
- [x] 3.9 RED: `PRAGMA user_version` equal to current → no migration, no
      backup runs; below current → an Online-Backup-API backup is taken
      before the first migration statement, each pending migration runs in
      its own transaction with `PRAGMA user_version` as the last statement;
      above current → startup aborts with an explicit message and the schema
      is not modified (RF-35, all three scenarios).
- [x] 3.10 GREEN: implement the forward-only migration runner and the
      Online-Backup-API pre-migration backup, exercised with **synthetic**
      migrations since v1 migrates nothing real (design §8).
- [x] 3.11 RED: exactly one stale open interval is closed at the startup
      instant and an `unknown` interval opens immediately from that point;
      more than one open interval logs an error and closes all but the most
      recent with `end = start` (RF-36, both scenarios).
- [x] 3.12 GREEN: implement `recover_on_startup` returning
      `Recovery { closed_stale, extra_open_rows }` (design §5 `IntervalStore`
      contract).
- [x] 3.13 REFACTOR: consolidate the DDL into one `const SCHEMA_V1: &str`
      used by every test and by `open()`, eliminating any duplicated fixture
      copy of the schema (design §6 testing-strategy note: "never a fixture
      copy").

---

## Phase 4: Interval Storage — Transitions, Clipping, Prune, Forget — `src/store.rs` (2/2)

**Traces:** RF-12, RF-13, RF-52, RF-53, RF-66, RNF-6 (durability trade-off).

- [x] 4.1 RED (D-1): within one `BEGIN IMMEDIATE` transaction, an
      `INSERT`-before-`UPDATE` ordering fails on `idx_intervals_one_open`; a
      `UPDATE`-before-`INSERT` ordering succeeds (interval-storage "Closing
      before opening within the transaction avoids a false collision").
- [x] 4.2 GREEN: implement `transition(at, open)`, `close_only(at)`,
      `open_only(at, open)` per the `IntervalStore` trait (design §5),
      `BEGIN IMMEDIATE`, `UPDATE` before `INSERT`, no `RETURNING` (keeps the
      3.31 floor true — D-1 rationale point 3).
- [x] 4.3 RED: a single transition writes exactly one transaction that both
      closes the old interval and opens the new one with identical
      `end`/`start` timestamps (RF-12 "A transition writes exactly one
      transaction").
- [x] 4.4 GREEN: fix any transaction-boundary gap 4.3 surfaces.
- [x] 4.5 RED: an interval spanning 23:50–00:10 attributes exactly 10 minutes
      to the earlier day's query and exactly the remaining 10 to the later
      day's, with no gap or overlap; an open interval clips against the
      query's upper bound (`now`) (RF-66, both scenarios).
- [x] 4.6 GREEN: implement the clipping CTE (PRD §11.3) as a `store.rs` query
      function, consumed later by Phase 16's `today`/`status`.
- [x] 4.7 RED: `prune --older-than <duration>` deletes closed intervals older
      than cutoff, never the open interval; preserves `rules`/`projects` even
      when their only referencing intervals are pruned; deletes orphaned
      `apps`/`titles` excluding sentinels; the file shrinks after `VACUUM`;
      `prune`'s delete/VACUUM logic is reachable only via the CLI, never from
      within the daemon (RF-13, all five scenarios).
- [x] 4.8 GREEN: implement `prune()` — delete phase in one `BEGIN IMMEDIATE`
      transaction with `busy_timeout = 5000`, `VACUUM` as a separate phase.
- [x] 4.9 RED (D-11, three contention cases): `VACUUM` succeeds against a
      second idle connection; `VACUUM` retries (`busy_timeout = 10000`, 1s /
      2s / 4s) and succeeds once a concurrent `BEGIN IMMEDIATE` writer
      commits; `VACUUM` against a permanently-busy database exhausts its 3
      retries, exits with the state-error code (2, not 1), emits the exact
      stderr from design §2 D-11, and `SELECT COUNT(*)` proves the deletes
      survived (interval-storage "VACUUM contends with a live daemon
      connection"; cli-reporting "prune exits 2 when VACUUM exhausts its
      retries").
- [x] 4.10 GREEN: implement the bounded `VACUUM` retry ladder, the exact
      stderr message, and `--vacuum-only`.
- [x] 4.11 RED: `forget --from/--to` and `forget --window <id>` physically
      delete matching rows (not a flag), clean up orphaned `apps`/`titles`,
      run `VACUUM`; without `--yes`, the command shows row count and range
      and waits for confirmation before deleting (RF-53, all scenarios).
- [x] 4.12 GREEN: implement `forget()` sharing `prune`'s delete/orphan-
      cleanup/`VACUUM` machinery, differing only in the `WHERE` clause and
      the interactive-confirmation path (proposal's stated cost rationale).
- [x] 4.13 REFACTOR: grep the crate for any write to `intervals` outside
      `store.rs` — there should be none. This is re-checked in Phase 13 once
      the control socket exists, since that is the module most tempted to
      write directly.

---

## Phase 5: Interval Tracker — Core Trait & Effects — `src/tracker.rs` (1/2)

**Traces:** RF-1 (consumption side), RF-2, RF-3 (partial), design §2 D-8.

- [x] 5.1 GREEN (pure type definitions — nothing to fail first): define
      `SourceEvent`, `WindowInfo`, `Effect`, `NewInterval` exactly per design
      §2 D-8. **No `x11rb`/`zbus`/`nix`/`rusqlite` type may appear anywhere in
      `tracker.rs`** — this is the module boundary the rest of the phase
      depends on.
- [x] 5.2 GREEN: define the `WindowSource` trait
      (`next_event(deadline) -> Result<Option<SourceEvent>, SourceError>`) and
      `Tracker::{on_event, next_deadline}` signatures.
- [x] 5.3 RED: `unknown` (startup) + first valid active-window event →
      `active` (interval-tracking "Every row of the transition table is
      exercised", first row).
- [x] 5.4 GREEN: implement the `unknown → active` transition.
- [x] 5.5 RED: `active` + active-window change → `active` (new window), with
      the closed interval's `end` and the new interval's `start` equal to the
      event instant (RF-3 "Consecutive transitions share the same boundary
      instant").
- [x] 5.6 GREEN: implement the window-change transition returning
      `Effect::Transition` with one `at`.
- [x] 5.7 RED: `_NET_ACTIVE_WINDOW` becomes unset (desktop focused) is
      recorded with the `"(desktop)"` sentinel, not discarded and not treated
      as absence (window-capture "Desktop focus is legitimate activity").
- [x] 5.8 GREEN: implement the desktop-focus branch.
- [x] 5.9 REFACTOR: add a doc comment on `Effect::Transition` recording that
      its single `at` field makes mismatched close/open timestamps a compile-
      time impossibility, not a discipline (design's stated type-level RF-3
      guarantee) — so the invariant is documented where a future editor would
      look, not only in the design doc.

---

## Phase 6: Interval Tracker — Full Transition Table — `src/tracker.rs` (2/2)

**Traces:** RF-3 (full table), RF-4 (backdating), RF-23, RF-30, P1, P2 — **the
phase's closing invariant (proposal §Intent).**

- [x] 6.1 RED+GREEN, one pair per remaining §11.1 row (interval-tracking
      "State transition table"), each proven with `ScriptedSource` +
      `FakeClock`: stable title change → `active` (new interval) at the title
      event's instant; `active`/`afk` + `LockedHint→true` or
      `PrepareForSleep(true)` → `locked`; `active`/`afk` + pause requested →
      `paused`; `locked` + `LockedHint→false` or `PrepareForSleep(false)` →
      `unknown`; `afk` + idle negative transition → `active` (same window if
      it still exists) or `unknown`; any + X11 connection loss → `unknown` at
      detection instant; any + shutdown signal → process-exit effect.
- [x] 6.2 RED: the absence closing timestamp is `t_alarm − ms_since_user_input`
      (backdated), never `t_alarm` itself (idle-detection "User goes idle
      past the threshold").
- [x] 6.3 GREEN: implement the backdated-close branch using Phase 2's
      `clock::backdated_close`.
- [x] 6.4 RED: a title change closes/opens an interval only after
      `title_debounce_ms` of stability; a second title change before the
      debounce elapses discards the first pending title and restarts the
      timer, recording no transition for the intermediate title (RF-30, both
      scenarios).
- [x] 6.5 GREEN: implement `ArmTimer(TitleDebounce)`/`CancelTimer` emission
      and the pending-title state.
- [x] 6.6 RED: `DestroyNotify` on the tracked window arms a 250 ms
      `DestroyGrace` deadline; a new `_NET_ACTIVE_WINDOW` change before it
      elapses cancels the deadline with no gap recorded; if the deadline
      fires first, the interval closes with `end` = the original
      `DestroyNotify` timestamp (not the deadline-fired time), transitioning
      to `unknown` (RF-23, both scenarios).
- [x] 6.7 GREEN: implement the `DestroyGrace` arm/cancel/fire logic, stashing
      the `DestroyNotify` timestamp at arm time.
- [x] 6.8 RED (P1): a `proptest` generator of arbitrary valid event
      sequences never produces two overlapping intervals (interval-tracking
      "Randomized event sequences never produce overlapping intervals").
- [x] 6.9 RED (P2 — **the phase's closing condition**): a scripted,
      deterministic full-simulated-day sequence (window changes, idle
      transitions, lock/unlock, suspend/resume, pause/resume, an X11
      connection loss and recovery) sums
      `active+afk+locked+paused+unknown` to exactly
      `end_of_day − start_of_day`, using `FakeClock` (interval-tracking "A
      full simulated day sums exactly").
- [x] 6.10 GREEN: fix whatever P1/P2 uncover. This task exists because the
      properties are expected to surface edge cases the row-by-row tests
      miss — get this green well before the phase ends, not at the end
      (proposal, design §1).
- [x] 6.11 REFACTOR: structure the transition function as one match arm per
      source state so it can be audited against the §11.1 table by
      inspection, side by side with `interval-tracking/spec.md`.
- [x] 6.12 REFACTOR (**interim-debt cleanup, added by the orchestrator after
      PR 2**): remove the module-level `#![allow(dead_code, reason = "...")]`
      from `src/clock.rs`. By the end of this phase `clock.rs` has real consumers
      in `store.rs` and `tracker.rs`, so its allow is no longer justified.
      Modules whose own consumers land later (`tracker.rs`, wired up in Phase
      8 and Phase 14) keep theirs until the sweep in task 15.13. A blanket
      module-level `dead_code` allow that outlives its reason silently hides
      genuinely dead code for the rest of the project's life — that is why
      this is a task in the plan and not a note anyone has to remember.
      Acceptance: `grep -rn 'allow(\s*$\|allow(dead_code' src/` returns
      nothing, and `cargo clippy --all-targets -- -D warnings` is still
      clean. If a specific item is legitimately unused at this point, narrow
      the allow to that item with its own `reason`, never leave it at module
      scope.

---

## Phase 7: Privacy Filtering — `src/exclude.rs`

**Traces:** RF-7, RF-8, RF-9 (unit half), RF-47, RF-48, RF-50, RF-51, P3
(partial — sum invariant proven in Phase 8), design §2 D-7.

- [x] 7.1 GREEN (pure type definitions): define `RawTitle` (redacting
      `Debug`, no `Display`) and `SafeTitle` (private field, single
      constructor `from_sanitized`, living in this module) per design §2 D-7.
- [x] 7.2 RED: a config rule matching `app_id` excludes the window; a rule
      matching title excludes the window; no raw title is ever logged during
      exclusion evaluation (RF-7, all three scenarios).
- [x] 7.3 GREEN: implement `$XDG_CONFIG_HOME/xwindowlog/config.toml` rule
      loading and `app_id`/title regex matching.
- [x] 7.4 RED: an excluded window's title stores as `[hidden]` with duration
      unchanged; two different excluded apps remain distinguishable by
      `app_id` (RF-8, both scenarios).
- [x] 7.5 GREEN: implement hidden-title substitution.
- [x] 7.6 RED: `hide_app = true` additionally hides `app_id`; a rule without
      `hide_app` leaves `app_id` visible (RF-47, both scenarios).
- [x] 7.7 GREEN: implement `hide_app`.
- [x] 7.8 RED, one task per RF-48 default-list category, **each with its own
      case table** (design §6): 7.8a `password-managers`; 7.8b
      `banking-generic`; 7.8c `private-browsing`; 7.8d `gpg-ssh-prompts`;
      7.8e `2fa-otp`; plus `disable_default_excludes = ["banking-generic"]`
      disabling one category while others stay active, and an unmatched
      window (`app_id = "firefox"`) passing through unmodified.
- [x] 7.9 GREEN: implement the embedded default exclusion list (active with
      no `config.toml`) and `disable_default_excludes`.
- [x] 7.10 RED: `mode = "allowlist"` excludes an app not matching any
      `[[include]]` rule and includes one that does (RF-50, both scenarios).
- [x] 7.11 GREEN: implement allowlist inversion as the same evaluation path,
      not a separate mechanism.
- [x] 7.12 RED, its own case table: a `ghp_...`-shaped token fragment is
      redacted; an email address fragment is redacted; `sanitize_secrets =
      false` leaves fragments intact; a title that both matches an exclusion
      rule and contains a redactable secret is stored as `[hidden]` with
      secret sanitization not separately visible (RF-51, all four scenarios).
- [x] 7.13 GREEN: implement `sanitize_secrets` — known token prefixes
      (`sk-`, `ghp_`, `xox*-`, `AKIA`), hex ≥32 chars, base64-shaped ≥24
      chars, email addresses — as fragment-level `[REDACTED]` replacement,
      independent of and in addition to exclusion.
- [x] 7.14 RED: `Excluder` reload applies an updated rule set to subsequent
      events without modifying already-recorded intervals (RF-9, unit-level —
      full `SIGHUP` E2E delivery is Phase 15's task 15.7).
- [x] 7.15 GREEN: implement config reload as an `Excluder` hot-swap.
- [x] 7.16 REFACTOR: confirm the crate has exactly one constructor path for
      `SafeTitle` (`from_sanitized`, in this file) — there is no second way
      to build one.

---

## Phase 8: Pipeline Integration & Invariants (P1–P3)

**Traces:** P3, §14.3 ordering guarantee (proposal *Success Criteria*).

- [x] 8.0 GREEN (**added by the orchestrator after PR 6 — blocking, do this
      first**): introduce `src/lib.rs` and turn the crate into a lib plus a
      thin `src/main.rs` shell. The crate is currently binary-only, and a
      binary-only crate cannot expose its internals to `tests/*.rs`, so the
      integration-test files this plan and the PRD both name are impossible
      as things stand: task 8.1's `tests/pipeline_integration.rs` and the
      `tests/invariants.rs` that PRD.md's M-1 line cites as the proof of the
      product's headline metric. Phase 6 had to put P1 and P2 inside
      `tracker.rs`'s own test module for exactly this reason.
      Move the module declarations to `lib.rs`, leave `main.rs` as argument
      parsing plus a call into the library, and re-export what integration
      tests need. This is ordinary Rust practice for a binary with logic
      worth testing, and it also unblocks the Phase 2 MCP work later.
      Acceptance: `cargo test` still green with the same test count, and a
      trivial `tests/` file can `use xwindowlog::...` and compile.
- [x] 8.0b REFACTOR: move P1 and P2 from `tracker.rs`'s test module into
      `tests/invariants.rs`, the location PRD.md's M-1 line actually names,
      now that 8.0 makes it possible. Keep the per-bucket assertions exactly
      as they are — they are what catches a boundary shift, since the grand
      total is conserved by any such bug.
- [x] 8.1 RED (P3): a scripted sequence mixing excluded and non-excluded
      windows over total duration `D`, run once with exclusion active and
      once without, sums to `D` in both cases (privacy-filtering "Mixed
      excluded and non-excluded windows sum correctly").
- [x] 8.2 GREEN: wire `x11-shaped synthetic events → exclude.rs → tracker.rs
      → store.rs` end to end in an integration test harness (still
      `ScriptedSource`, no real X11 yet), fixing whatever P3 uncovers at the
      exclude/tracker boundary.
- [x] 8.3 RED: `SafeTitle` equality means two different raw titles that both
      sanitize to `[hidden]` are equal, so an excluded app switching between
      hidden titles yields one continuous interval, not several — asserted
      explicitly (design §2 D-7's named consequence, DR-5), not left to be
      discovered.
- [x] 8.4 GREEN: confirm/adjust the tracker's title-change comparison to
      operate on `SafeTitle`.
- [x] 8.5 RED: across the wired pipeline, no log call, panic message, or
      persisted value contains raw-title content before `exclude.rs` runs
      (§14.3 ordering guarantee, made executable rather than a review-
      checklist item only) — capture all log/panic output during a scripted
      run and assert no `RawTitle` substring appears downstream of exclusion.
- [x] 8.6 GREEN: fix any violation 8.5 finds.
- [x] 8.7 REFACTOR: re-run Phase 6's P1/P2 proptests with `exclude.rs` now
      wired into the pipeline and confirm they still pass unchanged (no
      double-counting or overlap introduced by exclusion).

---

## Phase 9: X11 Capture — Connection, EWMH, Subscription — `src/x11.rs` (1/3)

**Traces:** RF-1, RF-2, RF-24, RF-29.

- [x] 9.1 RED (Xvfb E2E, using the fd/drain accessors proven in 1.6/1.7):
      `_NET_SUPPORTED` and `_NET_ACTIVE_WINDOW` both exist at startup → no
      diagnostic, proceeds with subscription (window-capture "Window manager
      is EWMH-compliant").
- [x] 9.2 GREEN: implement the EWMH verification. *(Mechanism corrected
      2026-09-17 — see task 9.14. Originally worded "implement
      `intern_atom(only_if_exists=true)` verification", which the PRD itself
      specified and which does not test what RF-24 means.)*
- [x] 9.3 RED (Xvfb with no EWMH-compliant WM): missing EWMH properties emit
      an explicit stderr diagnostic and the daemon falls back to
      `GetInputFocus`, without sitting idle waiting for events that will
      never arrive (RF-24, degradation scenario).
- [x] 9.4 GREEN: implement the `GetInputFocus` fallback path. *(Was checked
      over an enum variant with no behaviour; the actual polling landed
      2026-09-17 — see task 9.16.)*
- [x] 9.5 RED: subscribes to `PropertyNotify` on `_NET_ACTIVE_WINDOW` (root
      window) and to `PropertyChangeMask`/`StructureNotifyMask` on the active
      window; wakes exactly once per change with no query beforehand
      (window-capture "Active window changes while idle").
- [x] 9.6 GREEN: implement the subscription.
- [x] 9.7 RED: after every active-window change, an unconditional fresh read
      of `_NET_WM_NAME`/`WM_NAME`, `_NET_WM_PID`, `WM_CLASS` occurs, never
      reusing cached values (window-capture "Property read follows every
      active-window change").
- [x] 9.8 GREEN: implement the unconditional read.
- [x] 9.9 RED: `WM_CLASS = "firefox\0Firefox"` + title + `_NET_WM_PID = 4821`
      capture `app_id = "Firefox"` correctly (window-capture "Metadata
      captured for a normal window").
- [x] 9.10 GREEN: implement `WindowInfo` extraction from the read properties.
- [x] 9.11 RED: `DISPLAY` + `WAYLAND_DISPLAY` (or `XDG_SESSION_TYPE=wayland`)
      present → a reduced-reliability warning is emitted, daemon continues;
      native X11 → no warning (RF-29, both scenarios).
- [x] 9.12 GREEN: implement the XWayland startup check.
- [x] 9.13 REFACTOR: confirm `x11.rs` emits only `RawTitle`/owned
      `SourceEvent` values across its boundary — no `x11rb` type crosses into
      `tracker.rs` (design §1 layering).

### Phase 9 corrections (2026-09-17, from an adversarial verification pass)

Phase 9 was green — 106 tests, clippy and fmt clean — over three broken
guarantees. These tasks record the corrections. Every one was driven by an
observed RED against the code as it stood.

- [x] 9.14 RED (Xvfb, **no window manager at all**, both EWMH atom names
      already interned by an unrelated client): `X11Source::connect` must
      report non-compliance. Observed RED: reported `Ewmh`. Second RED: a
      compliant WM that exits leaves a stale `_NET_SUPPORTED` on the
      server-owned root window and still reported `Ewmh`. Third RED (found by
      mutation testing, not by reading): a *live* WM whose `_NET_SUPPORTED`
      omits `_NET_ACTIVE_WINDOW` — RF-24's headline tiling-WM case — was not
      covered by either of the first two, because both also lack a live check
      window.
- [x] 9.15 GREEN: correct all three layers, PRD first. `PRD.md` RF-24 and
      `specs/window-capture/spec.md` replace `intern_atom(only_if_exists =
      true)` — a query against the X server's *global atom-name table*, which
      is server-wide, written by any client and outlives all of them — with
      the root window's `_NET_SUPPORTED` contents plus a
      `_NET_SUPPORTING_WM_CHECK` liveness probe. Both carry a dated
      correction note. `src/x11.rs` implements the corrected check.
- [x] 9.16 RED: on a WM-less display where the X server confirms the input
      focus genuinely moved, the daemon must **observe** the change.
      Observed RED: zero events in 5 s — `CaptureMode::InputFocusFallback`
      was an enum variant with no behaviour behind it, and task 9.4 was
      checked over its absence.
- [x] 9.17 GREEN: implement the `GetInputFocus` degrade — query, resolve the
      focused window to its top-level ancestor, and emit `RawEvent` only on a
      real change. The polling interval stays a reactor concern (Phase 14).
- [x] 9.18 RED: one `poll_for_event` call must drain until the queue is
      genuinely empty. Observed RED: with an untranslated root-property event
      queued ahead of a real `_NET_ACTIVE_WINDOW` change, a single call
      returned `None` while the change sat pending — and since `x11rb` has
      already drained the socket, the fd is no longer readable, so a `poll(2)`
      reactor would sleep through it. The wakeup is lost, not delayed.
- [x] 9.19 GREEN: loop until the queue is empty; add `drained_untranslated()`
      so "no event" and "event I did not translate" are distinguishable; make
      the docstring's drain-before-poll claim true rather than asserted.
- [x] 9.20 RED/GREEN: the Xvfb harness dropped its readiness connection, so
      the client count hit zero, the server performed a close-down reset, and
      the next connect raced it — measured 1 failure in 30 runs. `XvfbGuard`
      now holds that connection for the test's lifetime: 0 failures in 30
      runs, then 0 in 25 more.
- [x] 9.21 RED/GREEN (threat-matrix, same class as 10.11/10.12's
      `/proc/<pid>/comm`): `WM_CLASS` is untrusted input from an arbitrary
      application and reached diagnostics and SQLite verbatim, control
      characters included. Observed RED: a class component of `"\n\x1b\x07"`
      arrived as `app_id` unchanged. Control characters are now stripped at
      the point of capture, with the `"?"` sentinel when nothing survives.
- [x] 9.22 GREEN (cheap correctness, each with its own RED): `_NET_WM_PID =
      0` yielded `Some(0)` and would have sent Phase 10 to `/proc/0/comm` —
      now `None`; `long_length = u32::MAX` read a 200,000-character title
      whole — now bounded to 2048 bytes, the widest encoding of RF-31's 512
      characters; `subscribe_window` never released the previous window, so
      subscriptions accumulated for the whole session and amplified 9.18 —
      the subscription now moves instead of accumulating.
- [x] 9.23 GREEN: `X11Source::connect` now emits its startup diagnostics to
      stderr via `emit_startup_diagnostics`, so task 9.3's "emit an explicit
      stderr diagnostic" is true at the point the condition is detected
      rather than deferred to a `reactor.rs` that is still a Phase 14 stub.
      The writer is injected so the emission is unit-tested against an
      in-memory sink; `connect` still returns the strings for callers.

---

## Phase 10: X11 Capture — Races, Debounce Surfacing, Decode — `src/x11.rs` (2/3)

**Traces:** RF-22, RF-23 (surfacing half), RF-30 (surfacing half), RF-31.

- [x] 10.1 RED: on every active-window change, `GetProperty` then
      `ChangeWindowAttributes(...).check()` runs in that order; a BadWindow
      result on the check is a valid transition — no error, no event emitted,
      daemon awaits the next change (RF-22, "Active window is destroyed
      before its event mask is set").
- [x] 10.2 GREEN: implement the ordered read-then-register-with-check
      sequence and BadWindow handling — validates assumption A-4 (design §12);
      adjust to whatever discriminant `x11rb` actually returns and record the
      correction if it differs from the PRD's hypothesis.
- [x] 10.3 RED: a title change racing the event-mask registration is still
      reflected by the next unconditional property read (RF-22, "Title
      changes between the property read and the event mask being active").
- [x] 10.4 GREEN: confirm 9.7's unconditional-read logic covers this; add the
      specific race-timing test.
- [x] 10.5 RED: `DestroyNotify` for the tracked window surfaces as
      `SourceEvent::ActiveWindowDestroyed` — E2E cases: the WM updates the
      property promptly (no gap recorded), and a lax WM under `kill -9` lets
      the 250 ms grace elapse (RF-23, both scenarios; the state-machine
      handling itself was proven in Phase 6, task 6.6-6.7).
- [x] 10.6 GREEN: implement `DestroyNotify` → `SourceEvent::
      ActiveWindowDestroyed` surfacing.
- [x] 10.7 RED: `x11.rs` surfaces raw `TitleChanged` events without itself
      debouncing — confirms the debounce state machine stays exclusively in
      `tracker.rs` (RF-30, x11-side surfacing only; the debounce logic is
      Phase 6).
- [x] 10.8 GREEN: confirm/adjust `x11.rs` to surface undebounced
      `TitleChanged` events.
- [x] 10.9 RED: a 600-character title truncates to 512 with a trailing
      ellipsis; `STRING` vs `UTF8_STRING` atom types decode correctly rather
      than assuming UTF-8; `WM_CLASS` absent + `_NET_WM_PID` present reads
      `/proc/<pid>/comm`; `WM_CLASS` absent and no readable `comm` falls back
      to the `"?"` sentinel without erroring (RF-31, all scenarios).
- [x] 10.10 GREEN: implement atom-type-aware decoding, 512-char truncation
      with ellipsis, and the `/proc/<pid>/comm` fallback chain.
- [x] 10.11 RED (threat-matrix "Process integration — subprocess inputs",
      design §7): `/proc/<pid>/comm` for an exited or recycled pid; a `comm`
      containing a newline; invalid UTF-8 in `comm`.
- [x] 10.12 GREEN: implement best-effort `comm` reading that never errors,
      falls back to `"?"` on any failure, strips control characters, and
      passes the result through `exclude.rs` like any other title component.
- [x] 10.13 REFACTOR: confirm the BadWindow/decode error paths never
      `panic!` and never log a raw title (cross-check against Phase 8's
      §14.3 ordering test).

### Phase 10 corrections (2026-09-17, from a second adversarial pass)

Phase 10 was green — 143 tests, clippy and fmt clean — over four defects the
suite could not observe. Every correction below was driven by an observed RED
against the code as it stood, never by reverting a fix afterwards.

- [x] 10.14 RED/GREEN: the drain loop `return`ed on a valid-transition
      `Ok(None)`. Observed RED: with a title change queued behind an
      active-window change naming an already-destroyed window, one
      `poll_for_event` answered `None` while the title event sat pending —
      Phase 9's task 9.18 defect reappearing at a different exit, and just as
      lost rather than delayed, since `x11rb` has already drained the fd. The
      loop now `continue`s; only a real event returns.
- [x] 10.15 RED/GREEN: `retarget_subscription` released the previous window's
      event mask *before* subscribing the new one, so a `BadWindow` on the
      subscribe left `active_window` naming an unsubscribed window that the
      `active_window == window` short-circuit then never re-subscribed.
      Observed RED: after the race, a title change on the tracked window never
      arrived (5 s timeout). Subscribing before releasing makes the failure
      path a no-op by construction, which is stronger than restoring state
      afterwards because the restore is itself a request that can fail.
- [x] 10.16 RED/GREEN: `read_title` in the drain loop was a third, untreated
      `BadWindow` surface — the module doc claimed tolerance "at both points
      it can occur" while this one propagated. Observed RED: a title change
      followed by the window's destruction surfaced
      `X11Error { error_kind: Window, request_name: "GetProperty" }` out of
      `poll_for_event`, which RF-6/RF-32 have the reactor read as connection
      loss. Now tolerated; the doc says three points.
- [x] 10.17 RED/GREEN: the 2048-byte read bound pre-truncated a title of
      four-byte codepoints to exactly 512 characters, so
      `truncate_with_ellipsis` took its no-op branch and the result was
      indistinguishable from a genuine 512-character title. Observed RED: a
      600-codepoint `U+1D11E` title came back as 512 characters with no
      ellipsis. The ellipsis now follows `reply.bytes_after` too, not only the
      character count.
- [x] 10.18 GREEN (mutation pin, no RED possible — `is_bad_window` was already
      correct): two mutations survived the whole suite, making it always true
      and making it accept any `X11Error` kind. Both turn connection loss into
      a silent `continue` at every call site, which is the opposite of what
      RF-6/RF-32 need. Validated by applying each mutation and observing the
      new pin — and only the new pin — fail, then restoring.

---

## Phase 11: X11 Capture — Absence Detection & Reconnection — `src/x11.rs` (3/3)

**Traces:** RF-4, RF-6, RF-25, RF-32.

- [x] 11.1 RED (validates assumption A-7): `SYNC`/`IDLETIME` alarm firing the
      positive transition at `afk_threshold_seconds` queries
      `ms_since_user_input` exactly once, at alarm-fire instant; the negative
      transition re-arms the alarm; no periodic idle-time query occurs
      between alarms (idle-detection "Event-driven absence detection", all
      three scenarios).
- [x] 11.2 GREEN: implement `SyncCreateAlarm`/`Trigger` registration and the
      alarm-fire handler surfacing `SourceEvent::UserIdle{idle_for}`/
      `UserActive`.
- [x] 11.3 RED: `SYNC` available → used, no degradation warning; `SYNC`
      unavailable + `MIT-SCREEN-SAVER` available → 30s polling timer, logged
      degradation; neither available → X11 absence detection disabled
      entirely, exactly one startup warning, daemon still starts (RF-25, all
      three scenarios).
- [x] 11.4 GREEN: implement the three-step degradation chain, none of the
      three outcomes blocking startup.
- [x] 11.5 RED: X11 connection lost mid-run closes the current interval and
      opens exactly one `unknown` interval, retrying from 500ms; three
      consecutive failed retries open no additional `unknown` intervals; a
      successful reconnection resumes capture and resets backoff to 500ms for
      the next outage (RF-6/RF-32, all three scenarios).
- [x] 11.6 GREEN: implement the exponential backoff (500ms → 1s → 2s → 4s →
      8s → 16s ceiling, ±20% jitter) and single-`unknown`-per-outage
      bookkeeping.
- [x] 11.7 RED (Xvfb E2E): three synthetic windows produce correct intervals
      within ≤1s tolerance, using an in-house `x11rb` helper binary for
      synthetic windows rather than an external `xdotool` dependency
      (proposal *Dependencies*; PRD §17 "three synthetic windows").
- [x] 11.8 GREEN: wire `x11.rs` end to end for the E2E harness; fix whatever
      11.7 surfaces.
- [x] 11.9 GREEN: implement the E2E readiness-poll helper on
      `_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW` with a bounded timeout,
      **replacing the PRD's fixed `sleep 1`** (E-3) — used here and reused
      verbatim by Phase 19's CI job.

### Phase 11 corrections (2026-09-17, from a third adversarial pass)

Phase 11 was green — 173 tests, clippy and fmt clean — over a real RF-4 defect
that loses the idle→active transition, plus two more the suite could not
observe. Every behavioural correction below was driven by an observed RED
against the code as it stood, never by reverting a fix afterwards.

- [x] 11.10 RED/GREEN (RF-4): the return transition is lost when the alarm
      is re-armed at the instant the away edge fires. Observed RED against the
      real code path, 120 trials of "connect, wait for `UserIdle`, return
      immediately": **8 returns lost (6.7%)**, each one latching the daemon in
      AFK; adding the arm-time read of task 11.11 alone still lost 7. A/B over three re-arm formulations, 60 trials each against a real
      Xvfb: `NegativeTransition` at the threshold lost **12/60**; the same
      with the counter attribute re-sent to force a server-side refresh lost
      **13/60**; `NegativeComparison` one millisecond below the threshold lost
      **0/60**. Every lost trial was one whose away alarm had fired at exactly
      the threshold value. GREEN: `alarm_trigger_value` + `flip_test_type` arm
      the return edge as a comparison below the threshold; re-measured at
      **0/120**. No timer and no poll was added — the module still issues no
      request at all between transitions, and
      `the_return_edge_reports_once_and_then_goes_quiet` pins that the
      level-triggered return edge stays a one-shot (RNF-2) rather than waking
      the daemon for as long as somebody keeps typing.
- [x] 11.11 RED/GREEN (RF-4): `on_sync_alarm_notify` now implements the
      arm-time `sync_query_counter` check the module doc already claimed,
      shared with `try_sync_idle` as `close_arm_time_gap`. Observed RED: with
      the user returning before the queued `AlarmNotify` was drained, no
      `UserActive` ever surfaced within 3s of continuous input. The read
      deliberately follows the arm rather than preceding it, so input landing
      in the gap produces a duplicate event instead of a lost one.
- [x] 11.12 RED/GREEN (RF-4): classify the alarm event from its own
      `counter_value`, never from the local `armed` field. Observed RED: a
      notification delivered a second time after the re-arm (an X11 wire proxy
      duplicates it, which the client cannot tell from the real queued-event
      race) was reported as `UserActive` with no user input at all.
- [x] 11.13 GREEN (coverage pin, no RED possible — the degraded steps were
      already correct): real behavioural coverage for RF-25 steps 2 and 3
      against genuine extension absence — `Xvfb -extension MIT-SCREEN-SAVER`
      for the server side, and an in-test X11 wire proxy rewriting
      `QueryExtension(SYNC)` for the extension the server refuses to disable.
      The false "only the Generic Event Extension can be toggled" claim is
      corrected in both module docs, in `select_degradation_diagnostic`, and
      in the `apply-progress` artifact. Validated by re-running the three
      `panic!()` mutants that previously survived the whole suite.
- [x] 11.14 GREEN: `wait_for_raw_event`'s deadline is back to 5s and the
      "contention" justification is gone; the measured worst case was 504ms.
- [x] 11.15 GREEN (mutation pin, no RED possible — `Reconnector` was already
      correct): the post-success backoff reset and the second outage's
      `OutageOpened` are asserted through `Reconnector` itself.
- [x] 11.16 RED/GREEN (RF-6): `ReconnectAttempt::Restored` now carries
      `outage_was_open`, so a first-attempt success tells its caller no
      `OutageOpened` preceded it and RF-6's one `unknown` interval is still
      owed. Observed RED against a stub that always answered "an outage was
      open".
- [x] 11.17 GREEN (doc): `RawEvent::UserIdle` records that the `SYNC` path
      takes `ms_since_user_input` from the `AlarmNotify`'s own `counter_value`
      rather than `XScreenSaverQueryInfo`, and why.
- [x] 11.18 GREEN (test harness): `spawn_xvfb` skips display numbers that are
      already answering instead of adopting a foreign X server. Observed
      failure: a leftover `Xvfb` from an earlier session made
      `title_change_on_the_tracked_window_surfaces_undebounced` receive
      `UserIdle { idle_for: 4027s }` — a real reading of a 67-minute-old
      display — instead of the title change it was waiting for.

---

## Phase 12: Session State — `src/logind.rs`

**Traces:** RF-5, RF-26, RF-27, RF-65, design §2 D-13.

- [x] 12.1 GREEN (trait first, no isolated RED): define `SessionMonitor`
      (`locked_hint`, `take_sleep_inhibitor`, `release_sleep_inhibitor`,
      `resolve_session`) and `FakeSessionMonitor` per design §2 D-13.
- [x] 12.2 RED (driven entirely by `FakeSessionMonitor`, no real D-Bus):
      `Lock()` observed but `LockedHint` still `false` → no transition;
      `LockedHint: false→true` → closes current interval, opens `locked` at
      the property-change instant; `LockedHint: true→false` → closes
      `locked`, opens `unknown` (RF-5, all three scenarios).
- [x] 12.3 GREEN: implement `LockedHint`-as-source-of-truth surfacing
      `SourceEvent::SessionLocked`/`SessionUnlocked`.
- [x] 12.4 RED: session resolves via `GetSessionByPID(<own PID>)`, never
      `$XDG_SESSION_ID`; a failed resolution logs, continues without
      lock/suspend awareness, and retries later rather than failing startup
      (RF-26, both scenarios).
- [x] 12.5 GREEN: implement `resolve_session` and the retry-needed signal
      consumed by Phase 14's `SessionReresolve` deadline.
- [x] 12.6 RED: the daemon takes a `delay` inhibitor at startup;
      `PrepareForSleep(true)` closes and commits the current interval
      (recorded as `locked`) before releasing the inhibitor;
      `PrepareForSleep(false)` opens `unknown` and re-acquires the inhibitor;
      a refused `Inhibit()` logs a warning and continues without blocking
      startup (RF-27, all three scenarios).
- [x] 12.7 GREEN: implement inhibitor acquisition/release and the
      `PrepareForSleep` handlers.
- [x] 12.8 RED: no D-Bus session bus reachable at startup → a warning is
      logged, session-state features are unavailable, window capture
      continues normally (RF-65).
- [x] 12.9 GREEN: implement the entirely-unreachable-D-Bus startup path.
- [x] 12.10 GREEN: implement `ZbusSessionMonitor` — the only file naming a
      `zbus` type (T-3 mitigation) — using `zbus::blocking` plus its own
      bridge thread and `eventfd` exactly per design §2 D-3, with
      `Builder::stack_size(64 * 1024)` on the bridge thread (RNF-1
      mitigation).
- [x] 12.11 REFACTOR: grep the crate for any `zbus` type outside
      `logind.rs`; confirm there is none.

---

## Phase 13: Signals & Control Channel — `src/signals.rs`, `src/control.rs`

**Traces:** RF-9 (signal half), RF-33 (signal half), RF-49, design §2 D-4, D-5.
Threat-matrix rows: "Process integration — control socket" (design §7).

- [x] 13.1 GREEN (registration is infrastructure, no isolated RED — validates
      assumption A-3 for real): register `SIGTERM`/`SIGINT`/`SIGHUP` with
      both `signal_hook::flag::register` (`Arc<AtomicBool>`) and
      `signal_hook::low_level::pipe::register` onto the same self-pipe
      (design §2 D-4).
- [x] 13.2 RED: a coalesced burst of `SIGTERM`+`SIGHUP` sent in quick
      succession is observed correctly as both flags set, with the pipe
      drained to empty (D-4's "levels, not edges" correctness).
- [x] 13.3 GREEN: implement flag-read-and-clear plus pipe-drain-to-empty.
- [x] 13.4 RED, one task per threat-matrix control-socket case (design §7,
      §5 wire-protocol constraints): 13.4a a request over 4096 bytes or
      missing a trailing `\n` is rejected; 13.4b `v != 1` returns
      `UnsupportedVersion`; 13.4c a connecting peer whose `SO_PEERCRED` uid
      differs from `geteuid()` is closed **without reading any bytes** and
      logged (daemon-lifecycle "A connection from a different UID is
      rejected without being read"); 13.4d a 5th concurrent client is
      accepted and immediately closed; 13.4e a client sending no line within
      1s is dropped; 13.4f `Pause` against an already-paused daemon and
      `Resume` against a non-paused daemon return `AlreadyPaused`/`NotPaused`.
- [x] 13.5 GREEN: implement the `UnixListener` on
      `$XDG_RUNTIME_DIR/xwindowlog.sock` (bound after `flock` succeeds — the
      actual `flock` wiring is Phase 15's task 15.4), the `Request`/
      `Response`/`Envelope`/`ErrCode` types (design §5), `SO_PEERCRED`
      check-before-read, the 4-concurrent-client cap with 1s per-client
      deadline, and stale-socket `unlink()`-before-bind.
- [x] 13.6 RED: the `pause`/`resume` CLI client code never writes to
      `intervals` — a module-boundary check that `control.rs`'s client-side
      helper has no dependency on `store` (daemon-lifecycle "pause/resume
      clients never write intervals directly").
- [x] 13.7 GREEN: implement the CLI-side `pause`/`resume` request senders
      (used by Phase 17's subcommands) as a thin client with no `store`
      dependency.
- [x] 13.8 REFACTOR: confirm `signals.rs`/`control.rs` unit-test in isolation
      from `reactor.rs` — they expose raw fds/event sources; `reactor.rs`
      (Phase 14) is what polls them.

---

## Phase 14: Reactor — `src/reactor.rs`

**Traces:** RF-1 (no-busy-wait half), RNF-2, design §2 D-2, D-6, D-12
(in-process counter half). Design's own "highest-risk logic" (§4 File
Changes rationale for splitting this module out).

- [x] 14.1 RED: `poll_timeout` rounds a 250.4 ms deadline **up** to 251ms,
      not truncated down to 250 (the D-2 spin-bug regression test, exercising
      the verified truncating behavior of `TryFrom<Duration> for
      PollTimeout`, design §12 V-3); an empty deadline set yields
      `PollTimeout::NONE`.
- [x] 14.2 GREEN: implement `Deadlines` (`DestroyGrace`, `TitleDebounce`,
      `ReconnectBackoff`, `PauseExpiry`, `SessionReresolve`) and
      `poll_timeout` with explicit round-up, driven by `FakeClock` — no real
      fds in this test.
- [x] 14.3 RED: `EINTR` from `poll(2)` retries without losing an
      already-armed deadline; budget exhaustion (`X11_BUDGET=64`/
      `DBUS_BUDGET=32`) yields `PollTimeout::ZERO`, not a sleep (D-6's
      fairness rule).
- [x] 14.4 GREEN: implement the main reactor loop per design §2 D-6 — flush
      outbound X11 requests, drain X11's userspace event queue up to budget
      before polling, drain the logind channel up to budget, apply effects,
      compute the timeout, `poll()`, handle `EINTR` by looping, and fire due
      deadlines **on every wakeup, not only on `Ok(0)`**.
- [x] 14.5 GREEN: assemble the permanent fd table (design §2 D-2): fd0 X11
      connection (Phase 9-11), fd1 logind bridge `eventfd` (Phase 12), fd2
      signal self-pipe (Phase 13), fd3 control-socket listener (Phase 13),
      plus up to 4 transient accepted control-client fds.
- [x] 14.6 RED: `ReactorSource` (`impl WindowSource for ReactorSource`) wakes
      exactly once for a synthetic X11 change with no prior read, and issues
      zero wakeups over a synthetic 60s idle window with nothing pending
      anywhere (window-capture "No busy-waiting between changes";
      daemon-lifecycle "Zero wakeups over an idle window") — the first place
      both scenarios are provable end to end, using synthetic fds.
- [x] 14.7 GREEN: implement `impl WindowSource for ReactorSource`
      translating fd readiness/deadline expiry into `SourceEvent`s.
- [x] 14.8 RED: control-socket `accept()` is limited to one per wakeup
      (bounds a connect-storm without a rate limiter, D-6).
- [x] 14.9 GREEN: implement accept-throttling.
- [x] 14.10 RED: a wakeup is always attributable to a real monitored source
      or a real armed deadline, never an unconditional periodic re-check
      (daemon-lifecycle "A wakeup is attributable to a real event or a real
      deadline") — an instrumentation test recording the cause of each
      observed wakeup during a scripted synthetic run.
- [x] 14.11 GREEN: implement the in-process `wakeups: u64` counter (D-12),
      incremented on every `poll()` return, logged at `debug` on shutdown —
      completed by Phase 19's per-thread `schedstat` half.
- [x] 14.12 REFACTOR: confirm `reactor.rs` contains no domain logic — it
      translates fd/deadline events into `SourceEvent`s and applies
      `Effect`s; state-transition decisions stay in `tracker.rs`.

---

## Phase 15: Daemon Composition — `src/main.rs`

**Traces:** RF-9 (E2E), RF-19 (partial — dispatch only), RF-20 (single-
instance half via flock config), RF-21, RF-33, RF-34, RF-36 (E2E), RF-49
(wiring), RNF-6 (E2E).

- [x] 15.1 GREEN (proven by the E2E tests below, which need the whole daemon
      wired): implement the `clap` CLI skeleton recognizing `daemon`,
      `status`, `today`, `pause`, `resume`, `prune`, `forget`, `completions`
      (RF-19 partial — argument parsing and dispatch only; subcommand
      behavior is Phases 16-17).
- [x] 15.2 GREEN: implement config load/validate from
      `$XDG_CONFIG_HOME/xwindowlog/config.toml` (or defaults if absent),
      wiring `afk_threshold_seconds`, `title_debounce_ms`, `mode`,
      `sanitize_secrets`, `status_show_title`, `retention_days`,
      `disable_default_excludes` into the constructed `Excluder`/`Tracker`.
- [x] 15.3 RED (E2E, real process): `flock(2)` `LOCK_EX|LOCK_NB` on
      `$XDG_RUNTIME_DIR/xwindowlog.lock`; a second instance exits non-zero
      with a clear message without killing/interrupting the first; a
      `SIGKILL`ed previous instance releases the lock automatically and the
      next instance starts normally (RF-21/RF-34, both scenarios).
- [x] 15.4 GREEN: implement the `flock` acquisition and the "another
      instance" exit path — this is also `status`'s "is the daemon running"
      mechanism (a non-blocking `flock` attempt), consumed by Phase 16.
- [x] 15.5 RED (E2E): `SIGTERM` closes the currently open interval with
      `end` = signal-receipt instant, commits the closure, releases the lock
      file and any held session inhibitor, exits `0`; `SIGINT` behaves
      identically (RF-33, both scenarios).
- [x] 15.6 GREEN: wire `signals.rs`'s `Shutdown` event through `tracker.rs`'s
      `CloseOnly` effect to `store.rs`, then lock/inhibitor release and exit.
- [ ] 15.7 RED (E2E): editing `config.toml` and sending `SIGHUP` causes
      subsequent captures to use the updated exclusion rules without
      affecting already-recorded intervals (RF-9, completing Phase 7's
      unit-level `Excluder`-swap test end to end).
- [ ] 15.8 GREEN: wire `SourceEvent::ReloadConfig` to the `Excluder`
      hot-swap inside `ReactorSource`, invisible to the tracker.
- [ ] 15.9 RED (E2E, `SIGKILL` + restart): killing the daemon mid-run and
      restarting it recovers to a consistent, uncorrupted database, Phase 3's
      startup recovery closing the stale interval (RNF-6, "SIGKILL followed
      by restart recovers to a consistent state").
- [ ] 15.10 GREEN: fix whatever full-process composition gaps 15.9 surfaces
      (this is the RNF-6/RF-36 end-to-end proof, not new logic).
- [ ] 15.11 GREEN: wire the control-socket `Pause`/`Resume` requests (Phase
      13) through `tracker.rs`'s `Pause{until}`/`Resume` events to
      `store.rs`, including the `PauseExpiry` deadline (monotonic + wall-
      clock target per design's dual-clock rationale) — RED tests for full
      pause/resume behavior live in Phase 17 with the CLI client.
- [ ] 15.12 REFACTOR: confirm `main.rs` contains only composition (clap,
      flock, config, reactor construction, effect application) with no
      state-machine or storage logic duplicated from `tracker.rs`/`store.rs`.
- [ ] 15.13 REFACTOR (**final interim-debt sweep, added by the orchestrator
      after PR 5**): once `main.rs` wires the daemon together, every module
      has real consumers, so no module-level `#![allow(dead_code)]` is
      justified any more. Remove every one of them — `tracker.rs` acquired
      one in Phase 5 for the same reason `clock.rs` did, and others may have
      since. These allows are honest while a module deliberately lands ahead
      of its consumers under design §8's ordering, but a blanket allow that
      outlives its reason hides genuinely dead code for the rest of the
      project's life. Acceptance: `grep -rn 'allow(dead_code' src/` returns
      nothing, and `cargo clippy --all-targets -- -D warnings` is still
      clean. If one item is legitimately unused even now, narrow the allow to
      that item with its own `reason` — never leave it at module scope.

---

## Phase 16: CLI — `status` / `today`

**Traces:** RF-54, RF-61 (partial), RF-63, RF-66 (consumption), RF-19
(partial).

- [ ] 16.1 RED: `xwindowlog today` reads exclusively from the persisted
      store (never X11 or in-memory daemon state), reflecting a `[hidden]`
      interval exactly as stored (cli-reporting "today reflects only
      persisted, sanitized data").
- [ ] 16.2 GREEN: implement `today` using Phase 4's clipping CTE against
      `[start_of_day, end_of_day)`.
- [ ] 16.3 RED: `status_show_title` defaults to `false` (shows `app_id` +
      elapsed time, no title); `status_show_title = true` includes the title
      (RF-54, both scenarios).
- [ ] 16.4 GREEN: implement `status`'s default-hidden-title behavior.
- [ ] 16.5 RED: default `status` output is exactly one line in the
      documented format (`xwindowlog: Editor · project-x · 2h34m today`); the
      paused state (Phase 15's pause wiring) is visibly shown instead of an
      app name (RF-63, both scenarios).
- [ ] 16.6 GREEN: implement the single-line status-bar formatter and its
      paused-state branch.
- [ ] 16.7 RED: `status --json`/`today --json` include an explicit
      schema-version field, and a dedicated test asserts its presence and
      value, failing if it is removed or changed without a matching test
      update (RF-61, both scenarios).
- [ ] 16.8 GREEN: implement versioned `--json` output for `status`/`today`.
- [ ] 16.9 REFACTOR: confirm `today`/`status` share the clipping-CTE query
      path with no independent SQL duplicating Phase 4's logic.

---

## Phase 17: CLI — `pause` / `resume` / `prune` / `forget` / `completions`

**Traces:** RF-13 (CLI half), RF-49 (CLI half), RF-53 (CLI half), RF-60,
RF-62, RF-19 (remaining).

- [ ] 17.1 RED: `xwindowlog pause` (via Phase 13's client) closes the current
      interval and opens `paused`; no `active` interval opens again until
      `resume` (daemon-lifecycle "pause opens a paused interval").
- [ ] 17.2 GREEN: implement the `pause` subcommand dispatch to the
      control-socket client.
- [ ] 17.3 RED: `pause --minutes 30` expires automatically after 30 minutes
      with no `resume` issued; a pause spanning a 20-minute suspend still
      ends at ~30 minutes of real elapsed time, not 50 (daemon-lifecycle,
      both `--minutes` scenarios) — the CLI-triggered E2E proof of the
      dual-clock logic already built in Phases 6/14.
- [ ] 17.4 GREEN: confirm the CLI-triggered path reaches the already-
      implemented `PauseExpiry` dual-clock logic.
- [ ] 17.5 RED: `resume` closes the `paused` interval and transitions to
      `active` or `unknown` depending on current window state
      (daemon-lifecycle "resume ends an active pause").
- [ ] 17.6 GREEN: implement the `resume` subcommand.
- [ ] 17.7 RED: `pause` against an already-paused daemon and `resume`
      against a non-paused daemon each exit `2` with the matching stderr
      message (cli-reporting, both "exits 2" scenarios), reusing Phase 13's
      `AlreadyPaused`/`NotPaused` error codes.
- [ ] 17.8 GREEN: map `ErrCode::AlreadyPaused`/`NotPaused` to CLI exit code
      `2`.
- [ ] 17.9 RED: `pause` when no daemon is running (socket absent or refusing
      connections) reports clearly and exits non-zero rather than hanging or
      silently succeeding (daemon-lifecycle "pause fails cleanly when the
      daemon is not running").
- [ ] 17.10 GREEN: implement the connect-failure path with a bounded connect
      timeout.
- [ ] 17.11 RED: `prune --older-than <duration>` and `prune --vacuum-only`
      CLI wiring dispatch to Phase 4's store logic; `prune` exits `2` (not
      `1`) when `VACUUM` exhausts its retries, with the exact actionable
      stderr (cli-reporting "prune exits 2").
- [ ] 17.12 GREEN: implement the `prune` subcommand.
- [ ] 17.13 RED: `forget --from/--to [--yes]` and `forget --window <id>
      --yes` CLI wiring dispatch to Phase 4's store logic, including the
      interactive confirmation prompt.
- [ ] 17.14 GREEN: implement the `forget` subcommand.
- [ ] 17.15 RED (sweep across every subcommand, RF-60): stdout carries only
      primary output; every log/warning/progress line goes to stderr, never
      intermixed; success exits `0`; a second `daemon` invocation exits `2`;
      startup with no reachable X11 exits `3` (cli-reporting, all five
      exit-code scenarios).
- [ ] 17.16 GREEN: fix any subcommand whose exit code or stream discipline
      the sweep in 17.15 catches.
- [ ] 17.17 RED: `xwindowlog completions bash|zsh|fish` each produce
      non-empty, shell-appropriate output on stdout with exit `0`; each
      generated script loads into its shell without a syntax error in a
      smoke test (RF-62, both "Completions" scenarios).
- [ ] 17.18 GREEN: implement `completions` via `clap_complete` and the
      completions smoke test.
- [ ] 17.19 GREEN: wire `clap_mangen` man-page generation at build time
      (RF-62 "Man page is generated at build time" — a build-time check, not
      a RED/GREEN behavior pair).
- [ ] 17.20 RED: every one of `daemon`, `status`, `today`, `pause`,
      `resume`, `prune`, `forget`, `completions` responds to `--help` with a
      real help message, not an "unrecognized subcommand" error
      (cli-reporting "All Phase 1 subcommands are recognized").
- [ ] 17.21 GREEN: fix any missing `--help` wiring 17.20 catches.
- [ ] 17.22 REFACTOR: confirm every subcommand's exit-code mapping lives in
      one place, so RF-60's contract stays reviewable as a single table
      against the code.
- [ ] 17.23 RED (**added by the orchestrator after PR 4**): `prune` must not
      report plain success when the on-disk file did not actually shrink.
      `vacuum_with_retry` runs `PRAGMA wal_checkpoint(TRUNCATE)` after a
      successful `VACUUM`, because in WAL mode `VACUUM` alone only lowers the
      logical page count and the file stays at its old size while any
      connection holds it open. That checkpoint is deliberately best-effort —
      a blocked checkpoint must not fail an already-successful `VACUUM` — but
      its result is currently discarded with `let _ =`, so a blocked
      checkpoint is indistinguishable from a full one. RF-13 says retention
      means the file shrinks *to the user*, so silently returning
      `VacuumOutcome::Vacuumed` in that case tells them something untrue.
      Write the test first: with the checkpoint unable to truncate, `prune`
      reports reclaimed-but-not-yet-truncated, not plain success.
- [ ] 17.24 GREEN: split the outcome (for example
      `VacuumOutcome::VacuumedNotTruncated`) by inspecting the checkpoint
      result instead of discarding it, and have the CLI say plainly that
      space was reclaimed inside the database and the file will shrink once
      other connections release it. Keep the checkpoint best-effort: report
      the difference, never fail the `VACUUM` over it.

---

## Phase 18: Service Packaging — systemd & contrib

**Traces:** RF-20, RF-21 (unit half), design §2 D-11 (timer half).

- [ ] 18.1 RED: `contrib/xwindowlog.service`'s `[Unit]` section contains
      both `After=graphical-session.target` and
      `PartOf=graphical-session.target` (RF-20 "Unit declares both ordering
      and grouping directives").
- [ ] 18.2 GREEN: write `contrib/xwindowlog.service` with `After=`/
      `PartOf=graphical-session.target`, `UMask=0077`,
      `RestrictAddressFamilies=AF_UNIX`, `IPAddressDeny=any`,
      `ReadWritePaths` covering `$XDG_DATA_HOME/xwindowlog` and `%t`,
      `LimitCORE=0`.
- [ ] 18.3 RED (threat-matrix "Service integration — systemd units", design
      §7): confirm `PartOf=` is present and, where the harness supports it,
      the unit actually stops as part of the graphical-session-target
      transition (RF-20 "Daemon stops when the graphical session ends") —
      mark best-effort/skip-with-reason where the CI sandbox cannot exercise
      a real logout transition, rather than silently omitting the case.
- [ ] 18.4 GREEN: fix any unit-file gap 18.3 finds.
- [ ] 18.5 GREEN: write `contrib/config.example.toml` with the full §11.2
      Phase 1 key set (`afk_threshold_seconds`, `title_debounce_ms`, `mode`,
      `sanitize_secrets`, `status_show_title`, `retention_days = 365`, plus
      example `[[exclude]]` rules) — `mcp_max_range_days`/`week_start` are
      Phase 2/3 keys, deliberately omitted here as out of scope.
- [ ] 18.6 GREEN: write `contrib/xwindowlog-prune.timer`
      (`OnCalendar=daily`, `RandomizedDelaySec=1h`, `Persistent=true`) and
      `contrib/xwindowlog-prune.service` (`Type=oneshot`, deliberately
      **no** `Conflicts=`/`ExecStartPre=stop`, per D-11's rationale for
      allowing timer/daemon concurrency).
- [ ] 18.7 REFACTOR: cross-check `contrib/xwindowlog.service`'s
      `RestrictAddressFamilies=AF_UNIX`/`IPAddressDeny=any` against Phase
      19's `/proc/self/fd` runtime test — both halves of RNF-5 must agree on
      what "no network" means.

---

## Phase 19: Measurement & CI Gates

**Traces:** RNF-1, RNF-2, RNF-3, RNF-4, RNF-5 (runtime half), RNF-11.

- [ ] 19.1 RED: `/proc/self/fd` after 60s of normal operation contains only
      Unix-domain sockets, pipes, or regular files — never
      `AF_INET`/`AF_INET6` (RNF-5, "No network-family file descriptors after
      60 seconds of operation"; the unit-file half is Phase 18's 18.1-18.4).
- [ ] 19.2 GREEN: implement the runtime FD-family enumeration test.
- [ ] 19.3 GREEN: implement the RNF-1 accelerated soak (`RssAnon` after a
      compressed 8-hour simulation), recording the measured figure
      numerically, gated 8 MB hard / 5 MB SHOULD.
- [ ] 19.4 GREEN: implement the RNF-2 local benchmark completing D-12's two
      instruments — Phase 14's in-process `wakeups` counter for the reactor
      thread (asserting exactly 0 over 60s idle), plus per-thread
      `/proc/self/task/<tid>/schedstat` sampling at t/t+60s for every thread,
      reporting each thread's count honestly rather than claiming zero for
      threads this design does not own. Not a CI gate (§15.6) — documented
      local benchmark only.
- [ ] 19.5 GREEN: implement the RNF-2 CPU-budget benchmark (<0.5% average
      CPU over a representative window-switching/idle-transition period).
- [ ] 19.6 GREEN: implement the RNF-3 binary-size baseline-and-regression CI
      gate — first measurement records the baseline; later builds fail if
      they regress beyond the configured margin (proposal *Scope
      adjustments*: a Phase-1-appropriate baseline, re-baselined in Phase 2).
- [ ] 19.7 GREEN: implement the RNF-4 startup-latency benchmark (<50ms,
      multi-run stable statistic, not a single sample) as a hard CI gate.
- [ ] 19.8 RED: the crate builds and `cargo test` passes on exactly the
      pinned MSRV (RNF-11 "CI builds against the pinned MSRV").
- [ ] 19.9 GREEN: add the dedicated MSRV CI job.
- [ ] 19.10 GREEN: assemble `.github/workflows/ci.yml` — stable+beta matrix,
      the MSRV job, the Xvfb+`openbox --sm-disable` job using Phase 11's
      readiness-poll helper (never a fixed `sleep`, E-3), and the NFR gates
      per §15.6 exactly as the proposal's *Approach* specifies (RNF-4/RNF-3
      hard gates, RNF-1 gate-with-margin, RNF-2 documented-not-gated, RNF-5
      structural + runtime-verified).
- [ ] 19.11 REFACTOR: confirm every numeric measurement in this phase
      (RNF-1..4, per-thread wakeups) is recorded as an explicit number in CI
      output/PR description, not asserted qualitatively.

---

## Phase 20: README & PRD Amendments

**Traces:** RF-63 (documentation half), proposal assumption A-5.

- [ ] 20.1 GREEN: write `README.md` with the frozen `today` output as a
      literal example (matching Phase 16's fixture-tested format exactly)
      and the frozen `status` single-line and `--json` examples, plus
      documented usage for i3blocks, waybar, and polybar without coupling
      the format to any one of them (RF-63).
- [ ] 20.2 RED: the README's literal `today` example, run against an
      equivalent fixture database, produces output matching the documented
      format byte-for-byte (cli-reporting "today's output format matches the
      documented example").
- [ ] 20.3 GREEN: fix any drift 20.2 finds between the README example and
      the actual formatter.
- [ ] 20.4 GREEN: write the §14.6 "what this does not protect against"
      section, including the D-5 control-socket honesty line ("the control
      socket does not widen the threat model" — a same-uid process could
      already read `xwindowlog.db`).
- [ ] 20.5 GREEN: apply the four PRD amendments authorized under proposal
      assumption A-5, each acknowledged individually rather than applied
      silently: (a) `PRD.md` §13 — reword "no async runtime in the daemon"
      per design §2 D-3's corrected wording; (b) `PRD.md` RNF-5 — add the
      `/proc/self/fd` verification note; (c) `PRD.md` RF-49 — add the
      control-channel specification from design §2 D-5; (d) `PRD.md`
      §17/§15.6 — the NFR-gating clarification per proposal *Approach*.
- [ ] 20.6 REFACTOR: final pass confirming every Phase 1 success-criterion
      checkbox in `openspec/changes/phase-1-daemon/proposal.md` (read-only)
      has a corresponding passing test named somewhere in this document's
      phases; report any gap rather than closing the phase silently.

---

## Traceability Summary

| RF/RNF/P | Phase(s) |
|---|---|
| RF-1 | 5, 9, 14 |
| RF-2 | 5, 9 |
| RF-3 | 2, 4, 5, 6 |
| RF-4 | 6, 11 |
| RF-5 | 12 |
| RF-6 | 11 |
| RF-7 | 7 |
| RF-8 | 7 |
| RF-9 | 7, 15 |
| RF-10 | 3 |
| RF-11 | 3 |
| RF-12 | 4 |
| RF-13 | 4, 17 |
| RF-19 (partial) | 15, 16, 17 |
| RF-20 | 18 |
| RF-21 | 15, 18 |
| RF-22 | 10 |
| RF-23 | 6, 10 |
| RF-24 | 9 |
| RF-25 | 11 |
| RF-26 | 12 |
| RF-27 | 12 |
| RF-28 | 2 |
| RF-29 | 9 |
| RF-30 | 6, 10 |
| RF-31 | 10 |
| RF-32 | 11 |
| RF-33 | 15 |
| RF-34 | 15 |
| RF-35 | 3 |
| RF-36 | 3, 15 |
| RF-47 | 7 |
| RF-48 | 7 |
| RF-49 | 13, 15, 17 |
| RF-50 | 7 |
| RF-51 | 7 |
| RF-52 | 4 |
| RF-53 | 4, 17 |
| RF-54 | 16 |
| RF-60 | 17 |
| RF-61 | 16 |
| RF-62 | 17 |
| RF-63 | 16, 20 |
| RF-65 | 12 |
| RF-66 | 4, 16 |
| RNF-1 | 12, 19 |
| RNF-2 | 14, 19 |
| RNF-3 | 19 |
| RNF-4 | 19 |
| RNF-5 | 18, 19 |
| RNF-6 | 3, 4, 15 |
| RNF-11 | 1, 19 |
| P1 | 6 |
| P2 | 6 |
| P3 | 7, 8 |

RF-14..18, RF-38..46, RF-55..59, RNF-7..10, RNF-12 (MCP), RF-17/41/45/59/64
(rules/projects behavior), RF-37 (v2 cache) are out of scope per the
proposal and carry no task above.
