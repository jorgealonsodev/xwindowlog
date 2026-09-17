# Window Capture Specification

## Purpose

Acquire the identity of the active X11 window (application, title, PID) as it
changes over time, without polling, and without silently failing when the
window manager, the window itself, or the display connection misbehaves.
This capability is the daemon's only source of raw window data; everything it
emits crosses into `privacy-filtering` before any other module may see it
(§14.3).

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-1 | Event-driven active-window subscription; Unconditional property read after a change |
| RF-2 | Window metadata capture |
| RF-6 | X11 connection loss and reconnection |
| RF-22 | Property-read/event-mask race handling |
| RF-23 | Destruction safety net |
| RF-24 | EWMH compliance verification |
| RF-29 | XWayland session detection |
| RF-30 | Title debounce |
| RF-31 | Title decoding, truncation, and app-id fallback |
| RF-32 | X11 connection loss and reconnection |

## Requirements

### Requirement: Event-driven active-window subscription

**Traces:** RF-1

The system MUST subscribe to `PropertyNotify` on `_NET_ACTIVE_WINDOW` on the
root window, and MUST subscribe to `PropertyChangeMask` and
`StructureNotifyMask` on the currently active window. The system MUST NOT
poll for window changes: it MUST remain blocked until X11 delivers a
notification or a deadline it has itself armed elapses.

#### Scenario: Active window changes while idle

- GIVEN the daemon is running with no armed deadlines and no pending events
- WHEN the active window changes on the X11 server
- THEN the daemon wakes exactly once for that change and processes it
- AND the daemon does not perform any window-state query before that wakeup

#### Scenario: No busy-waiting between changes

- GIVEN the daemon is running and no window or session-state event occurs
- WHEN 60 seconds elapse with no X11 events, no logind signals, and no armed
  deadline
- THEN the daemon issues zero read attempts against the X11 connection
  during that window

### Requirement: Unconditional property read after a change

**Traces:** RF-1

After every active-window change, the system MUST perform an unconditional
read of `_NET_WM_NAME`/`WM_NAME`, `_NET_WM_PID`, and `WM_CLASS` for the new
window, rather than relying solely on the event that announced the change.

#### Scenario: Property read follows every active-window change

- GIVEN a window becomes the active window
- WHEN the daemon processes `_NET_ACTIVE_WINDOW`'s change
- THEN the daemon issues a fresh read of title, PID, and `WM_CLASS` for that
  window rather than reusing any previously cached values

### Requirement: Window metadata capture

**Traces:** RF-2

For each active-window change, the system MUST make available the
application identifier (`WM_CLASS`, second component), the window title, the
PID if present (`_NET_WM_PID`), and the instant of the change, for
consumption by interval tracking (`interval-tracking`).

#### Scenario: Metadata captured for a normal window

- GIVEN a window with `WM_CLASS = "firefox\0Firefox"`, title
  `"GitHub - foo/bar"`, and `_NET_WM_PID = 4821` becomes active
- WHEN the daemon captures the change
- THEN the captured `app_id` is `"Firefox"`, the title is
  `"GitHub - foo/bar"`, and the PID is `4821`

#### Scenario: Desktop focus is legitimate activity

- GIVEN `_NET_ACTIVE_WINDOW` becomes unset (no window focused, desktop
  focused)
- WHEN the daemon processes this change
- THEN the daemon treats it as legitimate user activity, records it with the
  `"(desktop)"` sentinel application identifier, and does not discard it or
  treat it as absence

### Requirement: Property-read/event-mask race handling

**Traces:** RF-22

On every active-window change, the system MUST perform, in order:
`GetProperty(_NET_ACTIVE_WINDOW)`, then
`ChangeWindowAttributes(window, PROPERTY_CHANGE | STRUCTURE_NOTIFY).check()`.
If that check reports the window no longer exists (a "BadWindow" condition),
the system MUST treat this as a valid transition, not a daemon failure, and
MUST await the next `_NET_ACTIVE_WINDOW` change without emitting an error.

#### Scenario: Active window is destroyed before its event mask is set

- GIVEN the active window changes to a window `W`
- WHEN the daemon reads `_NET_ACTIVE_WINDOW` and then finds, while
  registering for property/structure events on `W`, that `W` no longer
  exists
- THEN the daemon does not report an error or a diagnostic
- AND the daemon does not emit a tracking event for `W`
- AND the daemon continues waiting for the next `_NET_ACTIVE_WINDOW` change

#### Scenario: Title changes between the property read and the event mask being active

- GIVEN the active window's title changes at the same time its event mask is
  being registered, so that the title-change event is lost
- WHEN the daemon completes registering the event mask
- THEN the daemon's next unconditional property read (per the requirement
  above) reflects the current title rather than a stale one

### Requirement: Destruction safety net

**Traces:** RF-23

If `DestroyNotify` arrives for the currently tracked active window, the
system MUST arm a one-shot 250 ms deadline. If a new `_NET_ACTIVE_WINDOW`
change arrives before that deadline elapses, the system MUST cancel the
deadline and take no further action for the destroyed window. If the
deadline elapses with no new active window announced, the system MUST close
the interval with its `end` set to the timestamp of the original
`DestroyNotify` event (not the timestamp the deadline fired) and transition
to the `unknown` state.

#### Scenario: Window manager updates the active window promptly after destruction

- GIVEN `DestroyNotify` arrives for the active window `W` at time `t`
- WHEN a new `_NET_ACTIVE_WINDOW` change to a different window arrives at
  `t + 100ms` (before the 250 ms grace period elapses)
- THEN the daemon cancels the destruction deadline
- AND no `unknown`-state transition is recorded for the gap between `t` and
  `t + 100ms`

#### Scenario: Active window is killed under a window manager that never updates the property

- GIVEN `DestroyNotify` arrives for the active window `W` at time `t`
- WHEN no `_NET_ACTIVE_WINDOW` change arrives within 250 ms of `t`
- THEN the daemon closes `W`'s interval with `end = t` (the `DestroyNotify`
  timestamp), not the time the 250 ms deadline elapsed
- AND the daemon transitions to the `unknown` state

### Requirement: EWMH compliance verification

**Traces:** RF-24

At startup, the system MUST verify, using `intern_atom(only_if_exists =
true)`, that `_NET_SUPPORTED` and `_NET_ACTIVE_WINDOW` exist. If either is
missing, the system MUST emit an explicit diagnostic on stderr stating that
the window manager does not appear to be EWMH-compliant, and MUST degrade to
using `GetInputFocus` as an approximation of the active window rather than
silently waiting for events that will never arrive.

#### Scenario: Window manager is EWMH-compliant

- GIVEN `_NET_SUPPORTED` and `_NET_ACTIVE_WINDOW` both exist at startup
- WHEN the daemon performs its startup verification
- THEN the daemon proceeds using `_NET_ACTIVE_WINDOW` subscription with no
  diagnostic emitted about EWMH compliance

#### Scenario: Window manager does not expose EWMH properties

- GIVEN a window manager that never creates `_NET_ACTIVE_WINDOW` (for
  example, a minimal tiling window manager with no EWMH support)
- WHEN the daemon performs its startup verification
- THEN the daemon emits an explicit diagnostic on stderr identifying the
  missing EWMH support
- AND the daemon falls back to tracking the focused window via
  `GetInputFocus`
- AND the daemon does not sit idle waiting for events that will never arrive

### Requirement: XWayland session detection

**Traces:** RF-29

At startup, if `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE=wayland` is present
alongside `DISPLAY`, the system MUST emit a warning that absence detection
reliability is reduced under XWayland, and MUST continue execution.

#### Scenario: Daemon starts inside an XWayland session

- GIVEN the environment has both `DISPLAY` and `WAYLAND_DISPLAY` set
- WHEN the daemon starts
- THEN the daemon emits a warning about reduced absence-detection
  reliability under XWayland
- AND the daemon continues starting and capturing normally

#### Scenario: Daemon starts under native X11

- GIVEN the environment has `DISPLAY` set and neither `WAYLAND_DISPLAY` nor
  `XDG_SESSION_TYPE=wayland`
- WHEN the daemon starts
- THEN no XWayland warning is emitted

### Requirement: Title debounce

**Traces:** RF-30

The system MUST apply a configurable debounce (`title_debounce_ms`, default
2000) to title changes on the same window: a title change MUST close and
open an interval only if the new title remains stable for the configured
duration. A subsequent title change, or any other event affecting the
tracked window, that arrives before the debounce elapses MUST cancel the
pending transition.

#### Scenario: Title change is stable past the debounce window

- GIVEN the active window's title changes at `t`
- WHEN no further title change occurs before `t + title_debounce_ms`
- THEN a transition is recorded with the new title at
  `t + title_debounce_ms`

#### Scenario: Title flickers within the debounce window

- GIVEN the active window's title changes at `t`, then changes again at
  `t + 300ms` (before the default 2000 ms debounce elapses)
- WHEN the daemon processes the second change
- THEN the daemon discards the first pending title and restarts the debounce
  timer from `t + 300ms`
- AND no transition is recorded for the title present between `t` and
  `t + 300ms`

### Requirement: Title decoding, truncation, and app-id fallback

**Traces:** RF-31

The system MUST decode window titles according to the atom type returned by
the server (`STRING` versus `UTF8_STRING`) rather than always assuming
UTF-8. The system MUST truncate stored titles to 512 characters, appending
an ellipsis when truncation occurs. If `WM_CLASS` is absent but
`_NET_WM_PID` is present, the system MUST use `/proc/<pid>/comm` as a
best-effort `app_id` before falling back to the `"?"` sentinel.

#### Scenario: Title exceeds the maximum length

- GIVEN a window title of 600 characters
- WHEN the daemon captures the title
- THEN the stored title is truncated to 512 characters with a trailing
  ellipsis

#### Scenario: WM_CLASS absent, PID present

- GIVEN a window with no `WM_CLASS` property but `_NET_WM_PID = 9001`, whose
  `/proc/9001/comm` contains `"myapp"`
- WHEN the daemon captures the window
- THEN the captured `app_id` is `"myapp"`

#### Scenario: WM_CLASS and PID both absent, or comm read fails

- GIVEN a window with no `WM_CLASS` property and no readable
  `/proc/<pid>/comm` (missing PID, exited process, or unreadable file)
- WHEN the daemon captures the window
- THEN the captured `app_id` is the `"?"` sentinel
- AND capture continues without error

### Requirement: X11 connection loss and reconnection

**Traces:** RF-6, RF-32

If the X11 connection is lost, or if it cannot be established at startup,
the system MUST retry with exponential backoff: 500 ms, 1 s, 2 s, 4 s, 8 s,
then a 16 s ceiling, with jitter of ±20%, retrying indefinitely for the
life of the daemon. The system MUST record the outage as the `unknown`
state and MUST NOT open a new `unknown` interval for each failed retry
attempt during a single outage.

#### Scenario: X11 connection lost mid-run

- GIVEN the daemon is running and tracking an `active` interval
- WHEN the X11 connection is lost
- THEN the daemon closes the current interval and opens exactly one
  `unknown` interval
- AND the daemon begins retrying reconnection starting at a 500 ms delay

#### Scenario: Repeated reconnection failures during one outage

- GIVEN the daemon is in an `unknown` state after losing the X11 connection
- WHEN three consecutive reconnection attempts fail
- THEN no additional `unknown` intervals are opened for those failed
  attempts; the single `unknown` interval opened at the start of the outage
  remains the only one

#### Scenario: Reconnection succeeds

- GIVEN the daemon is retrying reconnection after an outage
- WHEN a reconnection attempt succeeds
- THEN the daemon resumes normal active-window capture
- AND the backoff sequence resets, so a future outage again starts its
  retry delay at 500 ms
