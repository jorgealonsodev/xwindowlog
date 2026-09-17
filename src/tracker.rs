//! §11.1 transition table; `SourceEvent` → `Vec<Effect>`; pure, no I/O (D-8).
//!
//! This module performs no I/O and consults no clock other than through the
//! values it is given (interval-tracking spec, Purpose) — every timestamp
//! arrives as a `WallTs`/`MonoInstant` parameter, never read from the OS
//! directly. That is what makes the state machine testable with synthetic
//! event sequences and no X server, D-Bus connection, or database.
//!
//! **Deviation from design §2 D-8, documented rather than silent — now
//! partially resolved.** The design's literal `WindowInfo` uses
//! `app_id: SafeAppId, title: SafeTitle` — `exclude.rs` types (D-7) that did
//! not exist yet in Phase 5/6 (`exclude.rs` is Phase 7). `WindowInfo.title`
//! and `SourceEvent::TitleChanged` used owned `String` through Phase 7;
//! tasks 8.3/8.4 do the planned substitution to `SafeTitle`. `app_id` stays
//! a plain `String`: `SafeAppId` is not defined anywhere in this crate (see
//! `exclude.rs`'s own module doc, Deviation note — Phase 7 scoped that
//! module to `RawTitle`/`SafeTitle` only), and RF-47's `hide_app` already
//! substitutes the literal `"[hidden]"` string for `app_id`, the same
//! mechanism `exclude.rs` uses for the title.
//!
//! **Deviations introduced in Phase 6, documented rather than silent.**
//!
//! 1. `Tracker::on_event` gained a third parameter, `now_mono: MonoInstant`,
//!    beyond design §2 D-8's excerpted `on_event(&mut self, ev: SourceEvent,
//!    now: WallTs) -> Vec<Effect>`. `ArmTimer` carries an *absolute*
//!    `MonoInstant` deadline (RF-30's title debounce, RF-23's destroy grace),
//!    and the tracker cannot compute "now + 250ms" without knowing the
//!    current monotonic instant — reading `Instant::now()` itself would
//!    reintroduce the exact untestable clock read this module's Purpose
//!    forbids. D-8's excerpt is already known to be non-exhaustive (Phase 5
//!    Discovery: `Timer`/`SourceError` are referenced there but never
//!    defined), so extending its one shown signature the same way is
//!    consistent with that precedent, not a departure from it.
//! 2. `TrackerState::Unknown` is revisited after Phase 6 (`locked →
//!    unknown`, `any → unknown` on X11 loss), which Phase 5's
//!    `is_first_event = matches!(self.state, TrackerState::Unknown)` check
//!    would have mistaken for "the tracker's very first event", wrongly
//!    emitting `OpenOnly` instead of `Transition` and silently dropping the
//!    interval that was open before. Replaced with an explicit
//!    `has_opened_interval: bool` flag, set once and never unset — see
//!    `open_or_transition`.
//! 3. The PRD §11.1 / interval-tracking spec.md table's `afk | Idle alarm,
//!    negative transition` row reopens "the same window if it still exists,
//!    otherwise unknown". This tracker has no I/O and cannot itself confirm
//!    a window still exists — only `x11.rs`'s BadWindow race handling (RF-22,
//!    Phase 10) can. `on_user_active` therefore always reopens the window
//!    remembered from `TrackerState::Afk`; if it no longer exists, the next
//!    real `ActiveWindow`/`ActiveWindowDestroyed` event corrects the state
//!    immediately afterward, the same way RF-22's race is already handled
//!    for a window destroyed between event mask registration and the next
//!    property read.
//! 4. The PRD's transition table has no `paused + Resume` row at all — every
//!    row Phase 6 implements is exactly this table's set, and this table
//!    stops at `paused` having no listed exit. Task 15.11 explicitly wires
//!    `SourceEvent::Resume` later ("RED tests for full pause/resume behavior
//!    live in Phase 17"), so `on_event_paused` stays a no-op for every event
//!    in this phase, matching Phase 5's original wildcard fallback rather
//!    than inventing an unspecified transition.
use std::time::Duration;

use crate::clock::{backdated_close, close_at, Close, MonoInstant, WallTs};
use crate::exclude::SafeTitle;
use crate::store::{IntervalState, NewInterval};

/// RF-23's one-shot grace period after `DestroyNotify` on the tracked window.
const DESTROY_GRACE: Duration = Duration::from_millis(250);

/// RF-30's default `title_debounce_ms`, used unless a config value is wired
/// in via `Tracker::with_title_debounce` (Phase 15, task 15.2).
const DEFAULT_TITLE_DEBOUNCE: Duration = Duration::from_millis(2000);

/// The dictionary sentinels `apps(1, '?')`/`titles(1, '-')` (RF-11 schema),
/// used by every non-window-carrying interval (`unknown`, `afk`, `locked`,
/// `paused`) — there is no window identity to record for these states.
const SENTINEL_APP: &str = "?";
const SENTINEL_TITLE: &str = "-";

/// Metadata for the currently active window (design §2 D-8). See the
/// module-level deviation note re: `String` still standing in for
/// `SafeAppId` — `title` is `SafeTitle` as of task 8.3/8.4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub app_id: String,
    pub title: SafeTitle,
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
            title: SafeTitle::empty(),
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
    /// Already sanitized (D-7): the only way to obtain a `SafeTitle` is
    /// `Excluder::evaluate`, so this variant's own type is the proof that
    /// §14.3's boundary was crossed before this event was constructed.
    TitleChanged(SafeTitle),
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

/// The tracker's internal state (§11.1's five states). `Afk` carries the
/// window that was active when the idle alarm fired, so the negative
/// transition can reopen it (interval-tracking "afk | Idle alarm, negative
/// transition | active (same window if it still exists...)"). `Locked` and
/// `Paused` carry nothing: neither row in the table returns to a
/// remembered prior window — `locked` always resolves to `unknown`, and
/// `paused`'s own resume path is out of this phase's scope (see the
/// module-level Phase 6 deviation note on `SourceEvent::Resume`).
#[derive(Clone, Debug, PartialEq)]
enum TrackerState {
    Unknown,
    Active(WindowInfo),
    Afk(WindowInfo),
    Locked,
    Paused,
}

/// The single outstanding local deadline the tracker has armed, if any
/// (design §2 D-8's `ArmTimer`/`CancelTimer`/`next_deadline`). Title
/// debounce and destroy-grace are mutually exclusive in practice — a
/// destroyed window's pending title change is moot (see
/// `on_active_window_destroyed`), so one slot is enough for Phase 6's two
/// tracker-owned deadlines. `reactor.rs` (Phase 14) tracks the unrelated
/// `ReconnectBackoff`/`PauseExpiry`/`SessionReresolve` deadlines itself,
/// outside the tracker.
#[derive(Clone, Debug, PartialEq)]
enum PendingTimer {
    None,
    /// RF-30: the new title waiting to become stable, and the monotonic
    /// instant the debounce elapses.
    TitleDebounce {
        pending_title: SafeTitle,
        deadline: MonoInstant,
    },
    /// RF-23: the wall-clock instant of the *original* `DestroyNotify` —
    /// stashed here because that is the closing timestamp the row requires,
    /// not the instant the deadline itself fires.
    DestroyGrace {
        destroyed_at: WallTs,
        deadline: MonoInstant,
    },
}

/// Pure state machine: `(state, event, now) -> Vec<Effect>`. No I/O, no
/// clock read of its own (design §2 D-8).
#[derive(Debug)]
pub struct Tracker {
    state: TrackerState,
    /// `false` until the very first interval-opening effect this tracker
    /// instance has ever emitted. Distinguishes "truly the first event of
    /// the tracker's life" (→ `Effect::OpenOnly`) from "back in `Unknown`
    /// after some history" (→ `Effect::Transition`, closing the interval
    /// that was open before) — the two are not the same thing once Phase 6
    /// adds rows that revisit `Unknown` (`locked → unknown`, `any → unknown`
    /// on X11 loss). See the module-level Phase 6 deviation note.
    has_opened_interval: bool,
    /// The `start` of the currently open interval, tracked so every closing
    /// `at` can be clamped against it via `close_at` (RF-28) — not only the
    /// AFK-specific backdated case, but every transition, so a backwards
    /// wall-clock jump can never produce a negative-duration interval no
    /// matter which row triggers it.
    interval_start: WallTs,
    pending_timer: PendingTimer,
    title_debounce: Duration,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            state: TrackerState::Unknown,
            has_opened_interval: false,
            // Never read before `has_opened_interval` is true — see
            // `open_or_transition`.
            interval_start: WallTs(0),
            pending_timer: PendingTimer::None,
            title_debounce: DEFAULT_TITLE_DEBOUNCE,
        }
    }
}

impl Tracker {
    pub fn new() -> Self {
        Tracker::default()
    }

    /// Phase 15 (task 15.2) wires `title_debounce_ms` from `config.toml`
    /// through this constructor; Phase 6 exposes it now so the debounce
    /// tests do not have to wait 2000ms of `FakeClock` time to exercise a
    /// deterministic deadline.
    pub fn with_title_debounce(title_debounce: Duration) -> Self {
        Tracker {
            title_debounce,
            ..Tracker::default()
        }
    }

    /// Applies one event and returns the effects the caller must perform.
    /// `now_mono` is only consulted by events that arm a monotonic deadline
    /// (`TitleChanged`, `ActiveWindowDestroyed`) — see the module-level
    /// Phase 6 deviation note on why this parameter was added.
    ///
    /// Dispatches first on `self.state`, then on `event`, so this function
    /// and its five `on_event_*` arms can be read side by side with
    /// `interval-tracking/spec.md`'s "State transition table" and PRD.md
    /// §11.1's transition table, one source-state row group at a time
    /// (task 6.11):
    ///
    /// | Source state | Handled in | §11.1 rows |
    /// |---|---|---|
    /// | (any) | `on_shutdown`, `on_display_lost` | shutdown; X11 loss |
    /// | `unknown` | `on_event_unknown` → `on_active_window` | first active-window event |
    /// | `active` | `on_event_active` | title change (debounced), window change, desktop sentinel, destroy-grace arm, idle positive (backdated), lock, pause |
    /// | `afk` | `on_event_afk` | idle negative, lock, pause |
    /// | `locked` | `on_event_locked` | unlock |
    /// | `paused` | `on_event_paused` | none in this phase's scope — see deviation note 4 |
    pub fn on_event(
        &mut self,
        event: SourceEvent,
        now: WallTs,
        now_mono: MonoInstant,
    ) -> Vec<Effect> {
        // "any" rows (interval-tracking "State transition table") apply
        // regardless of source state, so they are dispatched before the
        // per-state match below.
        match event {
            SourceEvent::Shutdown => return self.on_shutdown(now),
            SourceEvent::DisplayLost => return self.on_display_lost(now),
            _ => {}
        }

        match self.state {
            TrackerState::Unknown => self.on_event_unknown(event, now),
            TrackerState::Active(_) => self.on_event_active(event, now, now_mono),
            TrackerState::Afk(_) => self.on_event_afk(event, now),
            TrackerState::Locked => self.on_event_locked(event, now),
            TrackerState::Paused => self.on_event_paused(event, now),
        }
    }

    fn on_event_unknown(&mut self, event: SourceEvent, now: WallTs) -> Vec<Effect> {
        match event {
            SourceEvent::ActiveWindow(window) => self.on_active_window(window, now),
            _ => Vec::new(),
        }
    }

    fn on_event_active(
        &mut self,
        event: SourceEvent,
        now: WallTs,
        now_mono: MonoInstant,
    ) -> Vec<Effect> {
        match event {
            SourceEvent::ActiveWindow(window) => self.on_active_window(window, now),
            SourceEvent::TitleChanged(new_title) => self.on_title_changed(new_title, now_mono),
            SourceEvent::ActiveWindowDestroyed => self.on_active_window_destroyed(now, now_mono),
            SourceEvent::DeadlineElapsed(timer) => self.on_deadline_elapsed(timer, now),
            SourceEvent::UserIdle { idle_for } => self.on_user_idle(idle_for, now),
            SourceEvent::SessionLocked | SourceEvent::PrepareForSleep(true) => self.on_lock(now),
            SourceEvent::Pause { .. } => self.on_pause(now),
            _ => Vec::new(),
        }
    }

    fn on_event_afk(&mut self, event: SourceEvent, now: WallTs) -> Vec<Effect> {
        match event {
            SourceEvent::UserActive => self.on_user_active(now),
            SourceEvent::SessionLocked | SourceEvent::PrepareForSleep(true) => self.on_lock(now),
            SourceEvent::Pause { .. } => self.on_pause(now),
            _ => Vec::new(),
        }
    }

    fn on_event_locked(&mut self, event: SourceEvent, now: WallTs) -> Vec<Effect> {
        match event {
            SourceEvent::SessionUnlocked | SourceEvent::PrepareForSleep(false) => {
                self.on_unlock(now)
            }
            _ => Vec::new(),
        }
    }

    /// No §11.1 row exists for `paused` in this phase's scope — `Resume`'s
    /// wiring is Phase 15's task 15.11, with its RED tests in Phase 17
    /// (tasks.md, explicit). Any event received while paused is a no-op
    /// here, matching Phase 5's original wildcard fallback.
    fn on_event_paused(&mut self, _event: SourceEvent, _now: WallTs) -> Vec<Effect> {
        Vec::new()
    }

    /// unknown (startup) + first valid active-window event → active;
    /// active + active window change → active (new window); active window
    /// becomes none (desktop focused) → active with the `"(desktop)"`
    /// sentinel, not discarded and not treated as absence
    /// (interval-tracking "State transition table"; window-capture
    /// "Desktop focus is legitimate activity").
    fn on_active_window(&mut self, window: Option<WindowInfo>, now: WallTs) -> Vec<Effect> {
        let window = window.unwrap_or_else(WindowInfo::desktop);
        let mut effects = self.cancel_pending_timer();
        let open = new_interval(&window);
        effects.extend(self.open_or_transition(now, open));
        self.state = TrackerState::Active(window);
        effects
    }

    /// RF-30: a title change only closes/opens an interval once the new
    /// title has been stable for `title_debounce_ms`. No transition happens
    /// here — only `ArmTimer`/`CancelTimer` bookkeeping; the actual close/
    /// open happens in `on_deadline_elapsed` when the debounce elapses.
    fn on_title_changed(&mut self, new_title: SafeTitle, now_mono: MonoInstant) -> Vec<Effect> {
        let unchanged = matches!(&self.state, TrackerState::Active(w) if w.title == new_title);
        if unchanged {
            return Vec::new();
        }

        let mut effects = Vec::new();
        if matches!(self.pending_timer, PendingTimer::TitleDebounce { .. }) {
            effects.push(Effect::CancelTimer(Timer::TitleDebounce));
        }

        let deadline = MonoInstant(now_mono.0 + self.title_debounce);
        self.pending_timer = PendingTimer::TitleDebounce {
            pending_title: new_title,
            deadline,
        };
        effects.push(Effect::ArmTimer(Timer::TitleDebounce, deadline));
        effects
    }

    /// RF-23: arms the 250ms destruction safety-net deadline, stashing the
    /// `DestroyNotify` instant for `on_deadline_elapsed` to use as `end` if
    /// the deadline fires (not the deadline-fire instant itself).
    fn on_active_window_destroyed(&mut self, now: WallTs, now_mono: MonoInstant) -> Vec<Effect> {
        // A destroyed window's pending title change is moot.
        let mut effects = self.cancel_pending_timer();

        let deadline = MonoInstant(now_mono.0 + DESTROY_GRACE);
        self.pending_timer = PendingTimer::DestroyGrace {
            destroyed_at: now,
            deadline,
        };
        effects.push(Effect::ArmTimer(Timer::DestroyGrace, deadline));
        effects
    }

    /// Resolves whichever local deadline just fired: `TitleDebounce`
    /// finalizes a stable title change; `DestroyGrace` closes the interval
    /// at the original `DestroyNotify` instant and moves to `unknown`.
    fn on_deadline_elapsed(&mut self, timer: Timer, now: WallTs) -> Vec<Effect> {
        match (
            timer,
            std::mem::replace(&mut self.pending_timer, PendingTimer::None),
        ) {
            (Timer::TitleDebounce, PendingTimer::TitleDebounce { pending_title, .. }) => {
                let mut new_window = match &self.state {
                    TrackerState::Active(w) => w.clone(),
                    // Defensive: a title can only debounce while active; the
                    // pending_timer bookkeeping above guarantees this branch
                    // is reached only when it was armed by `on_title_changed`.
                    _ => return Vec::new(),
                };
                new_window.title = pending_title;
                let open = new_interval(&new_window);
                let effects = self.open_or_transition(now, open);
                self.state = TrackerState::Active(new_window);
                effects
            }
            (Timer::DestroyGrace, PendingTimer::DestroyGrace { destroyed_at, .. }) => {
                let open = sentinel_interval(IntervalState::Unknown);
                let effects = self.open_or_transition(destroyed_at, open);
                self.state = TrackerState::Unknown;
                effects
            }
            (_, stale_pending) => {
                // A deadline fired for a timer this tracker no longer has
                // armed (e.g. it was already cancelled) — restore whatever
                // actually is pending and no-op, rather than dropping real
                // bookkeeping on a stale/mismatched event.
                self.pending_timer = stale_pending;
                Vec::new()
            }
        }
    }

    /// active + idle alarm positive transition → afk, closing timestamp
    /// backdated to `now - idle_for` (RF-4), clamped against this
    /// interval's `start` if backdating would go negative (RF-28).
    fn on_user_idle(&mut self, idle_for: Duration, now: WallTs) -> Vec<Effect> {
        let window = match &self.state {
            TrackerState::Active(w) => w.clone(),
            _ => return Vec::new(),
        };

        let mut effects = self.cancel_pending_timer();

        // RF-4: backdated to `now - idle_for`, never `now` itself — and
        // RF-28's clamp applies here just like every other close, in case
        // backdating would land before this interval's own `start`.
        let end = match backdated_close(self.interval_start, now, idle_for) {
            Close::Ok(end) => end,
            Close::ClampedBackwards { to, .. } => {
                effects.push(Effect::Diagnostic(format!(
                    "backdating an AFK close past its interval start; clamped end to start ({})",
                    to.0
                )));
                to
            }
        };

        let open = sentinel_interval(IntervalState::Afk);
        effects.extend(self.open_or_transition(end, open));
        self.state = TrackerState::Afk(window);
        effects
    }

    /// afk + idle alarm negative transition → active, reopening the window
    /// that was active before the alarm fired (interval-tracking "afk |
    /// Idle alarm, negative transition"). See the module-level Phase 6
    /// deviation note on the "otherwise unknown" half of this row.
    fn on_user_active(&mut self, now: WallTs) -> Vec<Effect> {
        let window = match &self.state {
            TrackerState::Afk(w) => w.clone(),
            _ => return Vec::new(),
        };
        let open = new_interval(&window);
        let effects = self.open_or_transition(now, open);
        self.state = TrackerState::Active(window);
        effects
    }

    /// active/afk + `LockedHint → true` or `PrepareForSleep(true)` → locked.
    fn on_lock(&mut self, now: WallTs) -> Vec<Effect> {
        let mut effects = self.cancel_pending_timer();
        let open = sentinel_interval(IntervalState::Locked);
        effects.extend(self.open_or_transition(now, open));
        self.state = TrackerState::Locked;
        effects
    }

    /// locked + `LockedHint → false` or `PrepareForSleep(false)` → unknown.
    fn on_unlock(&mut self, now: WallTs) -> Vec<Effect> {
        let open = sentinel_interval(IntervalState::Unknown);
        let effects = self.open_or_transition(now, open);
        self.state = TrackerState::Unknown;
        effects
    }

    /// active/afk + pause requested → paused.
    fn on_pause(&mut self, now: WallTs) -> Vec<Effect> {
        let mut effects = self.cancel_pending_timer();
        let open = sentinel_interval(IntervalState::Paused);
        effects.extend(self.open_or_transition(now, open));
        self.state = TrackerState::Paused;
        effects
    }

    /// any + loss of the X11 connection → unknown, at the detection instant.
    fn on_display_lost(&mut self, now: WallTs) -> Vec<Effect> {
        let mut effects = self.cancel_pending_timer();
        let open = sentinel_interval(IntervalState::Unknown);
        effects.extend(self.open_or_transition(now, open));
        self.state = TrackerState::Unknown;
        effects
    }

    /// any + shutdown signal → process exit. Closes the open interval
    /// (RF-33) if one has ever been opened; emits nothing otherwise (there
    /// is nothing to close).
    fn on_shutdown(&mut self, now: WallTs) -> Vec<Effect> {
        let mut effects = self.cancel_pending_timer();
        if self.has_opened_interval {
            effects.push(Effect::CloseOnly { at: now });
        }
        effects
    }

    /// Closes out any armed local deadline, returning the `CancelTimer`
    /// effect(s) for the caller to perform. A no-op (empty vec) when
    /// nothing was pending.
    fn cancel_pending_timer(&mut self) -> Vec<Effect> {
        match std::mem::replace(&mut self.pending_timer, PendingTimer::None) {
            PendingTimer::None => Vec::new(),
            PendingTimer::TitleDebounce { .. } => vec![Effect::CancelTimer(Timer::TitleDebounce)],
            PendingTimer::DestroyGrace { .. } => vec![Effect::CancelTimer(Timer::DestroyGrace)],
        }
    }

    /// The shared "close the currently open interval, open `open`" step
    /// every transition in this module funnels through (design §2 D-8's
    /// "why this is the linchpin" — one `at` field, so RF-3's contiguity is
    /// a type invariant). Clamps `requested_at` against `self.interval_start`
    /// via `close_at` (RF-28) so a backwards wall-clock jump can never
    /// produce a negative-duration interval, regardless of which row
    /// triggered the transition — not only the AFK-specific backdated case.
    /// Emits an `Effect::Diagnostic` when a clamp actually occurred.
    fn open_or_transition(&mut self, requested_at: WallTs, open: NewInterval) -> Vec<Effect> {
        let mut effects = Vec::new();

        let at = if self.has_opened_interval {
            match close_at(self.interval_start, requested_at) {
                Close::Ok(at) => at,
                Close::ClampedBackwards { to, .. } => {
                    effects.push(Effect::Diagnostic(format!(
                        "backwards wall-clock jump detected while closing an interval; \
                         clamped end to start ({})",
                        to.0
                    )));
                    to
                }
            }
        } else {
            requested_at
        };

        self.interval_start = at;

        if self.has_opened_interval {
            effects.push(Effect::Transition { at, open });
        } else {
            self.has_opened_interval = true;
            effects.push(Effect::OpenOnly { at, open });
        }

        effects
    }

    /// The next deadline the reactor must poll against, if any (Phase 6:
    /// `TitleDebounce`/`DestroyGrace`; `reactor.rs` (Phase 14) tracks the
    /// unrelated backoff/expiry/reresolve deadlines itself).
    pub fn next_deadline(&self) -> Option<(Timer, MonoInstant)> {
        match &self.pending_timer {
            PendingTimer::None => None,
            PendingTimer::TitleDebounce { deadline, .. } => Some((Timer::TitleDebounce, *deadline)),
            PendingTimer::DestroyGrace { deadline, .. } => Some((Timer::DestroyGrace, *deadline)),
        }
    }
}

fn new_interval(window: &WindowInfo) -> NewInterval {
    NewInterval {
        app: window.app_id.clone(),
        // `store.rs`'s `NewInterval.title` is a plain `String` (it names a
        // storage column, not a trust boundary) — this is the one place
        // that unwraps `SafeTitle`'s content, after sanitization has
        // already happened (D-7).
        title: window.title.as_str().to_string(),
        pid: window.pid,
        state: IntervalState::Active,
    }
}

fn sentinel_interval(state: IntervalState) -> NewInterval {
    NewInterval {
        app: SENTINEL_APP.to_string(),
        title: SENTINEL_TITLE.to_string(),
        pid: None,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock};

    /// Sanitizes `title` through a real, fixed pure-passthrough `Excluder`
    /// (no exclusion rules, no secret redaction) to obtain a `SafeTitle` —
    /// there is no other way to construct one outside `exclude.rs`
    /// (`SafeTitle::from_sanitized` is private to that module), so every
    /// test fixture in this module goes through the real boundary rather
    /// than a tracker-local shortcut.
    fn safe_title(title: &str) -> SafeTitle {
        use crate::exclude::{Excluder, RawTitle};
        static NO_OP: std::sync::LazyLock<Excluder> = std::sync::LazyLock::new(|| {
            Excluder::from_toml_str(
                r#"
                sanitize_secrets = false
                disable_default_excludes = [
                    "password-managers", "banking-generic", "private-browsing",
                    "gpg-ssh-prompts", "2fa-otp",
                ]
                "#,
            )
            .expect("valid literal config compiles")
        });
        NO_OP
            .evaluate("test-fixture-app-never-excluded", RawTitle::new(title))
            .title
    }

    fn window(app_id: &str, title: &str, pid: Option<u32>) -> WindowInfo {
        WindowInfo {
            app_id: app_id.to_string(),
            title: safe_title(title),
            pid,
        }
    }

    /// Sends `event` at the `FakeClock`'s current wall/mono readings —
    /// shared by every Phase 6 test to keep call sites focused on the
    /// event and the assertion, not the clock plumbing.
    fn send(tracker: &mut Tracker, event: SourceEvent, clock: &FakeClock) -> Vec<Effect> {
        tracker.on_event(event, clock.now_wall(), clock.now_mono())
    }

    fn sentinel_open(state: IntervalState) -> NewInterval {
        NewInterval {
            app: "?".to_string(),
            title: "-".to_string(),
            pid: None,
            state,
        }
    }

    #[test]
    fn unknown_startup_plus_first_active_window_event_opens_active() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        let win = window("firefox", "GitHub", Some(4821));

        let effects = tracker.on_event(
            SourceEvent::ActiveWindow(Some(win)),
            clock.now_wall(),
            clock.now_mono(),
        );

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
            clock.now_mono(),
        );

        clock.advance(Duration::from_secs(30));
        let new_window = window("kitty", "zsh", Some(9001));

        let effects = tracker.on_event(
            SourceEvent::ActiveWindow(Some(new_window)),
            clock.now_wall(),
            clock.now_mono(),
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
            clock.now_mono(),
        );

        clock.advance(Duration::from_secs(5));
        let effects = tracker.on_event(
            SourceEvent::ActiveWindow(None),
            clock.now_wall(),
            clock.now_mono(),
        );

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

    // ---- Phase 6, task 6.1: one RED+GREEN pair per remaining §11.1 row ----

    #[test]
    fn stable_title_change_becomes_a_transition_once_the_debounce_elapses() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("kitty", "zsh", Some(1)))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        let arm_effects = send(
            &mut tracker,
            SourceEvent::TitleChanged(safe_title("vim")),
            &clock,
        );
        assert_eq!(arm_effects.len(), 1);
        assert!(matches!(
            arm_effects[0],
            Effect::ArmTimer(Timer::TitleDebounce, _)
        ));

        clock.advance(Duration::from_millis(2000));
        let effects = send(
            &mut tracker,
            SourceEvent::DeadlineElapsed(Timer::TitleDebounce),
            &clock,
        );

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(1_003),
                open: NewInterval {
                    app: "kitty".to_string(),
                    title: "vim".to_string(),
                    pid: Some(1),
                    state: IntervalState::Active,
                },
            }]
        );
    }

    #[test]
    fn session_locked_transitions_active_to_locked() {
        let clock = FakeClock::new(WallTs(2_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(10));
        let effects = send(&mut tracker, SourceEvent::SessionLocked, &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(2_010),
                open: sentinel_open(IntervalState::Locked),
            }]
        );
    }

    #[test]
    fn prepare_for_sleep_true_also_transitions_to_locked() {
        let clock = FakeClock::new(WallTs(3_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        let effects = send(&mut tracker, SourceEvent::PrepareForSleep(true), &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(3_001),
                open: sentinel_open(IntervalState::Locked),
            }]
        );
    }

    #[test]
    fn active_plus_pause_transitions_to_paused() {
        let clock = FakeClock::new(WallTs(4_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(2));
        let effects = send(&mut tracker, SourceEvent::Pause { until: None }, &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(4_002),
                open: sentinel_open(IntervalState::Paused),
            }]
        );
    }

    #[test]
    fn locked_plus_unlock_signal_transitions_to_unknown() {
        let clock = FakeClock::new(WallTs(5_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );
        send(&mut tracker, SourceEvent::SessionLocked, &clock);

        clock.advance(Duration::from_secs(3));
        let effects = send(&mut tracker, SourceEvent::SessionUnlocked, &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(5_003),
                open: sentinel_open(IntervalState::Unknown),
            }]
        );
    }

    #[test]
    fn afk_negative_idle_transition_reopens_the_previously_active_window() {
        let clock = FakeClock::new(WallTs(6_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("kitty", "zsh", Some(7)))),
            &clock,
        );

        clock.advance(Duration::from_secs(240));
        send(
            &mut tracker,
            SourceEvent::UserIdle {
                idle_for: Duration::ZERO,
            },
            &clock,
        );

        clock.advance(Duration::from_secs(30));
        let effects = send(&mut tracker, SourceEvent::UserActive, &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(6_270),
                open: NewInterval {
                    app: "kitty".to_string(),
                    title: "zsh".to_string(),
                    pid: Some(7),
                    state: IntervalState::Active,
                },
            }]
        );
    }

    #[test]
    fn any_state_plus_display_lost_transitions_to_unknown_at_detection_instant() {
        let clock = FakeClock::new(WallTs(7_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(15));
        let effects = send(&mut tracker, SourceEvent::DisplayLost, &clock);

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(7_015),
                open: sentinel_open(IntervalState::Unknown),
            }]
        );
    }

    #[test]
    fn any_state_plus_shutdown_closes_without_opening() {
        let clock = FakeClock::new(WallTs(8_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(4));
        let effects = send(&mut tracker, SourceEvent::Shutdown, &clock);

        assert_eq!(effects, vec![Effect::CloseOnly { at: WallTs(8_004) }]);
    }

    // ---- Phase 6, tasks 6.2/6.3: RF-4 backdated afk close ----

    #[test]
    fn afk_close_is_backdated_by_ms_since_user_input_not_the_alarm_instant() {
        let clock = FakeClock::new(WallTs(9_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(300));
        let effects = send(
            &mut tracker,
            SourceEvent::UserIdle {
                idle_for: Duration::from_secs(60),
            },
            &clock,
        );

        assert_eq!(
            effects,
            vec![Effect::Transition {
                // t_alarm is 9_300; the close is backdated to 9_240, NOT 9_300.
                at: WallTs(9_240),
                open: sentinel_open(IntervalState::Afk),
            }]
        );
    }

    #[test]
    fn afk_close_backdating_past_interval_start_clamps_and_warns() {
        let clock = FakeClock::new(WallTs(10_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(5));
        // idle_for (10s) would backdate past interval_start (10_000):
        // 10_005 - 10 = 9_995 < 10_000.
        let effects = send(
            &mut tracker,
            SourceEvent::UserIdle {
                idle_for: Duration::from_secs(10),
            },
            &clock,
        );

        assert_eq!(
            effects,
            vec![
                Effect::Diagnostic(
                    "backdating an AFK close past its interval start; clamped end to start (10000)"
                        .to_string()
                ),
                Effect::Transition {
                    at: WallTs(10_000),
                    open: sentinel_open(IntervalState::Afk),
                },
            ]
        );
    }

    // ---- Phase 6, tasks 6.4/6.5: RF-30 title debounce ----

    #[test]
    fn title_flicker_within_debounce_discards_first_pending_title_and_restarts_timer() {
        let clock = FakeClock::new(WallTs(1_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("kitty", "zsh", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        let first_arm = send(
            &mut tracker,
            SourceEvent::TitleChanged(safe_title("T1")),
            &clock,
        );
        assert_eq!(first_arm.len(), 1);
        assert!(matches!(
            first_arm[0],
            Effect::ArmTimer(Timer::TitleDebounce, _)
        ));

        clock.advance(Duration::from_millis(300));
        let flicker_effects = send(
            &mut tracker,
            SourceEvent::TitleChanged(safe_title("T2")),
            &clock,
        );
        assert_eq!(flicker_effects.len(), 2);
        assert!(matches!(
            flicker_effects[0],
            Effect::CancelTimer(Timer::TitleDebounce)
        ));
        assert!(matches!(
            flicker_effects[1],
            Effect::ArmTimer(Timer::TitleDebounce, _)
        ));

        // The debounce elapsing now finalizes T2, not the discarded T1 — no
        // transition was ever recorded for T1's brief window.
        clock.advance(Duration::from_millis(2000));
        let effects = send(
            &mut tracker,
            SourceEvent::DeadlineElapsed(Timer::TitleDebounce),
            &clock,
        );
        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(1_003),
                open: NewInterval {
                    app: "kitty".to_string(),
                    title: "T2".to_string(),
                    pid: None,
                    state: IntervalState::Active,
                },
            }]
        );
    }

    // ---- Phase 6, tasks 6.6/6.7: RF-23 destruction safety net ----

    #[test]
    fn destroy_notify_followed_by_a_prompt_new_active_window_cancels_the_grace_deadline() {
        let clock = FakeClock::new(WallTs(20_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        let arm_effects = send(&mut tracker, SourceEvent::ActiveWindowDestroyed, &clock);
        assert_eq!(arm_effects.len(), 1);
        assert!(matches!(
            arm_effects[0],
            Effect::ArmTimer(Timer::DestroyGrace, _)
        ));

        clock.advance(Duration::from_millis(100));
        let effects = send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("kitty", "zsh", None))),
            &clock,
        );

        // No gap is recorded for the 100ms between DestroyNotify and the new
        // window: the deadline is cancelled and a single ordinary transition
        // closes `firefox` and opens `kitty`, both at the same instant.
        assert_eq!(
            effects,
            vec![
                Effect::CancelTimer(Timer::DestroyGrace),
                Effect::Transition {
                    at: WallTs(20_001),
                    open: NewInterval {
                        app: "kitty".to_string(),
                        title: "zsh".to_string(),
                        pid: None,
                        state: IntervalState::Active,
                    },
                },
            ]
        );
        assert_eq!(tracker.next_deadline(), None);
    }

    #[test]
    fn destroy_grace_elapsing_closes_at_the_destroy_notify_instant_not_the_deadline_fire_instant() {
        let clock = FakeClock::new(WallTs(30_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        send(&mut tracker, SourceEvent::ActiveWindowDestroyed, &clock);

        // The grace period elapses with nothing new arriving (a lax WM under
        // `kill -9`): `now` at the moment the deadline fires is later than
        // the original DestroyNotify instant.
        clock.advance(Duration::from_millis(250));
        let effects = send(
            &mut tracker,
            SourceEvent::DeadlineElapsed(Timer::DestroyGrace),
            &clock,
        );

        assert_eq!(
            effects,
            vec![Effect::Transition {
                // The DestroyNotify instant (20_001 -> 30_001 here), NOT the
                // deadline-fire instant (30_001 + 250ms, truncated to 30_001
                // by FakeClock's whole-second wall granularity — see the
                // next assertion for a case where they visibly differ).
                at: WallTs(30_001),
                open: sentinel_open(IntervalState::Unknown),
            }]
        );
    }

    #[test]
    fn destroy_grace_close_uses_the_original_instant_even_when_the_deadline_fires_a_full_second_later(
    ) {
        let clock = FakeClock::new(WallTs(40_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(window("firefox", "t", None))),
            &clock,
        );

        clock.advance(Duration::from_secs(1));
        send(&mut tracker, SourceEvent::ActiveWindowDestroyed, &clock);

        // A full extra second passes before the reactor actually delivers
        // the fired deadline (e.g. it was busy draining other fds first).
        clock.advance(Duration::from_secs(1));
        let effects = send(
            &mut tracker,
            SourceEvent::DeadlineElapsed(Timer::DestroyGrace),
            &clock,
        );

        assert_eq!(
            effects,
            vec![Effect::Transition {
                at: WallTs(40_001), // the DestroyNotify instant, not 40_002.
                open: sentinel_open(IntervalState::Unknown),
            }]
        );
    }

    // ---- Phase 8, tasks 8.3/8.4: SafeTitle equality collapses hidden
    // titles (design §2 D-7's named consequence, DR-5) ----

    /// D-7's named consequence: the tracker compares `SafeTitle`s, so two
    /// different raw titles that both sanitize to `[hidden]` are *equal*,
    /// and RF-3 produces no transition between them — an excluded app
    /// switching between hidden titles yields one continuous interval, not
    /// several. Both `SafeTitle`s are obtained through the real
    /// `Excluder::evaluate` boundary (there is no other way to construct
    /// one — `SafeTitle::from_sanitized` is private to `exclude.rs`).
    ///
    /// This does not compile against `SourceEvent::TitleChanged(String)` /
    /// `WindowInfo { title: String, .. }` (Phase 5/6's shape) — passing a
    /// `SafeTitle` where a `String` is expected is a type error. That
    /// compile failure IS this task's RED: task 8.4's GREEN is the
    /// `String` -> `SafeTitle` swap that makes it compile.
    #[test]
    fn excluded_app_switching_between_hidden_titles_yields_one_continuous_interval() {
        let excluder = crate::exclude::Excluder::from_toml_str(
            r#"
            [[exclude]]
            app = "keepassxc"
            "#,
        )
        .expect("valid config compiles");

        let hidden_t1 = excluder
            .evaluate(
                "keepassxc",
                crate::exclude::RawTitle::new("KeePassXC - workVault.kdbx"),
            )
            .title;
        let hidden_t2 = excluder
            .evaluate(
                "keepassxc",
                crate::exclude::RawTitle::new("KeePassXC - personalVault.kdbx"),
            )
            .title;
        // Different raw titles, same sanitized result — the premise DR-5
        // relies on; asserted explicitly rather than assumed.
        assert_eq!(hidden_t1, hidden_t2);

        let clock = FakeClock::new(WallTs(50_000));
        let mut tracker = Tracker::new();
        send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(WindowInfo {
                app_id: "keepassxc".to_string(),
                title: hidden_t1,
                pid: None,
            })),
            &clock,
        );

        clock.advance(Duration::from_secs(5));
        let effects = send(&mut tracker, SourceEvent::TitleChanged(hidden_t2), &clock);

        assert_eq!(
            effects,
            Vec::new(),
            "two different raw titles that both sanitize to [hidden] must compare \
             equal, so no transition (and no debounce timer) is armed at all"
        );
    }
}
