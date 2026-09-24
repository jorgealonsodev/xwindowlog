//! Real-binary coverage for the Phase 1 `status` and `today` reports.
//!
//! These tests deliberately provide a persisted fixture and remove the X11
//! environment. A report that reaches X11 or a live daemon therefore fails
//! instead of accidentally passing through a production desktop session.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use xwindowlog::clock::WallTs;
use xwindowlog::store::{IntervalState, IntervalStore, NewInterval, Store};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "xwindowlog-cli-status-today-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create CLI test scratch directory");
        Scratch { root }
    }

    fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
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

fn seed_hidden_interval(scratch: &Scratch) {
    let data_dir = scratch.data_home().join("xwindowlog");
    std::fs::create_dir_all(&data_dir).expect("create data directory");

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let start = now.checked_sub(120).expect("test timestamp subtraction");
    let end = now.checked_sub(60).expect("test timestamp subtraction");
    let mut store = Store::open(&scratch.database_path()).expect("open persisted fixture");
    store
        .open_only(
            WallTs::new(start),
            NewInterval {
                app: "keepassxc".to_string(),
                title: "[hidden]".to_string(),
                pid: None,
                state: IntervalState::Active,
            },
        )
        .expect("insert hidden interval");
    store
        .close_only(WallTs::new(end))
        .expect("close hidden interval");
}

fn seed_local_midnight_crossing_interval(scratch: &Scratch) {
    let data_dir = scratch.data_home().join("xwindowlog");
    std::fs::create_dir_all(&data_dir).expect("create data directory");

    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let now = time::OffsetDateTime::now_utc();
    let local_date = now.to_offset(offset).date();
    let midnight = time::OffsetDateTime::new_in_offset(local_date, time::Time::MIDNIGHT, offset)
        .unix_timestamp();
    let start = midnight
        .checked_sub(60)
        .expect("test timestamp subtraction");
    let end = midnight
        .checked_add(3_600)
        .expect("test timestamp addition");
    let mut store = Store::open(&scratch.database_path()).expect("open persisted fixture");
    store
        .open_only(
            WallTs::new(start),
            NewInterval {
                app: "editor-local-day".to_string(),
                title: "Local day boundary".to_string(),
                pid: None,
                state: IntervalState::Active,
            },
        )
        .expect("insert midnight-crossing interval");
    store
        .close_only(WallTs::new(end))
        .expect("close midnight-crossing interval");
}

fn seed_open_interval(scratch: &Scratch) {
    seed_open_interval_with_state(scratch, IntervalState::Active);
}

fn seed_open_interval_with_state(scratch: &Scratch, state: IntervalState) {
    let data_dir = scratch.data_home().join("xwindowlog");
    std::fs::create_dir_all(&data_dir).expect("create data directory");

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let start = now.checked_sub(90).expect("test timestamp subtraction");
    let mut store = Store::open(&scratch.database_path()).expect("open persisted fixture");
    store
        .open_only(
            WallTs::new(start),
            NewInterval {
                app: "editor".to_string(),
                title: "Sensitive window title".to_string(),
                pid: Some(4242),
                state,
            },
        )
        .expect("insert open interval");
}

fn write_config(scratch: &Scratch, contents: &str) {
    let config_dir = scratch.config_home().join("xwindowlog");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::write(config_dir.join("config.toml"), contents).expect("write test config");
}

fn run_report(scratch: &Scratch, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg(command)
        .env("XDG_DATA_HOME", scratch.data_home())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_RUNTIME_DIR", scratch.runtime_dir())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("run xwindowlog report")
}

#[test]
fn today_reads_persisted_hidden_data_without_x11_or_a_live_daemon() {
    let scratch = Scratch::new("persisted-only");
    seed_hidden_interval(&scratch);

    let output = run_report(&scratch, "today");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "today should succeed from the fixture; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stdout.contains("keepassxc") && stdout.contains("[hidden]"),
        "today must reflect the persisted sanitized interval, got {stdout:?}"
    );
    assert!(
        !stderr.contains("not_yet_implemented"),
        "today must not use the placeholder path, got {stderr:?}"
    );
}

#[test]
fn today_uses_the_local_calendar_day_boundary_for_utc_persisted_rows() {
    let scratch = Scratch::new("local-day");
    seed_local_midnight_crossing_interval(&scratch);

    let output = run_report(&scratch, "today");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "today should resolve local-day bounds; stdout={stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("00:00") && stdout.contains("editor-local-day"),
        "today must clip UTC-persisted data at local midnight, got {stdout:?}"
    );
}

#[test]
fn status_hides_title_by_default_and_shows_it_when_configured() {
    let default_scratch = Scratch::new("status-title-default");
    seed_open_interval(&default_scratch);

    let hidden = run_report(&default_scratch, "status");
    let hidden_stdout = String::from_utf8_lossy(&hidden.stdout);
    assert!(
        hidden.status.success(),
        "default status should succeed; stdout={hidden_stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&hidden.stderr)
    );
    assert!(
        hidden_stdout.contains("editor"),
        "default status should show the persisted app id, got {hidden_stdout:?}"
    );
    assert!(
        !hidden_stdout.contains("Sensitive window title"),
        "default status must hide the persisted title, got {hidden_stdout:?}"
    );

    let configured_scratch = Scratch::new("status-title-visible");
    seed_open_interval(&configured_scratch);
    write_config(&configured_scratch, "status_show_title = true\n");

    let visible = run_report(&configured_scratch, "status");
    let visible_stdout = String::from_utf8_lossy(&visible.stdout);
    assert!(
        visible.status.success(),
        "configured status should succeed; stdout={visible_stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&visible.stderr)
    );
    assert!(
        visible_stdout.contains("Sensitive window title"),
        "configured status should show the persisted title, got {visible_stdout:?}"
    );
}

#[test]
fn status_is_one_line_and_omits_unavailable_project_attribution() {
    let scratch = Scratch::new("status-shape");
    seed_open_interval(&scratch);

    let output = run_report(&scratch, "status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status should succeed; stdout={stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.lines().count(),
        1,
        "status-bar output must be exactly one line, got {stdout:?}"
    );
    assert!(
        stdout.starts_with("xwindowlog: editor · ") && stdout.ends_with(" today\n"),
        "status must use the documented app/time/today shape, got {stdout:?}"
    );
    assert!(
        !stdout.contains("project-x"),
        "Phase 1 status must not invent project attribution, got {stdout:?}"
    );
}

#[test]
fn status_visibly_reports_a_persisted_paused_state() {
    let scratch = Scratch::new("status-paused");
    seed_open_interval_with_state(&scratch, IntervalState::Paused);

    let output = run_report(&scratch, "status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "paused status should succeed; stdout={stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.to_ascii_lowercase().contains("paused"),
        "paused state must be visible in status, got {stdout:?}"
    );
    assert!(
        !stdout.contains("editor"),
        "non-active status must not be rendered as an active app, got {stdout:?}"
    );
}

#[test]
fn missing_database_is_reported_as_a_state_error() {
    let scratch = Scratch::new("missing-database");

    let output = run_report(&scratch, "today");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "missing persisted state must use RF-60 state exit code, stderr={stderr:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "state errors must not write primary output, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("database does not exist"),
        "missing database diagnostic belongs on stderr, got {stderr:?}"
    );
}

#[test]
fn both_reports_use_one_shared_clipping_query_path() {
    let main_source = include_str!("../src/main.rs");
    assert_eq!(
        main_source.matches(".clipped_intervals(").count(),
        1,
        "the clipping CTE must have one report-layer call site"
    );
    assert_eq!(
        main_source.matches("query_today_intervals(&store").count(),
        2,
        "today and status must both consume the shared clipping helper"
    );
}
