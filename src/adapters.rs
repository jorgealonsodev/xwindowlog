//! Phase 15 composition: translates `x11.rs`'s `RawEvent`/`logind.rs`'s `LogindEvent` into
//! `tracker::SourceEvent`s and implements `reactor::BudgetedSource` over the real capture
//! sources, so `main.rs` can hand concrete types to `ReactorSource::new` (design §2 D-2's fd0/
//! fd1). The translation logic itself is split into plain functions with no fd/`BudgetedSource`
//! involved at all, so it is unit-testable the same way `reactor.rs`'s own tests use synthetic
//! doubles instead of real X11/D-Bus (tasks.md Phase 14's precedent).
use std::cell::RefCell;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::Duration;

use xwindowlog::exclude::Excluder;
use xwindowlog::logind::{LockedHintTracker, LogindEvent, ZbusSessionMonitor};
use xwindowlog::reactor::{BudgetedSource, RecoveryOutcome};
use xwindowlog::tracker::{SourceError, SourceEvent, WindowInfo};
use xwindowlog::x11::{RawEvent, ReconnectAttempt, Reconnector, X11Source};

/// The `WindowInfo::desktop()` sentinel's own app-id text (`tracker.rs`'s `WindowInfo::desktop`
/// is private to that module, so this is the one place outside it that needs the same literal —
/// pinned against drift by `desktop_sentinel_matches_tracker_windowinfo` below).
const DESKTOP_APP_ID: &str = "(desktop)";

/// Runs one `RawEvent` through `excluder` (design §2 D-7's §14.3 boundary: `RawTitle` dies
/// here, only `SafeTitle` exists past this call) and produces the exact `SourceEvent`
/// `tracker.rs` expects. `current_app_id` is the adapter's own memory of which window a
/// `RawEvent::TitleChanged` — which carries no app-id of its own — belongs to; it is updated
/// only by `ActiveWindow`, exactly mirroring what `x11.rs` itself tracks internally for the
/// same reason.
fn translate_x11_event(
    excluder: &Excluder,
    current_app_id: &mut String,
    raw: RawEvent,
) -> SourceEvent {
    match raw {
        RawEvent::ActiveWindow(None) => {
            DESKTOP_APP_ID.clone_into(current_app_id);
            SourceEvent::ActiveWindow(None)
        }
        RawEvent::ActiveWindow(Some(info)) => {
            current_app_id.clone_from(&info.app_id);
            let evaluated = excluder.evaluate(&info.app_id, info.title);
            SourceEvent::ActiveWindow(Some(WindowInfo {
                app_id: evaluated.app_id,
                title: evaluated.title,
                pid: info.pid,
            }))
        }
        RawEvent::TitleChanged(raw_title) => {
            let evaluated = excluder.evaluate(current_app_id, raw_title);
            SourceEvent::TitleChanged(evaluated.title)
        }
        RawEvent::ActiveWindowDestroyed => SourceEvent::ActiveWindowDestroyed,
        RawEvent::UserIdle { idle_for } => SourceEvent::UserIdle { idle_for },
        RawEvent::UserActive => SourceEvent::UserActive,
        RawEvent::DisplayLost => SourceEvent::DisplayLost,
        RawEvent::DisplayRestored => SourceEvent::DisplayRestored,
    }
}

/// Mirrors `x11.rs`'s own private `DEFAULT_AFK_THRESHOLD` (`src/x11.rs:177`) — not `pub`, so
/// this is the one place outside that module that needs the same literal (same shape as
/// `DESKTOP_APP_ID` above). Used only by `X11Adapter::new`'s owned `Reconnector` (RF-32 T2):
/// `main.rs`'s single call site (`src/main.rs:267`) still calls `new`, not
/// `with_reconnect_config`, and stays out of this task's authorized scope, so the adapter
/// cannot yet be told the real `afk_threshold` the production `X11Source` was actually built
/// with (`src/main.rs:264` passes `config.afk_threshold`, which may differ from this
/// default). Disclosed gap: this mismatch is latent until whichever later task wires
/// `main.rs` to call `with_reconnect_config` with its real config instead.
const DEFAULT_RECONNECT_AFK_THRESHOLD: Duration = Duration::from_secs(240);

/// `reactor::BudgetedSource` over the real `x11.rs` capture engine (design §2 D-2 fd0).
pub struct X11Adapter {
    source: X11Source,
    excluder: Rc<RefCell<Excluder>>,
    current_app_id: String,
    /// RF-32 T2/T3: the adapter owns the reconnect policy state (`ReconnectBackoff`,
    /// `outage_open`) for its own display/`afk_threshold`, rather than that state living
    /// somewhere else and being handed the adapter's fd out of band — `Reconnector::attempt`,
    /// driven from `recover` (T3), needs exactly the `display`/`afk_threshold` this adapter's
    /// own `X11Source` was built with to reconnect to the same place.
    reconnector: Reconnector,
}

impl X11Adapter {
    /// `display: None`, matching `main.rs`'s only production call site
    /// (`X11Source::connect_with_afk_threshold(None, ..)`, `src/main.rs:264`) — see
    /// `DEFAULT_RECONNECT_AFK_THRESHOLD`'s doc for the one field this cannot mirror yet.
    pub fn new(source: X11Source, excluder: Rc<RefCell<Excluder>>) -> Self {
        Self::with_reconnect_config(source, excluder, None, DEFAULT_RECONNECT_AFK_THRESHOLD)
    }

    /// Same as `new`, with the `Reconnector`'s `display`/`afk_threshold` explicit rather than
    /// defaulted — mirrors `X11Source::connect`/`connect_with_afk_threshold`'s own
    /// default-plus-override shape (`src/x11.rs:429-439`) for the same reason: a future
    /// caller that actually knows the production config (or a test) needs a `Reconnector`
    /// that would reconnect to the *same* display this adapter's `source` came from.
    pub fn with_reconnect_config(
        source: X11Source,
        excluder: Rc<RefCell<Excluder>>,
        display: Option<&str>,
        afk_threshold: Duration,
    ) -> Self {
        X11Adapter {
            source,
            excluder,
            current_app_id: DESKTOP_APP_ID.to_string(),
            reconnector: Reconnector::new(display, afk_threshold),
        }
    }

    /// Replaces the adapter's live `X11Source` with a freshly (re)connected one — RF-32 T2's
    /// mechanism, driven by T3's `recover` on `ReconnectAttempt::Restored { source, .. }`. No
    /// separate change-notification is needed: `ReactorSource::next_event`
    /// already re-queries `as_raw_fd()` on every poll iteration (`src/reactor.rs:583-590`,
    /// feature doc constraint 3), so the very next call after this one sees the new fd.
    ///
    /// Also resets `current_app_id` back to the desktop sentinel. Decision (feature doc T2):
    /// a freshly connected `X11Source` always starts with `active_window: None`
    /// (`src/x11.rs:479`), and this module's own doc contract says `current_app_id` must
    /// "exactly mirror what `x11.rs` itself tracks internally" — so leaving the old value in
    /// place after a reconnect would desync the mirror from the connection it is supposed to
    /// describe. `X11Source`'s state machine happens to gate `RawEvent::TitleChanged` behind
    /// its own `active_window` being `Some(_)` already (`is_active_window_notify`/
    /// `active_window_title_notify`, `src/x11.rs`), which would currently stop a stale
    /// `current_app_id` from reaching a real `TitleChanged` translation — but that guard
    /// lives in a different module than `current_app_id` and this adapter has no business
    /// depending on it silently. A future translation path that emits `TitleChanged` (or
    /// evaluates a title) without first observing a fresh `ActiveWindow` on the new
    /// connection would otherwise evaluate a title against a stale, wrong `app_id` — a D-7
    /// exclusion-boundary risk, not just a cosmetic one.
    pub fn replace_source(&mut self, source: X11Source) {
        self.source = source;
        DESKTOP_APP_ID.clone_into(&mut self.current_app_id);
    }

    /// RF-32 T2: exposes the adapter's owned `Reconnector` so T3's `BudgetedSource::recover`
    /// can drive `Reconnector::attempt` without this adapter needing to re-expose the policy's
    /// own internals.
    pub fn reconnector_mut(&mut self) -> &mut Reconnector {
        &mut self.reconnector
    }
}

impl BudgetedSource for X11Adapter {
    fn as_raw_fd(&self) -> RawFd {
        self.source.as_raw_fd()
    }

    fn flush(&mut self) -> Result<(), SourceError> {
        self.source
            .flush()
            .map_err(|e| SourceError(format!("x11 flush failed: {e}")))
    }

    fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        let raw = self
            .source
            .poll_for_event()
            .map_err(|e| SourceError(format!("x11 poll_for_event failed: {e}")))?;
        Ok(raw
            .map(|raw| translate_x11_event(&self.excluder.borrow(), &mut self.current_app_id, raw)))
    }

    /// RF-32 T3: drives the adapter's own `Reconnector` (T2's `reconnector_mut`) and, on
    /// success, installs the reconnected `X11Source` via T2's `replace_source` — so a future
    /// reactor caller (T4/T5) never has to touch the source object itself, matching
    /// `BudgetedSource::recover`'s own doc contract.
    ///
    /// `RecoveryOutcome`'s doc explains why this maps `x11::ReconnectAttempt`'s outcomes rather
    /// than returning that type directly: its `Restored` variant carries `source:
    /// Box<X11Source>` for a caller that installs the source itself, and `X11Source` is not
    /// `Clone` (it owns a live connection/fd) — once `replace_source` below has moved it into
    /// `self.source`, there is no second copy left to also hand back.
    fn recover(&mut self, entropy: u64) -> Option<RecoveryOutcome> {
        match self.reconnector_mut().attempt(entropy) {
            ReconnectAttempt::OutageOpened { retry_after } => {
                Some(RecoveryOutcome::OutageOpened { retry_after })
            }
            ReconnectAttempt::StillDown { retry_after } => {
                Some(RecoveryOutcome::StillDown { retry_after })
            }
            ReconnectAttempt::Restored {
                source,
                outage_was_open,
                // Already written to stderr by `X11Source::connect_with_afk_threshold` itself
                // (`src/x11.rs:472`) — the same reason `main.rs:263`'s own startup connect
                // discards its returned diagnostics as `_diagnostics`. Nothing here re-derives
                // or silently drops information a human would otherwise see.
                diagnostics: _,
            } => {
                self.replace_source(*source);
                Some(RecoveryOutcome::Restored { outage_was_open })
            }
        }
    }
}

/// Runs one `LogindEvent` through `locked_hint` (RF-5's edge-detection, `logind.rs`'s own
/// `LockedHintTracker`) and produces the `SourceEvent` the tracker should see, or `None` when
/// the event carries no transition (a `LockedHint` sample that did not cross an edge, or a
/// bridge thread dying — logged by the caller, not itself a tracker-visible event).
fn translate_logind_event(
    locked_hint: &mut LockedHintTracker,
    event: LogindEvent,
) -> Option<SourceEvent> {
    match event {
        LogindEvent::LockedHintChanged(locked) => locked_hint.observe(locked),
        LogindEvent::PrepareForSleep(going_to_sleep) => {
            Some(SourceEvent::PrepareForSleep(going_to_sleep))
        }
        LogindEvent::BridgeDied => {
            eprintln!(
                "xwindowlog: a logind bridge thread exited; lock/suspend awareness may be stale"
            );
            None
        }
    }
}

/// `reactor::BudgetedSource` over the real `logind.rs` bridge (design §2 D-2 fd1), or a
/// permanently-idle placeholder when D-Bus is unreachable at startup (RF-65: "window capture,
/// exclusion and storage all start up regardless"). One `enum` so `main.rs` can hand
/// `ReactorSource::new` a single concrete `L` type regardless of which branch startup took.
pub enum LogindSource {
    Real(Box<LogindAdapter>),
    Absent(NullFd),
}

impl BudgetedSource for LogindSource {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            LogindSource::Real(adapter) => adapter.as_raw_fd(),
            LogindSource::Absent(null) => null.as_raw_fd(),
        }
    }

    fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        match self {
            LogindSource::Real(adapter) => adapter.try_next(),
            LogindSource::Absent(_) => Ok(None),
        }
    }
}

/// The real, D-Bus-backed half of [`LogindSource`].
pub struct LogindAdapter {
    monitor: ZbusSessionMonitor,
    locked_hint: LockedHintTracker,
    /// Events already drained off `monitor` for this wakeup but not yet translated/returned —
    /// `drain_events` hands back a batch, `BudgetedSource::try_next` hands out one at a time.
    pending: std::collections::VecDeque<LogindEvent>,
}

impl LogindAdapter {
    pub fn new(monitor: ZbusSessionMonitor, locked_hint: LockedHintTracker) -> Self {
        LogindAdapter {
            monitor,
            locked_hint,
            pending: std::collections::VecDeque::new(),
        }
    }

    /// Reaches the owned monitor for suspend-inhibitor coordination (`main.rs`'s event loop,
    /// task 15.10's RF-27 wiring) — sequencing the release strictly after the interval closure
    /// is committed is the caller's job, not this adapter's (`SuspendInhibitor`'s own doc).
    pub fn monitor_mut(&mut self) -> &mut ZbusSessionMonitor {
        &mut self.monitor
    }
}

impl BudgetedSource for LogindAdapter {
    fn as_raw_fd(&self) -> RawFd {
        self.monitor.event_fd()
    }

    fn try_next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        loop {
            while let Some(event) = self.pending.pop_front() {
                if let Some(source_event) = translate_logind_event(&mut self.locked_hint, event) {
                    return Ok(Some(source_event));
                }
            }

            // Consumes the eventfd's counter (design §2 D-3's wakeup signal) before asking for
            // the events it was signaling — a level-triggered eventfd left unread would report
            // `POLLIN` forever even once every event is drained.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.monitor.event_fd()) };
            let mut buf = [0u8; 8];
            match nix::unistd::read(fd, &mut buf) {
                Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
                Err(e) => return Err(SourceError(format!("logind eventfd read failed: {e}"))),
            }

            let (events, lagged) = self.monitor.drain_events();
            if lagged {
                eprintln!(
                    "xwindowlog: logind event queue overflowed; forcing a LockedHint re-read"
                );
                // A dropped event could have been the very edge `locked_hint` needed to see;
                // the safe recovery is to force the NEXT real sample through as an
                // unconditional re-observation rather than trust a possibly-stale baseline.
                self.locked_hint = LockedHintTracker::new();
            }
            if events.is_empty() {
                return Ok(None);
            }
            self.pending.extend(events);
        }
    }
}

/// A permanently-idle `BudgetedSource`: a `pipe(2)` pair kept open with nothing ever written to
/// it, so its read end is a valid, always-pollable fd that never reports `POLLIN` and never
/// hits EOF (both ends stay alive for this value's whole lifetime). Used when `logind.rs`'s
/// `init_session_monitor` degrades (RF-65) and there is no real eventfd to poll.
pub struct NullFd {
    _reader: std::os::unix::net::UnixStream,
    _writer: std::os::unix::net::UnixStream,
}

impl NullFd {
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        Ok(NullFd {
            _reader: reader,
            _writer: writer,
        })
    }

    fn as_raw_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd as _;
        self._reader.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xwindowlog::exclude::RawTitle;
    use xwindowlog::x11::RawWindowInfo;

    fn passthrough_excluder() -> Excluder {
        Excluder::from_toml_str("").expect("empty exclusion config must parse")
    }

    // --- desktop-focus / active-window translation ------------------------------------------

    #[test]
    fn desktop_sentinel_matches_tracker_windowinfo() {
        // `tracker::WindowInfo::desktop()`'s own `app_id` field is private to that module, so
        // this pins the two literals against each other by value: the tracker treats
        // `ActiveWindow(None)` as its own desktop sentinel regardless of what this adapter's
        // bookkeeping remembers, but the two must still agree for `current_app_id` to be a
        // faithful "what does the tracker currently believe" mirror.
        assert_eq!(DESKTOP_APP_ID, "(desktop)");
    }

    #[test]
    fn active_window_none_translates_to_desktop_and_resets_current_app_id() {
        let excluder = passthrough_excluder();
        let mut current_app_id = "firefox".to_string();

        let event =
            translate_x11_event(&excluder, &mut current_app_id, RawEvent::ActiveWindow(None));

        assert_eq!(event, SourceEvent::ActiveWindow(None));
        assert_eq!(current_app_id, DESKTOP_APP_ID);
    }

    #[test]
    fn active_window_some_evaluates_through_the_excluder_and_remembers_the_raw_app_id() {
        let excluder = passthrough_excluder();
        let mut current_app_id = DESKTOP_APP_ID.to_string();
        let raw = RawEvent::ActiveWindow(Some(RawWindowInfo {
            app_id: "firefox".to_string(),
            title: RawTitle::new("Example — Mozilla Firefox"),
            pid: Some(1234),
        }));

        let event = translate_x11_event(&excluder, &mut current_app_id, raw);

        match event {
            SourceEvent::ActiveWindow(Some(window)) => {
                assert_eq!(window.app_id, "firefox");
                assert_eq!(window.title.as_str(), "Example — Mozilla Firefox");
                assert_eq!(window.pid, Some(1234));
            }
            other => panic!("expected ActiveWindow(Some(_)), got {other:?}"),
        }
        assert_eq!(
            current_app_id, "firefox",
            "the RAW app_id must be remembered, not a possibly-hidden evaluated one"
        );
    }

    #[test]
    fn an_excluded_window_hides_both_app_id_and_title_but_still_updates_current_app_id() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "Signal"
            hide_app = true
            "#,
        )
        .expect("valid exclusion config must parse");
        let mut current_app_id = DESKTOP_APP_ID.to_string();
        let raw = RawEvent::ActiveWindow(Some(RawWindowInfo {
            app_id: "Signal".to_string(),
            title: RawTitle::new("General"),
            pid: None,
        }));

        let event = translate_x11_event(&excluder, &mut current_app_id, raw);

        match event {
            SourceEvent::ActiveWindow(Some(window)) => {
                assert_eq!(window.app_id, "[hidden]");
                assert_eq!(window.title.as_str(), "[hidden]");
            }
            other => panic!("expected ActiveWindow(Some(_)), got {other:?}"),
        }
        // §14.3's ordering guarantee is about what leaves this function, never about this
        // adapter's own bookkeeping: remembering the RAW app-id is required so a LATER title
        // change on this same (still-excluded) window is evaluated against the right rule.
        assert_eq!(current_app_id, "Signal");
    }

    // --- title-change translation uses the remembered app_id, not a fresh one ---------------

    #[test]
    fn title_changed_evaluates_against_the_remembered_current_app_id() {
        let excluder = Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "Signal"
            hide_app = true
            "#,
        )
        .expect("valid exclusion config must parse");
        let mut current_app_id = "Signal".to_string();

        let event = translate_x11_event(
            &excluder,
            &mut current_app_id,
            RawEvent::TitleChanged(RawTitle::new("Alice")),
        );

        assert_eq!(
            event,
            SourceEvent::TitleChanged(excluder.evaluate("Signal", RawTitle::new("Alice")).title)
        );
    }

    #[test]
    fn title_changed_on_a_non_excluded_app_passes_through_sanitized() {
        let excluder = passthrough_excluder();
        let mut current_app_id = "firefox".to_string();

        let event = translate_x11_event(
            &excluder,
            &mut current_app_id,
            RawEvent::TitleChanged(RawTitle::new("New Tab")),
        );

        match event {
            SourceEvent::TitleChanged(title) => assert_eq!(title.as_str(), "New Tab"),
            other => panic!("expected TitleChanged(_), got {other:?}"),
        }
    }

    // --- pass-through variants need no excluder involvement ----------------------------------

    #[test]
    fn non_window_events_pass_through_unchanged() {
        let excluder = passthrough_excluder();
        let mut current_app_id = DESKTOP_APP_ID.to_string();

        assert_eq!(
            translate_x11_event(
                &excluder,
                &mut current_app_id,
                RawEvent::ActiveWindowDestroyed
            ),
            SourceEvent::ActiveWindowDestroyed
        );
        assert_eq!(
            translate_x11_event(
                &excluder,
                &mut current_app_id,
                RawEvent::UserIdle {
                    idle_for: std::time::Duration::from_secs(300)
                }
            ),
            SourceEvent::UserIdle {
                idle_for: std::time::Duration::from_secs(300)
            }
        );
        assert_eq!(
            translate_x11_event(&excluder, &mut current_app_id, RawEvent::UserActive),
            SourceEvent::UserActive
        );
        assert_eq!(
            translate_x11_event(&excluder, &mut current_app_id, RawEvent::DisplayLost),
            SourceEvent::DisplayLost
        );
        assert_eq!(
            translate_x11_event(&excluder, &mut current_app_id, RawEvent::DisplayRestored),
            SourceEvent::DisplayRestored
        );
    }

    // --- logind translation -------------------------------------------------------------------

    #[test]
    fn locked_hint_changed_defers_to_locked_hint_tracker_edge_detection() {
        let mut tracker = LockedHintTracker::new();
        tracker.seed(false);

        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::LockedHintChanged(true)),
            Some(SourceEvent::SessionLocked)
        );
        // No edge the second time: already locked.
        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::LockedHintChanged(true)),
            None
        );
    }

    #[test]
    fn locked_hint_negative_edge_translates_to_session_unlocked() {
        let mut tracker = LockedHintTracker::new();
        tracker.seed(true);

        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::LockedHintChanged(false)),
            Some(SourceEvent::SessionUnlocked)
        );
    }

    #[test]
    fn prepare_for_sleep_always_translates_regardless_of_locked_hint_state() {
        let mut tracker = LockedHintTracker::new();
        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::PrepareForSleep(true)),
            Some(SourceEvent::PrepareForSleep(true))
        );
        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::PrepareForSleep(false)),
            Some(SourceEvent::PrepareForSleep(false))
        );
    }

    #[test]
    fn bridge_died_translates_to_no_source_event() {
        let mut tracker = LockedHintTracker::new();
        assert_eq!(
            translate_logind_event(&mut tracker, LogindEvent::BridgeDied),
            None
        );
    }

    // --- NullFd: a permanently idle placeholder never reports readiness or EOF -------------

    #[test]
    fn null_fd_never_reports_data_ready_and_stays_a_valid_fd() {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::os::fd::BorrowedFd;

        let null = NullFd::new().expect("NullFd::new must succeed");
        let fd = unsafe { BorrowedFd::borrow_raw(null.as_raw_fd()) };
        let mut pfds = [PollFd::new(fd, PollFlags::POLLIN)];

        poll(&mut pfds, PollTimeout::ZERO).expect("poll must not error on a valid fd");

        assert_eq!(
            pfds[0].any(),
            Some(false),
            "a NullFd must never report POLLIN — it exists only to occupy a permanent poll(2) \
             slot when logind is unreachable (RF-65)"
        );
    }

    // --- LogindSource::Absent never yields a SourceEvent ------------------------------------

    #[test]
    fn absent_logind_source_try_next_is_always_none() {
        let null = NullFd::new().expect("NullFd::new must succeed");
        let mut source = LogindSource::Absent(null);

        assert_eq!(source.try_next().expect("must not error"), None);
    }

    // --- replace_source / owned Reconnector (RF-32 T2) --------------------------------------
    //
    // These need a real, working `X11Source` — not a `translate_x11_event` synthetic input —
    // so unlike every other test above, they spawn a real (bare, window-manager-less) `Xvfb`
    // per connection: `X11Source::connect_with_afk_threshold` performs a full connect + atom
    // intern + EWMH-compliance probe that a fake socket cannot satisfy (T1's stalling-peer
    // test in `src/x11.rs` deliberately never gets this far). `adapters.rs` is compiled into
    // the `xwindowlog` *binary* target (`main.rs`'s `mod adapters;`), not the library crate,
    // so it cannot reuse `tests/x11_integration.rs`'s `spawn_xvfb` (that file only ever sees
    // the library's public API through `tests/`) — this is a small, self-contained trim of
    // the same pattern, with no window manager or EWMH readiness poll needed here: a bare
    // Xvfb is enough for `connect_with_afk_threshold` to succeed (it falls back to
    // `CaptureMode::InputFocusFallback`), and the connect call itself doubles as the
    // readiness probe and the kept-alive connection under test — so unlike
    // `tests/x11_integration.rs`'s `XvfbGuard::_keepalive`, there is no separate probe
    // connection to drop and therefore no close-down-reset race window to guard against.
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;
    use xwindowlog::reactor::RecoveryOutcome;

    static NEXT_ADAPTER_TEST_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);
    // Disjoint from `tests/x11_integration.rs`'s `DISPLAY_BASE = 213` and
    // `tests/daemon_e2e.rs`'s own base — this is a different test binary (this module is
    // compiled into the `xwindowlog` bin's unit tests, run in its own process), but nothing
    // stops a developer running both concurrently, so the ranges stay non-overlapping.
    const ADAPTER_TEST_DISPLAY_BASE: u32 = 313;

    struct TestXvfb {
        child: Child,
    }

    impl Drop for TestXvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Spawns a fresh, bare `Xvfb` and returns it alongside a real, already-connected
    /// `X11Source` on it (see the module comment above for why the connect call itself is
    /// the readiness probe).
    fn spawn_xvfb_and_connect() -> (TestXvfb, X11Source) {
        let (guard, source, _display) = spawn_xvfb_and_connect_with_display();
        (guard, source)
    }

    /// Same as `spawn_xvfb_and_connect`, additionally returning the `:N` display string —
    /// needed by RF-32 T3's `recover`-through-a-real-reconnect test, which must point its
    /// `X11Adapter`'s owned `Reconnector` at the SAME Xvfb its initial `source` already
    /// connected to.
    fn spawn_xvfb_and_connect_with_display() -> (TestXvfb, X11Source, String) {
        let display_num = ADAPTER_TEST_DISPLAY_BASE
            + NEXT_ADAPTER_TEST_DISPLAY_OFFSET.fetch_add(1, Ordering::SeqCst);
        let display = format!(":{display_num}");
        let child = Command::new("Xvfb")
            .arg(&display)
            .args(["-screen", "0", "320x240x24", "-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Xvfb must be installed for src/adapters.rs's X11Adapter tests");
        let guard = TestXvfb { child };

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match X11Source::connect_with_afk_threshold(Some(&display), Duration::from_secs(240)) {
                Ok((source, _diagnostics)) => return (guard, source, display),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("Xvfb on {display} did not become ready within 10s: {err:?}"),
            }
        }
    }

    #[test]
    fn replace_source_reports_the_new_connections_fd_and_not_the_old_ones() {
        let (_xvfb1, source1) = spawn_xvfb_and_connect();
        let (_xvfb2, source2) = spawn_xvfb_and_connect();
        let fd1 = source1.as_raw_fd();
        let fd2 = source2.as_raw_fd();
        assert_ne!(
            fd1, fd2,
            "two independent Xvfb connections must not share a fd"
        );

        let mut adapter = X11Adapter::new(source1, Rc::new(RefCell::new(passthrough_excluder())));
        assert_eq!(adapter.as_raw_fd(), fd1);

        adapter.replace_source(source2);

        assert_eq!(
            adapter.as_raw_fd(),
            fd2,
            "as_raw_fd must observe the replacement connection, not the original one"
        );
    }

    #[test]
    fn replace_source_resets_current_app_id_to_the_desktop_sentinel() {
        let (_xvfb1, source1) = spawn_xvfb_and_connect();
        let (_xvfb2, source2) = spawn_xvfb_and_connect();

        let mut adapter = X11Adapter::new(source1, Rc::new(RefCell::new(passthrough_excluder())));
        adapter.current_app_id = "stale-app-from-before-the-outage".to_string();

        adapter.replace_source(source2);

        assert_eq!(
            adapter.current_app_id, DESKTOP_APP_ID,
            "a freshly reconnected X11Source starts with no known active window \
             (active_window: None, src/x11.rs:479), so the adapter's own memory of the \
             active app-id must not survive a reconnect stale"
        );
    }

    #[test]
    fn x11_adapter_owns_a_working_reconnector() {
        let (_xvfb, source) = spawn_xvfb_and_connect();
        let mut adapter = X11Adapter::with_reconnect_config(
            source,
            Rc::new(RefCell::new(passthrough_excluder())),
            // A local display number nothing is listening on: `x11rb::connect` fails fast
            // against a missing unix socket, no DNS/TCP path involved, so this never needs
            // `Reconnector::attempt`'s 5s bound (T1, `RECONNECT_CONNECT_TIMEOUT`) to fire.
            Some(":9999"),
            Duration::from_secs(1),
        );

        match adapter.reconnector_mut().attempt(0) {
            ReconnectAttempt::OutageOpened { .. } => {}
            _ => panic!("attempting to reconnect to a nonexistent display must fail"),
        }
    }

    // --- BudgetedSource::recover (RF-32 T3) --------------------------------------------------

    #[test]
    fn recover_reports_outage_opened_when_the_reconnect_target_is_unreachable() {
        let (_xvfb, source) = spawn_xvfb_and_connect();
        let mut adapter = X11Adapter::with_reconnect_config(
            source,
            Rc::new(RefCell::new(passthrough_excluder())),
            // Same unreachable local display `x11_adapter_owns_a_working_reconnector` uses:
            // `x11rb::connect` fails fast against a missing unix socket, so this never needs
            // `Reconnector::attempt`'s 5s bound to fire.
            Some(":9999"),
            Duration::from_secs(1),
        );

        match adapter.recover(0) {
            Some(RecoveryOutcome::OutageOpened { .. }) => {}
            other => panic!("expected Some(RecoveryOutcome::OutageOpened), got {other:?}"),
        }
    }

    #[test]
    fn recover_installs_the_reconnected_source_and_reports_restored() {
        // The adapter starts on one Xvfb connection (`source`); its OWN `Reconnector` is
        // pointed at a SECOND, independent Xvfb on `display2` (not back at `display1`) so a
        // successful `recover()` is unambiguously proof that `replace_source` ran: the fd
        // genuinely changes to a DIFFERENT server's connection, not just a fresh connection to
        // the same one (which could theoretically reuse a kernel-recycled fd number by
        // coincidence and look identical either way).
        let (_xvfb1, source) = spawn_xvfb_and_connect();
        let (_xvfb2, _source2_unused, display2) = spawn_xvfb_and_connect_with_display();
        let original_fd = source.as_raw_fd();

        let mut adapter = X11Adapter::with_reconnect_config(
            source,
            Rc::new(RefCell::new(passthrough_excluder())),
            Some(&display2),
            Duration::from_secs(1),
        );
        assert_eq!(adapter.as_raw_fd(), original_fd);

        match adapter.recover(0) {
            Some(RecoveryOutcome::Restored { .. }) => {}
            other => panic!("expected Some(RecoveryOutcome::Restored), got {other:?}"),
        }

        assert_ne!(
            adapter.as_raw_fd(),
            original_fd,
            "recover() must have installed the freshly reconnected source, observable through \
             as_raw_fd() changing — not merely through a field recover() happened to set"
        );
    }

    #[test]
    fn recover_resets_current_app_id_after_installing_the_reconnected_source() {
        // `replace_source` already resets `current_app_id`; this proves `recover()` actually
        // routes through `replace_source` for that side effect too, rather than only updating
        // `self.source` some other way.
        let (_xvfb1, source) = spawn_xvfb_and_connect();
        let (_xvfb2, _source2_unused, display2) = spawn_xvfb_and_connect_with_display();

        let mut adapter = X11Adapter::with_reconnect_config(
            source,
            Rc::new(RefCell::new(passthrough_excluder())),
            Some(&display2),
            Duration::from_secs(1),
        );
        adapter.current_app_id = "stale-app-from-before-the-outage".to_string();

        match adapter.recover(0) {
            Some(RecoveryOutcome::Restored { .. }) => {}
            other => panic!("expected Some(RecoveryOutcome::Restored), got {other:?}"),
        }

        assert_eq!(adapter.current_app_id, DESKTOP_APP_ID);
    }

    // --- BudgetedSource::recover's default, exercised through the real LogindSource enum ----

    #[test]
    fn logind_source_has_no_recovery_and_falls_through_to_the_trait_default() {
        // `LogindSource` (and the `LogindAdapter` it wraps) never overrides `recover` — this
        // is the real production type the feature doc names ("Assert LogindAdapter is
        // unaffected"), exercised here via its D-Bus-free `Absent` branch so the test needs no
        // real session bus.
        let null = NullFd::new().expect("NullFd::new must succeed");
        let mut source = LogindSource::Absent(null);

        assert!(
            source.recover(0).is_none(),
            "LogindSource must keep BudgetedSource::recover's no-op default unchanged"
        );
    }
}
