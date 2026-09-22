//! `x11rb` capture; EWMH verification + `GetInputFocus` fallback (RF-24); BadWindow race (RF-22);
//! `SYNC`/`IDLETIME` alarms and the RF-25 degradation chain; XWayland warning (RF-29); title decode
//! by atom type + 512-char truncation + `/proc/<pid>/comm` fallback (RF-31); reconnect backoff (RF-32).
//! Emits `RawTitle`.
//!
//! **Phase 9 scope (slice 1 of 3).** Connection + EWMH verification/fallback (RF-24),
//! `_NET_ACTIVE_WINDOW` subscription (RF-1), the unconditional post-change property read (RF-1,
//! RF-2), and the XWayland startup warning (RF-29).
//!
//! **Phase 10 scope (slice 2 of 3, tasks 10.1-10.13).** RF-22's `BadWindow` race, tolerated at
//! all three points it can occur — `retarget_subscription`'s `ChangeWindowAttributes(...)
//! .check()`, the metadata `GetProperty` that follows it in `apply_active_window`, and the
//! `read_title` a queued title `PropertyNotify` triggers in `poll_for_event` — as a valid
//! transition, never a propagated error. `DestroyNotify` for the tracked
//! window surfaces as `RawEvent::ActiveWindowDestroyed` (RF-23's raw half; the 250 ms grace
//! timer and `unknown`-state transition are `tracker.rs`'s, Phase 6). `PropertyNotify` for a
//! title change on the tracked window surfaces as `RawEvent::TitleChanged`, undebounced
//! (RF-30's raw half; the debounce state machine is exclusively `tracker.rs`'s). `read_title`
//! decodes by the reply's actual atom type — `UTF8_STRING` as UTF-8, anything else (`STRING`
//! included) as Latin-1 — then truncates to 512 characters with a trailing ellipsis
//! (`decode_property_text`, `truncate_with_ellipsis`). `read_app_id` gains the
//! `WM_CLASS` → `/proc/<pid>/comm` → `"?"` fallback chain (`read_process_comm`,
//! `sanitize_comm`), best-effort throughout: a nonexistent, exited, or recycled pid, a `comm`
//! containing control characters, or invalid UTF-8 in `comm` all fall through to the sentinel,
//! never an error (RF-31, design §7's "Process integration — subprocess inputs" row).
//!
//! **Corrections applied 2026-09-17 (tasks 9.14-9.23), after an adversarial verification
//! pass.** RF-24's verification no longer asks the X server whether two atom *names* exist —
//! a server-wide table that any client populates and that outlives all of them, and which
//! therefore said "compliant" on a display with no window manager at all. It reads the root
//! window's `_NET_SUPPORTED` and probes `_NET_SUPPORTING_WM_CHECK` for liveness instead; see
//! `X11Source::verify_ewmh` and PRD.md RF-24's correction note. `CaptureMode::
//! InputFocusFallback` now has behaviour behind it (`poll_input_focus`) rather than being a
//! label. `poll_for_event` drains to empty instead of stopping at the first event it does not
//! translate, which was silently losing wakeups. `WM_CLASS` is sanitized at the point of
//! capture, property reads are length-bounded, and the tracked-window subscription moves
//! rather than accumulating.
//!
//! **Corrections applied 2026-09-17 (tasks 10.14-10.18), after a second adversarial pass.**
//! Four defects Phase 10 was green over; each is explained at the site it corrects.
//!
//! **Phase 11 scope (slice 3 of 3, tasks 11.1-11.9).** RF-4's `SYNC`/`IDLETIME` alarm
//! (`try_sync_idle`, `on_sync_alarm_notify`) surfacing `RawEvent::UserIdle`/`UserActive`;
//! RF-25's three-step degradation chain — `SYNC` first, `MIT-SCREEN-SAVER` polling second
//! (`poll_screensaver_idle`, driven externally at `SCREENSAVER_POLL_INTERVAL` exactly like
//! `poll_input_focus`'s own cadence note), X11 absence detection disabled third, exactly one
//! startup warning either way (`init_idle_detection`, `select_degradation_diagnostic`); and
//! RF-6/RF-32's reconnect backoff (`ReconnectBackoff`, `Reconnector`) with the "single
//! `unknown` interval per outage" bookkeeping.
//!
//! **A-7 correctness fix, found empirically (2026-09-17), not assumed.** A SYNC alarm with a
//! `Transition` test type fires only on the *edge* crossing its trigger value — confirmed
//! against a real Xvfb, not read from documentation. If `IDLETIME` is already on the far side
//! of `afk_threshold_seconds` at the instant the alarm is armed (the realistic "daemon starts
//! while the user is already away" case, and unavoidable in a from-cold-boot Xvfb test
//! environment where `IDLETIME` free-runs from server start), the edge has already happened
//! and a plain `PositiveTransition` alarm would never fire on its own — the daemon would wait
//! forever for a crossing that already occurred. `try_sync_idle` and `on_sync_alarm_notify`
//! both close this gap through `close_arm_time_gap`: a single `sync_query_counter` read tied
//! to the arm/re-arm action itself (`crosses_armed_test`), synthesizing the missed transition
//! immediately when needed. This is a one-shot check at the instant of arming, never a
//! periodic poll between alarms — the idle-detection "No polling while waiting for idle or
//! activity" scenario is unaffected.
//! A second empirical finding: `ChangeAlarm`/`CreateAlarm` both answer `BadMatch` unless
//! `delta` is supplied alongside `test_type` (even as zero) — every call site here sets it
//! explicitly. A third: re-arming by destroying and recreating the alarm resource (rather than
//! `ChangeAlarm` on the same id) produces a spurious immediate `AlarmNotify` against an
//! undefined baseline; `on_sync_alarm_notify` therefore always uses `ChangeAlarm`.
//!
//! **Corrections applied 2026-09-17 (tasks 11.10-11.18), after a third adversarial pass.**
//! The suite was green over an RF-4 defect that loses the idle->active transition outright.
//!
//! 1. The A-7 paragraph above described `on_sync_alarm_notify` as closing the arm-time gap.
//!    It did not: it re-armed the opposite edge and read nothing. When the user came back
//!    while the positive `AlarmNotify` was still queued — the ordinary case, since the
//!    notification and the input race each other — the negative edge was already in the past
//!    at the moment it was armed, and a `Transition` alarm whose condition is already true
//!    reports nothing at all. Both arming sites now share `close_arm_time_gap`, which reads
//!    the counter **after** arming, precisely so that input landing in the gap produces a
//!    duplicate event rather than a lost one.
//!
//!    That was only half of it. The other half is inside the server: an alarm armed at
//!    exactly the value the counter currently holds is never woken for the crossing that
//!    follows. The away alarm fires at exactly the threshold, so re-arming the return edge
//!    at that same value at that instant is the losing case. Measured against the real code
//!    path, 120 trials of "go away, come straight back": **8 returns lost (6.7%)** as the
//!    code stood, 7 with the arm-time read added, and **0** once `alarm_trigger_value` moved
//!    the return edge to a comparison one millisecond below the threshold. With the default
//!    240s threshold each loss is hours of active time recorded as away.
//! 2. `on_sync_alarm_notify` classified the transition from its own `armed` field. A queued
//!    `AlarmNotify` outlives the `ChangeAlarm` that follows it, so one arming's event was
//!    read under the next one's rules, fabricating a `UserActive` while the user was idle.
//!    The event's own `counter_value` is now the only classifier.
//! 3. RF-25's degraded steps had no behavioural coverage, justified by a claim about this
//!    environment's Xvfb that was simply false; `tests/x11_integration.rs` covers both
//!    degraded steps against genuine extension absence and its module doc records what the
//!    server really does and does not allow.
//!
//! **Module boundary (task 9.13, design §1 layering).** `tracker.rs` never names an `x11rb`
//! type and this module never names a `tracker` type. What crosses out of this module is
//! `RawTitle` (`crate::exclude`, D-7 — sanitization has NOT happened yet) plus this module's own
//! owned `RawEvent`/`RawWindowInfo` types, never a raw `x11rb::protocol::xproto` value. Turning a
//! `RawEvent` into a real `tracker::SourceEvent` means running its `RawTitle` through
//! `Excluder::evaluate` first (D-7's "in the source adapter, before an event is ever handed to
//! the tracker") — that composition is `reactor.rs`'s job (Phase 14, design §5's `WindowSource`
//! note: "Implemented by `ReactorSource` (Phase 14) in production").
//!
//! **Deviation from the tasks.md runtime harness, documented rather than silent.** The Suggested
//! Work Units table names "Xvfb + openbox `--sm-disable`" as PR 9's runtime harness. `openbox` is
//! not installed in this environment and installing it needs interactive `sudo` this session does
//! not have. `tests/x11_integration.rs` therefore uses a `FakeWm` test helper — a second, plain
//! `x11rb` connection that does exactly what an EWMH window manager does for the properties this
//! module reads — rather than a real `openbox` process. It is still a real Xvfb E2E test over the
//! real X11 wire protocol (RF-24's actual requirement), not a synthetic/mocked connection.
//! Recorded here and in the apply-progress artifact as a risk for the maintainer to accept, or to
//! swap back to real `openbox` once it can be installed.

use std::env;
use std::fmt;
use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::sync::{
    self, ChangeAlarmAux, ConnectionExt as _, CreateAlarmAux, Int64, TESTTYPE, VALUETYPE,
};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window,
};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

use crate::exclude::RawTitle;

/// `WM_CLASS` absent, and either `_NET_WM_PID` is also absent or its `/proc/<pid>/comm`
/// could not be read (task 10.9-10.12, RF-31's final fallback).
const APP_ID_SENTINEL: &str = "?";

/// RF-31's stored-title character limit. Character-counted, never byte-counted: a byte-index
/// truncation can split a multi-byte UTF-8 codepoint in half.
const MAX_TITLE_CHARS: usize = 512;

/// Bound on the root window's `_NET_SUPPORTED` read, in `GetProperty`'s 32-bit units — 512
/// atoms, 2 KiB. Real window managers advertise on the order of 60-90 hints, so this is a wide
/// margin, but it is a *bound*: every property this module reads is length-limited so a
/// hostile or broken property cannot make the daemon allocate without limit.
const MAX_SUPPORTED_ATOMS: u32 = 512;

/// Bound on each title read, in `GetProperty`'s 32-bit units — 512 units, 2048 bytes. That is
/// the widest UTF-8 encoding of RF-31's 512-character limit (4 bytes per character), so the
/// bound never costs a character the requirement says to keep, while making the read finite.
/// The truncation to `MAX_TITLE_CHARS` with a trailing ellipsis happens after decoding
/// (`truncate_with_ellipsis`), since this bound is in bytes and the limit is in characters.
const MAX_TITLE_UNITS: u32 = 512;

/// Bound on the `WM_CLASS` read, in `GetProperty`'s 32-bit units — 128 units, 512 bytes.
/// `WM_CLASS` holds two short identifiers; this is generous for both.
const MAX_WM_CLASS_UNITS: u32 = 128;

/// Maximum `QueryTree` hops when resolving a focused window to its top-level ancestor. A sane
/// window tree is a handful deep; the bound is what stops a malformed or adversarial tree from
/// spinning this walk.
const MAX_TOPLEVEL_WALK: usize = 32;

/// `GetInputFocus` reply sentinels from the core X11 protocol. `None` means no window has the
/// input focus; `PointerRoot` means focus follows the pointer, so no single window owns it.
/// Both are "no focused window" for RF-24's degraded path.
const FOCUS_NONE: Window = 0;
const FOCUS_POINTER_ROOT: Window = 1;

/// RF-4's default `afk_threshold_seconds` — overridden by `connect_with_afk_threshold` once
/// config.rs (a later phase) wires the real value through.
const DEFAULT_AFK_THRESHOLD: Duration = Duration::from_secs(240);

/// RF-25 step 2's fixed polling cadence over `MIT-SCREEN-SAVER`. Like
/// `poll_input_focus`'s own cadence, arming a timer at this interval is the caller's job
/// (Phase 14's reactor) — this constant is the contract between the two.
pub const SCREENSAVER_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// RF-32's backoff sequence: 500ms, 1s, 2s, 4s, 8s, then this ceiling forever.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
const RECONNECT_CEILING: Duration = Duration::from_secs(16);

/// RF-32's jitter bound, in percent either side of the unjittered delay.
const RECONNECT_JITTER_PERCENT: i64 = 20;

/// RF-25 step 2's exact degradation diagnostic — a `const` so the message tested by
/// `select_degradation_diagnostic`'s unit tests and the one actually emitted by
/// `init_idle_detection` cannot drift apart.
const SCREENSAVER_DEGRADATION_DIAGNOSTIC: &str =
    "xwindowlog: SYNC/IDLETIME unavailable — degrading absence detection to a 30s \
     MIT-SCREEN-SAVER poll";

/// RF-25 step 3's exact diagnostic, emitted exactly once at startup when neither extension is
/// available.
const IDLE_DETECTION_DISABLED_DIAGNOSTIC: &str =
    "xwindowlog: neither SYNC/IDLETIME nor MIT-SCREEN-SAVER is available — X11 absence \
     detection is disabled; relying solely on logind session signals";

/// Errors connecting to or initializing the X11 capture layer.
#[derive(Debug)]
pub enum X11InitError {
    Connect(ConnectError),
    Protocol(ReplyError),
    Io(ConnectionError),
    /// `connect_bounded` could not spawn the thread it runs the connect attempt on (T1,
    /// RF-32) — mirrors `logind::connect_bounded`'s own spawn-failure variant.
    ReconnectSpawn(String),
    /// `connect_bounded` exceeded its timeout (T1, RF-32): the peer accepted the connection
    /// and then stalled, exactly the failure class `logind::connect_bounded` was already
    /// built to guard against.
    ReconnectTimeout,
}

impl From<ConnectError> for X11InitError {
    fn from(err: ConnectError) -> Self {
        X11InitError::Connect(err)
    }
}

impl From<ReplyError> for X11InitError {
    fn from(err: ReplyError) -> Self {
        X11InitError::Protocol(err)
    }
}

impl From<ConnectionError> for X11InitError {
    fn from(err: ConnectionError) -> Self {
        X11InitError::Io(err)
    }
}

impl fmt::Display for X11InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            X11InitError::Connect(e) => write!(f, "X11 connection failed: {e}"),
            X11InitError::Protocol(e) => write!(f, "X11 protocol error: {e}"),
            X11InitError::Io(e) => write!(f, "X11 connection error: {e}"),
            X11InitError::ReconnectSpawn(e) => {
                write!(f, "failed to spawn X11 reconnect thread: {e}")
            }
            X11InitError::ReconnectTimeout => {
                write!(f, "X11 reconnect attempt timed out")
            }
        }
    }
}

impl std::error::Error for X11InitError {}

/// How the daemon is determining the active window (RF-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// A live window manager advertises `_NET_ACTIVE_WINDOW` in the root window's
    /// `_NET_SUPPORTED` and passes the `_NET_SUPPORTING_WM_CHECK` liveness probe: subscribe to
    /// `PropertyNotify` and wait, per the "Event-driven active-window subscription"
    /// requirement (RF-1) — no polling. See `X11Source::verify_ewmh`.
    Ewmh,
    /// No live EWMH-compliant window manager: approximate the active window with periodic
    /// `GetInputFocus` queries instead of waiting for events this window manager will never
    /// send (task 9.4). See `X11Source::poll_input_focus` for the polling itself, which is
    /// what makes this a real degradation rather than a label.
    InputFocusFallback,
}

/// Metadata for a window as X11 handed it to us — **not yet sanitized**. `title` is `RawTitle`
/// (design §2 D-7); turning this into the tracker's `SafeTitle`-typed `WindowInfo` is the
/// reactor's job (Phase 14), by running it through `Excluder::evaluate`, never this module's.
#[derive(Debug)]
pub struct RawWindowInfo {
    pub app_id: String,
    pub title: RawTitle,
    pub pid: Option<u32>,
}

/// This module's boundary event type (task 9.13). Phase 9's slice of the eventual
/// `tracker::SourceEvent` shape, with `RawTitle` standing in for `SafeTitle` — see the
/// module-level boundary note. Later phases (10, 11) add more variants (`ActiveWindowDestroyed`,
/// `TitleChanged`, `UserIdle`/`UserActive`, `DisplayLost`/`DisplayRestored`).
#[derive(Debug)]
pub enum RawEvent {
    /// `None` = desktop focused (window-capture "Desktop focus is legitimate activity") — the
    /// caller decides the `"(desktop)"` sentinel, matching how `tracker::WindowInfo::desktop()`
    /// already models it; this module carries no window-capture domain sentinel of its own.
    ActiveWindow(Option<RawWindowInfo>),
    /// `DestroyNotify` for the window this module was actively tracking (RF-23, task
    /// 10.5/10.6). Carries no timestamp of its own: the wall-clock instant the safety net's
    /// 250 ms grace period measures from is the caller's job — it must stamp `now` at the
    /// moment it receives this event (see `tracker::PendingTimer::DestroyGrace`'s
    /// `destroyed_at`, which is exactly that stamp). This module's job stops at noticing and
    /// translating; the timer and the `unknown`-state transition are entirely `tracker.rs`'s
    /// (Phase 6, tasks 6.6-6.7).
    ActiveWindowDestroyed,
    /// RF-30: a title change on the tracked window, surfaced exactly as X11 reported it —
    /// **undebounced** (task 10.7/10.8). The debounce state machine lives exclusively in
    /// `tracker.rs` (design §2 D-8); this module's only job is to notice the property
    /// changed and hand back a fresh, unconditional read, never to decide whether the change
    /// is "stable" long enough to matter.
    TitleChanged(RawTitle),
    /// RF-4: the idle threshold was crossed upward. `idle_for` is `ms_since_user_input` read
    /// exactly once, at the instant that mattered — either a real alarm fire
    /// (`on_sync_alarm_notify`), the 30s screensaver poll (`poll_screensaver_idle`), or the
    /// one-shot "already past threshold" check tied to arming the alarm (`try_sync_idle`'s
    /// doc, and the module doc's A-7 note).
    ///
    /// **Documented deviation (task 11.17).** The requirement words `ms_since_user_input` as
    /// coming from `XScreenSaverQueryInfo`. Only the `MIT-SCREEN-SAVER` path actually reads
    /// it there. On the `SYNC` path the same quantity comes from the `AlarmNotify`'s own
    /// `counter_value` (or, at arm time, from `sync_query_counter` on `IDLETIME`): the same
    /// millisecond count of elapsed input-free time, taken from the counter the alarm itself
    /// is defined over, with no extra round trip and no second extension required. Reading
    /// `XScreenSaverQueryInfo` instead would answer a *different* question at a *later*
    /// instant than the one the alarm fired on.
    UserIdle { idle_for: Duration },
    /// RF-4's negative transition: input resumed.
    UserActive,
    /// RF-6/RF-32: the X11 connection was lost. Not constructed anywhere in this module —
    /// `poll_for_event`'s `?` propagates the underlying `ReplyError` on connection loss (see
    /// `is_bad_window`'s doc: "connection loss MUST propagate so the reconnect path can
    /// run"), and translating that into this variant, exactly once per outage, is
    /// `Reconnector`'s contract with its caller (its own doc).
    DisplayLost,
    /// RF-6/RF-32: reconnection succeeded. Same construction note as `DisplayLost`.
    DisplayRestored,
}

/// How this source is currently detecting user absence (RF-4/RF-25's three-step
/// degradation chain). Not exposed directly — `X11Source::mode`-style introspection wasn't
/// needed by any caller this phase, so this stays private; add an accessor if one appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleMode {
    /// RF-4's primary path: an alarm on the `SYNC` extension's `IDLETIME` system counter.
    /// `armed` names which edge the alarm is currently watching for — flips every time it
    /// fires (`on_sync_alarm_notify`) or every time the one-shot arm-time check
    /// (`crosses_armed_test`) finds the condition already true.
    Sync {
        alarm: sync::Alarm,
        counter: sync::Counter,
        threshold_ms: u32,
        armed: TESTTYPE,
    },
    /// RF-25 step 2: no event stream, so the caller drives `poll_screensaver_idle` on its own
    /// `SCREENSAVER_POLL_INTERVAL` cadence — the same "this module doesn't own the timer"
    /// shape as `CaptureMode::InputFocusFallback`/`poll_input_focus`.
    ScreenSaverPolling { threshold_ms: u32, was_idle: bool },
    /// RF-25 step 3: neither extension is available. `poll_screensaver_idle` and the `SYNC`
    /// alarm branch of `poll_for_event` are both no-ops in this mode.
    Disabled,
}

/// Atoms this module resolves once per connection (task 9.2, 9.7-9.10).
///
/// Every name here is interned with `only_if_exists = false`, which creates the name if it is
/// not already in the server's table. That is correct **because no decision is made from the
/// existence of a name.** An atom id is only ever used here as the key to read a property
/// with; RF-24's compliance decision is made from the *contents of properties on the root
/// window*, which only a running window manager writes. Interning with `only_if_exists = true`
/// and treating atom `0` as "not compliant" is exactly the superseded mechanism — see
/// `verify_ewmh` and PRD.md RF-24's 2026-09-17 correction note.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    net_supported: u32,
    net_supporting_wm_check: u32,
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    wm_name: u32,
    wm_class: u32,
    utf8_string: u32,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, ReplyError> {
        let net_supported = conn.intern_atom(false, b"_NET_SUPPORTED")?;
        let net_supporting_wm_check = conn.intern_atom(false, b"_NET_SUPPORTING_WM_CHECK")?;
        let net_active_window = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?;
        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?;
        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?;
        let wm_name = conn.intern_atom(false, b"WM_NAME")?;
        let wm_class = conn.intern_atom(false, b"WM_CLASS")?;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?;
        Ok(Atoms {
            net_supported: net_supported.reply()?.atom,
            net_supporting_wm_check: net_supporting_wm_check.reply()?.atom,
            net_active_window: net_active_window.reply()?.atom,
            net_wm_name: net_wm_name.reply()?.atom,
            net_wm_pid: net_wm_pid.reply()?.atom,
            wm_name: wm_name.reply()?.atom,
            wm_class: wm_class.reply()?.atom,
            utf8_string: utf8_string.reply()?.atom,
        })
    }
}

/// The X11 capture engine. Not a `WindowSource` itself (design §5's `WindowSource` note): the
/// reactor's `ReactorSource` (Phase 14) composes this with `exclude.rs` to produce real
/// `tracker::SourceEvent`s.
pub struct X11Source {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    mode: CaptureMode,
    active_window: Option<Window>,
    drained_untranslated: u64,
    idle_mode: IdleMode,
    /// A `RawEvent` synthesized at arm/re-arm time rather than read from the X11 socket (the
    /// module doc's A-7 note) — drained by `poll_for_event` before it touches the connection
    /// at all, so it is surfaced exactly once and never blocks the drain-before-poll
    /// invariant (D-6) it sits in front of.
    pending_idle_event: Option<RawEvent>,
}

impl X11Source {
    /// Connects, resolves atoms, verifies EWMH compliance (RF-24), and subscribes the root
    /// window when compliant.
    ///
    /// Startup diagnostics are **written to stderr here**, by `emit_startup_diagnostics`, as
    /// soon as the condition that produced them is detected — RF-24 and RF-29 both require an
    /// emitted diagnostic, and deferring that to a caller meant it reached nobody, because the
    /// only thing that will eventually call `connect` is a `reactor.rs` that is still a Phase
    /// 14 stub. They are also returned, so callers and tests can inspect what was emitted.
    /// This mirrors `tracker::Effect::Diagnostic`'s "stderr only, never a persisted value"
    /// contract, restated here as plain strings since this module predates the tracker
    /// boundary.
    pub fn connect(display: Option<&str>) -> Result<(Self, Vec<String>), X11InitError> {
        Self::connect_with_afk_threshold(display, DEFAULT_AFK_THRESHOLD)
    }

    /// Same as `connect`, with RF-4's `afk_threshold_seconds` explicit rather than
    /// `DEFAULT_AFK_THRESHOLD` — the constructor a future `config.rs` (and this phase's own
    /// tests, which need a threshold short enough to observe without a multi-minute wait) use
    /// instead of `connect`. Mirrors `tracker::Tracker::with_title_debounce`'s
    /// default-plus-override shape.
    pub fn connect_with_afk_threshold(
        display: Option<&str>,
        afk_threshold: Duration,
    ) -> Result<(Self, Vec<String>), X11InitError> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = Atoms::intern(&conn)?;

        let mut diagnostics = Vec::new();
        let mode = if Self::verify_ewmh(&conn, root, &atoms)? {
            CaptureMode::Ewmh
        } else {
            diagnostics.push(
                "xwindowlog: window manager does not appear to be EWMH-compliant \
                 (the root window does not advertise _NET_ACTIVE_WINDOW in _NET_SUPPORTED, \
                 or no live _NET_SUPPORTING_WM_CHECK window exists); falling back to \
                 GetInputFocus polling"
                    .to_string(),
            );
            CaptureMode::InputFocusFallback
        };
        if let Some(warning) = xwayland_warning() {
            diagnostics.push(warning);
        }

        let (idle_mode, pending_idle_event, idle_diagnostic) =
            Self::init_idle_detection(&conn, afk_threshold);
        if let Some(diagnostic) = idle_diagnostic {
            diagnostics.push(diagnostic);
        }

        // RF-24/RF-25/RF-29: emit now, at the moment the condition is detected. Failing to
        // write to stderr must not stop the daemon from capturing, so the result is
        // deliberately ignored rather than turned into a startup error.
        let _ = emit_startup_diagnostics(&diagnostics, &mut io::stderr());

        let source = X11Source {
            conn,
            root,
            atoms,
            mode,
            active_window: None,
            drained_untranslated: 0,
            idle_mode,
            pending_idle_event,
        };
        if source.mode == CaptureMode::Ewmh {
            source.subscribe_root()?;
        }
        Ok((source, diagnostics))
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    /// The connection's raw fd, for a Phase 15 composition adapter to register in the
    /// reactor's `poll(2)` set (design §2 D-2's fd0). `RustConnection<DefaultStream>`'s stream
    /// implements `AsRawFd` on unix (verified: `x11rb-0.13.2/src/rust_connection/stream.rs`);
    /// this is the one place that reaches through `.stream()` to expose it, so no other module
    /// needs to know `x11rb`'s stream type at all.
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.conn.stream().as_raw_fd()
    }

    /// Flushes any outbound requests before this wakeup's drain (design §2 D-6: "outbound
    /// requests must actually leave" — a `SYNC` alarm re-arm or an EWMH probe sent earlier this
    /// iteration must reach the server even when nothing new arrived to read).
    pub fn flush(&self) -> Result<(), ConnectionError> {
        self.conn.flush()
    }

    /// How many X11 events this source has drained without translating them into a
    /// `RawEvent` — the "event I did not translate" half of `poll_for_event`'s outcomes,
    /// which its `Option` return type collapses into the same `None` as "queue empty".
    ///
    /// It exists because that distinction is load-bearing and was otherwise unobservable:
    /// the drain-before-poll invariant (D-6) is only true if `Ok(None)` means the queue is
    /// genuinely empty, and a subscription leak shows up here as events nobody asked for.
    /// Both are asserted in `tests/x11_integration.rs`. Cheap enough to keep in production
    /// as a diagnostic counter.
    pub fn drained_untranslated(&self) -> u64 {
        self.drained_untranslated
    }

    /// RF-25's three-step degradation chain, attempted in order. Every probe is best-effort:
    /// `try_sync_idle` swallows any failure (extension absent, no `IDLETIME` counter, the
    /// alarm request itself rejected) and reports unavailability rather than propagating an
    /// error, because RF-25 requires none of the three outcomes to block startup. The
    /// diagnostic text itself comes from `select_degradation_diagnostic`, kept separate so
    /// its selection logic is unit-testable without a real X11 connection.
    fn init_idle_detection(
        conn: &RustConnection,
        afk_threshold: Duration,
    ) -> (IdleMode, Option<RawEvent>, Option<String>) {
        let threshold_ms = threshold_millis(afk_threshold);
        if let Some((mode, pending)) = Self::try_sync_idle(conn, threshold_ms) {
            return (mode, pending, select_degradation_diagnostic(true, false));
        }
        let screensaver_available = Self::screensaver_present(conn);
        let mode = if screensaver_available {
            IdleMode::ScreenSaverPolling {
                threshold_ms,
                was_idle: false,
            }
        } else {
            IdleMode::Disabled
        };
        (
            mode,
            None,
            select_degradation_diagnostic(false, screensaver_available),
        )
    }

    /// RF-4/A-7: attempts the `SYNC`/`IDLETIME` path end to end — extension init, locating
    /// the `IDLETIME` system counter, and creating the `PositiveTransition` alarm. `None` on
    /// **any** failure at any step, which `init_idle_detection` reads as "step 1
    /// unavailable, fall through to step 2" (RF-25), never as a daemon failure. `delta` is
    /// set explicitly (even at zero) on every alarm request in this module — omitting it
    /// answers `BadMatch` (module doc's second empirical note).
    fn try_sync_idle(
        conn: &RustConnection,
        threshold_ms: u32,
    ) -> Option<(IdleMode, Option<RawEvent>)> {
        conn.sync_initialize(3, 1).ok()?.reply().ok()?;
        let counters = conn.sync_list_system_counters().ok()?.reply().ok()?;
        let idletime = counters
            .counters
            .iter()
            .find(|counter| counter.name == b"IDLETIME")
            .map(|counter| counter.counter)?;
        let alarm = conn.generate_id().ok()?;
        conn.sync_create_alarm(
            alarm,
            &CreateAlarmAux::new()
                .counter(idletime)
                .value_type(VALUETYPE::ABSOLUTE)
                .value(alarm_trigger_value(
                    threshold_ms,
                    TESTTYPE::POSITIVE_TRANSITION,
                ))
                .test_type(TESTTYPE::POSITIVE_TRANSITION)
                .delta(Int64 { hi: 0, lo: 0 })
                .events(1),
        )
        .ok()?
        .check()
        .ok()?;
        conn.flush().ok()?;

        let (armed, pending) = Self::close_arm_time_gap(
            conn,
            alarm,
            idletime,
            threshold_ms,
            TESTTYPE::POSITIVE_TRANSITION,
        )
        .ok()?;
        Some((
            IdleMode::Sync {
                alarm,
                counter: idletime,
                threshold_ms,
                armed,
            },
            pending,
        ))
    }

    /// Issues one `ChangeAlarm` re-arming `alarm` for `test_type`. Factored out because the
    /// request has four mandatory pieces — including `delta`, which answers `BadMatch` by
    /// its absence (module doc's second empirical note) — and three call sites that must not
    /// drift apart.
    fn change_alarm_test(
        conn: &RustConnection,
        alarm: sync::Alarm,
        threshold_ms: u32,
        test_type: TESTTYPE,
    ) -> Result<(), ReplyError> {
        conn.sync_change_alarm(
            alarm,
            &ChangeAlarmAux::new()
                .value_type(VALUETYPE::ABSOLUTE)
                .value(alarm_trigger_value(threshold_ms, test_type))
                .test_type(test_type)
                .delta(Int64 { hi: 0, lo: 0 }),
        )?
        .check()?;
        conn.flush()?;
        Ok(())
    }

    /// The module doc's A-7 check, shared by the two places an alarm is armed
    /// (`try_sync_idle` at startup, `on_sync_alarm_notify` on every transition). `test_type`
    /// is the edge that was **just armed**; the answer is the edge that ends up armed, plus
    /// the transition to synthesize when the arming missed one.
    ///
    /// A `Transition` alarm fires on the crossing and on nothing else: arming it for a
    /// condition that is already true watches for an edge that has already happened, and
    /// the server will never report it (`ChangeAlarm` whose trigger is already satisfied
    /// notifies nothing, confirmed against a real server). One `sync_query_counter` read,
    /// **after** the arm rather than before it, closes that gap in both directions:
    ///
    /// - input that lands before the arm is caught by this read, and the transition is
    ///   synthesized here;
    /// - input that lands after the arm produces the real crossing, and the alarm reports it.
    ///
    /// The read follows the arm deliberately. Reading first and arming second leaves a window
    /// in which input arrives after the read but before the arm, and that transition would be
    /// lost for good, latching the daemon in AFK until the next full idle->return cycle. In
    /// this order the same window instead produces one duplicate event, and both
    /// `Tracker::on_user_idle`/`on_user_active` already ignore a transition they are already
    /// in. A lost transition is hours of mislabelled time; a duplicate one is nothing.
    ///
    /// This check alone is not what fixes the lost return, and it was measured rather than
    /// assumed: with this check in place and the old trigger formulation, 7 of 120 returns
    /// were still lost, because the loss happens inside the server rather than in this
    /// window. `alarm_trigger_value` is the half that closes it. Both are kept — this one
    /// covers the return that arrives before the alarm is armed at all, which no trigger
    /// formulation can report.
    ///
    /// One read per arm, never a timer and never a poll — the idle-detection "No polling
    /// while waiting for idle or activity" scenario and RNF-2 both hold: between two
    /// transitions this module issues no request at all.
    fn close_arm_time_gap(
        conn: &RustConnection,
        alarm: sync::Alarm,
        counter: sync::Counter,
        threshold_ms: u32,
        test_type: TESTTYPE,
    ) -> Result<(TESTTYPE, Option<RawEvent>), ReplyError> {
        let idle_ms = int64_to_ms(&conn.sync_query_counter(counter)?.reply()?.counter_value);
        if !crosses_armed_test(idle_ms, threshold_ms, test_type) {
            return Ok((test_type, None));
        }
        let flipped = flip_test_type(test_type);
        Self::change_alarm_test(conn, alarm, threshold_ms, flipped)?;
        let synthesized = if test_type == TESTTYPE::POSITIVE_TRANSITION {
            RawEvent::UserIdle {
                idle_for: Duration::from_millis(idle_ms),
            }
        } else {
            RawEvent::UserActive
        };
        Ok((flipped, Some(synthesized)))
    }

    /// RF-25 step 2's availability probe: a real `MIT-SCREEN-SAVER` `QueryVersion` round
    /// trip, best-effort exactly like `try_sync_idle`.
    fn screensaver_present(conn: &RustConnection) -> bool {
        conn.screensaver_query_version(1, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some()
    }

    /// RF-25 step 2: the 30s `MIT-SCREEN-SAVER` poll. A no-op returning `Ok(None)` when this
    /// source isn't in `IdleMode::ScreenSaverPolling` — exactly like `poll_input_focus`, how
    /// often this is called is the caller's business (Phase 14 arms a timer at
    /// `SCREENSAVER_POLL_INTERVAL`), not this module's.
    pub fn poll_screensaver_idle(&mut self) -> Result<Option<RawEvent>, ReplyError> {
        let IdleMode::ScreenSaverPolling {
            threshold_ms,
            was_idle,
        } = self.idle_mode
        else {
            return Ok(None);
        };
        let info = self.conn.screensaver_query_info(self.root)?.reply()?;
        let idle_now = u64::from(info.ms_since_user_input) >= u64::from(threshold_ms);
        if idle_now == was_idle {
            return Ok(None);
        }
        self.idle_mode = IdleMode::ScreenSaverPolling {
            threshold_ms,
            was_idle: idle_now,
        };
        Ok(Some(if idle_now {
            RawEvent::UserIdle {
                idle_for: Duration::from_millis(u64::from(info.ms_since_user_input)),
            }
        } else {
            RawEvent::UserActive
        }))
    }

    /// RF-4: a `SyncAlarmNotify` for the alarm this source owns. `Ok(None)` for any other
    /// alarm (or when this source isn't in `IdleMode::Sync` at all) — drained like any other
    /// untranslated event by the caller. Unlike `try_sync_idle`'s best-effort probing, every
    /// request here propagates its real error: this runs only after `SYNC` idle detection is
    /// already established, so a failure here is a genuine fault (most likely connection
    /// loss), which RF-6/RF-32 need to see, not silently swallow.
    ///
    /// Always re-arms via `ChangeAlarm` on the same alarm id, never destroy-then-recreate —
    /// the module doc's third empirical note explains why the latter produces a spurious
    /// immediate `AlarmNotify`.
    fn on_sync_alarm_notify(
        &mut self,
        ev: &sync::AlarmNotifyEvent,
    ) -> Result<Option<RawEvent>, ReplyError> {
        let (alarm, counter, threshold_ms) = match self.idle_mode {
            IdleMode::Sync {
                alarm,
                counter,
                threshold_ms,
                ..
            } if alarm == ev.alarm => (alarm, counter, threshold_ms),
            _ => return Ok(None),
        };

        // Task 11.12: classify this event from ITS OWN counter value, never from the
        // `armed` field. A queued `AlarmNotify` survives a later `ChangeAlarm` — measured
        // directly — so an event generated under one arming can be read under the next one,
        // and the local field then names the wrong edge. Sweeping the startup window over
        // 500 connections produced exactly that: a `UserIdle` followed immediately by a
        // `UserActive` nobody generated, recording away-time as active. `counter_value` is
        // the server's own statement of what the counter held when the alarm fired, and it
        // cannot drift out from under the event that carries it.
        let idle_ms = int64_to_ms(&ev.counter_value);
        let fired_idle = crosses_armed_test(idle_ms, threshold_ms, TESTTYPE::POSITIVE_TRANSITION);
        // Watch for the opposite edge next. `flip_test_type` is the single place that names
        // which test each edge uses, so this never has to repeat it.
        let next_test = if fired_idle {
            flip_test_type(TESTTYPE::POSITIVE_TRANSITION)
        } else {
            TESTTYPE::POSITIVE_TRANSITION
        };

        Self::change_alarm_test(&self.conn, alarm, threshold_ms, next_test)?;
        let (armed, synthesized) =
            Self::close_arm_time_gap(&self.conn, alarm, counter, threshold_ms, next_test)?;
        self.idle_mode = IdleMode::Sync {
            alarm,
            counter,
            threshold_ms,
            armed,
        };
        // `poll_for_event` drains this slot before it touches the socket and this method
        // only runs from inside that drain, so the slot is empty here by construction.
        if synthesized.is_some() {
            self.pending_idle_event = synthesized;
        }

        Ok(Some(if fired_idle {
            RawEvent::UserIdle {
                idle_for: Duration::from_millis(idle_ms),
            }
        } else {
            RawEvent::UserActive
        }))
    }

    /// RF-24 (mechanism corrected 2026-09-17): verifies that a **live, EWMH-compliant window
    /// manager** is actually running, by reading the two root-window properties EWMH §2
    /// defines for exactly that purpose.
    ///
    /// 1. The root window's `_NET_SUPPORTED` (`ATOM[]`) must list `_NET_ACTIVE_WINDOW`. That
    ///    property is the window manager's advertisement of the hints it implements.
    /// 2. The `_NET_SUPPORTING_WM_CHECK` probe must succeed: the root window's copy names a
    ///    window the WM created, and that window's own copy must point back at itself.
    ///
    /// **Why not `intern_atom(only_if_exists = true)`, which this requirement used to
    /// prescribe.** Interning queries the X server's global atom-*name* table. That table is
    /// server-wide, is populated by any client that mentions a name (GTK3 interns
    /// `_NET_ACTIVE_WINDOW` unconditionally at startup, with or without a window manager), and
    /// outlives every client that used it. It answers "has this string been assigned a
    /// number?", never "is a window manager running?". Under Xvfb with no window manager at
    /// all, the interning check reported compliance and the daemon then blocked forever — the
    /// exact silent failure RF-24 exists to prevent. `tests/x11_integration.rs` pins all three
    /// proofs: atoms interned with no WM at all, a WM that exited after declaring support, and
    /// a live WM whose `_NET_SUPPORTED` simply omits `_NET_ACTIVE_WINDOW` — that last one is
    /// the realistic tiling-WM case, and it is the only one step 1 catches on its own.
    ///
    /// Step 2 is what makes this a *liveness* check rather than a historical one. The root
    /// window belongs to the server, so `_NET_SUPPORTED` survives the window manager that
    /// wrote it; the check window does not, and reading a property from a destroyed window
    /// answers `BadWindow` — treated here as "no live WM", not as a daemon failure.
    fn verify_ewmh(conn: &RustConnection, root: Window, atoms: &Atoms) -> Result<bool, ReplyError> {
        if !Self::root_advertises_active_window(conn, root, atoms)? {
            return Ok(false);
        }
        Self::supporting_wm_is_live(conn, root, atoms)
    }

    /// Step 1 of `verify_ewmh`: is `_NET_ACTIVE_WINDOW` listed in the root window's
    /// `_NET_SUPPORTED`? An absent property yields a zero-length reply, not an error.
    fn root_advertises_active_window(
        conn: &RustConnection,
        root: Window,
        atoms: &Atoms,
    ) -> Result<bool, ReplyError> {
        let reply = conn
            .get_property(
                false,
                root,
                atoms.net_supported,
                AtomEnum::ATOM,
                0,
                MAX_SUPPORTED_ATOMS,
            )?
            .reply()?;
        let Some(mut supported) = reply.value32() else {
            return Ok(false);
        };
        Ok(supported.any(|atom| atom == atoms.net_active_window))
    }

    /// Step 2 of `verify_ewmh`: the `_NET_SUPPORTING_WM_CHECK` liveness probe (EWMH §2).
    fn supporting_wm_is_live(
        conn: &RustConnection,
        root: Window,
        atoms: &Atoms,
    ) -> Result<bool, ReplyError> {
        let Some(check_window) = read_window_property(conn, root, atoms.net_supporting_wm_check)?
        else {
            return Ok(false);
        };
        match read_window_property(conn, check_window, atoms.net_supporting_wm_check) {
            Ok(Some(back_reference)) => Ok(back_reference == check_window),
            Ok(None) => Ok(false),
            // The window manager exited and its check window went with it. A valid answer to
            // "is a WM running?", not a daemon error.
            Err(err) if is_bad_window(&err) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Subscribes the root window to `PropertyNotify` (window-capture "Active window changes
    /// while idle" — the root half of that subscription; the tracked-window half is
    /// `subscribe_window`, driven by `on_active_window_changed`).
    fn subscribe_root(&self) -> Result<(), ReplyError> {
        self.conn
            .change_window_attributes(
                self.root,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Reads `_NET_ACTIVE_WINDOW` off the root window — RF-22's `GetProperty(_NET_ACTIVE_WINDOW)`
    /// half of the ordered read-then-register sequence. `None` means no window is focused
    /// (desktop focus, or the property is unset/zero-length).
    fn read_active_window(&self) -> Result<Option<Window>, ReplyError> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;
        if reply.value_len == 0 {
            return Ok(None);
        }
        let window = reply.value32().and_then(|mut it| it.next());
        Ok(window.filter(|&w| w != 0))
    }

    /// Subscribes `window` to `PropertyChangeMask | StructureNotifyMask` (window-capture "Active
    /// window changes while idle" — the tracked-window half). RF-22's ordering — this call MUST
    /// follow `read_active_window`, never precede it — and its BadWindow handling are Phase 10's
    /// task 10.1/10.2; this slice lets a `ReplyError::X11Error` propagate rather than treating it
    /// as a valid transition yet.
    fn subscribe_window(&self, window: Window) -> Result<(), ReplyError> {
        self.conn
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Clears this client's event mask on `window`, releasing the subscription
    /// `subscribe_window` took.
    ///
    /// Without this, every window that was ever active stayed subscribed for the daemon's
    /// whole session: an 8-hour day of window switching accumulates subscriptions
    /// monotonically, and each one keeps delivering `PropertyNotify` for every title change in
    /// a window nobody is tracking any more. That is unbounded work in the event loop for
    /// events that are all discarded.
    ///
    /// `BadWindow` here means the window is already destroyed, which is exactly the case where
    /// there is nothing left to unsubscribe from — a valid outcome, not a failure.
    fn unsubscribe_window(&self, window: Window) -> Result<(), ReplyError> {
        match self
            .conn
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            )?
            .check()
        {
            Ok(()) => {}
            Err(err) if is_bad_window(&err) => {}
            Err(err) => return Err(err),
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Moves the single tracked-window subscription from the previously active window to
    /// `window`, so exactly one window is subscribed at a time.
    ///
    /// Returns `Ok(false)` when `window` itself no longer existed by the time the subscribe
    /// request reached the server (RF-22, task 10.1/10.2's `ChangeWindowAttributes(...)
    /// .check()` half) — the caller MUST treat that as a valid transition producing no
    /// event, never propagate the error. `Ok(true)` covers every ordinary case, including
    /// retargeting to `None` (desktop focus) and the no-op "already tracking this window"
    /// case.
    ///
    /// **The new window is subscribed BEFORE the previous one is released** (task 10.15); that
    /// order is the correctness property. Releasing first left a failed retarget with
    /// `active_window` naming a window whose mask was already cleared, which the short-circuit
    /// above then never re-subscribed — killing RF-30 and RF-23 for the active window.
    /// Subscribing first makes the failure path a no-op by construction, stronger than
    /// restoring afterwards because the restore is itself a request that can fail. Both windows
    /// are briefly subscribed, costing at most one drained untranslated event.
    fn retarget_subscription(&self, window: Option<Window>) -> Result<bool, ReplyError> {
        if self.active_window == window {
            return Ok(true);
        }
        if let Some(next) = window {
            match self.subscribe_window(next) {
                Ok(()) => {}
                Err(err) if is_bad_window(&err) => return Ok(false),
                Err(err) => return Err(err),
            }
        }
        if let Some(previous) = self.active_window {
            self.unsubscribe_window(previous)?;
        }
        Ok(true)
    }

    /// The window that is active right now, determined the way this connection's
    /// `CaptureMode` permits (RF-24): the window manager's `_NET_ACTIVE_WINDOW` when one is
    /// running and compliant, `GetInputFocus` when it is not.
    fn current_window(&self) -> Result<Option<Window>, ReplyError> {
        match self.mode {
            CaptureMode::Ewmh => self.read_active_window(),
            CaptureMode::InputFocusFallback => self.read_focus_window(),
        }
    }

    /// RF-24's degraded path: approximate the active window with `GetInputFocus`, resolving
    /// the reply to the top-level ancestor a window manager would have named in
    /// `_NET_ACTIVE_WINDOW`. Toolkits routinely focus an inner sub-window, so the raw reply is
    /// not comparable with what the EWMH path reports.
    ///
    /// `None` covers "nothing focused", "focus follows the pointer" and the root window
    /// itself — all desktop focus as far as the tracker is concerned, matching what
    /// `read_active_window` reports for an unset `_NET_ACTIVE_WINDOW`.
    fn read_focus_window(&self) -> Result<Option<Window>, ReplyError> {
        let focus = self.conn.get_input_focus()?.reply()?.focus;
        if focus == FOCUS_NONE || focus == FOCUS_POINTER_ROOT || focus == self.root {
            return Ok(None);
        }
        self.toplevel_of(focus)
    }

    /// Walks up from `window` to the child of the root window that contains it. `None` if the
    /// window vanished mid-walk (a `BadWindow` here just means focus already moved on) or if
    /// the walk hit `MAX_TOPLEVEL_WALK`.
    fn toplevel_of(&self, window: Window) -> Result<Option<Window>, ReplyError> {
        let mut current = window;
        for _ in 0..MAX_TOPLEVEL_WALK {
            let tree = match self.conn.query_tree(current)?.reply() {
                Ok(tree) => tree,
                Err(err) if is_bad_window(&err) => return Ok(None),
                Err(err) => return Err(err),
            };
            if tree.parent == self.root || tree.parent == FOCUS_NONE {
                return Ok(Some(current));
            }
            current = tree.parent;
        }
        Ok(None)
    }

    /// RF-24's degraded path has no event stream to wait on: the root window is deliberately
    /// left unsubscribed, because a window manager that maintains no `_NET_ACTIVE_WINDOW` will
    /// never send the `PropertyNotify` the EWMH path waits for. So this mode queries
    /// `GetInputFocus` and reports a `RawEvent` only when the focused top-level actually
    /// changed.
    ///
    /// This is the one place in the module that polls, and it is the approximation RF-24
    /// explicitly asks for — the alternative is the silent do-nothing daemon the requirement
    /// exists to prevent. How often it is called is the reactor's business (Phase 14 arms the
    /// timer), not this module's.
    fn poll_input_focus(&mut self) -> Result<Option<RawEvent>, ReplyError> {
        let window = self.read_focus_window()?;
        if window == self.active_window {
            return Ok(None);
        }
        self.apply_active_window(window)
    }

    /// Handles one active-window change end to end (tasks 9.5-9.10): determines the active
    /// window for the current `CaptureMode`, moves the subscription to it, then performs the
    /// unconditional fresh metadata read — never reusing any value from a previous call
    /// (window-capture "Property read follows every active-window change").
    ///
    /// `Ok(None)` is RF-22's valid-transition case (task 10.1/10.2): the window named by
    /// `_NET_ACTIVE_WINDOW` no longer existed by the time the daemon tried to subscribe to
    /// it or read its metadata. Not an error, not a daemon failure — the caller simply keeps
    /// waiting for the next `_NET_ACTIVE_WINDOW` change.
    pub fn on_active_window_changed(&mut self) -> Result<Option<RawEvent>, ReplyError> {
        let window = self.current_window()?;
        self.apply_active_window(window)
    }

    /// The shared tail of both paths into an active-window transition: retarget the
    /// subscription, record the new window, and read its metadata fresh.
    ///
    /// RF-22 (task 10.1/10.2): a `BadWindow` from either half of that sequence — the
    /// subscribe's `.check()`, or the metadata `GetProperty` that follows it — means the
    /// window was destroyed mid-transition. Both are valid transitions, not failures:
    /// `Ok(None)`, no event, no propagated error.
    fn apply_active_window(
        &mut self,
        window: Option<Window>,
    ) -> Result<Option<RawEvent>, ReplyError> {
        if !self.retarget_subscription(window)? {
            return Ok(None);
        }
        self.active_window = window;
        let info = match window {
            Some(w) => match self.read_window_info(w) {
                Ok(info) => Some(info),
                Err(err) if is_bad_window(&err) => return Ok(None),
                Err(err) => return Err(err),
            },
            None => None,
        };
        Ok(Some(RawEvent::ActiveWindow(info)))
    }

    /// Unconditional read of title, PID and `WM_CLASS` for `window` (RF-2, RF-31): atom-type
    /// dispatch and 512-character truncation for the title, and the `WM_CLASS` →
    /// `/proc/<pid>/comm` → `"?"` fallback chain for `app_id` (tasks 10.9-10.12). `pid` is
    /// read before `app_id` because the fallback chain needs it.
    pub fn read_window_info(&self, window: Window) -> Result<RawWindowInfo, ReplyError> {
        let title = self.read_title(window)?;
        let pid = self.read_pid(window)?;
        let app_id = self.read_app_id(window, pid)?;
        Ok(RawWindowInfo {
            app_id,
            title: RawTitle::new(title),
            pid,
        })
    }

    /// Prefers `_NET_WM_NAME`, falls back to the legacy `WM_NAME`. Both are requested with
    /// `AtomEnum::ANY` (`AnyPropertyType`) rather than a fixed expected type, so the reply's
    /// own `type_` field always reflects what the property was actually stored as —
    /// `decode_property_text` then dispatches on that, rather than this module assuming
    /// `_NET_WM_NAME` is always `UTF8_STRING` and `WM_NAME` is always `STRING` (RF-31).
    ///
    /// Both reads are bounded by `MAX_TITLE_UNITS`. They previously passed `long_length =
    /// u32::MAX`, which asks the server for the entire property: a window advertising a
    /// 200,000-character title had all 200,000 characters copied into the daemon in one reply,
    /// for a value RF-31 caps at 512 characters anyway. The title is attacker-influenced
    /// content from an arbitrary application, so its read has to be bounded at the point it
    /// enters the process. The character-count truncation to `MAX_TITLE_CHARS` happens after
    /// decoding, since a byte bound cannot express a character limit exactly.
    ///
    /// `reply.bytes_after` tells `truncate_with_ellipsis` the *read bound* cut the property
    /// short (task 10.17); see there for why the character count alone cannot notice.
    fn read_title(&self, window: Window) -> Result<String, ReplyError> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.net_wm_name,
                AtomEnum::ANY,
                0,
                MAX_TITLE_UNITS,
            )?
            .reply()?;
        if reply.value_len > 0 {
            return Ok(truncate_with_ellipsis(
                &decode_property_text(&reply.value, reply.type_, self.atoms.utf8_string),
                reply.bytes_after > 0,
            ));
        }
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.wm_name,
                AtomEnum::ANY,
                0,
                MAX_TITLE_UNITS,
            )?
            .reply()?;
        Ok(truncate_with_ellipsis(
            &decode_property_text(&reply.value, reply.type_, self.atoms.utf8_string),
            reply.bytes_after > 0,
        ))
    }

    fn read_pid(&self, window: Window) -> Result<Option<u32>, ReplyError> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.net_wm_pid,
                AtomEnum::CARDINAL,
                0,
                1,
            )?
            .reply()?;
        // `0` is the reserved pid, never a real process — mirroring how `read_active_window`
        // already filters the reserved `0` window id. Without this a `_NET_WM_PID` of 0 yields
        // `Some(0)` and the `/proc/<pid>/comm` fallback below goes looking for `/proc/0/comm`.
        Ok(reply
            .value32()
            .and_then(|mut it| it.next())
            .filter(|&pid| pid != 0))
    }

    /// `WM_CLASS`'s second (class) component (window-capture "Metadata captured for a normal
    /// window"), falling back to `/proc/<pid>/comm` when `WM_CLASS` is absent but `pid` is
    /// `Some`, and finally to the `"?"` sentinel (RF-31, tasks 10.9-10.12). The `comm` read is
    /// best-effort — any failure at all (missing pid, exited or recycled process, unreadable
    /// procfs) falls through to the sentinel rather than propagating an error, matching design
    /// §7's "Process integration — subprocess inputs" threat-matrix response.
    fn read_app_id(&self, window: Window, pid: Option<u32>) -> Result<String, ReplyError> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.wm_class,
                AtomEnum::STRING,
                0,
                MAX_WM_CLASS_UNITS,
            )?
            .reply()?;
        if let Some(class) = parse_wm_class(&reply.value) {
            return Ok(class);
        }
        if let Some(pid) = pid {
            if let Some(comm) = read_process_comm(pid) {
                return Ok(comm);
            }
        }
        Ok(APP_ID_SENTINEL.to_string())
    }

    /// Drains `x11rb`'s event queue until it is **genuinely empty** or an event translates
    /// into a `RawEvent` (task 9.5-9.8): a `PropertyNotify` for `_NET_ACTIVE_WINDOW` on the
    /// root window triggers the full read-subscribe-read sequence
    /// (`on_active_window_changed`); every other event this slice does not yet understand is
    /// drained, counted in `drained_untranslated`, and discarded (Phase 10/11 add
    /// `DestroyNotify`, debounced title changes, and `SYNC` alarms on top).
    ///
    /// **`Ok(None)` means the queue is empty, and the loop is what makes that true.** It used
    /// to mean "either the queue is empty, or the one event I looked at was not one I
    /// translate", and those two are not interchangeable to a `poll(2)` reactor. `x11rb` reads
    /// the socket into an in-process queue, so by the time an event has been parsed the
    /// connection's file descriptor is no longer readable: answering `Ok(None)` with an event
    /// still queued sends the reactor back to sleep with nothing left to wake it, and the
    /// pending active-window change is lost rather than delayed. Draining to empty restores
    /// D-6's drain-before-poll invariant, which the reactor consumes in Phase 14.
    ///
    /// **Every in-loop exit that produces no event must `continue`, not `return`** (task
    /// 10.14). Task 9.19 fixed the untranslated exit and left two more — RF-22's valid
    /// transition, and a title read racing destruction — so the defect had moved, not gone.
    ///
    /// In `CaptureMode::Ewmh` this method issues no query of its own, so "no query
    /// beforehand" (window-capture "Active window changes while idle") holds: nothing runs
    /// until an event is already queued. `CaptureMode::InputFocusFallback` is the deliberate
    /// exception — RF-24's degraded path has no events to wait for and must poll
    /// `GetInputFocus`; see `poll_input_focus`.
    pub fn poll_for_event(&mut self) -> Result<Option<RawEvent>, ReplyError> {
        // A-7 (module doc): a startup or re-arm-time idle transition synthesized without a
        // real X11 event behind it. Drained before the connection is touched at all — this
        // is a one-shot value set at most once per arm, never a queue, so returning it here
        // does not risk leaving a real event undrained the way an early `return` inside the
        // loop below would (task 10.14's defect class).
        if let Some(event) = self.pending_idle_event.take() {
            return Ok(Some(event));
        }
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::SyncAlarmNotify(ref alarm_event) = event {
                if let Some(raw) = self.on_sync_alarm_notify(alarm_event)? {
                    return Ok(Some(raw));
                }
                // Not this source's alarm (or idle detection isn't in `IdleMode::Sync` at
                // all) — drained like any other untranslated event, below.
            }
            if self.is_active_window_notify(&event) {
                // `Ok(None)` is RF-22's valid transition, NOT "the queue is empty". Returning
                // it here abandons whatever is queued behind it, and the fd is already drained,
                // so the reactor would sleep through it. Keep draining.
                match self.on_active_window_changed()? {
                    Some(event) => return Ok(Some(event)),
                    None => continue,
                }
            }
            if self.is_active_window_destroy_notify(&event) {
                return Ok(Some(self.on_active_window_destroyed()));
            }
            if let Some(window) = self.active_window_title_notify(&event) {
                // RF-22's third `BadWindow` surface: this read races the window's death.
                // Propagating it reads as connection loss to the reactor (RF-6, RF-32). Keep
                // draining — the `DestroyNotify` behind it is what describes what happened.
                match self.read_title(window) {
                    Ok(title) => return Ok(Some(RawEvent::TitleChanged(RawTitle::new(title)))),
                    Err(err) if is_bad_window(&err) => continue,
                    Err(err) => return Err(err),
                }
            }
            self.drained_untranslated += 1;
        }
        match self.mode {
            CaptureMode::Ewmh => Ok(None),
            CaptureMode::InputFocusFallback => self.poll_input_focus(),
        }
    }

    /// A root-window `PropertyNotify` for `_NET_ACTIVE_WINDOW` — the one event this slice
    /// translates. Only meaningful in `CaptureMode::Ewmh`, which is also the only mode that
    /// subscribes the root window at all.
    fn is_active_window_notify(&self, event: &Event) -> bool {
        self.mode == CaptureMode::Ewmh
            && matches!(
                event,
                Event::PropertyNotify(ev)
                    if ev.window == self.root && ev.atom == self.atoms.net_active_window
            )
    }

    /// `DestroyNotify` for the window this module currently considers active (RF-23, task
    /// 10.5/10.6). A `DestroyNotify` for any other window — one that was active earlier and
    /// has since been retargeted away from — is not this module's concern and is drained
    /// like any other untranslated event.
    fn is_active_window_destroy_notify(&self, event: &Event) -> bool {
        matches!(event, Event::DestroyNotify(ev) if Some(ev.window) == self.active_window)
    }

    /// Bookkeeping for a translated `DestroyNotify` on the tracked window: the destroyed
    /// window is no longer active from this module's perspective, so `active_window` clears
    /// — X11 has already released its own resources for it, this is purely local state.
    fn on_active_window_destroyed(&mut self) -> RawEvent {
        self.active_window = None;
        RawEvent::ActiveWindowDestroyed
    }

    /// A `PropertyNotify` for a title atom (`_NET_WM_NAME` or the legacy `WM_NAME`) on the
    /// tracked window (RF-30, task 10.7/10.8) — returns the window id so the caller can issue
    /// the fresh, unconditional read without a second lookup through `self.active_window`.
    /// `None` for anything else, including a title change on a window that is not (or is no
    /// longer) the tracked one: only the active window's title is this module's concern.
    fn active_window_title_notify(&self, event: &Event) -> Option<Window> {
        match event {
            Event::PropertyNotify(ev)
                if Some(ev.window) == self.active_window
                    && (ev.atom == self.atoms.net_wm_name || ev.atom == self.atoms.wm_name) =>
            {
                Some(ev.window)
            }
            _ => None,
        }
    }
}

/// Writes startup diagnostics to `out`, one per line.
///
/// RF-24 does not say "compose a diagnostic", it says **emit an explicit diagnostic on
/// stderr** — the whole requirement is that the tiling-WM user finds out. `X11Source::connect`
/// calls this with `io::stderr()` as it connects, so the message is emitted at the moment the
/// condition is detected rather than waiting for a caller that does not exist yet (`reactor.rs`
/// is a Phase 14 stub, and nothing else in `src/` calls `connect`). `connect` still returns
/// the same strings so callers and tests can inspect what was emitted.
///
/// The writer is a parameter rather than a hardcoded `eprintln!` so the emission itself is
/// testable against an in-memory sink instead of being asserted by reading the source.
/// Diagnostics never carry a window title, so no D-7 privacy boundary is crossed here.
pub fn emit_startup_diagnostics<W: Write>(diagnostics: &[String], out: &mut W) -> io::Result<()> {
    for diagnostic in diagnostics {
        writeln!(out, "{diagnostic}")?;
    }
    Ok(())
}

/// RF-29: warns when `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE=wayland` is present, since absence
/// detection is less reliable under XWayland. Pure function of the environment — no X11
/// connection needed, which is why this is tested without Xvfb.
pub fn xwayland_warning() -> Option<String> {
    let wayland_display = env::var_os("WAYLAND_DISPLAY").is_some();
    let session_type_wayland = env::var("XDG_SESSION_TYPE")
        .map(|v| v == "wayland")
        .unwrap_or(false);
    if wayland_display || session_type_wayland {
        Some(
            "xwindowlog: running under XWayland — absence-detection reliability is reduced"
                .to_string(),
        )
    } else {
        None
    }
}

/// RF-4/RF-28: `afk_threshold` is operator configuration, not attacker input, but it still
/// crosses a process boundary into a SYNC wire field only 32 bits wide — `checked_*`, never a
/// bare cast (rust-systems: no bare arithmetic on a value that came from outside the
/// process). Durations beyond `u32::MAX` milliseconds (~49 days) saturate rather than wrap.
fn threshold_millis(afk_threshold: Duration) -> u32 {
    u32::try_from(afk_threshold.as_millis()).unwrap_or(u32::MAX)
}

fn threshold_int64(threshold_ms: u32) -> Int64 {
    Int64 {
        hi: 0,
        lo: threshold_ms,
    }
}

/// The trigger value to put on the wire for `test_type` (task 11.10).
///
/// The away edge is a `PositiveTransition` at the threshold itself. The return edge is a
/// `NegativeComparison` one millisecond **below** it, and both halves of that are load-bearing:
///
/// - **One below.** A trigger armed at exactly the value the counter currently holds is the
///   case this whole function exists for. The away alarm is armed at `threshold` and the
///   server fires it the instant `IDLETIME` reaches `threshold` — the notification carries
///   exactly `threshold`, not a millisecond more, in most fires. Arming the return edge at
///   that same value, at that instant, is what loses it: measured 12 returns lost in 60
///   against a real server, and every single lost trial was one whose away alarm had fired
///   at exactly `threshold`. Arming one below removes the equality entirely.
/// - **Comparison, not transition.** A transition fires only on the crossing, so a return
///   that happens before the alarm is armed is gone for good. A comparison is a level: the
///   server reports it as soon as the counter *is* below the value, including at the moment
///   of arming. Nothing about the meaning changes — `idle < threshold` and
///   `idle <= threshold - 1` are the same statement about milliseconds — but the level form
///   cannot be missed by being armed a moment too late.
///
/// Measured against a real Xvfb, 60 trials each, identical shape (away alarm fires, re-arm,
/// user returns immediately): `NegativeTransition` at `threshold` lost 12; the same with the
/// counter attribute re-sent to force the server to refresh its cached value lost 13;
/// `NegativeComparison` at `threshold - 1` lost 0.
fn alarm_trigger_value(threshold_ms: u32, test_type: TESTTYPE) -> Int64 {
    if test_type == TESTTYPE::POSITIVE_TRANSITION {
        threshold_int64(threshold_ms)
    } else {
        threshold_int64(threshold_ms.saturating_sub(1))
    }
}

/// `Int64` is SYNC's counter-value wire type — external input from the X server, so this is
/// `checked_*`/defensive, never a bare cast. `IDLETIME` is defined to never be negative; a
/// negative `hi` is treated as "not idle" rather than panicking or underflowing.
fn int64_to_ms(value: &Int64) -> u64 {
    if value.hi < 0 {
        return 0;
    }
    (u64::from(value.hi as u32) << 32) | u64::from(value.lo)
}

/// Pure decision the module doc's A-7 note is built on: given a freshly-read idle duration
/// and the test that was just armed, is that test's condition **already** satisfied? All I/O
/// (reading the value, changing the alarm) stays in the caller — this is what makes the
/// decision itself unit-testable without a real X11 connection.
///
/// Stated against the threshold in both directions, deliberately, even though the return
/// edge is armed one millisecond below it (`alarm_trigger_value`): `idle_ms < threshold_ms`
/// and `idle_ms <= threshold_ms - 1` are the same condition over whole milliseconds, so this
/// is the same test the server is applying, not an approximation of it.
fn crosses_armed_test(idle_ms: u64, threshold_ms: u32, armed: TESTTYPE) -> bool {
    if armed == TESTTYPE::POSITIVE_TRANSITION {
        idle_ms >= u64::from(threshold_ms)
    } else {
        idle_ms < u64::from(threshold_ms)
    }
}

/// The only two test types this module ever arms (module doc, RF-4): the away edge is a
/// `PositiveTransition`, the return edge a `NegativeComparison`, and each re-arms as the
/// other. See `alarm_trigger_value` for why the return edge is a comparison one millisecond
/// below the threshold rather than the symmetric `NegativeTransition` at it.
fn flip_test_type(test_type: TESTTYPE) -> TESTTYPE {
    if test_type == TESTTYPE::POSITIVE_TRANSITION {
        TESTTYPE::NEGATIVE_COMPARISON
    } else {
        TESTTYPE::POSITIVE_TRANSITION
    }
}

/// RF-25's degradation-chain diagnostic selection, pulled out of `init_idle_detection` as a
/// pure function of "was SYNC available" / "was MIT-SCREEN-SAVER available" so its three
/// outcomes are unit-testable without a connection at all. The selection this feeds is
/// covered behaviourally too, against genuine extension absence — `-extension
/// MIT-SCREEN-SAVER` for step 3 and an X11 wire proxy for `SYNC`, which the server refuses
/// to disable (`tests/x11_integration.rs`, task 11.13). `sync_ok` makes `screensaver_ok`
/// irrelevant, matching `init_idle_detection`'s short-circuit.
fn select_degradation_diagnostic(sync_ok: bool, screensaver_ok: bool) -> Option<String> {
    if sync_ok {
        None
    } else if screensaver_ok {
        Some(SCREENSAVER_DEGRADATION_DIAGNOSTIC.to_string())
    } else {
        Some(IDLE_DETECTION_DISABLED_DIAGNOSTIC.to_string())
    }
}

/// True for a `ReplyError` carrying X11's `BadWindow` — the discriminant RF-22's "a destroyed
/// window is a valid transition, not a failure" condition is built on (task 10.13's
/// cross-check). Centralized here so every call site that must tolerate a dead window agrees
/// on exactly the same X11 condition, rather than four separate inline matches drifting apart.
///
/// **`ErrorKind::Window` and nothing else**, deliberately. Widening it to any `X11Error`, or to
/// any error, turns connection loss into a silent `continue` at every call site — RF-6/RF-32
/// need exactly the opposite. Pinned by
/// `is_bad_window_tolerates_only_badwindow_and_never_connection_loss` (task 10.18).
fn is_bad_window(err: &ReplyError) -> bool {
    matches!(err, ReplyError::X11Error(e) if e.error_kind == ErrorKind::Window)
}

/// Reads a single-window-valued property (`WINDOW`, length 1) from `window`. `None` when the
/// property is absent, has the wrong format, or holds the reserved `0` window id. Used for
/// `_NET_SUPPORTING_WM_CHECK` on both the root window and the check window itself.
fn read_window_property(
    conn: &RustConnection,
    window: Window,
    property: u32,
) -> Result<Option<Window>, ReplyError> {
    let reply = conn
        .get_property(false, window, property, AtomEnum::WINDOW, 0, 1)?
        .reply()?;
    Ok(reply
        .value32()
        .and_then(|mut it| it.next())
        .filter(|&w| w != 0))
}

/// RF-31: decodes a text property's raw bytes according to the atom type the server actually
/// returned, rather than always assuming UTF-8 (task 10.9/10.10).
///
/// `UTF8_STRING` decodes as UTF-8, lossily — invalid byte sequences become the replacement
/// character instead of an error, since a malformed title must never fail capture. Anything
/// else — the X11 core/ICCCM legacy `STRING` type, and any other/unrecognized type this
/// module has no reason to special-case — decodes as Latin-1 (ISO 8859-1), where every byte
/// maps directly onto the Unicode code point of the same numeric value; that is `STRING`'s
/// defined encoding. Decoding `STRING` content as UTF-8 corrupted a Latin-1 accented
/// character such as 'é' (byte `0xE9`, a single valid Latin-1 code point but an invalid lone
/// UTF-8 continuation byte) into the replacement character; Latin-1 decoding keeps it intact.
fn decode_property_text(value: &[u8], type_: u32, utf8_string_atom: u32) -> String {
    if type_ == utf8_string_atom {
        String::from_utf8_lossy(value).into_owned()
    } else {
        value.iter().map(|&byte| byte as char).collect()
    }
}

/// RF-31: caps a decoded title at `MAX_TITLE_CHARS`, appending a single ellipsis when
/// truncation happens (task 10.9/10.10). Character-counted, not byte-counted —
/// `MAX_TITLE_UNITS` already bounds the `GetProperty` read itself in bytes; this bounds the
/// *decoded* value in characters, so a multi-byte UTF-8 codepoint is never sliced in half.
///
/// `bound_truncated` says the `GetProperty` read itself was capped (task 10.17) — truncation
/// the character count cannot see. A title of four-byte codepoints hits the 2048-byte bound at
/// exactly 512 characters, so `MAX_TITLE_CHARS` finds nothing to cut and the result is
/// indistinguishable from a genuine 512-character title. The ellipsis follows either bound.
fn truncate_with_ellipsis(text: &str, bound_truncated: bool) -> String {
    let chars = text.chars().count();
    if !bound_truncated && chars <= MAX_TITLE_CHARS {
        return text.to_string();
    }
    // Never grow the value: when the read bound already stopped short of `MAX_TITLE_CHARS`,
    // the ellipsis replaces the last character it did return rather than being appended.
    let keep = chars.min(MAX_TITLE_CHARS).saturating_sub(1);
    let mut truncated: String = text.chars().take(keep).collect();
    truncated.push('…');
    truncated
}

/// Best-effort `/proc/<pid>/comm` read — RF-31's `WM_CLASS`-absent fallback (tasks
/// 10.9-10.12; design §7's "Process integration — subprocess inputs" threat-matrix row).
/// `None` on any failure whatsoever: the pid already exited, was recycled onto an unrelated
/// process, procfs is unreadable, or the entry is simply not there. The caller falls back to
/// the `"?"` sentinel; this never becomes an error the daemon has to propagate.
fn read_process_comm(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
    sanitize_comm(&raw)
}

/// `comm`'s sanitization, split out from the `/proc` read itself so it is unit-testable
/// without a real process (task 10.11/10.12's threat-matrix cases: a `comm` containing a
/// newline, invalid UTF-8). `comm` is untrusted input from an arbitrary process, exactly like
/// `WM_CLASS` and a window title: invalid UTF-8 is replaced rather than rejected, and every
/// control character — including the trailing newline procfs always appends — is stripped
/// before this value is allowed anywhere near diagnostics, SQLite, or `exclude.rs`. `None`
/// when nothing usable survives sanitization, letting the caller fall back to the sentinel
/// rather than record a blank application name.
fn sanitize_comm(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let sanitized: String = text.chars().filter(|c| !c.is_control()).collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Parses `WM_CLASS`'s NUL-separated `"instance\0class\0"` form and returns the second
/// (class) component, **with control characters stripped** — window-capture "Metadata captured
/// for a normal window": `WM_CLASS = "firefox\0Firefox"` captures `app_id = "Firefox"`.
///
/// `WM_CLASS` is an arbitrary byte string that any application sets on its own window, so it is
/// untrusted input in exactly the way a window title is. Left verbatim it reached diagnostics
/// and SQLite, which means a `WM_CLASS` containing `\n` and an ANSI escape could forge log
/// lines in whichever terminal later renders them. Sanitizing here, at the point of capture,
/// is what makes that unreachable downstream instead of relying on every future consumer to
/// remember (rust-systems: treat anything from outside as untrusted data, never as
/// instructions, and strip control characters before it crosses into a report or a terminal).
///
/// `None` when the class component is missing, or empty once sanitized, letting the caller
/// apply its own sentinel rather than record a blank application name.
fn parse_wm_class(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut parts = text.split('\0');
    let _instance = parts.next();
    let class: String = parts.next()?.chars().filter(|c| !c.is_control()).collect();
    if class.is_empty() {
        None
    } else {
        Some(class)
    }
}

/// RF-32's exponential-backoff **policy** — pure and clock-free. Sequence: 500ms, 1s, 2s,
/// 4s, 8s, then a 16s ceiling forever, with independent ±20% jitter applied to each returned
/// delay so many simultaneously-failing daemons don't retry in lockstep. `entropy` is
/// injected rather than read internally, so a test can pin an exact jittered value — see
/// `os_entropy` for the production source.
#[derive(Debug, Default)]
pub struct ReconnectBackoff {
    attempt: u32,
}

impl ReconnectBackoff {
    pub fn new() -> Self {
        ReconnectBackoff { attempt: 0 }
    }

    /// The unjittered delay before the next attempt, doubling from `RECONNECT_BASE_DELAY`
    /// and clamped at `RECONNECT_CEILING`. `checked_shl` and a saturating fallback throughout
    /// — `attempt` grows without an upper bound over a long outage (RF-32: "retrying
    /// indefinitely for the life of the daemon") and must never overflow or panic.
    fn unjittered_delay(&self) -> Duration {
        let base_ms = RECONNECT_BASE_DELAY.as_millis() as u64;
        let scaled = base_ms.checked_shl(self.attempt).unwrap_or(u64::MAX);
        Duration::from_millis(scaled).min(RECONNECT_CEILING)
    }

    /// Advances to the next attempt and returns its jittered delay.
    pub fn next_delay(&mut self, entropy: u64) -> Duration {
        let delay = jitter(self.unjittered_delay(), entropy);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// RF-32: "a future outage again starts its retry delay at 500ms" — called once
    /// reconnection succeeds (`Reconnector::attempt`).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// ±`RECONNECT_JITTER_PERCENT`. `entropy % 41` maps onto `-20..=20` inclusive on both ends,
/// matching the requirement's exact bound. `entropy` is caller-supplied external input by
/// this module's own convention, so every step here is `checked_*`, never a bare arithmetic
/// op on it (rust-systems: no bare arithmetic on values from outside the process).
fn jitter(base: Duration, entropy: u64) -> Duration {
    let percent = (entropy % 41) as i64 - RECONNECT_JITTER_PERCENT;
    let base_ms = base.as_millis() as i64;
    let delta = base_ms
        .checked_mul(percent)
        .and_then(|scaled| scaled.checked_div(100))
        .unwrap_or(0);
    let jittered_ms = base_ms.checked_add(delta).unwrap_or(base_ms).max(0);
    Duration::from_millis(jittered_ms as u64)
}

/// Production entropy for `ReconnectBackoff::next_delay`. Process/thread-seeded, not
/// cryptographic — RF-32's jitter only needs to avoid a reconnect thundering herd, not
/// resist an adversary. `RandomState::new()` draws fresh OS-seeded keys on every call
/// (verified empirically: five successive calls in the same process produced five distinct
/// hash outputs with no input written), so no extra dependency or `Instant` mixing is
/// needed.
pub fn os_entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// How long `connect_bounded` waits for a reconnect attempt before treating the peer as
/// unreachable (T1, RF-32). Matches `logind::connect_bounded`'s own `BUS_CONNECT_TIMEOUT` —
/// the precedent this bound follows, for the same class of problem: a local IPC peer that
/// accepts a connection and then stalls the application-level handshake.
const RECONNECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

type X11ConnectResult = Result<(X11Source, Vec<String>), X11InitError>;

/// Bounds a connect attempt so a peer that accepts the socket and then stalls the X11 setup
/// handshake cannot block the single-threaded reactor that drives `Reconnector::attempt`
/// forever (T1, RF-32). Phase 12 already learned this failure class the hard way for logind's
/// D-Bus connect (`logind::connect_bounded`); this follows the exact same shape: run the
/// connect on a throwaway thread and bound how long the caller waits for it with
/// `recv_timeout`. `x11rb::rust_connection::RustConnection::connect` (called from
/// `X11Source::connect_with_afk_threshold`) has no such bound of its own and cannot be
/// interrupted once blocked, so a stalling peer leaves that thread parked on the read forever
/// — an accepted one-thread leak, never a hang for the reactor.
fn connect_bounded(
    connect: Box<dyn FnOnce() -> X11ConnectResult + Send>,
    timeout: Duration,
) -> X11ConnectResult {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("xwl-x11-reconnect".to_string())
        .spawn(move || {
            let _ = tx.send(connect());
        })
        .map_err(|e| X11InitError::ReconnectSpawn(e.to_string()))?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(X11InitError::ReconnectTimeout),
    }
}

/// RF-6/RF-32's outage driver: owns the "single `unknown` interval per outage" bookkeeping
/// that `tracker::Tracker::on_display_lost` cannot enforce on its own — it is an "any" row
/// that transitions to `unknown` every time it is called (see its doc), so which
/// `SourceEvent` the caller sends, and how often, is a property of the loop shape driving
/// it, not something the tracker can guard by itself. Driving this on a timer instead of
/// blocking, and turning its outcomes into real `SourceEvent`s, is `reactor.rs`'s job (Phase
/// 14); this type is the retry *policy*, fully testable without one.
pub struct Reconnector {
    display: Option<String>,
    afk_threshold: Duration,
    backoff: ReconnectBackoff,
    outage_open: bool,
}

/// The result of one `Reconnector::attempt`.
pub enum ReconnectAttempt {
    /// The first failure of a new outage. The caller MUST translate this into exactly one
    /// `SourceEvent::DisplayLost` (RF-6/RF-32) — never again until `Restored` is observed.
    OutageOpened { retry_after: Duration },
    /// A subsequent failed attempt during an outage already reported via `OutageOpened`. No
    /// additional event — this is the "no additional `unknown` intervals for failed
    /// retries" half of RF-6/RF-32.
    StillDown { retry_after: Duration },
    /// Reconnection succeeded. The caller MUST translate this into exactly one
    /// `SourceEvent::DisplayRestored`. Backoff is already reset by the time this is
    /// returned, so the *next* outage starts its delay at `RECONNECT_BASE_DELAY` again.
    ///
    /// `outage_was_open` is `false` when this is the very first attempt of an outage and it
    /// succeeded immediately, so no `OutageOpened` preceded it (task 11.16). The caller was
    /// therefore never told to close the current interval and open the outage's one
    /// `unknown` interval, and RF-6 still owes both: on `false` it must emit
    /// `SourceEvent::DisplayLost` before `DisplayRestored`, so a same-instant recovery is
    /// recorded as the short `unknown` gap it really was rather than disappearing.
    /// `source` is boxed so this enum's other, tiny variants don't all pay `X11Source`'s
    /// size (clippy's `large_enum_variant`).
    Restored {
        source: Box<X11Source>,
        diagnostics: Vec<String>,
        outage_was_open: bool,
    },
}

impl Reconnector {
    pub fn new(display: Option<&str>, afk_threshold: Duration) -> Self {
        Reconnector {
            display: display.map(str::to_string),
            afk_threshold,
            backoff: ReconnectBackoff::new(),
            outage_open: false,
        }
    }

    /// The idle threshold this policy will reconnect with. Read-only: exposed so a caller
    /// (or a test, RF-32 T2a) can confirm which value actually reached this `Reconnector`,
    /// as distinct from whatever default its owner's own constructor might otherwise apply.
    pub fn afk_threshold(&self) -> Duration {
        self.afk_threshold
    }

    /// One attempt. `entropy` feeds `ReconnectBackoff::next_delay` — see its doc. Bounded by
    /// `connect_bounded` (T1, RF-32) so a peer that accepts and then stalls cannot hang this
    /// call.
    pub fn attempt(&mut self, entropy: u64) -> ReconnectAttempt {
        let display = self.display.clone();
        let afk_threshold = self.afk_threshold;
        match connect_bounded(
            Box::new(move || {
                X11Source::connect_with_afk_threshold(display.as_deref(), afk_threshold)
            }),
            RECONNECT_CONNECT_TIMEOUT,
        ) {
            Ok((source, diagnostics)) => {
                self.backoff.reset();
                let outage_was_open = std::mem::replace(&mut self.outage_open, false);
                ReconnectAttempt::Restored {
                    source: Box::new(source),
                    diagnostics,
                    outage_was_open,
                }
            }
            Err(_) => {
                let retry_after = self.backoff.next_delay(entropy);
                if self.outage_open {
                    ReconnectAttempt::StillDown { retry_after }
                } else {
                    self.outage_open = true;
                    ReconnectAttempt::OutageOpened { retry_after }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // task 9.11/9.12 (RF-29) — pure function of the environment, no X11 connection needed.
    // `std::env` is process-global, so these mutate/restore env vars serially via a lock to
    // avoid racing other tests in this binary.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous: Vec<(&str, Option<String>)> =
            vars.iter().map(|(k, _)| (*k, env::var(k).ok())).collect();
        for (k, v) in vars {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
        f();
        for (k, v) in previous {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
    }

    #[test]
    fn xwayland_warning_present_when_wayland_display_set() {
        with_env(
            &[
                ("WAYLAND_DISPLAY", Some("wayland-0")),
                ("XDG_SESSION_TYPE", None),
            ],
            || {
                assert!(xwayland_warning().is_some());
            },
        );
    }

    #[test]
    fn xwayland_warning_present_when_session_type_wayland() {
        with_env(
            &[
                ("WAYLAND_DISPLAY", None),
                ("XDG_SESSION_TYPE", Some("wayland")),
            ],
            || {
                assert!(xwayland_warning().is_some());
            },
        );
    }

    #[test]
    fn xwayland_warning_absent_under_native_x11() {
        with_env(
            &[("WAYLAND_DISPLAY", None), ("XDG_SESSION_TYPE", Some("x11"))],
            || {
                assert_eq!(xwayland_warning(), None);
            },
        );
    }

    #[test]
    fn startup_diagnostics_are_written_to_the_sink_one_per_line() {
        let diagnostics = vec![
            "xwindowlog: window manager does not appear to be EWMH-compliant".to_string(),
            "xwindowlog: running under XWayland".to_string(),
        ];
        let mut sink = Vec::new();
        emit_startup_diagnostics(&diagnostics, &mut sink).expect("write to an in-memory sink");
        assert_eq!(
            String::from_utf8(sink).expect("utf-8"),
            "xwindowlog: window manager does not appear to be EWMH-compliant\n\
             xwindowlog: running under XWayland\n"
        );
    }

    #[test]
    fn no_diagnostics_writes_nothing_at_all() {
        let mut sink = Vec::new();
        emit_startup_diagnostics(&[], &mut sink).expect("write to an in-memory sink");
        assert!(sink.is_empty(), "a compliant startup must stay silent");
    }

    #[test]
    fn parse_wm_class_strips_control_characters_from_the_class_component() {
        // A newline plus an ANSI escape introducer: enough to forge a log line in any
        // terminal that later renders this value.
        assert_eq!(
            parse_wm_class(b"evil\x00ev\x1b[31mil\nApp\x07\x00"),
            Some("ev[31milApp".to_string())
        );
    }

    #[test]
    fn parse_wm_class_none_when_class_is_only_control_characters() {
        assert_eq!(parse_wm_class(b"evil\x00\n\x1b\x07\x00"), None);
    }

    #[test]
    fn parse_wm_class_extracts_second_component() {
        assert_eq!(
            parse_wm_class(b"firefox\0Firefox\0"),
            Some("Firefox".to_string())
        );
    }

    #[test]
    fn parse_wm_class_none_when_class_component_missing() {
        assert_eq!(parse_wm_class(b"firefox\0"), None);
        assert_eq!(parse_wm_class(b""), None);
    }

    // task 10.9/10.10 (RF-31) — pure decode/truncate functions, no X11 connection needed.

    #[test]
    fn decode_property_text_utf8_string_decodes_as_utf8() {
        let utf8_atom = 999;
        assert_eq!(
            decode_property_text("Café".as_bytes(), utf8_atom, utf8_atom),
            "Café"
        );
    }

    #[test]
    fn decode_property_text_string_type_decodes_as_latin1() {
        let utf8_atom = 999;
        let string_atom = AtomEnum::STRING.into();
        // 0xE9 is Latin-1 'é' — a single code point, not a UTF-8 continuation byte.
        assert_eq!(
            decode_property_text(&[b'C', b'a', b'f', 0xE9], string_atom, utf8_atom),
            "Café"
        );
    }

    #[test]
    fn decode_property_text_unrecognized_type_falls_back_to_latin1() {
        let utf8_atom = 999;
        // Neither UTF8_STRING nor STRING: still must not assume UTF-8.
        assert_eq!(decode_property_text(&[0xE9], utf8_atom, utf8_atom + 1), "é");
    }

    #[test]
    fn truncate_with_ellipsis_leaves_short_titles_untouched() {
        assert_eq!(truncate_with_ellipsis("short", false), "short");
        assert_eq!(
            truncate_with_ellipsis(&"x".repeat(512), false),
            "x".repeat(512),
            "exactly at the limit must not be truncated"
        );
    }

    #[test]
    fn truncate_with_ellipsis_caps_long_titles_at_512_with_trailing_ellipsis() {
        let truncated = truncate_with_ellipsis(&"x".repeat(600), false);
        assert_eq!(truncated.chars().count(), 512);
        assert!(truncated.ends_with('…'));
        assert_eq!(
            &truncated[..truncated.len() - '…'.len_utf8()],
            "x".repeat(511)
        );
    }

    /// Task 10.17: the *read bound*, not the character count, did the truncating. Without the
    /// ellipsis the value is indistinguishable from a genuine 512-character title.
    #[test]
    fn truncate_with_ellipsis_marks_a_title_the_read_bound_capped() {
        let capped = truncate_with_ellipsis(&"x".repeat(512), true);
        assert_eq!(capped.chars().count(), 512);
        assert!(capped.ends_with('…'));
    }

    /// **`is_bad_window` mutation pin (task 10.18; task 10.13's cross-check made executable).**
    ///
    /// Two mutations survived the whole suite: always true, and accepting any `X11Error` kind.
    /// Both turn every protocol fault — connection loss included — into a silent `continue`.
    /// Nothing else catches them: every other test only ever produces a real `BadWindow`.
    #[test]
    fn is_bad_window_tolerates_only_badwindow_and_never_connection_loss() {
        fn x11(error_kind: ErrorKind) -> ReplyError {
            ReplyError::X11Error(x11rb::x11_utils::X11Error {
                error_kind,
                error_code: 0,
                sequence: 0,
                bad_value: 0,
                minor_opcode: 0,
                major_opcode: 0,
                extension_name: None,
                request_name: None,
            })
        }

        assert!(is_bad_window(&x11(ErrorKind::Window)));
        // A different X11 error is a genuine protocol fault, not a window that went away.
        for kind in [ErrorKind::Value, ErrorKind::Access, ErrorKind::Match] {
            assert!(
                !is_bad_window(&x11(kind)),
                "{kind:?} is not a destroyed window and must not be tolerated"
            );
        }
        // RF-6/RF-32: connection loss MUST propagate so the reconnect path can run.
        let lost = ReplyError::ConnectionError(ConnectionError::UnknownError);
        assert!(!is_bad_window(&lost));
    }

    // task 10.11/10.12 (RF-31, design §7 "Process integration — subprocess inputs") —
    // `comm` sanitization, pure and X11-independent.

    #[test]
    fn sanitize_comm_strips_trailing_newline() {
        // Real `/proc/<pid>/comm` always ends with `\n`.
        assert_eq!(sanitize_comm(b"firefox\n"), Some("firefox".to_string()));
    }

    #[test]
    fn sanitize_comm_strips_embedded_control_characters() {
        assert_eq!(
            sanitize_comm(b"ev\x1b[31mil\napp\x07"),
            Some("ev[31milapp".to_string())
        );
    }

    #[test]
    fn sanitize_comm_handles_invalid_utf8_without_panicking() {
        // 0xFF is never valid UTF-8 on its own; lossy decoding must replace it, not panic.
        let result = sanitize_comm(b"a\xFFb\n");
        assert!(result.is_some());
        assert!(result.unwrap().contains('\u{FFFD}'));
    }

    #[test]
    fn sanitize_comm_none_when_only_control_characters_survive() {
        assert_eq!(sanitize_comm(b"\n\x1b\x07"), None);
        assert_eq!(sanitize_comm(b""), None);
    }

    #[test]
    fn read_process_comm_none_for_a_pid_that_does_not_exist() {
        // /proc pids are bounded well below u32::MAX on every real Linux kernel; this pid
        // cannot correspond to a live, recycled, or exited-but-still-cached process.
        assert_eq!(read_process_comm(u32::MAX), None);
    }

    #[test]
    fn read_process_comm_reads_a_real_process() {
        // This test binary's own process is guaranteed to exist for the test's duration.
        let comm = read_process_comm(std::process::id()).expect("own process must be readable");
        assert!(!comm.is_empty());
        assert!(!comm.chars().any(char::is_control));
    }

    // Phase 11 (tasks 11.1-11.6) — pure functions, no X11 connection needed.

    #[test]
    fn crosses_armed_test_positive_transition_fires_at_and_above_threshold() {
        assert!(!crosses_armed_test(299, 300, TESTTYPE::POSITIVE_TRANSITION));
        assert!(crosses_armed_test(300, 300, TESTTYPE::POSITIVE_TRANSITION));
        assert!(crosses_armed_test(301, 300, TESTTYPE::POSITIVE_TRANSITION));
    }

    #[test]
    fn crosses_armed_test_return_edge_fires_strictly_below_threshold() {
        assert!(!crosses_armed_test(300, 300, TESTTYPE::NEGATIVE_COMPARISON));
        assert!(crosses_armed_test(299, 300, TESTTYPE::NEGATIVE_COMPARISON));
    }

    #[test]
    fn flip_test_type_alternates_the_away_and_return_edges() {
        assert_eq!(
            flip_test_type(TESTTYPE::POSITIVE_TRANSITION),
            TESTTYPE::NEGATIVE_COMPARISON
        );
        assert_eq!(
            flip_test_type(TESTTYPE::NEGATIVE_COMPARISON),
            TESTTYPE::POSITIVE_TRANSITION
        );
    }

    /// Task 11.10: the return edge is armed one millisecond below the threshold, and the
    /// away edge at it. Arming both at the same value is what lost 12 returns in 60 against
    /// a real server, every one of them on a trial whose away alarm had fired at exactly the
    /// threshold; see `alarm_trigger_value`'s doc for the measurement.
    #[test]
    fn alarm_trigger_value_puts_the_return_edge_one_millisecond_below_the_threshold() {
        assert_eq!(
            int64_to_ms(&alarm_trigger_value(300, TESTTYPE::POSITIVE_TRANSITION)),
            300
        );
        assert_eq!(
            int64_to_ms(&alarm_trigger_value(300, TESTTYPE::NEGATIVE_COMPARISON)),
            299
        );
    }

    /// A zero threshold has no millisecond below it. `saturating_sub` keeps that a value of
    /// zero rather than `u32::MAX` wrapping into an alarm that fires on everything.
    #[test]
    fn alarm_trigger_value_saturates_at_zero_rather_than_wrapping() {
        assert_eq!(
            int64_to_ms(&alarm_trigger_value(0, TESTTYPE::NEGATIVE_COMPARISON)),
            0
        );
    }

    #[test]
    fn int64_to_ms_combines_hi_and_lo() {
        assert_eq!(int64_to_ms(&Int64 { hi: 0, lo: 4242 }), 4242);
        assert_eq!(int64_to_ms(&Int64 { hi: 1, lo: 0 }), 1u64 << 32);
    }

    /// A negative `hi` cannot be a real `IDLETIME` value — defensive, not reachable in
    /// practice, but must degrade to "not idle" rather than panic (rust-systems: external
    /// values never get bare arithmetic).
    #[test]
    fn int64_to_ms_negative_hi_is_treated_as_zero_not_a_panic() {
        assert_eq!(int64_to_ms(&Int64 { hi: -1, lo: 500 }), 0);
    }

    #[test]
    fn threshold_millis_converts_seconds_to_milliseconds() {
        assert_eq!(threshold_millis(Duration::from_secs(240)), 240_000);
    }

    #[test]
    fn threshold_millis_saturates_rather_than_overflows_for_absurd_durations() {
        assert_eq!(threshold_millis(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    #[test]
    fn select_degradation_diagnostic_sync_available_means_no_diagnostic() {
        assert_eq!(select_degradation_diagnostic(true, false), None);
        assert_eq!(select_degradation_diagnostic(true, true), None);
    }

    #[test]
    fn select_degradation_diagnostic_screensaver_only_names_the_degradation() {
        let diagnostic = select_degradation_diagnostic(false, true).expect("must warn");
        assert!(diagnostic.contains("MIT-SCREEN-SAVER"));
        assert_eq!(diagnostic, SCREENSAVER_DEGRADATION_DIAGNOSTIC);
    }

    #[test]
    fn select_degradation_diagnostic_neither_available_names_logind_only() {
        let diagnostic = select_degradation_diagnostic(false, false).expect("must warn");
        assert!(diagnostic.contains("logind"));
        assert_eq!(diagnostic, IDLE_DETECTION_DISABLED_DIAGNOSTIC);
    }

    /// RF-32's exact sequence: 500ms, 1s, 2s, 4s, 8s, then the 16s ceiling forever.
    /// `entropy = 20` maps to `20 % 41 - 20 = 0` percent jitter (see `jitter`'s doc), so this
    /// pins the unjittered sequence exactly rather than only its bounds.
    #[test]
    fn reconnect_backoff_sequence_matches_rf32_exactly_with_zero_jitter() {
        let mut backoff = ReconnectBackoff::new();
        let expected = [500u64, 1000, 2000, 4000, 8000, 16000, 16000, 16000];
        for expected_ms in expected {
            assert_eq!(backoff.next_delay(20).as_millis() as u64, expected_ms);
        }
    }

    #[test]
    fn reconnect_backoff_reset_returns_to_the_base_delay() {
        let mut backoff = ReconnectBackoff::new();
        for _ in 0..4 {
            backoff.next_delay(20);
        }
        backoff.reset();
        assert_eq!(backoff.next_delay(20).as_millis(), 500);
    }

    #[test]
    fn jitter_stays_within_plus_minus_20_percent_across_the_entropy_domain() {
        let base = Duration::from_millis(1000);
        for entropy in 0..123u64 {
            let jittered = jitter(base, entropy).as_millis() as i64;
            assert!(
                (800..=1200).contains(&jittered),
                "entropy {entropy} produced {jittered}ms, outside ±20% of 1000ms"
            );
        }
    }

    #[test]
    fn jitter_zero_percent_entropy_leaves_the_delay_unchanged() {
        // entropy = 20 -> (20 % 41) - 20 = 0 percent.
        assert_eq!(jitter(Duration::from_millis(1000), 20).as_millis(), 1000);
    }

    #[test]
    fn os_entropy_varies_across_calls() {
        // Not a statistical proof, just a smoke test that this isn't a hardcoded constant.
        let samples: std::collections::HashSet<u64> = (0..8).map(|_| os_entropy()).collect();
        assert!(
            samples.len() > 1,
            "os_entropy() returned the same value every time"
        );
    }

    /// **RED (task 11.5): single-unknown-per-outage bookkeeping.** A `Reconnector` pointed at
    /// a display nothing is listening on must report the FIRST failure as `OutageOpened` and
    /// every subsequent failure as `StillDown` — never a second `OutageOpened` for the same
    /// outage.
    #[test]
    fn reconnector_reports_outage_opened_once_then_still_down() {
        // Port :9199 is far outside this suite's Xvfb range (`DISPLAY_BASE = 213` in
        // tests/x11_integration.rs) and nothing else in this environment listens there.
        let mut reconnector = Reconnector::new(Some(":9199"), Duration::from_secs(240));
        match reconnector.attempt(20) {
            ReconnectAttempt::OutageOpened { .. } => {}
            ReconnectAttempt::StillDown { .. } => panic!("first failure must be OutageOpened"),
            ReconnectAttempt::Restored { .. } => panic!("nothing is listening on :9199"),
        }
        for _ in 0..3 {
            match reconnector.attempt(20) {
                ReconnectAttempt::StillDown { .. } => {}
                ReconnectAttempt::OutageOpened { .. } => {
                    panic!("must not re-report an outage already open")
                }
                ReconnectAttempt::Restored { .. } => panic!("nothing is listening on :9199"),
            }
        }
    }

    /// **RED (T1, RF-32): the reconnect attempt must be bounded.** `reconnector_reports_
    /// outage_opened_once_then_still_down` above only proves behavior against a *refused*
    /// connection, which fails fast on its own and would pass even with no bound at all. This
    /// proves the stronger property the constraint actually requires: a peer that accepts the
    /// TCP connection and then never speaks — so `x11rb::connect`'s setup-handshake read would
    /// block forever with no bound — must still make `Reconnector::attempt` return, because the
    /// single-threaded reactor that will drive this (`src/reactor.rs`) cannot afford to hang.
    #[test]
    fn reconnector_attempt_times_out_against_a_peer_that_accepts_and_then_stalls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("failed to bind a local TCP listener for the test");
        let port = listener
            .local_addr()
            .expect("local_addr on a just-bound listener")
            .port();
        // x11rb maps a "host:N" display string to TCP port 6000+N
        // (x11rb_protocol::parse_display::connect_addresses). Linux's ephemeral port range
        // (32768+ by default) is always above 6000, so this subtraction never underflows.
        let display_num = port - 6000;
        let display = format!("127.0.0.1:{display_num}");

        let _accept_thread = thread::spawn(move || {
            // Accept and never write a single byte back: the client's setup-handshake read
            // blocks forever on this end. Kept alive for the test binary's lifetime, exactly
            // like `logind::connect_bounded`'s own stalling-peer test.
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(Duration::from_secs(60));
            }
        });

        let mut reconnector = Reconnector::new(Some(&display), Duration::from_secs(240));
        let start = std::time::Instant::now();
        let result = reconnector.attempt(1);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "attempt against a stalling peer must return well within the bound, took {elapsed:?}"
        );
        match result {
            ReconnectAttempt::OutageOpened { .. } | ReconnectAttempt::StillDown { .. } => {}
            ReconnectAttempt::Restored { .. } => {
                panic!("a stalling peer never completes the X11 setup handshake")
            }
        }
    }
}
