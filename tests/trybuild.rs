//! Compile-fail fixtures for invariants that are enforced by the type system
//! rather than by assertion.
//!
//! These are the guarantees a normal test cannot make. A unit test samples a
//! property on the inputs it happens to try; a compile-fail fixture proves the
//! violation cannot be written at all. Each fixture is named after the
//! invariant it guards, so a failure says which one broke rather than just
//! "trybuild failed".

/// RF-28 / design D-9: `WallTs` and `MonoInstant` are never derived from one
/// another. The monotonic clock freezes across suspend and the wall clock does
/// not, so any code able to convert between them will eventually produce
/// `locked` intervals shorter than reality. The guarantee is the *absence* of
/// a `From` in either direction — if one is ever added, these fixtures fail.
#[test]
fn wall_ts_and_mono_instant_have_no_conversion() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/trybuild/fail/no_from_between_wallts_and_mono_instant.rs");
    cases.compile_fail("tests/trybuild/fail/no_from_between_mono_instant_and_wallts.rs");
}

/// RF-7 / §14.3: a `SafeTitle` may only be produced by putting a `RawTitle`
/// through `Excluder::evaluate`. `SafeTitle::from_sanitized` is private to
/// `exclude.rs` for exactly this reason: were it `pub(crate)`, any module could
/// mint a "safe" title from a raw string that had never been filtered, and the
/// sanitization boundary would be a naming convention rather than a guarantee.
/// This database holds every window title the user has seen, so that
/// distinction is the difference between a privacy control and a comment.
#[test]
fn safe_title_cannot_be_minted_outside_the_sanitizer() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/trybuild/fail/no_safetitle_bypass.rs");
}
