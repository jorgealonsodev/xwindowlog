//! Phase 9-11 `x11.rs` E2E tests: a real Xvfb, real `x11rb` wire protocol, no mocked
//! connection. Focused command for this slice (tasks.md Phase 9 row):
//! `cargo test --test x11_integration -- ewmh`.
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

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use xwindowlog::exclude::Excluder;
use xwindowlog::x11::{CaptureMode, RawEvent, X11Source};

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
    let display_num = DISPLAY_BASE + NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::SeqCst);
    let display = format!(":{display_num}");
    let child = Command::new("Xvfb")
        .arg(&display)
        .args(["-screen", "0", "320x240x24", "-nolisten", "tcp"])
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
