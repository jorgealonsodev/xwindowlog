//! Library crate root (task 8.0).
//!
//! The crate was binary-only through Phase 7, which meant `tests/*.rs`
//! integration tests — external crates that link against this one — could
//! not reach any of its modules: `Tracker`, `Excluder`, `Store` and their
//! supporting types all lived behind `mod` declarations private to
//! `src/main.rs`'s own binary crate. That made `tests/pipeline_integration.rs`
//! (task 8.1+) and `tests/invariants.rs` (task 8.0b, the location PRD.md's
//! M-1 line names as the proof of the product's headline metric)
//! structurally impossible, which is why Phase 6 had to put P1 and P2 inside
//! `tracker.rs`'s own `#[cfg(test)]` module instead.
//!
//! This file is the fix: the module tree moves here, `pub`, so `tests/*.rs`
//! can `use xwindowlog::...`. `src/main.rs` becomes a thin shell over this
//! crate; argument parsing and daemon composition are Phase 15-17's job
//! (design §1: `main.rs · reactor · CLI (clap) · flock, config`).
//!
//! Module boundary ordering matches design §1's layering (pure/no I/O
//! first, then I/O adapters, then composition) and PRD §14.3's data-flow
//! ordering (`x11.rs -> exclude.rs -> tracker.rs -> store.rs`).

pub mod clock;
pub mod control;
pub mod exclude;
pub mod logind;
pub mod reactor;
pub mod signals;
pub mod store;
pub mod tracker;
pub mod x11;
