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
use std::time::{Duration, Instant};

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

/// Bound on `wakeup_causes` (R3-wakeup-causes-unbounded): a long-lived daemon must not grow
/// this attribution record without limit, so it is a ring — oldest evicted first.
const WAKEUP_CAUSES_CAP: usize = 256;

/// Pushes onto the bounded ring, evicting the oldest entry once full, and recovers a poisoned
/// lock instead of `expect`ing it, so another holder's panic degrades this record rather than
/// taking the reactor thread down with it.
fn record_wakeup_cause(causes: &Mutex<VecDeque<WakeupCause>>, cause: WakeupCause) {
    let mut guard = causes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.len() >= WAKEUP_CAUSES_CAP {
        guard.pop_front();
    }
    guard.push_back(cause);
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

    /// Attempts to recover this source after a fault (RF-32's mid-run X11 reconnection). Most
    /// sources have no such notion — the logind bridge has nothing analogous to "the display
    /// went away and came back" — so the default means exactly that: "this source does not
    /// support recovery," and every existing `BudgetedSource` (`LogindAdapter`/`LogindSource`)
    /// keeps that default unchanged (RF-32 T3).
    ///
    /// `entropy` is forwarded, unused by the default, to whichever backoff policy an
    /// overriding implementor drives internally (`x11::ReconnectBackoff::next_delay`'s own
    /// parameter — same convention, so a caller supplying real entropy doesn't need to know
    /// which sources care).
    ///
    /// A `Some(_)` return means an attempt was actually made; `RecoveryOutcome` tells a future
    /// reactor-side caller (RF-32 T4/T5) everything it needs to act: whether an outage just
    /// opened, how long to wait before retrying, or whether the source was restored — telling
    /// it exactly which `SourceEvent` to emit. An overriding implementor is responsible for
    /// installing any replacement source itself before returning (RF-32 T3:
    /// `X11Adapter::recover` does this via `replace_source`) — this module's own fd-liveness
    /// contract already covers the rest: `ReactorSource::next_event` re-queries
    /// `self.x11.as_raw_fd()` on every poll iteration, so a replaced connection needs no
    /// separate change notification.
    fn recover(&mut self, _entropy: u64) -> Option<RecoveryOutcome> {
        None
    }
}

/// The outcome of one `BudgetedSource::recover` attempt.
///
/// This mirrors `x11::ReconnectAttempt`'s three-outcome contract (RF-32's feature doc,
/// `src/x11.rs:1649-1671`: whether an outage just opened, whether it's still down, or whether
/// it was restored) rather than reusing that type directly, for two reasons:
///
/// 1. This module's own doc comment on `BudgetedSource` states the existing invariant this
///    type must not break: "no `x11rb`/`zbus` type is named anywhere in this module"
///    (task 14.12) — `ReconnectAttempt::Restored` carries `source: Box<x11::X11Source>`, an
///    X11-specific type wrapping `x11rb::rust_connection::RustConnection`, and naming it here
///    would pull a concrete X11 type into the trait every `BudgetedSource` implementor
///    (including `LogindAdapter`) must depend on, undermining the feature doc's own stated
///    goal of keeping `ReactorSource<X, L, C>` generic.
/// 2. It would be a lie anyway: RF-32 T3 (`X11Adapter::recover`) installs the reconnected
///    source into its own field via `replace_source` before returning, exactly so a future
///    reactor caller never has to touch the source object directly. `X11Source` isn't `Clone`
///    (it owns a live connection/fd), so a type that DID carry the replacement source could
///    never also be handed back here once `X11Adapter` had already moved it into `self.source`.
#[derive(Debug)]
pub enum RecoveryOutcome {
    /// The first failure of a new outage. The caller MUST translate this into exactly one
    /// `SourceEvent::DisplayLost` (RF-6/RF-32) — never again until `Restored` is observed.
    OutageOpened { retry_after: Duration },
    /// A subsequent failed attempt during an outage already reported via `OutageOpened`. No
    /// additional event — re-arm `Timer::ReconnectBackoff` for `retry_after` and keep waiting.
    StillDown { retry_after: Duration },
    /// Recovery succeeded; the implementor already installed the replacement source (see this
    /// trait's `recover` doc). The caller MUST translate this into exactly one
    /// `SourceEvent::DisplayRestored`. `outage_was_open` is `false` when the very first attempt
    /// of an outage succeeded immediately, so no `OutageOpened` preceded it — the caller must
    /// then emit `SourceEvent::DisplayLost` before `DisplayRestored`, mirroring
    /// `x11::ReconnectAttempt::Restored`'s own doc contract exactly.
    Restored { outage_was_open: bool },
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
/// `CLIENT_DEADLINE` names, enforced here instead of inside a blocking read loop — and also
/// folded into the `poll(2)` timeout, so an idle reactor still wakes to reap it.
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
    wakeup_causes: Arc<Mutex<VecDeque<WakeupCause>>>,
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
    /// RF-32 U1's gate: `true` from the moment `recover_x11` reports `OutageOpened`/`StillDown`
    /// until it reports `Restored`. While `true`, `next_event` does not call `service_x11`
    /// (and therefore does not call `X::recover`/`Reconnector::attempt`) at all — the *only*
    /// path back to another attempt is `Timer::ReconnectBackoff` actually elapsing. Without
    /// this, `next_event`'s drain-before-poll loop restarts immediately after `recover_x11`
    /// arms the timer and `continue`s, so `service_x11` fails again against the still-dead
    /// source before `poll(2)` is ever reached — the exact defect this unit fixes.
    x11_down: bool,
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
            wakeup_causes: Arc::new(Mutex::new(VecDeque::new())),
            pending: VecDeque::new(),
            paused: false,
            x11_down: false,
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
    pub fn wakeup_causes_handle(&self) -> Arc<Mutex<VecDeque<WakeupCause>>> {
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

    /// Reaches the owned logind source for suspend-inhibitor coordination (Phase 15
    /// composition, task 15.10's RF-27 wiring): sequencing a `PrepareForSleep` reaction
    /// strictly after its own interval-closing effect is committed is `main.rs`'s job, not
    /// this module's, and doing that from outside requires reaching the exact adapter this
    /// reactor was built with.
    pub fn logind_mut(&mut self) -> &mut L {
        &mut self.logind
    }

    /// Marks this module's own `paused` bookkeeping resumed (task 15.11). A manual `resume`
    /// request already updates it internally (`decide`, driven through `service_clients`), but
    /// an unattended `PauseExpiry` deadline firing has no other path back into this module —
    /// `self.paused`'s own doc says it is "expected to move in lockstep with the `PauseExpiry`
    /// deadline once Phase 15 arms/cancels it"; this is that lockstep's other half.
    pub fn mark_resumed(&mut self) {
        self.paused = false;
    }

    /// Flushes and drains the X11 source only — split out of `next_event` so a mid-run error
    /// from either call can be caught in one place (RF-32 T4) without disturbing
    /// `self.logind`'s own, unrelated `?` propagation right next to it.
    fn service_x11(&mut self) -> Result<bool, SourceError> {
        self.x11.flush()?;
        drain_budget(&mut self.x11, X11_BUDGET, &mut self.pending)
    }

    /// RF-32 T4/U1: intercepts a mid-run X11 fault caught by `service_x11`, and is also the
    /// sole handler `next_event` calls when `Timer::ReconnectBackoff` elapses (U1). Drives one
    /// recovery attempt through the `BudgetedSource` seam (`X::recover`, RF-32 T3) and queues
    /// exactly the `SourceEvent` sequence `RecoveryOutcome`'s own doc contract requires,
    /// mirroring `x11::ReconnectAttempt`'s — the policy `Reconnector::attempt` actually drives.
    /// Queues onto `self.pending` rather than returning a value: every caller just `continue`s
    /// (or falls through to) the loop and the top-of-loop `pop_front` hands the event straight
    /// back out.
    ///
    /// Also owns `self.x11_down`, U1's gate: set once an outage opens or persists, so
    /// `next_event` stops calling `service_x11` (and therefore this method) until the armed
    /// deadline fires; cleared on `Restored`, so normal servicing resumes.
    fn recover_x11(&mut self) {
        match self.x11.recover(crate::x11::os_entropy()) {
            Some(RecoveryOutcome::OutageOpened { retry_after }) => {
                self.x11_down = true;
                self.pending.push_back(SourceEvent::DisplayLost);
                self.arm_reconnect_backoff(retry_after);
            }
            Some(RecoveryOutcome::StillDown { retry_after }) => {
                self.x11_down = true;
                self.arm_reconnect_backoff(retry_after);
            }
            Some(RecoveryOutcome::Restored { outage_was_open }) => {
                self.x11_down = false;
                if !outage_was_open {
                    self.pending.push_back(SourceEvent::DisplayLost);
                }
                self.pending.push_back(SourceEvent::DisplayRestored);
            }
            None => {
                // `X11Adapter::recover` (RF-32 T3) always returns `Some`; only a source that
                // never overrides `recover` (this trait's no-op default), or a test double
                // whose outcome queue ran out, can land here. There is nothing to translate and
                // `x11_down` is deliberately left as it was: if it was already `true`, staying
                // gated is the safer failure than silently resuming un-recovered; production's
                // `X11Adapter` never actually reaches this branch.
            }
        }
    }

    /// Arms `Timer::ReconnectBackoff` for `retry_after` from now, the same
    /// `clock.now_mono().checked_add(duration)` -> `arm_timer` mechanism `main.rs` already uses
    /// for `Timer::PauseExpiry` (`src/main.rs:391-393`). An unrepresentable overflow (never
    /// observed in practice — see `MonoInstant::checked_add`'s own doc) leaves the timer
    /// unarmed rather than panicking or arming the wrong instant.
    fn arm_reconnect_backoff(&mut self, retry_after: Duration) {
        if let Some(at) = self.clock.now_mono().checked_add(retry_after) {
            self.arm_timer(Timer::ReconnectBackoff, at);
        }
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
    let outcome = control::accept(listener, clients.len(), own_uid, &mut stderr);
    apply_accept_outcome(outcome, clients);
}

/// A persistent `accept()` failure never consumes the queued connection, so unthrottled it
/// spins `poll(2)` at 100% CPU. Split from [`accept_one`] so a test can inject a synthetic
/// `Err` without exhausting real fds.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(20);

fn apply_accept_outcome(outcome: io::Result<Accepted>, clients: &mut Vec<ControlClient>) {
    match outcome {
        Ok(Accepted::Client(stream)) => {
            let _ = stream.set_nonblocking(true);
            clients.push(ControlClient {
                stream,
                partial: Vec::new(),
                deadline: Instant::now() + CLIENT_DEADLINE,
            });
        }
        Ok(Accepted::RejectedUid) | Ok(Accepted::RejectedCapacity) => {}
        Err(e) => {
            eprintln!("xwindowlog: accept() failed, backing off {ACCEPT_ERROR_BACKOFF:?}: {e}");
            std::thread::sleep(ACCEPT_ERROR_BACKOFF);
        }
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

            // RF-32 U1's gate: while an outage is open, do not call `service_x11` (and
            // therefore do not call `X::recover`/`Reconnector::attempt`) at all. Without this,
            // a failed `service_x11` below would drive `recover_x11`, which `continue`s back to
            // the top of this very loop with `pending` still not holding anything that stops
            // it — `service_x11` runs again immediately against the still-dead source, forever,
            // and `poll(2)` (and the deadline this same call armed) is never reached. Gated,
            // this iteration instead falls straight through to servicing logind/signals/clients
            // and `poll(2)` below; the only way back to another attempt is
            // `Timer::ReconnectBackoff` actually elapsing (handled further down).
            let x11_backlog = if self.x11_down {
                false
            } else {
                match self.service_x11() {
                    Ok(backlog) => backlog,
                    Err(_) => {
                        // RF-32 T4: a mid-run X11 error must never escape `next_event` as an
                        // `Err` — that would unwind into `main.rs`'s startup-error path and
                        // exit the daemon (the feature doc's Decision). Route it through the T3
                        // seam instead; `self.logind`'s own drain right below keeps propagating
                        // via `?` unchanged — only this X11 slot is caught here.
                        self.recover_x11();
                        continue;
                    }
                }
            };
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
                // R3-client-deadline-unenforced: fold the earliest client deadline in too.
                let client_deadline = self.clients.iter().map(|c| c.deadline).min();
                poll_retrying(
                    || {
                        let now = clock.now_mono();
                        let mut timeout =
                            timeout_for_wakeup(deadlines, now, x11_backlog, dbus_backlog);
                        if let Some(at) = deadline {
                            timeout = min_timeout(timeout, timeout_until(now, at));
                        }
                        if let Some(at) = client_deadline {
                            timeout = min_timeout(timeout, timeout_until(now, MonoInstant(at)));
                        }
                        timeout
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
                if timer == Timer::ReconnectBackoff {
                    // RF-32 U1: the backoff deadline drives the next reconnect attempt
                    // internally rather than escaping as a raw `DeadlineElapsed` event —
                    // nothing outside this module owns X11 reconnection state
                    // (`tracker.rs`'s own doc: `reactor.rs` tracks this deadline "itself,
                    // outside the tracker"), and this is the design's "reactor-internal
                    // recovery" route. `recover_x11` queues whichever `SourceEvent`s the
                    // outcome requires (`DisplayRestored`, or nothing while still down) and,
                    // on success, clears `x11_down` so normal servicing resumes.
                    self.recover_x11();
                } else {
                    self.pending.push_back(SourceEvent::DeadlineElapsed(timer));
                }
            }

            if let Some(at) = deadline {
                if now.0 >= at.0 {
                    cause.external_deadline_elapsed = true;
                }
            }
            record_wakeup_cause(&self.wakeup_causes, cause);

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

    // --- BudgetedSource::recover's default (RF-32 T3) ---------------------------------------
    //
    // `SyntheticSource` (above) does not override `recover`, exactly like `LogindAdapter` and
    // `LogindSource` in `src/adapters.rs` — it "stands in for X11's/logind's `BudgetedSource`"
    // per its own doc comment, so it is a faithful proxy for "any implementor that does not opt
    // into recovery."

    #[test]
    fn default_recover_is_a_genuine_no_op_returning_none() {
        let (mut source, _writer) = SyntheticSource::pair();

        assert!(
            source.recover(0).is_none(),
            "a source that never overrides recover must fall through to BudgetedSource's \
             default, which means \"this source does not support recovery\""
        );
    }

    #[test]
    fn default_recover_does_not_disturb_already_queued_events() {
        // The genuine property under test is behavioral, not structural: calling the default
        // `recover` must not perturb the source's own state — proven here by an event queued
        // before the call still being the exact one drained after it, through the real
        // `try_next`/fd machinery, not by inspecting a private field.
        let (mut source, mut writer) = SyntheticSource::pair();
        source.push_event(SourceEvent::ActiveWindowDestroyed);
        writer
            .write_all(b"x")
            .expect("write to the paired socket must succeed");

        let _ = source.recover(0xdead_beef);

        assert_eq!(
            source.try_next().expect("try_next must not error"),
            Some(SourceEvent::ActiveWindowDestroyed),
            "the queued event must still be exactly what try_next drains after recover() runs"
        );
    }

    // --- RF-32 T4: a mid-run X11 error must be caught inside `next_event`, drive recovery
    // through the `BudgetedSource::recover` seam (RF-32 T3), and never itself escape as `Err`
    // ------------------------------------------------------------------------------------------

    /// A `BudgetedSource` double that can be told to fail its next `try_next` on demand, and to
    /// hand back a scripted `RecoveryOutcome` from `recover` — proof that `next_event` reacts to
    /// exactly what the `BudgetedSource` seam reports, without depending on a real
    /// `x11rb`/`Reconnector` type (task 14.12's "no x11rb/zbus type in this module" boundary,
    /// held even for this recoverable double). Also usable, unscripted, as a plain
    /// non-recovering source (its own `recover` falls through to `None` once the queue is
    /// empty) — used in the logind slot to prove RF-32 T4 does NOT generically swallow every
    /// `BudgetedSource`'s errors, only the X11 slot's.
    struct RecoverableSource {
        reader: UnixStream,
        events: VecDeque<SourceEvent>,
        fail_next_try_next: bool,
        /// RF-32 U1: how many MORE consecutive `try_next` calls must fail, simulating a
        /// connection that stays genuinely dead across several loop iterations rather than
        /// `fail_next_try_next`'s single point-in-time fault. Bounded (not an unconditional
        /// "always fail") so that a still-ungated `next_event` — which retries in a tight loop
        /// with no `poll(2)` in between — burns through it and returns instead of hanging the
        /// test suite; the resulting inflated `recover_call_count` is itself the proof the gate
        /// is missing.
        remaining_failures: u32,
        recover_outcomes: VecDeque<RecoveryOutcome>,
        /// RF-32 U1: counts every call to `recover` (i.e. every `Reconnector::attempt` a real
        /// `X11Adapter` would have made), independent of what outcome was queued for it. This
        /// is the mandatory test property's own counter — proving the gate — rather than a
        /// production concern of this double.
        recover_calls: u32,
    }

    impl RecoverableSource {
        fn pair() -> (Self, UnixStream) {
            let (reader, writer) = UnixStream::pair().expect("UnixStream::pair must succeed");
            reader
                .set_nonblocking(true)
                .expect("set_nonblocking must succeed");
            (
                RecoverableSource {
                    reader,
                    events: VecDeque::new(),
                    fail_next_try_next: false,
                    remaining_failures: 0,
                    recover_outcomes: VecDeque::new(),
                    recover_calls: 0,
                },
                writer,
            )
        }

        /// Makes the very next `try_next` call return a synthetic `SourceError`, mirroring the
        /// shape `X11Adapter::try_next` produces from a real mid-run `x11rb` failure — one
        /// error, then normal behavior resumes (RF-32's error is a single point-in-time fault,
        /// not a source that is permanently broken).
        fn fail_next_try_next(&mut self) {
            self.fail_next_try_next = true;
        }

        /// Makes the next `times` calls to `try_next` all fail — a source that stays down
        /// across iterations, unlike `fail_next_try_next`'s one-shot fault. This is what a
        /// gating test needs: under a correctly gated `next_event`, `try_next` is never called
        /// again while `x11_down` is set, so this budget is never touched past the first
        /// failure; under an ungated one, every loop restart calls it again and burns through
        /// the budget immediately, in the very call the test is asserting against.
        fn fail_try_next_persistently(&mut self, times: u32) {
            self.remaining_failures = times;
        }

        /// Queues one `RecoveryOutcome` for `recover` to hand back, in call order.
        fn queue_recover_outcome(&mut self, outcome: RecoveryOutcome) {
            self.recover_outcomes.push_back(outcome);
        }

        /// How many times `recover` (this double's stand-in for `Reconnector::attempt`) has
        /// actually been called so far.
        fn recover_call_count(&self) -> u32 {
            self.recover_calls
        }
    }

    impl BudgetedSource for RecoverableSource {
        fn as_raw_fd(&self) -> RawFd {
            self.reader.as_raw_fd()
        }

        fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err(SourceError(
                    "synthetic persistent mid-run source failure (RF-32 U1 test double)"
                        .to_string(),
                ));
            }
            if self.fail_next_try_next {
                self.fail_next_try_next = false;
                return Err(SourceError(
                    "synthetic mid-run source failure (RF-32 T4 test double)".to_string(),
                ));
            }
            let mut buf = [0u8; 64];
            match self.reader.read(&mut buf) {
                Ok(0) => Ok(None),
                Ok(_) => Ok(self.events.pop_front()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(SourceError(e.to_string())),
            }
        }

        fn recover(&mut self, _entropy: u64) -> Option<RecoveryOutcome> {
            self.recover_calls += 1;
            self.recover_outcomes.pop_front()
        }
    }

    /// Builds a reactor with a `RecoverableSource` in the X11 slot and a plain
    /// `SyntheticSource` in the logind slot — the shape every T4 test below needs, factored out
    /// so each test states only what makes it distinct (mirrors `make_reactor_source` above,
    /// which cannot be reused as-is because it fixes both slots to `SyntheticSource`).
    fn make_recoverable_x11_reactor(
        case: &str,
    ) -> (
        ReactorSource<RecoverableSource, SyntheticSource, FakeClock>,
        UnixStream,
    ) {
        let (x11, x11_writer) = RecoverableSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener(case);
        let clock = FakeClock::new(WallTs(0));
        let reactor = ReactorSource::new(x11, logind, signals, listener, Uid::current(), clock);
        (reactor, x11_writer)
    }

    #[test]
    fn a_mid_run_x11_error_produces_display_lost_instead_of_propagating() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, _x11_writer) = make_recoverable_x11_reactor("x11-error-display-lost");
        reactor.x11.fail_next_try_next();
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::OutageOpened {
                retry_after: Duration::from_millis(80),
            });

        let event = reactor
            .next_event(None)
            .expect("a mid-run X11 error must be caught inside next_event, never propagate");

        assert_eq!(
            event,
            Some(SourceEvent::DisplayLost),
            "an OutageOpened outcome must translate into exactly one DisplayLost event \
             (x11::ReconnectAttempt::OutageOpened's doc contract)"
        );
    }

    /// RF-32 U1's mandatory test property. `next_event` is drain-before-poll (`recover_x11` is
    /// called again on every loop restart unless something gates it), so a single-iteration
    /// test cannot distinguish "waits for the deadline" from "retries immediately" — this test
    /// drives a SECOND external call to `next_event` before the armed deadline elapses and
    /// proves `recover` (this double's stand-in for `Reconnector::attempt`) was not called
    /// again by it.
    ///
    /// Uses `fail_try_next_persistently`, not `fail_next_try_next`: a one-shot failure heals
    /// itself on the very next `try_next` call regardless of whether the gate exists, which
    /// makes the assertions below pass vacuously either way (verified by mutation: replacing
    /// the production `if self.x11_down` check with `if false` still left this test green
    /// against a one-shot double, because `service_x11` simply stopped erroring on the second
    /// call — see this unit's report for the exact mutation transcript). The source here stays
    /// down across several consecutive `try_next` calls, so an ungated retry loop actually
    /// observes it still failing and keeps calling `recover`.
    #[test]
    fn no_reconnect_attempt_happens_before_the_backoff_deadline_fires() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (x11, _x11_writer) = RecoverableSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener("reconnect-backoff-gated");
        // A real `SystemClock`, not `FakeClock`: proving the timer's own duration requires real
        // wall-clock time to actually pass, the same reason
        // `an_idle_client_past_its_deadline_is_reaped_and_frees_its_slot` above uses it.
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );
        // Stays down for 5 more `try_next` calls past the first failure — comfortably enough
        // that an ungated retry loop (which would call it again immediately, with nothing
        // between iterations) burns through the budget and inflates `recover_call_count` well
        // past 1 inside the second `next_event` call itself, rather than this test hanging.
        reactor.x11.fail_try_next_persistently(5);
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::OutageOpened {
                retry_after: Duration::from_millis(80),
            });

        // First external call: the outage opens. Exactly one `recover` call so far.
        let lost = reactor.next_event(None).expect("must not error");
        assert_eq!(lost, Some(SourceEvent::DisplayLost));
        assert_eq!(
            reactor.x11.recover_call_count(),
            1,
            "the first failure must drive exactly one recovery attempt"
        );

        // Second external call, well short of the armed 80ms backoff: the drain-before-poll
        // defect called `recover` again right here, on this very call, before ever reaching
        // `poll(2)`. Gated correctly, this call must reach `poll(2)`, time out against the
        // caller's own short deadline, and return `None` without touching `recover` again.
        let too_early_deadline = MonoInstant(Instant::now() + Duration::from_millis(20));
        let too_early = reactor
            .next_event(Some(too_early_deadline))
            .expect("must not error");
        assert_eq!(
            too_early, None,
            "nothing is due yet: the call must time out on the caller's own short deadline"
        );
        assert_eq!(
            reactor.x11.recover_call_count(),
            1,
            "THE mandatory assertion: driving next_event a second time, before the backoff \
             deadline elapses, must NOT increment the recovery-attempt count — proving the \
             retry is gated on the deadline rather than retried immediately on every loop \
             restart"
        );

        // Third external call, past the armed 80ms: only now must a second attempt happen.
        let due_deadline = MonoInstant(Instant::now() + Duration::from_millis(500));
        let _ = reactor
            .next_event(Some(due_deadline))
            .expect("must not error");
        assert_eq!(
            reactor.x11.recover_call_count(),
            2,
            "once the armed deadline has genuinely elapsed, exactly one more recovery attempt \
             must happen"
        );
    }

    /// The other half of the same property: once the deadline drives the next attempt and it
    /// succeeds, the caller must see exactly one `DisplayRestored` — never the raw
    /// `DeadlineElapsed(Timer::ReconnectBackoff)` event, which nothing outside this module
    /// (`tracker.rs`'s own doc: "`reactor.rs` tracks the unrelated `ReconnectBackoff` ... \
    /// deadline itself, outside the tracker") is equipped to act on.
    #[test]
    fn the_backoff_deadline_drives_the_next_attempt_and_a_success_emits_display_restored() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (x11, _x11_writer) = RecoverableSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener("reconnect-backoff-drives-restore");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );
        reactor.x11.fail_next_try_next();
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::OutageOpened {
                retry_after: Duration::from_millis(50),
            });
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::Restored {
                outage_was_open: true,
            });

        let lost = reactor.next_event(None).expect("must not error");
        assert_eq!(lost, Some(SourceEvent::DisplayLost));

        let due_deadline = MonoInstant(Instant::now() + Duration::from_millis(500));
        let restored = reactor
            .next_event(Some(due_deadline))
            .expect("must not error");
        assert_eq!(
            restored,
            Some(SourceEvent::DisplayRestored),
            "the elapsed backoff deadline must drive the next attempt internally and translate \
             a Restored outcome into exactly one DisplayRestored — never a raw \
             DeadlineElapsed(Timer::ReconnectBackoff)"
        );
    }

    /// The gate itself: while an outage is open, `next_event` must not call `service_x11`
    /// (and therefore not `recover`) again at all until the deadline fires — proven here by
    /// observing the reactor still services the OTHER sources (signals, in this case) normally
    /// while X11 stays down, rather than being stuck busy-retrying it.
    ///
    /// Uses `fail_try_next_persistently`, not `fail_next_try_next`, for the same reason as
    /// `no_reconnect_attempt_happens_before_the_backoff_deadline_fires`: a one-shot failure
    /// heals itself on the very next call, so an ungated `service_x11` would simply succeed on
    /// the second `next_event` here too — with or without the gate — and the
    /// `recover_call_count` assertion below would pass vacuously.
    #[test]
    fn other_sources_keep_being_serviced_normally_while_x11_is_down() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (x11, _x11_writer) = RecoverableSource::pair();
        let (mut logind, mut logind_writer) = SyntheticSource::pair();
        logind.push_event(SourceEvent::PrepareForSleep(true));
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener("reconnect-backoff-other-sources");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );
        reactor.x11.fail_try_next_persistently(5);
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::OutageOpened {
                retry_after: Duration::from_secs(3600),
            });

        let lost = reactor.next_event(None).expect("must not error");
        assert_eq!(lost, Some(SourceEvent::DisplayLost));

        // The backoff is armed an hour out, so nothing about it is due. If X11 being down
        // blocked the reactor from reaching `poll(2)` for any other source, this logind event
        // — already sitting on its own fd — would never be observed.
        logind_writer
            .write_all(b"x")
            .expect("write to the synthetic logind fd must succeed");
        let event = reactor
            .next_event(None)
            .expect("logind must still be serviced while X11 is down");
        assert_eq!(event, Some(SourceEvent::PrepareForSleep(true)));
        assert_eq!(
            reactor.x11.recover_call_count(),
            1,
            "servicing another source while X11 is down must not itself trigger another \
             recovery attempt"
        );
    }

    #[test]
    fn a_logind_error_still_propagates_and_is_not_recovered() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (x11, _x11_writer) = SyntheticSource::pair();
        let (mut logind, _logind_writer) = RecoverableSource::pair();
        logind.fail_next_try_next();
        // Deliberately no queued `recover` outcome: if `next_event` ever caught the logind
        // slot's error the way it catches X11's, `recover` would still be called here and this
        // test's own setup would silently hide the regression by returning `None` from the
        // default-shaped fallback. The distinguishing proof is `result.is_err()` below — only
        // the X11 slot may swallow its own `SourceError`.
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, _socket_path) = bind_test_listener("logind-error-propagates");
        let clock = FakeClock::new(WallTs(0));
        let mut reactor = ReactorSource::new(x11, logind, signals, listener, Uid::current(), clock);

        let result = reactor.next_event(None);

        assert!(
            result.is_err(),
            "a logind-shaped SourceError must still propagate as Err — drain_budget is generic \
             over S: BudgetedSource and RF-32 T4 must only special-case the X11 slot, never \
             every BudgetedSource"
        );
    }

    #[test]
    fn a_same_instant_recovery_emits_display_lost_before_display_restored() {
        // x11::ReconnectAttempt::Restored's doc contract (mirrored by RecoveryOutcome::Restored,
        // src/reactor.rs): when `outage_was_open` is false, the caller was never told to open
        // the outage's one `unknown` interval, and must still emit DisplayLost before
        // DisplayRestored so a same-instant recovery is recorded as the short gap it really
        // was, rather than disappearing entirely.
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, _x11_writer) =
            make_recoverable_x11_reactor("same-instant-recovery-display-lost-then-restored");
        reactor.x11.fail_next_try_next();
        reactor
            .x11
            .queue_recover_outcome(RecoveryOutcome::Restored {
                outage_was_open: false,
            });

        let first = reactor.next_event(None).expect("must not error");
        assert_eq!(
            first,
            Some(SourceEvent::DisplayLost),
            "outage_was_open: false must still open the unknown interval with DisplayLost first"
        );

        let second = reactor.next_event(None).expect("must not error");
        assert_eq!(
            second,
            Some(SourceEvent::DisplayRestored),
            "exactly one DisplayRestored must follow, closing the interval DisplayLost opened"
        );
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
        // produced; the reactor keeps waking up until the external deadline elapses. That
        // 200ms deadline stays shorter than `CLIENT_DEADLINE`'s 1s, so all three survive here
        // regardless of R3-client-deadline-unenforced's fix; see
        // `an_idle_client_past_its_deadline_is_reaped_and_frees_its_slot` for the reaping case.
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

    // --- R3-client-deadline-unenforced: an idle client's own 1s budget must wake the reactor
    // even when nothing else does -----------------------------------------------------------

    #[test]
    fn an_idle_client_past_its_deadline_is_reaped_and_frees_its_slot() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();

        // Real `SystemClock`: `ControlClient::deadline` is always a real `Instant`, so only
        // real wall-clock time can prove it was folded into the poll timeout.
        let (x11, _x11_writer) = SyntheticSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, socket_path) = bind_test_listener("idle-client-reaped");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );

        // One silent client, never sending a byte. The external deadline (1.5s) is set just
        // past `CLIENT_DEADLINE` (1s) so the call is bounded either way; only whether the
        // client's own budget was folded into the poll timeout decides whether it is reaped
        // *before* that unrelated external deadline fires.
        let _client = UnixStream::connect(&socket_path).expect("connect must succeed");
        let deadline = MonoInstant(Instant::now() + Duration::from_millis(1500));
        let event = reactor
            .next_event(Some(deadline))
            .expect("next_event must not error");

        assert_eq!(event, None, "a silent client never produces a SourceEvent");
        assert_eq!(
            reactor.clients.len(),
            0,
            "a client that never completes a line must be reaped once its own 1s deadline \
             elapses, freeing its MAX_CONCURRENT_CLIENTS slot, instead of surviving until an \
             unrelated external deadline"
        );
    }

    // --- 13.4a (regression, Phase 15+16): an oversized/unterminated request must be rejected
    // through the real production caller, not only the unit-level `try_read_request_line` ----

    #[test]
    fn an_oversized_request_without_a_newline_is_rejected_and_frees_its_slot() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();

        let (x11, _x11_writer) = SyntheticSource::pair();
        let (logind, _logind_writer) = SyntheticSource::pair();
        let signals = SelfPipe::install().expect("SelfPipe::install must succeed");
        let (listener, socket_path) = bind_test_listener("oversized-request");
        let mut reactor = ReactorSource::new(
            x11,
            logind,
            signals,
            listener,
            Uid::current(),
            crate::clock::SystemClock,
        );

        let mut client = UnixStream::connect(&socket_path).expect("connect must succeed");
        // No trailing `\n` anywhere: only the byte cap can end this, never a completed line.
        let payload = vec![b'x'; control::MAX_REQUEST_BYTES + 100];
        client.write_all(&payload).expect("write must succeed");

        let start = Instant::now();
        // Well short of CLIENT_DEADLINE's 1s: if this only passed because the idle-client
        // timeout fired instead of the byte cap, it would still be waiting past this deadline.
        let deadline = MonoInstant(Instant::now() + Duration::from_millis(400));
        let event = reactor
            .next_event(Some(deadline))
            .expect("next_event must not error");

        assert_eq!(
            event, None,
            "an oversized, unterminated request must never produce a SourceEvent"
        );
        assert_eq!(
            reactor.clients.len(),
            0,
            "a client whose request exceeded MAX_REQUEST_BYTES must be evicted, freeing its \
             MAX_CONCURRENT_CLIENTS slot"
        );
        assert!(
            start.elapsed() < CLIENT_DEADLINE,
            "the oversized request must be rejected well before CLIENT_DEADLINE's 1s idle \
             timeout, proving the byte cap itself fired, not the unrelated deadline"
        );

        // Matches the reject/close semantics `service_clients` already applies to every other
        // rejected client (deadline expiry, I/O error): no reply, connection just closes.
        let mut reply = Vec::new();
        match client.read_to_end(&mut reply) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected read error: {e}"),
        }
        assert!(reply.is_empty(), "an oversized request must get no reply");
    }

    // --- R3-accept-failure-spin: a persistent accept() failure must back off, never spin ----

    #[test]
    fn apply_accept_outcome_adds_the_client_on_success() {
        let mut clients = Vec::new();
        let (_listener, socket_path) = bind_test_listener("apply-outcome-success");
        let stream = UnixStream::connect(&socket_path).expect("connect must succeed");
        apply_accept_outcome(Ok(Accepted::Client(stream)), &mut clients);
        assert_eq!(
            clients.len(),
            1,
            "a successful accept must add exactly one client"
        );
    }

    #[test]
    fn apply_accept_outcome_ignores_a_rejection_without_backing_off() {
        let mut clients = Vec::new();
        let started = Instant::now();
        apply_accept_outcome(Ok(Accepted::RejectedCapacity), &mut clients);
        apply_accept_outcome(Ok(Accepted::RejectedUid), &mut clients);
        assert!(clients.is_empty(), "a rejection must never add a client");
        assert!(
            started.elapsed() < ACCEPT_ERROR_BACKOFF,
            "a rejection already consumed its fd and must not pay the hard-failure backoff"
        );
    }

    #[test]
    fn apply_accept_outcome_backs_off_on_a_hard_failure_instead_of_spinning() {
        let mut clients = Vec::new();
        let started = Instant::now();
        apply_accept_outcome(
            Err(io::Error::other(
                "synthetic EMFILE for R3-accept-failure-spin",
            )),
            &mut clients,
        );
        assert!(clients.is_empty(), "a hard failure must never add a client");
        assert!(
            started.elapsed() >= ACCEPT_ERROR_BACKOFF,
            "a persistent accept() failure that does not consume the queued connection must be \
             throttled, or the listener stays ready and poll(2) spins at 100% CPU"
        );
    }

    // --- R3-wakeup-causes-unbounded: the attribution record must be bounded and lock-poison
    // safe -------------------------------------------------------------------------------------

    #[test]
    fn wakeup_causes_ring_never_grows_past_its_cap() {
        let causes = Mutex::new(VecDeque::new());
        for _ in 0..(WAKEUP_CAUSES_CAP + 5) {
            record_wakeup_cause(&causes, WakeupCause::default());
        }
        assert_eq!(
            causes.lock().unwrap().len(),
            WAKEUP_CAUSES_CAP,
            "a long-lived daemon must never grow this record past its cap"
        );
    }

    #[test]
    fn record_wakeup_cause_survives_a_poisoned_lock() {
        let causes = Mutex::new(VecDeque::new());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = causes.lock().unwrap();
            panic!("simulate another holder (e.g. a test observer) panicking with the lock held");
        }));
        assert!(causes.is_poisoned());

        // Must not panic: a poisoned lock degrades this record, it does not take the reactor
        // thread down with it.
        record_wakeup_cause(&causes, WakeupCause::default());
        assert_eq!(
            causes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
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

    // --- 15.10: `logind_mut` reaches the exact adapter this reactor was built with ---------

    #[test]
    fn logind_mut_reaches_the_same_source_the_reactor_polls() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, _x11_writer, mut logind_writer, _socket_path) =
            make_reactor_source("logind-mut");

        // Pushed through the accessor, not through the constructor — proving `logind_mut`
        // reaches the SAME instance `next_event` polls, not a detached copy.
        reactor
            .logind_mut()
            .push_event(SourceEvent::PrepareForSleep(true));
        logind_writer
            .write_all(b"x")
            .expect("write to the synthetic logind fd must succeed");

        let event = reactor.next_event(None).expect("next_event must not error");
        assert_eq!(event, Some(SourceEvent::PrepareForSleep(true)));
    }

    // --- 15.11: `mark_resumed` is the automatic-`PauseExpiry` half of the `paused`
    // bookkeeping's lockstep (the manual `resume` half is already covered by
    // `pause_while_paused_and_resume_while_not_paused_return_state_errors` in control.rs) ---

    #[test]
    fn mark_resumed_lets_a_new_pause_request_succeed_instead_of_already_paused() {
        let _signal_guard = crate::signals::SIGNAL_TEST_GUARD.lock().unwrap();
        let (mut reactor, _x11_writer, _logind_writer, socket_path) =
            make_reactor_source("mark-resumed");

        let mut first_client = UnixStream::connect(&socket_path).expect("connect must succeed");
        first_client
            .write_all(b"{\"v\":1,\"cmd\":\"pause\"}\n")
            .expect("write must succeed");
        let first = reactor.next_event(None).expect("next_event must not error");
        assert!(
            matches!(first, Some(SourceEvent::Pause { .. })),
            "the first pause request must succeed: {first:?}"
        );

        // Simulates task 15.11's own automatic-expiry path (`main.rs`'s event loop calling
        // this on `SourceEvent::DeadlineElapsed(Timer::PauseExpiry)`), not a real deadline.
        reactor.mark_resumed();

        let mut second_client = UnixStream::connect(&socket_path).expect("connect must succeed");
        second_client
            .write_all(b"{\"v\":1,\"cmd\":\"pause\"}\n")
            .expect("write must succeed");
        let second = reactor.next_event(None).expect("next_event must not error");
        assert!(
            matches!(second, Some(SourceEvent::Pause { .. })),
            "mark_resumed must clear the paused bookkeeping so a fresh pause request succeeds \
             instead of answering AlreadyPaused: {second:?}"
        );
    }
}
