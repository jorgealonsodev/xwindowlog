# Proposal: Phase 1 — Daemon

**Change:** `phase-1-daemon` · **Date:** 2026-09-17 · **Store:** hybrid
**Engram mirror:** topic `sdd/phase-1-daemon/proposal`
**Upstream:** `openspec/changes/phase-1-daemon/exploration.md` · Engram `sdd/phase-1-daemon/explore` (#6195)

## Intent

`xwindowlog` has a 765-line PRD and no code. Phase 1 turns the PRD into the
first thing that can be *used*: a resident daemon that records what window was
active, for how long, and when the user was away, plus the two CLI verbs
(`today`, `status`) that read it back without any AI in the loop.

Three reasons this is the right thing to build first, and to build whole:

1. **Everything else is downstream of it.** MCP (Phase 2) and rules (Phase 3)
   are interpretations of the interval table. Until intervals are correct and
   contiguous, every later phase is building on sand.
2. **The CLI is the product's real adoption path, not an accessory.** PRD §6
   says so explicitly: personas P1 (freelancer) and P2 (employee) are the
   highest-volume profiles and are the *worst* served by MCP, because it
   requires an open Claude Desktop and a paid subscription. Phase 1 is the only
   phase that delivers standalone value to them.
3. **It is the phase that starts persisting sensitive data.** From the first
   `cargo run --  daemon`, window titles land on disk in plaintext. The privacy
   machinery (default exclusion list, `hide_app`, secret sanitization, `0600`
   permissions, `forget`) has to ship *with* the capture, not after it.
   Shipping capture first and privacy later would be indefensible.

Success looks like PRD metric **M-1** holding as an automated test: for a full
simulated day, `active + afk + locked + paused + unknown == end − start`,
exactly. That is property P2 in §15.5, and PRD §17 names it the phase's real
closing condition. Everything else in this proposal exists to make that
invariant true and keep it true.

## Scope

### In Scope

This change covers the **full Annex B Phase 1 range** — RF-1..RF-13,
RF-20..RF-36, RF-47..RF-54, RF-60..RF-63, RNF-1..RNF-6 and RNF-11 — with the
three deliberate adjustments listed under *Scope adjustments* below. Concretely:

- **Crate bootstrap.** `Cargo.toml` (edition 2021, release profile, pinned
  MSRV per RNF-11), module skeletons, and a verified-runnable `cargo test`.
  Phase 1 dependencies only — explicitly **no `rmcp`, no `tokio`**.
- **X11 capture.** `_NET_ACTIVE_WINDOW` subscription, unconditional
  post-change property read, the BadWindow race (RF-22), the `DestroyNotify`
  250 ms safety net (RF-23), EWMH verification with `GetInputFocus` fallback
  (RF-24), XWayland warning (RF-29), title debounce (RF-30), 512-char
  truncation and `/proc/<pid>/comm` fallback (RF-31), reconnection backoff
  (RF-32).
- **Absence detection.** `SYNC`/`IDLETIME` alarms with the full three-step
  degradation chain to `MIT-SCREEN-SAVER` and then to logind-only (RF-4,
  RF-25).
- **Session state.** `zbus::blocking` against `org.freedesktop.login1`:
  `LockedHint` as source of truth (RF-5), `GetSessionByPID` resolution (RF-26),
  `PrepareForSleep` with a held `delay` inhibitor (RF-27).
- **Interval tracking.** The §11.1 state machine over the `WindowSource`
  trait, strict contiguity (RF-3), and the wall/monotonic clock discipline
  including the backwards-NTP-jump clamp (RF-28).
- **Privacy filtering.** Config-file exclusion (RF-7), `[hidden]` with time
  preserved (RF-8), `SIGHUP` reload (RF-9), `hide_app` (RF-47), the built-in
  default exclusion list with per-id opt-out (RF-48), `allowlist` mode (RF-50),
  `sanitize_secrets` (RF-51).
- **Storage.** WAL schema of RF-11 with sentinels, `umask(0o077)` before open
  (RF-10), write policy (RF-12, **as amended** — see Risk S-1), forward-only
  migrations with Online-Backup-API pre-migration backup (RF-35), startup
  recovery (RF-36), `prune` (RF-13), `forget` (RF-53), and the interval-clipping
  CTE that `today` depends on.
- **Daemon lifecycle.** The single-threaded multiplexed event loop, `SIGTERM`/
  `SIGINT` clean shutdown (RF-33), `flock(2)` single instance (RF-34),
  `pause`/`resume` control channel and `paused` state (RF-49), config load and
  validation, `contrib/xwindowlog.service` hardened per §14.5 (RF-20, RF-21).
- **CLI surface.** `daemon`, `today`, `status` (with `status_show_title = false`
  default, RF-54, and the status-bar line format of RF-63), `pause`, `resume`,
  `prune`, `forget`, `completions`; stdout/stderr discipline and the 0/1/2/3
  exit-code contract across **all** of them (RF-60); versioned `--json`
  (RF-61); `clap_complete` + `clap_mangen` output (RF-62).
- **Measurement and CI.** Xvfb + `openbox --sm-disable` integration job, the
  `proptest` invariants P1–P3, the automated permissions test covering
  `-wal`/`-shm`, the `SIGKILL` crash-recovery test, and RNF-1..RNF-4
  measurement per the gate policy in *Approach*.
- **`README.md`** with the frozen `today`/`status` output as a literal example,
  plus the §14.6 "what this does not protect against" section.

### Out of Scope

- **MCP entirely** — RF-14..RF-18, RF-38..RF-40, RF-46, RF-55..RF-58, RNF-7..
  RNF-10, RNF-12. No `rmcp`, no `tokio`, no `mcp.rs`, no `install`.
- **Rules and projects behaviour** — RF-17, RF-41, RF-45, RF-59, RF-64
  (`doctor`), `export`. The `rules`/`projects` *tables* are created (see A-4);
  nothing reads or writes them in Phase 1.
- **The automatic retention trigger.** Annex B assigns it to Phase 4. Phase 1
  ships the `retention_days = 365` key (D-3), `contrib/xwindowlog-prune.timer`
  and the documentation; nothing prunes on its own.
- **Packaging** — musl, `.deb`, `PKGBUILD`, checksums, signing (D-5).
- **Wayland** (D-4) and **RF-37** (materialized cache, v2).
- **Deciding D-2.** See A-1.

### Scope adjustments

Three places where this proposal does not simply mirror Annex B, each stated
plainly:

- **`forget` (RF-53) is kept in, against §17's silence.** Annex B's RF-47..RF-54
  range includes it, but the §17 Phase 1 checklist never exercises it, which
  reads like an invitation to defer. Deferring it is wrong: Phase 1 is the phase
  that begins writing plaintext titles to disk, and shipping capture without a
  way to delete a specific stretch is a privacy regression the user cannot work
  around. It is also cheap — it shares `prune`'s delete/orphan-cleanup/`VACUUM`
  machinery and differs only in the `WHERE` clause and an interactive
  confirmation.
- **RNF-3 is measured, not gated at 6–8 MB.** The 6–8 MB figure in RNF-3 is
  explicitly the size of the *shipped single binary that links tokio*. The
  Phase 1 binary does not link tokio, so a "fail above 8 MB" CI gate here would
  pass trivially and certify nothing. Phase 1 records the actual figure as a
  baseline and gates against a Phase-1-appropriate threshold derived from the
  first measurement; the real RNF-3 gate is re-baselined in Phase 2 when
  `rmcp`/`tokio` land. Stated so it is a decision, not an oversight.
- **RF-19 is delivered partially, by necessity.** Annex B assigns RF-19 (the
  full subcommand list) to no phase at all — a genuine traceability hole. Phase 1
  delivers the subset above; `mcp`/`install` land in Phase 2 and
  `export`/`doctor` in Phase 3. This is called out so the gap is closed on
  paper, not discovered at archive time.

## Capabilities

### New Capabilities

`openspec/specs/` is empty; everything below is new.

- `window-capture`: X11 active-window and title acquisition — subscription,
  unconditional property reads, the destruction/event-mask races, EWMH
  verification and degradation, XWayland detection, debounce, title decoding and
  truncation, reconnection backoff. (RF-1, RF-2, RF-6, RF-22, RF-23, RF-24,
  RF-29, RF-30, RF-31, RF-32)
- `idle-detection`: event-driven absence via `SYNC` alarms on `IDLETIME`, the
  backdated closing timestamp, and the three-step degradation chain. (RF-4,
  RF-25)
- `session-state`: logind integration — session resolution, `LockedHint` as
  source of truth, suspend handling with a delay inhibitor, and graceful
  degradation when `Inhibit()` is refused. (RF-5, RF-26, RF-27)
- `interval-tracking`: the tracker state machine, the §11.1 transition table,
  strict contiguity, and wall-vs-monotonic clock discipline. (RF-3, RF-28,
  properties P1 and P2)
- `privacy-filtering`: exclusion and sanitization — config rules, the built-in
  default list with per-id opt-out, `hide_app`, `allowlist` inversion, secret
  redaction, and the §14.3 ordering guarantee that no raw title escapes before
  this layer. (RF-7, RF-8, RF-9, RF-47, RF-48, RF-50, RF-51, property P3)
- `interval-storage`: SQLite persistence — schema and sentinels, file-mode
  discipline, the write/durability policy, forward-only migrations with backup,
  startup recovery, interval clipping, `prune` and `forget`. (RF-10, RF-11,
  RF-12, RF-13, RF-35, RF-36, RF-53, RNF-6)
- `daemon-lifecycle`: process composition — the multiplexed single-threaded
  reactor, signal handling, single-instance locking, the `pause`/`resume`
  control channel and `paused` state, config loading/validation/reload, and the
  hardened systemd `--user` unit. (RF-20, RF-21, RF-33, RF-34, RF-49, RNF-5)
- `cli-reporting`: the user-facing command surface — `today`, `status` and its
  status-bar format, the stdout/stderr and exit-code contract, versioned
  `--json`, completions and man pages. (RF-54, RF-60, RF-61, RF-62, RF-63,
  RF-19 partial)

### Modified Capabilities

None. Greenfield.

## Approach

Build inward-out from what is testable without hardware, then attach the two
external systems last.

1. **Bootstrap first, and prove the runner.** `Cargo.toml` plus module stubs is
   the single documented exemption from RED-first TDD (Engram
   `sdd/xwindowlog/testing-capabilities`), because it is what *creates* the test
   runner. It must also prove the two environment preconditions that would
   otherwise fail silently later: a working C toolchain for `rusqlite bundled`,
   and a vendored SQLite ≥ 3.31 for the generated-column index. Both are checked
   here, not assumed.
2. **`store.rs`, `tracker.rs` and `exclude.rs` before any real I/O.** All three
   are fully testable with `Connection::open_in_memory()` and synthetic events —
   this is the PRD's own stated design intent (§13, the `WindowSource` trait).
   This is deliberate sequencing, not convenience: it means the P1/P2/P3
   invariants are provable before a single X11 byte is read, so when capture
   later produces a wrong interval, the bug is provably in capture.
3. **Wire the pipeline and lock the invariants.** tracker → exclude → store with
   the `proptest` properties. P2 is the phase's acceptance hinge and should be
   green well before the phase ends, not at the end.
4. **Then `x11.rs`, then `logind.rs`.** These are the modules that need Xvfb and
   a D-Bus double. They are last because they are the only ones whose failures
   can be environmental rather than logical.
5. **Then `main.rs`: the reactor.** Composition is deliberately last, because
   the reactor's shape depends on how many descriptors the earlier modules
   actually expose.
6. **Then service, CLI polish, measurement, README.**

**NFR gate policy.** Where PRD §17 and §15.6 disagree on which of RNF-1..RNF-4
are hard CI gates, **this change follows §15.6**. It is the more specific and
more considered statement, it is the one that explains *why* each threshold is
set where it is, and §17's phrasing ("RNF-1 to RNF-4 measured with a concrete
tool and threshold") is a summary bullet, not a gate specification. Concretely:
RNF-4 is a hard CI gate; RNF-3 is a hard gate against a Phase-1 baseline (see
*Scope adjustments*); RNF-1 is a CI gate with margin (fail above 8 MB) plus a
recorded exact figure; **RNF-2 is a documented local benchmark and explicitly
not a CI gate**, because virtualized runners cannot honestly measure "zero
wakeups". RNF-5 is structural (no network crate in the dependency graph, plus
the systemd `RestrictAddressFamilies=AF_UNIX`/`IPAddressDeny=any` directives) and
RNF-6 is certified by the `SIGKILL` crash-recovery test, not by a benchmark.
This should be reflected in the PRD so §17 stops contradicting §15.6.

**Two architecture questions this proposal does not answer, and hands to
`sdd-design` as required input:**

- **The multiplexed reactor (see Risk A-1).** The single largest unaddressed
  gap in the PRD.
- **The `pause`/`resume` control channel (see Risk A-2).** Smaller, but it
  changes the reactor's descriptor count, so it must be answered *with* the
  reactor and not after it.

**Delivery.** Roughly 14–16 PR-sized slices under the session's `auto-chain`
strategy, with `x11.rs` warranting three on its own. `sdd-tasks` owns the exact
decomposition and the 400-line forecast.

## Affected Areas

| Area | Impact | Description |
|---|---|---|
| `Cargo.toml` | New | Phase 1 deps only, release profile, MSRV pin |
| `src/main.rs` | New | clap CLI, reactor composition, signals, `flock`, config |
| `src/x11.rs` | New | x11rb capture, SYNC/IDLETIME, EWMH checks, backoff |
| `src/logind.rs` | New | `zbus::blocking` login1 proxy, inhibitor |
| `src/tracker.rs` | New | State machine, `WindowSource` trait, clock discipline |
| `src/exclude.rs` | New | Exclusion, default list, allowlist, secret redaction |
| `src/store.rs` | New | Schema, migrations, write policy, recovery, clipping, prune/forget |
| `contrib/xwindowlog.service` | New | Hardened systemd `--user` unit (§14.5) |
| `contrib/config.example.toml` | New | Example config, `retention_days = 365` |
| `contrib/xwindowlog-prune.timer` | New | Opt-in pruning timer (RF-52, v1 = manual install) |
| `tests/` | New | Unit, in-memory SQLite, `x11_integration`, `invariants.rs` |
| `.github/workflows/` | New | stable+beta matrix, MSRV job, Xvfb job, NFR gates |
| `README.md` | New | Frozen output formats, privacy limitations (§14.6) |
| `PRD.md` | Modified | Corrections for Risks S-1 and A-2, and the §17/§15.6 gate wording |

## Risks

Ordered by how much they can hurt. `A-*` are architecture gaps in the PRD,
`S-*` are specification defects, `E-*` are environmental.

| id | Risk | Likelihood | Mitigation |
|---|---|---|---|
| **A-1** | **The three-descriptor reactor has no off-the-shelf solution.** The daemon must wait on the X11 socket, the D-Bus socket (`zbus::blocking`) and a `signal-hook` self-pipe simultaneously, in one thread, with no async runtime. Neither `x11rb`'s blocking API nor `zbus::blocking` composes with the other or with `signal-hook`. This requires a hand-rolled `poll(2)`/`epoll` reactor over raw fds, and **RNF-2 depends entirely on getting it right**. It is an implicit assumption in the PRD, never a specified architecture. | High | **Explicitly handed to `sdd-design` as the first thing it must answer.** Not to be discovered during `sdd-apply`. Design must also state the timer story: RF-23's 250 ms one-shot and the RF-32 backoff both need a timeout arm on the same wait, without becoming a poll. |
| **S-1** | **RF-12's deferred closes are unimplementable as written.** RF-12.2 defers interval closes to a 30 s batch, but RF-3 requires the next interval to open at the *exact same instant*, RF-12.1 requires that open to be written immediately, and RF-11's `idx_intervals_one_open` permits at most one row with `"end" IS NULL`. A deferred close plus an immediate open is therefore a `UNIQUE` violation on the most common transition in the system (`active` → `active`). | **Certain** — it is a logical contradiction, not a probability | Recommended resolution: **a state transition is one atomic transaction** (`UPDATE` the close and `INSERT` the open together); the "batch" is reinterpreted as the WAL/`synchronous = NORMAL` durability policy it already relies on, and optionally as batching of `apps`/`titles` dictionary upserts. This *strengthens* RF-12's stated worst case rather than weakening it. `sdd-design` decides and the PRD is corrected to match. Blocks the `interval-storage` spec until resolved. |
| **A-2** | **The `pause`/`resume` control channel is unspecified.** RF-49 says `xwindowlog pause [--minutes N]` produces a `paused` interval, but `pause` is a separate process and the PRD names no mechanism. It cannot write the interval itself without racing the daemon's buffer and the one-open-interval index. Signals cannot carry `--minutes N`. A unix socket or a watched state file adds a fourth descriptor to A-1's reactor. | High | Must be decided **together with** A-1, not after it, because it changes the reactor's descriptor set. Handed to `sdd-design`. |
| **E-1** | **`rusqlite bundled` needs a C toolchain from the very first `cargo build`** — not only at Phase 4 musl packaging, where the PRD does flag it. Without `cc`, the bootstrap task fails before any Rust is written. | Medium | The bootstrap task verifies the toolchain as an explicit precondition and CI installs it. Fail loudly at task 1, never midway. |
| **E-2** | **The vendored SQLite may predate 3.31.** The `open_marker` VIRTUAL generated column and its unique index require SQLite ≥ 3.31. | Low (current `rusqlite` vendors far newer) but **high impact** — the whole one-open-interval invariant rests on it | Assert the vendored version in the bootstrap task and keep the assertion as a test, so a future dependency downgrade fails visibly instead of silently dropping the invariant. |
| **E-3** | **Xvfb + openbox CI timing is inherently flaky.** The PRD's own CI snippet waits with fixed `sleep 1` for the WM to claim EWMH properties, which directly threatens the "`cargo test` passes in CI" acceptance bullet. | High | Replace the fixed sleeps with an explicit readiness poll on `_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW` with a bounded timeout. A flaky required job gets ignored, and an ignored job protects nothing. |
| **T-1** | **`x11rb` SYNC/`IDLETIME` alarms are low-level protocol work** — `SyncCreateAlarm`, `Trigger`, `INT64` marshaling — with sparse Rust-ecosystem precedent. This is a *binding-level* risk, separate from the PRD's own note that `IDLETIME` is poorly documented at the protocol level. | Medium-High | RF-25's degradation chain is the built-in mitigation: the daemon must start and work correctly with absence detection degraded or disabled. Prove the degraded paths in CI (Xvfb typically lacks these extensions anyway, which makes the fallbacks the *easy* thing to test and the primary path the hard one). |
| **T-2** | **Backward clock jumps meet Rust's overflow semantics.** RF-28 anticipates NTP moving the wall clock backwards; naive `end - start` panics in debug and wraps silently in release. | Medium | `checked_sub`/`saturating_sub` throughout as a reviewable convention, plus an explicit `proptest` case feeding a backwards jump. A wrapped duration would silently corrupt every aggregation, which is the failure mode M-1 exists to catch. |
| **T-3** | **`zbus` version drift is unflagged in the PRD.** R-7 calls out `rmcp` as young and unstable, but the hand-rolled `login1` blocking proxy carries the same structural exposure and no crate wraps it. | Medium | Apply R-7's own mitigation to `zbus`: isolate all usage behind an in-house trait in `logind.rs`, pin the exact version. Also makes `logind.rs` mockable, which the §17 lock/suspend test needs anyway. |
| **T-4** | **`prune`'s `VACUUM` can contend with a live daemon.** RF-13 says `prune` is never run *by* the daemon, but `contrib/xwindowlog-prune.timer` runs it while the daemon holds the database open, and `VACUUM` under WAL can return `SQLITE_BUSY`. The PRD specifies neither retry behaviour nor a prohibition. | Medium | Decide and specify: bounded retry with backoff, a clear non-zero exit with actionable stderr, or a documented stop-daemon-first contract. Small, but it is the failure the user meets after installing the timer and walking away. |
| **P-1** | **Phase 1 is very large** — the full Annex B range, ~14–16 PR slices, and the phase that must not be cut short because everything downstream depends on it. The realistic failure is not a wrong decision but a phase that never closes. | Medium-High | `auto-chain` with independently-revertible slices; P2 green early rather than at the end; the augmented success criteria below make "done" checkable instead of a judgement call. |

Not repeated in the table but carried forward: PRD §16's own product/strategic
risks, in particular **R-8** (X11's decline) and **R-9** (silent failure under
non-EWMH window managers, which RF-24 converts into a diagnostic — this is the
requirement that protects the exact audience most likely to try the project).

## Rollback Plan

Rollback splits cleanly in two, and only one half is interesting.

**Before the daemon writes a database anyone cares about** — i.e. the whole
implementation window — rollback is trivial and needs no ceremony. There is no
deployed state, no user, no published crate, no migration in the field. Reverting
a slice is `git revert`; reverting the phase is deleting the branch. Each
`auto-chain` slice must be independently revertible without breaking the slice
before it, which is a constraint on how `sdd-tasks` cuts them, not a recovery
procedure.

**After the daemon first writes a real database** — which happens the moment the
maintainer dogfoods it — exactly one thing stops being reversible: **the
schema**. RF-35 makes migrations forward-only and forbids rewriting a published
migration. So:

- The schema shipped at `user_version = 1` is the one decision in this phase
  with no clean undo, and it must be reviewed as such rather than as ordinary
  code. Correcting it later means adding migration 2, never editing migration 1.
- Every migration takes an Online-Backup-API backup first (RF-35) — not a `cp`,
  which can capture an inconsistent file without the WAL. That backup is the
  actual recovery path for a bad migration.
- The daemon refuses to open a database newer than itself and aborts with an
  explicit message rather than degrading. A downgrade is therefore a manual
  restore-from-backup, by design.

Reversible without concern, listed so nobody treats them as load-bearing:
`panic = "unwind"` (A-1), dependency pins, the MSRV floor, the systemd unit, the
CLI output formats — the last of these becomes semver-constrained only at
release (§19), not during Phase 1.

## Dependencies

- **C toolchain (`cc`)** — required by `rusqlite bundled` from the first build
  (E-1).
- **SQLite ≥ 3.31 vendored by `rusqlite bundled`** (E-2).
- **CI:** `Xvfb`, an EWMH window manager (`openbox --sm-disable`), and either
  `xdotool` or an in-house `x11rb` helper binary for synthetic windows. Preferring
  the in-house helper removes an external tool from the CI contract.
- **A D-Bus session bus or a service double** for the `logind` tests. `x11rb`'s
  pure-Rust connection avoids a `libxcb`/`libX11` build dependency — confirm the
  chosen backend at bootstrap so this stays true.
- **Resolved decisions consumed by this phase:** D-3 (`retention_days = 365`
  compiled-in). D-1 is resolved but Phase-1-irrelevant.
- **Open decisions this phase does not need:** D-2 (see A-1 assumption), D-4,
  D-5.

## Assumptions

Stated so they can be challenged rather than inherited silently.

- **A-1 — `panic = "unwind"` is retained in Phase 1 and D-2 is not decided
  here.** Phase 1 never links `rmcp`/`tokio`, so the RNF-3 size pressure that
  motivates D-2 does not bite yet, and RNF-9's argument against `abort` is
  about the long-lived MCP process, which does not exist yet. Revisit at Phase 2
  when D-2 is actually settled. **This is the exploration's recommendation
  adopted as an assumption, not a decision taken on the owner's behalf.**
- **A-2 — §15.6 governs NFR gating** wherever §17 disagrees. See *Approach*.
- **A-3 — The full RF-11 schema, including the `rules` and `projects` tables,
  is created at `user_version = 1`.** Rationale: it is the schema the PRD
  specifies, and splitting it means Phase 3 must migrate a live dogfood database
  for no behavioural gain. Consequence to accept honestly: the migration runner
  then ships without having migrated anything real, so it must be covered by
  tests using synthetic migrations. `sdd-design` may overturn this.
- **A-4 — Strict TDD (RED → GREEN → REFACTOR, runner `cargo test`) applies from
  the bootstrap task onward,** with the bootstrap task itself as the single
  documented exemption, because it creates the runner. Recorded in Engram
  `sdd/xwindowlog/testing-capabilities` and `openspec/config.yaml`.
- **A-5 — The PRD is amendable.** Risks S-1 and A-2, and the §17/§15.6 gate
  wording, are corrections to the source of truth, not local workarounds. They
  should land in `PRD.md` so the next phase does not rediscover them.

## Success Criteria

PRD §17's eleven bullets are necessary but **not sufficient**: they under-cover
the RF range Annex B assigns to this phase, so the phase could pass its literal
checklist while leaving much of its own scope unverified. The criteria below are
§17 plus the gaps closed.

**From §17, unchanged:**

- [ ] `cargo test` passes in CI, including the state machine with synthetic
      events and no X11.
- [ ] Under Xvfb + an EWMH WM, three synthetic windows produce the correct
      intervals within ≤ 1 s.
- [ ] Absence: when synthetic input stops, the active interval closes at the
      last instant of activity and `afk` opens, verified by SQL.
- [ ] Lock and suspend: a logind double emits `LockedHint`/`PrepareForSleep`;
      the interval closes as `locked` and the inhibitor is released.
- [ ] Exclusion battery passes, **including every rule of the RF-48 default
      list**; title `[hidden]`, duration preserved.
- [ ] Permissions verified by an automated test, **including `-wal` and `-shm`**.
- [ ] Crash recovery (RF-36) tested by `SIGKILL` and restart.
- [ ] RNF-1..RNF-4 measured with a concrete tool, with numeric results in the
      phase PRs — **gated per §15.6, not uniformly** (see *Approach*).
- [ ] **P2 holds for a complete simulated day, as an automated test. This is the
      phase's closing condition.**
- [ ] Single instance: the second invocation exits non-zero with a clear message.
- [ ] `today` and `status` formats frozen and documented with a literal README
      example.

**Added, because Annex B assigns these to Phase 1 and §17 does not exercise
them:**

- [ ] **RF-22 and RF-23 exercised explicitly** — a destroyed active window and a
      title that changes between the property read and the event-mask
      registration. These are the two newest, most safety-critical capture
      requirements, and "three synthetic windows produce correct intervals" does
      not touch either.
- [ ] **RF-24 degradation asserted**: under a display with no EWMH-compliant WM,
      the daemon emits the diagnostic and falls back to `GetInputFocus` — it
      never sits silent.
- [ ] **RF-25 degradation asserted at every step** of the chain, including the
      "no `SYNC`, no `MIT-SCREEN-SAVER`" case, where the daemon must still start.
- [ ] **Systemd unit (RF-20, RF-21) validated** — `PartOf=graphical-session.target`
      present and the unit stops on logout; `systemd-analyze security` recorded as
      information only, per RNF-13.
- [ ] **`pause`/`resume` (RF-49) tested end to end**, including `--minutes N`
      expiry, a `paused` interval that keeps the working day summing, and
      `status` showing the paused state visibly.
- [ ] **`prune` (RF-13) tested**: the open interval survives, sentinels survive,
      `rules`/`projects` survive, and the file actually shrinks after `VACUUM`.
- [ ] **`forget` (RF-53) tested**: physical deletion, orphan cleanup, and the
      interactive confirmation path as well as `--yes`.
- [ ] **Exit-code contract (RF-60) tested across every subcommand** — 0/1/2/3 —
      with stdout carrying only primary output and every log on stderr.
- [ ] **Completions and man pages (RF-62) generated and smoke-tested** for bash,
      zsh and fish.
- [ ] **`--json` (RF-61) carries a schema version field**, and that is asserted —
      not merely "the format is frozen".
- [ ] **`hide_app` (RF-47), `allowlist` (RF-50) and `sanitize_secrets` (RF-51)
      each have their own case table**, not just coverage by the generic
      exclusion battery.
- [ ] **P1 and P3 hold** alongside P2 (no overlap; exclusion preserves time).
- [ ] **The §14.3 ordering guarantee is enforceable**: no code path logs, panics
      with, or persists a raw title before `exclude.rs`. Worth a review checklist
      item at minimum, since it is the guarantee most easily broken by a
      well-meaning debug line.
- [ ] **RF-28 clock discipline tested** with a synthetic backwards wall-clock
      jump: `end = start`, a warning, and no negative-duration row.

## Open Questions for the Orchestrator

Returned, not decided here:

1. **S-1 (the RF-12 / RF-11 / RF-3 contradiction)** needs a resolution before the
   `interval-storage` spec can be written. The recommended resolution is above;
   it is a technical correction to the PRD, not a product choice, so `sdd-design`
   can carry it — but the PRD edit should be acknowledged rather than silent.
2. **A-2 (the `pause`/`resume` control channel)** is genuinely unspecified and
   must be designed with the reactor.
3. **D-2** remains open and is deliberately not answered here (A-1).
