//! clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10);
//! reactor construction and `Effect` application.
//!
//! Design §1: `main.rs` is composition only — clap, flock, config, reactor construction, effect
//! application (task 15.12). No state-machine or storage logic is duplicated here; it all stays
//! in `tracker.rs`/`store.rs`, reached only through their existing public API.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use nix::fcntl::{Flock, FlockArg};
use nix::unistd::Uid;
use serde::Serialize;

use time::{OffsetDateTime, Time, UtcOffset};
use xwindowlog::clock::{duration_secs, Clock, SystemClock, WallTs};
use xwindowlog::control;
use xwindowlog::exclude::Excluder;
use xwindowlog::logind::{
    self, LockedHintTracker, SessionMonitor as _, SessionResolution, SuspendInhibitor,
};
use xwindowlog::reactor::ReactorSource;
use xwindowlog::signals::SelfPipe;
use xwindowlog::store::{
    vacuum_exhausted_message, DeletionOutcome, DisplayInterval, IntervalStore as _, OpenInterval,
    Store, StoreError, VacuumOutcome, VACUUM_EXHAUSTED_EXIT_CODE,
};
use xwindowlog::tracker::{SourceEvent, Timer, WindowSource as _};
use xwindowlog::x11::{X11InitError, X11Source};

mod adapters;
mod config;

use adapters::{LogindAdapter, LogindSource, NullFd, X11Adapter};
use config::{ConfigError, DaemonConfig};

/// The concrete reactor this composition builds — `main.rs`'s own type, not `reactor.rs`'s
/// concern (design §1: composition picks the real adapters; the reactor stays generic).
type Reactor = ReactorSource<X11Adapter, LogindSource, SystemClock>;

/// RF-19 (partial): the subset of Annex B's full subcommand list this phase delivers.
/// `mcp`/`install` land in Phase 2, `export`/`doctor` in Phase 3 (proposal *Scope adjustments*).
#[derive(Debug, Parser)]
#[command(name = "xwindowlog", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Runs the resident capture daemon (RF-1..RF-13, RF-20..RF-36, RF-49).
    Daemon,
    /// Prints a single-line status summary (Phase 16).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Prints today's recorded intervals (Phase 16).
    Today {
        #[arg(long)]
        json: bool,
    },
    /// Pauses capture, optionally for a bounded number of minutes (Phase 17).
    Pause {
        #[arg(long)]
        minutes: Option<u32>,
    },
    /// Resumes capture after a `pause` (Phase 17).
    Resume,
    /// Deletes intervals older than a retention window and reclaims disk space (Phase 17).
    Prune {
        #[arg(long = "older-than")]
        older_than: Option<String>,
        #[arg(long)]
        vacuum_only: bool,
    },
    /// Deletes a specific range or window of intervals (Phase 17).
    Forget {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        window: Option<i64>,
        #[arg(long)]
        yes: bool,
    },
    /// Generates a shell completion script (Phase 17).
    Completions { shell: clap_complete::Shell },
}

/// RF-60's exit-code contract: `0` success, `1` generic failure, `2` state error (a second
/// instance, an already-paused daemon, ...), `3` environment error (unreachable X11, an
/// unusable config, a corrupted database).
#[repr(u8)]
enum ExitStatus {
    Ok = 0,
    Failure = 1,
    State = 2,
    Environment = 3,
}

impl From<ExitStatus> for ExitCode {
    fn from(status: ExitStatus) -> Self {
        ExitCode::from(status as u8)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => run_daemon(),
        Command::Completions { shell } => print_completions(shell),
        Command::Today { json: false } => run_today(),
        Command::Today { json: true } => run_today_json(),
        Command::Status { json: false } => run_status(),
        Command::Status { json: true } => run_status_json(),
        Command::Pause { minutes } => run_pause(minutes),
        Command::Resume => run_resume(),
        Command::Prune {
            older_than,
            vacuum_only,
        } => run_prune(older_than, vacuum_only),
        Command::Forget {
            from,
            to,
            window,
            yes,
        } => run_forget(from, to, window, yes),
    }
}

fn print_completions(shell: clap_complete::Shell) -> ExitCode {
    use clap::CommandFactory;
    clap_complete::generate(
        shell,
        &mut Cli::command(),
        "xwindowlog",
        &mut std::io::stdout(),
    );
    ExitStatus::Ok.into()
}

fn run_pause(minutes: Option<u32>) -> ExitCode {
    let runtime_dir = match runtime_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{}", error.message());
            return error.exit_status().into();
        }
    };
    let socket_path = control::socket_path(&runtime_dir);

    match control::send_pause(&socket_path, minutes) {
        Ok(control::Response::Ok { state, .. }) => {
            println!("xwindowlog: {state}");
            ExitStatus::Ok.into()
        }
        Ok(control::Response::Err {
            error: response_error,
            message,
            ..
        }) => {
            eprintln!("xwindowlog: {message}");
            let status = match response_error {
                control::ErrCode::AlreadyPaused | control::ErrCode::NotPaused => ExitStatus::State,
                control::ErrCode::UnsupportedVersion
                | control::ErrCode::Malformed
                | control::ErrCode::Internal => ExitStatus::Failure,
            };
            status.into()
        }
        Err(error) => {
            eprintln!("xwindowlog: daemon is not running: {error}");
            ExitStatus::Environment.into()
        }
    }
}

fn run_resume() -> ExitCode {
    let runtime_dir = match runtime_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{}", error.message());
            return error.exit_status().into();
        }
    };
    let socket_path = control::socket_path(&runtime_dir);

    match control::send_resume(&socket_path) {
        Ok(control::Response::Ok { state, .. }) => {
            println!("xwindowlog: {state}");
            ExitStatus::Ok.into()
        }
        Ok(control::Response::Err {
            error: response_error,
            message,
            ..
        }) => {
            eprintln!("xwindowlog: {message}");
            let status = match response_error {
                control::ErrCode::AlreadyPaused | control::ErrCode::NotPaused => ExitStatus::State,
                control::ErrCode::UnsupportedVersion
                | control::ErrCode::Malformed
                | control::ErrCode::Internal => ExitStatus::Failure,
            };
            status.into()
        }
        Err(error) => {
            eprintln!("xwindowlog: daemon is not running: {error}");
            ExitStatus::Environment.into()
        }
    }
}

enum PruneResult {
    Disabled,
    Pruned(DeletionOutcome),
    VacuumOnly(VacuumOutcome),
}

fn run_prune(older_than: Option<String>, vacuum_only: bool) -> ExitCode {
    match try_run_prune(older_than.as_deref(), vacuum_only) {
        Ok(PruneResult::Disabled) => {
            println!("xwindowlog: retention pruning is disabled (retention_days = 0)");
            ExitStatus::Ok.into()
        }
        Ok(PruneResult::Pruned(outcome)) => report_deletion_outcome(outcome),
        Ok(PruneResult::VacuumOnly(VacuumOutcome::Vacuumed)) => {
            println!("xwindowlog: database vacuumed");
            ExitStatus::Ok.into()
        }
        Ok(PruneResult::VacuumOnly(VacuumOutcome::Exhausted)) => {
            eprintln!("{}", vacuum_exhausted_message(0));
            vacuum_exhausted_exit_code()
        }
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn try_run_prune(older_than: Option<&str>, vacuum_only: bool) -> Result<PruneResult, ReportError> {
    // Parse explicit input before opening the database. Invalid user input must not reach any
    // destructive store operation, even if the database already exists.
    let explicit_seconds = older_than
        .map(|raw| parse_retention_duration(raw, vacuum_only))
        .transpose()?;

    if vacuum_only {
        let mut store = open_report_store()?;
        return store
            .vacuum_only()
            .map(PruneResult::VacuumOnly)
            .map_err(ReportError::Store);
    }

    let config = DaemonConfig::load_default().map_err(ReportError::Config)?;
    let retention_seconds = match explicit_seconds {
        Some(seconds) => Some(seconds),
        None if config.retention_days == 0 => None,
        None => Some(configured_retention_seconds(config.retention_days)?),
    };

    // Keep the configured zero-retention behavior a safe no-op. Opening the existing store still
    // validates the data-home/database path and preserves the normal missing-database contract.
    let retention_seconds = match retention_seconds {
        Some(seconds) => seconds,
        None => {
            let _store = open_report_store()?;
            return Ok(PruneResult::Disabled);
        }
    };

    let now = SystemClock.now_wall();
    let cutoff = WallTs::new(
        now.as_unix_secs()
            .checked_sub(retention_seconds)
            .ok_or_else(|| ReportError::Overflow("retention cutoff overflowed".to_string()))?,
    );
    let mut store = open_report_store()?;
    store
        .prune(cutoff)
        .map(PruneResult::Pruned)
        .map_err(ReportError::Store)
}

fn parse_retention_duration(raw: &str, allow_zero: bool) -> Result<i64, ReportError> {
    let days_text = raw
        .strip_suffix('d')
        .ok_or_else(|| invalid_retention_duration(raw))?;
    let days = days_text
        .parse::<u64>()
        .map_err(|_| invalid_retention_duration(raw))?;
    if days == 0 && !allow_zero {
        return Err(invalid_retention_duration(raw));
    }

    let seconds = days
        .checked_mul(86_400)
        .ok_or_else(|| too_large_retention_duration(raw))?;
    i64::try_from(seconds).map_err(|_| too_large_retention_duration(raw))
}

fn configured_retention_seconds(days: u32) -> Result<i64, ReportError> {
    let seconds = u64::from(days).checked_mul(86_400).ok_or_else(|| {
        ReportError::Overflow("configured retention duration overflowed".to_string())
    })?;
    i64::try_from(seconds)
        .map_err(|_| ReportError::Overflow("configured retention duration overflowed".to_string()))
}

fn invalid_retention_duration(raw: &str) -> ReportError {
    ReportError::Usage(format!(
        "invalid retention duration '{raw}': expected a positive number of days such as 180d"
    ))
}

fn too_large_retention_duration(raw: &str) -> ReportError {
    ReportError::Usage(format!(
        "invalid retention duration '{raw}': duration is too large"
    ))
}

#[derive(Copy, Clone)]
enum ForgetSelector {
    Range { from: WallTs, to: WallTs },
    Window { id: i64 },
}

enum ForgetResult {
    Cancelled,
    Deleted(DeletionOutcome),
}

fn run_forget(
    from: Option<String>,
    to: Option<String>,
    window: Option<i64>,
    yes: bool,
) -> ExitCode {
    match try_run_forget(from.as_deref(), to.as_deref(), window, yes) {
        Ok(ForgetResult::Cancelled) => ExitStatus::Ok.into(),
        Ok(ForgetResult::Deleted(outcome)) => report_deletion_outcome(outcome),
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn try_run_forget(
    from: Option<&str>,
    to: Option<&str>,
    window: Option<i64>,
    yes: bool,
) -> Result<ForgetResult, ReportError> {
    let selector = parse_forget_selector(from, to, window)?;

    if !yes && !confirm_forget(selector) {
        return Ok(ForgetResult::Cancelled);
    }

    let mut store = open_report_store()?;
    let outcome = match selector {
        ForgetSelector::Range { from, to } => {
            store.forget_range(from, to).map_err(ReportError::Store)?
        }
        ForgetSelector::Window { id } => store.forget_window(id).map_err(ReportError::Store)?,
    };
    Ok(ForgetResult::Deleted(outcome))
}

fn parse_forget_selector(
    from: Option<&str>,
    to: Option<&str>,
    window: Option<i64>,
) -> Result<ForgetSelector, ReportError> {
    match (from, to, window) {
        (Some(from), Some(to), None) => {
            let from = parse_forget_timestamp("--from", from)?;
            let to = parse_forget_timestamp("--to", to)?;
            if from >= to {
                return Err(ReportError::Usage(
                    "invalid forget range: --from must be before --to".to_string(),
                ));
            }
            Ok(ForgetSelector::Range { from, to })
        }
        (None, None, Some(id)) => Ok(ForgetSelector::Window { id }),
        _ => Err(ReportError::Usage(
            "forget requires exactly one selector: provide both --from and --to, or --window <id>"
                .to_string(),
        )),
    }
}

fn parse_forget_timestamp(name: &str, raw: &str) -> Result<WallTs, ReportError> {
    let timestamp = OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|error| {
            ReportError::Usage(format!(
                "invalid {name} timestamp '{raw}': expected an RFC3339/ISO 8601 timestamp ({error})"
            ))
        })?;
    if timestamp.nanosecond() != 0 {
        return Err(ReportError::Usage(format!(
            "invalid {name} timestamp '{raw}': use whole-second precision"
        )));
    }
    Ok(WallTs::new(timestamp.unix_timestamp()))
}

fn confirm_forget(selector: ForgetSelector) -> bool {
    match selector {
        ForgetSelector::Range { .. } => {
            eprint!("xwindowlog: confirm permanent deletion of the selected range? [y/N] ");
        }
        ForgetSelector::Window { .. } => {
            eprint!("xwindowlog: confirm permanent deletion of the selected interval? [y/N] ");
        }
    }
    let _ = std::io::stderr().flush();

    let mut response = String::new();
    let confirmed = std::io::stdin()
        .read_line(&mut response)
        .map(|_| {
            let answer = response.trim();
            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false);
    if !confirmed {
        eprintln!("xwindowlog: deletion cancelled");
    }
    confirmed
}

fn report_deletion_outcome(outcome: DeletionOutcome) -> ExitCode {
    match outcome.vacuum {
        VacuumOutcome::Vacuumed => {
            println!(
                "xwindowlog: deleted {} intervals; database vacuumed",
                outcome.deleted_intervals
            );
            ExitStatus::Ok.into()
        }
        VacuumOutcome::Exhausted => {
            eprintln!("{}", vacuum_exhausted_message(outcome.deleted_intervals));
            vacuum_exhausted_exit_code()
        }
    }
}

fn vacuum_exhausted_exit_code() -> ExitCode {
    match VACUUM_EXHAUSTED_EXIT_CODE {
        2 => ExitStatus::State.into(),
        3 => ExitStatus::Environment.into(),
        _ => ExitStatus::Failure.into(),
    }
}

#[derive(Debug)]
enum ReportError {
    Config(ConfigError),
    NoDataHome,
    MissingDatabase(PathBuf),
    Store(StoreError),
    Time(String),
    Overflow(String),
    Usage(String),
    Json(serde_json::Error),
}

impl ReportError {
    fn exit_status(&self) -> ExitStatus {
        match self {
            ReportError::Config(_)
            | ReportError::NoDataHome
            | ReportError::Time(_)
            | ReportError::Overflow(_) => ExitStatus::Environment,
            ReportError::Usage(_) => ExitStatus::Failure,
            ReportError::MissingDatabase(_) => ExitStatus::State,
            ReportError::Json(_) => ExitStatus::Failure,
            ReportError::Store(error) => match error.exit_code() {
                2 => ExitStatus::State,
                3 => ExitStatus::Environment,
                _ => ExitStatus::Failure,
            },
        }
    }

    fn message(&self) -> String {
        match self {
            ReportError::Config(error) => format!("xwindowlog: {error}"),
            ReportError::NoDataHome => {
                "xwindowlog: could not determine XDG data home (no XDG_DATA_HOME or HOME)"
                    .to_string()
            }
            ReportError::MissingDatabase(path) => format!(
                "xwindowlog: database does not exist at {}; start the daemon before requesting a report",
                path.display()
            ),
            ReportError::Store(error) => format!("xwindowlog: {error}"),
            ReportError::Time(error) => format!("xwindowlog: could not resolve report time: {error}"),
            ReportError::Overflow(error) => format!("xwindowlog: report value overflowed: {error}"),
            ReportError::Usage(error) => format!("xwindowlog: {error}"),
            ReportError::Json(error) => format!("xwindowlog: could not serialize JSON report: {error}"),
        }
    }
}

#[derive(Copy, Clone)]
enum ReportFormat {
    PlainText,
    Json,
}

const REPORT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Serialize)]
struct TodayJsonReport {
    schema_version: u8,
    report: &'static str,
    intervals: Vec<TodayJsonInterval>,
}

#[derive(Debug, Serialize)]
struct TodayJsonInterval {
    start: i64,
    end: i64,
    duration_seconds: u64,
    app_id: String,
    title: String,
    state: String,
}

#[derive(Debug, Serialize)]
struct StatusJsonReport {
    schema_version: u8,
    report: &'static str,
    state: String,
    app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    duration_seconds: u64,
}

fn run_today() -> ExitCode {
    match try_run_today(ReportFormat::PlainText) {
        Ok(()) => ExitStatus::Ok.into(),
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn run_today_json() -> ExitCode {
    match try_run_today(ReportFormat::Json) {
        Ok(()) => ExitStatus::Ok.into(),
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn try_run_today(format: ReportFormat) -> Result<(), ReportError> {
    let now = SystemClock.now_wall();
    let offset = resolve_local_offset();
    let store = open_report_store()?;
    let intervals = query_today_intervals(&store, now, offset)?;
    match format {
        ReportFormat::PlainText => print_today(&intervals, offset),
        ReportFormat::Json => print_today_json(&intervals),
    }
}

fn open_report_store() -> Result<Store, ReportError> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or(ReportError::NoDataHome)?;
    let db_path = data_home.join("xwindowlog").join("xwindowlog.db");
    if !db_path.is_file() {
        return Err(ReportError::MissingDatabase(db_path));
    }
    Store::open(&db_path).map_err(ReportError::Store)
}

fn resolve_local_offset() -> UtcOffset {
    match UtcOffset::current_local_offset() {
        Ok(offset) => offset,
        Err(error) => {
            eprintln!("xwindowlog: could not determine the local UTC offset ({error}); using UTC");
            UtcOffset::UTC
        }
    }
}

struct DayBounds {
    start: WallTs,
    query_end: WallTs,
}

fn day_bounds(now: WallTs, offset: UtcOffset) -> Result<DayBounds, ReportError> {
    let utc_now = OffsetDateTime::from_unix_timestamp(now.as_unix_secs())
        .map_err(|error| ReportError::Time(error.to_string()))?;
    let local_date = utc_now.to_offset(offset).date();
    let next_date = local_date
        .next_day()
        .ok_or_else(|| ReportError::Time("local calendar date overflowed".to_string()))?;
    let start = OffsetDateTime::new_in_offset(local_date, Time::MIDNIGHT, offset).unix_timestamp();
    let end = OffsetDateTime::new_in_offset(next_date, Time::MIDNIGHT, offset).unix_timestamp();
    let query_end = now.as_unix_secs().min(end);
    Ok(DayBounds {
        start: WallTs::new(start),
        query_end: WallTs::new(query_end),
    })
}

fn query_today_intervals(
    store: &Store,
    now: WallTs,
    offset: UtcOffset,
) -> Result<Vec<DisplayInterval>, ReportError> {
    let bounds = day_bounds(now, offset)?;
    let clipped = store
        .clipped_intervals(bounds.start, bounds.query_end)
        .map_err(ReportError::Store)?;
    store
        .lookup_display_values(&clipped)
        .map_err(ReportError::Store)
}

fn print_today(intervals: &[DisplayInterval], offset: UtcOffset) -> Result<(), ReportError> {
    if intervals.is_empty() {
        println!("xwindowlog: no activity today");
        return Ok(());
    }
    for interval in intervals {
        let start = format_hhmm(interval.start, offset)?;
        let end = format_hhmm(interval.end, offset)?;
        println!(
            "{start}–{end} · {} · {} · {} · {}",
            interval.app_id,
            interval.title,
            interval.state,
            format_duration(duration_secs(interval.start, interval.end))
        );
    }
    Ok(())
}

fn run_status() -> ExitCode {
    match try_run_status(ReportFormat::PlainText) {
        Ok(()) => ExitStatus::Ok.into(),
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn run_status_json() -> ExitCode {
    match try_run_status(ReportFormat::Json) {
        Ok(()) => ExitStatus::Ok.into(),
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_status().into()
        }
    }
}

fn try_run_status(format: ReportFormat) -> Result<(), ReportError> {
    let config = DaemonConfig::load_default().map_err(ReportError::Config)?;
    let now = SystemClock.now_wall();
    let offset = resolve_local_offset();
    let store = open_report_store()?;
    // Status deliberately consumes the same clipped read path as today. The
    // open row below only selects the persisted label/state to display.
    let intervals = query_today_intervals(&store, now, offset)?;
    let open = store.open_interval().map_err(ReportError::Store)?;
    match format {
        ReportFormat::PlainText => {
            print_status_line(open.as_ref(), &intervals, config.status_show_title)
        }
        ReportFormat::Json => {
            print_status_json(open.as_ref(), &intervals, config.status_show_title)
        }
    }
}

fn print_status_line(
    open: Option<&OpenInterval>,
    intervals: &[DisplayInterval],
    show_title: bool,
) -> Result<(), ReportError> {
    match open {
        Some(interval) if interval.state == "active" => {
            let seconds = status_duration(interval, intervals)?;
            if show_title {
                println!(
                    "xwindowlog: {} · {} · {} today",
                    interval.app_id,
                    interval.title,
                    format_duration(seconds)
                );
            } else {
                println!(
                    "xwindowlog: {} · {} today",
                    interval.app_id,
                    format_duration(seconds)
                );
            }
        }
        Some(interval) => {
            let seconds = status_duration(interval, intervals)?;
            println!(
                "xwindowlog: {} · {} today",
                interval.state,
                format_duration(seconds)
            );
        }
        None => println!("xwindowlog: not tracking · 0m today"),
    }
    Ok(())
}

fn status_duration(open: &OpenInterval, intervals: &[DisplayInterval]) -> Result<u64, ReportError> {
    let mut total = 0_u64;
    for candidate in intervals.iter().filter(|candidate| {
        if open.state == "active" {
            candidate.state == "active" && candidate.app_id == open.app_id
        } else {
            candidate.state == open.state
        }
    }) {
        total = total
            .checked_add(duration_secs(candidate.start, candidate.end))
            .ok_or(ReportError::Overflow(if open.state == "active" {
                "active duration exceeded the report counter".to_string()
            } else {
                "non-active duration exceeded the report counter".to_string()
            }))?;
    }
    Ok(total)
}

fn print_today_json(intervals: &[DisplayInterval]) -> Result<(), ReportError> {
    let intervals = intervals
        .iter()
        .map(|interval| TodayJsonInterval {
            start: interval.start.as_unix_secs(),
            end: interval.end.as_unix_secs(),
            duration_seconds: duration_secs(interval.start, interval.end),
            app_id: interval.app_id.clone(),
            title: interval.title.clone(),
            state: interval.state.clone(),
        })
        .collect();
    print_json(&TodayJsonReport {
        schema_version: REPORT_SCHEMA_VERSION,
        report: "today",
        intervals,
    })
}

fn print_status_json(
    open: Option<&OpenInterval>,
    intervals: &[DisplayInterval],
    show_title: bool,
) -> Result<(), ReportError> {
    let report = match open {
        Some(interval) => StatusJsonReport {
            schema_version: REPORT_SCHEMA_VERSION,
            report: "status",
            state: interval.state.clone(),
            app_id: (interval.state == "active").then(|| interval.app_id.clone()),
            title: show_title.then(|| interval.title.clone()),
            duration_seconds: status_duration(interval, intervals)?,
        },
        None => StatusJsonReport {
            schema_version: REPORT_SCHEMA_VERSION,
            report: "status",
            state: "not_tracking".to_string(),
            app_id: None,
            title: None,
            duration_seconds: 0,
        },
    };
    print_json(&report)
}

fn print_json<T: Serialize>(report: &T) -> Result<(), ReportError> {
    let payload = serde_json::to_string(report).map_err(ReportError::Json)?;
    println!("{payload}");
    Ok(())
}

fn format_hhmm(timestamp: WallTs, offset: UtcOffset) -> Result<String, ReportError> {
    let utc = OffsetDateTime::from_unix_timestamp(timestamp.as_unix_secs())
        .map_err(|error| ReportError::Time(error.to_string()))?;
    let local = utc.to_offset(offset);
    Ok(format!("{:02}:{:02}", local.hour(), local.minute()))
}

fn format_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    let hours = minutes / 60;
    if hours > 0 {
        format!("{}h{}m", hours, minutes % 60)
    } else {
        format!("{}m", minutes)
    }
}

/// The lock-file name inside `$XDG_RUNTIME_DIR` (design §2 D-2 fd table, daemon-lifecycle
/// "Single instance via flock").
const LOCK_FILE_NAME: &str = "xwindowlog.lock";

#[derive(Debug)]
enum StartupError {
    Config(ConfigError),
    NoRuntimeDir,
    NoDataHome,
    AlreadyRunning,
    Lock(std::io::Error),
    Socket(std::io::Error),
    Signals(std::io::Error),
    Store(StoreError),
    X11(X11InitError),
    Source(xwindowlog::tracker::SourceError),
}

impl StartupError {
    fn exit_status(&self) -> ExitStatus {
        match self {
            StartupError::Config(_)
            | StartupError::NoRuntimeDir
            | StartupError::NoDataHome
            | StartupError::Lock(_)
            | StartupError::Socket(_)
            | StartupError::Signals(_)
            | StartupError::Store(_)
            | StartupError::X11(_)
            | StartupError::Source(_) => ExitStatus::Environment,
            StartupError::AlreadyRunning => ExitStatus::State,
        }
    }

    fn message(&self) -> String {
        match self {
            StartupError::Config(e) => format!("xwindowlog: {e}"),
            StartupError::NoRuntimeDir => {
                "xwindowlog: $XDG_RUNTIME_DIR is not set; cannot locate the lock/control socket"
                    .to_string()
            }
            StartupError::NoDataHome => {
                "xwindowlog: could not determine XDG data home (no XDG_DATA_HOME or HOME)"
                    .to_string()
            }
            StartupError::AlreadyRunning => {
                "xwindowlog: another instance is already running (lock held on the runtime lock \
                 file)"
                    .to_string()
            }
            StartupError::Lock(e) => format!("xwindowlog: failed to open the lock file: {e}"),
            StartupError::Socket(e) => format!("xwindowlog: failed to bind control socket: {e}"),
            StartupError::Signals(e) => {
                format!("xwindowlog: failed to install signal handling: {e}")
            }
            StartupError::Store(e) => format!("xwindowlog: {e}"),
            StartupError::X11(e) => format!("xwindowlog: {e}"),
            StartupError::Source(e) => format!("xwindowlog: reactor error: {}", e.0),
        }
    }
}

/// Resolves `$XDG_RUNTIME_DIR`, the same directory RF-49's control socket and RF-34's lock file
/// both live in (design §2 D-5: "no new trust boundary").
fn runtime_dir() -> Result<PathBuf, StartupError> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(StartupError::NoRuntimeDir)
}

/// Resolves `$XDG_DATA_HOME` (falling back to `$HOME/.local/share`, the XDG base directory
/// spec's own fallback) for RF-10's `xwindowlog.db`.
fn data_home() -> Result<PathBuf, StartupError> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or(StartupError::NoDataHome)
}

/// Acquires `flock(2)` `LOCK_EX | LOCK_NB` on `$XDG_RUNTIME_DIR/xwindowlog.lock` (RF-21, RF-34).
/// A `SIGKILL`ed predecessor's lock is released by the kernel before this ever runs, so opening
/// (never truncating) the same path always finds either no lock or a live holder — never a
/// stale one (daemon-lifecycle "A crashed instance's lock is released automatically").
fn acquire_lock(runtime_dir: &Path) -> Result<Flock<File>, StartupError> {
    let path = runtime_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(StartupError::Lock)?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_file, errno)| {
        if errno == nix::errno::Errno::EWOULDBLOCK {
            StartupError::AlreadyRunning
        } else {
            StartupError::Lock(std::io::Error::from(errno))
        }
    })
}

fn run_daemon() -> ExitCode {
    match try_run_daemon() {
        Ok(()) => ExitStatus::Ok.into(),
        Err(err) => {
            eprintln!("{}", err.message());
            err.exit_status().into()
        }
    }
}

/// Builds the production `X11Adapter`, threading the CONFIGURED `afk_threshold` into its
/// owned `Reconnector` (RF-32 T2a) instead of a hardcoded default, which would silently
/// diverge from `afk_threshold` once `recover()` gets a production caller (T5). `display:
/// None` matches this function's own `X11Source::connect_with_afk_threshold(None, ..)` call
/// above.
fn build_x11_adapter(
    source: X11Source,
    excluder: Rc<RefCell<Excluder>>,
    afk_threshold: Duration,
) -> X11Adapter {
    X11Adapter::with_reconnect_config(source, excluder, None, afk_threshold)
}

fn try_run_daemon() -> Result<(), StartupError> {
    let config = DaemonConfig::load_default().map_err(StartupError::Config)?;
    let runtime_dir = runtime_dir()?;
    let _lock = acquire_lock(&runtime_dir)?;

    let db_path = data_home()?.join("xwindowlog").join("xwindowlog.db");
    let mut store = Store::open(&db_path).map_err(StartupError::Store)?;

    let clock = SystemClock;
    let recovery_at = clock.now_wall();
    store
        .recover_on_startup(recovery_at)
        .map_err(StartupError::Store)?;
    // RF-36's recovery above always leaves exactly one `unknown` interval open — a fresh
    // Tracker must be told so, or its first real event emits OpenOnly instead of Transition
    // and leaves that row open forever alongside the new one (tracker.rs task 15.10 doc).
    let mut tracker = config.tracker.resuming_after_recovery(recovery_at);

    let (x11_source, _diagnostics) =
        X11Source::connect_with_afk_threshold(None, config.afk_threshold)
            .map_err(StartupError::X11)?;
    let excluder = Rc::new(RefCell::new(config.excluder));
    let x11_adapter = build_x11_adapter(x11_source, Rc::clone(&excluder), config.afk_threshold);

    let (monitor, degrade_diagnostic) = logind::init_session_monitor();
    if let Some(diagnostic) = degrade_diagnostic {
        eprintln!("xwindowlog: {diagnostic}");
    }
    let mut suspend_inhibitor = SuspendInhibitor::new();
    let logind_source = match monitor {
        Some(mut monitor) => {
            let mut locked_hint = LockedHintTracker::new();
            match logind::resolve_session_at_startup(&mut monitor, std::process::id()) {
                SessionResolution::Resolved => {
                    if let Ok(locked) = monitor.locked_hint() {
                        locked_hint.seed(locked);
                    }
                }
                SessionResolution::RetryNeeded(diagnostic) => {
                    eprintln!("xwindowlog: {diagnostic}");
                }
            }
            if let Some(diagnostic) = suspend_inhibitor.acquire(&mut monitor) {
                eprintln!("xwindowlog: {diagnostic}");
            }
            LogindSource::Real(Box::new(LogindAdapter::new(monitor, locked_hint)))
        }
        None => LogindSource::Absent(NullFd::new().map_err(StartupError::Signals)?),
    };

    let signals = SelfPipe::install().map_err(StartupError::Signals)?;
    let socket_path = control::socket_path(&runtime_dir);
    let listener = control::bind(&socket_path).map_err(StartupError::Socket)?;
    let own_uid = Uid::effective();
    let mut reactor: Reactor = ReactorSource::new(
        x11_adapter,
        logind_source,
        signals,
        listener,
        own_uid,
        clock,
    );

    let shutdown_result = run_event_loop(
        &mut reactor,
        &mut tracker,
        &mut store,
        &excluder,
        &mut suspend_inhibitor,
        &SystemClock,
    );

    let _ = std::fs::remove_file(&socket_path);
    shutdown_result
}

/// The composed daemon's own loop (design §1: this is the whole of "reactor construction and
/// `Effect` application" — no state-machine or storage decision is made here, only dispatched).
/// Runs until `SourceEvent::Shutdown` (RF-33's `SIGTERM`/`SIGINT`).
fn run_event_loop(
    reactor: &mut Reactor,
    tracker: &mut xwindowlog::tracker::Tracker,
    store: &mut Store,
    excluder: &Rc<RefCell<xwindowlog::exclude::Excluder>>,
    suspend_inhibitor: &mut SuspendInhibitor,
    clock: &SystemClock,
) -> Result<(), StartupError> {
    loop {
        // `deadline: None` means block indefinitely; `next_event`'s own contract guarantees
        // `Ok(None)` is only possible when a caller-supplied deadline elapses, so this always
        // yields a real event or an error.
        let event = reactor
            .next_event(None)
            .map_err(StartupError::Source)?
            .expect("next_event(None) never returns Ok(None): no external deadline was given");

        // RF-9: handled here, invisible to the tracker (whose own ReloadConfig arm is already
        // a no-op) — the Excluder is shared with the X11 adapter via the same Rc<RefCell<_>>,
        // so a hot-swap here is picked up by the very next title/window evaluation.
        if matches!(event, SourceEvent::ReloadConfig) {
            match xwindowlog::exclude::Excluder::load_default() {
                Ok(fresh) => *excluder.borrow_mut() = fresh,
                Err(e) => eprintln!(
                    "xwindowlog: SIGHUP config reload failed, keeping previous rules: {e}"
                ),
            }
        }

        let shutdown = matches!(event, SourceEvent::Shutdown);
        let prepare_for_sleep = match event {
            SourceEvent::PrepareForSleep(going_to_sleep) => Some(going_to_sleep),
            _ => None,
        };

        let now_wall = clock.now_wall();
        let now_mono = clock.now_mono();

        // RF-49's `--minutes N` dual-clock expiry (design §2 D-5) is refreshed from the retained
        // wall target. The reactor compares WallTs values and uses only the resulting duration for
        // its monotonic poll deadline (RF-28); it also preempts a post-suspend event when the wall
        // target has already elapsed. A manual `resume` already clears the reactor's own `paused`
        // flag inside `decide` (reactor.rs, Phase 14); an unattended expiry has no other path back
        // in, hence `mark_resumed`.
        match &event {
            SourceEvent::Pause { .. } | SourceEvent::PrepareForSleep(_) => {
                reactor.refresh_pause_deadline(now_wall, now_mono)
            }
            SourceEvent::Resume => reactor.cancel_timer(Timer::PauseExpiry),
            SourceEvent::DeadlineElapsed(Timer::PauseExpiry) => reactor.mark_resumed(),
            _ => {}
        }

        let effects = tracker.on_event(event, now_wall, now_mono);
        apply_effects(effects, store, reactor)?;

        // RF-27: the inhibitor release/reacquire happens strictly AFTER the interval closure
        // above is committed (`SuspendInhibitor`'s own doc) — sequencing that from outside the
        // reactor's generic fd table is exactly what `ReactorSource::logind_mut` exists for.
        if let Some(going_to_sleep) = prepare_for_sleep {
            if let LogindSource::Real(adapter) = reactor.logind_mut() {
                let diagnostic = if going_to_sleep {
                    suspend_inhibitor.release_for_suspend(adapter.monitor_mut());
                    None
                } else {
                    suspend_inhibitor.reacquire_after_resume(adapter.monitor_mut())
                };
                if let Some(diagnostic) = diagnostic {
                    eprintln!("xwindowlog: {diagnostic}");
                }
            }
        }

        if shutdown {
            if let LogindSource::Real(adapter) = reactor.logind_mut() {
                suspend_inhibitor.release_for_suspend(adapter.monitor_mut());
            }
            // D-12's in-process wakeup counter, "logged on shutdown" (task 14.11's tail,
            // deferred to this phase's real shutdown hook). This crate has no structured
            // logging framework, so it uses the same plain stderr diagnostic convention as
            // every other line in this module rather than inventing a log-level distinction.
            eprintln!(
                "xwindowlog: reactor recorded {} poll(2) wakeup(s) over its lifetime",
                reactor.wakeups()
            );
            return Ok(());
        }
    }
}

/// Applies every `Effect` the tracker returned, in order: `Transition`/`CloseOnly`/`OpenOnly`
/// reach `store.rs` through its existing `IntervalStore` trait; `ArmTimer`/`CancelTimer` reach
/// the reactor's own deadline set; `Diagnostic` goes to stderr, never persisted (RF-24/RF-25/
/// RF-29's own contract). No state-machine or storage logic is duplicated here (task 15.12).
fn apply_effects(
    effects: Vec<xwindowlog::tracker::Effect>,
    store: &mut Store,
    reactor: &mut Reactor,
) -> Result<(), StartupError> {
    use xwindowlog::tracker::Effect;

    for effect in effects {
        match effect {
            Effect::Transition { at, open } => {
                store.transition(at, open).map_err(StartupError::Store)?
            }
            Effect::CloseOnly { at } => store.close_only(at).map_err(StartupError::Store)?,
            Effect::OpenOnly { at, open } => {
                store.open_only(at, open).map_err(StartupError::Store)?
            }
            Effect::ArmTimer(timer, at) => reactor.arm_timer(timer, at),
            Effect::CancelTimer(timer) => reactor.cancel_timer(timer),
            Effect::Diagnostic(message) => eprintln!("xwindowlog: {message}"),
        }
    }
    Ok(())
}
