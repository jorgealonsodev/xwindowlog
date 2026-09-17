# CLI Reporting Specification

## Purpose

Give the user a standalone, AI-free path to their own data: `today` and
`status`, plus the operational subcommands (`daemon`, `pause`, `resume`,
`prune`, `forget`, `completions`), all sharing one predictable stdout/stderr
and exit-code contract so they can be scripted and embedded in status bars.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-19 (partial: daemon, status, today, pause, resume, prune, forget, completions) | Phase 1 subcommand surface; today reads only from the store |
| RF-49 (partial: status visibility of the paused state) | status-bar single-line format |
| RF-54 | status default privacy |
| RF-60 | stdout/stderr discipline and exit-code contract |
| RF-61 | Versioned --json output |
| RF-62 | Shell completions and man pages |
| RF-63 | status-bar single-line format |

## Requirements

### Requirement: Phase 1 subcommand surface (RF-19, partial)

**Traces:** RF-19 (partial: `daemon`, `status`, `today`, `pause`, `resume`,
`prune`, `forget`, `completions`)

The system MUST expose, at minimum, the following subcommands: `daemon`
(launched by systemd), `status`, `today`, `pause`, `resume`, `prune`,
`forget`, and `completions`. Subcommands out of Phase 1 scope (`mcp`,
`install`, `export`, `doctor`) are not required by this specification.

#### Scenario: All Phase 1 subcommands are recognized

- GIVEN the compiled binary
- WHEN each of `daemon`, `status`, `today`, `pause`, `resume`, `prune`,
  `forget`, and `completions` is invoked with `--help`
- THEN each produces a help message describing that subcommand, rather than
  an "unrecognized subcommand" error

### Requirement: today reads only from the store

**Traces:** RF-19 (partial: `today`); reflects the §14.3 sanitization
boundary, which has no independent RF number of its own

`xwindowlog today` MUST read exclusively from the persisted, already
sanitized store — never directly from X11 or from in-memory daemon state —
and MUST produce a frozen, documented output format with a literal example
committed to the README.

#### Scenario: today reflects only persisted, sanitized data

- GIVEN a database containing today's intervals, including at least one
  `[hidden]` interval from exclusion
- WHEN `xwindowlog today` is run
- THEN the output reflects exactly the persisted intervals (including the
  `[hidden]` title), and no data is sourced by querying X11 directly

#### Scenario: today's output format matches the documented example

- GIVEN the README's literal `today` output example
- WHEN `xwindowlog today` is run against an equivalent fixture database
- THEN the produced output matches the documented format

### Requirement: status default privacy (RF-54)

**Traces:** RF-54

`xwindowlog status` MUST default `status_show_title` to `false`, showing
only `app_id` and active time, never the window title, unless the user has
explicitly enabled title display in configuration.

#### Scenario: status hides the title by default

- GIVEN `status_show_title` is not set in config (default `false`) and the
  daemon has an active window with a real title
- WHEN `xwindowlog status` is run
- THEN the output shows the `app_id` and elapsed active time, and does not
  include the window title

#### Scenario: status shows the title when explicitly enabled

- GIVEN `status_show_title = true` is set in config
- WHEN `xwindowlog status` is run
- THEN the output includes the window title

### Requirement: status-bar single-line format (RF-63)

**Traces:** RF-63; RF-49 (partial: status visibility of the paused state)

`xwindowlog status` MUST, by default, produce a single line suitable for
status-bar embedding (for example:
`xwindowlog: Editor · project-x · 2h34m today`), and MUST support `--json`
for structured consumption. The README MUST document usage examples for at
least i3blocks, waybar, and polybar, without coupling the output format to
any one of them.

#### Scenario: Default status output is a single line

- GIVEN the daemon is running and tracking an active window
- WHEN `xwindowlog status` is run with no flags
- THEN the output is exactly one line

#### Scenario: status shows the paused state visibly

- GIVEN the daemon is currently paused (per `daemon-lifecycle`'s pause/resume
  requirement)
- WHEN `xwindowlog status` is run
- THEN the single-line output visibly indicates the paused state (for
  example, showing "paused" rather than an app name)

### Requirement: stdout/stderr discipline and exit-code contract (RF-60)

**Traces:** RF-60

Across every subcommand, the system MUST reserve stdout exclusively for
primary output; every log, warning, or progress message MUST go to stderr.
Exit codes MUST follow: `0` success; `1` generic or usage error; `2` state
error (for example: instance already running, database missing, already
paused, not paused, VACUUM exhausted its retries in `prune`); `3`
environment error (for example: no X11, no D-Bus).

#### Scenario: stdout carries only primary output

- GIVEN any subcommand that both produces primary output and logs a warning
  during its run
- WHEN that subcommand executes
- THEN the primary output appears on stdout and the warning appears on
  stderr, never intermixed on the same stream

#### Scenario: Success exits 0

- GIVEN a subcommand completes its intended action with no error
- WHEN it exits
- THEN its exit code is `0`

#### Scenario: A second daemon instance exits 2 (state error)

- GIVEN a first daemon instance is already running
- WHEN a second `xwindowlog daemon` invocation is attempted (per
  `daemon-lifecycle`'s single-instance requirement)
- THEN it exits with code `2`, not `1`

#### Scenario: pause against an already-paused daemon exits 2

- GIVEN the daemon is already in the `paused` state
- WHEN `xwindowlog pause` is run again
- THEN it exits with code `2` (state error), with a stderr message
  indicating the daemon is already paused

#### Scenario: resume against a non-paused daemon exits 2

- GIVEN the daemon is not currently paused
- WHEN `xwindowlog resume` is run
- THEN it exits with code `2` (state error), with a stderr message
  indicating the daemon is not paused

#### Scenario: prune exits 2 when VACUUM exhausts its retries

- GIVEN `prune`'s delete phase completes successfully but its `VACUUM` phase
  exhausts its bounded retry budget due to sustained contention
- WHEN `prune` finishes
- THEN it exits with code `2`, and stderr states that the deletes are
  durable but the file was not compacted, with actionable next steps

#### Scenario: Daemon startup with no reachable X11 exits 3

- GIVEN no X11 display is reachable and no fallback path applies
- WHEN `xwindowlog daemon` is started
- THEN it exits with code `3` (environment error)

### Requirement: Versioned --json output (RF-61)

**Traces:** RF-61

Subcommands that produce aggregated data and support `--json` MUST include
an explicit schema version field in their JSON output, and that field MUST
be asserted by an automated test — the format being merely stable is not
sufficient.

#### Scenario: --json output includes a schema version field

- GIVEN `xwindowlog status --json` or `xwindowlog today --json` is run
- WHEN the JSON output is parsed
- THEN it contains an explicit schema version field with a defined value

#### Scenario: Schema version field is asserted, not merely present by accident

- GIVEN a test asserting the shape of `--json` output
- WHEN that test runs against the current binary
- THEN it explicitly checks the schema version field's presence and value,
  failing if that field is removed or changed without a corresponding test
  update

### Requirement: Shell completions and man pages (RF-62)

**Traces:** RF-62

`xwindowlog completions <bash|zsh|fish>` MUST generate valid completion
scripts for each of bash, zsh, and fish via `clap_complete`. A man page MUST
be generated at build time via `clap_mangen`.

#### Scenario: Completions generate for all three shells

- GIVEN the compiled binary
- WHEN `xwindowlog completions bash`, `xwindowlog completions zsh`, and
  `xwindowlog completions fish` are each run
- THEN each produces non-empty, shell-appropriate completion script output
  on stdout with exit code `0`

#### Scenario: Generated completions are smoke-tested

- GIVEN the generated bash, zsh, and fish completion scripts
- WHEN each is loaded into its respective shell in a smoke test
- THEN the shell accepts the script without a syntax error

#### Scenario: Man page is generated at build time

- GIVEN a release build
- WHEN the build completes
- THEN a man page for the binary has been generated via `clap_mangen`
