# RF-32 — wire X11 reconnection into production

## Objective

Give the existing, fully tested X11 reconnection policy a production caller, so a
mid-run X11 connection loss makes the daemon reconnect on the RF-32 backoff
schedule instead of exiting.

## Problem

`Reconnector` and `ReconnectBackoff` (`src/x11.rs:1574-1704`) were built and tested
in Phases 9-11 against real Xvfb. Phase 15's composition never gave them a caller.
Verified 2026-09-22 against `e2d63ae`:

- `grep -n 'Reconnector' src/main.rs src/adapters.rs` — no matches.
- `Timer::ReconnectBackoff` is armed only at `src/reactor.rs:1176`, inside
  `#[cfg(test)]` (the test module starts at `src/reactor.rs:656`). The single
  production caller of `arm_timer` is `src/main.rs:378`, and it passes
  `Timer::PauseExpiry`.
- `src/main.rs:338` maps the source error to `StartupError::Source`;
  `src/main.rs:164` maps that to `ExitStatus::Environment`.

So on a real connection loss the daemon prints a message and exits. RF-32 is
unimplemented in practice despite having a fully tested policy.

## Why it matters

RF-32 is a shipped acceptance criterion with 11 traced tasks
(`openspec/changes/phase-1-daemon/tasks.md:1248`), all checked `[x]`. The policy is
correct; only the wiring is missing. This is the same defect shape this codebase
keeps producing: a claim without a mechanism.

## Constraints and findings that shape the design

1. **`SourceError(pub String)` (`src/tracker.rs:183`) erases everything.**
   `src/adapters.rs:81-94` stringifies the underlying `x11rb` error, so the reactor
   and `main.rs` cannot tell a socket-level disconnect from a protocol fault, nor
   which source produced it. It is the shared error currency for X11, logind and
   `poll(2)` failures alike (21 references across 5 files).
2. **Startup and mid-run failures already travel different code paths.** Startup
   goes through `X11Source::connect_with_afk_threshold` returning `X11InitError`
   (`src/x11.rs:204-208`), called directly from `src/main.rs:263-265` before
   `X11Adapter` exists. Every error `X11Adapter::try_next`/`flush` can produce is
   therefore mid-run **by construction**. The distinction does not need to be
   invented — it needs to stop being discarded.
3. **The fd needs no change notification.** `ReactorSource::next_event` re-queries
   `self.x11.as_raw_fd()` on every poll iteration (`src/reactor.rs:583-590`), so a
   replaced connection is picked up on the next iteration.
4. **`X11Adapter.source` is private with no setter** (`src/adapters.rs:60-73`).
5. **`Reconnector::attempt` blocks**: it calls `x11rb::connect` inline
   (`src/x11.rs:1687-1704`). `ReactorSource::next_event` must stay non-blocking.
6. `SourceEvent::DisplayLost` / `DisplayRestored` already exist, and
   `ReconnectAttempt`'s doc contract (`src/x11.rs:1649-1671`) instructs its caller to
   emit exactly those. The type was designed for a reactor-side caller, and
   `src/x11.rs:48` says so outright: driving the backoff on a timer and turning its
   outcomes into `SourceEvent`s "is `reactor.rs`'s job (Phase 14)". Phase 14 never
   did it.

## Decision

**Route: reactor-internal recovery, `SourceError` untouched.**

A mid-run X11 error is intercepted inside the reactor and never escapes
`next_event` as an `Err`. The reactor emits `DisplayLost`, arms
`Timer::ReconnectBackoff` with the policy's own `retry_after`, and on
`DeadlineElapsed(ReconnectBackoff)` drives the next attempt, emitting
`DisplayRestored` on success.

Rejected: restructuring `SourceError` to carry a reconnectable variant and deciding
in `main.rs`. It would touch 21 call sites across 5 files and push an X11-specific
concept into the error type shared by logind and `poll(2)` — wrong altitude for the
problem.

To keep `ReactorSource<X, L, C>` generic, the recovery capability goes on the
`BudgetedSource` trait with a no-op default, so `LogindAdapter` is untouched.
`X11Adapter` owns its `Reconnector` and implements it; the reactor keeps ownership
of `Deadlines` and arms the timer from the `Duration` the adapter returns.

**The blocking attempt must be bounded.** Phase 12 already learned that an
unbounded connect to a peer that accepts and then stalls hangs the daemon, and the
reactor is single-threaded: a stalled reconnect would freeze logind, signals and
every control client. `connect_bounded` in `src/logind.rs` is the precedent to
follow.

## Authorized scope

`src/x11.rs`, `src/adapters.rs`, `src/reactor.rs`, `src/main.rs`, and their tests.
No change to `SourceError`'s shape. No rewrite of `Reconnector`/`ReconnectBackoff`
policy — it is tested and correct; it only gains a caller.

## Mode

- TDD: **strict, enabled** (source: global CLAUDE.md). Runner: `cargo test`.
- Checks per task: `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- Baseline at start: **269 passed, 0 failed** on `e2d63ae` (verified 2026-09-22).
- Delivery: work-unit commits under ~400 authored lines, straight to `main`, no PRs.

## Tasks

- [x] **T1** — Bound the reconnect attempt. Route: delegated writer (writer trigger:
      `src/x11.rs` plus its tests). RED was a genuine hang, `EXIT=124` under a 15 s
      shell timeout with no output — `Reconnector::attempt` parked forever in
      `x11rb::connect`'s setup handshake against a peer that accepted and never wrote.
      GREEN in 5.00 s, the bound firing. `connect_bounded` + `RECONNECT_CONNECT_TIMEOUT
      = 5 s` at `src/x11.rs:1650-1681`, matching `logind::connect_bounded`'s
      `BUS_CONNECT_TIMEOUT` (`src/logind.rs:241`) exactly; `Reconnector::attempt` now
      routes through it (`src/x11.rs:1737-1747`). New `X11InitError::ReconnectSpawn`/
      `ReconnectTimeout` variants (`src/x11.rs:207`). Test
      `reconnector_attempt_times_out_against_a_peer_that_accepts_and_then_stalls`.
      Checks: `cargo test --all-targets` 270 passed 0 failed (parent re-ran it
      independently: 270 passed); `cargo clippy --all-targets -- -D warnings` clean;
      `cargo fmt --check` clean. +108/-2 in one file.
- [x] **T2** — Give `X11Adapter` a way to replace its live `X11Source` and to own a
      `Reconnector`. Route: delegated writer in the isolated worktree `rf-32-t2`,
      because the main checkout held a frozen review candidate. RED: two tests panicking
      on a `todo!()` stub, `14 passed; 2 failed`. GREEN: `16 passed`. `replace_source`
      and `reconnector_mut` at `src/adapters.rs:143-160`, `with_reconnect_config` at
      `:103-115`, `reconnector` field at `:72-88`. The fd test drives two genuinely
      distinct Xvfb connections rather than asserting on a field that was just set.
      Checks: `cargo test --all-targets` 273 passed 0 failed (parent re-ran it
      independently: 273 passed); clippy `-D warnings` clean; `cargo fmt --check` clean.
      +213/-1 in one file.

- [x] **T2a** — Make the adapter's `Reconnector` use the CONFIGURED afk threshold.
      Route: delegated writer, worktree `rf-32-t2`. `build_x11_adapter` at
      `src/main.rs:252-258`, production call site at `:282` now passing
      `config.afk_threshold`; read-only `Reconnector::afk_threshold()` accessor at
      `src/x11.rs:1737`. `X11Adapter::new` and `DEFAULT_RECONNECT_AFK_THRESHOLD` were
      DELETED, not `#[cfg(test)]`-gated: once `main.rs` stopped calling them, clippy
      `-D warnings` failed with two genuine `dead_code` errors. The two tests that used
      `new` now call `with_reconnect_config` directly. The stale doc comment declaring
      the gap is gone, replaced by one describing what the code actually does.
      Checks: `cargo test --all-targets` 280 passed 0 failed; clippy `-D warnings`
      clean; fmt clean — all three re-run by the parent after its mutation check below.
      +67/-26 across three files.

      **Mutation-verified by the parent.** The writer's RED was a compile error
      (`E0425: cannot find function build_x11_adapter`), not an assertion failing
      against the defaulting behavior, so the test had never been shown to reject the
      real defect. The parent hardcoded the threshold back to `Duration::from_secs(240)`
      and re-ran the test: it failed with `left: 240s, right: 37s`. The guard is real.
      Source then restored and all three checks re-run clean.

- [x] **U1 — Complete the reconnection loop** (absorbed the old T4 and T5).
      Route: delegated writer, worktree `rf-32-t4`, returned once by the parent with
      mutation evidence. Gate is `x11_down: bool` on `ReactorSource` (`src/reactor.rs:404`),
      set on `OutageOpened`/`StillDown` and cleared on `Restored` in `recover_x11`
      (`:508`), read at the top of the X11 step in `next_event` (`:686`) so `service_x11`
      — and therefore `attempt()` — is never called while down. The backoff deadline is
      consumed inside the `take_due` loop (`:790`) and drives the next attempt directly,
      instead of escaping as a raw `DeadlineElapsed(Timer::ReconnectBackoff)`; nothing
      outside this module owns X11 reconnection state, and `tracker.rs` says so itself.
      Checks: `cargo test --all-targets` 286 passed 0 failed, clippy `-D warnings` clean,
      fmt clean — all three re-run by the parent. +514/-14 in `src/reactor.rs`.

      **The first submission passed its own mandatory assertion while the gate was
      disabled.** The parent caught it by mutation: replacing `if self.x11_down` with
      `if false` left `no_reconnect_attempt_happens_before_the_backoff_deadline_fires`
      passing. Cause: `RecoverableSource::fail_next_try_next` is one-shot and `try_next`
      clears it on the first failure, so the source HEALED ITSELF on the second call —
      `service_x11` returned `Ok`, `recover` was never reached, and the count stayed at 1
      with or without a gate. The test measured a recovered source, not a dead one.

      Fixed with `fail_try_next_persistently(times)` (`remaining_failures`, bounded at 5
      rather than unconditional so a missing gate burns the budget and fails fast instead
      of hanging the suite forever — the ungated `Err(_) => { recover_x11(); continue; }`
      has no other exit). Re-verified by the parent's own mutation: the assertion now
      fails `left: 5, right: 1`. That `5` is direct evidence of the hot loop — the whole
      failure budget is consumed inside a SINGLE `next_event` call.

      **Rule this unit establishes**: a test that arms or gates on state must observe the
      SECOND iteration against a fault that PERSISTS. A one-shot fault double heals
      itself and makes the assertion vacuous. Verify every such test by mutation before
      believing it.

- [x] **U2 — Prove it end to end** (absorbed the old T6 and T7). Route: delegated
      writer, worktree `rf-32-u2`. Scope `tests/daemon_e2e.rs` only, +182/-2. Checks:
      `cargo test --all-targets` 288 passed 0 failed, clippy `-D warnings` clean, fmt
      clean — all three re-run by the parent.

      **Half 1** — `daemon_exits_with_environment_status_when_the_first_x11_connect_fails`
      (`tests/daemon_e2e.rs:869`): spawns the real binary with `DISPLAY` on a port proved
      dead by a live connect probe, asserts exit `3` (`ExitStatus::Environment`) and
      non-empty stderr. Honestly reported as a **characterization test**, not a driven
      RED: the property already held, because the startup connect runs before any
      `X11Adapter` exists and reactor-internal recovery structurally cannot reach it.
      Mutation-verified anyway — flipping `StartupError::exit_status`'s `X11(_)` arm to
      `ExitStatus::Ok` makes it fail.

      **Half 2** — `daemon_reconnects_to_a_restarted_xvfb_and_keeps_capturing`
      (`tests/daemon_e2e.rs:912`): real daemon, real Xvfb, captures an interval, SIGKILLs
      the server, asserts the DAEMON PROCESS SURVIVES, restarts Xvfb on the same display,
      and proves a second activation is still captured — through the wired path
      (`next_event` -> `recover_x11` -> `X11Adapter::recover` -> `Reconnector`), unlike
      `x11_integration.rs`'s tests which drive `Reconnector` directly and never touch the
      reactor. Mutation-verified independently by the parent: making `recover` an
      unconditional `StillDown` that never reconnects fails the test at its bounded 30 s
      deadline (31.05 s wall), and `src/adapters.rs` restored clean afterwards.

      **A real race was found and fixed, not papered over.** In 1 of 19 repeated runs the
      daemon's reconnect beat the replacement `FakeWm`'s `declare_ewmh_supported()`. RF-24
      decides EWMH compliance once, at connect time, so losing that race locks the
      reconnected `X11Source` into `CaptureMode::InputFocusFallback`, which reads real
      input focus rather than `_NET_ACTIVE_WINDOW` — and `FakeWm::set_active_window` only
      wrote the EWMH property, so the stimulus was invisible in that mode. Fixed by having
      `set_active_window` also call `set_input_focus` (mirroring `x11_integration.rs`'s own
      `FakeWm::focus` precedent), making the stimulus legible under either capture mode.
      25/25 clean runs after the fix. A longer sleep would have hidden this instead.

## Progress

Exploration complete 2026-09-22 (read-only mapping agent). **All 6 units done.**
Baseline moved 269 -> 270 -> 273 -> 279 -> 280 -> 286 -> 288 tests.

Reviews: `review-7d07179fee1ffc81` (T1, high, four lenses) and
`review-cc5cf7104f4f472c` (T2+T3, medium, one lens) and `review-c551e5e1c5735b23`
(T2a, high, four lenses) all approved with zero findings and acknowledged, authority
burned. The U1 slice candidate (`review-d746919b58808c2b`) was **declined by the
user** — candidate-scoped, no review record created, delivery under ordinary policy.
U2 was not assessed separately.

**The defect that mattered most was found by neither reviews nor TDD.** The ungated
retry loop was caught by reading the order of `next_event`, and the vacuous gate
assertion by mutation. See the rule under U1.

Reviews so far: `review-7d07179fee1ffc81` (T1, high, four lenses) and
`review-cc5cf7104f4f472c` (T2+T3 slice, medium, one lens). Both approved with zero
findings and acknowledged, authority burned. The reviewed boundary is `aaf95c6`.

T1 delivered on `main` as `1991d0d`. Its native review, lineage
`review-7d07179fee1ffc81`, assessed `high` (`process_boundary`, `src/x11.rs`), ran four
lenses, found nothing, and was acknowledged with authority burned (consumed revision
`sha256:c0d60cff...deb1d4de`). The reviewed boundary is now `1991d0d`.

T2 lives on branch `rf-32-t2` in a sibling worktree, created so its writes could not
contaminate T1's frozen review candidate. Merge it back to `main` once its own review
closes.

## Disclosed debt from T1

1. **A timed-out attempt leaks one thread.** `connect_bounded` cannot interrupt a
   blocked `x11rb::connect`, so on timeout the throwaway thread stays parked on the
   read. The doc comment at `src/x11.rs:1658-1668` states this openly. Bounded in
   practice by the RF-32 backoff spacing attempts out, but an hours-long outage
   against a pathological peer would accumulate threads. Same tradeoff
   `logind::connect_bounded` already accepted.
2. **Test-only underflow assumption.** The new test computes `port - 6000` from an
   ephemeral port, relying on `net.ipv4.ip_local_port_range` starting above 6000
   (the Linux default is 32768). A host tuned below 6000 would panic on underflow in
   a debug build. Disclosed in a comment at the call site; not worth a guard unless
   it ever fires.

## Next step

**RF-32 is complete.** All six units are done and the daemon reconnects instead of
exiting. Remaining work is integration, not implementation: branches `rf-32-t2`,
`rf-32-t4` and `rf-32-u2` are stacked and unmerged, and `main` has unpushed commits.
Delivery is the user's decision under ordinary repository policy.

Carried debt, all disclosed, none blocking:
1. A timed-out reconnect attempt leaks one parked thread (`src/x11.rs:1658-1668`
   states this openly; same tradeoff `logind::connect_bounded` accepted).
2. Test-only `port - 6000` underflow assumption in T1's stalling-peer test.
3. The reactor's 20 ms `thread::sleep` accept back-off, inherited from Phase 14.

## Why the remaining work was regrouped

The first four tasks were sized by edit surface, which produced a review cycle per
small task and, worse, split one mechanism across two of them. The remainder is sized
by MECHANISM instead: U1 is "a deadline is armed and honored", U2 is "startup fatal,
mid-run recoverable". Each is one coherent behavior with its own tests and one review,
which is both cheaper and harder to ship broken.

## Post-review correction — 2026-09-23

The native review of `e2d63ae..bfef399` returned `correction_required` with two introduced
CRITICAL defects:

1. Once `x11_down` was set, the dead X11 fd stayed in `poll(2)`. A killed X server therefore
   produced permanent HUP/POLLIN readiness and a 100% CPU drain/poll spin instead of waiting for
   `ReconnectBackoff`.
2. `recover_x11` called the production `Reconnector::attempt` on the reactor thread. Its bounded
   connect could still occupy that thread for `RECONNECT_CONNECT_TIMEOUT` (5 seconds), starving
   logind, signals, and control clients.

The bounded correction keeps the generic `BudgetedSource` boundary and moves only Send-compatible
recovery policy/result state to a worker. `X11Adapter` and its `Rc<RefCell<Excluder>>` remain on the
reactor thread; the worker publishes its result before signaling a pollable completion fd. The
reactor omits the dead X11 fd while down, services the completion fd and other sources, applies a
completed result only after the `PollFd` block drops, replaces the source on the reactor thread,
and rejects overlapping recovery. Worker/notification setup failures surface as `SourceError`.

Strict-TDD evidence: the new dead-peer test first observed 8,017 poll wakeups in 50ms; the silent
peer test first took 5.32s and returned `DisplayLost` instead of servicing its control request.
Both are green after the correction. Mutation checks also restored the dead-fd branch and inline
recovery path and reproduced each failure. Focused RF-32 tests, the restarted-Xvfb daemon E2E,
`cargo test --all-targets` (290 passed), clippy with `-D warnings`, and formatting all pass.

**Current progress:** the post-review correction is implemented and verified; the historical
“RF-32 is complete” entry above describes only the pre-review boundary. No RF-32 implementation
work remains in this correction.

**Current next step:** deliver/integrate this single correction work unit under ordinary repository
policy; do not start another RF-32 correction unless a new review finding provides new evidence.
