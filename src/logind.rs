//! `SessionMonitor` trait (D-13), `ZbusSessionMonitor`, `FakeSessionMonitor`, the `xwl-logind`
//! bridge threads and their `eventfd` (D-3). The only file naming a `zbus`/`zvariant` type
//! (T-3 mitigation, task 12.11).
//!
//! **Phase 12 scope (tasks 12.1-12.11, RF-5, RF-26, RF-27, RF-65).** `SessionMonitor` is the
//! in-house trait D-13 specifies; `FakeSessionMonitor` drives every degraded path with no
//! D-Bus at all. `LockedHintTracker` turns raw `LockedHint` samples into the
//! `SourceEvent::SessionLocked`/`SessionUnlocked` edges `tracker.rs` already knows how to
//! apply (RF-5) — a `Lock()` D-Bus signal is not even a variant this type accepts, so it
//! cannot, by construction, produce a transition. `resolve_session_at_startup` and
//! `SuspendInhibitor` cover RF-26/RF-27's retry-and-degrade paths. `connect_bounded` and
//! `init_session_monitor_with` cover RF-65: a bus that never completes its handshake cannot
//! block daemon startup, and total D-Bus absence degrades to a logged warning, not a fatal
//! error. `ZbusSessionMonitor` is the production impl; its two bridge threads
//! (`xwl-logind-sleep`, `xwl-logind-lockedhint`) each install their D-Bus match rule
//! synchronously, on the caller's thread, before spawning — so no signal sent during the
//! spawn window is lost.
//!
//! **Deviation from design §2 D-3's diagram.** D-3 draws one bridge thread. This file uses
//! two (`PrepareForSleep` on the `Manager` interface, `PropertiesChanged` on the resolved
//! session's `Properties` interface): `zbus::blocking`'s signal iterators each block on their
//! own `.next()`, and multiplexing two independent blocking sources on one OS thread needs
//! either `epoll`-over-zbus-internals (not exposed) or hand-written async composition, which
//! is out of scope for a module whose whole point (D-13) is confining `zbus` to this file
//! without growing an async runtime here. Reported as a stated deviation, not a silent one;
//! the thread-count consequence (RNF-1) is one more than D-3's own "3-4" estimate.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nix::sys::eventfd::{EfdFlags, EventFd};

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

/// Whether Phase 14's `SessionReresolve` deadline needs arming after a startup resolution
/// attempt (RF-26, task 12.4/12.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionResolution {
    Resolved,
    /// Carries a ready-to-log diagnostic; resolution failure is never fatal.
    RetryNeeded(String),
}

/// RF-26: resolves via `SessionMonitor::resolve_session` (which is `GetSessionByPID` in the
/// real impl, never `$XDG_SESSION_ID`), and reports whether a retry is owed instead of
/// treating failure as fatal.
pub fn resolve_session_at_startup(monitor: &mut dyn SessionMonitor, pid: u32) -> SessionResolution {
    match monitor.resolve_session(pid) {
        Ok(()) => SessionResolution::Resolved,
        Err(err) => SessionResolution::RetryNeeded(format!(
            "session resolution failed, continuing without lock/suspend awareness, retrying later: {err:?}"
        )),
    }
}

/// Coordinates the RF-27 `delay` sleep inhibitor across `PrepareForSleep` transitions (tasks
/// 12.6/12.7). Sequencing the inhibitor release *after* the interval closure is committed is
/// the reactor's job (Phase 14, not built yet); this type only performs the acquire/release
/// and reports whether the inhibitor is currently held.
#[derive(Debug, Default)]
pub struct SuspendInhibitor {
    held: bool,
}

impl SuspendInhibitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Attempts to take the inhibitor. A refusal (e.g. restrictive polkit) logs — returns a
    /// diagnostic, never panics or blocks — and startup continues regardless (RF-27 scenario
    /// 3).
    pub fn acquire(&mut self, monitor: &mut dyn SessionMonitor) -> Option<String> {
        match monitor.take_sleep_inhibitor() {
            Ok(()) => {
                self.held = true;
                None
            }
            Err(err) => {
                self.held = false;
                Some(format!(
                    "sleep inhibitor refused, continuing best-effort: {err:?}"
                ))
            }
        }
    }

    /// `PrepareForSleep(true)`: the caller has already committed the closing interval; this
    /// releases the descriptor so suspend may proceed.
    pub fn release_for_suspend(&mut self, monitor: &mut dyn SessionMonitor) {
        monitor.release_sleep_inhibitor();
        self.held = false;
    }

    /// `PrepareForSleep(false)`: resume re-acquires the inhibitor.
    pub fn reacquire_after_resume(&mut self, monitor: &mut dyn SessionMonitor) -> Option<String> {
        self.acquire(monitor)
    }
}

/// How long `connect_bounded` waits for a connection attempt before treating the bus as
/// unreachable (RF-65).
const BUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds a D-Bus connect attempt so a bus that accepts the socket and then stalls the SASL
/// handshake cannot block daemon startup forever (RF-65, task 12.8/12.9).
///
/// `method_timeout` cannot do this: zbus 5.19.0 applies it only *after* the handshake
/// completes (`zbus-5.19.0/src/blocking/connection/builder.rs:595,603`), and
/// `zbus::blocking::Connection::system()` bypasses the connection builder entirely (`system()`
/// is `block_on(crate::Connection::system())`,
/// `zbus-5.19.0/src/blocking/connection/mod.rs:42`), so it can never carry one anyway. `connect`
/// therefore runs on a throwaway thread; a stalling bus leaves that thread parked on `connect`
/// forever — an accepted one-thread leak, never a startup hang.
fn connect_bounded(
    connect: Box<dyn FnOnce() -> zbus::Result<zbus::blocking::Connection> + Send>,
    timeout: Duration,
) -> Result<zbus::blocking::Connection, SessionError> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("xwl-logind-connect".to_string())
        .spawn(move || {
            let _ = tx.send(connect());
        })
        .map_err(|e| SessionError(format!("failed to spawn D-Bus connect thread: {e}")))?;

    match rx.recv_timeout(timeout) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(SessionError(e.to_string())),
        Err(_) => Err(SessionError(
            "D-Bus system bus connection attempt timed out".to_string(),
        )),
    }
}

/// RF-65: attempts session monitoring, degrading to a logged warning — never a fatal error —
/// when D-Bus/logind is entirely unreachable. `connect` is a parameter so the degrade path
/// itself is unit-testable without touching any real socket (task 12.9).
fn init_session_monitor_with<F>(connect: F) -> (Option<ZbusSessionMonitor>, Option<String>)
where
    F: FnOnce() -> Result<ZbusSessionMonitor, SessionError>,
{
    match connect() {
        Ok(monitor) => (Some(monitor), None),
        Err(err) => (
            None,
            Some(format!(
                "session-state tracking (lock/suspend) unavailable, D-Bus unreachable: {err:?}"
            )),
        ),
    }
}

/// RF-65's production entry point: window capture, exclusion and storage all start up
/// regardless of what this returns.
pub fn init_session_monitor() -> (Option<ZbusSessionMonitor>, Option<String>) {
    init_session_monitor_with(ZbusSessionMonitor::connect)
}

/// Owned event handed from a bridge thread to the reactor (design §2 D-3). No `zbus`/
/// `zvariant` type crosses this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogindEvent {
    LockedHintChanged(bool),
    PrepareForSleep(bool),
    /// A bridge thread's signal iterator ended (bus restart/drop); no longer silent.
    BridgeDied,
}

/// Bridge-to-reactor event queue (design §2 D-3): bounded, drop-oldest-on-full with a
/// `lagged` flag the reactor turns into a forced `LockedHint` re-read — a logind event storm
/// degrades latency but never blocks capture.
const EVENT_QUEUE_CAPACITY: usize = 32;

#[derive(Debug, Default)]
struct EventQueue {
    inner: Mutex<VecDeque<LogindEvent>>,
    lagged: AtomicBool,
}

impl EventQueue {
    fn push(&self, event: LogindEvent) {
        let mut queue = self.inner.lock().expect("logind event queue lock poisoned");
        if queue.len() >= EVENT_QUEUE_CAPACITY {
            queue.pop_front();
            self.lagged.store(true, Ordering::Relaxed);
        }
        queue.push_back(event);
    }

    fn drain(&self) -> (Vec<LogindEvent>, bool) {
        let mut queue = self.inner.lock().expect("logind event queue lock poisoned");
        let events = queue.drain(..).collect();
        let lagged = self.lagged.swap(false, Ordering::Relaxed);
        (events, lagged)
    }
}

/// Runs `handle` per item; once `iter` ends, pushes `BridgeDied` so exit is observable.
fn run_bridge_loop<T>(
    iter: impl Iterator<Item = T>,
    events: &EventQueue,
    event_fd: &EventFd,
    mut handle: impl FnMut(T, &EventQueue, &EventFd),
) {
    for item in iter {
        handle(item, events, event_fd);
    }
    events.push(LogindEvent::BridgeDied);
    let _ = event_fd.write(1);
}

/// How long a steady-state D-Bus round trip may block before it's treated as a failure.
const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Generalizes `connect_bounded`'s throwaway-thread pattern to any steady-state D-Bus call.
fn call_bounded<T: Send + 'static>(
    f: impl FnOnce() -> zbus::Result<T> + Send + 'static,
    timeout: Duration,
) -> Result<T, SessionError> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("xwl-logind-call".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| SessionError(format!("failed to spawn D-Bus call thread: {e}")))?;

    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(SessionError(e.to_string())),
        Err(_) => Err(SessionError("D-Bus call timed out".to_string())),
    }
}

/// Spawns a named bridge thread (design §2 D-3) with `RNF-1`'s bounded 64 KiB stack. `run`
/// receives the shared queue/eventfd so it can push translated events; the D-Bus match rule
/// this thread drains must already be installed by the caller before this is invoked (see
/// `ZbusSessionMonitor::connect`/`resolve_session`), so nothing sent during the spawn window
/// is lost.
fn spawn_bridge_thread<F>(
    name: &str,
    events: Arc<EventQueue>,
    event_fd: Arc<EventFd>,
    run: F,
) -> Result<thread::JoinHandle<()>, SessionError>
where
    F: FnOnce(Arc<EventQueue>, Arc<EventFd>) + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .stack_size(64 * 1024)
        .spawn(move || run(events, event_fd))
        .map_err(|e| SessionError(format!("failed to spawn {name}: {e}")))
}

/// The production `SessionMonitor` (D-13, task 12.10). The only type in the crate naming a
/// `zbus`/`zvariant` type (task 12.11).
pub struct ZbusSessionMonitor {
    connection: zbus::blocking::Connection,
    manager: zbus::blocking::Proxy<'static>,
    session_path: Option<zbus::zvariant::OwnedObjectPath>,
    inhibit_fd: Option<std::os::fd::OwnedFd>,
    events: Arc<EventQueue>,
    event_fd: Arc<EventFd>,
    // Kept alive for the daemon's lifetime; never joined (the process owns these threads
    // until it exits, matching D-3's transports).
    bridge_threads: Vec<thread::JoinHandle<()>>,
    /// Refuse-second-spawn guard: set once the `lockedhint` bridge thread exists.
    lockedhint_bridge_spawned: bool,
}

impl ZbusSessionMonitor {
    /// Connects to the real system bus, bounded per `connect_bounded`, and starts the
    /// `PrepareForSleep` bridge (RF-27's suspend half). `resolve_session` starts the second
    /// bridge (`LockedHint`) once a session path is known.
    pub fn connect() -> Result<Self, SessionError> {
        let connection = connect_bounded(
            Box::new(zbus::blocking::Connection::system),
            BUS_CONNECT_TIMEOUT,
        )?;

        let manager = zbus::blocking::Proxy::new_owned(
            connection.clone(),
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .map_err(|e| SessionError(e.to_string()))?;

        let events = Arc::new(EventQueue::default());
        let event_fd = Arc::new(
            EventFd::from_flags(EfdFlags::EFD_NONBLOCK)
                .map_err(|e| SessionError(format!("eventfd() failed: {e}")))?,
        );

        // Match rule installed synchronously here (`SignalStream::new` awaits
        // `MessageStream::for_match_rule`, zbus-5.19.0 src/proxy/mod.rs) before the bridge
        // thread spawns below.
        let sleep_signals = manager
            .receive_signal("PrepareForSleep")
            .map_err(|e| SessionError(e.to_string()))?;

        let sleep_thread = spawn_bridge_thread(
            "xwl-logind-sleep",
            events.clone(),
            event_fd.clone(),
            move |events, event_fd| {
                run_bridge_loop(
                    sleep_signals,
                    &events,
                    &event_fd,
                    |msg, events, event_fd| {
                        if let Ok(going_to_sleep) = msg.body().deserialize::<bool>() {
                            events.push(LogindEvent::PrepareForSleep(going_to_sleep));
                            let _ = event_fd.write(1);
                        }
                    },
                );
            },
        )?;

        Ok(ZbusSessionMonitor {
            connection,
            manager,
            session_path: None,
            inhibit_fd: None,
            events,
            event_fd,
            bridge_threads: vec![sleep_thread],
            lockedhint_bridge_spawned: false,
        })
    }

    /// The fd Phase 14's reactor adds to its permanent `poll(2)` set.
    pub fn event_fd(&self) -> RawFd {
        self.event_fd.as_raw_fd()
    }

    /// Drains every translated event queued since the last drain, plus whether the queue
    /// overflowed (design §2 D-3's `lagged`).
    pub fn drain_events(&self) -> (Vec<LogindEvent>, bool) {
        self.events.drain()
    }

    #[cfg(test)]
    pub(crate) fn bridge_thread_count(&self) -> usize {
        self.bridge_threads.len()
    }
}

impl SessionMonitor for ZbusSessionMonitor {
    fn locked_hint(&self) -> Result<bool, SessionError> {
        let path = self
            .session_path
            .as_ref()
            .ok_or_else(|| SessionError("session not resolved yet".to_string()))?;

        let session = zbus::blocking::Proxy::new(
            &self.connection,
            "org.freedesktop.login1",
            path.clone(),
            "org.freedesktop.login1.Session",
        )
        .map_err(|e| SessionError(e.to_string()))?;

        call_bounded(
            move || session.get_property::<bool>("LockedHint"),
            DBUS_CALL_TIMEOUT,
        )
    }

    fn take_sleep_inhibitor(&mut self) -> Result<(), SessionError> {
        let manager = self.manager.clone();
        let fd: zbus::zvariant::OwnedFd = call_bounded(
            move || {
                manager.call(
                    "Inhibit",
                    &("sleep", "xwindowlog", "Recording session activity", "delay"),
                )
            },
            DBUS_CALL_TIMEOUT,
        )?;
        self.inhibit_fd = Some(fd.into());
        Ok(())
    }

    fn release_sleep_inhibitor(&mut self) {
        self.inhibit_fd = None;
    }

    fn resolve_session(&mut self, pid: u32) -> Result<(), SessionError> {
        let manager = self.manager.clone();
        let path: zbus::zvariant::OwnedObjectPath = call_bounded(
            move || manager.call("GetSessionByPID", &(pid,)),
            DBUS_CALL_TIMEOUT,
        )?;

        // Refuse-second-spawn: RF-26 re-resolution updates `session_path` only, past the first.
        if self.lockedhint_bridge_spawned {
            self.session_path = Some(path);
            return Ok(());
        }

        let properties = zbus::blocking::Proxy::new(
            &self.connection,
            "org.freedesktop.login1",
            path.clone(),
            "org.freedesktop.DBus.Properties",
        )
        .map_err(|e| SessionError(e.to_string()))?;

        // Same synchronous-install guarantee as the `PrepareForSleep` watch above, filtered
        // server-side to this session's own `PropertiesChanged` (arg 0 is the interface name).
        let locked_hint_signals = properties
            .receive_signal_with_args(
                "PropertiesChanged",
                &[(0, "org.freedesktop.login1.Session")],
            )
            .map_err(|e| SessionError(e.to_string()))?;

        let thread = spawn_bridge_thread(
            "xwl-logind-lockedhint",
            self.events.clone(),
            self.event_fd.clone(),
            move |events, event_fd| {
                run_bridge_loop(
                    locked_hint_signals,
                    &events,
                    &event_fd,
                    |msg, events, event_fd| {
                        let Ok((_iface, changed, _invalidated)) = msg.body().deserialize::<(
                            String,
                            HashMap<String, zbus::zvariant::OwnedValue>,
                            Vec<String>,
                        )>(
                        ) else {
                            return;
                        };
                        let Some(value) = changed.get("LockedHint") else {
                            return;
                        };
                        if let Ok(locked) = bool::try_from(value.clone()) {
                            events.push(LogindEvent::LockedHintChanged(locked));
                            let _ = event_fd.write(1);
                        }
                    },
                );
            },
        )?;
        self.bridge_threads.push(thread);
        self.lockedhint_bridge_spawned = true;

        self.session_path = Some(path);
        Ok(())
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

    // --- 12.4/12.5: session resolution + retry signal (RF-26) --------------------------

    #[test]
    fn resolve_session_at_startup_succeeds_via_the_pid_path() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_resolve_session(Ok(()));

        let result = resolve_session_at_startup(&mut fake, 999);

        assert_eq!(result, SessionResolution::Resolved);
        assert_eq!(fake.resolved_pids, vec![999]);
    }

    #[test]
    fn resolve_session_at_startup_failure_requests_a_retry_without_failing() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_resolve_session(Err(SessionError("no session yet".to_string())));

        let result = resolve_session_at_startup(&mut fake, 999);

        match result {
            SessionResolution::RetryNeeded(diagnostic) => {
                assert!(diagnostic.contains("retrying later"));
            }
            SessionResolution::Resolved => panic!("expected RetryNeeded, got Resolved"),
        }
    }

    // --- 12.6/12.7: suspend inhibitor (RF-27, all three scenarios) ----------------------

    #[test]
    fn suspend_inhibitor_close_then_release_on_prepare_for_sleep_true() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_take_inhibitor(Ok(()));
        let mut inhibitor = SuspendInhibitor::new();
        assert_eq!(inhibitor.acquire(&mut fake), None);
        assert!(inhibitor.is_held());

        inhibitor.release_for_suspend(&mut fake);

        assert!(!inhibitor.is_held());
        assert_eq!(fake.release_calls, 1);
    }

    #[test]
    fn suspend_inhibitor_reacquires_on_resume() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_take_inhibitor(Ok(()));
        fake.push_take_inhibitor(Ok(()));
        let mut inhibitor = SuspendInhibitor::new();
        inhibitor.acquire(&mut fake);
        inhibitor.release_for_suspend(&mut fake);

        let diagnostic = inhibitor.reacquire_after_resume(&mut fake);

        assert_eq!(diagnostic, None);
        assert!(inhibitor.is_held());
    }

    #[test]
    fn suspend_inhibitor_refused_warns_and_continues() {
        let mut fake = FakeSessionMonitor::new();
        fake.push_take_inhibitor(Err(SessionError("polkit refused".to_string())));
        let mut inhibitor = SuspendInhibitor::new();

        let diagnostic = inhibitor.acquire(&mut fake);

        assert!(!inhibitor.is_held());
        let message = diagnostic.expect("refusal must produce a diagnostic");
        assert!(message.contains("refused"));
    }

    // --- 12.8/12.9: D-Bus unreachable / stalling at startup (RF-65) --------------------

    #[test]
    fn connect_bounded_times_out_against_a_bus_that_accepts_and_then_stalls() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

        // Abstract socket: no filesystem cleanup, and unique enough not to collide with a
        // parallel test run.
        let name = format!(
            "xwindowlog-test-stalling-bus-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let bind_addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&bind_addr).unwrap();

        let _accept_thread = thread::spawn(move || {
            // Accept and never write a single byte back: the client's SASL handshake read
            // blocks forever on this end. Kept alive for the test binary's lifetime.
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(Duration::from_secs(60));
            }
        });

        let connect_addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let result = connect_bounded(
            Box::new(move || {
                let stream = UnixStream::connect_addr(&connect_addr)?;
                zbus::blocking::connection::Builder::async_io_unix_stream(stream).build()
            }),
            Duration::from_millis(300),
        );

        match result {
            Err(SessionError(message)) => {
                assert_eq!(message, "D-Bus system bus connection attempt timed out")
            }
            Ok(_) => panic!("expected a timeout, got a connection to the stalling socket"),
        }
    }

    #[test]
    fn init_session_monitor_degrades_to_a_warning_when_dbus_is_unreachable() {
        let (monitor, diagnostic) =
            init_session_monitor_with(|| Err(SessionError("no bus reachable".to_string())));

        assert!(monitor.is_none());
        let message = diagnostic.expect("unreachable D-Bus must produce a diagnostic");
        assert!(message.contains("unavailable"));
    }

    // --- Real-bus acceptance (task 12.10), gated per hard-won lesson #8 ----------------

    fn require_real_bus() -> bool {
        std::env::var("XWINDOWLOG_REQUIRE_REAL_BUS").as_deref() == Ok("1")
    }

    #[test]
    fn real_bus_resolves_own_session_and_reads_locked_hint() {
        if !require_real_bus() {
            return;
        }

        let mut monitor = ZbusSessionMonitor::connect().expect("system bus must be reachable");
        monitor
            .resolve_session(std::process::id())
            .expect("GetSessionByPID(own pid) must succeed on a real logind session");

        let locked = monitor
            .locked_hint()
            .expect("LockedHint must be readable once resolved");
        assert!(!locked, "environment precondition: session starts unlocked");
    }

    #[test]
    fn real_bus_inhibit_acquire_and_release_round_trip() {
        if !require_real_bus() {
            return;
        }

        let mut monitor = ZbusSessionMonitor::connect().expect("system bus must be reachable");
        monitor
            .take_sleep_inhibitor()
            .expect("Inhibit() must succeed for an unprivileged session in this environment");

        monitor.release_sleep_inhibitor();
        // No further assertion: releasing drops the `OwnedFd`, which closes it on `Drop`.
        // Definition of done requires no leaked fd across the whole suite, checked externally.
    }

    #[test]
    fn real_bus_resolve_session_twice_spawns_only_one_lockedhint_bridge() {
        if !require_real_bus() {
            return;
        }

        let mut monitor = ZbusSessionMonitor::connect().expect("system bus must be reachable");
        monitor
            .resolve_session(std::process::id())
            .expect("first resolve must succeed");
        let after_first = monitor.bridge_thread_count();

        monitor
            .resolve_session(std::process::id())
            .expect("second resolve must succeed");

        assert_eq!(
            monitor.bridge_thread_count(),
            after_first,
            "a repeat resolve_session call must not spawn another bridge thread"
        );
    }

    #[test]
    fn bridge_loop_end_pushes_bridge_died_so_it_is_visible_to_the_reactor() {
        let events = EventQueue::default();
        let event_fd = EventFd::from_flags(EfdFlags::EFD_NONBLOCK).unwrap();

        run_bridge_loop(std::iter::empty::<()>(), &events, &event_fd, |_, _, _| {});

        let (drained, _lagged) = events.drain();
        assert_eq!(drained, vec![LogindEvent::BridgeDied]);
    }

    #[test]
    fn bounded_call_times_out_against_a_call_that_never_returns() {
        let result: Result<(), SessionError> = call_bounded(
            || {
                thread::sleep(Duration::from_secs(2));
                Ok(())
            },
            Duration::from_millis(50),
        );

        let SessionError(message) = result.expect_err("expected the call to time out");
        assert_eq!(message, "D-Bus call timed out");
    }
}
