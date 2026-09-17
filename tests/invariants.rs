//! Properties P1, P2, P3 (PRD §15.5 / interval-tracking spec.md /
//! privacy-filtering spec.md), the location PRD.md's M-1 line names as the
//! proof of the product's headline metric: "Property P2 in
//! `tests/invariants.rs` with `proptest`, plus direct SQL verification per
//! day. It is not an aspiration: it is a test."
//!
//! Task 8.0b moves P1 and P2 here from `tracker.rs`'s own `#[cfg(test)]`
//! module now that task 8.0's `src/lib.rs` makes this file possible at all
//! — Phase 6 had to put them inside `tracker.rs` for exactly the reason
//! task 8.0's doc comment explains. Both are moved **verbatim** in
//! substance: the per-bucket assertions in `p2` are kept exactly as they
//! were, because the grand total is conserved by any interior boundary
//! shift, so those per-bucket constants are the only thing that catches
//! one — and seg3's non-zero `idle_for` is what makes the RF-4 backdating
//! path actually run. Only the crate-boundary mechanics changed: `crate::`
//! becomes `xwindowlog::`, and `WallTs`'s tuple-literal/`.0` access (which
//! only works inside the crate that defines it, since the field is
//! `pub(crate)`) becomes `WallTs::new`/`.as_unix_secs()` (task 8.0b's own
//! addition to `clock.rs`, needed for exactly this move).
//!
//! P3 (task 8.1) is new in this phase: privacy-filtering's own property,
//! "exclusion preserves elapsed time" — run the same scripted sequence
//! through the real `Excluder` twice, once with exclusion rules active and
//! once with every rule disabled, and confirm both runs record the same
//! total duration `D`. Lives here, not in `pipeline_integration.rs`,
//! because it is a property over the tracker's own effect boundaries
//! (`Tracker`/`Excluder` only) — no `Store` involved — matching P1/P2's own
//! shape; `pipeline_integration.rs` is reserved for the full
//! exclude→tracker→store wiring and the §14.3 ordering guarantee.
//!
//! P3 asserts per-`app_id` duration buckets, not only the grand total
//! (adversarial verification finding, post-Phase-8): a telescoping total is
//! conserved by any *interior* boundary shift, exactly like P2's own
//! comment explains for the working-day sum, so a total-only P3 only
//! catches a dropped interval when the drop happens to move the very first
//! or very last boundary. The case that matters is different and more
//! privacy-relevant: an excluded window opening no interval of its own,
//! folding its time into the *previous visible app's* bucket — e.g.
//! KeePassXC minutes silently recorded as Firefox minutes. Per-bucket
//! assertions catch that misattribution; a total-only assertion does not.
//!
//! Task 8.4's `WindowInfo.title: String` -> `SafeTitle` swap lands between
//! 8.0b and 8.1 in this same batch, so every window fixture in this file
//! (moved-in P1/P2 included) goes through `safe_title()` below — the real
//! `Excluder::evaluate` boundary — rather than `title.to_string()`; there is
//! no other way to construct a `SafeTitle` (`from_sanitized` is private to
//! `exclude.rs`).

use std::collections::HashMap;
use std::time::Duration;

use xwindowlog::clock::{duration_secs, Clock, FakeClock, WallTs};
use xwindowlog::exclude::{Excluder, RawTitle, SafeTitle};
use xwindowlog::store::IntervalState;
use xwindowlog::tracker::{Effect, SourceEvent, Timer, Tracker, WindowInfo};

/// Sanitizes `title` through a real, fixed pure-passthrough `Excluder` (no
/// exclusion rules, no secret redaction) to obtain a `SafeTitle` — the only
/// legitimate way to build one outside `exclude.rs`. Shared by every test
/// module in this file that needs a plain, unmodified title.
fn safe_title(title: &str) -> SafeTitle {
    Excluder::from_toml_str(
        r#"
        sanitize_secrets = false
        disable_default_excludes = [
            "password-managers", "banking-generic", "private-browsing",
            "gpg-ssh-prompts", "2fa-otp",
        ]
        "#,
    )
    .expect("valid literal config compiles")
    .evaluate("test-fixture-app-never-excluded", RawTitle::new(title))
    .title
}

fn window(app_id: &str, title: &str, pid: Option<u32>) -> WindowInfo {
    WindowInfo {
        app_id: app_id.to_string(),
        title: safe_title(title),
        pid,
    }
}

/// Sends `event` at the `FakeClock`'s current wall/mono readings — the same
/// helper `tracker.rs`'s own Phase 6 tests used, moved verbatim.
fn send(tracker: &mut Tracker, event: SourceEvent, clock: &FakeClock) -> Vec<Effect> {
    tracker.on_event(event, clock.now_wall(), clock.now_mono())
}

// ---- Phase 6, task 6.9 (P2 — the phase's closing condition), moved here by
// task 8.0b -------------------------------------------------------------
//
// interval-tracking spec.md "A full simulated day sums exactly": for a
// complete simulated day, `sum(active) + sum(afk) + sum(locked) +
// sum(paused) + sum(unknown)` must equal `end_of_day - start_of_day`
// exactly (PRD.md M-1). A test that only checks the grand total against
// the day's span would pass even if a bug shifted time from one state's
// bucket into an adjacent one (e.g. RF-23 closing at the wrong instant),
// because the closed/open chain's own telescoping sum is *structurally*
// always equal to `last_at - first_at`, independent of correctness at
// any interior boundary. So this test independently hand-derives the
// expected duration of every segment from the script below and asserts
// each state's bucket — not only the total — against that independent
// expectation.
mod p2_working_day_sum {
    use super::*;

    #[test]
    fn p2_full_simulated_day_sums_exactly() {
        let clock = FakeClock::new(WallTs::new(0));
        let mut tracker = Tracker::new();
        let mut all_effects = Vec::new();

        let win_a = window("editor", "notes.md", Some(1));
        let win_b = window("browser", "docs", Some(2));
        let win_c = window("terminal", "zsh", Some(3));

        // seg1 [0, 3_600) active(win_a) — startup.
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(win_a)),
            &clock,
        ));

        // seg2 [3_600, 5_100) active(win_b) — window change; ends early
        // because seg3's close is backdated by 300s (RF-4).
        clock.advance(Duration::from_secs(3_600));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(win_b.clone())),
            &clock,
        ));

        // seg3 [5_100, 6_000) afk — idle alarm positive transition, with a
        // NON-ZERO `idle_for` on purpose. RF-4 requires the close to be
        // backdated to `now - idle_for`, so the alarm firing at 5_400 after
        // 300s of inactivity ends seg2 at 5_100, not 5_400. A zero here
        // would leave the backdating path untested inside the invariant:
        // the grand total is conserved by any boundary shift, so only the
        // per-bucket assertions below can catch a backdating bug, and they
        // can only catch it if backdating actually happens.
        clock.advance(Duration::from_secs(1_800));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::UserIdle {
                idle_for: Duration::from_secs(300),
            },
            &clock,
        ));

        // seg4 [6_000, 13_200) active(win_b) — idle alarm negative transition.
        clock.advance(Duration::from_secs(600));
        all_effects.extend(send(&mut tracker, SourceEvent::UserActive, &clock));

        // seg5 [13_200, 15_000) locked — session lock.
        clock.advance(Duration::from_secs(7_200));
        all_effects.extend(send(&mut tracker, SourceEvent::SessionLocked, &clock));

        // seg6 [15_000, 15_005) unknown — session unlock.
        clock.advance(Duration::from_secs(1_800));
        all_effects.extend(send(&mut tracker, SourceEvent::SessionUnlocked, &clock));

        // seg7 [15_005, 18_605) active(win_b) — capture resumes.
        clock.advance(Duration::from_secs(5));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(win_b)),
            &clock,
        ));

        // seg8 [18_605, 20_405) paused.
        clock.advance(Duration::from_secs(3_600));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::Pause { until: None },
            &clock,
        ));

        // seg9 [20_405, 20_415) unknown — X11 connection loss (also the
        // path out of `paused`, since Resume is out of this phase's scope).
        clock.advance(Duration::from_secs(1_800));
        all_effects.extend(send(&mut tracker, SourceEvent::DisplayLost, &clock));

        // seg10 [20_415, 24_015) active(win_c) — X11 connection recovery.
        clock.advance(Duration::from_secs(10));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(win_c.clone())),
            &clock,
        ));

        // seg11 [24_015, 24_135) locked — suspend.
        clock.advance(Duration::from_secs(3_600));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::PrepareForSleep(true),
            &clock,
        ));

        // seg12 [24_135, 24_143) unknown — resume from suspend.
        clock.advance(Duration::from_secs(120));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::PrepareForSleep(false),
            &clock,
        ));

        // seg13 [24_143, 24_743) active(win_c) — capture resumes.
        clock.advance(Duration::from_secs(8));
        all_effects.extend(send(
            &mut tracker,
            SourceEvent::ActiveWindow(Some(win_c)),
            &clock,
        ));

        // Closes seg13 and the day.
        clock.advance(Duration::from_secs(600));
        all_effects.extend(send(&mut tracker, SourceEvent::Shutdown, &clock));

        let mut active_secs = 0u64;
        let mut afk_secs = 0u64;
        let mut locked_secs = 0u64;
        let mut paused_secs = 0u64;
        let mut unknown_secs = 0u64;
        let mut boundaries: Vec<WallTs> = Vec::new();
        let mut states: Vec<IntervalState> = Vec::new();

        for effect in &all_effects {
            match effect {
                Effect::OpenOnly { at, open } | Effect::Transition { at, open } => {
                    boundaries.push(*at);
                    states.push(open.state);
                }
                Effect::CloseOnly { at } => {
                    boundaries.push(*at);
                }
                Effect::Diagnostic(_) | Effect::ArmTimer(..) | Effect::CancelTimer(_) => {}
            }
        }

        assert_eq!(
            boundaries.len(),
            14,
            "expected 13 opened segments plus the closing shutdown boundary"
        );

        assert_eq!(
            states.len(),
            boundaries.len() - 1,
            "every boundary except the final shutdown close must have opened a state"
        );

        // `states[i]` is the state opened at `boundaries[i]`, i.e. the state
        // in effect over the span [boundaries[i], boundaries[i + 1]).
        for i in 0..boundaries.len() - 1 {
            let secs = duration_secs(boundaries[i], boundaries[i + 1]);
            match states[i] {
                IntervalState::Active => active_secs += secs,
                IntervalState::Afk => afk_secs += secs,
                IntervalState::Locked => locked_secs += secs,
                IntervalState::Paused => paused_secs += secs,
                IntervalState::Unknown => unknown_secs += secs,
            }
        }

        // Independently hand-derived from the script's own advances above —
        // not re-derived from `all_effects`, so a bug that shifts duration
        // from one state's bucket into an adjacent one is still caught.
        assert_eq!(active_secs, 20_100, "active bucket");
        assert_eq!(afk_secs, 900, "afk bucket");
        assert_eq!(locked_secs, 1_920, "locked bucket");
        assert_eq!(paused_secs, 1_800, "paused bucket");
        assert_eq!(unknown_secs, 23, "unknown bucket");

        let start_of_day = *boundaries.first().unwrap();
        let end_of_day = *boundaries.last().unwrap();
        let total = active_secs + afk_secs + locked_secs + paused_secs + unknown_secs;
        assert_eq!(
            total,
            duration_secs(start_of_day, end_of_day),
            "M-1: active + afk + locked + paused + unknown must equal end_of_day - start_of_day exactly"
        );
    }
}

// ---- Phase 6, task 6.8 (P1), moved here by task 8.0b -------------------
//
// interval-tracking spec.md "Randomized event sequences never produce
// overlapping intervals": for any sequence of events, no two resulting
// intervals overlap. Since every emitted interval boundary comes from
// `Effect::{OpenOnly, Transition, CloseOnly}.at`, and consecutive
// transitions chain end=start through one shared field by construction,
// "no overlap" reduces to "the sequence of emitted boundary instants is
// non-decreasing" — this also exercises `open_or_transition`'s general
// RF-28 clamp under randomly interleaved *backwards* wall-clock jumps,
// not only forward-advancing time.
mod p1_proptest {
    use super::*;
    use proptest::prelude::*;

    fn window_for(idx: u8) -> WindowInfo {
        match idx % 3 {
            0 => WindowInfo {
                app_id: "editor".to_string(),
                title: safe_title("notes"),
                pid: Some(1),
            },
            1 => WindowInfo {
                app_id: "browser".to_string(),
                title: safe_title("docs"),
                pid: Some(2),
            },
            _ => WindowInfo {
                app_id: "terminal".to_string(),
                title: safe_title("zsh"),
                pid: Some(3),
            },
        }
    }

    /// Decodes `(op, aux)` into one `SourceEvent`, covering every variant
    /// the tracker actually consumes so the generator can reach every
    /// branch of the transition table, not only the "happy path" rows.
    fn event_for(op: u8, aux: u8) -> SourceEvent {
        match op % 13 {
            0 => SourceEvent::ActiveWindow(Some(window_for(aux))),
            1 => SourceEvent::ActiveWindow(None),
            2 => SourceEvent::TitleChanged(safe_title(&format!("title-{}", aux % 4))),
            3 => SourceEvent::ActiveWindowDestroyed,
            4 => SourceEvent::UserIdle {
                idle_for: Duration::from_millis(u64::from(aux) * 100),
            },
            5 => SourceEvent::UserActive,
            6 => SourceEvent::SessionLocked,
            7 => SourceEvent::SessionUnlocked,
            8 => SourceEvent::PrepareForSleep(aux.is_multiple_of(2)),
            9 => SourceEvent::Pause { until: None },
            10 => SourceEvent::DisplayLost,
            11 => SourceEvent::DeadlineElapsed(Timer::TitleDebounce),
            _ => SourceEvent::DeadlineElapsed(Timer::DestroyGrace),
        }
    }

    proptest! {
        #[test]
        fn p1_no_overlap_for_arbitrary_event_sequences(
            ops in proptest::collection::vec(
                (0u16..500, 0u8..13, 0u8..8, -50i64..50),
                1..60,
            )
        ) {
            let clock = FakeClock::new(WallTs::new(1_000_000));
            let mut tracker = Tracker::new();
            let mut boundaries: Vec<i64> = Vec::new();

            for (advance_secs, op, aux, backward_jump) in ops {
                clock.advance(Duration::from_secs(u64::from(advance_secs)));
                if backward_jump < 0 {
                    // Adversarial input: an NTP-style backwards wall-clock
                    // jump, independent of the (always forward) mono clock.
                    clock.set_wall(WallTs::new(clock.now_wall().as_unix_secs() + backward_jump));
                }

                let effects = tracker.on_event(
                    event_for(op, aux),
                    clock.now_wall(),
                    clock.now_mono(),
                );
                for effect in effects {
                    match effect {
                        Effect::OpenOnly { at, .. }
                        | Effect::Transition { at, .. }
                        | Effect::CloseOnly { at } => boundaries.push(at.as_unix_secs()),
                        Effect::Diagnostic(_) | Effect::ArmTimer(..) | Effect::CancelTimer(_) => {}
                    }
                }
            }

            for pair in boundaries.windows(2) {
                prop_assert!(
                    pair[0] <= pair[1],
                    "intervals overlapped: boundary {} was followed by an earlier boundary {}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }
}

// ---- Task 8.1 (P3) ------------------------------------------------------
//
// privacy-filtering spec.md "Mixed excluded and non-excluded windows sum
// correctly": for a scripted sequence mixing excluded and non-excluded
// windows spanning total duration `D`, running the sequence once with
// exclusion rules active and once with every rule disabled must record the
// same total duration `D` in both cases. Only titles/app_ids may change;
// durations never do. The classic silent failure this guards against is
// "excluding" a window ending up discarding its interval entirely, which
// would break the working-day sum (M-1) for anyone with an excluded app.
mod p3_exclusion_preserves_time {
    use super::*;
    use proptest::prelude::*;

    /// Every RF-48 default category disabled: a true "no exclusion rules
    /// active at all" control. Using an empty config (`from_toml_str("")`)
    /// would NOT be a no-exclusion control, since the RF-48 defaults stay
    /// active even with no `config.toml` at all — that is the entire point
    /// of RF-48 — so `keepassxc` below would still be excluded in the
    /// "control" run too, and the comparison this test exists to make would
    /// be vacuous.
    fn no_exclusion_excluder() -> Excluder {
        Excluder::from_toml_str(
            r#"
            disable_default_excludes = [
                "password-managers", "banking-generic", "private-browsing",
                "gpg-ssh-prompts", "2fa-otp",
            ]
            "#,
        )
        .expect("valid literal config compiles")
    }

    /// `keepassxc` matches RF-48's password-managers default rule;
    /// `firefox` matches nothing. Alternating between them exercises both
    /// the excluded and non-excluded path within one scripted sequence, as
    /// the spec scenario requires ("mixing excluded and non-excluded
    /// windows").
    fn window_for(idx: u8) -> (&'static str, &'static str) {
        if idx.is_multiple_of(2) {
            ("keepassxc", "KeePassXC - vault.kdbx")
        } else {
            ("firefox", "GitHub - foo/bar")
        }
    }

    /// Runs `advances` through the real `Excluder::evaluate` -> `Tracker`
    /// boundary (§14.3's ordering) and returns both the total recorded
    /// duration and a per-`app_id` duration bucket map. The total is derived
    /// the same telescoping-sum way P1/P2 above do: the difference between
    /// the first and last emitted effect boundary. The per-app buckets are
    /// derived the same per-bucket way P2's own `active_secs`/`afk_secs`/...
    /// are: `boundaries[i]`'s opened `NewInterval.app` owns the span
    /// `[boundaries[i], boundaries[i + 1])`, for every boundary except the
    /// final `Shutdown` close (which opens nothing). A trailing `Shutdown`
    /// closes the final open interval so it is counted too.
    fn run_scripted(excluder: &Excluder, advances: &[(u16, u8)]) -> (u64, HashMap<String, u64>) {
        let clock = FakeClock::new(WallTs::new(1_000_000));
        let mut tracker = Tracker::new();
        let mut boundaries: Vec<WallTs> = Vec::new();
        let mut opened_apps: Vec<String> = Vec::new();

        for &(advance_secs, idx) in advances {
            clock.advance(Duration::from_secs(u64::from(advance_secs)));
            let (app_id, title) = window_for(idx);
            let evaluated = excluder.evaluate(app_id, RawTitle::new(title));

            let event = SourceEvent::ActiveWindow(Some(WindowInfo {
                app_id: evaluated.app_id,
                title: evaluated.title,
                pid: None,
            }));
            for effect in send(&mut tracker, event, &clock) {
                match effect {
                    Effect::OpenOnly { at, open } | Effect::Transition { at, open } => {
                        boundaries.push(at);
                        opened_apps.push(open.app);
                    }
                    Effect::CloseOnly { at } => boundaries.push(at),
                    Effect::Diagnostic(_) | Effect::ArmTimer(..) | Effect::CancelTimer(_) => {}
                }
            }
        }

        for effect in send(&mut tracker, SourceEvent::Shutdown, &clock) {
            if let Effect::CloseOnly { at } = effect {
                boundaries.push(at);
            }
        }

        if boundaries.len() < 2 {
            return (0, HashMap::new());
        }

        let total = duration_secs(*boundaries.first().unwrap(), *boundaries.last().unwrap());

        let mut buckets: HashMap<String, u64> = HashMap::new();
        for i in 0..boundaries.len() - 1 {
            let secs = duration_secs(boundaries[i], boundaries[i + 1]);
            *buckets.entry(opened_apps[i].clone()).or_insert(0) += secs;
        }

        (total, buckets)
    }

    proptest! {
        #[test]
        fn mixed_excluded_and_non_excluded_windows_sum_to_d_with_and_without_exclusion(
            advances in proptest::collection::vec((1u16..500, 0u8..2), 1..40),
        ) {
            let excluding = Excluder::from_toml_str("").expect("defaults compile");
            let not_excluding = no_exclusion_excluder();

            let (with_exclusion_total, with_exclusion_buckets) =
                run_scripted(&excluding, &advances);
            let (without_exclusion_total, without_exclusion_buckets) =
                run_scripted(&not_excluding, &advances);

            prop_assert_eq!(
                with_exclusion_total, without_exclusion_total,
                "excluding vs not-excluding must record the same total duration D"
            );

            // Per-app buckets, not only the total: `app_id` stays visible for
            // the password-managers default rule (`hide_app = false`), so
            // "keepassxc" is directly comparable between both runs. A bug
            // that folds an excluded window's time into the previous app
            // (privacy misattribution) shifts an interior boundary and is
            // conserved by the total above, but not by these per-app sums.
            let mut all_apps: Vec<&String> = with_exclusion_buckets
                .keys()
                .chain(without_exclusion_buckets.keys())
                .collect();
            all_apps.sort();
            all_apps.dedup();
            for app in all_apps {
                let with_secs = with_exclusion_buckets.get(app).copied().unwrap_or(0);
                let without_secs = without_exclusion_buckets.get(app).copied().unwrap_or(0);
                prop_assert_eq!(
                    with_secs, without_secs,
                    "app {:?} must record the same duration excluding vs not-excluding",
                    app
                );
            }
        }
    }
}
