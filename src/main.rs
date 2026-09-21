//! clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10);
//! reactor construction and `Effect` application.
//!
//! Design §1: `main.rs` is composition only — clap, flock, config, reactor construction, effect
//! application (task 15.12). No state-machine or storage logic is duplicated here; it all stays
//! in `tracker.rs`/`store.rs`, reached only through their existing public API.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use clap::{Parser, Subcommand};
use nix::fcntl::{Flock, FlockArg};
use nix::unistd::Uid;

use xwindowlog::clock::{Clock, SystemClock};
use xwindowlog::control;
use xwindowlog::logind::{
    self, LockedHintTracker, SessionMonitor as _, SessionResolution, SuspendInhibitor,
};
use xwindowlog::reactor::ReactorSource;
use xwindowlog::signals::SelfPipe;
use xwindowlog::store::{IntervalStore as _, Store, StoreError};
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
        _ => not_yet_implemented(),
    }
}

/// Phase 16/17's subcommands are dispatched here but their behavior is out of Phase 15's scope
/// (task 15.1: "argument parsing and dispatch only").
fn not_yet_implemented() -> ExitCode {
    eprintln!("xwindowlog: this subcommand's behavior lands in Phase 16/17");
    ExitStatus::Failure.into()
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
    let x11_adapter = X11Adapter::new(x11_source, Rc::clone(&excluder));

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

        // RF-49's `--minutes N` dual-clock expiry (design §2 D-5): the monotonic deadline is
        // derived from a WallTs comparison, never a direct WallTs->MonoInstant conversion
        // (RF-28's own prohibition — see clock.rs::MonoInstant::checked_add's doc). A manual
        // `resume` already clears the reactor's own `paused` flag inside `decide`
        // (reactor.rs, Phase 14); an unattended expiry has no other path back in, hence
        // `mark_resumed`.
        match &event {
            SourceEvent::Pause {
                until: Some(target),
            } => {
                let delta = target
                    .as_unix_secs()
                    .saturating_sub(now_wall.as_unix_secs())
                    .max(0) as u64;
                if let Some(deadline) = now_mono.checked_add(std::time::Duration::from_secs(delta))
                {
                    reactor.arm_timer(Timer::PauseExpiry, deadline);
                }
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
