//! `WallTs`, `MonoInstant`, `Clock`, `SystemClock`, `FakeClock`, `close_at`, `duration_secs`, `backdated_close` (D-9).
//!
//! RF-28: wall clock and monotonic clock are never derived from one another.
//! `WallTs` is the only value ever persisted; `MonoInstant` measures durations
//! and deadlines only. There is deliberately no `From` in either direction
//! (proven by the `tests/trybuild/` compile-fail fixture).
//!
//! Design §8's explicit ordering constraint lands this module before its
//! consumers (`store.rs`, Phase 3/4; `tracker.rs`, Phase 6). By the end of
//! Phase 6 both consumers exist for real (`store.rs`'s `WallTs` usage;
//! `tracker.rs`'s `close_at`/`backdated_close`/`Clock`/`FakeClock` usage), so
//! the module-level `dead_code` allow this file carried through Phases 2-5
//! is no longer justified and was removed here (task 6.12).

use std::cell::Cell;
use std::time::{Duration, Instant};

/// Wall clock, UTC epoch seconds. The ONLY value ever persisted (RF-28).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WallTs(pub(crate) i64);

impl WallTs {
    /// Constructs a `WallTs` from a raw Unix-epoch second count.
    ///
    /// Task 8.0b: `tests/invariants.rs` (a separate crate from this one) is
    /// where P1/P2 now live, and both build synthetic `WallTs` values
    /// directly (`FakeClock::new(WallTs(...))`, adversarial backwards
    /// jumps). The tuple field stays `pub(crate)` — arithmetic on wall time
    /// still only ever happens inside this module (RF-28) — so an explicit,
    /// narrow constructor/accessor pair is what crosses the crate boundary,
    /// not a wider field.
    pub fn new(unix_secs: i64) -> Self {
        WallTs(unix_secs)
    }

    /// Read access to the raw Unix-epoch second count, for the same reason
    /// as `new` above.
    pub fn as_unix_secs(self) -> i64 {
        self.0
    }
}

/// Monotonic instant. NEVER persisted, NEVER derived from `WallTs`, and vice
/// versa (RF-28). There is deliberately no `From` in either direction.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MonoInstant(pub(crate) Instant);

/// Result of clamping a computed `end` against an interval's `start` (RF-28).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Close {
    Ok(WallTs),
    ClampedBackwards { requested: WallTs, to: WallTs },
}

/// RF-28. Note: this COMPARES, it does not subtract. There is no arithmetic
/// here and therefore no overflow, in debug or release, for any input
/// including `i64::MIN`.
pub fn close_at(start: WallTs, requested_end: WallTs) -> Close {
    if requested_end.0 < start.0 {
        Close::ClampedBackwards {
            requested: requested_end,
            to: start,
        }
    } else {
        Close::Ok(requested_end)
    }
}

/// Only ever called on an already-clamped pair (`close_at`'s output).
/// `checked_sub` guards the `i64::MIN`/`i64::MAX` case; the `filter` guards a
/// caller that bypassed `close_at`. Never negative, never a panic (T-2).
#[allow(
    dead_code,
    reason = "task 6.12: no non-test consumer exists yet, unlike close_at/backdated_close \
              which tracker.rs's production code now calls directly. duration_secs is \
              currently exercised only by store.rs's and tracker.rs's own #[cfg(test)] \
              modules; a real caller (e.g. cli-reporting's today/status) lands later. \
              Narrowed to this item per tasks.md 6.12's own fallback clause rather than \
              reinstating the module-level allow."
)]
pub fn duration_secs(start: WallTs, end: WallTs) -> u64 {
    end.0.checked_sub(start.0).filter(|d| *d >= 0).unwrap_or(0) as u64
}

/// RF-4's backdated AFK close, which has its own trap: `idle` is an
/// externally-reported duration (the X11 `IDLETIME` alarm), so it goes
/// through `checked_sub`/`try_from` rather than a bare cast, and backdating
/// past `start` clamps to `start` via `close_at` instead of going negative.
pub fn backdated_close(start: WallTs, now: WallTs, idle: Duration) -> Close {
    let idle_secs = i64::try_from(idle.as_secs()).unwrap_or(i64::MAX);
    let target = now.0.checked_sub(idle_secs).map(WallTs).unwrap_or(now);
    close_at(start, target)
}

/// Wall time for persisted timestamps, monotonic time for durations and
/// deadlines (design §2 D-8). The two are never substituted for one another.
#[allow(
    dead_code,
    reason = "task 6.12: no non-test caller invokes now_wall/now_mono through this trait \
              yet — main.rs (Phase 15) is what wires SystemClock through it for real; \
              tracker.rs takes WallTs/MonoInstant values directly rather than a &dyn Clock, \
              by design (module Purpose: no clock consulted except via given values)."
)]
pub trait Clock {
    fn now_wall(&self) -> WallTs;
    fn now_mono(&self) -> MonoInstant;
}

/// The real clock, backed by the OS.
#[allow(
    dead_code,
    reason = "task 6.12: SystemClock is constructed for real at Phase 15's daemon \
              composition (main.rs); no non-test consumer exists yet in Phase 6."
)]
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_wall(&self) -> WallTs {
        WallTs(time::OffsetDateTime::now_utc().unix_timestamp())
    }

    fn now_mono(&self) -> MonoInstant {
        MonoInstant(Instant::now())
    }
}

/// An injected, advanceable clock (design §2 D-8) — the mechanism that turns
/// a full simulated day (P2) into a millisecond-scale test instead of a
/// twenty-four-hour one.
#[allow(
    dead_code,
    reason = "task 6.12: FakeClock exists purely as the design §2 D-8 test-injection \
              mechanism, consumed only by store.rs's and tracker.rs's own #[cfg(test)] \
              modules — it has no place in the shipped daemon by design, unlike \
              SystemClock which Phase 15 wires in for real."
)]
#[derive(Debug)]
pub struct FakeClock {
    wall: Cell<WallTs>,
    mono: Cell<Instant>,
}

#[allow(
    dead_code,
    reason = "task 6.12: see the FakeClock struct's own allow immediately above — same \
              test-only rationale applies to its constructor and mutators."
)]
impl FakeClock {
    pub fn new(start: WallTs) -> Self {
        FakeClock {
            wall: Cell::new(start),
            mono: Cell::new(Instant::now()),
        }
    }

    /// Advances both clocks together, simulating ordinary elapsed time (as
    /// opposed to a wall-clock-only jump — see `set_wall`).
    pub fn advance(&self, by: Duration) {
        let bumped_secs = i64::try_from(by.as_secs()).unwrap_or(i64::MAX);
        let wall = self.wall.get();
        let advanced_wall = wall.0.checked_add(bumped_secs).map(WallTs).unwrap_or(wall);
        self.wall.set(advanced_wall);

        if let Some(advanced_mono) = self.mono.get().checked_add(by) {
            self.mono.set(advanced_mono);
        }
    }

    /// Sets the wall clock independently of the monotonic clock, simulating a
    /// wall-clock-only jump (e.g. an NTP correction) that leaves the
    /// monotonic clock untouched — exactly RF-28's "never derived from the
    /// other" scenario, made reproducible for tests.
    pub fn set_wall(&self, wall: WallTs) {
        self.wall.set(wall);
    }
}

impl Clock for FakeClock {
    fn now_wall(&self) -> WallTs {
        self.wall.get()
    }

    fn now_mono(&self) -> MonoInstant {
        MonoInstant(self.mono.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_at_forward_end_returns_ok() {
        let start = WallTs(100);
        let end = WallTs(150);

        assert_eq!(close_at(start, end), Close::Ok(end));
    }

    #[test]
    fn close_at_backwards_end_clamps_to_start() {
        let start = WallTs(100);
        let requested_end = WallTs(50);

        assert_eq!(
            close_at(start, requested_end),
            Close::ClampedBackwards {
                requested: requested_end,
                to: start
            }
        );
    }

    #[test]
    fn duration_secs_forward_gap_is_the_difference() {
        assert_eq!(duration_secs(WallTs(100), WallTs(150)), 50);
    }

    #[test]
    fn duration_secs_equal_timestamps_is_zero() {
        assert_eq!(duration_secs(WallTs(100), WallTs(100)), 0);
    }

    #[test]
    fn duration_secs_end_before_start_floors_at_zero_without_panicking() {
        // Guards a caller that bypassed `close_at` and handed a still-negative pair.
        assert_eq!(duration_secs(WallTs(100), WallTs(50)), 0);
    }

    #[test]
    fn duration_secs_i64_min_start_never_panics() {
        assert_eq!(duration_secs(WallTs(i64::MIN), WallTs(i64::MAX)), 0);
    }

    #[test]
    fn duration_secs_i64_max_end_never_panics() {
        assert_eq!(duration_secs(WallTs(i64::MAX), WallTs(i64::MIN)), 0);
    }

    #[test]
    fn backdated_close_within_interval_backdates_exactly() {
        let start = WallTs(100);
        let now = WallTs(200);
        let idle = Duration::from_secs(30);

        // now (200) - idle (30) = 170, which is still after start (100).
        assert_eq!(backdated_close(start, now, idle), Close::Ok(WallTs(170)));
    }

    #[test]
    fn backdated_close_past_start_clamps_to_start() {
        let start = WallTs(100);
        let now = WallTs(200);
        let idle = Duration::from_secs(150);

        // now (200) - idle (150) = 50, which is before start (100): clamp.
        assert_eq!(
            backdated_close(start, now, idle),
            Close::ClampedBackwards {
                requested: WallTs(50),
                to: start
            }
        );
    }

    #[test]
    fn fake_clock_starts_at_the_given_wall_time() {
        let clock = FakeClock::new(WallTs(1_000));

        assert_eq!(clock.now_wall(), WallTs(1_000));
    }

    #[test]
    fn fake_clock_advance_moves_wall_and_mono_together() {
        let clock = FakeClock::new(WallTs(1_000));
        let mono_before = clock.now_mono();

        clock.advance(Duration::from_secs(5));

        assert_eq!(clock.now_wall(), WallTs(1_005));
        assert!(clock.now_mono().0 >= mono_before.0 + Duration::from_secs(5));
    }

    #[test]
    fn fake_clock_set_wall_jumps_wall_only_leaving_mono_untouched() {
        let clock = FakeClock::new(WallTs(1_000));
        let mono_before = clock.now_mono();

        // Simulates a backwards NTP jump: only the wall clock moves.
        clock.set_wall(WallTs(500));

        assert_eq!(clock.now_wall(), WallTs(500));
        assert_eq!(clock.now_mono(), mono_before);
    }

    #[test]
    fn system_clock_now_wall_is_a_plausible_unix_epoch_second() {
        let clock = SystemClock;

        // A sanity bound, not an exact assertion: the daemon's earliest
        // realistic use post-dates 2020-01-01T00:00:00Z (1_577_836_800).
        assert!(clock.now_wall().0 > 1_577_836_800);
    }

    #[test]
    fn system_clock_now_mono_advances_with_real_elapsed_time() {
        let clock = SystemClock;
        let before = clock.now_mono();
        std::thread::sleep(Duration::from_millis(5));
        let after = clock.now_mono();

        assert!(after.0 > before.0);
    }
}
