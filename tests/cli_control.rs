//! Real-binary coverage for the Phase 17 `pause`/`resume` CLI control paths.
//!
//! The test runs a compiled daemon against an isolated Xvfb and asks the
//! compiled CLI to pause it. SQLite is queried read-only afterwards so the
//! observed state must have been written by the daemon, not by the CLI.

use std::io::Write;
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
use xwindowlog::clock::{Clock, SystemClock, WallTs};
use xwindowlog::store::{IntervalState, IntervalStore, NewInterval, Store};

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

    fn write_config(&self, contents: &str) {
        let config_dir = self.config_home().join("xwindowlog");
        std::fs::create_dir_all(&config_dir).expect("create xwindowlog config directory");
        std::fs::write(config_dir.join("config.toml"), contents).expect("write xwindowlog config");
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

fn run_prune(scratch: &Scratch, older_than: Option<&str>, vacuum_only: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xwindowlog"));
    command
        .arg("prune")
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    if let Some(older_than) = older_than {
        command.args(["--older-than", older_than]);
    }
    if vacuum_only {
        command.arg("--vacuum-only");
    }
    command
        .output()
        .expect("run the compiled xwindowlog prune command")
}

fn run_forget(scratch: &Scratch, args: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xwindowlog"));
    command
        .arg("forget")
        .args(args)
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_DATA_HOME", scratch.data_home())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command
        .spawn()
        .expect("run the compiled xwindowlog forget command");
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("open forget command stdin");
        stdin
            .write_all(input.as_bytes())
            .expect("write forget command confirmation");
    }
    child
        .wait_with_output()
        .expect("wait for the compiled xwindowlog forget command")
}

fn interval(app: &str, title: &str) -> NewInterval {
    NewInterval {
        app: app.to_string(),
        title: title.to_string(),
        pid: None,
        state: IntervalState::Active,
    }
}

fn seed_prune_database(scratch: &Scratch, old_age_days: i64) {
    const SECONDS_PER_DAY: i64 = 86_400;

    let now = SystemClock.now_wall().as_unix_secs();
    let old_age = old_age_days
        .checked_mul(SECONDS_PER_DAY)
        .expect("test fixture age must fit in seconds");
    let old_start = now
        .checked_sub(old_age)
        .expect("test fixture timestamp must fit in i64");
    let old_end = old_start
        .checked_add(60 * 60)
        .expect("test fixture timestamp must fit in i64");
    let recent_start = now
        .checked_sub(SECONDS_PER_DAY)
        .expect("test fixture timestamp must fit in i64");
    let recent_end = now
        .checked_sub(12 * 60 * 60)
        .expect("test fixture timestamp must fit in i64");

    let mut store = Store::open(&scratch.database_path()).expect("open prune fixture database");
    store
        .open_only(WallTs::new(old_start), interval("old-app", "old title"))
        .expect("open old fixture interval");
    store
        .close_only(WallTs::new(old_end))
        .expect("close old fixture interval");
    store
        .open_only(
            WallTs::new(recent_start),
            interval("recent-app", "recent title"),
        )
        .expect("open recent fixture interval");
    store
        .transition(WallTs::new(recent_end), interval("open-app", "open title"))
        .expect("open live fixture interval");
}

fn seed_forget_database(scratch: &Scratch) {
    let mut store = Store::open(&scratch.database_path()).expect("open forget fixture database");
    store
        .open_only(
            WallTs::new(1_000),
            interval("forget-target", "forget target"),
        )
        .expect("open forget target interval");
    store
        .close_only(WallTs::new(1_100))
        .expect("close forget target interval");
    store
        .open_only(
            WallTs::new(2_000),
            interval("forget-survivor", "forget survivor"),
        )
        .expect("open forget survivor interval");
    store
        .close_only(WallTs::new(2_100))
        .expect("close forget survivor interval");
}

fn seed_forget_precision_intervals(scratch: &Scratch, intervals: &[(&str, i64, i64)]) {
    let mut store =
        Store::open(&scratch.database_path()).expect("open forget precision fixture database");
    for &(app, start, end) in intervals {
        store
            .open_only(WallTs::new(start), interval(app, app))
            .expect("open forget precision fixture interval");
        store
            .close_only(WallTs::new(end))
            .expect("close forget precision fixture interval");
    }
}

fn interval_id_for_app(db_path: &Path, app: &str) -> i64 {
    let connection = rusqlite::Connection::open(db_path).expect("open forget fixture for reading");
    connection
        .query_row(
            "SELECT intervals.id \
             FROM intervals \
             JOIN apps ON apps.id = intervals.app \
             WHERE apps.app_id = ?1 \
             ORDER BY intervals.id \
             LIMIT 1",
            [app],
            |row| row.get(0),
        )
        .expect("find forget fixture interval id")
}

fn interval_count_for_app(db_path: &Path, app: &str) -> i64 {
    let connection = rusqlite::Connection::open(db_path).expect("open prune fixture for reading");
    connection
        .query_row(
            "SELECT COUNT(*) FROM intervals JOIN apps ON apps.id = intervals.app WHERE apps.app_id = ?1",
            [app],
            |row| row.get(0),
        )
        .expect("count fixture intervals")
}

fn open_interval_count(db_path: &Path) -> i64 {
    let connection = rusqlite::Connection::open(db_path).expect("open prune fixture for reading");
    connection
        .query_row(
            "SELECT COUNT(*) FROM intervals WHERE \"end\" IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count open fixture intervals")
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

#[test]
fn prune_cli_explicit_retention_deletes_old_closed_intervals_and_preserves_open() {
    let scratch = Scratch::new("prune-explicit");
    seed_prune_database(&scratch, 200);

    let output = run_prune(&scratch, Some("180d"), false);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(
        output.stderr.is_empty(),
        "successful prune should not emit diagnostics: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "old-app"),
        0
    );
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "recent-app"),
        1
    );
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "open-app"),
        1
    );
    assert_eq!(open_interval_count(&scratch.database_path()), 1);
}

#[test]
fn prune_cli_uses_configured_retention_when_flag_is_omitted() {
    let scratch = Scratch::new("prune-configured");
    scratch.write_config("retention_days = 180\n");
    seed_prune_database(&scratch, 200);

    let output = run_prune(&scratch, None, false);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "old-app"),
        0
    );
    assert_eq!(open_interval_count(&scratch.database_path()), 1);
}

#[test]
fn prune_cli_uses_default_retention_when_config_and_flag_are_omitted() {
    let scratch = Scratch::new("prune-default");
    seed_prune_database(&scratch, 400);

    let output = run_prune(&scratch, None, false);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "old-app"),
        0
    );
    assert_eq!(open_interval_count(&scratch.database_path()), 1);
}

#[test]
fn prune_cli_configured_zero_retention_disables_deletion() {
    let scratch = Scratch::new("prune-disabled");
    scratch.write_config("retention_days = 0\n");
    seed_prune_database(&scratch, 400);

    let output = run_prune(&scratch, None, false);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: retention pruning is disabled (retention_days = 0)\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "old-app"),
        1
    );
    assert_eq!(open_interval_count(&scratch.database_path()), 1);
}

#[test]
fn prune_cli_vacuum_only_does_not_delete_intervals_and_accepts_zero_duration() {
    let scratch = Scratch::new("prune-vacuum-only");
    seed_prune_database(&scratch, 400);

    let output = run_prune(&scratch, Some("0d"), true);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "old-app"),
        1
    );
    assert_eq!(open_interval_count(&scratch.database_path()), 1);
}

#[test]
fn prune_cli_rejects_invalid_duration_before_mutating_the_database() {
    for (label, duration, diagnostic) in [
        (
            "zero",
            "0d",
            "xwindowlog: invalid retention duration '0d': expected a positive number of days such as 180d\n",
        ),
        (
            "missing-unit",
            "180",
            "xwindowlog: invalid retention duration '180': expected a positive number of days such as 180d\n",
        ),
        (
            "overflow",
            "18446744073709551615d",
            "xwindowlog: invalid retention duration '18446744073709551615d': duration is too large\n",
        ),
    ] {
        let scratch = Scratch::new(&format!("prune-invalid-{label}"));
        seed_prune_database(&scratch, 400);

        let output = run_prune(&scratch, Some(duration), false);

        assert_eq!(output.status.code(), Some(1), "duration={duration:?}");
        assert!(
            output.stdout.is_empty(),
            "invalid duration must not write primary output: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), diagnostic);
        assert_eq!(interval_count_for_app(&scratch.database_path(), "old-app"), 1);
        assert_eq!(open_interval_count(&scratch.database_path()), 1);
    }
}

#[test]
fn prune_cli_missing_database_reports_state_error_on_stderr() {
    let scratch = Scratch::new("prune-no-database");

    let output = run_prune(&scratch, Some("180d"), false);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("xwindowlog: database does not exist at"));
    assert!(stderr.contains("start the daemon before requesting a report\n"));
}

#[test]
fn forget_cli_range_with_yes_deletes_matching_intervals_and_preserves_others() {
    let scratch = Scratch::new("forget-range");
    seed_forget_database(&scratch);

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1970-01-01T00:16:40Z",
            "--to",
            "1970-01-01T00:18:20Z",
            "--yes",
        ],
        None,
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        0
    );
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-survivor"),
        1
    );
}

#[test]
fn forget_cli_window_with_yes_deletes_only_the_selected_interval() {
    let scratch = Scratch::new("forget-window");
    seed_forget_database(&scratch);
    let target_id = interval_id_for_app(&scratch.database_path(), "forget-target");

    let output = run_forget(
        &scratch,
        &["--window", &target_id.to_string(), "--yes"],
        None,
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        0
    );
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-survivor"),
        1
    );
}

#[test]
fn forget_cli_rejects_missing_incomplete_and_mixed_selectors_before_opening_database() {
    for (label, args) in [
        ("missing", vec![]),
        ("from-only", vec!["--from", "1970-01-01T00:16:40Z"]),
        ("to-only", vec!["--to", "1970-01-01T00:18:20Z"]),
        (
            "mixed",
            vec![
                "--from",
                "1970-01-01T00:16:40Z",
                "--to",
                "1970-01-01T00:18:20Z",
                "--window",
                "1",
            ],
        ),
    ] {
        let scratch = Scratch::new(&format!("forget-selector-{label}"));
        let output = run_forget(&scratch, &args, None);
        assert_eq!(output.status.code(), Some(1), "selector case={label}");
        assert!(
            output.stdout.is_empty(),
            "selector diagnostics must not write to stdout: case={label}, stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exactly one selector"),
            "selector case={label} should report exclusivity: stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn forget_cli_rejects_malformed_equal_and_reversed_ranges_before_opening_database() {
    for (label, args, expected) in [
        (
            "malformed",
            vec!["--from", "not-a-timestamp", "--to", "1970-01-01T00:18:20Z"],
            "invalid --from timestamp",
        ),
        (
            "equal",
            vec![
                "--from",
                "1970-01-01T00:18:20Z",
                "--to",
                "1970-01-01T00:18:20Z",
            ],
            "must be before --to",
        ),
        (
            "reversed",
            vec![
                "--from",
                "1970-01-01T00:18:20Z",
                "--to",
                "1970-01-01T00:16:40Z",
            ],
            "must be before --to",
        ),
    ] {
        let scratch = Scratch::new(&format!("forget-range-{label}"));
        let output = run_forget(&scratch, &args, None);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "range case={label}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.contains(expected),
            "range case={label} should report {expected:?}: stderr={stderr:?}"
        );
    }
}

#[test]
fn forget_cli_fractional_range_deletes_only_overlapping_integer_intervals() {
    let scratch = Scratch::new("forget-fractional-range");
    seed_forget_precision_intervals(
        &scratch,
        &[
            ("forget-before", 900, 1_000),
            ("forget-lower-overlap", 1_000, 1_001),
            ("forget-upper-overlap", 1_100, 1_101),
            ("forget-after", 1_101, 1_102),
        ],
    );

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1970-01-01T00:16:40.500Z",
            "--to",
            "1970-01-01T00:18:20.500Z",
            "--yes",
        ],
        None,
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 2 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    let db_path = scratch.database_path();
    assert_eq!(interval_count_for_app(&db_path, "forget-before"), 1);
    assert_eq!(interval_count_for_app(&db_path, "forget-lower-overlap"), 0);
    assert_eq!(interval_count_for_app(&db_path, "forget-upper-overlap"), 0);
    assert_eq!(interval_count_for_app(&db_path, "forget-after"), 1);
}

#[test]
fn forget_cli_pre_epoch_fractional_range_uses_floor_and_ceiling_bounds() {
    let scratch = Scratch::new("forget-pre-epoch-fractional-range");
    seed_forget_precision_intervals(
        &scratch,
        &[
            ("forget-pre-before", -2, -1),
            ("forget-pre-lower-overlap", -1, 0),
            ("forget-pre-upper-overlap", 0, 1),
            ("forget-pre-after", 1, 2),
        ],
    );

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1969-12-31T23:59:59.500Z",
            "--to",
            "1970-01-01T00:00:00.500Z",
            "--yes",
        ],
        None,
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 2 intervals; database vacuumed\n"
    );
    assert!(output.stderr.is_empty());
    let db_path = scratch.database_path();
    assert_eq!(interval_count_for_app(&db_path, "forget-pre-before"), 1);
    assert_eq!(
        interval_count_for_app(&db_path, "forget-pre-lower-overlap"),
        0
    );
    assert_eq!(
        interval_count_for_app(&db_path, "forget-pre-upper-overlap"),
        0
    );
    assert_eq!(interval_count_for_app(&db_path, "forget-pre-after"), 1);
}

#[test]
fn forget_cli_range_requires_confirmation_and_negative_input_does_not_mutate() {
    let scratch = Scratch::new("forget-negative");
    seed_forget_database(&scratch);

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1970-01-01T00:16:40Z",
            "--to",
            "1970-01-01T00:18:20Z",
        ],
        Some("no\n"),
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("confirm permanent deletion"));
    assert!(stderr.contains("cancelled"));
    assert!(!stderr.contains("forget target"));
    assert!(!stderr.contains("forget survivor"));
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        1
    );
}

#[test]
fn forget_cli_window_requires_confirmation_and_negative_input_does_not_mutate() {
    let scratch = Scratch::new("forget-window-negative");
    seed_forget_database(&scratch);
    let target_id = interval_id_for_app(&scratch.database_path(), "forget-target");

    let output = run_forget(&scratch, &["--window", &target_id.to_string()], Some("n\n"));

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("confirm permanent deletion"));
    assert!(stderr.contains("cancelled"));
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        1
    );
}

#[test]
fn forget_cli_eof_cancels_without_mutating_the_database() {
    let scratch = Scratch::new("forget-eof");
    seed_forget_database(&scratch);

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1970-01-01T00:16:40Z",
            "--to",
            "1970-01-01T00:18:20Z",
        ],
        None,
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("confirm permanent deletion"));
    assert!(stderr.contains("cancelled"));
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        1
    );
}

#[test]
fn forget_cli_accepts_case_insensitive_affirmative_confirmation() {
    let scratch = Scratch::new("forget-confirm");
    seed_forget_database(&scratch);

    let output = run_forget(
        &scratch,
        &[
            "--from",
            "1970-01-01T00:16:40Z",
            "--to",
            "1970-01-01T00:18:20Z",
        ],
        Some("YeS\n"),
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "xwindowlog: deleted 1 intervals; database vacuumed\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirm"));
    assert_eq!(
        interval_count_for_app(&scratch.database_path(), "forget-target"),
        0
    );
}

#[test]
fn cli_usage_errors_exit_one() {
    let output = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("--not-a-real-option")
        .output()
        .expect("run the compiled xwindowlog binary with an unknown option");

    assert_eq!(
        output.status.code(),
        Some(1),
        "invalid CLI usage should use RF-60 generic failure; stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
}

#[test]
fn cli_help_and_version_exit_zero_on_stdout() {
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
            .arg(argument)
            .output()
            .expect("run the compiled xwindowlog binary");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert_eq!(
            output.status.code(),
            Some(0),
            "{argument} should exit successfully; stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("xwindowlog") && !stdout.is_empty(),
            "{argument} should write its output to stdout, got {stdout:?}"
        );
        assert!(output.stderr.is_empty());
    }
}
