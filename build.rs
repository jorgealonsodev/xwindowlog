use std::env;
use std::fs::File;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

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

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/main.rs");

    let man_page_path = PathBuf::from(
        env::var_os("OUT_DIR").expect("Cargo must provide the build output directory"),
    )
    .join("xwindowlog.1");
    let mut man_page = File::create(&man_page_path).expect("create generated xwindowlog man page");
    clap_mangen::Man::new(Cli::command())
        .render(&mut man_page)
        .expect("render generated xwindowlog man page");

    println!(
        "cargo:rustc-env=XWINDOWLOG_MANPAGE={}",
        man_page_path.display()
    );
}
