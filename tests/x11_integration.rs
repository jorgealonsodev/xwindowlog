//! Phase 9-11 `x11.rs` E2E tests: a real Xvfb, real `x11rb` wire protocol, no mocked
//! connection. Focused command for this slice (tasks.md Phase 9 row):
//! `cargo test --test x11_integration -- ewmh`. Phase 11's own focused command:
//! `cargo test --test x11_integration -- idle synthetic reconnect`.
//!
//! **Phase 11's extension-absence harness (task 11.13 — this note previously claimed the
//! opposite, and the claim was false).** RF-25's degradation chain has three steps, and both
//! degraded steps are now covered behaviourally, against genuine extension absence:
//!
//! - `MIT-SCREEN-SAVER` **can** be removed from this Xvfb at runtime. `Xvfb -extension
//!   MIT-SCREEN-SAVER` starts a server whose extension list omits it (verified with
//!   `xdpyinfo`), which is what `spawn_xvfb_with_args` uses.
//! - `SYNC` genuinely cannot: the server answers `[mi] Extension "SYNC" can not be disabled`
//!   and keeps it. `spawn_xproxy` below therefore puts a transparent X11 proxy between the
//!   client and the real server and rewrites the `QueryExtension(SYNC)` reply's `present`
//!   byte to 0 — a client, `x11rb` included, cannot tell that apart from an extension the
//!   server was never built with.
//!
//! The earlier note claimed "only the Generic Event Extension supports runtime `+/-extension`
//! toggling" and used it to justify leaving steps 2 and 3 with no behavioural coverage at
//! all. That justification was wrong on the facts (`Xvfb -help` documents `+extension name` /
//! `-extension name` generally, and `MIT-SCREEN-SAVER` really does disappear), and the
//! missing coverage was real: a `panic!()` inserted at the top of `poll_screensaver_idle`,
//! at the top of `screensaver_present`, or on `init_idle_detection`'s step-2/3 branch all
//! survived the whole suite.
//!
//! **Deviation from tasks.md's stated runtime harness, documented rather than silent
//! (see `src/x11.rs`'s module doc for the full rationale).** The Suggested Work Units
//! table names "Xvfb + openbox `--sm-disable`" as this PR's harness. `openbox` is not
//! installed here and this session has no interactive `sudo` to install it. `FakeWm`
//! below is a second, plain `x11rb` connection that performs exactly the X11 requests a
//! real EWMH window manager performs for the properties `src/x11.rs` reads — interning
//! and setting `_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW`, owning the "application" window —
//! so every test here still exercises the real X11 wire protocol end to end against a
//! real Xvfb server, not a synthetic/mocked connection. It is deterministic and free of
//! a real WM's own startup-timing flakiness, at the cost of not proving compatibility
//! with `openbox` specifically. Each test spawns its own Xvfb on a unique display number
//! so tests remain parallel-safe (no shared root-window state races).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, Window, WindowClass,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use xwindowlog::clock::{Clock, SystemClock};
use xwindowlog::exclude::Excluder;
use xwindowlog::tracker::{Effect, SourceEvent, Tracker, WindowInfo};
use xwindowlog::x11::{CaptureMode, RawEvent, ReconnectAttempt, Reconnector, X11Source};

/// Guarantees unique, non-colliding display numbers across concurrently running tests in
/// this binary (`cargo test` runs `#[test]` functions on multiple threads by default).
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

/// Base kept well above any display a developer's own desktop session is likely to be
/// using (`:0`, `:1`) — this is a throwaway Xvfb instance, not the developer's X server.
const DISPLAY_BASE: u32 = 213;

struct XvfbGuard {
    child: Child,
    display: String,
    /// The readiness-probe connection, deliberately **kept alive** for the whole test rather
    /// than dropped once it proves the server is up.
    ///
    /// An X server whose last client disconnects performs a *close-down reset*: it destroys
    /// every resource, resets its state and briefly stops accepting new connections. Dropping
    /// the probe connection took the client count back to zero and made the very next
    /// `x11rb::connect` (the `FakeWm`, or `X11Source::connect`) race that reset window —
    /// measured at 1 failure in 30 runs of this suite, always as a `connect` panic. Holding
    /// one connection open keeps the client count at ≥ 1 for the guard's whole lifetime, so
    /// the reset simply never happens while a test is running.
    _keepalive: RustConnection,
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns a fresh `Xvfb` and waits for it to accept X11 connections — **readiness-polled**,
/// matching design.md E-3's "EWMH readiness poll replacing `sleep 1`" principle rather than a
/// fixed sleep, which would be both slower than necessary and flaky under load. The connection
/// that proves readiness is retained by the returned guard; see `XvfbGuard::_keepalive`.
fn spawn_xvfb() -> XvfbGuard {
    spawn_xvfb_on(format!(":{}", free_display_number()))
}

/// The next display number nothing is already answering on (task 11.18).
///
/// The readiness poll below cannot tell "the server I just started" from "a server that was
/// already there", so without this check a test silently adopts a foreign X server that
/// happens to hold the number. That is not hypothetical: an `Xvfb` left behind by an earlier
/// session made `title_change_on_the_tracked_window_surfaces_undebounced` receive
/// `UserIdle { idle_for: 4027s }` — a real reading of a display that had been idle for 67
/// minutes — instead of the title change it was waiting for. Skipping occupied numbers also
/// stops `spawn_xproxy` from unlinking a live server's socket to bind its own.
fn free_display_number() -> u32 {
    loop {
        let display_num = DISPLAY_BASE + NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::SeqCst);
        if x11rb::connect(Some(&format!(":{display_num}"))).is_err() {
            return display_num;
        }
    }
}

/// The display-number-explicit half of `spawn_xvfb`, split out so Phase 11's reconnect E2E
/// test (`reconnector_recovers_when_a_real_xvfb_restarts_on_the_same_display`) can spawn a
/// SECOND Xvfb on the exact same display a first one just died on, rather than the always-
/// fresh display `spawn_xvfb` hands out for parallel-safety.
fn spawn_xvfb_on(display: String) -> XvfbGuard {
    spawn_xvfb_with_args(display, &[])
}

/// The extra-server-arguments half of `spawn_xvfb_on`, split out for task 11.13: RF-25's
/// degradation chain needs a server that genuinely lacks `MIT-SCREEN-SAVER`, and
/// `-extension MIT-SCREEN-SAVER` genuinely removes it (verified: `xdpyinfo` then lists 22
/// extensions with `MIT-SCREEN-SAVER` absent and `SYNC` still present).
fn spawn_xvfb_with_args(display: String, extra: &[&str]) -> XvfbGuard {
    let child = Command::new("Xvfb")
        .arg(&display)
        .args(["-screen", "0", "320x240x24", "-nolisten", "tcp"])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb must be installed for tests/x11_integration.rs");

    let deadline = Instant::now() + Duration::from_secs(10);
    let keepalive = loop {
        if let Ok((conn, _screen_num)) = x11rb::connect(Some(&display)) {
            break conn;
        }
        assert!(
            Instant::now() < deadline,
            "Xvfb on {display} did not become ready within 10s"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    XvfbGuard {
        child,
        display,
        _keepalive: keepalive,
    }
}

/// Task 11.9: the E2E readiness-poll helper on `_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW`,
/// **bounded, no fixed `sleep`** (design.md E-3) — written to be reused verbatim wherever a
/// real window manager's EWMH declaration must be awaited (Phase 19's CI job, per tasks.md).
/// `FakeWm::declare_ewmh_supported` sets this state synchronously (its own `.check()` calls
/// already round-trip before returning), so every test below that uses it observes readiness
/// on this function's very first poll — this still exercises the real bounded-poll code path
/// against a real Xvfb connection, not only its timeout arithmetic.
fn wait_for_ewmh_ready(display: &str, timeout: Duration) -> bool {
    let (conn, screen_num) = x11rb::connect(Some(display)).expect("readiness probe connect");
    let root = conn.setup().roots[screen_num].root;
    let net_supported = intern(&conn, b"_NET_SUPPORTED");
    let net_active_window = intern(&conn, b"_NET_ACTIVE_WINDOW");

    let deadline = Instant::now() + timeout;
    loop {
        let reply = conn
            .get_property(false, root, net_supported, AtomEnum::ATOM, 0, 512)
            .expect("_NET_SUPPORTED request")
            .reply()
            .expect("_NET_SUPPORTED reply");
        if let Some(mut supported) = reply.value32() {
            if supported.any(|atom| atom == net_active_window) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn intern(conn: &RustConnection, name: &[u8]) -> u32 {
    conn.intern_atom(false, name)
        .expect("intern_atom request")
        .reply()
        .expect("intern_atom reply")
        .atom
}

/// A minimal stand-in for a real EWMH window manager — see this file's module doc for why
/// it replaces `openbox` in this environment. Owns a second connection to the same Xvfb
/// display and performs exactly the requests `src/x11.rs` expects a compliant WM to have
/// made: interning/setting `_NET_SUPPORTED`, creating windows, setting `_NET_ACTIVE_WINDOW`
/// and the per-window metadata properties this phase's slice reads.
struct FakeWm {
    conn: RustConnection,
    root: Window,
    net_supported: u32,
    net_supporting_wm_check: u32,
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    wm_name: u32,
    wm_class: u32,
    utf8_string: u32,
}

impl FakeWm {
    fn connect(display: &str) -> Self {
        let (conn, screen_num) = x11rb::connect(Some(display)).expect("fake WM connect");
        let root = conn.setup().roots[screen_num].root;
        let net_supported = intern(&conn, b"_NET_SUPPORTED");
        let net_supporting_wm_check = intern(&conn, b"_NET_SUPPORTING_WM_CHECK");
        let net_active_window = intern(&conn, b"_NET_ACTIVE_WINDOW");
        let net_wm_name = intern(&conn, b"_NET_WM_NAME");
        let net_wm_pid = intern(&conn, b"_NET_WM_PID");
        let wm_name = intern(&conn, b"WM_NAME");
        let wm_class = intern(&conn, b"WM_CLASS");
        let utf8_string = intern(&conn, b"UTF8_STRING");
        FakeWm {
            conn,
            root,
            net_supported,
            net_supporting_wm_check,
            net_active_window,
            net_wm_name,
            net_wm_pid,
            wm_name,
            wm_class,
            utf8_string,
        }
    }

    /// Destroys `window` outright — task 10.1/10.2's RF-22 race: the active window can be
    /// destroyed between the daemon's `GetProperty(_NET_ACTIVE_WINDOW)` and its own
    /// `ChangeWindowAttributes(...).check()`/metadata read on that same window.
    fn destroy(&self, window: Window) {
        self.conn
            .destroy_window(window)
            .expect("destroy_window request")
            .check()
            .expect("destroy_window reply");
        self.conn.flush().expect("flush");
    }

    /// Sets the **legacy** `WM_NAME` property with the X11 core protocol's `STRING` type
    /// directly — never touching `_NET_WM_NAME`/`UTF8_STRING` — so a test can prove RF-31's
    /// atom-type-aware decoding rather than always assuming UTF-8 (task 10.9/10.10).
    /// `latin1_bytes` is written verbatim: the caller supplies the raw bytes a Latin-1-typed
    /// `STRING` property would actually carry, not text `x11rb` reinterprets.
    fn set_legacy_wm_name(&self, window: Window, latin1_bytes: &[u8]) {
        self.conn
            .change_property8(
                PropMode::REPLACE,
                window,
                self.wm_name,
                AtomEnum::STRING,
                latin1_bytes,
            )
            .expect("set WM_NAME request")
            .check()
            .expect("set WM_NAME reply");
        self.conn.flush().expect("flush");
    }

    /// The acts a real EWMH window manager performs at startup, which is what RF-24's
    /// corrected verification actually detects:
    ///
    /// 1. `_NET_SUPPORTED` on the **root window**, an `ATOM[]` listing every hint this WM
    ///    implements — `_NET_ACTIVE_WINDOW` among them. This is the property that carries
    ///    the compliance claim; the atom *names* are irrelevant on their own.
    /// 2. `_NET_SUPPORTING_WM_CHECK` (EWMH §2), a child window the WM creates and points at
    ///    from both the root window and the child itself. Both properties must agree, and
    ///    the child window dies with the window manager — which is what makes it a
    ///    *liveness* signal rather than a permanent mark on a server-wide table.
    fn declare_ewmh_supported(&self) {
        self.declare_ewmh(&[self.net_active_window, self.net_supporting_wm_check]);
    }

    /// A live, EWMH-aware window manager that simply **does not implement**
    /// `_NET_ACTIVE_WINDOW`: its check window exists and its `_NET_SUPPORTED` is a perfectly
    /// valid list, just without the one hint this daemon depends on.
    ///
    /// This is the most realistic form of RF-24's target case. A minimal or tiling window
    /// manager is rarely "not EWMH at all" — it usually implements part of the spec and
    /// omits the rest. The liveness probe alone cannot catch it, because the window manager
    /// really is alive; only reading `_NET_SUPPORTED`'s contents can.
    fn declare_ewmh_without_active_window(&self) {
        self.declare_ewmh(&[self.net_supporting_wm_check]);
    }

    fn declare_ewmh(&self, supported: &[u32]) {
        let check_window = self.create_window();
        for target in [self.root, check_window] {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    target,
                    self.net_supporting_wm_check,
                    AtomEnum::WINDOW,
                    &[check_window],
                )
                .expect("set _NET_SUPPORTING_WM_CHECK request")
                .check()
                .expect("set _NET_SUPPORTING_WM_CHECK reply");
        }
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.net_supported,
                AtomEnum::ATOM,
                supported,
            )
            .expect("set _NET_SUPPORTED request")
            .check()
            .expect("set _NET_SUPPORTED reply");
        self.conn.flush().expect("flush");
    }

    /// Models the window manager exiting: its `_NET_SUPPORTING_WM_CHECK` window is destroyed
    /// along with it. The `_NET_SUPPORTED` property it wrote on the **root** window survives,
    /// because the root window belongs to the server and outlives every client — which is
    /// precisely why the root property alone is not sufficient evidence.
    fn simulate_exit(&self) {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.net_supporting_wm_check,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .expect("read _NET_SUPPORTING_WM_CHECK request")
            .reply()
            .expect("read _NET_SUPPORTING_WM_CHECK reply");
        let check_window = reply
            .value32()
            .and_then(|mut it| it.next())
            .expect("a declared WM must have a check window");
        self.conn
            .destroy_window(check_window)
            .expect("destroy_window request")
            .check()
            .expect("destroy_window reply");
        self.conn.flush().expect("flush");
    }

    fn create_window(&self) -> Window {
        let win = self.conn.generate_id().expect("generate_id");
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::COPY_FROM_PARENT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .expect("create_window request")
            .check()
            .expect("create_window reply");
        self.conn.flush().expect("flush");
        win
    }

    /// Maps a window so the server will accept it as an input-focus target — `SetInputFocus`
    /// answers `BadMatch` for a window that is not viewable.
    fn map_window(&self, window: Window) {
        self.conn
            .map_window(window)
            .expect("map_window request")
            .check()
            .expect("map_window reply");
        self.conn.flush().expect("flush");
    }

    /// What a window manager with no EWMH support does instead of maintaining
    /// `_NET_ACTIVE_WINDOW`: it moves the X input focus and nothing else. This is the only
    /// signal RF-24's degraded path has to work with.
    fn focus(&self, window: Window) {
        self.conn
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .expect("set_input_focus request")
            .check()
            .expect("set_input_focus reply");
        self.conn.flush().expect("flush");
    }

    /// RF-4's idle-detection tests: injects one synthetic key press+release via the `XTEST`
    /// extension, which resets the `SYNC` `IDLETIME` system counter to (near) zero exactly
    /// as real keyboard input would — confirmed empirically against a live Xvfb before
    /// writing any of `src/x11.rs`'s idle-detection code (module doc's empirical notes).
    /// `XTEST` requests don't target a specific window, so this works from any connection —
    /// `FakeWm`'s is reused here only because it already owns one.
    fn generate_activity(&self) {
        self.conn
            .xtest_fake_input(KEY_PRESS_EVENT, 38, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .expect("xtest_fake_input(press) request")
            .check()
            .expect("xtest_fake_input(press) reply");
        self.conn
            .xtest_fake_input(
                KEY_RELEASE_EVENT,
                38,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )
            .expect("xtest_fake_input(release) request")
            .check()
            .expect("xtest_fake_input(release) reply");
        self.conn.flush().expect("flush");
    }

    fn set_active_window(&self, window: Window) {
        let value = [window];
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.net_active_window,
                AtomEnum::WINDOW,
                &value,
            )
            .expect("set _NET_ACTIVE_WINDOW request")
            .check()
            .expect("set _NET_ACTIVE_WINDOW reply");
        self.conn.flush().expect("flush");
    }

    /// Ordinary root-window property churn, of the kind a real desktop produces constantly
    /// and the daemon has no interest in. `_NET_CLIENT_LIST_STACKING` is rewritten by every
    /// EWMH window manager on every stacking change; it reaches our root `PropertyChange`
    /// subscription just like `_NET_ACTIVE_WINDOW` does, and it is what was observed
    /// swallowing a real wakeup.
    fn churn_unrelated_root_property(&self) {
        let atom = intern(&self.conn, b"_NET_CLIENT_LIST_STACKING");
        self.conn
            .change_property32(PropMode::REPLACE, self.root, atom, AtomEnum::WINDOW, &[])
            .expect("churn request")
            .check()
            .expect("churn reply");
        self.conn.flush().expect("flush");
    }

    fn set_title(&self, window: Window, title: &str) {
        self.conn
            .change_property8(
                PropMode::REPLACE,
                window,
                self.net_wm_name,
                self.utf8_string,
                title.as_bytes(),
            )
            .expect("set _NET_WM_NAME request")
            .check()
            .expect("set _NET_WM_NAME reply");
        self.conn.flush().expect("flush");
    }

    fn set_pid(&self, window: Window, pid: u32) {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                window,
                self.net_wm_pid,
                AtomEnum::CARDINAL,
                &[pid],
            )
            .expect("set _NET_WM_PID request")
            .check()
            .expect("set _NET_WM_PID reply");
        self.conn.flush().expect("flush");
    }

    fn set_wm_class(&self, window: Window, instance: &str, class: &str) {
        let mut data = Vec::new();
        data.extend_from_slice(instance.as_bytes());
        data.push(0);
        data.extend_from_slice(class.as_bytes());
        data.push(0);
        self.conn
            .change_property8(
                PropMode::REPLACE,
                window,
                self.wm_class,
                AtomEnum::STRING,
                &data,
            )
            .expect("set WM_CLASS request")
            .check()
            .expect("set WM_CLASS reply");
        self.conn.flush().expect("flush");
    }
}

/// window-capture "Window manager is EWMH-compliant" (task 9.1/9.2): both EWMH atoms
/// exist → `X11Source::connect` reports `CaptureMode::Ewmh` with no diagnostic.
#[test]
fn ewmh_compliant_no_diagnostic_and_ewmh_mode() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();

    let (source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    assert_eq!(source.mode(), CaptureMode::Ewmh);
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics for a compliant WM, got {diagnostics:?}"
    );
}

/// window-capture "Window manager does not expose EWMH properties" (task 9.3/9.4): a bare
/// Xvfb with no window manager at all has never interned `_NET_SUPPORTED`/`_NET_ACTIVE_WINDOW`
/// — `X11Source::connect` must emit an explicit diagnostic and fall back to
/// `CaptureMode::InputFocusFallback`, not sit idle waiting for events that will never arrive.
#[test]
fn ewmh_missing_emits_diagnostic_and_falls_back_to_input_focus() {
    let xvfb = spawn_xvfb();
    // deliberately no FakeWm here — a fresh Xvfb has interned neither EWMH atom.

    let (source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    assert_eq!(source.mode(), CaptureMode::InputFocusFallback);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("EWMH") || d.contains("compliant")),
        "expected an EWMH-noncompliance diagnostic, got {diagnostics:?}"
    );
}

/// Interns the two EWMH atom *names* on `display` and then disconnects, without ever
/// becoming a window manager or setting a single root property.
///
/// This is what an ordinary toolkit does. GTK3 interns `_NET_ACTIVE_WINDOW` unconditionally
/// on startup even on a display with no window manager at all. Atom interning writes to the
/// X server's **server-wide** atom-name table, which outlives every client and carries no
/// information whatsoever about whether a window manager is running — which is exactly why
/// the connection is dropped here: the atoms survive it.
fn intern_ewmh_atom_names_as_unrelated_client(display: &str) {
    let (conn, _screen_num) = x11rb::connect(Some(display)).expect("unrelated client connect");
    intern(&conn, b"_NET_SUPPORTED");
    intern(&conn, b"_NET_ACTIVE_WINDOW");
    conn.flush().expect("flush");
}

/// **CRITICAL-1 anti-regression (RF-24, corrected 2026-09-17).** A display with *no window
/// manager at all*, where an unrelated ordinary client has already interned both EWMH atom
/// names, MUST be reported as non-compliant.
///
/// The superseded implementation verified compliance with `intern_atom(only_if_exists =
/// true)`, which only asks whether a *name* is present in the server's global atom table.
/// Under this exact scenario that check returns "compliant" with no window manager present,
/// so the daemon would subscribe to `_NET_ACTIVE_WINDOW` and block forever waiting for
/// events nothing will ever send — the precise silent failure RF-24 exists to prevent, on
/// the precise audience (tiling/minimal-WM users) RF-24 names.
#[test]
fn ewmh_atoms_interned_without_a_window_manager_is_not_compliant() {
    let xvfb = spawn_xvfb();
    // No FakeWm: nothing sets `_NET_SUPPORTED` on the root window, because nothing is
    // managing this display. Only the atom *names* get created.
    intern_ewmh_atom_names_as_unrelated_client(&xvfb.display);

    let (source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    assert_eq!(
        source.mode(),
        CaptureMode::InputFocusFallback,
        "interned atom names are not evidence of a window manager; \
         compliance lives in the root window's _NET_SUPPORTED property"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("EWMH") || d.contains("compliant")),
        "expected an EWMH-noncompliance diagnostic, got {diagnostics:?}"
    );
}

/// **CRITICAL-1 anti-regression, `_NET_SUPPORTED` contents half (RF-24).** A window manager
/// that is alive and EWMH-aware but does not implement `_NET_ACTIVE_WINDOW` must be reported
/// as non-compliant.
///
/// This is RF-24's headline case stated precisely: the minimal/tiling window manager that
/// implements part of EWMH and omits the hint this daemon depends on. The liveness probe
/// cannot catch it — the window manager really is running — so the only thing that can is
/// reading what `_NET_SUPPORTED` actually lists.
///
/// This test exists because a mutation that made the `_NET_SUPPORTED` contents check
/// unconditionally return "compliant" was **not** caught by the rest of the suite: every other
/// non-compliant scenario also lacks a live check window, so the liveness probe was masking
/// the gap.
#[test]
fn ewmh_supported_without_active_window_is_not_compliant() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_without_active_window();

    let (source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    assert_eq!(
        source.mode(),
        CaptureMode::InputFocusFallback,
        "a live WM that does not implement _NET_ACTIVE_WINDOW is not usable by this daemon"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("EWMH") || d.contains("compliant")),
        "expected an EWMH-noncompliance diagnostic, got {diagnostics:?}"
    );
}

/// **CRITICAL-1 anti-regression, liveness half (RF-24).** A window manager that declared EWMH
/// support and then exited leaves the daemon's view of the world stale. Once its
/// `_NET_SUPPORTING_WM_CHECK` window is gone, the daemon MUST report non-compliance rather
/// than keep waiting on a property nobody maintains any more.
#[test]
fn ewmh_supporting_wm_check_window_gone_is_not_compliant() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    // The window manager exits: its `_NET_SUPPORTING_WM_CHECK` window is destroyed, but the
    // `_NET_SUPPORTED` property it set on the root window is *not* removed with it (root is
    // owned by the server, not by the WM), so the root property alone still looks compliant.
    wm.simulate_exit();

    let (source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    assert_eq!(
        source.mode(),
        CaptureMode::InputFocusFallback,
        "a stale _NET_SUPPORTED left behind by a dead WM must not count as compliance"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("EWMH") || d.contains("compliant")),
        "expected an EWMH-noncompliance diagnostic, got {diagnostics:?}"
    );
}

/// window-capture "Active window changes while idle" (task 9.5/9.6): after subscribing, a
/// `_NET_ACTIVE_WINDOW` change wakes the daemon exactly once (`poll_for_event` yields exactly
/// one `RawEvent`, not zero, not two) and no window-state query was issued before that wakeup.
#[test]
fn ewmh_active_window_change_wakes_exactly_once() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w1 = wm.create_window();
    wm.set_title(w1, "first");
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_active_window(w1);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    // No query issued yet, and nothing pending: proves the daemon is not busy-waiting.
    assert!(
        source.poll_for_event().expect("poll_for_event").is_none(),
        "expected no event pending before any change occurred"
    );

    let w2 = wm.create_window();
    wm.set_title(w2, "second");
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_active_window(w2);

    let event = wait_for_raw_event(&mut source);
    match event {
        RawEvent::ActiveWindow(Some(info)) => {
            assert_eq!(info.app_id, "App2");
        }
        other => panic!("expected ActiveWindow(Some(..)) for w2, got {other:?}"),
    }

    // Exactly once: no second event queued for the same single change.
    assert!(
        source.poll_for_event().expect("poll_for_event").is_none(),
        "expected exactly one wakeup for one active-window change"
    );
}

/// **CRITICAL-3 anti-regression.** One `poll_for_event` call must drain the queue until it is
/// genuinely empty, not stop at the first event it does not translate.
///
/// `x11rb` reads the socket into its own in-process queue eagerly, so once an event has been
/// parsed the connection's file descriptor is **no longer readable**. A `poll_for_event` that
/// consumes one untranslated event and answers `Ok(None)` is therefore indistinguishable, to
/// its caller, from "nothing happened" — and the Phase 14 `poll(2)` reactor would go back to
/// sleep with a real active-window change sitting in the queue and no fd to wake it. The
/// wakeup is not delayed, it is lost until something unrelated happens to arrive.
///
/// The ordering here is guaranteed, not raced: X11 delivers events on a connection in the
/// order the server processed the requests that caused them, and the `read_window_info` round
/// trip below cannot receive its reply before the events the server had already sent.
#[test]
fn ewmh_poll_drains_past_an_untranslated_event_in_one_call() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_active_window(w1);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    // Queued first: an event this module does not translate. Queued second: the real change.
    wm.churn_unrelated_root_property();
    wm.set_active_window(w2);

    // Round trip on the source's own connection, forcing both pending events into `x11rb`'s
    // in-process queue before the single `poll_for_event` call below.
    let _ = source.read_window_info(w1).expect("round trip");

    let event = source
        .poll_for_event()
        .expect("poll_for_event")
        .expect("one call must drain past the untranslated event and surface the change");
    match event {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App2"),
        other => panic!("expected ActiveWindow(Some(..)) for w2, got {other:?}"),
    }
    assert_eq!(
        source.drained_untranslated(),
        1,
        "the unrelated root property change must have been drained, not left queued"
    );

    // And now the queue really is empty, which is what makes `Ok(None)` safe to sleep on.
    assert!(source.poll_for_event().expect("poll_for_event").is_none());
}

/// The tracked-window subscription must **move** to the new active window, not accumulate.
///
/// `subscribe_window` used to only ever add: every window that was ever active stayed
/// subscribed for the rest of the daemon's session. Over an 8-hour day of window switching
/// that grows without bound, and each stale subscription keeps delivering `PropertyNotify` for
/// every title change in a window nobody is tracking — a media player retitling itself once a
/// second is the ordinary case. All of it is discarded, so it is pure event-loop work, and it
/// directly amplifies the lost-wakeup defect the drain loop above fixes.
#[test]
fn ewmh_subscription_moves_to_the_new_window_instead_of_accumulating() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_active_window(w1);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    // Explicit initial read: `_NET_ACTIVE_WINDOW` was already w1 before the subscription
    // began, so no event will ever fire for it. This is what subscribes w1.
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_active_window(w2);
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App2"),
        other => panic!("expected ActiveWindow(Some(..)) for w2, got {other:?}"),
    }

    let baseline = source.drained_untranslated();

    // w1 is no longer tracked. Nothing it does should reach the daemon at all.
    for n in 0..5 {
        wm.set_title(w1, &format!("stale churn {n}"));
    }
    let _ = source.read_window_info(w2).expect("round trip");

    assert!(source.poll_for_event().expect("poll_for_event").is_none());
    assert_eq!(
        source.drained_untranslated(),
        baseline,
        "title churn in a window that is no longer active must not reach the daemon"
    );
}

/// window-capture "Property read follows every active-window change" (task 9.7/9.8): the
/// daemon issues a fresh read on every change, never reusing a previously cached value — proven
/// by changing the window's title *without* changing which window is active, then forcing a
/// second read of the same window and observing the updated title.
#[test]
fn ewmh_unconditional_read_never_reuses_cached_metadata() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_title(w, "before");
    wm.set_wm_class(w, "app", "App");
    wm.set_active_window(w);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    // `_NET_ACTIVE_WINDOW` was already set before `connect`'s subscription began, so no
    // `PropertyNotify` will ever fire for it — an explicit initial read is the real startup
    // path (there is no "change" event for state that predates the subscription).
    let first = source
        .on_active_window_changed()
        .expect("on_active_window_changed");
    let Some(RawEvent::ActiveWindow(Some(first_info))) = first else {
        panic!("expected ActiveWindow(Some(..)) on first read, got {first:?}")
    };
    // `RawTitle` deliberately has no public raw-content accessor outside `exclude.rs` (D-7's
    // privacy boundary) — its redacting `Debug` (char count only) is the one thing this test,
    // outside that module, is allowed to observe, and it is enough to distinguish two titles of
    // different lengths without ever reading the raw text.
    assert_eq!(
        format!("{:?}", first_info.title),
        format!("RawTitle(<{} chars, redacted>)", "before".chars().count())
    );

    wm.set_title(w, "after-a-much-longer-title");

    // Re-run the same unconditional read path directly (task 9.7's own read, not a second
    // active-window change) and confirm it reflects the new title rather than the cached one.
    let second_info = source.read_window_info(w).expect("read_window_info");
    assert_eq!(
        format!("{:?}", second_info.title),
        format!(
            "RawTitle(<{} chars, redacted>)",
            "after-a-much-longer-title".chars().count()
        )
    );
    assert_ne!(
        format!("{:?}", first_info.title),
        format!("{:?}", second_info.title),
        "second read must reflect the updated title, not a cached value"
    );
}

/// window-capture "Title changes between the property read and the event mask being active"
/// (task 10.3/10.4, RF-22): a title change that lands on a window while it is becoming the
/// active window — before the daemon's subscription to *that* window's own events is even
/// registered, so no `PropertyNotify` for the change could ever be delivered to it — must
/// still be reflected. It is, because task 9.7's unconditional read runs *after* the
/// subscribe completes and reads current server state, never relying on a notify for the
/// value that raced it.
#[test]
fn title_racing_the_subscribe_is_still_reflected_by_the_unconditional_read() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_title(w, "before-subscribe");
    wm.set_active_window(w);
    // The race: this title change happens before the daemon has ever subscribed to `w`'s own
    // events (that only starts once it processes the active-window change above), so no
    // `PropertyNotify` for it is lost — there was never a subscription to lose one from.
    wm.set_title(w, "after-subscribe");

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    // `_NET_ACTIVE_WINDOW` was already set to `w` before `connect`'s subscription began, so
    // no `PropertyNotify` will ever fire for it — an explicit initial read is the real
    // startup path, exactly as in `ewmh_unconditional_read_never_reuses_cached_metadata`.
    let event = source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    match event {
        Some(RawEvent::ActiveWindow(Some(info))) => assert_eq!(
            format!("{:?}", info.title),
            format!(
                "RawTitle(<{} chars, redacted>)",
                "after-subscribe".chars().count()
            ),
            "the unconditional read must reflect the racing title, not the one that predates it"
        ),
        other => panic!("expected ActiveWindow(Some(..)) for w, got {other:?}"),
    }
}

/// **CRITICAL-2 anti-regression (RF-24's degraded path).** On a display whose window manager
/// exposes no EWMH properties, the daemon must **actually observe focus changes**, not merely
/// report `CaptureMode::InputFocusFallback` and a diagnostic string.
///
/// This is the whole point of the degradation: RF-24 exists so a tiling-WM user does not get a
/// daemon that runs happily and records nothing. Asserting `mode()` and the diagnostic text
/// proves the daemon *noticed* the problem; it proves nothing about whether it then worked.
/// In that mode the root window is deliberately left unsubscribed, so there is no event stream
/// at all — if `GetInputFocus` is never queried, the daemon observes exactly zero changes
/// while the X server can confirm focus genuinely moved.
#[test]
fn input_focus_fallback_observes_real_focus_changes_without_a_window_manager() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    // Deliberately no `declare_ewmh_supported()`: this models a minimal or tiling window
    // manager that moves the input focus and maintains no EWMH property whatsoever.
    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_title(w1, "first");
    wm.map_window(w1);
    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_title(w2, "second");
    wm.map_window(w2);
    wm.focus(w1);

    let (mut source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    assert_eq!(source.mode(), CaptureMode::InputFocusFallback);
    assert!(!diagnostics.is_empty(), "expected a degradation diagnostic");

    // The window focused before the daemon started must still be observed: in this mode there
    // is no "change" event for pre-existing state, so a query is the only way to see it.
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App1"),
        other => panic!("expected the already-focused window App1, got {other:?}"),
    }

    // And the real assertion: a focus change that happens while the daemon is running is
    // actually observed by the daemon.
    wm.focus(w2);
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App2"),
        other => panic!("expected the newly focused window App2, got {other:?}"),
    }

    // Steady state: no spurious repeat for a focus that did not change.
    assert!(
        source.poll_for_event().expect("poll_for_event").is_none(),
        "unchanged focus must not be reported as a change"
    );
}

/// window-capture "Metadata captured for a normal window" (task 9.9/9.10): `WM_CLASS =
/// "firefox\0Firefox"` + title + `_NET_WM_PID = 4821` captures `app_id = "Firefox"` and
/// `pid = Some(4821)`.
#[test]
fn ewmh_metadata_captured_for_normal_window() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_title(w, "GitHub - foo/bar");
    wm.set_wm_class(w, "firefox", "Firefox");
    wm.set_pid(w, 4821);
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(info.app_id, "Firefox");
    assert_eq!(info.pid, Some(4821));
}

/// **WARNING-6 anti-regression.** `app_id` comes from `WM_CLASS`, which is an arbitrary
/// byte string any application sets on its own window — untrusted input, exactly like a title.
/// It reached diagnostics and SQLite verbatim, so a `WM_CLASS` carrying a newline and an ANSI
/// escape could forge log lines in whatever terminal later displays them. Control characters
/// are stripped at the point `app_id` is read, which is the only place that can guarantee
/// nothing downstream ever sees them (rust-systems: "strip control characters before it
/// crosses into a report, a terminal or a model prompt").
#[test]
fn wm_class_control_characters_are_stripped_at_the_point_of_capture() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_title(w, "title");
    wm.set_wm_class(w, "evil", "ev\u{1b}[31mil\nApp\u{7}");
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert!(
        !info.app_id.chars().any(char::is_control),
        "app_id must carry no control characters, got {:?}",
        info.app_id
    );
    // The escape introducer, the newline and the bell are gone; the inert remainder stays,
    // so the app is still identifiable rather than silently renamed.
    assert_eq!(info.app_id, "ev[31milApp");
}

/// A `WM_CLASS` whose class component is nothing but control characters sanitizes to the empty
/// string, which is not a usable `app_id` — it must fall back to the `"?"` sentinel rather
/// than record a blank application name.
#[test]
fn wm_class_of_only_control_characters_falls_back_to_the_sentinel() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "evil", "\n\u{1b}\u{7}");
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(info.app_id, "?");
}

/// `_NET_WM_PID = 0` is not a process id. `read_active_window` already filters the reserved
/// `0` window id; the PID read did not, so it yielded `Some(0)` and Phase 10 would go on to
/// read `/proc/0/comm`.
#[test]
fn zero_net_wm_pid_is_not_a_pid() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_pid(w, 0);
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(
        info.pid, None,
        "pid 0 is the reserved id, not a real process"
    );
}

/// The title read must be **bounded**. It used to pass `long_length = u32::MAX`, so a window
/// advertising a 200,000-character title had all 200,000 characters read into the daemon's
/// memory in one reply — for a value RF-31 caps at 512 characters anyway.
///
/// Phase 9's job here is only to stop the unbounded read; RF-31's 512-character truncation
/// with an ellipsis is Phase 10 (task 10.9/10.10). So the assertion is the bound itself: the
/// read must be small, and must still be large enough to leave Phase 10 the 512 characters it
/// has to truncate to.
#[test]
fn title_read_is_bounded_rather_than_unlimited() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    let huge = "x".repeat(200_000);
    wm.set_title(w, &huge);
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    // `RawTitle`'s redacting `Debug` reports the character count and nothing else (D-7).
    let observed = format!("{:?}", info.title);
    let chars: usize = observed
        .trim_start_matches("RawTitle(<")
        .split(' ')
        .next()
        .expect("char count")
        .parse()
        .expect("char count is numeric");
    assert!(
        chars <= 2048,
        "title read must be bounded, got {chars} characters"
    );
    assert!(
        chars >= 512,
        "the bound must still leave RF-31's 512-character limit reachable, got {chars}"
    );
}

/// window-capture "Active window is destroyed before its event mask is set" (task 10.1/10.2,
/// RF-22, subscribe half): `_NET_ACTIVE_WINDOW` names a window that is destroyed before the
/// daemon's `ChangeWindowAttributes(...).check()` on it — deterministically simulated here by
/// destroying the window *before* the daemon ever attempts to subscribe to it, so the check
/// always observes `BadWindow`. This must be a valid transition, not a daemon failure: no
/// error, no event, and the daemon keeps waiting for the next real change.
#[test]
fn active_window_destroyed_before_subscribe_is_a_valid_transition_not_an_error() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let dead = wm.create_window();
    wm.set_active_window(dead);
    wm.destroy(dead);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    let event = source
        .on_active_window_changed()
        .expect("BadWindow on the subscribe half must not surface as an error");
    assert!(
        event.is_none(),
        "a destroyed active window must produce no event, got {event:?}"
    );

    // RF-22: the daemon must keep waiting for the next real change, not get stuck.
    let alive = wm.create_window();
    wm.set_wm_class(alive, "app", "App");
    wm.set_active_window(alive);
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App"),
        other => panic!("expected the daemon to observe the next real change, got {other:?}"),
    }
}

/// window-capture "Active window is destroyed before its event mask is set" (task 10.1/10.2,
/// RF-22, the `GetProperty` half Phase 9's verification additionally found): the subscribe
/// succeeds because the window still exists, but the window is destroyed before the following
/// unconditional metadata read. Deterministically forced by re-triggering the same still-
/// current window after destroying it out from under the daemon: `retarget_subscription`
/// short-circuits (the window is already the one subscribed), so only the `GetProperty` half
/// runs and hits `BadWindow`.
#[test]
fn active_window_destroyed_before_metadata_read_is_a_valid_transition_not_an_error() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_active_window(w);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let first = source
        .on_active_window_changed()
        .expect("on_active_window_changed");
    assert!(
        matches!(first, Some(RawEvent::ActiveWindow(Some(_)))),
        "expected a normal ActiveWindow event while w is alive, got {first:?}"
    );

    wm.destroy(w);

    // `_NET_ACTIVE_WINDOW` still names `w` (destroying a window does not touch the root
    // property), so this re-read exercises the metadata `GetProperty` on an already-dead,
    // already-subscribed window.
    let second = source
        .on_active_window_changed()
        .expect("BadWindow on the GetProperty half must not surface as an error");
    assert!(
        second.is_none(),
        "a metadata read racing window destruction must produce no event, got {second:?}"
    );
}

/// window-capture "Destruction safety net" (task 10.5/10.6, RF-23): `DestroyNotify` for the
/// currently tracked active window surfaces as `RawEvent::ActiveWindowDestroyed` — the raw
/// surfacing half; the 250 ms grace-period state machine itself was already proven in
/// `tracker.rs` (Phase 6, tasks 6.6-6.7). This module's only job is to notice and translate.
#[test]
fn destroy_notify_on_the_tracked_window_surfaces_as_active_window_destroyed() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_active_window(w);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    // Establishes the subscription on `w` (pre-existing state, no notify for it — see the
    // other tests' identical initial-read pattern).
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    wm.destroy(w);

    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindowDestroyed => {}
        other => panic!("expected ActiveWindowDestroyed, got {other:?}"),
    }
}

/// A `DestroyNotify` for a window that is **not** the currently tracked active window (for
/// example, a window that was active earlier and has since been retargeted away from) must be
/// drained like any other event this module does not translate, never surfaced.
#[test]
fn destroy_notify_on_an_untracked_window_is_drained_not_surfaced() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_active_window(w1);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_active_window(w2);
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App2"),
        other => panic!("expected ActiveWindow(Some(..)) for w2, got {other:?}"),
    }

    let baseline = source.drained_untranslated();
    // w1 is no longer subscribed at all (`unsubscribe_window` cleared its event mask when
    // w2 took over), so its destruction generates no event whatsoever on this connection —
    // not even one to drain. The real assertion is simply that it never surfaces as
    // `ActiveWindowDestroyed`.
    wm.destroy(w1);
    let _ = source.read_window_info(w2).expect("round trip");
    assert!(source.poll_for_event().expect("poll_for_event").is_none());
    assert_eq!(
        source.drained_untranslated(),
        baseline,
        "an untracked window's destruction must produce nothing to drain, since it was already unsubscribed"
    );
}

/// window-capture "Title debounce" (task 10.7/10.8, RF-30 — the surfacing half only): a title
/// change on the tracked window surfaces as `RawEvent::TitleChanged` with no delay of its
/// own, proving the debounce state machine is exclusively `tracker.rs`'s concern (design §2
/// D-8) and this module merely notices and reports.
#[test]
fn title_change_on_the_tracked_window_surfaces_undebounced() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_title(w, "before");
    wm.set_active_window(w);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    wm.set_title(w, "after");

    let started = Instant::now();
    let event = wait_for_raw_event(&mut source);
    let elapsed = started.elapsed();

    match event {
        RawEvent::TitleChanged(title) => assert_eq!(
            format!("{title:?}"),
            format!("RawTitle(<{} chars, redacted>)", "after".chars().count())
        ),
        other => panic!("expected TitleChanged, got {other:?}"),
    }
    // The default debounce is 2000 ms (RF-30); surfacing well under that proves this module
    // does not itself wait for stability — the tracker's debounce state machine does that.
    assert!(
        elapsed < Duration::from_millis(500),
        "x11.rs must not itself debounce title changes; took {elapsed:?}"
    );
}

/// A title change on a window that is **not** the tracked active window must not surface at
/// all — only the currently active window's title is this module's concern.
#[test]
fn title_change_on_an_untracked_window_is_drained_not_surfaced() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_title(w1, "w1 title");
    wm.set_active_window(w1);

    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");

    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_active_window(w2);
    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindow(Some(info)) => assert_eq!(info.app_id, "App2"),
        other => panic!("expected ActiveWindow(Some(..)) for w2, got {other:?}"),
    }

    // w1 is retargeted away (unsubscribed) — its title churn must produce no event at all.
    wm.set_title(w1, "w1 title changed after losing focus");
    let _ = source.read_window_info(w2).expect("round trip");
    assert!(source.poll_for_event().expect("poll_for_event").is_none());
}

/// window-capture "Title exceeds the maximum length" (task 10.9/10.10, RF-31): a 600-character
/// title truncates to exactly 512 characters, the last of which is the trailing ellipsis.
#[test]
fn title_over_512_characters_truncates_with_a_trailing_ellipsis() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_title(w, &"x".repeat(600));
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(
        format!("{:?}", info.title),
        "RawTitle(<512 chars, redacted>)",
        "600 characters must truncate to exactly 512, ellipsis included"
    );
}

/// A title at or under the 512-character limit must be left untouched — no ellipsis added.
#[test]
fn title_at_512_characters_is_not_truncated() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    wm.set_title(w, &"x".repeat(512));
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(
        format!("{:?}", info.title),
        "RawTitle(<512 chars, redacted>)"
    );
}

/// **RF-31 atom-type-aware decoding.** `WM_NAME` typed `STRING` (the X11 core/ICCCM legacy
/// type, Latin-1) must be decoded as Latin-1, not assumed to be UTF-8. Bytes `[0xC3, 0xA9]`
/// are a deliberately adversarial pair: interpreted as UTF-8 they form one perfectly valid
/// character ('é', U+00E9) with no decode error at all — silently wrong, not merely garbled —
/// while the correct Latin-1 reading is **two** separate characters (U+00C3 'Ã', U+00A9 '©').
/// The two readings are distinguishable purely by character count, which is all `RawTitle`'s
/// redacting `Debug` exposes outside `exclude.rs` (D-7).
#[test]
fn legacy_wm_name_typed_string_decodes_as_latin1_not_utf8() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let w = wm.create_window();
    wm.set_wm_class(w, "app", "App");
    // No `_NET_WM_NAME` is ever set — only the legacy `WM_NAME`/`STRING` path is exercised.
    wm.set_legacy_wm_name(w, &[0xC3, 0xA9]);
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(
        format!("{:?}", info.title),
        "RawTitle(<2 chars, redacted>)",
        "STRING must decode as Latin-1 (2 code points), not UTF-8 (which reads 1)"
    );
}

/// window-capture "WM_CLASS absent, PID present" (task 10.9-10.12, RF-31): no `WM_CLASS`, but
/// `_NET_WM_PID` names a real, live process — `app_id` falls back to `/proc/<pid>/comm`. Uses
/// a real spawned child so the pid and its `comm` are both genuine, not fabricated.
#[test]
fn wm_class_absent_pid_present_reads_proc_comm() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn a short-lived child process");
    let w = wm.create_window();
    wm.set_title(w, "no wm_class here");
    wm.set_pid(w, child.id());
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(info.app_id, "sleep");
}

/// window-capture "WM_CLASS and PID both absent, or comm read fails" (task 10.9-10.12,
/// RF-31): no `WM_CLASS` and an already-exited pid (so `/proc/<pid>/comm` cannot be read)
/// falls back to the `"?"` sentinel — capture continues, no error.
#[test]
fn wm_class_absent_pid_exited_falls_back_to_sentinel() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let mut child = Command::new("true").spawn().expect("spawn a child process");
    let dead_pid = child.id();
    let _ = child.wait(); // reaped: /proc/<dead_pid>/comm is now gone.

    let w = wm.create_window();
    wm.set_title(w, "no wm_class here either");
    wm.set_pid(w, dead_pid);
    wm.set_active_window(w);

    let (source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    let info = source.read_window_info(w).expect("read_window_info");

    assert_eq!(info.app_id, "?");
}

/// Polls (bounded) until an event arrives and translates it — mirrors what the reactor
/// (Phase 14) will eventually drive via `poll(2)`; here it is a short bounded loop since this
/// test has no separate reactor thread of its own.
fn wait_for_raw_event(source: &mut X11Source) -> RawEvent {
    // 5s (task 11.14). It was briefly raised to 10s and blamed on contention; instrumenting
    // every call in this file across ten full-suite runs put the slowest real wait at 504ms,
    // so contention was never the cause and the raise only doubled the time to fail. A
    // timeout here means an event was genuinely lost, which is a defect to fix in
    // `src/x11.rs` (see task 11.10), never a bound to widen.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = source.poll_for_event().expect("poll_for_event") {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an X11 event"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One EWMH window, active and already subscribed on a throwaway Xvfb: the setup all four
/// Phase 10 correction scenarios below share. Pre-existing state produces no notify, so the
/// explicit `on_active_window_changed` is what establishes the subscription — the same
/// initial-read pattern the tests above use. The guard is returned, not dropped: dropping it
/// tears the display down.
fn tracked_window(title: &str) -> (XvfbGuard, FakeWm, Window, X11Source) {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    let window = wm.create_window();
    wm.set_wm_class(window, "app", "App");
    wm.set_title(window, title);
    wm.set_active_window(window);
    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    source
        .on_active_window_changed()
        .expect("on_active_window_changed");
    (xvfb, wm, window, source)
}

/// **CRITICAL-1 anti-regression** (task 10.14). `ewmh_poll_drains_past_an_untranslated_event_in_one_call`
/// covers only the *untranslated* exit; this pins the `Ok(None)` valid-transition exit, whose
/// lost-wakeup consequence `poll_for_event`'s own comment explains.
#[test]
fn poll_continues_draining_past_a_valid_transition_that_produced_no_event() {
    let (_xvfb, wm, tracked, mut source) = tracked_window("before");

    // Queued first: an active-window change naming an already-destroyed window — RF-22's valid
    // transition, which yields no event. Queued second: a real title change on the tracked
    // window. The `read_window_info` round trip forces both into `x11rb`'s in-process queue
    // before the single `poll_for_event` below.
    let dead = wm.create_window();
    wm.set_active_window(dead);
    wm.destroy(dead);
    wm.set_title(tracked, "after");
    let _ = source.read_window_info(tracked).expect("round trip");

    let event = source
        .poll_for_event()
        .expect("poll_for_event")
        .expect("one call must drain past the valid transition and surface the queued change");
    assert!(
        matches!(event, RawEvent::TitleChanged(_)),
        "expected the title change queued behind the dead window, got {event:?}"
    );
}

/// **CRITICAL-2 anti-regression** (task 10.15): a retarget that fails must leave the tracked
/// window still subscribed and still named by `active_window`. `retarget_subscription`'s doc
/// explains why subscribing before releasing is the property that guarantees it.
#[test]
fn a_retarget_that_fails_leaves_the_tracked_window_subscribed() {
    let (_xvfb, wm, tracked, mut source) = tracked_window("before");

    // The race: `_NET_ACTIVE_WINDOW` names a window destroyed before the subscribe reaches the
    // server, so the retarget fails and the tracked window must stay tracked.
    let dead = wm.create_window();
    wm.set_active_window(dead);
    wm.destroy(dead);
    let _ = source.read_window_info(tracked).expect("round trip");
    assert!(
        source.poll_for_event().expect("poll_for_event").is_none(),
        "a destroyed active window must produce no event"
    );

    // Still tracked, so its title change must still reach the daemon — this is the assertion
    // that fails if the subscription was released and never retaken.
    wm.set_title(tracked, "after");
    match wait_for_raw_event(&mut source) {
        RawEvent::TitleChanged(_) => {}
        other => panic!("expected TitleChanged for the still-tracked window, got {other:?}"),
    }
}

/// **CRITICAL-3 anti-regression** (task 10.16): the title read inside the drain loop is RF-22's
/// third `BadWindow` surface. See `poll_for_event` for why propagating it would read as
/// connection loss to the reactor (RF-6, RF-32).
#[test]
fn a_title_read_racing_window_destruction_is_tolerated_not_an_error() {
    let (_xvfb, wm, tracked, mut source) = tracked_window("before");

    // X11 delivers these in the order the server processed them, so the title `PropertyNotify`
    // is queued ahead of the `DestroyNotify` — and by the time the daemon reads the title, the
    // window is already gone.
    wm.set_title(tracked, "after");
    wm.destroy(tracked);

    match wait_for_raw_event(&mut source) {
        RawEvent::ActiveWindowDestroyed => {}
        other => panic!("expected ActiveWindowDestroyed, got {other:?}"),
    }
}

/// **WARNING-1 anti-regression** (task 10.17). Unlike `truncate_with_ellipsis`'s unit test,
/// which passes the flag by hand, this proves the real X11 path actually *sets* it: that a
/// title the 2048-byte read bound capped arrives with `bytes_after > 0` and so keeps its
/// ellipsis. `truncate_with_ellipsis`'s doc explains why the character count cannot see it.
#[test]
fn a_title_capped_by_the_read_bound_still_carries_the_ellipsis() {
    // U+1D11E encodes as four UTF-8 bytes, so 600 of them are 2400 bytes and the 2048-byte
    // read bound returns exactly 512 whole codepoints — precisely `MAX_TITLE_CHARS`.
    let (_xvfb, _wm, tracked, source) = tracked_window(&"\u{1D11E}".repeat(600));
    let info = source.read_window_info(tracked).expect("read_window_info");

    // D-7: the only sanctioned way to read a captured title outside `exclude.rs` is to put it
    // through `Excluder::evaluate`, which is exactly what the reactor does in production.
    let excluder = Excluder::from_toml_str("").expect("an empty config compiles");
    let evaluated = excluder.evaluate("App", info.title);
    assert_eq!(evaluated.title.as_str().chars().count(), 512);
    assert!(
        evaluated.title.as_str().ends_with('…'),
        "a title the read bound truncated must be marked as truncated"
    );
}

// Phase 11 (tasks 11.1-11.9): SYNC/IDLETIME absence detection, the RF-25 degradation chain,
// RF-6/RF-32 reconnection, and the three-synthetic-window E2E harness.

/// `RawEvent` -> `SourceEvent`, exactly the conversion `reactor.rs` (Phase 14) will own in
/// production (D-7: the only sanctioned way to obtain a `SafeTitle` outside `exclude.rs` is
/// `Excluder::evaluate`). Local to this test file because no reactor exists yet to own it —
/// the same reasoning `tracked_window` and the CRITICAL/WARNING tests above already rely on.
fn to_source_event(excluder: &Excluder, raw: RawEvent) -> SourceEvent {
    match raw {
        RawEvent::ActiveWindow(info) => SourceEvent::ActiveWindow(info.map(|info| {
            let evaluated = excluder.evaluate(&info.app_id, info.title);
            WindowInfo {
                app_id: evaluated.app_id,
                title: evaluated.title,
                pid: info.pid,
            }
        })),
        RawEvent::TitleChanged(title) => {
            SourceEvent::TitleChanged(excluder.evaluate("", title).title)
        }
        RawEvent::ActiveWindowDestroyed => SourceEvent::ActiveWindowDestroyed,
        RawEvent::UserIdle { idle_for } => SourceEvent::UserIdle { idle_for },
        RawEvent::UserActive => SourceEvent::UserActive,
        RawEvent::DisplayLost => SourceEvent::DisplayLost,
        RawEvent::DisplayRestored => SourceEvent::DisplayRestored,
    }
}

/// **RED (task 11.3): step 1 available -> no degradation diagnostic.** This environment's
/// Xvfb always exposes `SYNC` (confirmed empirically; see this file's module doc), so this
/// is the one degradation-chain outcome provable end to end here.
#[test]
fn sync_idle_used_when_available_no_degradation_diagnostic() {
    let xvfb = spawn_xvfb();
    let (_source, diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");
    for diagnostic in &diagnostics {
        assert!(
            !diagnostic.contains("SYNC") && !diagnostic.contains("SCREEN-SAVER"),
            "SYNC is available in this environment; no degradation diagnostic was expected, \
             got {diagnostic:?}"
        );
    }
}

/// **RED (task 11.1): the alarm fires a positive transition at the threshold, and
/// `idle_for` is read exactly once, at that instant.** `generate_activity` resets `IDLETIME`
/// to (near) zero immediately before connecting, so the 500ms threshold is crossed for real
/// by real elapsed time, not by an already-past-threshold startup (that scenario is its own
/// test below).
#[test]
fn sync_idle_alarm_fires_user_idle_with_correct_idle_for() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(500))
            .expect("X11Source::connect_with_afk_threshold");

    match wait_for_raw_event(&mut source) {
        RawEvent::UserIdle { idle_for } => {
            assert!(
                (400..=2000).contains(&idle_for.as_millis()),
                "expected idle_for close to the 500ms threshold, got {idle_for:?}"
            );
        }
        other => panic!("expected UserIdle, got {other:?}"),
    }
}

/// **RED (task 11.1): the negative transition re-arms the alarm to detect the return of
/// activity.**
#[test]
fn sync_idle_user_returns_after_idle_produces_user_active() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(300))
            .expect("X11Source::connect_with_afk_threshold");

    match wait_for_raw_event(&mut source) {
        RawEvent::UserIdle { .. } => {}
        other => panic!("expected UserIdle first, got {other:?}"),
    }

    wm.generate_activity();
    match wait_for_raw_event(&mut source) {
        RawEvent::UserActive => {}
        other => panic!("expected UserActive after activity resumed, got {other:?}"),
    }
}

/// **RED (module doc's A-7 empirical note): a `Transition` alarm only fires on the edge, so
/// a user already idle past the threshold at connect time must be detected immediately, not
/// left waiting for a crossing that already happened.** Reproduces the exact defect found
/// while implementing this task: without the one-shot arm-time check, this scenario hangs
/// until the test's own timeout.
#[test]
fn sync_idle_already_past_threshold_at_connect_synthesizes_immediately() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();
    std::thread::sleep(Duration::from_millis(350));

    let start = Instant::now();
    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(200))
            .expect("X11Source::connect_with_afk_threshold");

    let event = source
        .poll_for_event()
        .expect("poll_for_event")
        .expect("the already-past-threshold idle state must be synthesized immediately");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the synthesized event must not wait for a real alarm that will never fire, took {:?}",
        start.elapsed()
    );
    match event {
        RawEvent::UserIdle { idle_for } => {
            assert!(
                idle_for.as_millis() >= 200,
                "idle_for {idle_for:?} must be >= threshold"
            );
        }
        other => panic!("expected an immediately-synthesized UserIdle, got {other:?}"),
    }
}

/// Proves the real wire-level `MIT-SCREEN-SAVER` `QueryInfo` round trip against Xvfb — the
/// I/O primitive `X11Source::poll_screensaver_idle` depends on. `IdleMode::ScreenSaverPolling`
/// itself can't be forced through `X11Source`'s public API without `SYNC` genuinely absent,
/// which this Xvfb cannot simulate (this file's module doc) — this test proves the request
/// this module would issue in that mode actually works, independent of mode selection.
#[test]
fn screensaver_query_info_round_trip_returns_a_plausible_value() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let info = wm
        .conn
        .screensaver_query_info(wm.root)
        .expect("screensaver_query_info request")
        .reply()
        .expect("screensaver_query_info reply");
    assert!(
        info.ms_since_user_input < 2000,
        "expected a small idle value right after generating activity, got {}ms",
        info.ms_since_user_input
    );
}

/// Task 11.9: the readiness-poll helper observes `FakeWm`'s EWMH declaration and returns
/// quickly (state was set synchronously before this call, via `declare_ewmh_supported`'s own
/// `.check()` round trips) rather than consuming its whole bounded timeout.
#[test]
fn ewmh_readiness_poll_returns_quickly_once_declared() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();

    let start = Instant::now();
    assert!(wait_for_ewmh_ready(&xvfb.display, Duration::from_secs(5)));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "EWMH state was already declared; the poll should not have waited, took {:?}",
        start.elapsed()
    );
}

/// **RED (tasks 11.7/11.8): three synthetic windows, driven end to end through `X11Source` +
/// a real `Tracker`, produce intervals whose total duration matches real elapsed wall time
/// within the PRD's own ≤1s tolerance.** `FakeWm::create_window` is this phase's in-house
/// `x11rb` synthetic-window mechanism (no `xdotool`), extending the same deviation already
/// documented for Phase 9/10 (module doc). `WallTs` is whole-second-granular by design (D-9),
/// which is exactly why the PRD's own tolerance is ≤1s rather than sub-second: real per-window
/// dwell times below are chosen comfortably above that granularity.
#[test]
fn three_synthetic_windows_produce_correct_intervals_within_one_second_tolerance() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.declare_ewmh_supported();
    assert!(wait_for_ewmh_ready(&xvfb.display, Duration::from_secs(2)));

    let w1 = wm.create_window();
    wm.set_wm_class(w1, "app1", "App1");
    wm.set_title(w1, "Window One");
    let w2 = wm.create_window();
    wm.set_wm_class(w2, "app2", "App2");
    wm.set_title(w2, "Window Two");
    let w3 = wm.create_window();
    wm.set_wm_class(w3, "app3", "App3");
    wm.set_title(w3, "Window Three");

    wm.set_active_window(w1);
    let (mut source, _diagnostics) =
        X11Source::connect(Some(&xvfb.display)).expect("X11Source::connect");

    let clock = SystemClock;
    let mut tracker = Tracker::new();
    let excluder = Excluder::from_toml_str("").expect("empty config compiles");
    let mut opens: Vec<(String, xwindowlog::clock::WallTs)> = Vec::new();

    let apply = |tracker: &mut Tracker, event, opens: &mut Vec<(String, _)>| {
        let now = clock.now_wall();
        for effect in tracker.on_event(event, now, clock.now_mono()) {
            match effect {
                Effect::OpenOnly { at, open } | Effect::Transition { at, open } => {
                    opens.push((open.app, at));
                }
                _ => {}
            }
        }
        now
    };

    // Pre-existing state (w1 was set before `connect`) produces no `PropertyNotify` — the
    // same explicit-establish pattern `tracked_window` uses above.
    let raw = source
        .on_active_window_changed()
        .expect("on_active_window_changed")
        .expect("w1 must be reported");
    apply(&mut tracker, to_source_event(&excluder, raw), &mut opens);

    let per_window = Duration::from_millis(2000);
    std::thread::sleep(per_window);

    wm.set_active_window(w2);
    let raw = wait_for_raw_event(&mut source);
    apply(&mut tracker, to_source_event(&excluder, raw), &mut opens);

    std::thread::sleep(per_window);

    wm.set_active_window(w3);
    let raw = wait_for_raw_event(&mut source);
    apply(&mut tracker, to_source_event(&excluder, raw), &mut opens);

    std::thread::sleep(per_window);

    // RF-33-shaped close so w3's interval has an observable `end` too.
    let end = apply(&mut tracker, SourceEvent::Shutdown, &mut opens);

    assert_eq!(
        opens
            .iter()
            .map(|(app, _)| app.as_str())
            .collect::<Vec<_>>(),
        vec!["App1", "App2", "App3"],
        "expected three distinct intervals in switch order"
    );

    let start = opens.first().expect("at least one interval").1;
    let total_secs = end.as_unix_secs() - start.as_unix_secs();
    assert!(
        (5..=7).contains(&total_secs),
        "expected roughly 6s of total elapsed wall time across the three ~2s switches \
         (±1s tolerance), got {total_secs}s"
    );
}

/// **RED (tasks 11.5/11.6, real E2E): a real X11 connection loss, three consecutive real
/// failed reconnection attempts (the idle-detection spec's own "Repeated reconnection
/// failures" scenario, applied to `Reconnector`), then a real Xvfb restart on the same
/// display is recovered.** Reports the outage exactly once (`OutageOpened`, then `StillDown`
/// for the three failed retries — never a second `OutageOpened`) and exactly one `Restored`
/// once the replacement server is reachable.
#[test]
fn reconnector_recovers_when_a_real_xvfb_restarts_on_the_same_display() {
    let xvfb = spawn_xvfb();
    let display = xvfb.display.clone();
    drop(xvfb); // kills the first Xvfb — a real connection loss, not a simulated one.

    let mut reconnector = Reconnector::new(Some(&display), Duration::from_secs(240));

    let first = reconnector.attempt(20);
    assert!(
        matches!(first, ReconnectAttempt::OutageOpened { .. }),
        "the first failed attempt must open the outage"
    );

    // Three consecutive real failures against a display nothing is listening on yet —
    // nothing must re-report `OutageOpened`. `attempt` is retried immediately rather than
    // slept on its reported `retry_after`: the exact backoff *values* are already pinned by
    // `x11::tests::reconnect_backoff_sequence_matches_rf32_exactly_with_zero_jitter`; this
    // test proves the real reconnection mechanism, not the timing.
    for _ in 0..3 {
        match reconnector.attempt(20) {
            ReconnectAttempt::StillDown { .. } => {}
            ReconnectAttempt::OutageOpened { .. } => {
                panic!("the outage was already open; must not report a second OutageOpened")
            }
            ReconnectAttempt::Restored { .. } => {
                panic!("nothing is listening on {display} yet")
            }
        }
    }

    // The replacement Xvfb comes up on the same display; `spawn_xvfb_on` blocks internally
    // until it accepts connections (its own readiness poll), so the very next attempt must
    // succeed.
    let _replacement = spawn_xvfb_on(display.clone());
    match reconnector.attempt(20) {
        ReconnectAttempt::Restored { .. } => {}
        ReconnectAttempt::StillDown { .. } => {
            panic!("the replacement Xvfb is ready; this attempt must succeed")
        }
        ReconnectAttempt::OutageOpened { .. } => {
            panic!("the outage was already open; must not report a second OutageOpened")
        }
    }
}

// ---------------------------------------------------------------------------
// Task 11.13 / 11.12: a transparent X11 wire proxy
// ---------------------------------------------------------------------------

/// The core-protocol opcode for `QueryExtension` — the only request this proxy inspects.
const QUERY_EXTENSION_OPCODE: u8 = 98;

/// `XSyncAlarmNotify` is SYNC's *second* event (`XSyncCounterNotify` is the first), so its
/// wire event number is `first_event + 1` — `x11rb`'s `sync::ALARM_NOTIFY_EVENT`.
const SYNC_ALARM_NOTIFY_OFFSET: u8 = 1;

/// X11 pads every variable-length field out to a 4-byte boundary.
fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// What one proxied connection has learned so far. Shared between the two directions.
#[derive(Default)]
struct ProxyState {
    /// Sequence numbers of `QueryExtension` requests whose reply must be rewritten to
    /// "absent".
    hidden_sequences: HashSet<u16>,
    /// Sequence number of the client's `QueryExtension("SYNC")`, so the matching reply can
    /// be read for the extension's `first_event` base.
    sync_query_sequence: Option<u16>,
    sync_first_event: Option<u8>,
    duplicated_alarm_notify: bool,
}

/// A transparent X11 proxy in front of a real Xvfb, listening on a display number of its
/// own. See this file's module doc for why it exists: `SYNC` cannot be removed from this
/// Xvfb, so RF-25's degradation chain can only be given real behavioural coverage by
/// answering the client's `QueryExtension` the way a server without the extension would.
///
/// It is a byte-level relay, not a parser: every request and every reply is forwarded
/// verbatim except the specific `QueryExtension` reply byte being rewritten, so the client
/// is talking to the real server over the real wire protocol throughout.
struct XProxy {
    display: String,
    socket_path: String,
    shutdown: Arc<AtomicBool>,
}

impl Drop for XProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Starts a proxy in front of `target_display`, reporting every extension named in `hide` as
/// absent. When `duplicate_alarm_notify` is set, the first `XSyncAlarmNotify` the server
/// sends is delivered to the client **twice** — task 11.12's deterministic stand-in for an
/// alarm event that was already queued when the source re-armed the alarm, which is exactly
/// what a real server does under the create->query window and which the client cannot tell
/// apart from a duplicate.
fn spawn_xproxy(target_display: &str, hide: &[&str], duplicate_alarm_notify: bool) -> XProxy {
    let listen_num = free_display_number();
    let socket_path = format!("/tmp/.X11-unix/X{listen_num}");
    let _ = std::fs::remove_file(&socket_path);
    let target_path = format!("/tmp/.X11-unix/X{}", target_display.trim_start_matches(':'));
    let hidden: Vec<Vec<u8>> = hide.iter().map(|name| name.as_bytes().to_vec()).collect();
    let shutdown = Arc::new(AtomicBool::new(false));

    let path_listener = UnixListener::bind(&socket_path).expect("bind the proxy X socket");
    // `x11rb` tries Linux's abstract namespace before the filesystem path
    // (`rust_connection::stream`: "Try abstract unix socket first"), so the proxy has to own
    // both names or the client connects straight past it to the real server.
    let abstract_address =
        SocketAddr::from_abstract_name(socket_path.as_bytes()).expect("abstract socket address");
    let abstract_listener =
        UnixListener::bind_addr(&abstract_address).expect("bind the abstract proxy X socket");

    for listener in [path_listener, abstract_listener] {
        listener
            .set_nonblocking(true)
            .expect("non-blocking accept so the guard can stop this thread");
        let hidden = hidden.clone();
        let target_path = target_path.clone();
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let hidden = hidden.clone();
                        let target_path = target_path.clone();
                        thread::spawn(move || {
                            proxy_one_connection(
                                client,
                                &target_path,
                                hidden,
                                duplicate_alarm_notify,
                            );
                        });
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
    }

    XProxy {
        display: format!(":{listen_num}"),
        socket_path,
        shutdown,
    }
}

fn proxy_one_connection(
    client: UnixStream,
    target_path: &str,
    hidden: Vec<Vec<u8>>,
    duplicate_alarm_notify: bool,
) {
    let Ok(server) = UnixStream::connect(target_path) else {
        return;
    };
    let state = Arc::new(Mutex::new(ProxyState::default()));
    let client_read = client.try_clone().expect("clone the client socket");
    let server_write = server.try_clone().expect("clone the server socket");
    let upstream_state = Arc::clone(&state);
    thread::spawn(move || {
        let _ = client_to_server(client_read, server_write, hidden, upstream_state);
    });
    let _ = server_to_client(server, client, duplicate_alarm_notify, state);
}

/// Client -> server. Counts request sequence numbers exactly as the server does (the first
/// request after the setup is sequence 1) and remembers which of them are `QueryExtension`
/// calls whose reply has to be rewritten.
fn client_to_server(
    mut client: UnixStream,
    mut server: UnixStream,
    hidden: Vec<Vec<u8>>,
    state: Arc<Mutex<ProxyState>>,
) -> std::io::Result<()> {
    // Connection setup request: a 12-byte header, then the padded authorization protocol
    // name and data.
    let mut header = [0u8; 12];
    client.read_exact(&mut header)?;
    let name_len = usize::from(u16::from_le_bytes([header[6], header[7]]));
    let data_len = usize::from(u16::from_le_bytes([header[8], header[9]]));
    let mut authorization = vec![0u8; pad4(name_len) + pad4(data_len)];
    client.read_exact(&mut authorization)?;
    server.write_all(&header)?;
    server.write_all(&authorization)?;
    server.flush()?;

    let mut sequence: u16 = 0;
    loop {
        let mut head = [0u8; 4];
        client.read_exact(&mut head)?;
        let length = usize::from(u16::from_le_bytes([head[2], head[3]]));
        let mut request = head.to_vec();
        let body_len = if length == 0 {
            // BIG-REQUESTS: the real length is the next four bytes, in 4-byte units, and it
            // counts the 8-byte header this form uses.
            let mut extended = [0u8; 4];
            client.read_exact(&mut extended)?;
            request.extend_from_slice(&extended);
            (u32::from_le_bytes(extended) as usize)
                .saturating_mul(4)
                .saturating_sub(8)
        } else {
            length.saturating_mul(4).saturating_sub(4)
        };
        let mut body = vec![0u8; body_len];
        client.read_exact(&mut body)?;
        sequence = sequence.wrapping_add(1);

        if head[0] == QUERY_EXTENSION_OPCODE && body.len() >= 4 {
            let name_len = usize::from(u16::from_le_bytes([body[0], body[1]]));
            if body.len() >= 4 + name_len {
                let name = &body[4..4 + name_len];
                let mut state = state.lock().expect("proxy state");
                if hidden.iter().any(|extension| extension == name) {
                    state.hidden_sequences.insert(sequence);
                }
                if name == b"SYNC" {
                    state.sync_query_sequence = Some(sequence);
                }
            }
        }

        request.extend_from_slice(&body);
        server.write_all(&request)?;
        server.flush()?;
    }
}

/// Server -> client. Rewrites the `present` byte of the `QueryExtension` replies the
/// upstream direction flagged, learns SYNC's `first_event` from its own reply, and
/// optionally delivers the first alarm notification twice.
fn server_to_client(
    mut server: UnixStream,
    mut client: UnixStream,
    duplicate_alarm_notify: bool,
    state: Arc<Mutex<ProxyState>>,
) -> std::io::Result<()> {
    // Connection setup reply: 8 bytes, then `additional_data_len` 4-byte units.
    let mut header = [0u8; 8];
    server.read_exact(&mut header)?;
    let extra = usize::from(u16::from_le_bytes([header[6], header[7]])).saturating_mul(4);
    let mut rest = vec![0u8; extra];
    server.read_exact(&mut rest)?;
    client.write_all(&header)?;
    client.write_all(&rest)?;
    client.flush()?;

    loop {
        let mut message = vec![0u8; 32];
        server.read_exact(&mut message)?;
        let kind = message[0];
        // A reply (type 1) and a GenericEvent (type 35) are the two messages that carry a
        // variable-length tail beyond the fixed 32 bytes.
        if kind == 1 || (kind & 0x7f) == 35 {
            let tail_len = (u32::from_le_bytes([message[4], message[5], message[6], message[7]])
                as usize)
                .saturating_mul(4);
            let mut tail = vec![0u8; tail_len];
            server.read_exact(&mut tail)?;
            message.extend_from_slice(&tail);
        }

        let mut duplicate = false;
        {
            let mut state = state.lock().expect("proxy state");
            if kind == 1 {
                let sequence = u16::from_le_bytes([message[2], message[3]]);
                if state.hidden_sequences.contains(&sequence) {
                    // `QueryExtension`'s reply: byte 8 is `present`, byte 10 `first_event`.
                    message[8] = 0;
                } else if state.sync_query_sequence == Some(sequence) {
                    state.sync_first_event = Some(message[10]);
                }
            } else if duplicate_alarm_notify && !state.duplicated_alarm_notify {
                if let Some(first_event) = state.sync_first_event {
                    if kind & 0x7f == first_event.saturating_add(SYNC_ALARM_NOTIFY_OFFSET) {
                        state.duplicated_alarm_notify = true;
                        duplicate = true;
                    }
                }
            }
        }

        client.write_all(&message)?;
        if duplicate {
            client.write_all(&message)?;
        }
        client.flush()?;
    }
}

// ---------------------------------------------------------------------------
// Phase 11 corrections (tasks 11.10-11.16)
// ---------------------------------------------------------------------------

/// **RED (tasks 11.10/11.11): the user comes back before the daemon has drained the alarm
/// that reported them idle.** The positive `AlarmNotify` is already queued on the client
/// socket when the input arrives, so by the time the source re-arms for the negative edge
/// `IDLETIME` is already below the threshold — the crossing is in the past and the server
/// will never report it (`ChangeAlarm` whose trigger is already satisfied notifies nothing
/// for a `Transition` test). Without a read tied to the arm action, `UserActive` never
/// arrives and the daemon stays latched in AFK while the user is typing.
///
/// This is the deterministic form of the ~2% loss measured against the real code path
/// (task 11.10): 1 of 60 trials of "connect, wait for `UserIdle`, return immediately" lost
/// the transition entirely, with `IDLETIME` confirming at the moment of failure that the
/// input really had reset the counter.
#[test]
fn a_return_that_races_the_queued_idle_alarm_still_produces_user_active() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(300))
            .expect("X11Source::connect_with_afk_threshold");

    // Let the alarm fire and its notification queue up on the socket, then return BEFORE
    // that notification is drained.
    std::thread::sleep(Duration::from_millis(600));
    wm.generate_activity();

    match wait_for_raw_event(&mut source) {
        RawEvent::UserIdle { .. } => {}
        other => panic!("expected the queued UserIdle first, got {other:?}"),
    }

    // The return already happened. Keep the user active so no later crossing can rescue
    // the daemon: the only correct source of `UserActive` here is the check tied to the
    // re-arm itself.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match source.poll_for_event().expect("poll_for_event") {
            Some(RawEvent::UserActive) => break,
            Some(other) => panic!("expected UserActive, got {other:?}"),
            None => {}
        }
        assert!(
            Instant::now() < deadline,
            "the user returned while the idle alarm was still queued; UserActive was never \
             surfaced, so the daemon is latched in AFK with the user typing"
        );
        wm.generate_activity();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// **RED (task 11.12): an alarm notification generated under one arming must not be read as
/// the opposite transition once the alarm has been re-armed.** A queued `AlarmNotify`
/// survives a later `ChangeAlarm`, so the local "which edge am I waiting for" field is not a
/// safe classifier — the event's own `counter_value` is. The proxy delivers the first
/// notification twice, which is indistinguishable at the client from the real
/// create->query window that produced one spurious `UserActive` in 500 startups.
#[test]
fn a_duplicated_alarm_notification_never_fabricates_user_active() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    let proxy = spawn_xproxy(&xvfb.display, &[], true);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&proxy.display), Duration::from_millis(300))
            .expect("X11Source::connect_with_afk_threshold through the proxy");

    match wait_for_raw_event(&mut source) {
        RawEvent::UserIdle { .. } => {}
        other => panic!("expected UserIdle from the real positive transition, got {other:?}"),
    }

    // The duplicate is already on the socket, delivered after the source re-armed for the
    // negative edge. Nobody has touched the keyboard since, so no UserActive may appear.
    let event = wait_for_raw_event(&mut source);
    match event {
        RawEvent::UserIdle { idle_for } => assert!(
            idle_for.as_millis() >= 300,
            "the duplicate carries the idle value it was generated with, got {idle_for:?}"
        ),
        RawEvent::UserActive => panic!(
            "the duplicated alarm notification was classified from the local `armed` field \
             instead of its own counter_value, fabricating a UserActive while the user is \
             still idle"
        ),
        other => panic!("expected the duplicate to be read as UserIdle, got {other:?}"),
    }
}

/// **Task 11.13, RF-25 step 2: `SYNC` genuinely absent, `MIT-SCREEN-SAVER` present.** The
/// proxy answers `QueryExtension(SYNC)` exactly as a server without the extension does, so
/// this exercises the real selection branch, the real 30s-cadence poll's body, and the real
/// `XScreenSaverQueryInfo` wire call — not only `select_degradation_diagnostic`'s strings.
#[test]
fn sync_absent_degrades_to_a_real_screensaver_poll() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    let proxy = spawn_xproxy(&xvfb.display, &["SYNC"], false);

    let (mut source, diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&proxy.display), Duration::from_millis(700))
            .expect("startup must not be blocked by SYNC being unavailable");

    let degradations: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("MIT-SCREEN-SAVER"))
        .collect();
    assert_eq!(
        degradations.len(),
        1,
        "exactly one degradation diagnostic was expected, got {diagnostics:?}"
    );
    assert!(
        degradations[0].contains("SYNC/IDLETIME unavailable"),
        "the diagnostic must name what degraded, got {:?}",
        degradations[0]
    );

    // Freshly active: the poll must report nothing at all.
    wm.generate_activity();
    assert!(
        source
            .poll_screensaver_idle()
            .expect("poll_screensaver_idle")
            .is_none(),
        "the user is active; the poll must report no transition"
    );

    // Past the threshold: one UserIdle carrying a real ms_since_user_input reading.
    std::thread::sleep(Duration::from_millis(900));
    match source
        .poll_screensaver_idle()
        .expect("poll_screensaver_idle")
    {
        Some(RawEvent::UserIdle { idle_for }) => assert!(
            idle_for.as_millis() >= 700,
            "idle_for {idle_for:?} must be at least the threshold"
        ),
        other => panic!("expected UserIdle past the threshold, got {other:?}"),
    }

    // Still idle: the transition already happened, so the next poll reports nothing.
    assert!(
        source
            .poll_screensaver_idle()
            .expect("poll_screensaver_idle")
            .is_none(),
        "still idle; UserIdle must not re-fire on every poll"
    );

    // And the return is reported exactly once.
    wm.generate_activity();
    match source
        .poll_screensaver_idle()
        .expect("poll_screensaver_idle")
    {
        Some(RawEvent::UserActive) => {}
        other => panic!("expected UserActive once input resumed, got {other:?}"),
    }
    assert!(
        source
            .poll_screensaver_idle()
            .expect("poll_screensaver_idle")
            .is_none(),
        "still active; UserActive must not re-fire on every poll"
    );
}

/// **Task 11.13, RF-25 step 3: neither extension available.** `MIT-SCREEN-SAVER` is removed
/// from the server itself (`-extension MIT-SCREEN-SAVER`, which this Xvfb does honour) and
/// `SYNC` is hidden by the proxy, so both probes fail for real. The daemon must still start,
/// emit exactly one warning naming `logind` as what is left, and leave the idle path inert.
#[test]
fn neither_idle_extension_available_disables_absence_detection_without_blocking_startup() {
    let display_num = free_display_number();
    let xvfb = spawn_xvfb_with_args(
        format!(":{display_num}"),
        &["-extension", "MIT-SCREEN-SAVER"],
    );
    let proxy = spawn_xproxy(&xvfb.display, &["SYNC"], false);

    let started = Instant::now();
    let (mut source, diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&proxy.display), Duration::from_millis(300))
            .expect("neither extension may block startup (RF-25 step 3)");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "startup must not block waiting for absence detection, took {:?}",
        started.elapsed()
    );

    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("absence detection is disabled"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one startup warning was expected, got {diagnostics:?}"
    );
    assert!(
        warnings[0].contains("logind"),
        "the warning must say what absence detection falls back to, got {:?}",
        warnings[0]
    );

    // Inert, not broken: the polling entry point is a no-op and nothing is ever reported.
    assert!(source
        .poll_screensaver_idle()
        .expect("poll_screensaver_idle must stay a no-op, never an error")
        .is_none());
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        source
            .poll_screensaver_idle()
            .expect("poll_screensaver_idle")
            .is_none(),
        "with absence detection disabled, no idle transition may ever be reported"
    );
}

/// **Task 11.15 (mutation pin): `Reconnector`'s own bookkeeping, asserted through
/// `Reconnector`.** Deleting `self.backoff.reset()` or `self.outage_open = false` from the
/// success branch left the whole suite green; the second is RF-6's exact inversion, where
/// every outage after the first reports `StillDown` and opens no `unknown` interval at all.
/// `entropy = 20` lands on 0% jitter (`20 % 41 - 20`), so the delays are exact.
#[test]
fn reconnector_resets_backoff_and_reopens_the_outage_on_the_second_outage() {
    let xvfb = spawn_xvfb();
    let display = xvfb.display.clone();
    drop(xvfb);

    let mut reconnector = Reconnector::new(Some(&display), Duration::from_secs(240));

    match reconnector.attempt(20) {
        ReconnectAttempt::OutageOpened { retry_after } => {
            assert_eq!(retry_after, Duration::from_millis(500))
        }
        _ => panic!("the first failed attempt must open the outage at the base delay"),
    }
    match reconnector.attempt(20) {
        ReconnectAttempt::StillDown { retry_after } => {
            assert_eq!(
                retry_after,
                Duration::from_secs(1),
                "the backoff must have advanced within this outage"
            )
        }
        _ => panic!("the outage is already open; a second failure is StillDown"),
    }

    let first_server = spawn_xvfb_on(display.clone());
    match reconnector.attempt(20) {
        ReconnectAttempt::Restored {
            outage_was_open, ..
        } => assert!(
            outage_was_open,
            "this recovery closes an outage the caller was already told about"
        ),
        _ => panic!("the replacement Xvfb is ready; this attempt must succeed"),
    }
    drop(first_server);

    // A second, independent outage. RF-32: it starts over at the base delay, and RF-6: it
    // opens its own `unknown` interval rather than reporting StillDown forever.
    match reconnector.attempt(20) {
        ReconnectAttempt::OutageOpened { retry_after } => assert_eq!(
            retry_after,
            Duration::from_millis(500),
            "a successful reconnection resets the backoff for the next outage"
        ),
        ReconnectAttempt::StillDown { .. } => panic!(
            "the previous outage was closed by a successful reconnection; this new outage \
             must open its own, or RF-6 never opens a second `unknown` interval"
        ),
        ReconnectAttempt::Restored { .. } => panic!("the server was killed; this must fail"),
    }
}

/// **RED (task 11.16, RF-6): a reconnection that succeeds on the very first attempt.** The
/// caller was never told an outage opened, so it still owes RF-6 its "close the current
/// interval and open exactly one `unknown` interval" — which it can only know from the
/// outcome itself.
#[test]
fn a_first_attempt_that_succeeds_reports_that_no_outage_was_opened() {
    let xvfb = spawn_xvfb();
    let mut reconnector = Reconnector::new(Some(&xvfb.display), Duration::from_secs(240));

    match reconnector.attempt(20) {
        ReconnectAttempt::Restored {
            outage_was_open, ..
        } => assert!(
            !outage_was_open,
            "no OutageOpened preceded this success, so the caller still owes RF-6 the one \
             `unknown` interval for this outage"
        ),
        _ => panic!("the display is up; the first attempt must succeed"),
    }
}

/// Waits for `UserIdle` or `UserActive`, tolerating a repeat of the state the source is
/// already in. A duplicate is legitimate — the arm-time check and a real alarm fire can both
/// report the same transition when input lands between them, and `Tracker` ignores a
/// transition it is already in — whereas the *wrong* transition, or none at all, is the
/// defect this helper's callers are pinning.
fn wait_for_idle_or_active(source: &mut X11Source, want_idle: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match source.poll_for_event().expect("poll_for_event") {
            Some(RawEvent::UserIdle { .. }) if want_idle => return,
            Some(RawEvent::UserActive) if !want_idle => return,
            Some(RawEvent::UserIdle { .. }) | Some(RawEvent::UserActive) => {}
            Some(other) => {
                panic!("unexpected event while waiting for an idle transition: {other:?}")
            }
            None => {}
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {} timed out: the transition was never reported",
            if want_idle { "UserIdle" } else { "UserActive" }
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **RED (task 11.10): fifteen tight away-and-back cycles, where the user returns the
/// instant the daemon learns they were away.**
///
/// That is the shape that loses the return: the away alarm fires at exactly the threshold,
/// the return edge gets armed against that same value, and the crossing that follows is
/// never reported. Measured against the real code path, one cycle per trial, 120 trials
/// each: **8 returns lost before the fix (6.7%), 0 after it**. Against a raw `x11rb` probe
/// with a tighter re-arm the pre-fix rate was 12 in 60, and every lost trial was one whose
/// away alarm had fired at exactly the threshold.
///
/// **This pin is statistical, and weakly so: re-applying the pre-fix trigger formulation
/// failed it in 2 runs out of 6.** Fifteen cycles are a cheap net, not a proof. The
/// deterministic pin for the same root cause is
/// `a_return_that_races_the_queued_idle_alarm_still_produces_user_active` above, which the
/// pre-fix code failed every time; this one exists because that test cannot reach the
/// exactly-at-the-threshold arming the server loses.
#[test]
fn fifteen_tight_away_and_back_cycles_never_lose_the_return() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(150))
            .expect("X11Source::connect_with_afk_threshold");

    for cycle in 0..15 {
        wait_for_idle_or_active(&mut source, true);
        // The user comes back the moment the daemon reports them away — no pause at all,
        // which is exactly when the re-arm and the input race each other.
        wm.generate_activity();
        let started = Instant::now();
        wait_for_idle_or_active(&mut source, false);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cycle {cycle}: the return took {:?}",
            started.elapsed()
        );
    }
}

/// **Task 11.10 (RNF-2, idle-detection "No polling while waiting for idle or activity"): the
/// return edge is level-triggered, and must still report exactly once.**
///
/// `alarm_trigger_value` arms the return edge as a `NegativeComparison`, which is true for as
/// long as the user is active rather than only at the instant they come back. If that
/// re-triggered while the condition held, the daemon would wake continuously for as long as
/// somebody is typing — the exact opposite of what RF-4 exists to do, and invisible to every
/// other test here, which stop polling as soon as they get the event they wanted. A SYNC
/// alarm with `delta = 0` goes inactive when it triggers; this pins that it really does.
#[test]
fn the_return_edge_reports_once_and_then_goes_quiet() {
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(&xvfb.display);
    wm.generate_activity();

    let (mut source, _diagnostics) =
        X11Source::connect_with_afk_threshold(Some(&xvfb.display), Duration::from_millis(300))
            .expect("X11Source::connect_with_afk_threshold");

    wait_for_idle_or_active(&mut source, true);
    wm.generate_activity();
    wait_for_idle_or_active(&mut source, false);

    // The user keeps typing: the return condition stays true the whole time, and the away
    // alarm cannot fire because the counter never climbs to the threshold. Nothing at all
    // may be reported.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        wm.generate_activity();
        if let Some(event) = source.poll_for_event().expect("poll_for_event") {
            panic!(
                "the return edge re-triggered while the user was still active: {event:?} — \
                 the daemon would wake for as long as somebody keeps typing"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
