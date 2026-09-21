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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock, WallTs};
    use std::time::Duration;

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
