//! RF-28 / design.md D-9: `WallTs` and `MonoInstant` are never derived from
//! one another. This is enforced by the absence of a `From`/`Into`
//! conversion, and the absence is what this compile-fail fixture proves —
//! if either conversion is ever added, this test starts failing (tasks.md
//! 2.7/2.8).

#[test]
fn wall_ts_and_mono_instant_have_no_conversion() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/trybuild/fail/*.rs");
}
