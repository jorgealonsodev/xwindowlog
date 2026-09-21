//! `poll` loop, `PollFd` set, `Deadlines`, budgets, `EINTR` handling, `impl WindowSource for ReactorSource`.
//!
//! Design §2 D-2/D-6: the reactor's own "highest-risk logic" (§4 File Changes rationale for
//! splitting this module out of `main.rs`). This module owns no domain logic — it translates
//! fd readiness and deadline expiry into `tracker::SourceEvent`s; state-transition decisions
//! stay in `tracker.rs` (task 14.12).

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::Uid;

use crate::clock::{Clock, MonoInstant};
use crate::control::{self, Accepted, PauseState, CLIENT_DEADLINE};
use crate::signals::SelfPipe;
use crate::tracker::{SourceError, SourceEvent, Timer, WindowSource};

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
            Some((_, at)) => timeout_until(now, at),
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

/// Shared rounding logic (design §12 V-3) between [`Deadlines::poll_timeout`] and the
/// external-deadline merge `ReactorSource::next_event` performs against its `WindowSource`
/// caller's own `deadline` parameter — both must round a sub-millisecond remainder UP the
/// same way.
fn timeout_until(now: MonoInstant, at: MonoInstant) -> PollTimeout {
    let left = at.0.saturating_duration_since(now.0);
    if left.is_zero() {
        return PollTimeout::ZERO;
    }
    let ms = left.as_millis() + u128::from(!left.subsec_nanos().is_multiple_of(1_000_000));
    PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)
}

/// The smaller of two timeouts, treating [`PollTimeout::NONE`] (infinite) as larger than any
/// finite value — the opposite of what `PollTimeout`'s own derived `Ord` would say, since its
/// `NONE` is internally `-1`.
fn min_timeout(a: PollTimeout, b: PollTimeout) -> PollTimeout {
    match (a.is_none(), b.is_none()) {
        (true, true) => PollTimeout::NONE,
        (true, false) => b,
        (false, true) => a,
        (false, false) => {
            if a.as_millis() <= b.as_millis() {
                a
            } else {
                b
            }
        }
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

/// Calls `poll_once` with a timeout freshly recomputed by `timeout_for` on every attempt,
/// retrying on `EINTR` (design §2 D-6: `Err(Errno::EINTR) => continue`, looping back to the
/// top of the reactor's own loop rather than re-issuing the same syscall argument). A signal
/// landing mid-`poll()` must never let an already-armed deadline's *effective* wait grow —
/// that only holds if the timeout is recomputed against the current `now` on every retry, not
/// reused from before the interruption.
pub fn poll_retrying<T, F>(mut timeout_for: T, mut poll_once: F) -> nix::Result<i32>
where
    T: FnMut() -> PollTimeout,
    F: FnMut(PollTimeout) -> nix::Result<i32>,
{
    loop {
        let timeout = timeout_for();
        match poll_once(timeout) {
            Err(nix::errno::Errno::EINTR) => continue,
            other => return other,
        }
    }
}

/// Design §2 D-6 / daemon-lifecycle's own attribution requirement: every wakeup is
/// attributable to a real monitored source having data ready or a real armed deadline having
/// elapsed, never an unconditional periodic re-check. Recorded once per `poll(2)` return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WakeupCause {
    pub x11_ready: bool,
    pub logind_ready: bool,
    pub signals_ready: bool,
    pub listener_ready: bool,
    pub client_ready: bool,
    /// A named `Timer` (design §2 D-2) fired.
    pub deadline_due: bool,
    /// The caller's own `WindowSource::next_event` `deadline` parameter elapsed.
    pub external_deadline_elapsed: bool,
    /// A budgeted drain hit its cap last iteration, forcing a `PollTimeout::ZERO` re-check of
    /// a source already known to have more buffered work (design §2 D-6) — attributable to
    /// that known backlog, not an unconditional re-check.
    pub forced_by_backlog: bool,
}

impl WakeupCause {
    /// Whether at least one real cause explains this wakeup.
    pub fn is_attributable(&self) -> bool {
        self.x11_ready
            || self.logind_ready
            || self.signals_ready
            || self.listener_ready
            || self.client_ready
            || self.deadline_due
            || self.external_deadline_elapsed
            || self.forced_by_backlog
    }
}

/// A budgeted, userspace-buffered event source (design §2 D-6): X11's event queue and the
/// logind bridge channel both drain multiple buffered items off one `POLLIN` notification, so
/// a single read is not enough — the fd can be empty while the source's own queue is not. This
/// trait is the generic shape `ReactorSource` polls fd0/fd1 through; Phase 15's `main.rs`
/// composition instantiates it with adapters over the real `X11Source`/logind bridge. This
/// phase's own tests use synthetic pipe-backed doubles (tasks.md PR 14 Work Unit: "no real
/// X11/D-Bus needed here"), so no `x11rb`/`zbus` type is named anywhere in this module
/// (task 14.12).
pub trait BudgetedSource {
    /// The raw fd whose `POLLIN` means "at least one item may be ready to drain."
    fn as_raw_fd(&self) -> RawFd;

    /// Flushes any outbound requests before this wakeup's drain. A no-op default covers
    /// sources with nothing to flush (design §2 D-6: "outbound requests must actually leave").
    fn flush(&mut self) -> Result<(), SourceError> {
        Ok(())
    }

    /// One non-blocking attempt to pull the next already-buffered item, already translated
    /// into a `SourceEvent`. `Ok(None)` means genuinely empty right now — the drain-before-poll
    /// stopping condition.
    fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError>;
}

/// Drains `source` up to `budget` items into `out`, per D-6's fairness rule (service every
/// ready source once per wakeup, never one to exhaustion while others wait). Returns whether
/// the budget was exhausted — the caller's signal that known work is left over and the next
/// `poll()` timeout must collapse to `PollTimeout::ZERO` rather than sleep on it.
fn drain_budget<S: BudgetedSource>(
    source: &mut S,
    budget: u32,
    out: &mut VecDeque<SourceEvent>,
) -> Result<bool, SourceError> {
    let mut drained = 0;
    while drained < budget {
        match source.try_next()? {
            Some(event) => {
                out.push_back(event);
                drained += 1;
            }
            None => return Ok(false),
        }
    }
    Ok(true)
}

/// An already-accepted, credential-checked control-client fd (design §2 D-5). Kept
/// non-blocking end to end (task 14's CRITICAL fix): the reactor drives
/// `control::try_read_request_line` across as many wakeups as it takes, never
/// `control::service_connection`'s blocking read, so a peer that trickles bytes in slowly
/// cannot stall any other fd in the poll set. `deadline` is a single `Instant` captured at
/// `accept()` time and re-checked on every wakeup, never reset per read — the same 1s budget
/// `CLIENT_DEADLINE` names, enforced here instead of inside a blocking read loop.
struct ControlClient {
    stream: std::os::unix::net::UnixStream,
    partial: Vec<u8>,
    deadline: Instant,
}

/// Translates fd readiness and deadline expiry into `tracker::SourceEvent`s (design §2 D-8).
/// Owns the permanent fd table (design §2 D-2): fd0 `x11` (a `BudgetedSource`), fd1 `logind` (a
/// `BudgetedSource`), fd2 the signal self-pipe, fd3 the control-socket listener, plus up to
/// [`control::MAX_CONCURRENT_CLIENTS`] transient accepted control-client fds. Contains no domain
/// logic — state-transition decisions stay in `tracker.rs` (task 14.12).
pub struct ReactorSource<X, L, C> {
    x11: X,
    logind: L,
    signals: SelfPipe,
    listener: UnixListener,
    clients: Vec<ControlClient>,
    own_uid: Uid,
    deadlines: Deadlines,
    clock: C,
    /// Design §2 D-12's in-process counter half, behind an `Arc` so an observer (a shutdown
    /// logger in production; a test asserting "zero wakeups over an idle window" here) can
    /// read it without owning `self`, including while `next_event` is blocked inside `poll()`
    /// on another thread.
    wakeups: Arc<AtomicU64>,
    /// Task 14.10/D-12's attribution record: one [`WakeupCause`] pushed per `poll(2)` return,
    /// behind the same `Arc` pattern as `wakeups` for cross-thread test observability.
    wakeup_causes: Arc<Mutex<Vec<WakeupCause>>>,
    /// Events already drained but not yet handed to the caller (design §2 D-6's
    /// per-wakeup drain can find more than one item; `WindowSource::next_event` hands them out
    /// one at a time without polling again while this is non-empty).
    pending: VecDeque<SourceEvent>,
    /// Whether the control protocol currently believes the daemon is paused, purely to answer
    /// `AlreadyPaused`/`NotPaused` synchronously per design §5 — the real pause/resume ->
    /// `tracker`/`store` wiring is task 15.11's job; this flag only tracks what this module
    /// itself has already told a client, and is expected to move in lockstep with the
    /// `PauseExpiry` deadline once Phase 15 arms/cancels it through [`Self::arm_timer`].
    paused: bool,
}

impl<X: BudgetedSource, L: BudgetedSource, C: Clock> ReactorSource<X, L, C> {
    pub fn new(
        x11: X,
        logind: L,
        signals: SelfPipe,
        listener: UnixListener,
        own_uid: Uid,
        clock: C,
    ) -> Self {
        // Defense in depth: `accept_one` is only ever called after `poll(2)` reports the
        // listener readable, so this should never block either way — but a spurious `POLLIN`
        // (or a future refactor that calls it from elsewhere) must return `WouldBlock` rather
        // than stall the whole reactor on the daemon's single thread.
        let _ = listener.set_nonblocking(true);
        ReactorSource {
            x11,
            logind,
            signals,
            listener,
            clients: Vec::new(),
            own_uid,
            deadlines: Deadlines::new(),
            clock,
            wakeups: Arc::new(AtomicU64::new(0)),
            wakeup_causes: Arc::new(Mutex::new(Vec::new())),
            pending: VecDeque::new(),
            paused: false,
        }
    }

    /// How many times the underlying `poll(2)` call has returned (design §2 D-12's in-process
    /// counter half). Never incremented for anything except an actual `poll()` return.
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    /// A shared, clonable handle onto the wakeup counter, readable independently of `self`
    /// (design §2 D-12: "logged on shutdown" from wherever shutdown runs, not necessarily the
    /// reactor thread itself).
    pub fn wakeups_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.wakeups)
    }

    /// A shared, clonable handle onto the per-wakeup attribution record (task 14.10), readable
    /// independently of `self` for the same reason as [`Self::wakeups_handle`].
    pub fn wakeup_causes_handle(&self) -> Arc<Mutex<Vec<WakeupCause>>> {
        Arc::clone(&self.wakeup_causes)
    }

    /// Arms one of the five deadline kinds (design §2 D-2), for both this module's own
    /// internal timers (`ReconnectBackoff`, `PauseExpiry`, `SessionReresolve`) and the
    /// tracker-owned ones (`TitleDebounce`, `DestroyGrace`) applied here by Phase 15's
    /// composition when it processes a `tracker::Effect::ArmTimer`.
    pub fn arm_timer(&mut self, timer: Timer, at: MonoInstant) {
        self.deadlines.arm(timer, at);
    }

    /// Cancels a previously armed deadline (`tracker::Effect::CancelTimer`).
    pub fn cancel_timer(&mut self, timer: Timer) {
        self.deadlines.cancel(timer);
    }
}

/// One `accept()` per wakeup at most (design §2 D-6: "bounds the cost of a connect storm
/// without needing a rate limiter"). Only called when the listener itself reported `POLLIN`;
/// a queued backlog beyond one connection waits for the next wakeup rather than being drained
/// here, unlike the budgeted sources above. A free function (not a `ReactorSource` method) so
/// `WindowSource::next_event` can call it while a `PollFd` slice still holds other fields of
/// `self` borrowed (task 14.9).
fn accept_one(listener: &UnixListener, clients: &mut Vec<ControlClient>, own_uid: Uid) {
    let mut stderr = io::stderr();
    match control::accept(listener, clients.len(), own_uid, &mut stderr) {
        Ok(Accepted::Client(stream)) => {
            let _ = stream.set_nonblocking(true);
            clients.push(ControlClient {
                stream,
                partial: Vec::new(),
                deadline: Instant::now() + CLIENT_DEADLINE,
            });
        }
        Ok(Accepted::RejectedUid) | Ok(Accepted::RejectedCapacity) => {}
        Err(_) => {}
    }
}

/// Services every already-accepted client fd once: a completed request line is decided and
/// replied to immediately (translated into a `Pause`/`Resume` `SourceEvent` for the caller); a
/// client past its own deadline is dropped without a reply (design §5's 1s budget); everyone
/// else keeps their partial buffer for the next wakeup. Never blocks — this drives
/// `control::try_read_request_line` exclusively, never `control::service_connection`'s
/// blocking read (task 14's CRITICAL fix). A free function for the same disjoint-borrow reason
/// as [`accept_one`].
fn service_clients(
    clients: &mut Vec<ControlClient>,
    paused: &mut bool,
    now_wall: crate::clock::WallTs,
    out: &mut VecDeque<SourceEvent>,
) {
    let now = Instant::now();
    clients.retain_mut(|client| {
        if now >= client.deadline {
            return false;
        }
        match control::try_read_request_line(&mut client.stream, &mut client.partial) {
            Ok(Some(line)) => {
                if let Some(event) = decide(&line, paused, now_wall, &mut client.stream) {
                    out.push_back(event);
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    });
}

/// Decodes one complete request line, decides the reply against `paused` (the wire-protocol's
/// own `AlreadyPaused`/`NotPaused` bookkeeping — see [`ReactorSource::paused`]'s doc), writes
/// the reply, and returns the `Pause`/`Resume` `SourceEvent` a caller should feed to the
/// tracker. Reuses `control::parse_envelope`'s already-tested version-before-body decode (R3)
/// rather than re-implementing it here.
fn decide(
    line: &[u8],
    paused: &mut bool,
    now_wall: crate::clock::WallTs,
    stream: &mut std::os::unix::net::UnixStream,
) -> Option<SourceEvent> {
    use crate::control::{Request, Response};
    use std::io::Write as _;

    let (response, event) = match control::parse_envelope(line) {
        Ok(envelope) => {
            let state = if *paused {
                PauseState::Paused
            } else {
                PauseState::Active
            };
            let response = control::handle_request(&envelope.req, state);
            let event = match (&envelope.req, &response) {
                (Request::Pause { minutes }, Response::Ok { .. }) => {
                    *paused = true;
                    let until = minutes.map(|m| {
                        crate::clock::WallTs::new(now_wall.as_unix_secs() + i64::from(m) * 60)
                    });
                    Some(SourceEvent::Pause { until })
                }
                (Request::Resume, Response::Ok { .. }) => {
                    *paused = false;
                    Some(SourceEvent::Resume)
                }
                _ => None,
            };
            (response, event)
        }
        Err(response) => (response, None),
    };
    let mut body = serde_json::to_vec(&response).unwrap_or_default();
    body.push(b'\n');
    let _ = stream.write_all(&body);
    event
}

impl<X: BudgetedSource, L: BudgetedSource, C: Clock> WindowSource for ReactorSource<X, L, C> {
    /// Design §2 D-6's whole loop body, adapted to `WindowSource`'s one-event-per-call
    /// contract: drain-before-poll first (fd readiness from a *previous* wakeup may still hold
    /// buffered, undrained items), hand out anything already pending without polling again,
    /// and only actually call `poll(2)` — incrementing `wakeups` — once nothing anywhere is
    /// immediately available. Deadlines fire on **every** wakeup (task 14.4), not only when
    /// `poll()` returns 0.
    fn next_event(
        &mut self,
        deadline: Option<MonoInstant>,
    ) -> Result<Option<SourceEvent>, SourceError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            self.x11.flush()?;
            let x11_backlog = drain_budget(&mut self.x11, X11_BUDGET, &mut self.pending)?;
            let dbus_backlog = drain_budget(&mut self.logind, DBUS_BUDGET, &mut self.pending)?;

            let now_wall = self.clock.now_wall();
            service_clients(
                &mut self.clients,
                &mut self.paused,
                now_wall,
                &mut self.pending,
            );

            if self.signals.drain() > 0 {
                let levels = self.signals.take_levels();
                if levels.terminate || levels.interrupt {
                    self.pending.push_back(SourceEvent::Shutdown);
                }
                if levels.reload {
                    self.pending.push_back(SourceEvent::ReloadConfig);
                }
            }

            if !self.pending.is_empty() {
                continue;
            }

            // Nothing anywhere is immediately available: this is the one branch that actually
            // calls `poll(2)` (RNF-2's "zero wakeups" is this branch never being reached while
            // idle, not a fast loop around it).
            let (listener_ready, mut cause) = {
                // Safety: `x11`/`logind`/`signals` only expose a `RawFd`, not `AsFd`; each
                // fd is kept alive by its owning field for this entire block, and no fd is
                // closed before `pfds` (and the `BorrowedFd`s it holds) are dropped at the end
                // of this block.
                let x11_fd = unsafe { BorrowedFd::borrow_raw(self.x11.as_raw_fd()) };
                let logind_fd = unsafe { BorrowedFd::borrow_raw(self.logind.as_raw_fd()) };
                let signals_fd = unsafe { BorrowedFd::borrow_raw(self.signals.as_raw_fd()) };

                let mut pfds = vec![
                    PollFd::new(x11_fd, PollFlags::POLLIN),
                    PollFd::new(logind_fd, PollFlags::POLLIN),
                    PollFd::new(signals_fd, PollFlags::POLLIN),
                    PollFd::new(self.listener.as_fd(), PollFlags::POLLIN),
                ];
                for client in &self.clients {
                    pfds.push(PollFd::new(client.stream.as_fd(), PollFlags::POLLIN));
                }

                let deadlines = &self.deadlines;
                let clock = &self.clock;
                poll_retrying(
                    || {
                        let now = clock.now_mono();
                        let base = timeout_for_wakeup(deadlines, now, x11_backlog, dbus_backlog);
                        match deadline {
                            Some(at) => min_timeout(base, timeout_until(now, at)),
                            None => base,
                        }
                    },
                    |timeout| poll(&mut pfds, timeout),
                )
                .map_err(|e| SourceError(format!("poll(2) failed: {e}")))?;

                let cause = WakeupCause {
                    x11_ready: pfds[0].any().unwrap_or(false),
                    logind_ready: pfds[1].any().unwrap_or(false),
                    signals_ready: pfds[2].any().unwrap_or(false),
                    listener_ready: pfds[3].any().unwrap_or(false),
                    client_ready: pfds[4..].iter().any(|p| p.any().unwrap_or(false)),
                    forced_by_backlog: x11_backlog || dbus_backlog,
                    ..WakeupCause::default()
                };
                (cause.listener_ready, cause)
            };
            self.wakeups.fetch_add(1, Ordering::Relaxed);

            if listener_ready {
                accept_one(&self.listener, &mut self.clients, self.own_uid);
            }

            let now = self.clock.now_mono();
            let due = self.deadlines.take_due(now);
            cause.deadline_due = !due.is_empty();
            for timer in due {
                self.pending.push_back(SourceEvent::DeadlineElapsed(timer));
            }

            if let Some(at) = deadline {
                if now.0 >= at.0 {
                    cause.external_deadline_elapsed = true;
                }
            }
            self.wakeup_causes
                .lock()
                .expect("wakeup_causes lock poisoned")
                .push(cause);

            if self.pending.is_empty() && cause.external_deadline_elapsed {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock, WallTs};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// A synthetic stand-in for X11's/logind's `BudgetedSource` (tasks.md PR 14 Work Unit:
    /// "synthetic fds (pipes) standing in for the four real sources; no real X11/D-Bus needed
    /// here"). One queued event is emitted per byte read off the paired socket; reads are
    /// non-blocking, matching every real `BudgetedSource` impl's contract.
    struct SyntheticSource {
        reader: UnixStream,
        events: VecDeque<SourceEvent>,
    }

    impl SyntheticSource {
        fn pair() -> (Self, UnixStream) {
            let (reader, writer) = UnixStream::pair().expect("UnixStream::pair must succeed");
            reader
                .set_nonblocking(true)
                .expect("set_nonblocking must succeed");
            (
                SyntheticSource {
                    reader,
                    events: VecDeque::new(),
                },
                writer,
            )
        }

        fn push_event(&mut self, event: SourceEvent) {
            self.events.push_back(event);
        }
    }

    impl BudgetedSource for SyntheticSource {
        fn as_raw_fd(&self) -> RawFd {
            self.reader.as_raw_fd()
        }

        fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
            let mut buf = [0u8; 64];
            match self.reader.read(&mut buf) {
                Ok(0) => Ok(None),
                Ok(_) => Ok(self.events.pop_front()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(SourceError(e.to_string())),
            }
        }
    }

    /// A fresh, bound control-socket listener for tests that need the real fd table shape
    /// (design §2 D-2's fd3) without wiring a whole daemon around it.
    fn bind_test_listener(case: &str) -> (UnixListener, std::path::PathBuf) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "xwindowlog-test-reactor-{case}-{}-{n}.sock",
            std::process::id()
        ));
        let listener = control::bind(&path).expect("bind must succeed on a fresh path");
        (listener, path)
    }

    fn make_reactor_source(
        case: &str,
    ) -> (
        ReactorSource<SyntheticSource, SyntheticSource, FakeClock>,
        UnixStream,
        UnixStream,
        std::path::PathBuf,
    ) {
        let (x11, x11_writer) = SyntheticSource::pair();
        let (logind, logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, socket_path) = bind_test_listener(case);
        let clock = FakeClock::new(WallTs(0));
        let reactor = ReactorSource::new(x11, logind, signals, listener, Uid::current(), clock);
        (reactor, x11_writer, logind_writer, socket_path)
    }

    // --- 14.6: `ReactorSource` wakes exactly once for a synthetic X11 change, with no prior
    // read, and issues zero wakeups over a synthetic idle window with nothing pending anywhere
    // (window-capture "No busy-waiting between changes"; daemon-lifecycle "Zero wakeups over an
    // idle window") ------------------------------------------------------------------------

    #[test]
    fn reactor_source_wakes_exactly_once_for_a_synthetic_change_with_no_prior_read() {
        // Real signal delivery is process-global (see `signals::SIGNAL_TEST_GUARD`'s doc):
        // this reactor installs a real `SelfPipe`, so it must not run concurrently with any
        // test that raises a real SIGTERM/SIGINT/SIGHUP.
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, mut x11_writer, _logind_writer, _socket_path) =
            make_reactor_source("wakes-once");
        reactor.x11.push_event(SourceEvent::UserActive);

        let handle = std::thread::spawn(move || {
            // The write happens only after the reactor thread has had time to block inside
            // `poll(2)` — proving the event is delivered by a genuine wakeup, not found by a
            // pre-poll drain of data that was already sitting on the fd (which window-capture's
            // scenario explicitly forbids: "the daemon does not perform any window-state query
            // before that wakeup").
            std::thread::sleep(Duration::from_millis(100));
            x11_writer
                .write_all(b"x")
                .expect("write to the synthetic x11 fd must succeed");
            x11_writer
        });

        let event = reactor.next_event(None).expect("next_event must not error");
        handle.join().expect("writer thread must not panic");

        assert_eq!(event, Some(SourceEvent::UserActive));
        assert_eq!(
            reactor.wakeups(),
            1,
            "one synthetic change must cost exactly one poll(2) wakeup, never a spin"
        );
    }

    #[test]
    fn reactor_source_issues_zero_wakeups_over_an_idle_window_with_nothing_pending() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, mut x11_writer, _logind_writer, _socket_path) =
            make_reactor_source("idle-window");
        reactor.x11.push_event(SourceEvent::UserActive);
        let wakeups = reactor.wakeups_handle();

        let handle = std::thread::spawn(move || reactor.next_event(None));

        // A fast, deterministic stand-in for the daemon-lifecycle scenario's literal 60s idle
        // window (design §12 D-12 reserves the literal duration for the local benchmark, not
        // this unit test): if the loop were spinning instead of genuinely blocked in poll(),
        // this window would already show a nonzero count.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            wakeups.load(Ordering::Relaxed),
            0,
            "nothing pending anywhere must cost zero poll(2) wakeups over the idle window"
        );

        x11_writer
            .write_all(b"x")
            .expect("write to the synthetic x11 fd must succeed");
        let event = handle
            .join()
            .expect("reactor thread must not panic")
            .expect("next_event must not error");

        assert_eq!(event, Some(SourceEvent::UserActive));
        assert_eq!(wakeups.load(Ordering::Relaxed), 1);
    }

    // --- CRITICAL fix (carried over from Phase 13's R4-blocking-service-stalls-reactor): a
    // control client that trickles bytes in slowly must never stall any other fd. Proves
    // `service_clients` drives `control::try_read_request_line` (never
    // `control::service_connection`'s blocking read) from the poll loop. ---------------------

    #[test]
    fn a_dribbling_control_peer_never_stalls_the_reactor_from_servicing_other_fds() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, x11_writer, _logind_writer, socket_path) = make_reactor_source("dribble");
        let mut client = UnixStream::connect(&socket_path).expect("connect must succeed");

        // One byte every 150ms, never a trailing `\n`: this dribble deliberately never
        // completes a request line during this test, and spans 750ms — most of
        // `control::CLIENT_DEADLINE`'s 1s budget. If the reactor's control-fd handling ever
        // called the blocking `read_request_line`/`service_connection` path instead of
        // `try_read_request_line`, accepting this connection would park the whole reactor for
        // up to that 1s budget before it could look at any other fd.
        let dribble = std::thread::spawn(move || {
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(150));
                let _ = client.write_all(b"{");
            }
            client
        });

        // The unrelated X11 change arrives while the peer is still mid-dribble.
        let mut writer = x11_writer.try_clone().expect("try_clone must succeed");
        reactor.x11.push_event(SourceEvent::UserActive);
        let x11_delay = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            writer
                .write_all(b"x")
                .expect("write to the synthetic x11 fd must succeed");
        });

        let started = Instant::now();
        let event = reactor.next_event(None).expect("next_event must not error");
        let elapsed = started.elapsed();

        assert_eq!(event, Some(SourceEvent::UserActive));
        assert!(
            elapsed < Duration::from_millis(500),
            "delivering the unrelated X11 event took {elapsed:?}; a dribbling control peer \
             must never delay another fd anywhere close to CLIENT_DEADLINE's 1s budget"
        );

        x11_delay.join().expect("writer thread must not panic");
        dribble.join().expect("dribble thread must not panic");
    }

    // --- 14.8: control-socket accept() is limited to one per wakeup (bounds a connect-storm
    // without a rate limiter, D-6) -----------------------------------------------------------

    #[test]
    fn accept_is_limited_to_one_connection_per_wakeup() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();

        // A real `SystemClock`, not `FakeClock`: this test lets `poll(2)` actually block on a
        // real wall-clock deadline, so the deadline-elapsed check inside `next_event` (which
        // reads `self.clock.now_mono()`) must observe real time passing too, or it can never
        // see the external deadline as reached.
        let (x11, _x11_writer) = SyntheticSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, socket_path) = bind_test_listener("connect-storm");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );

        // Three connections queued in the listener's backlog before the reactor ever polls —
        // a connect storm. None of them ever sends a byte, so no `SourceEvent` is ever
        // produced; the reactor keeps waking up (each wakeup finding the listener still
        // backlogged) until the external deadline elapses.
        let _clients: Vec<UnixStream> = (0..3)
            .map(|_| UnixStream::connect(&socket_path).expect("connect must succeed"))
            .collect();

        let deadline = MonoInstant(Instant::now() + Duration::from_millis(200));
        let event = reactor
            .next_event(Some(deadline))
            .expect("next_event must not error");

        assert_eq!(event, None, "no client ever completed a request line");
        assert_eq!(
            reactor.clients.len(),
            3,
            "all three connections must eventually be accepted"
        );
        assert!(
            reactor.wakeups() >= 3,
            "accepting three connections one per wakeup costs at least three poll(2) wakeups \
             (observed {}); a lower count means the listener was drained in a single wakeup's \
             accept loop instead of being throttled to one accept() per wakeup",
            reactor.wakeups()
        );
    }

    // --- 14.10: a wakeup is always attributable to a real monitored source or a real armed
    // deadline, never an unconditional periodic re-check (daemon-lifecycle "A wakeup is
    // attributable to a real event or a real deadline") ---------------------------------------

    #[test]
    fn every_recorded_wakeup_is_attributable_to_a_real_source_or_deadline() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();

        // A real `SystemClock` (see `accept_is_limited_to_one_connection_per_wakeup`'s doc):
        // the second scripted wakeup relies on a real external deadline actually elapsing.
        let (mut x11, x11_writer) = SyntheticSource::pair();
        x11.push_event(SourceEvent::UserActive);
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener("attribution");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );
        let causes = reactor.wakeup_causes_handle();

        // Scripted run: one real fd-readiness wakeup (the synthetic X11 change) followed by
        // one real external-deadline wakeup (`next_event`'s own `deadline` parameter expiring
        // with nothing else pending).
        let mut writer = x11_writer;
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            writer
                .write_all(b"x")
                .expect("write to the synthetic x11 fd must succeed");
        });
        let first = reactor.next_event(None).expect("next_event must not error");
        handle.join().expect("writer thread must not panic");
        assert_eq!(first, Some(SourceEvent::UserActive));

        let deadline = MonoInstant(Instant::now() + Duration::from_millis(100));
        let second = reactor
            .next_event(Some(deadline))
            .expect("next_event must not error");
        assert_eq!(second, None);

        let recorded = causes.lock().unwrap();
        assert!(
            recorded.len() >= 2,
            "the scripted run must have produced at least two wakeups"
        );
        for (i, cause) in recorded.iter().enumerate() {
            assert!(
                cause.is_attributable(),
                "wakeup #{i} ({cause:?}) was recorded with no attributable cause at all — an \
                 unconditional periodic re-check, exactly what daemon-lifecycle's scenario \
                 forbids"
            );
        }
        assert!(
            recorded.iter().any(|c| c.x11_ready),
            "the first wakeup must be attributable to the synthetic X11 fd"
        );
        assert!(
            recorded.iter().any(|c| c.external_deadline_elapsed),
            "the second wakeup must be attributable to the external deadline elapsing"
        );
    }

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
        let result = poll_retrying(
            || deadlines.poll_timeout(clock.now_mono()),
            |timeout| {
                seen_timeouts.push(timeout);
                call += 1;
                if call == 1 {
                    // Simulate real time elapsing inside the interrupted `poll(2)` call before
                    // a signal landed self-pipe-side.
                    clock.advance(Duration::from_secs(4));
                    Err(nix::errno::Errno::EINTR)
                } else {
                    Ok(0)
                }
            },
        );

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
