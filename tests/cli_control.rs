//! Real-binary coverage for the Phase 17 `pause`/`resume` CLI control paths.
//!
//! The test runs a compiled daemon against an isolated Xvfb and asks the
//! compiled CLI to pause it. SQLite is queried read-only afterwards so the
//! observed state must have been written by the daemon, not by the CLI.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const DISPLAY_BASE: u32 = 413;
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "xwindowlog-cli-control-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("runtime")).expect("create runtime directory");
        std::fs::create_dir_all(root.join("config")).expect("create config directory");
        std::fs::create_dir_all(root.join("data")).expect("create data directory");
        Scratch { root }
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    fn lock_path(&self) -> PathBuf {
        self.runtime_dir().join("xwindowlog.lock")
    }

    fn database_path(&self) -> PathBuf {
        self.data_home().join("xwindowlog").join("xwindowlog.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct XvfbGuard {
    child: Child,
    display: String,
    // Keep the readiness connection alive for the duration of the test so Xvfb
    // does not perform a close-down reset between readiness and daemon startup.
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
        let number = DISPLAY_BASE + NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::SeqCst);
        if x11rb::connect(Some(&format!(":{number}"))).is_err() {
            return number;
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
        .expect("Xvfb must be installed for CLI control tests");

    let deadline = Instant::now() + Duration::from_secs(10);
    let keepalive = loop {
        if let Ok((connection, _screen)) = x11rb::connect(Some(&display)) {
            break connection;
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

fn intern(connection: &RustConnection, name: &[u8]) -> u32 {
    connection
        .intern_atom(false, name)
        .expect("intern_atom request")
        .reply()
        .expect("intern_atom reply")
        .atom
}

struct FakeWm {
    connection: RustConnection,
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
        let (connection, screen_number) =
            x11rb::connect(Some(display)).expect("fake WM connection");
        let root = connection.setup().roots[screen_number].root;
        FakeWm {
            net_supported: intern(&connection, b"_NET_SUPPORTED"),
            net_supporting_wm_check: intern(&connection, b"_NET_SUPPORTING_WM_CHECK"),
            net_active_window: intern(&connection, b"_NET_ACTIVE_WINDOW"),
            net_wm_name: intern(&connection, b"_NET_WM_NAME"),
            utf8_string: intern(&connection, b"UTF8_STRING"),
            wm_class: intern(&connection, b"WM_CLASS"),
            connection,
            root,
        }
    }

    fn declare_ewmh_supported(&self) {
        let check_window = self.create_window();
        for target in [self.root, check_window] {
            self.connection
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
        self.connection
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
        self.connection.flush().expect("flush EWMH properties");
    }

    fn create_window(&self) -> Window {
        let window = self.connection.generate_id().expect("generate window id");
        self.connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
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
            .expect("create window request")
            .check()
            .expect("create window reply");
        self.connection.flush().expect("flush create window");
        window
    }

    fn prepare_window(&self) -> Window {
        let window = self.create_window();
        self.connection
            .map_window(window)
            .expect("map window request")
            .check()
            .expect("map window reply");

        let mut wm_class = b"cli-pause-app".to_vec();
        wm_class.push(0);
        wm_class.extend_from_slice(b"cli-pause-app");
        wm_class.push(0);
        self.connection
            .change_property8(
                PropMode::REPLACE,
                window,
                self.wm_class,
                AtomEnum::STRING,
                &wm_class,
            )
            .expect("set WM_CLASS request")
            .check()
            .expect("set WM_CLASS reply");
        self.connection
            .change_property8(
                PropMode::REPLACE,
                window,
                self.net_wm_name,
                self.utf8_string,
                b"CLI pause test",
            )
            .expect("set _NET_WM_NAME request")
            .check()
            .expect("set _NET_WM_NAME reply");
        self.connection.flush().expect("flush window properties");
        window
    }

    fn activate(&self, window: Window) {
        self.connection
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .expect("set input focus request")
            .check()
            .expect("set input focus reply");
        self.connection
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
        self.connection.flush().expect("flush active window");
    }
}

struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(scratch: &Scratch, xvfb: &XvfbGuard) -> DaemonChild {
    let child = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env("DISPLAY", xvfb.display())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the compiled xwindowlog daemon");
    DaemonChild(child)
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct IntervalRow {
    end: Option<i64>,
    app: String,
    title: String,
    state: String,
}

fn most_recent_interval(db_path: &Path) -> rusqlite::Result<IntervalRow> {
    let connection = rusqlite::Connection::open(db_path)?;
    connection.query_row(
        "SELECT intervals.\"end\", apps.app_id, titles.title, intervals.state \
         FROM intervals \
         JOIN apps ON apps.id = intervals.app \
         JOIN titles ON titles.id = intervals.title \
         ORDER BY intervals.start DESC, intervals.id DESC \
         LIMIT 1",
        [],
        |row| {
            Ok(IntervalRow {
                end: row.get(0)?,
                app: row.get(1)?,
                title: row.get(2)?,
                state: row.get(3)?,
            })
        },
    )
}

fn active_interval_is_closed(db_path: &Path) -> rusqlite::Result<bool> {
    let connection = rusqlite::Connection::open(db_path)?;
    let end: Option<i64> = connection.query_row(
        "SELECT intervals.\"end\" \
         FROM intervals \
         JOIN apps ON apps.id = intervals.app \
         WHERE apps.app_id = 'cli-pause-app' AND intervals.state = 'active' \
         ORDER BY intervals.start DESC, intervals.id DESC \
         LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    Ok(end.is_some())
}

fn paused_interval_is_closed(db_path: &Path) -> rusqlite::Result<bool> {
    let connection = rusqlite::Connection::open(db_path)?;
    let end: Option<i64> = connection.query_row(
        "SELECT intervals.\"end\" \
         FROM intervals \
         WHERE intervals.state = 'paused' \
         ORDER BY intervals.start DESC, intervals.id DESC \
         LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    Ok(end.is_some())
}

fn activate_window_until_observed(scratch: &Scratch, wm: &FakeWm, timeout: Duration) -> Window {
    let window = wm.prepare_window();
    let deadline = Instant::now() + timeout;
    loop {
        wm.activate(window);
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(row) = most_recent_interval(&scratch.database_path()) {
            if row.app == "cli-pause-app" && row.title == "CLI pause test" && row.state == "active"
            {
                return window;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not record the active test window within {timeout:?}"
        );
    }
}

fn run_pause(scratch: &Scratch) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("pause")
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("run the compiled xwindowlog pause command")
}

fn run_pause_for_minutes(scratch: &Scratch, minutes: u32) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .args(["pause", "--minutes", &minutes.to_string()])
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("run the compiled xwindowlog pause --minutes command")
}

fn run_resume(scratch: &Scratch) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("resume")
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("run the compiled xwindowlog resume command")
}

fn wait_for_paused_interval(scratch: &Scratch, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(row) = most_recent_interval(&scratch.database_path()) {
            if row.state == "paused" && row.end.is_none() {
                assert!(
                    active_interval_is_closed(&scratch.database_path())
                        .expect("read active interval state"),
                    "pause must close the active interval before opening paused"
                );
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not record an open paused interval within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_resumed_interval(scratch: &Scratch, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(row) = most_recent_interval(&scratch.database_path()) {
            if row.state == "unknown" && row.end.is_none() {
                assert!(
                    paused_interval_is_closed(&scratch.database_path())
                        .expect("read paused interval state"),
                    "resume must close the open paused interval before opening unknown"
                );
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not record an open unknown interval within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pause_cli_reaches_running_daemon_and_opens_a_paused_interval() {
    let scratch = Scratch::new("dispatch");
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(xvfb.display());
    wm.declare_ewmh_supported();

    let _daemon = spawn_daemon(&scratch, &xvfb);
    wait_for_file(&scratch.lock_path(), Duration::from_secs(5));
    activate_window_until_observed(&scratch, &wm, Duration::from_secs(10));

    let output = run_pause(&scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pause should succeed against a running daemon; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert_eq!(stdout, "xwindowlog: paused\n");
    assert!(
        stderr.is_empty(),
        "successful pause should keep diagnostics off stdout and emit no stderr: {stderr:?}"
    );

    wait_for_paused_interval(&scratch, Duration::from_secs(5));
}

#[test]
fn pause_cli_minutes_reaches_running_daemon_and_expires_the_pause() {
    let scratch = Scratch::new("minutes-expiry");
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(xvfb.display());
    wm.declare_ewmh_supported();

    let _daemon = spawn_daemon(&scratch, &xvfb);
    wait_for_file(&scratch.lock_path(), Duration::from_secs(5));
    activate_window_until_observed(&scratch, &wm, Duration::from_secs(10));

    let output = run_pause_for_minutes(&scratch, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pause --minutes should succeed against a running daemon; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert_eq!(stdout, "xwindowlog: paused\n");
    assert!(
        stderr.is_empty(),
        "successful pause --minutes should emit no stderr: {stderr:?}"
    );

    wait_for_resumed_interval(&scratch, Duration::from_secs(5));
}

#[test]
fn resume_cli_reaches_running_daemon_and_closes_a_paused_interval() {
    let scratch = Scratch::new("resume-dispatch");
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(xvfb.display());
    wm.declare_ewmh_supported();

    let _daemon = spawn_daemon(&scratch, &xvfb);
    wait_for_file(&scratch.lock_path(), Duration::from_secs(5));
    activate_window_until_observed(&scratch, &wm, Duration::from_secs(10));

    let pause_output = run_pause(&scratch);
    assert!(
        pause_output.status.success(),
        "pause setup should succeed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&pause_output.stdout),
        String::from_utf8_lossy(&pause_output.stderr)
    );
    wait_for_paused_interval(&scratch, Duration::from_secs(5));

    let output = run_resume(&scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "resume should succeed against a paused running daemon; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert_eq!(stdout, "xwindowlog: active\n");
    assert!(
        stderr.is_empty(),
        "successful resume should keep diagnostics off stdout and emit no stderr: {stderr:?}"
    );

    wait_for_resumed_interval(&scratch, Duration::from_secs(5));
}

#[test]
fn pause_cli_while_already_paused_reports_state_error() {
    let scratch = Scratch::new("pause-already-paused");
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(xvfb.display());
    wm.declare_ewmh_supported();

    let _daemon = spawn_daemon(&scratch, &xvfb);
    wait_for_file(&scratch.lock_path(), Duration::from_secs(5));
    activate_window_until_observed(&scratch, &wm, Duration::from_secs(10));

    let first_pause = run_pause(&scratch);
    assert!(
        first_pause.status.success(),
        "pause setup should succeed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&first_pause.stdout),
        String::from_utf8_lossy(&first_pause.stderr)
    );
    wait_for_paused_interval(&scratch, Duration::from_secs(5));

    let output = run_pause(&scratch);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "a state error must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "xwindowlog: daemon is already paused\n"
    );
}

#[test]
fn resume_cli_while_active_reports_state_error() {
    let scratch = Scratch::new("resume-active");
    let xvfb = spawn_xvfb();
    let wm = FakeWm::connect(xvfb.display());
    wm.declare_ewmh_supported();

    let _daemon = spawn_daemon(&scratch, &xvfb);
    wait_for_file(&scratch.lock_path(), Duration::from_secs(5));
    activate_window_until_observed(&scratch, &wm, Duration::from_secs(10));

    let output = run_resume(&scratch);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "a state error must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "xwindowlog: daemon is not paused\n"
    );
}

#[test]
fn pause_cli_without_daemon_reports_environment_error() {
    let scratch = Scratch::new("pause-no-daemon");
    let start = Instant::now();

    let output = run_pause(&scratch);

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "pause without a daemon must fail promptly"
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        output.stdout.is_empty(),
        "a daemon connection diagnostic must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "xwindowlog: daemon is not running: No such file or directory (os error 2)\n"
    );
}

#[test]
fn resume_cli_without_daemon_reports_environment_error() {
    let scratch = Scratch::new("resume-no-daemon");
    let start = Instant::now();

    let output = run_resume(&scratch);

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "resume without a daemon must fail promptly"
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        output.stdout.is_empty(),
        "a daemon connection diagnostic must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "xwindowlog: daemon is not running: No such file or directory (os error 2)\n"
    );
}
