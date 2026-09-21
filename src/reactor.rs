//! `poll` loop, `PollFd` set, `Deadlines`, budgets, `EINTR` handling, `impl WindowSource for ReactorSource`.
//!
//! Design §2 D-2/D-6: the reactor's own "highest-risk logic" (§4 File Changes rationale for
//! splitting this module out of `main.rs`). This module owns no domain logic — it translates
//! fd readiness and deadline expiry into `tracker::SourceEvent`s; state-transition decisions
//! stay in `tracker.rs` (task 14.12).

use nix::poll::PollTimeout;

use crate::clock::MonoInstant;
use crate::tracker::Timer;

/// The reactor's own deadline set (design §2 D-2): one optional monotonic `Instant` per
/// `Timer` kind, `arm`ed either by this module itself (`ReconnectBackoff`, `PauseExpiry`,
/// `SessionReresolve`) or by the caller applying `tracker::Effect::ArmTimer`/`CancelTimer`
/// for the two tracker-owned timers (`TitleDebounce`, `DestroyGrace`) — the timeout argument
/// to `poll(2)` *is* the timer subsystem; there is no `timerfd`, no sleeping thread, no tick.
#[derive(Debug, Default)]
pub struct Deadlines {
    destroy_grace: Option<MonoInstant>,
    title_debounce: Option<MonoInstant>,
    reconnect_backoff: Option<MonoInstant>,
    pause_expiry: Option<MonoInstant>,
    session_reresolve: Option<MonoInstant>,
}

impl Deadlines {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&mut self, timer: Timer) -> &mut Option<MonoInstant> {
        match timer {
            Timer::DestroyGrace => &mut self.destroy_grace,
            Timer::TitleDebounce => &mut self.title_debounce,
            Timer::ReconnectBackoff => &mut self.reconnect_backoff,
            Timer::PauseExpiry => &mut self.pause_expiry,
            Timer::SessionReresolve => &mut self.session_reresolve,
        }
    }

    /// Arms `timer` for `at`, replacing any previous deadline of the same kind (at most one
    /// per kind, design §2 D-2).
    pub fn arm(&mut self, timer: Timer, at: MonoInstant) {
        *self.slot(timer) = Some(at);
    }

    /// Disarms `timer`, if it was armed.
    pub fn cancel(&mut self, timer: Timer) {
        *self.slot(timer) = None;
    }

    /// The earliest armed deadline, and which `Timer` it belongs to.
    fn earliest(&self) -> Option<(Timer, MonoInstant)> {
        [
            (Timer::DestroyGrace, self.destroy_grace),
            (Timer::TitleDebounce, self.title_debounce),
            (Timer::ReconnectBackoff, self.reconnect_backoff),
            (Timer::PauseExpiry, self.pause_expiry),
            (Timer::SessionReresolve, self.session_reresolve),
        ]
        .into_iter()
        .filter_map(|(timer, at)| at.map(|at| (timer, at)))
        .min_by_key(|(_, at)| at.0)
    }

    /// Design §2 D-2's `poll_timeout`. `None` means nothing is pending anywhere: `poll()`
    /// blocks forever and the kernel does not schedule this thread at all — RNF-2's "zero
    /// wakeups" is a property of this branch being reachable, not of a fast loop.
    ///
    /// VERIFIED (design §12 V-3): `nix`'s `impl TryFrom<Duration> for PollTimeout` goes
    /// through `Duration::as_millis()`, which TRUNCATES. Truncating a 250.4ms deadline to
    /// 250ms wakes the reactor before it is due, finds nothing ready, and immediately
    /// re-polls: a spin loop that would defeat RNF-2 while looking correct. Round UP,
    /// explicitly, instead of relying on the truncating conversion.
    pub fn poll_timeout(&self, now: MonoInstant) -> PollTimeout {
        match self.earliest() {
            None => PollTimeout::NONE,
            Some((_, at)) => {
                let left = at.0.saturating_duration_since(now.0);
                if left.is_zero() {
                    return PollTimeout::ZERO;
                }
                let ms = left.as_millis() + u128::from(left.subsec_nanos() % 1_000_000 != 0);
                PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)
            }
        }
    }

    /// Every timer whose deadline is `<= now`, cleared as they are returned (design §2 D-6:
    /// deadlines are fired on **every** wakeup, not only when `poll()` returns 0 — a window
    /// switch on fd #0 must not silently defer an already-due `DestroyGrace`).
    pub fn take_due(&mut self, now: MonoInstant) -> Vec<Timer> {
        let mut due = Vec::new();
        for timer in [
            Timer::DestroyGrace,
            Timer::TitleDebounce,
            Timer::ReconnectBackoff,
            Timer::PauseExpiry,
            Timer::SessionReresolve,
        ] {
            if let Some(at) = *self.slot(timer) {
                if at.0 <= now.0 {
                    *self.slot(timer) = None;
                    due.push(timer);
                }
            }
        }
        due
    }
}

/// Per-wakeup drain cap for X11's userspace event queue (design §2 D-6). The fd can be empty
/// while `x11rb`'s internal queue is not; draining fully before polling again is what avoids
/// the classic xcb reactor hang. A budget bounds the cost of a burst so one source can never
/// starve the others.
pub const X11_BUDGET: u32 = 64;

/// Per-wakeup drain cap for the logind bridge channel (design §2 D-6), the `dbus_backlog` half
/// of the same fairness rule.
pub const DBUS_BUDGET: u32 = 32;

/// The timeout to hand `poll(2)` this wakeup (design §2 D-6). If a budgeted drain hit its cap
/// there is known work left over: sleeping on it would defer already-ready data, so the
/// timeout collapses to `PollTimeout::ZERO` regardless of what `deadlines` would otherwise
/// compute — a busy source is serviced again immediately, never starved by a distant deadline.
pub fn timeout_for_wakeup(
    deadlines: &Deadlines,
    now: MonoInstant,
    x11_backlog: bool,
    dbus_backlog: bool,
) -> PollTimeout {
    if x11_backlog || dbus_backlog {
        PollTimeout::ZERO
    } else {
        deadlines.poll_timeout(now)
    }
}

/// Calls `poll_once` with a timeout freshly recomputed from `deadlines`/`clock` on every
/// attempt, retrying on `EINTR` (design §2 D-6: `Err(Errno::EINTR) => continue`, looping back
/// to the top of the reactor's own loop rather than re-issuing the same syscall argument). A
/// signal landing mid-`poll()` must never let an already-armed deadline's *effective* wait
/// grow — that only holds if the timeout is recomputed against the current `now`, not reused
/// from before the interruption.
pub fn poll_retrying<F>(
    deadlines: &Deadlines,
    clock: &dyn crate::clock::Clock,
    mut poll_once: F,
) -> nix::Result<i32>
where
    F: FnMut(PollTimeout) -> nix::Result<i32>,
{
    loop {
        let timeout = deadlines.poll_timeout(clock.now_mono());
        match poll_once(timeout) {
            Err(nix::errno::Errno::EINTR) => continue,
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock, WallTs};
    use std::time::Duration;

    // --- 14.3: budget exhaustion yields PollTimeout::ZERO, never a sleep -------------------

    #[test]
    fn x11_budget_exhaustion_forces_a_zero_timeout_even_with_a_distant_deadline() {
        let clock = FakeClock::new(WallTs(0));
        let mut deadlines = Deadlines::new();
        let far_future = MonoInstant(clock.now_mono().0 + Duration::from_secs(60));
        deadlines.arm(Timer::ReconnectBackoff, far_future);

        let timeout = timeout_for_wakeup(&deadlines, clock.now_mono(), true, false);

        assert_eq!(
            timeout,
            PollTimeout::ZERO,
            "an exhausted X11 drain budget means known work is left; sleeping on a distant \
             deadline instead would defer it, which is exactly the buffered-queue hang D-6 \
             exists to prevent"
        );
    }

    #[test]
    fn dbus_budget_exhaustion_also_forces_a_zero_timeout() {
        let clock = FakeClock::new(WallTs(0));
        let deadlines = Deadlines::new();

        let timeout = timeout_for_wakeup(&deadlines, clock.now_mono(), false, true);

        assert_eq!(timeout, PollTimeout::ZERO);
    }

    #[test]
    fn no_backlog_falls_through_to_the_ordinary_deadline_computation() {
        let clock = FakeClock::new(WallTs(0));
        let deadlines = Deadlines::new();

        let timeout = timeout_for_wakeup(&deadlines, clock.now_mono(), false, false);

        assert_eq!(timeout, PollTimeout::NONE);
    }

    // --- 14.3: EINTR retries with a freshly recomputed timeout, never a stale one ----------

    #[test]
    fn eintr_retry_recomputes_the_timeout_instead_of_reusing_the_pre_interruption_value() {
        let clock = FakeClock::new(WallTs(0));
        let mut deadlines = Deadlines::new();
        let target = MonoInstant(clock.now_mono().0 + Duration::from_secs(10));
        deadlines.arm(Timer::PauseExpiry, target);

        let mut seen_timeouts = Vec::new();
        let mut call = 0;
        let result = poll_retrying(&deadlines, &clock, |timeout| {
            seen_timeouts.push(timeout);
            call += 1;
            if call == 1 {
                // Simulate real time elapsing inside the interrupted `poll(2)` call before a
                // signal landed self-pipe-side.
                clock.advance(Duration::from_secs(4));
                Err(nix::errno::Errno::EINTR)
            } else {
                Ok(0)
            }
        });

        assert!(
            result.is_ok(),
            "the retry must eventually surface poll()'s real result"
        );
        assert_eq!(
            seen_timeouts.len(),
            2,
            "EINTR must cause exactly one retry here"
        );
        let first_ms = seen_timeouts[0]
            .as_millis()
            .expect("armed deadline is not NONE");
        let second_ms = seen_timeouts[1]
            .as_millis()
            .expect("armed deadline is not NONE");
        assert!(
            second_ms < first_ms,
            "the retry's timeout ({second_ms}ms) must reflect the 4s that already elapsed, \
             not reuse the pre-interruption {first_ms}ms — an unarmed-looking-armed deadline \
             whose remaining time silently grows on every signal never actually fires"
        );
    }

    // --- 14.1: `poll_timeout` rounds a 250.4ms deadline UP to 251ms, never truncates -------

    #[test]
    fn poll_timeout_rounds_a_sub_millisecond_remainder_up() {
        let clock = FakeClock::new(WallTs(0));
        let base = clock.now_mono();
        clock.advance(Duration::from_micros(250_400));
        let target = clock.now_mono();

        let mut deadlines = Deadlines::new();
        deadlines.arm(Timer::DestroyGrace, target);

        let timeout = deadlines.poll_timeout(base);

        assert_eq!(
            timeout.as_millis(),
            Some(251),
            "a 250.4ms remaining deadline must round UP to 251ms (nix's \
             TryFrom<Duration> truncates via Duration::as_millis(); rounding \
             down here would wake the reactor before the deadline is due and \
             spin — the D-2 regression this test pins)"
        );
    }

    #[test]
    fn poll_timeout_with_no_armed_deadline_is_none() {
        let deadlines = Deadlines::new();
        let clock = FakeClock::new(WallTs(0));

        assert_eq!(
            deadlines.poll_timeout(clock.now_mono()),
            nix::poll::PollTimeout::NONE
        );
    }
}
