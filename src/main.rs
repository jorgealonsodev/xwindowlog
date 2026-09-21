//! clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10);
//! reactor construction and `Effect` application.
//!
//! Design §1: `main.rs` is composition only — clap, flock, config, reactor construction, effect
//! application (task 15.12). No state-machine or storage logic is duplicated here; it all stays
//! in `tracker.rs`/`store.rs`, reached only through their existing public API.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use nix::fcntl::{Flock, FlockArg};

mod config;

use config::{ConfigError, DaemonConfig};

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
    AlreadyRunning,
    Lock(std::io::Error),
}

impl StartupError {
    fn exit_status(&self) -> ExitStatus {
        match self {
            StartupError::Config(_) | StartupError::NoRuntimeDir | StartupError::Lock(_) => {
                ExitStatus::Environment
            }
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
            StartupError::AlreadyRunning => {
                "xwindowlog: another instance is already running (lock held on the runtime lock \
                 file)"
                    .to_string()
            }
            StartupError::Lock(e) => format!("xwindowlog: failed to open the lock file: {e}"),
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
    let _config = DaemonConfig::load_default().map_err(StartupError::Config)?;
    let runtime_dir = runtime_dir()?;
    let _lock = acquire_lock(&runtime_dir)?;

    // Composition beyond the lock (reactor construction, X11/logind/store wiring, signal
    // handling) lands in later work units of this phase; holding the lock is this unit's whole
    // observable contract (task 15.3/15.4).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
