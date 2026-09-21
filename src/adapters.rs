//! Phase 15 composition: translates `x11.rs`'s `RawEvent`/`logind.rs`'s `LogindEvent` into
//! `tracker::SourceEvent`s and implements `reactor::BudgetedSource` over the real capture
//! sources, so `main.rs` can hand concrete types to `ReactorSource::new` (design §2 D-2's fd0/
//! fd1). The translation logic itself is split into plain functions with no fd/`BudgetedSource`
//! involved at all, so it is unit-testable the same way `reactor.rs`'s own tests use synthetic
//! doubles instead of real X11/D-Bus (tasks.md Phase 14's precedent).
#![allow(
    dead_code,
    reason = "consumed by main.rs's daemon composition landing in a later work unit of this \
              phase; this allow does not survive past it"
)]

use std::cell::RefCell;
use std::os::fd::RawFd;
use std::rc::Rc;

use xwindowlog::exclude::Excluder;
use xwindowlog::logind::{LockedHintTracker, LogindEvent, ZbusSessionMonitor};
use xwindowlog::reactor::BudgetedSource;
use xwindowlog::tracker::{SourceError, SourceEvent, WindowInfo};
use xwindowlog::x11::{RawEvent, X11Source};

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

/// `reactor::BudgetedSource` over the real `x11.rs` capture engine (design §2 D-2 fd0).
pub struct X11Adapter {
    source: X11Source,
    excluder: Rc<RefCell<Excluder>>,
    current_app_id: String,
}

impl X11Adapter {
    pub fn new(source: X11Source, excluder: Rc<RefCell<Excluder>>) -> Self {
        X11Adapter {
            source,
            excluder,
            current_app_id: DESKTOP_APP_ID.to_string(),
        }
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
}
