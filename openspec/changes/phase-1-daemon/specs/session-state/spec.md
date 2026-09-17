# Session State Specification

## Purpose

Track session lock and suspend state through `org.freedesktop.login1`,
treating the observable session state — not a mere request to change it —
as the source of truth, and degrading gracefully when D-Bus/logind is
unavailable or restricted.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-5 | LockedHint as the source of truth for session lock |
| RF-26 | Session resolution by PID, not by environment |
| RF-27 | Suspend handling with a held delay inhibitor |
| RF-65 | Graceful degradation when logind is unavailable |

**Note:** the last requirement in this file has no direct PRD RF tag. It
generalizes RF-27's "Inhibit() refused" degradation to the broader case of
the D-Bus session bus being entirely unreachable at startup, which the PRD
does not name as a separate requirement. Reported to the coordinator as a
spec-only addition, not silently attributed to an RF it does not discharge.

## Requirements

### Requirement: LockedHint as the source of truth for session lock

**Traces:** RF-5

The system MUST distinguish a lock *request* from a lock *state*: the
`Lock()` signal is only a request to an external locker, which may be slow
or may never run. The system MUST treat the `LockedHint` property as the
sole source of truth, and MUST close the current interval and transition to
`locked` only when `LockedHint` becomes `true` — never merely upon receiving
`Lock()`.

#### Scenario: Lock() signal received but LockedHint has not yet changed

- GIVEN the daemon is tracking an `active` interval
- WHEN a `Lock()` signal is observed but `LockedHint` is still `false`
- THEN the daemon does not close the current interval and does not
  transition to `locked`

#### Scenario: LockedHint transitions to true

- GIVEN the daemon is tracking an `active` or `afk` interval
- WHEN `LockedHint` transitions from `false` to `true`
- THEN the daemon closes the current interval at the instant of that
  property change and opens a `locked` interval

#### Scenario: LockedHint transitions to false

- GIVEN the daemon is tracking a `locked` interval
- WHEN `LockedHint` transitions from `true` to `false`
- THEN the daemon closes the `locked` interval at the instant of that
  property change and opens an `unknown` interval, pending the next window
  or absence event

### Requirement: Session resolution by PID, not by environment

**Traces:** RF-26

The system MUST resolve its own login1 session object path using
`Manager.GetSessionByPID(<own PID>)`, and MUST NOT rely on reading
`$XDG_SESSION_ID` from the environment. If resolution fails, the system MUST
retry resolution later (bounded retries) rather than treating the failure as
fatal.

#### Scenario: Session resolves successfully at startup

- GIVEN a running logind session for the daemon's own process
- WHEN the daemon starts
- THEN the daemon resolves its session object path via
  `GetSessionByPID(<own PID>)` and does not read `$XDG_SESSION_ID`

#### Scenario: Session resolution fails and is retried

- GIVEN `GetSessionByPID` fails (for example, immediately after a session
  restart, before logind has registered the new session)
- WHEN the daemon starts or session resolution is otherwise triggered
- THEN the daemon logs the failure, continues operating without lock/suspend
  awareness in the meantime, and retries session resolution later rather
  than treating this as fatal

### Requirement: Suspend handling with a held delay inhibitor

**Traces:** RF-27

The system MUST take a `delay`-type sleep inhibitor at startup. On
`PrepareForSleep(true)`, the system MUST close the currently tracked
interval, commit that closure, and release the inhibitor descriptor so the
suspend may proceed. On `PrepareForSleep(false)` (resume), the system MUST
open an `unknown` interval and re-acquire the inhibitor. If acquiring the
inhibitor fails (for example, a restrictive polkit policy), the system MUST
log a warning, continue on a best-effort basis, and MUST NOT block daemon
startup.

#### Scenario: System suspends with the inhibitor held

- GIVEN the daemon holds a sleep delay inhibitor and is tracking an `active`
  interval
- WHEN `PrepareForSleep(true)` is received
- THEN the daemon closes the current interval and commits that closure
  before releasing the inhibitor
- AND the interval closed for this reason is recorded as `locked`

#### Scenario: System resumes from suspend

- GIVEN the daemon has released its inhibitor and the system is suspended
- WHEN `PrepareForSleep(false)` is received
- THEN the daemon opens an `unknown` interval
- AND the daemon re-acquires the sleep delay inhibitor

#### Scenario: Inhibitor acquisition is refused by policy

- GIVEN a polkit policy refuses the daemon's `Inhibit()` call
- WHEN the daemon starts
- THEN the daemon logs a warning describing the refusal
- AND the daemon continues starting and capturing normally, with the
  documented caveat that suspend-time closure is now best-effort rather than
  guaranteed

### Requirement: Graceful degradation when logind is unavailable

**Traces:** RF-65

The system MUST start and continue functioning (window capture, exclusion,
storage) even when D-Bus or `org.freedesktop.login1` is entirely
unreachable at startup. Session-state features MUST be unavailable in that
case, not a fatal condition.

#### Scenario: D-Bus session bus is unreachable at startup

- GIVEN no D-Bus session bus is reachable when the daemon starts
- WHEN the daemon starts
- THEN the daemon logs a warning that session-state tracking (lock/suspend)
  is unavailable
- AND the daemon continues to start and to capture window activity normally
