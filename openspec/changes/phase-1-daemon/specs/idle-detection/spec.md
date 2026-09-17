# Idle Detection Specification

## Purpose

Detect when the user is away from the keyboard and mouse without polling,
and degrade gracefully — never blocking daemon startup — when the X11
extensions this relies on are unavailable.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-4 | Event-driven absence detection via SYNC/IDLETIME |
| RF-25 | Three-step absence-detection degradation chain |

## Requirements

### Requirement: Event-driven absence detection via SYNC/IDLETIME

**Traces:** RF-4

The system MUST detect user absence using an alarm registered against the
`SYNC` extension's `IDLETIME` system counter, firing a positive transition
when idle time exceeds `afk_threshold_seconds` (configurable, default 240),
and re-arming the alarm on the negative transition to detect the return of
activity. The system MUST query the exact `ms_since_user_input` value (via
`XScreenSaverQueryInfo`) only once, at the instant the alarm fires, solely to
compute the backdated closing timestamp of the interval that was active
during the idle period.

#### Scenario: User goes idle past the threshold

- GIVEN the daemon is tracking an `active` interval and
  `afk_threshold_seconds = 240`
- WHEN no keyboard or mouse input occurs for 240 seconds and the `IDLETIME`
  alarm fires its positive transition
- THEN the daemon queries `ms_since_user_input` exactly once at that instant
- AND the active interval is closed with `end = now() - ms_since_user_input`
  (backdated to when input actually stopped, not to when the alarm fired)
- AND an `afk` interval opens from that backdated instant

#### Scenario: User returns from idle

- GIVEN the daemon is tracking an `afk` interval
- WHEN the `IDLETIME` alarm's negative transition fires (input resumes)
- THEN the daemon closes the `afk` interval at the instant of the return
- AND the daemon opens a new interval for the currently active window if one
  still exists, or `unknown` if it no longer exists
- AND the alarm is re-armed for the next positive transition

#### Scenario: No polling while waiting for idle or activity

- GIVEN the daemon is tracking either an `active` or an `afk` interval
- WHEN no idle-related alarm fires
- THEN the daemon performs no periodic query of idle time; it only reads
  `ms_since_user_input` at the instant an alarm actually fires

### Requirement: Three-step absence-detection degradation chain

**Traces:** RF-25

The system MUST attempt absence detection in this order: (1) the `SYNC`
extension with the `IDLETIME` counter; (2) if unavailable, the
`MIT-SCREEN-SAVER` extension with a 30-second timer; (3) if that is also
unavailable, absence detection via X11 is disabled entirely, a warning is
logged exactly once at startup, and the daemon relies solely on `logind`
signals (`session-state`) for lock/suspend-based absence. None of these
three outcomes MUST block daemon startup.

#### Scenario: SYNC/IDLETIME available

- GIVEN the X server exposes the `SYNC` extension with an `IDLETIME` counter
- WHEN the daemon starts
- THEN the daemon uses SYNC/IDLETIME-based absence detection
- AND no degradation warning is logged

#### Scenario: SYNC unavailable, MIT-SCREEN-SAVER available

- GIVEN the X server does not expose the `SYNC` extension but does expose
  `MIT-SCREEN-SAVER`
- WHEN the daemon starts
- THEN the daemon uses a 30-second polling timer over `MIT-SCREEN-SAVER` for
  absence detection
- AND the daemon logs the degradation from SYNC to MIT-SCREEN-SAVER
- AND daemon startup completes successfully

#### Scenario: Neither extension available

- GIVEN the X server exposes neither `SYNC` nor `MIT-SCREEN-SAVER` (for
  example, a minimal Xvfb server with no extensions)
- WHEN the daemon starts
- THEN the daemon disables X11-based absence detection entirely
- AND the daemon logs exactly one warning at startup stating that X11-based
  absence detection is disabled and only `logind` signals will be used
- AND daemon startup completes successfully and window capture continues to
  function
