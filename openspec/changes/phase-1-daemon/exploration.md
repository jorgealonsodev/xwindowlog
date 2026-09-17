# Exploration: Phase 1 — Daemon

**Change:** `phase-1-daemon` · **Date:** 2026-09-17 · **Status:** complete
**Engram mirror:** topic `sdd/phase-1-daemon/explore` (observation #6195)

## Current state

Greenfield. `git ls-files` returns exactly `.gitignore`, `PRD.md`,
`odd/tasks/prd-english-translation.md`, plus the `openspec/` skeleton. No
`Cargo.toml`, no `src/`, no crate. CodeGraph is initialized but holds no
symbols, because there is no code. This exploration is entirely PRD-derived.

## What Phase 1 must deliver

Per the Annex B traceability table, Phase 1 closes RF-1..RF-13, RF-20..RF-36,
RF-47..RF-54, RF-60..RF-63, and RNF-1..RNF-6 plus RNF-11. MCP (RF-14..RF-19)
and `doctor` (RF-64) are Phase 2/3 and out of scope.

### Build order

Respects the bootstrap-first TDD exemption recorded in
`sdd/xwindowlog/testing-capabilities`.

1. **Bootstrap** (the one task exempt from RED-first, because it creates the
   test runner): `Cargo.toml` with only the Phase 1 crates — `x11rb`
   (`screensaver`, `sync`), `zbus` (blocking), `rusqlite` (`bundled`,
   `functions`, `backup`), `clap` (+`clap_complete`, `clap_mangen`), `serde` +
   `toml`, `regex`, `time`, `signal-hook`. Deliberately **no `rmcp`/`tokio`**:
   Phase 1 never uses them. Module stubs plus the MSRV pin (RNF-11). Must land
   first, and `cargo test` must be confirmed runnable before any other task.
2. **`store.rs`** — schema DDL, sentinel rows, migrations (RF-35), write buffer
   (RF-12), startup recovery (RF-36). Fully testable with
   `Connection::open_in_memory()`.
3. **`tracker.rs`** — state machine over the `WindowSource` trait with
   synthetic events (RF-2, RF-3, RF-22, RF-23, RF-28, RF-30, RF-31 and the full
   transition table). Needs no X server, by the PRD's own design intent (§13).
4. **`exclude.rs`** — exclusion, `hide_app`, `sanitize_secrets`, default list
   (RF-47, RF-48), allowlist mode (RF-50). Pure functions, no I/O.
5. **Integration wiring** — tracker → exclude → store, plus the `proptest`
   invariants P1 (no overlap), P2 (working-day sum), P3 (exclusion preserves
   time).
6. **`x11.rs`** — real `x11rb` event loop (RF-1, RF-6, RF-22, RF-24, RF-25,
   RF-29, RF-32). First module needing Xvfb + a real EWMH WM in CI.
7. **`logind.rs`** — `zbus::blocking` for `LockedHint`, `PrepareForSleep`, the
   delay inhibitor and session resolution (RF-5, RF-26, RF-27).
8. **`main.rs`** — signals (RF-33), `flock` single instance (RF-34), the event
   loop composition, and the CLI subcommands.
9. **`contrib/xwindowlog.service`** plus automated permission tests (RF-10,
   RF-20, RF-21).
10. **Benchmark tooling** for RNF-1..RNF-4.
11. **README** — the frozen `today`/`status` output format with a literal
    example.

## Affected areas

All new: `Cargo.toml`, `src/{main,store,tracker,exclude,x11,logind}.rs`,
`contrib/xwindowlog.service`, `tests/`, `README.md`.

## Rust-specific implementation risks

Distinct from the PRD's own §16 table, which is product and strategic. These
are code-level unknowns.

1. **No off-the-shelf multiplexed event loop.** The daemon must block on the
   X11 socket fd, the D-Bus socket fd and a `signal-hook` self-pipe at once, in
   one thread, with no async runtime. Neither `x11rb`'s blocking API nor
   `zbus::blocking` compose with each other or with `signal-hook`, so this
   needs a hand-rolled `poll(2)`/`epoll` reactor over three raw fds. **This is
   the largest unaddressed architecture gap in the PRD**, and RNF-2 ("zero
   wakeups") depends entirely on it. Flagged for explicit `sdd-design`
   attention.
2. **SYNC / `IDLETIME` alarms through `x11rb` are low-level protocol work.**
   `x11rb` is protocol-accurate, not Xlib-ergonomic: `SyncCreateAlarm`,
   `Trigger` and `INT64` marshaling with sparse ecosystem precedent. This is a
   separate risk from the PRD's own note that `IDLETIME` is poorly documented
   at the protocol level.
3. **`zbus` version drift is unflagged.** The PRD calls out `rmcp` as young and
   unstable (R-7) but says nothing about `zbus`, although the `login1` blocking
   proxy is hand-rolled and carries the same structural risk.
4. **Backward clock jumps meet Rust's overflow semantics.** RF-28 anticipates
   NTP moving the wall clock backwards. Naive `end - start` arithmetic panics
   in debug builds and wraps silently in release; the duration arithmetic must
   be written with `checked_sub`/`saturating_sub` throughout.
5. **`rusqlite bundled` needs a C toolchain from the very first `cargo build`**,
   not only at Phase 4 musl packaging where the PRD does flag it. Without `cc`
   the bootstrap task fails before any Rust code is written.
6. **The schema needs SQLite >= 3.31** for the `open_marker` VIRTUAL generated
   column and the filtered unique index. Verify against the exact version
   `rusqlite`'s `bundled` feature vendors; do not assume.
7. **Xvfb + openbox CI timing is inherently flaky.** The PRD's own CI snippet
   waits with fixed `sleep 1` for the WM to claim EWMH properties, which
   threatens the "cargo test passes in CI" acceptance bullet.

## Open product decisions — relevance to Phase 1

Assessed, **not decided**. Returned to the orchestrator as gaps.

- **D-3 (default retention) genuinely blocks Phase 1.** RF-52 sits inside Phase
  1's traceability range, and the config parser plus `config.example.toml` need
  a concrete literal default. The PRD is internally inconsistent: RF-52 and the
  §11.2 example already state 365 as the default, while §20 still asks the
  question. Someone has to pick.
- **D-2 (one binary or two) is only marginally relevant.** Phase 1 never links
  `rmcp`/`tokio`, so the RNF-3 size concern does not bite yet. Recommendation:
  keep `panic = "unwind"` in Phase 1 and defer the `abort` decision to Phase 2,
  when D-2 is actually settled. Non-blocking.
- **D-4 (Wayland scope) is irrelevant to Phase 1.** Wayland is already a
  settled v1 non-goal. RF-29 (XWayland warning) is in Phase 1 scope but
  independent of D-4: it only detects and warns.
- **D-5 (release signing) is irrelevant.** Explicitly Phase 4.

## Are the §17 acceptance criteria sufficient and testable?

Mostly, with real gaps.

- **The checklist under-covers its own RF range.** Annex B assigns the systemd
  unit (RF-20, RF-21), `pause`/`resume` (RF-49), `prune` (RF-13), the exit-code
  contract (RF-60), completions and man pages (RF-62), and the
  `hide_app`/allowlist/secret-sanitization detail (RF-47, RF-50, RF-51) to
  Phase 1, but the eleven §17 bullets do not explicitly exercise most of them.
  The phase can pass its literal checklist while leaving much of its assigned
  range unverified. This is the most material gap.
- **RF-22 and RF-23 are not named in the Xvfb bullet.** "Three synthetic
  windows produce the correct intervals" does not obviously exercise window
  destruction or the property-read/event-mask race that these two
  safety-critical requirements exist to cover.
- **The RNF-1..RNF-4 bullet contradicts §15.6.** §17 reads as though all four
  are measured against thresholds together, while §15.6 makes RNF-3/RNF-4 hard
  CI gates, RNF-1 a margin-based gate, and RNF-2 explicitly *not* a CI gate.
- **The "output format frozen" bullet omits RF-61's versioned `--json` schema.**
  It should test for a schema version field, not only format stability.
- Everything else is concretely testable and well specified. P2, the
  working-day invariant, is the strongest criterion in the phase and is its
  real closing condition.

## Size against the 400-line review budget

Estimates, not measurements — no code exists. Roughly **14–16 PR-sized
slices**. `x11.rs` is the most likely to blow the budget on its own given how
verbose `x11rb` protocol code is, and probably warrants 3 slices (atoms and
property reads; event loop, backoff and degradation chain; SYNC/IDLETIME).
`store.rs` and `tracker.rs` warrant 2 each (core versus edge cases). Consistent
with the `auto-chain` delivery strategy resolved for this session.

## Recommendation

Proceed to `sdd-propose`. Resolve D-3 with the user before `sdd-tasks` locks
the task list. Flag the three-fd reactor for explicit `sdd-design` attention
rather than leaving it an implicit assumption.
