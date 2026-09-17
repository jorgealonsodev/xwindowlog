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

use x11rb::connection::Connection;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
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

/// Errors connecting to or initializing the X11 capture layer.
#[derive(Debug)]
pub enum X11InitError {
    Connect(ConnectError),
    Protocol(ReplyError),
    Io(ConnectionError),
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

        // RF-24/RF-29: emit now, at the moment the condition is detected. Failing to write to
        // stderr must not stop the daemon from capturing, so the result is deliberately
        // ignored rather than turned into a startup error.
        let _ = emit_startup_diagnostics(&diagnostics, &mut io::stderr());

        let source = X11Source {
            conn,
            root,
            atoms,
            mode,
            active_window: None,
            drained_untranslated: 0,
        };
        if source.mode == CaptureMode::Ewmh {
            source.subscribe_root()?;
        }
        Ok((source, diagnostics))
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
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
        while let Some(event) = self.conn.poll_for_event()? {
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
}
