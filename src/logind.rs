//! `SessionMonitor` trait (D-13) and `FakeSessionMonitor`, its test double.
//!
//! **Phase 12, tasks 12.1-12.3 (RF-5).** `SessionMonitor` is the in-house trait D-13
//! specifies; `FakeSessionMonitor` drives every degraded path with no D-Bus at all.
//! `LockedHintTracker` turns raw `LockedHint` samples into the
//! `SourceEvent::SessionLocked`/`SessionUnlocked` edges `tracker.rs` already knows how to
//! apply (RF-5) — a `Lock()` D-Bus signal is not even a variant this type accepts, so it
//! cannot, by construction, produce a transition.

use std::cell::RefCell;
use std::collections::VecDeque;

use crate::tracker::SourceEvent;

/// An error surfaced by a `SessionMonitor` operation. Owned data only, matching
/// `tracker::SourceError`'s module-boundary convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionError(pub String);

/// D-13's in-house trait. `ZbusSessionMonitor` is the only implementor naming a `zbus` type;
/// `FakeSessionMonitor` drives every scenario below with none.
pub trait SessionMonitor: Send {
    /// RF-5's source of truth for session lock.
    fn locked_hint(&self) -> Result<bool, SessionError>;
    /// RF-27: acquire the `delay` sleep inhibitor.
    fn take_sleep_inhibitor(&mut self) -> Result<(), SessionError>;
    /// RF-27: release the held inhibitor descriptor so suspend may proceed.
    fn release_sleep_inhibitor(&mut self);
    /// RF-26: resolve the daemon's own session by PID.
    fn resolve_session(&mut self, pid: u32) -> Result<(), SessionError>;
}

/// Pops the next scripted response, or returns the last one again if only one remains — so a
/// test can push a short sequence and keep calling past its end without a panic.
fn next_scripted<T: Clone>(queue: &mut VecDeque<T>) -> Option<T> {
    if queue.len() > 1 {
        queue.pop_front()
    } else {
        queue.front().cloned()
    }
}

/// Test double for `SessionMonitor` (D-13, task 12.1): every response is scripted, so RF-5,
/// RF-26 and RF-27's degraded paths — the ones a real D-Bus session cannot cheaply force — are
/// reachable in a unit test.
#[derive(Debug, Default)]
pub struct FakeSessionMonitor {
    locked_hint_script: RefCell<VecDeque<Result<bool, SessionError>>>,
    take_inhibitor_script: VecDeque<Result<(), SessionError>>,
    resolve_session_script: VecDeque<Result<(), SessionError>>,
    pub release_calls: u32,
    pub resolved_pids: Vec<u32>,
}

impl FakeSessionMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_locked_hint(&mut self, response: Result<bool, SessionError>) {
        self.locked_hint_script.get_mut().push_back(response);
    }

    pub fn push_take_inhibitor(&mut self, response: Result<(), SessionError>) {
        self.take_inhibitor_script.push_back(response);
    }

    pub fn push_resolve_session(&mut self, response: Result<(), SessionError>) {
        self.resolve_session_script.push_back(response);
    }
}

impl SessionMonitor for FakeSessionMonitor {
    fn locked_hint(&self) -> Result<bool, SessionError> {
        let mut script = self.locked_hint_script.borrow_mut();
        next_scripted(&mut script).unwrap_or(Ok(false))
    }

    fn take_sleep_inhibitor(&mut self) -> Result<(), SessionError> {
        next_scripted(&mut self.take_inhibitor_script).unwrap_or(Ok(()))
    }

    fn release_sleep_inhibitor(&mut self) {
        self.release_calls += 1;
    }

    fn resolve_session(&mut self, pid: u32) -> Result<(), SessionError> {
        self.resolved_pids.push(pid);
        next_scripted(&mut self.resolve_session_script).unwrap_or(Ok(()))
    }
}

/// Turns raw `LockedHint` samples into the edge `SourceEvent`s RF-5 requires (tasks 12.2/12.3).
/// A `Lock()` signal is not a variant this type accepts at all — only `LockedHint` values are
/// — so observing one with no accompanying property change cannot, by construction, produce a
/// transition (RF-5 scenario 1).
#[derive(Debug, Default)]
pub struct LockedHintTracker {
    last_known: bool,
    /// Set by `seed` or `observe`; `observe` only emits an edge once this was already true
    /// when it runs.
    has_baseline: bool,
    /// Set only by `observe`. Guards `seed`: once a real observation has happened, a later
    /// (possibly stale, possibly racing-behind) startup seed must never overwrite it, or it
    /// could silently swallow the next genuine transition (hard-won lesson: a naive
    /// unconditional reseed did exactly this to the next real unlock).
    observed: bool,
}

impl LockedHintTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the baseline from a value read before any transition has been observed (e.g. a
    /// one-shot `locked_hint()` poll at startup). A no-op once `observe` has run for real.
    pub fn seed(&mut self, locked: bool) {
        if !self.observed {
            self.last_known = locked;
            self.has_baseline = true;
        }
    }

    /// Records an observed `LockedHint` value, returning the crossed edge's `SourceEvent`, if
    /// any.
    pub fn observe(&mut self, locked: bool) -> Option<SourceEvent> {
        let event = if !self.has_baseline {
            None
        } else if locked && !self.last_known {
            Some(SourceEvent::SessionLocked)
        } else if !locked && self.last_known {
            Some(SourceEvent::SessionUnlocked)
        } else {
            None
        };
        self.last_known = locked;
        self.has_baseline = true;
        self.observed = true;
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 12.1: trait + FakeSessionMonitor -----------------------------------------------

    #[test]
    fn fake_session_monitor_replays_scripted_locked_hint() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_locked_hint(Ok(false));
        fake.push_locked_hint(Ok(true));

        assert_eq!(fake.locked_hint(), Ok(false));
        assert_eq!(fake.locked_hint(), Ok(true));
        // Sticky at the last scripted value past the end of the script.
        assert_eq!(fake.locked_hint(), Ok(true));
    }

    #[test]
    fn fake_session_monitor_records_resolve_session_pid_and_release_calls() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_resolve_session(Ok(()));

        assert_eq!(fake.resolve_session(4242), Ok(()));
        assert_eq!(fake.resolved_pids, vec![4242]);

        fake.release_sleep_inhibitor();
        fake.release_sleep_inhibitor();
        assert_eq!(fake.release_calls, 2);
    }

    // --- 12.2/12.3: LockedHintTracker (RF-5, all three scenarios) -----------------------

    #[test]
    fn lock_signal_with_no_locked_hint_change_produces_no_transition() {
        // RF-5 scenario 1: a `Lock()` signal was observed elsewhere but LockedHint itself
        // never changed — this type only ever sees `LockedHint` samples, so feeding it the
        // same still-false value twice must never emit an edge.
        let mut tracker = LockedHintTracker::new();
        tracker.seed(false);

        assert_eq!(tracker.observe(false), None);
        assert_eq!(tracker.observe(false), None);
    }

    #[test]
    fn locked_hint_false_to_true_emits_session_locked() {
        // RF-5 scenario 2.
        let mut tracker = LockedHintTracker::new();
        tracker.seed(false);

        assert_eq!(tracker.observe(true), Some(SourceEvent::SessionLocked));
    }

    #[test]
    fn locked_hint_true_to_false_emits_session_unlocked() {
        // RF-5 scenario 3.
        let mut tracker = LockedHintTracker::new();
        tracker.seed(true);

        assert_eq!(tracker.observe(false), Some(SourceEvent::SessionUnlocked));
    }

    #[test]
    fn a_stale_seed_after_a_real_observation_never_overwrites_it() {
        // Hard-won regression: seeding must be a no-op once a real observation has happened,
        // or a late/stale startup seed can permanently swallow the next real unlock.
        let mut tracker = LockedHintTracker::new();
        tracker.seed(false);
        assert_eq!(tracker.observe(true), Some(SourceEvent::SessionLocked));

        // A late seed racing in after the real observation must not clobber `last_known`.
        tracker.seed(false);

        assert_eq!(tracker.observe(false), Some(SourceEvent::SessionUnlocked));
    }

    #[test]
    fn locked_hint_tracker_end_to_end_through_a_real_tracker() {
        // Ties the literal spec wording ("closes the current interval ... opens a `locked`
        // interval") to this module's output, reusing the already-verified `Tracker` (see
        // `tracker.rs::session_locked_transitions_active_to_locked`) rather than
        // re-deriving its interval bookkeeping here.
        use crate::clock::{Clock, FakeClock, WallTs};
        use crate::store::{IntervalState, NewInterval};
        use crate::tracker::{Effect, Tracker};

        let clock = FakeClock::new(WallTs(2_000));
        let mut app_tracker = Tracker::new();
        let mut lock_tracker = LockedHintTracker::new();
        lock_tracker.seed(false);

        app_tracker.on_event(
            SourceEvent::ActiveWindow(Some(crate::tracker::WindowInfo {
                app_id: "firefox".to_string(),
                title: crate::exclude::SafeTitle::empty(),
                pid: None,
            })),
            clock.now_wall(),
            clock.now_mono(),
        );

        clock.advance(std::time::Duration::from_secs(10));
        let event = lock_tracker.observe(true);
        assert_eq!(event, Some(SourceEvent::SessionLocked));

        let effects = app_tracker.on_event(event.unwrap(), clock.now_wall(), clock.now_mono());

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(2_010),
                open: NewInterval {
                    app: "?".to_string(),
                    title: "-".to_string(),
                    pid: None,
                    state: IntervalState::Locked,
                }
            }]
        );
    }
}
