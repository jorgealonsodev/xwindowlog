//! §11.1 transition table; `SourceEvent` → `Vec<Effect>`; pure, no I/O (D-8).
//!
//! This module performs no I/O and consults no clock other than through the
//! values it is given (interval-tracking spec, Purpose) — every timestamp
//! arrives as a `WallTs`/`MonoInstant` parameter, never read from the OS
//! directly. That is what makes the state machine testable with synthetic
//! event sequences and no X server, D-Bus connection, or database.
//!
//! **Deviation from design §2 D-8, documented rather than silent.** The
//! design's literal `WindowInfo` uses `app_id: SafeAppId, title: SafeTitle`
//! — `exclude.rs` types (D-7) that do not exist yet at this point in the
//! build order (`exclude.rs` is Phase 7; this is Phase 5). `WindowInfo` and
//! `SourceEvent::TitleChanged` use owned `String` here instead. This is not
//! a gap being papered over: tasks 8.3/8.4 already plan to "confirm/adjust
//! the tracker's title-change comparison to operate on `SafeTitle`" once
//! `exclude.rs` exists, which is exactly this substitution, done in the
//! order the task list itself expects.
#![allow(
    dead_code,
    reason = "tracker.rs lands before its consumers per design §8, the same ordering constraint clock.rs already documents. Its public API (Tracker, WindowSource, SourceEvent, Effect) is exercised only by this module's own tests until the pipeline (Phase 8) and the reactor (Phase 14) wire a real WindowSource and call Tracker::on_event. Phase 6 task 6.12 removes clock.rs's identical allow once store.rs/tracker.rs consume it; tracker.rs's own allow needs the equivalent removal task once Phase 8 or Phase 14 gives it a real caller — flagged here rather than left to be rediscovered."
)]

use std::time::Duration;

use crate::clock::{MonoInstant, WallTs};
use crate::store::{IntervalState, NewInterval};

/// Metadata for the currently active window (design §2 D-8). See the
/// module-level deviation note re: `String` in place of `SafeAppId`/
/// `SafeTitle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub app_id: String,
    pub title: String,
    pub pid: Option<u32>,
}

impl WindowInfo {
    /// The desktop-focus sentinel (window-capture "Desktop focus is
    /// legitimate activity"): `_NET_ACTIVE_WINDOW` becoming unset is
    /// recorded as this window identity, not discarded and not treated as
    /// absence.
    pub fn desktop() -> Self {
        WindowInfo {
            app_id: "(desktop)".to_string(),
            title: String::new(),
            pid: None,
        }
    }
}

/// A named deadline the tracker can arm/cancel via `Effect::ArmTimer`/
/// `CancelTimer` (design §2 D-8). The closed set here is drawn from the
/// task list's own later phases — 6.5 `TitleDebounce`, 6.7 `DestroyGrace`,
/// 14.2 `ReconnectBackoff`/`PauseExpiry`/`SessionReresolve` — because
/// `Effect` and `SourceEvent` reference this type now, in Phase 5, even
/// though no variant is constructed until its owning phase lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    TitleDebounce,
    DestroyGrace,
    ReconnectBackoff,
    PauseExpiry,
    SessionReresolve,
}

/// Owned event data crossing the `WindowSource` boundary (design §2 D-8).
/// **No `x11rb`/`zbus`/`nix`/`rusqlite` type may appear anywhere in this
/// enum** — this is the module boundary the rest of the phase depends on.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceEvent {
    /// `None` = desktop focused; legitimate activity, not absence.
    ActiveWindow(Option<WindowInfo>),
    /// Pre-debounce, already sanitized (D-7 — see the module-level
    /// deviation note: `String` stands in for `SafeTitle` until Phase 7/8).
    TitleChanged(String),
    ActiveWindowDestroyed,
    UserIdle {
        idle_for: Duration,
    },
    UserActive,
    SessionLocked,
    SessionUnlocked,
    PrepareForSleep(bool),
    Pause {
        until: Option<WallTs>,
    },
    Resume,
    DisplayLost,
    DisplayRestored,
    DeadlineElapsed(Timer),
    ReloadConfig,
    Shutdown,
}

/// An instruction the tracker returns for the caller to perform. The
/// tracker itself performs no I/O (design §2 D-8).
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// The atomic transition of D-1: closing one interval and opening the
    /// next. `at` is a single field shared by both the closed interval's
    /// `end` and the new interval's `start`, so a mismatched close/open
    /// timestamp is a compile-time impossibility here, not a discipline a
    /// caller has to remember — the caller cannot supply two different
    /// values because there is only one field to supply (RF-3's contiguity,
    /// enforced at the type level; see design §2 D-8, "why this is the
    /// linchpin").
    Transition {
        at: WallTs,
        open: NewInterval,
    },
    /// RF-33 shutdown, RF-27 pre-suspend: closes the open interval, opens
    /// nothing.
    CloseOnly {
        at: WallTs,
    },
    /// Startup, resume-from-suspend: opens `open` without closing anything
    /// first.
    OpenOnly {
        at: WallTs,
        open: NewInterval,
    },
    ArmTimer(Timer, MonoInstant),
    CancelTimer(Timer),
    /// stderr only (RF-24/RF-25/RF-29 warnings) — never a persisted value.
    Diagnostic(String),
}

/// An error surfaced by a `WindowSource` implementation. Owned data only —
/// see the module boundary note on `SourceEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError(pub String);

/// Blocks until an event arrives or a deadline passes, whichever is first
/// (design §2 D-8). Implemented by `ReactorSource` (Phase 14) in
/// production, and by scripted test doubles.
pub trait WindowSource {
    /// `Ok(None)` means the deadline elapsed with nothing to report.
    /// `deadline == None` means block indefinitely (the RNF-2 idle case).
    fn next_event(
        &mut self,
        deadline: Option<MonoInstant>,
    ) -> Result<Option<SourceEvent>, SourceError>;
}

/// The tracker's internal state (§11.1). Phase 5 implements the `Unknown`
/// and `Active` arms and the three transitions that do not need a deadline;
/// Phase 6 adds `Afk`/`Locked`/`Paused` and the remaining rows.
#[derive(Clone, Debug, PartialEq)]
enum TrackerState {
    Unknown,
    Active(WindowInfo),
}

/// Pure state machine: `(state, event, now) -> Vec<Effect>`. No I/O, no
/// clock read of its own (design §2 D-8).
#[derive(Debug)]
pub struct Tracker {
    state: TrackerState,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            state: TrackerState::Unknown,
        }
    }
}

impl Tracker {
    pub fn new() -> Self {
        Tracker::default()
    }

    /// Applies one event and returns the effects the caller must perform.
    pub fn on_event(&mut self, event: SourceEvent, now: WallTs) -> Vec<Effect> {
        match event {
            SourceEvent::ActiveWindow(window) => self.on_active_window(window, now),
            // The remaining §11.1 rows land in Phase 6.
            _ => Vec::new(),
        }
    }

    /// unknown (startup) + first valid active-window event → active;
    /// active + active window change → active (new window); active window
    /// becomes none (desktop focused) → active with the `"(desktop)"`
    /// sentinel, not discarded and not treated as absence
    /// (interval-tracking "State transition table"; window-capture
    /// "Desktop focus is legitimate activity").
    fn on_active_window(&mut self, window: Option<WindowInfo>, now: WallTs) -> Vec<Effect> {
        let window = window.unwrap_or_else(WindowInfo::desktop);

        let is_first_event = matches!(self.state, TrackerState::Unknown);
        let open = new_interval(&window);
        let effect = if is_first_event {
            Effect::OpenOnly { at: now, open }
        } else {
            Effect::Transition { at: now, open }
        };

        self.state = TrackerState::Active(window);
        vec![effect]
    }

    /// The next deadline the reactor must poll against, if any. Phase 5 has
    /// no deadlines to arm yet; Phase 6 adds debounce and destroy-grace.
    pub fn next_deadline(&self) -> Option<(Timer, MonoInstant)> {
        None
    }
}

fn new_interval(window: &WindowInfo) -> NewInterval {
    NewInterval {
        app: window.app_id.clone(),
        title: window.title.clone(),
        pid: window.pid,
        state: IntervalState::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock};

    fn window(app_id: &str, title: &str, pid: Option<u32>) -> WindowInfo {
        WindowInfo {
            app_id: app_id.to_string(),
            title: title.to_string(),
            pid,
        }
    }

    #[test]
    fn unknown_startup_plus_first_active_window_event_opens_active() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        let win = window("firefox", "GitHub", Some(4821));

        let effects = tracker.on_event(SourceEvent::ActiveWindow(Some(win)), clock.now_wall());

        assert_eq!(
            effects,
            vec![Effect::OpenOnly {
                at: WallTs(1_000),
                open: NewInterval {
                    app: "firefox".to_string(),
                    title: "GitHub".to_string(),
                    pid: Some(4821),
                    state: IntervalState::Active,
                },
            }]
        );
    }

    #[test]
    fn active_window_change_closes_and_opens_at_the_same_instant() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        tracker.on_event(
            SourceEvent::ActiveWindow(Some(window("firefox", "GitHub", Some(4821)))),
            clock.now_wall(),
        );

        clock.advance(Duration::from_secs(30));
        let new_window = window("kitty", "zsh", Some(9001));

        let effects = tracker.on_event(
            SourceEvent::ActiveWindow(Some(new_window)),
            clock.now_wall(),
        );

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(1_030),
                open: NewInterval {
                    app: "kitty".to_string(),
                    title: "zsh".to_string(),
                    pid: Some(9001),
                    state: IntervalState::Active,
                },
            }]
        );
    }

    #[test]
    fn desktop_focus_is_recorded_with_the_sentinel_not_discarded() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        tracker.on_event(
            SourceEvent::ActiveWindow(Some(window("firefox", "GitHub", Some(4821)))),
            clock.now_wall(),
        );

        clock.advance(Duration::from_secs(5));
        let effects = tracker.on_event(SourceEvent::ActiveWindow(None), clock.now_wall());

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(1_005),
                open: NewInterval {
                    app: "(desktop)".to_string(),
                    title: String::new(),
                    pid: None,
                    state: IntervalState::Active,
                },
            }]
        );
    }
}
