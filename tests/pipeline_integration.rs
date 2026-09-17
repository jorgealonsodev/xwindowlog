//! Task 8.2 (wiring GREEN) / 8.5-8.6 (§14.3 ordering guarantee, made
//! executable rather than a review-checklist item only): exercises the real
//! `x11-shaped synthetic events -> exclude.rs -> tracker.rs -> store.rs`
//! pipeline end to end (design §3's data-flow diagram, PRD §14.3), through
//! a real `Store::open()` on a temp-file SQLite database — the same
//! production code path `main.rs` uses from Phase 15 on, not an in-memory
//! shortcut. `store.rs`'s own `IntervalStore` correctness (D-1's atomic
//! transaction, RF-36 recovery, and so on) was already proven in Phase 3/4;
//! this file proves the WIRING between the three modules, which no earlier
//! phase could touch (the crate was binary-only — see task 8.0).
//!
//! Still synthetic `WindowSource`-shaped events, per the Review Workload
//! Forecast's own note for this phase ("N/A — still synthetic WindowSource,
//! no real X11 yet"): a real `x11.rs`/`ReactorSource` implementation is
//! Phase 9-14's job.

use std::path::{Path, PathBuf};
use std::time::Duration;

use xwindowlog::clock::{Clock, FakeClock, WallTs};
use xwindowlog::exclude::{Excluder, RawTitle};
use xwindowlog::store::{IntervalStore, Store};
use xwindowlog::tracker::{Effect, SourceEvent, Timer, Tracker, WindowInfo};

/// A uniquely-named scratch database file under the OS temp dir, removed
/// (including its WAL/SHM/migration-backup siblings) on drop. Hand-rolled
/// instead of depending on an external tempfile crate, matching
/// `store.rs`'s own `ScratchDir` / `exclude.rs`'s own `temp_config_home`
/// test-only precedent (design §6 testing-strategy note; rust-systems
/// skill: avoid adding a dependency outside the committed list for a
/// test-only need).
struct ScratchDb(PathBuf);

impl ScratchDb {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "xwindowlog-pipeline-test-{label}-{}-{n}.db",
            std::process::id()
        ));
        ScratchDb(path)
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", ".migration-backup"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

/// Applies every effect the tracker emits to `store`, exactly as Phase 15's
/// composed daemon will (design §1's reactor `Effect` application).
/// `ArmTimer`/`CancelTimer` are the reactor's own bookkeeping (Phase 14, not
/// this phase's scope); `Diagnostic` is collected rather than discarded —
/// task 8.5/8.6 need every diagnostic string to check for a raw-title leak.
fn apply_effects(store: &mut Store, effects: Vec<Effect>, diagnostics: &mut Vec<String>) {
    for effect in effects {
        match effect {
            Effect::Transition { at, open } => store.transition(at, open).expect("transition"),
            Effect::CloseOnly { at } => store.close_only(at).expect("close_only"),
            Effect::OpenOnly { at, open } => store.open_only(at, open).expect("open_only"),
            Effect::Diagnostic(msg) => diagnostics.push(msg),
            Effect::ArmTimer(..) | Effect::CancelTimer(_) => {}
        }
    }
}

fn send(
    tracker: &mut Tracker,
    event: SourceEvent,
    clock: &FakeClock,
    store: &mut Store,
    diagnostics: &mut Vec<String>,
) {
    let effects = tracker.on_event(event, clock.now_wall(), clock.now_mono());
    apply_effects(store, effects, diagnostics);
}

/// One persisted `intervals` row, resolved to its real `apps`/`titles`
/// dictionary text — `store.rs`'s own `ClippedInterval` deliberately leaves
/// these as dictionary ids (Phase 16's `today`/`status` concern), so this
/// test reopens the database file with an independent read-only-in-spirit
/// connection to see exactly what a `SELECT` against the real schema
/// returns. WAL mode (RF-11) allows this concurrently with `store` still
/// holding the file open — a reader never blocks on a WAL writer.
struct StoredInterval {
    app: String,
    title: String,
    state: String,
    start: i64,
    end: Option<i64>,
}

fn read_all_intervals(db_path: &Path) -> Vec<StoredInterval> {
    let conn = rusqlite::Connection::open(db_path).expect("reopen db for verification");
    let mut stmt = conn
        .prepare(
            "SELECT apps.app_id, titles.title, intervals.state, intervals.start, intervals.\"end\" \
             FROM intervals \
             JOIN apps ON apps.id = intervals.app \
             JOIN titles ON titles.id = intervals.title \
             ORDER BY intervals.start, intervals.id",
        )
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok(StoredInterval {
            app: r.get(0)?,
            title: r.get(1)?,
            state: r.get(2)?,
            start: r.get(3)?,
            end: r.get(4)?,
        })
    })
    .expect("query")
    .collect::<rusqlite::Result<_>>()
    .expect("collect")
}

// ---- Task 8.2 (GREEN): the wiring itself, proven against a real Store ----

/// `x11-shaped synthetic events -> exclude.rs -> tracker.rs -> store.rs`,
/// proven end to end: a non-excluded window's real content lands in the
/// store verbatim, an excluded window's content is hidden but its duration
/// is not (RF-8, P3), and the two intervals are contiguous (RF-3) — all
/// through the exact sequence `main.rs` will run from Phase 15 on.
#[test]
fn pipeline_wiring_persists_sanitized_content_in_order() {
    let scratch = ScratchDb::new("wiring");
    let mut store = Store::open(&scratch.0).expect("open store");
    let excluder = Excluder::from_toml_str(
        r#"
        [[exclude]]
        app = "keepassxc"
        "#,
    )
    .expect("valid config compiles");
    let clock = FakeClock::new(WallTs::new(1_700_000_000));
    let mut tracker = Tracker::new();
    let mut diagnostics = Vec::new();

    // step 1: a non-excluded window — real content persisted verbatim.
    let opened = excluder.evaluate("firefox", RawTitle::new("GitHub - foo/bar"));
    send(
        &mut tracker,
        SourceEvent::ActiveWindow(Some(WindowInfo {
            app_id: opened.app_id,
            title: opened.title,
            pid: Some(4821),
        })),
        &clock,
        &mut store,
        &mut diagnostics,
    );

    // step 2: an excluded window — content hidden, time still recorded.
    clock.advance(Duration::from_secs(90));
    let excluded = excluder.evaluate("keepassxc", RawTitle::new("KeePassXC - vault.kdbx"));
    send(
        &mut tracker,
        SourceEvent::ActiveWindow(Some(WindowInfo {
            app_id: excluded.app_id,
            title: excluded.title,
            pid: None,
        })),
        &clock,
        &mut store,
        &mut diagnostics,
    );

    // step 3: shutdown closes the last open interval.
    clock.advance(Duration::from_secs(30));
    send(
        &mut tracker,
        SourceEvent::Shutdown,
        &clock,
        &mut store,
        &mut diagnostics,
    );

    drop(store);
    let intervals = read_all_intervals(&scratch.0);

    assert_eq!(intervals.len(), 2, "one interval per ActiveWindow event");

    assert_eq!(intervals[0].app, "firefox");
    assert_eq!(intervals[0].title, "GitHub - foo/bar");
    assert_eq!(intervals[0].state, "active");
    assert_eq!(intervals[0].start, 1_700_000_000);
    assert_eq!(
        intervals[0].end,
        Some(1_700_000_090),
        "RF-3: end of the first interval == start of the second"
    );

    assert_eq!(intervals[1].app, "keepassxc", "RF-8: app_id stays visible");
    assert_eq!(
        intervals[1].title, "[hidden]",
        "RF-8: excluded window's title is hidden"
    );
    assert_eq!(intervals[1].start, 1_700_000_090);
    assert_eq!(intervals[1].end, Some(1_700_000_120));
    assert_eq!(
        intervals[1].end.unwrap() - intervals[1].start,
        30,
        "P3: exclusion hides content, never drops duration"
    );
}

// ---- Task 8.5 (RED) / 8.6 (GREEN): no raw title ever reaches a log call,
// panic message, or persisted value ----

/// §14.3's ordering guarantee, made executable: runs a scripted sequence
/// through the real pipeline — an excluded window (raw title carries a
/// unique marker that must never surface anywhere), a non-excluded window
/// whose title contains a redactable secret (RF-51), and a debounced TITLE
/// CHANGE (not just a window change) on that same non-excluded window,
/// also carrying a secret — then captures every `Diagnostic` string, any
/// panic message, and every string actually persisted to the store, and
/// asserts none of them contain the raw marker or the raw secret fragment.
///
/// The title-change path matters on its own: `on_title_changed`'s equality
/// check only sees whatever `SourceEvent::TitleChanged` was constructed
/// with, so if a future wiring mistake fed it a raw, unsanitized title
/// instead of routing it through `Excluder::evaluate` first, this is the
/// scenario that would catch it — the general case task 8.3's unit test
/// (constant, already-sanitized inputs) does not exercise, because that
/// test does not touch a harness that itself has to remember to sanitize
/// every event variant, only the tracker's own comparison.
#[test]
fn no_raw_title_content_reaches_diagnostics_panics_or_the_store() {
    const EXCLUDED_MARKER: &str = "TOTALLY-SECRET-EXCLUDED-MARKER-9f3c1";
    const SECRET_EMAIL: &str = "jane.doe@example.com";

    let scratch = ScratchDb::new("no-leak");
    let mut store = Store::open(&scratch.0).expect("open store");
    let excluder = Excluder::from_toml_str(
        r#"
        [[exclude]]
        app = "keepassxc"
        "#,
    )
    .expect("valid config compiles");
    let clock = FakeClock::new(WallTs::new(1_800_000_000));
    let mut tracker = Tracker::new();
    let mut diagnostics = Vec::new();

    // step 1: excluded window carrying the marker in its raw title.
    let excluded = excluder.evaluate(
        "keepassxc",
        RawTitle::new(format!("KeePassXC - {EXCLUDED_MARKER}")),
    );
    send(
        &mut tracker,
        SourceEvent::ActiveWindow(Some(WindowInfo {
            app_id: excluded.app_id,
            title: excluded.title,
            pid: None,
        })),
        &clock,
        &mut store,
        &mut diagnostics,
    );

    // step 2: a non-excluded window whose title carries a redactable secret.
    clock.advance(Duration::from_secs(10));
    let opened = excluder.evaluate(
        "firefox",
        RawTitle::new(format!("Invite sent to {SECRET_EMAIL}")),
    );
    send(
        &mut tracker,
        SourceEvent::ActiveWindow(Some(WindowInfo {
            app_id: opened.app_id,
            title: opened.title,
            pid: Some(1),
        })),
        &clock,
        &mut store,
        &mut diagnostics,
    );

    // step 3: a debounced TITLE CHANGE on that same window, also carrying a
    // secret — exercises the `TitleChanged` path specifically, not just
    // `ActiveWindow`.
    clock.advance(Duration::from_secs(5));
    let changed = excluder.evaluate(
        "firefox",
        RawTitle::new(format!("Reply from {SECRET_EMAIL} received")),
    );
    send(
        &mut tracker,
        SourceEvent::TitleChanged(changed.title),
        &clock,
        &mut store,
        &mut diagnostics,
    );
    clock.advance(Duration::from_millis(2000));
    send(
        &mut tracker,
        SourceEvent::DeadlineElapsed(Timer::TitleDebounce),
        &clock,
        &mut store,
        &mut diagnostics,
    );

    // step 4: shutdown.
    clock.advance(Duration::from_secs(1));
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send(
            &mut tracker,
            SourceEvent::Shutdown,
            &clock,
            &mut store,
            &mut diagnostics,
        );
    }));
    let panic_message = panic_result.err().map(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string())
    });
    assert!(
        panic_message.is_none(),
        "unexpected panic during a scripted run with no fault injected"
    );

    drop(store);
    let intervals = read_all_intervals(&scratch.0);

    // Sanitization actually happened, not just "coincidentally absent below".
    assert_eq!(intervals[0].title, "[hidden]", "excluded window is hidden");
    assert_eq!(
        intervals[1].title, "Invite sent to [REDACTED]",
        "RF-51 redacts the email fragment, keeps the rest verbatim"
    );
    assert_eq!(
        intervals[2].title, "Reply from [REDACTED] received",
        "RF-51 applies on the debounced TitleChanged path too"
    );

    let mut haystacks: Vec<String> = diagnostics.clone();
    if let Some(msg) = &panic_message {
        haystacks.push(msg.clone());
    }
    for interval in &intervals {
        haystacks.push(interval.app.clone());
        haystacks.push(interval.title.clone());
    }

    for haystack in &haystacks {
        assert!(
            !haystack.contains(EXCLUDED_MARKER),
            "raw excluded-window marker leaked into: {haystack:?}"
        );
        assert!(
            !haystack.contains(SECRET_EMAIL),
            "raw secret email leaked into: {haystack:?}"
        );
    }
}
