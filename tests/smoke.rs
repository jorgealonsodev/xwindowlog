//! Task 8.0 acceptance: a trivial `tests/*.rs` file can `use xwindowlog::...`
//! and compile. This is the structural proof that the crate is no longer
//! binary-only — `tests/pipeline_integration.rs` and `tests/invariants.rs`
//! (task 8.0b onward) depend on exactly this reachability.

use xwindowlog::tracker::Tracker;

#[test]
fn the_library_crate_is_reachable_from_an_integration_test() {
    // Constructing a `Tracker` from outside the crate is only possible if
    // `tracker` is a `pub mod` reachable from `lib.rs` — this would not
    // compile against the Phase 1-7 binary-only crate layout.
    let _tracker = Tracker::new();
}
