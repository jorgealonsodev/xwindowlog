# Interval Tracking Specification

## Purpose

Maintain a strictly contiguous, non-overlapping sequence of state intervals
(`active`, `afk`, `locked`, `paused`, `unknown`) from the events produced by
`window-capture`, `idle-detection`, and `session-state`, such that the sum
of all states across a day exactly equals the day's duration (PRD metric
M-1). This capability performs no I/O and consults no clock other than
through the values it is given, so that its behavior is fully testable with
synthetic event sequences and no X server, D-Bus connection, or database.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-3 | Strict interval contiguity; State transition table |
| RF-28 | Wall/monotonic clock discipline with backwards-jump clamping |
| P1 (PRD §15.5) | No overlap between intervals |
| P2 (PRD §15.5) | Working-day sum invariant |

## Requirements

### Requirement: Strict interval contiguity

**Traces:** RF-3

The system MUST guarantee that the `end` of a closed interval and the
`start` of the interval that replaces it are the exact same instant: no
gaps and no overlaps between consecutive intervals. A title change on the
same window MUST close and open an interval subject to the debounce defined
in `window-capture` (RF-30), using this same contiguity guarantee.

#### Scenario: Consecutive transitions share the same boundary instant

- GIVEN an open interval for window `A`
- WHEN a transition occurs to window `B` at instant `t`
- THEN the interval for `A` is closed with `end = t`
- AND the interval for `B` is opened with `start = t`

#### Scenario: Stable title change produces contiguous intervals

- GIVEN an open interval for a window with title `T1`
- WHEN the title changes to `T2` and `T2` remains stable past the RF-30
  debounce window, closing at instant `t`
- THEN the `T1` interval closes with `end = t` and the `T2` interval opens
  with `start = t`, with no gap between them

### Requirement: State transition table

**Traces:** RF-3 (implements the full §11.1 transition table as a whole; the
individual event sources feeding each row are specified in their own
capabilities: `idle-detection` for RF-4, `session-state` for RF-5,
`window-capture` for RF-6/RF-30, `daemon-lifecycle` for RF-33/RF-49)

The system MUST implement exactly the following transitions, driven by
events from `window-capture`, `idle-detection`, and `session-state`:

| Source state | Event | Target | Closing timestamp (`end`) |
|---|---|---|---|
| `unknown` (startup) | First valid active-window event | `active` | — |
| `active` | Stable title change | `active` (new interval) | instant of the title event |
| `active` | Active window change | `active` (another window) | instant of the event |
| `active` | Active window becomes none (desktop focused) | `active` with `"(desktop)"` sentinel | instant of the event |
| `active` | Idle alarm, positive transition | `afk` | backdated: `now() − ms_since_user_input` |
| `active`/`afk` | `LockedHint → true`, or `PrepareForSleep(true)` | `locked` | instant of the property change or signal |
| `active`/`afk` | Pause requested | `paused` | instant of the pause request |
| `locked` | `LockedHint → false`, or `PrepareForSleep(false)` | `unknown` | — |
| `afk` | Idle alarm, negative transition | `active` (same window if it still exists, otherwise `unknown`) | instant of the return |
| any | Loss of the X11 connection | `unknown` | instant the error is detected |
| any | Shutdown signal | process exit | instant of the signal |

The `start` of every new interval MUST equal the `end` of the interval it
replaces.

#### Scenario: Every row of the transition table is exercised

- GIVEN a scripted sequence of synthetic events covering every row of the
  table above
- WHEN each event is applied in turn to the tracker starting from `unknown`
- THEN the resulting sequence of intervals matches the table's target state
  and closing-timestamp rule for each transition, with no row producing an
  unexpected state or an incorrectly computed `end`

#### Scenario: Absence closing timestamp is backdated, not the alarm's firing time

- GIVEN an `active` interval and an idle alarm that fires its positive
  transition at wall time `t_alarm`, reporting `ms_since_user_input = 5000`
- WHEN the tracker processes this event
- THEN the `active` interval closes with `end = t_alarm − 5000ms`, not
  `t_alarm`

### Requirement: Wall/monotonic clock discipline with backwards-jump clamping

**Traces:** RF-28

The system MUST use the wall clock (UTC epoch) exclusively for values that
are persisted, and the monotonic clock exclusively for measuring durations
of internal timers. The system MUST NOT derive one clock's value from the
other, in either direction. When closing an interval, if the computed `end`
would be earlier than the interval's `start` (for example, because of a
backwards NTP jump), the system MUST clamp `end = start`, log a warning, and
MUST NOT produce a negative-duration interval.

#### Scenario: Backwards wall-clock jump during an open interval

- GIVEN an open interval with `start = t0`
- WHEN the wall clock jumps backwards and the interval is closed with a
  computed `end` earlier than `t0`
- THEN the interval is closed with `end = t0` (zero duration for that
  closure)
- AND a warning is logged
- AND no interval with `end < start` is ever produced

#### Scenario: Backdated absence close clamped by a backwards jump

- GIVEN an `active` interval with `start = t0`, and an idle alarm reporting
  an `ms_since_user_input` value that, when subtracted from the alarm's
  firing time, would compute an `end` earlier than `t0`
- WHEN the tracker processes this alarm
- THEN the closing timestamp is clamped to `end = t0`, not left negative
  relative to `start`

#### Scenario: Monotonic and wall clocks are never substitutable

- GIVEN any tracker computation that arms or measures a timer deadline
- WHEN that computation is inspected
- THEN it uses only the monotonic clock, and any computation that produces a
  persisted timestamp uses only the wall clock; no code path converts one
  into the other

### Requirement: No overlap between intervals (Property P1)

**Traces:** P1 (PRD §15.5)

For any sequence of events processed by the tracker, the system MUST
produce a set of intervals that never overlap in time.

#### Scenario: Randomized event sequences never produce overlapping intervals

- GIVEN an arbitrary, randomly generated sequence of valid tracker events
  and a starting clock value
- WHEN the tracker processes the entire sequence
- THEN no two resulting intervals overlap: for every pair of intervals,
  either one ends before or at the instant the other starts

### Requirement: Working-day sum invariant (Property P2)

**Traces:** P2 (PRD §15.5)

For a complete simulated day of events, the sum of the durations of all
`active`, `afk`, `locked`, `paused`, and `unknown` intervals MUST equal
exactly the duration between the first interval's `start` and the last
interval's `end`.

#### Scenario: A full simulated day sums exactly

- GIVEN a scripted, deterministic sequence of events spanning a complete
  simulated day (window changes, idle transitions, lock/unlock, suspend and
  resume, pause and resume, and at least one X11 connection loss and
  recovery)
- WHEN the tracker processes the entire day using an injected fake clock
- THEN `sum(active) + sum(afk) + sum(locked) + sum(paused) + sum(unknown)`
  equals exactly `end_of_day − start_of_day`, with zero discrepancy
