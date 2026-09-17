# Daemon Lifecycle Specification

## Purpose

Compose window capture, idle detection, session state, tracking, exclusion,
and storage into one resident process that starts fast, stays within a
small resource envelope, never runs two instances at once, shuts down
cleanly, opens no network, and can be paused and resumed from a separate CLI
invocation without racing its own single-writer guarantee on `intervals`.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-20 | Hardened, session-scoped systemd unit |
| RF-21 | Single instance via flock |
| RF-33 | Clean shutdown on SIGTERM/SIGINT |
| RF-34 | Single instance via flock |
| RF-49 | pause/resume control channel |
| RNF-1 | Resident memory budget |
| RNF-2 | No busy-waiting when idle; CPU budget under active use |
| RNF-3 | Binary size baseline and regression gate |
| RNF-4 | Startup latency |
| RNF-5 | No network access at runtime |
| RNF-11 | Explicit MSRV |

## Requirements

### Requirement: No busy-waiting when idle (RNF-2)

**Traces:** RNF-2

The system's primary event-processing thread MUST NOT wake up unless a
monitored event source has data ready to read or an armed deadline has
elapsed. This requirement is defined by observable behavior — the number of
times the primary event loop resumes execution — not by which underlying
mechanism (file-descriptor multiplexing, a bridge thread, or otherwise)
produces that behavior. A design that requires periodic re-checking to
compensate for a source it cannot wait on directly fails this requirement,
regardless of the reason.

#### Scenario: Zero wakeups over an idle window

- GIVEN the daemon is running with no pending X11 events, no logind
  signals, and no armed deadline
- WHEN 60 seconds elapse
- THEN the primary event-processing thread records zero wakeups over that
  window, measured by an in-process counter incremented each time the
  underlying wait call returns

#### Scenario: A wakeup is attributable to a real event or a real deadline

- GIVEN the daemon's primary event-processing thread wakes up at some
  instant
- WHEN the cause of that wakeup is inspected
- THEN it is always attributable to either a monitored source having data
  ready, or a deadline the daemon itself armed having elapsed — never to an
  unconditional periodic re-check

### Requirement: Resident memory budget (RNF-1)

**Traces:** RNF-1

The system's resident memory (measured as `RssAnon`) SHOULD be below 5 MB
after 8 hours of continuous operation, and MUST NOT exceed 8 MB, measured
via an accelerated soak test. The measured figure MUST be recorded as a
concrete number for each build, not asserted qualitatively.

#### Scenario: Accelerated soak stays within the hard budget

- GIVEN an accelerated soak test that simulates 8 hours of typical activity
  in compressed wall-clock time
- WHEN the soak completes
- THEN the daemon's `RssAnon` at the end of the soak is recorded as a
  numeric value and is below 8 MB

### Requirement: CPU budget under active use (RNF-2, CPU component)

**Traces:** RNF-2

Average CPU consumption during a representative period of active window
switching and idle transitions MUST stay below 0.5%.

#### Scenario: Average CPU stays under budget during representative activity

- GIVEN a representative local benchmark simulating typical window-switching
  and idle-transition activity over a bounded period
- WHEN the benchmark completes
- THEN the measured average CPU consumption over that period is below 0.5%,
  recorded as a numeric value

### Requirement: Binary size baseline and regression gate (RNF-3)

**Traces:** RNF-3

Because this phase's binary links no `tokio`/`rmcp` (unlike the eventual
multi-phase binary this PRD ultimately targets), the system MUST record the
compiled release binary's size as an explicit, numeric CI baseline on first
measurement, and subsequent builds MUST NOT regress beyond a threshold
derived from that baseline.

#### Scenario: First measurement establishes the baseline

- GIVEN no baseline binary size has yet been recorded for this phase
- WHEN the release binary is built and measured for the first time
- THEN its size in bytes is recorded as the baseline for future comparisons

#### Scenario: A later build regresses beyond the allowed threshold

- GIVEN a recorded baseline binary size
- WHEN a subsequent release build's binary size exceeds the baseline by more
  than the configured allowed margin
- THEN the CI check fails, reporting both the baseline and the measured size

### Requirement: Startup latency (RNF-4)

**Traces:** RNF-4

Daemon startup, from process launch to being ready to capture, MUST
complete in under 50 ms, measured with a benchmarking tool that performs
multiple runs and reports a stable statistic (not a single sample).

#### Scenario: Cold startup completes within budget

- GIVEN a typical local environment with an EWMH-compliant window manager
  and a reachable D-Bus session bus
- WHEN daemon startup is measured across multiple runs
- THEN the reported startup time is under 50 ms

### Requirement: No network access at runtime (RNF-5)

**Traces:** RNF-5

The system MUST open no network connections at any point during normal
operation. This MUST be verified by an automated runtime check, not merely
argued from the dependency graph: after the daemon has been running for at
least 60 seconds under normal conditions, every entry in `/proc/self/fd`
MUST be a Unix domain socket, a pipe, or a regular file — never an
`AF_INET`/`AF_INET6` socket. This behavior MUST also be materialized by the
systemd unit via `RestrictAddressFamilies=AF_UNIX` and `IPAddressDeny=any`.

#### Scenario: No network-family file descriptors after 60 seconds of operation

- GIVEN the daemon has been running normally for at least 60 seconds
- WHEN `/proc/self/fd` is enumerated and each entry's socket family (where
  applicable) is inspected
- THEN every entry is a Unix domain socket, a pipe, or a regular file; none
  is an `AF_INET` or `AF_INET6` socket

#### Scenario: The systemd unit restricts network address families

- GIVEN the shipped `contrib/xwindowlog.service` unit
- WHEN its `[Service]` directives are inspected
- THEN `RestrictAddressFamilies=AF_UNIX` and `IPAddressDeny=any` are both
  present

### Requirement: Explicit MSRV (RNF-11)

**Traces:** RNF-11

The system MUST declare a specific minimum supported Rust version, and CI
MUST verify the crate builds and its tests pass on exactly that pinned
version. Any change to the pinned version MUST be documented in the
CHANGELOG.

#### Scenario: CI builds against the pinned MSRV

- GIVEN a declared MSRV in the crate manifest
- WHEN CI runs the dedicated MSRV job
- THEN the crate builds and `cargo test` passes using exactly that pinned
  Rust version

#### Scenario: MSRV changes are documented

- GIVEN a change to the pinned MSRV value
- WHEN that change is reviewed
- THEN a corresponding CHANGELOG entry describing the bump is present

### Requirement: Hardened, session-scoped systemd unit (RF-20, RF-21)

**Traces:** RF-20

The system MUST ship a `systemd --user` unit with both `After=` and
`PartOf=graphical-session.target`, so the daemon starts after the graphical
session and stops when the graphical session ends (rather than being
orphaned until systemd's shutdown timeout). The unit MUST enforce single
instancing consistent with the flock-based mechanism below.

#### Scenario: Daemon stops when the graphical session ends

- GIVEN the daemon is running under its `systemd --user` unit within a
  graphical session
- WHEN the graphical session target stops (for example, on logout)
- THEN the daemon's unit is stopped as part of that same transaction,
  without waiting for a separate timeout

#### Scenario: Unit declares both ordering and grouping directives

- GIVEN the shipped `contrib/xwindowlog.service` unit
- WHEN its `[Unit]` section is inspected
- THEN both `After=graphical-session.target` and
  `PartOf=graphical-session.target` are present

### Requirement: Single instance via flock

**Traces:** RF-21, RF-34

The system MUST acquire `flock(2)` with `LOCK_EX | LOCK_NB` on
`$XDG_RUNTIME_DIR/xwindowlog.lock` before proceeding past startup. The
system MUST NOT determine "another instance is running" by checking file
existence or comparing a written PID against `/proc/<pid>`. If the lock
cannot be acquired (`EWOULDBLOCK`), the system MUST exit with a non-zero
status and a clear message, without retrying and without attempting to
terminate the other instance.

#### Scenario: Second instance refuses to start

- GIVEN a first daemon instance holds the flock on the lock file
- WHEN a second instance is launched
- THEN the second instance exits with a non-zero status and a message
  stating another instance is already running
- AND the second instance does not attempt to kill or interrupt the first

#### Scenario: A crashed instance's lock is released automatically

- GIVEN a previous daemon instance was terminated with `SIGKILL` while
  holding the flock
- WHEN a new instance is launched
- THEN the new instance successfully acquires the flock (the kernel already
  released it when the previous process died) and starts normally

### Requirement: Clean shutdown on SIGTERM/SIGINT (RF-33)

**Traces:** RF-33

On receiving `SIGTERM` or `SIGINT`, the system MUST close the currently open
interval with `end = now()` in whatever state it was in, commit that
closure, release the lock file and any held session inhibitor, and exit
with status code `0`.

#### Scenario: SIGTERM closes the open interval and exits cleanly

- GIVEN the daemon is tracking an open `active` interval
- WHEN the daemon receives `SIGTERM`
- THEN the open interval is closed with `end` = the instant the signal was
  received, the closure is committed, the lock file is released, and the
  process exits with status `0`

#### Scenario: SIGINT behaves identically to SIGTERM

- GIVEN the daemon is tracking an open interval
- WHEN the daemon receives `SIGINT`
- THEN the same closure, commit, release, and exit-code-0 behavior as the
  SIGTERM scenario occurs

### Requirement: pause/resume control channel (RF-49)

**Traces:** RF-49

`xwindowlog pause [--minutes N]` and `xwindowlog resume` MUST communicate
with the running daemon over a Unix `SOCK_STREAM` socket located in
`$XDG_RUNTIME_DIR` (the same directory already used for the lock file, so no
new trust boundary is introduced). The daemon MUST verify the connecting
peer's credentials (matching its own effective UID) before reading any data
from the connection, and MUST close the connection without reading if the
peer's UID does not match. The `pause`/`resume` client processes MUST NEVER
write to `intervals` themselves; only the daemon writes intervals. While
paused, the system MUST NOT open `active` intervals; it MUST record a
`paused` interval so the working day continues to sum correctly.
`status` (see `cli-reporting`) MUST visibly indicate the paused state.
`--minutes N`, when given, MUST cause the pause to expire automatically
after that many minutes, using both the monotonic deadline and a
wall-clock target, so that a system suspend during the pause does not
silently extend it beyond the requested real-world duration.

#### Scenario: pause opens a paused interval, not an active one

- GIVEN the daemon is tracking an `active` interval
- WHEN `xwindowlog pause` is run and successfully reaches the daemon
- THEN the current interval is closed and a `paused` interval is opened
- AND no `active` interval opens again until `resume` is run

#### Scenario: A connection from a different UID is rejected without being read

- GIVEN the control socket is listening
- WHEN a process running as a different UID than the daemon connects
- THEN the daemon closes the connection without reading any bytes from it,
  and logs the rejection

#### Scenario: pause --minutes expires automatically

- GIVEN `xwindowlog pause --minutes 30` succeeds
- WHEN 30 minutes elapse with no `resume` command issued
- THEN the daemon automatically ends the pause and resumes normal tracking,
  closing the `paused` interval

#### Scenario: A pause spanning a system suspend does not silently extend

- GIVEN `xwindowlog pause --minutes 30` succeeds
- WHEN the system suspends for 20 minutes during that pause and then resumes
- THEN the pause still ends at approximately 30 minutes of real elapsed
  time, not 50 minutes (the frozen monotonic clock during suspend does not
  extend the wall-clock-anchored expiry)

#### Scenario: resume ends an active pause

- GIVEN the daemon is in the `paused` state
- WHEN `xwindowlog resume` is run
- THEN the `paused` interval is closed and normal tracking resumes,
  transitioning to `active` or `unknown` depending on current window state

#### Scenario: pause/resume clients never write intervals directly

- GIVEN the `pause` and `resume` CLI processes
- WHEN their implementation is inspected for any path that writes to the
  `intervals` table
- THEN no such path exists; both commands only send a request over the
  control socket and read the daemon's response

#### Scenario: pause fails cleanly when the daemon is not running

- GIVEN no daemon instance is running (the control socket does not exist or
  refuses connections)
- WHEN `xwindowlog pause` is run
- THEN the command reports that the daemon is not running and exits
  non-zero, rather than hanging or silently succeeding
