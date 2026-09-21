//! clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10);
//! reactor construction and `Effect` application.
//!
//! Design §1: `main.rs` is composition only — clap, flock, config, reactor construction, effect
//! application (task 15.12). No state-machine or storage logic is duplicated here; it all stays
//! in `tracker.rs`/`store.rs`, reached only through their existing public API.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod config;

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
    #[allow(
        dead_code,
        reason = "wired by the flock/daemon work unit landing right after this one"
    )]
    State = 2,
    #[allow(
        dead_code,
        reason = "wired by the flock/daemon work unit landing right after this one"
    )]
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
        Command::Completions { shell } => print_completions(shell),
        // `Daemon`'s real behavior (config load, flock, reactor composition) lands in the next
        // work unit of this phase; argument parsing/dispatch is task 15.1's whole scope.
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
