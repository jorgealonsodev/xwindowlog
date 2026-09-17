//! Acceptance check (E-2, D-10 layer 2, assumption A-5): diagnosable-in-10-seconds
//! scaffolding for the RF-11 schema floor. This is NOT the load-bearing proof —
//! that is D-10 layer 1 (the behavioural `idx_intervals_one_open` test in Phase 3,
//! task 3.1). This test only makes a version regression fail with a clear message
//! instead of a mysterious constraint failure somewhere else.
//!
//! Floor is SQLite 3.31.0 (`3_031_000`), per RF-11 — the version that introduced
//! generated columns and filtered unique indexes, which D-1 depends on. D-1 also
//! bans `RETURNING` (added in 3.35), so this floor is deliberately not raised to
//! match it.

#[test]
fn linked_sqlite_meets_rf11_floor() {
    let linked = rusqlite::version_number();
    assert!(
        linked >= 3_031_000,
        "linked SQLite version {linked} is below the RF-11 floor of 3.31.0 \
         (3_031_000); D-1's generated `open_marker` column and filtered unique \
         index require it. See design.md D-10 for the full three-layer proof."
    );
}
