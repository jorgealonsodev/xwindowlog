//! Phase 15 `main.rs` composition E2E tests: a real compiled `xwindowlog` binary, real
//! processes, real signals, a real `Xvfb` (tasks.md Phase 15's Work Units row). Focused
//! command for this slice: `cargo test --test daemon_e2e -- flock`.
//!
//! Each test gets its own `$XDG_RUNTIME_DIR`/`$XDG_DATA_HOME`/`$XDG_CONFIG_HOME` (fresh scratch
//! directories) and its own `Xvfb` display, so daemon instances from different tests never
//! contend on the same lock file or window server — matching `x11_integration.rs`'s own
//! per-test-isolation precedent, and reusing its `openbox`-absence rationale (this file's
//! module doc for the deviation, `x11_integration.rs`'s for the full history): a second,
//! plain `x11rb` connection performs exactly the requests a real EWMH window manager would.
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

// ---------------------------------------------------------------------------------------------
// Xvfb + FakeWm harness (mirrors tests/x11_integration.rs's own, trimmed to what this file's
// daemon-level tests need: one window, EWMH declared, an active window, and a title).
// ---------------------------------------------------------------------------------------------

static NEXT_DISPLAY_OFFSET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Kept well clear of `x11_integration.rs`'s own `DISPLAY_BASE` (213+) and any developer
/// desktop session, so the two test binaries never collide even when run concurrently.
const DISPLAY_BASE: u32 = 313;

struct XvfbGuard {
    child: Child,
    display: String,
    /// Kept alive for the guard's whole lifetime — see `x11_integration.rs`'s `XvfbGuard` doc
    /// for why dropping the readiness-probe connection risks a close-down-reset race.
    _keepalive: RustConnection,
}

impl XvfbGuard {
    fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_display_number() -> u32 {
    loop {
        let n =
            DISPLAY_BASE + NEXT_DISPLAY_OFFSET.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if x11rb::connect(Some(&format!(":{n}"))).is_err() {
            return n;
        }
    }
}

fn spawn_xvfb() -> XvfbGuard {
    let display = format!(":{}", free_display_number());
    let child = Command::new("Xvfb")
        .arg(&display)
        .args(["-screen", "0", "320x240x24", "-nolisten", "tcp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb must be installed for tests/daemon_e2e.rs");

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

/// A minimal stand-in for a real EWMH window manager — see this file's module doc.
struct FakeWm {
    conn: RustConnection,
    root: Window,
    net_supported: u32,
    net_supporting_wm_check: u32,
    net_active_window: u32,
    net_wm_name: u32,
    utf8_string: u32,
    wm_class: u32,
}

impl FakeWm {
    fn connect(display: &str) -> Self {
        let (conn, screen_num) = x11rb::connect(Some(display)).expect("fake WM connect");
        let root = conn.setup().roots[screen_num].root;
        FakeWm {
            net_supported: intern(&conn, b"_NET_SUPPORTED"),
            net_supporting_wm_check: intern(&conn, b"_NET_SUPPORTING_WM_CHECK"),
            net_active_window: intern(&conn, b"_NET_ACTIVE_WINDOW"),
            net_wm_name: intern(&conn, b"_NET_WM_NAME"),
            utf8_string: intern(&conn, b"UTF8_STRING"),
            wm_class: intern(&conn, b"WM_CLASS"),
            conn,
            root,
        }
    }

    /// `app_id` (`x11.rs::read_app_id`) reads `WM_CLASS`'s second (class) component.
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

    fn declare_ewmh_supported(&self) {
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
                &[self.net_active_window, self.net_supporting_wm_check],
            )
            .expect("set _NET_SUPPORTED request")
            .check()
            .expect("set _NET_SUPPORTED reply");
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

    fn map_window(&self, window: Window) {
        self.conn
            .map_window(window)
            .expect("map_window request")
            .check()
            .expect("map_window reply");
        self.conn.flush().expect("flush");
    }

    fn set_active_window(&self, window: Window) {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.net_active_window,
                AtomEnum::WINDOW,
                &[window],
            )
            .expect("set _NET_ACTIVE_WINDOW request")
            .check()
            .expect("set _NET_ACTIVE_WINDOW reply");
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
}

// ---------------------------------------------------------------------------------------------
// Scratch XDG directories and daemon process management
// ---------------------------------------------------------------------------------------------

/// A scratch directory, removed on drop. Used for `$XDG_RUNTIME_DIR`, `$XDG_CONFIG_HOME` and
/// `$XDG_DATA_HOME` alike — none of the tests in this file depend on real `pam_systemd`-style
/// permissions, only on each daemon instance getting its own isolated set of paths.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xwindowlog-test-scratch-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch directory must succeed");
        ScratchDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every scratch directory + display a spawned daemon needs, held together so each test only
/// tracks one value; also fixes the display each daemon connects to.
struct DaemonEnv {
    runtime_dir: ScratchDir,
    config_home: ScratchDir,
    data_home: ScratchDir,
    xvfb: XvfbGuard,
}

impl DaemonEnv {
    fn new(label: &str) -> Self {
        DaemonEnv {
            runtime_dir: ScratchDir::new(&format!("{label}-runtime")),
            config_home: ScratchDir::new(&format!("{label}-config")),
            data_home: ScratchDir::new(&format!("{label}-data")),
            xvfb: spawn_xvfb(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.runtime_dir.path().join("xwindowlog.lock")
    }

    fn db_path(&self) -> PathBuf {
        self.data_home
            .path()
            .join("xwindowlog")
            .join("xwindowlog.db")
    }
}

/// A running `xwindowlog daemon` child, killed on drop so a failing assertion never leaks a
/// process holding a flock (or an Xvfb connection) into the next test.
struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(env: &DaemonEnv) -> DaemonChild {
    let child = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", env.runtime_dir.path())
        .env("XDG_CONFIG_HOME", env.config_home.path())
        .env("XDG_DATA_HOME", env.data_home.path())
        .env("DISPLAY", env.xvfb.display())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the compiled xwindowlog binary must succeed");
    DaemonChild(child)
}

/// Polls until `path` exists (the lock file is created as part of `flock` acquisition), bounded
/// so a genuine startup failure fails the test instead of hanging it.
fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "{} never appeared within {timeout:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- 15.3/15.4: a second instance refuses to start while the first holds the lock -----------

#[test]
fn second_daemon_instance_exits_nonzero_while_the_first_holds_the_lock() {
    let env = DaemonEnv::new("second-instance");
    let _first = spawn_daemon(&env);
    wait_for_file(&env.lock_path(), Duration::from_secs(5));
    // The first instance holds an exclusive, non-blocking lock the instant the file exists
    // (`acquire_lock` creates then immediately locks it), so a short settle is enough to avoid
    // a race against the open()-then-flock() gap.
    std::thread::sleep(Duration::from_millis(100));

    let mut second = spawn_daemon(&env);
    let status = second
        .0
        .wait()
        .expect("waiting on the second instance must succeed");

    assert!(
        !status.success(),
        "a second instance must not start while the first holds the lock"
    );
    assert_eq!(
        status.code(),
        Some(2),
        "cli-reporting: a second daemon invocation exits 2 (a state error), not a generic 1"
    );

    let mut stderr = String::new();
    second
        .0
        .stderr
        .take()
        .expect("stderr must be piped")
        .read_to_string(&mut stderr)
        .expect("reading stderr must succeed");
    assert!(
        stderr.to_lowercase().contains("already running"),
        "the second instance must report a clear reason, got: {stderr:?}"
    );
}

// --- 15.3/15.4: a SIGKILLed instance's lock is released automatically -----------------------

#[test]
fn a_sigkilled_instances_lock_is_released_automatically() {
    let env = DaemonEnv::new("sigkill-lock-release");

    {
        let mut first = spawn_daemon(&env);
        wait_for_file(&env.lock_path(), Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(100));
        // SIGKILL, not a graceful terminate: the kernel — not this process's own cleanup —
        // must be what releases the flock (daemon-lifecycle "A crashed instance's lock is
        // released automatically").
        first.0.kill().expect("SIGKILL must succeed");
        first
            .0
            .wait()
            .expect("waiting on the killed instance must succeed");
    }

    let mut second = spawn_daemon(&env);
    wait_for_file(&env.lock_path(), Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(200));
    let status = second.0.try_wait().expect("try_wait must not error");
    assert_eq!(
        status, None,
        "a fresh instance must start normally (and keep running) once the crashed \
         predecessor's lock is released by the kernel"
    );
}

// ---------------------------------------------------------------------------------------------
// 15.5/15.6: SIGTERM/SIGINT close the currently open interval and exit 0
// ---------------------------------------------------------------------------------------------

struct IntervalRow {
    end: Option<i64>,
    app: String,
    title: String,
    state: String,
}

/// Reads the most recently opened interval, joined against its dictionary tables — a plain
/// read-only `rusqlite` connection against the file the daemon itself wrote, with no shared
/// production code path (this test must not trust the same code it is verifying).
fn most_recent_interval(db_path: &std::path::Path) -> rusqlite::Result<IntervalRow> {
    let conn = rusqlite::Connection::open(db_path)?;
    conn.query_row(
        "SELECT intervals.\"end\", apps.app_id, titles.title, intervals.state \
         FROM intervals \
         JOIN apps ON apps.id = intervals.app \
         JOIN titles ON titles.id = intervals.title \
         ORDER BY intervals.start DESC, intervals.id DESC \
         LIMIT 1",
        [],
        |r| {
            Ok(IntervalRow {
                end: r.get(0)?,
                app: r.get(1)?,
                title: r.get(2)?,
                state: r.get(3)?,
            })
        },
    )
}

/// Whether ANY interval row (not only the most recent) matches `(app, title)` exactly — used to
/// prove a historical row survives untouched (RF-9: "without affecting already-recorded
/// intervals").
fn any_interval_matches(
    db_path: &std::path::Path,
    app: &str,
    title: &str,
) -> rusqlite::Result<bool> {
    let conn = rusqlite::Connection::open(db_path)?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM intervals \
         JOIN apps ON apps.id = intervals.app \
         JOIN titles ON titles.id = intervals.title \
         WHERE apps.app_id = ?1 AND titles.title = ?2",
        rusqlite::params![app, title],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Creates one titled, classed window and repeatedly (re-)asserts it as
/// `_NET_ACTIVE_WINDOW` until the daemon's own database shows the resulting `active` interval,
/// bounded by `timeout`. The daemon is a separate process started slightly before this call
/// returns control to the caller — its own connect/subscribe sequence (RF-1) has no externally
/// observable "ready" signal, and a `PropertyNotify` sent before the daemon subscribes is never
/// replayed. Re-asserting the same property is a real, repeatable X11 write (every
/// `PropMode::REPLACE` generates a fresh event regardless of the previous value), so this is
/// the stimulus retried, not a passive poll of an already-fired event.
fn activate_window_until_observed(
    env: &DaemonEnv,
    wm: &FakeWm,
    app_id: &str,
    title: &str,
    timeout: Duration,
) -> Window {
    let window = wm.create_window();
    wm.map_window(window);
    wm.set_wm_class(window, app_id, app_id);
    wm.set_title(window, title);

    let deadline = Instant::now() + timeout;
    loop {
        wm.set_active_window(window);
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(row) = most_recent_interval(&env.db_path()) {
            if row.app == app_id && row.title == title && row.state == "active" {
                return window;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no active interval for app={app_id:?} appeared in {} within {timeout:?}",
            env.db_path().display()
        );
    }
}

fn send_signal(pid: u32, signal: nix::sys::signal::Signal) {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal)
        .expect("sending the signal must succeed");
}

/// Shared body for the SIGTERM/SIGINT scenarios (daemon-lifecycle: "the same closure, commit,
/// release, and exit-code-0 behavior as the SIGTERM scenario occurs").
fn assert_signal_closes_the_open_interval_and_exits_zero(
    label: &str,
    signal: nix::sys::signal::Signal,
) {
    let env = DaemonEnv::new(label);
    // EWMH compliance is decided once, at the daemon's own connect() time (RF-24) — the fake
    // WM must declare it BEFORE the daemon starts, or the daemon falls back to
    // GetInputFocus polling for its entire lifetime, exactly as a real late-starting WM would.
    let wm = FakeWm::connect(env.xvfb.display());
    wm.declare_ewmh_supported();

    let mut daemon = spawn_daemon(&env);
    wait_for_file(&env.lock_path(), Duration::from_secs(5));

    activate_window_until_observed(
        &env,
        &wm,
        "example-app",
        "Example Doc",
        Duration::from_secs(10),
    );

    send_signal(daemon.0.id(), signal);
    let status = daemon
        .0
        .wait()
        .expect("waiting on the signaled daemon must succeed");

    assert_eq!(
        status.code(),
        Some(0),
        "RF-33: a clean signal shutdown exits 0"
    );

    let row = most_recent_interval(&env.db_path()).expect("an interval row must exist");
    assert_eq!(
        row.state, "active",
        "the active interval must not be re-opened as anything else"
    );
    assert!(
        row.end.is_some(),
        "the open interval must be closed (end IS NOT NULL) on a clean signal shutdown"
    );
}

#[test]
fn sigterm_closes_the_open_interval_and_exits_zero() {
    assert_signal_closes_the_open_interval_and_exits_zero(
        "sigterm-clean-shutdown",
        nix::sys::signal::Signal::SIGTERM,
    );
}

#[test]
fn sigint_behaves_identically_to_sigterm() {
    assert_signal_closes_the_open_interval_and_exits_zero(
        "sigint-clean-shutdown",
        nix::sys::signal::Signal::SIGINT,
    );
}

// ---------------------------------------------------------------------------------------------
// 15.7/15.8: editing config.toml and sending SIGHUP applies new exclusion rules to subsequent
// captures without altering already-recorded intervals (RF-9)
// ---------------------------------------------------------------------------------------------

#[test]
fn sighup_applies_new_exclusion_rules_without_altering_past_intervals() {
    let env = DaemonEnv::new("sighup-reload");
    let wm = FakeWm::connect(env.xvfb.display());
    wm.declare_ewmh_supported();

    let mut daemon = spawn_daemon(&env);
    wait_for_file(&env.lock_path(), Duration::from_secs(5));

    // 1. Captured with the empty (no-exclusion) startup config: recorded in the clear.
    activate_window_until_observed(
        &env,
        &wm,
        "secret-app",
        "Before Hup",
        Duration::from_secs(10),
    );

    // 2. Edit config.toml to exclude `secret-app`, then SIGHUP.
    let config_dir = env.config_home.path().join("xwindowlog");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "[[exclude]]\napp = \"secret-app\"\nhide_app = true\n",
    )
    .expect("write config.toml");
    send_signal(daemon.0.id(), nix::sys::signal::Signal::SIGHUP);

    // 3. A NEW window from the same app, activated after the reload, must be captured hidden —
    // retried because SIGHUP's own delivery/reload is not synchronous from this process's view.
    let deadline = Instant::now() + Duration::from_secs(10);
    let window = wm.create_window();
    wm.map_window(window);
    wm.set_wm_class(window, "secret-app", "secret-app");
    wm.set_title(window, "After Hup");
    loop {
        wm.set_active_window(window);
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(row) = most_recent_interval(&env.db_path()) {
            if row.app == "[hidden]" && row.title == "[hidden]" && row.state == "active" {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no hidden active interval appeared within 10s of sending SIGHUP with an updated \
             exclusion rule"
        );
    }

    // 4. The pre-SIGHUP interval must survive untouched — RF-9's "without affecting
    // already-recorded intervals".
    assert!(
        any_interval_matches(&env.db_path(), "secret-app", "Before Hup")
            .expect("query must succeed"),
        "the interval recorded before the config reload must not be rewritten or removed"
    );

    send_signal(daemon.0.id(), nix::sys::signal::Signal::SIGTERM);
    let status = daemon.0.wait().expect("wait");
    assert_eq!(status.code(), Some(0));
}
