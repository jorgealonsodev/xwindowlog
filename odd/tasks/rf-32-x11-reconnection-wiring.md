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

- [ ] **T2a** — Make the adapter's `Reconnector` use the CONFIGURED afk threshold.
      T2 could not: `Reconnector::new` needs `display`/`afk_threshold`, and supplying the
      real ones meant changing `X11Adapter::new`'s signature, whose only call site is
      `src/main.rs:267` — a file T2 was forbidden to touch. `new` therefore defaults to
      `(None, DEFAULT_RECONNECT_AFK_THRESHOLD)`. `display` matches production, which always
      passes `None`; **`afk_threshold` does not** — `main.rs` passes
      `config.afk_threshold`, which can differ from the 240 s default. Harmless today
      because nothing calls `reconnector_mut()` in production, and it must be fixed
      BEFORE T5 gives it a production caller, or a reconnected source would silently run
      on a different idle threshold than the configured one. Fix: switch `src/main.rs:267`
      to `with_reconnect_config`. Disclosed by the T2 writer at `src/adapters.rs:60-68`
      rather than decided silently.
- [ ] **T3** — Add the recovery capability to `BudgetedSource` with a no-op default,
      implemented by `X11Adapter`. Assert `LogindAdapter` is unaffected.
- [ ] **T4** — Intercept the mid-run X11 error inside `ReactorSource::next_event`:
      emit `DisplayLost`, arm `Timer::ReconnectBackoff`, stop it escaping as `Err`.
- [ ] **T5** — Drive `DeadlineElapsed(ReconnectBackoff)` through the policy; emit
      `DisplayRestored` and reset backoff on success, re-arm on `StillDown`.
- [ ] **T6** — Keep a genuine startup failure fatal. Prove `main.rs` still exits with
      `ExitStatus::Environment` when the first connect fails.
- [ ] **T7** — End-to-end against real Xvfb: kill the server mid-run, restart it,
      prove the daemon reconnects and keeps capturing.

## Progress

Exploration complete 2026-09-22 (read-only mapping agent). **T1 and T2 done**, 2 of 8
(T2a added from T2's disclosure). Baseline moved 269 -> 270 -> 273 tests.

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

T3 — add the recovery capability to `BudgetedSource` with a no-op default, implemented
by `X11Adapter`. T2a must land before T5.
