# Interval Storage Specification

## Purpose

Persist tracked intervals to SQLite durably enough to survive a crash
without corruption, atomically enough that a state transition never leaves
two open intervals, and with file permissions that keep the database
private to the user. Provide retention (`prune`) and selective deletion
(`forget`) without ever pruning while the daemon itself is live-writing.

## Traceability

| RF/RNF | Covered by |
|---|---|
| RF-10 | Restrictive file permissions on all database artifacts |
| RF-11 | Schema with enforced invariants |
| RF-12 | Atomic single-transaction state transitions |
| RF-13 | Retention pruning |
| RF-35 | Forward-only migrations with pre-migration backup |
| RF-36 | Startup recovery from an unclean shutdown |
| RF-52 | Retention pruning (`retention_days` default) |
| RF-53 | Selective deletion (forget) |
| RNF-6 | Consistency guaranteed on power loss; durability of the most recent transactions is not |
| RF-66 | Interval clipping for range queries |

**Note:** "Interval clipping for range queries" has no direct PRD RF number.
It implements the unnumbered "Aggregations: interval clipping" subsection of
PRD §11.3, which underlies `today` (RF-19, partial — see `cli-reporting`)
in this phase and `summary` (RF-15, Phase 2/out of scope) later. Reported to
the coordinator as a spec-only addition rather than silently tagged to an RF
that does not name it.

## Requirements

### Requirement: Restrictive file permissions on all database artifacts

**Traces:** RF-10

The system MUST call `umask(0o077)` before opening any database connection,
so that `xwindowlog.db`, `xwindowlog.db-wal`, and `xwindowlog.db-shm` are all
created with mode `0600` regardless of the ambient process umask. Data and
configuration directories MUST be `0700`. The lock file MUST be `0600`.

#### Scenario: WAL and SHM files inherit restrictive permissions

- GIVEN the process umask is the common system default `022`
- WHEN the daemon opens the database for the first time in WAL mode
- THEN `xwindowlog.db`, `xwindowlog.db-wal`, and `xwindowlog.db-shm` are all
  created with mode `0600`

#### Scenario: Data and config directories are created with 0700

- GIVEN the data and config directories do not yet exist
- WHEN the daemon starts for the first time
- THEN the created data directory and config directory both have mode
  `0700`

### Requirement: Schema with enforced invariants

**Traces:** RF-11

The system MUST create a schema in which: `intervals.state` is constrained
to one of `active`, `afk`, `locked`, `unknown`, `paused`; `intervals.app` and
`intervals.title` are `NOT NULL` foreign keys into `apps`/`titles`; a
generated `open_marker` column and a unique index (`idx_intervals_one_open`)
together guarantee that at most one row in the entire `intervals` table has
`"end" IS NULL` at any time; and `"end" IS NULL OR "end" >= start` is
enforced as a table-level check.

#### Scenario: A second open interval is rejected

- GIVEN one interval already exists with `"end" IS NULL`
- WHEN a second interval is inserted with `"end" IS NULL`
- THEN the insert fails with a uniqueness constraint violation on
  `idx_intervals_one_open`

#### Scenario: An invalid state value is rejected

- GIVEN an attempt to insert an interval with `state = 'activ'` (a typo)
- WHEN the insert is executed
- THEN it fails a `CHECK` constraint and no row is written

#### Scenario: A negative-duration row is rejected at the schema level

- GIVEN an attempt to insert an interval with `end < start`
- WHEN the insert is executed
- THEN it fails the `CHECK ("end" IS NULL OR "end" >= start)` constraint

#### Scenario: An interval referencing a nonexistent app is rejected

- GIVEN foreign keys are enabled on the connection
- WHEN an interval is inserted referencing an `apps.id` that does not exist
- THEN the insert fails a foreign-key constraint violation

### Requirement: Atomic single-transaction state transitions

**Traces:** RF-12

Every state transition MUST be written as exactly one transaction that
closes the currently open interval (if any) and opens the next interval,
setting the closed interval's `end` and the new interval's `start` to the
identical instant. The system MUST NOT defer, batch, or buffer closes
separately from opens; no interval is ever left open across more than one
transaction boundary except across daemon restarts (see startup recovery,
below).

#### Scenario: A transition writes exactly one transaction

- GIVEN an open interval for window `A`
- WHEN the tracker emits a transition to window `B`
- THEN the daemon commits a single transaction that both closes `A`'s
  interval and opens `B`'s interval, with both timestamps identical

#### Scenario: Closing before opening within the transaction avoids a false collision

- GIVEN an open interval exists
- WHEN a transition transaction is applied
- THEN the close (`UPDATE`) of the existing open interval is applied before
  the insert (`INSERT`) of the new open interval within the same
  transaction, so the unique-open-interval constraint is never violated by
  the transition itself

#### Scenario: No interval is ever left open across two application-level writes for the same transition

- GIVEN the daemon is processing a single tracker-emitted transition
- WHEN that transition's transaction is applied
- THEN no other process or code path writes to `intervals` for that same
  transition; only the daemon's storage layer performs this write

### Requirement: Forward-only migrations with pre-migration backup

**Traces:** RF-35

The system MUST read `PRAGMA user_version` at startup. If it matches the
current schema version, the system MUST proceed. If it is lower, the system
MUST apply each pending migration in its own transaction, in order, setting
`PRAGMA user_version` as the final statement of each migration's
transaction, and MUST take a backup via SQLite's Online Backup API
immediately before migrating. If `user_version` is higher than the current
schema version, the system MUST abort startup with an explicit message and
MUST NOT downgrade the schema or continue in a best-effort mode. A published
migration MUST NEVER be rewritten; corrections are added as new migrations.

#### Scenario: Database is already at the current version

- GIVEN `PRAGMA user_version` equals the daemon's current schema version
- WHEN the daemon opens the database
- THEN no migration runs and no backup is taken

#### Scenario: Database requires migration

- GIVEN `PRAGMA user_version` is lower than the current schema version
- WHEN the daemon opens the database
- THEN a backup is taken via the Online Backup API before any migration
  statement executes
- AND each pending migration runs in its own transaction, with
  `PRAGMA user_version` updated as the last statement of that transaction

#### Scenario: Database was created by a newer version

- GIVEN `PRAGMA user_version` is higher than the daemon's current schema
  version
- WHEN the daemon attempts to open the database
- THEN startup aborts with an explicit message stating the database was
  created by a newer version
- AND the schema is not modified in any way

### Requirement: Startup recovery from an unclean shutdown

**Traces:** RF-36

Before accepting any X11 events, the system MUST query for intervals with
`"end" IS NULL`. If exactly one exists, the system MUST close it with
`end` set to the current startup instant and immediately open an `unknown`
interval from that point until the first real event. If more than one open
row exists, the system MUST treat this as an invariant violation, log an
error, close all but the most recently opened row with `end = start` (zero
duration, inventing no time), and continue.

#### Scenario: Single stale open interval from a crash

- GIVEN the database contains exactly one interval with `"end" IS NULL`,
  left by a previous `SIGKILL`
- WHEN the daemon starts
- THEN that interval is closed with `end` = the current startup instant
- AND an `unknown` interval is opened immediately from that instant

#### Scenario: Multiple open intervals (invariant violation)

- GIVEN the database contains two intervals with `"end" IS NULL` (a state
  that should be impossible under normal operation)
- WHEN the daemon starts
- THEN an error is logged
- AND all but the most recently opened of those intervals are closed with
  `end = start` (zero duration)
- AND daemon startup proceeds normally afterward

### Requirement: Retention pruning

**Traces:** RF-13, RF-52

`xwindowlog prune --older-than <duration>` MUST delete `intervals` rows with
`"end" < cutoff`, MUST NEVER delete the currently open interval, MUST delete
orphaned rows from `apps` and `titles` (excluding the reserved sentinel
rows), MUST NOT delete any row from `rules` or `projects`, and MUST run
`VACUUM` after the deletes. `prune` MUST NEVER be invoked by the daemon
itself; it is a separate CLI-only command. `retention_days` (default `365`;
`0` disables) is the configured default retention when no explicit
`--older-than` value is supplied; in Phase 1 this value is not applied
automatically — pruning happens only when `prune` is invoked or its optional
timer unit is installed.

#### Scenario: prune deletes old intervals but preserves the open one

- GIVEN a database with closed intervals older than the cutoff and one
  currently open interval
- WHEN `xwindowlog prune --older-than 180d` is run
- THEN all closed intervals older than the cutoff are deleted
- AND the open interval is not deleted regardless of its age

#### Scenario: prune preserves rules and projects

- GIVEN a database with `rules` and `projects` rows whose only referencing
  intervals are older than the prune cutoff
- WHEN `prune` runs and deletes those intervals
- THEN the `rules` and `projects` rows are not deleted

#### Scenario: prune shrinks the file via VACUUM

- GIVEN a prune operation has deleted a substantial number of rows
- WHEN `prune` completes its delete phase
- THEN `VACUUM` is run and the resulting database file size is smaller than
  before the prune

#### Scenario: prune never runs from within the daemon

- GIVEN the daemon is running normally
- WHEN normal daemon operation is inspected over its lifetime
- THEN the daemon process never invokes `prune`'s delete or VACUUM logic
  itself; that logic is reachable only via the separate `prune` CLI
  invocation

#### Scenario: VACUUM contends with a live daemon connection

- GIVEN the daemon holds an open, idle connection to the database in WAL
  mode
- WHEN `prune` runs concurrently, including its `VACUUM` phase
- THEN the delete phase commits successfully and is durable even if
  `VACUUM` encounters contention
- AND `VACUUM` retries on contention with a bounded backoff before either
  succeeding or exhausting its retry budget
- AND if `VACUUM` exhausts its retries, the command reports on stderr that
  the deletes are committed and durable but the file was not compacted,
  with actionable next steps, and exits with the dedicated state exit code
  for "state error" (not the generic error code)

### Requirement: Selective deletion (forget)

**Traces:** RF-53

`xwindowlog forget --from <ISO8601> --to <ISO8601>` or
`xwindowlog forget --window <id>` MUST physically delete the matching
interval rows (not merely flag them), MUST clean up orphaned `apps`/`titles`
rows, and MUST run `VACUUM`. Without `--yes`, the command MUST ask for
interactive confirmation showing the number of rows and the range to be
deleted before proceeding. `forget` MUST NEVER be reachable from any
non-CLI interface.

#### Scenario: forget deletes a specific time range

- GIVEN intervals exist between 15:32 and 15:40 on a given day
- WHEN `xwindowlog forget --from 15:32 --to 15:40 --yes` is run
- THEN those interval rows are physically deleted from the database
- AND orphaned `apps`/`titles` rows resulting from that deletion are also
  deleted
- AND `VACUUM` runs afterward

#### Scenario: forget without --yes requires confirmation

- GIVEN intervals exist in the specified range
- WHEN `xwindowlog forget --from <ISO8601> --to <ISO8601>` is run without
  `--yes` in an interactive terminal
- THEN the command displays the number of rows and the range that would be
  deleted and waits for explicit confirmation before deleting anything

#### Scenario: forget by window id

- GIVEN a specific interval with `id = 4821`
- WHEN `xwindowlog forget --window 4821 --yes` is run
- THEN that single interval row is deleted, along with any orphans it
  leaves behind

### Requirement: Interval clipping for range queries

**Traces:** RF-66

Every time-range aggregation (used by `today` and `status` in
`cli-reporting`) MUST operate on intervals clipped to the queried range, not
on raw interval boundaries, such that an interval spanning a range boundary
contributes only its overlapping portion to that range.

#### Scenario: An interval spanning midnight is clipped correctly

- GIVEN an interval from 23:50 to 00:10 (spanning a day boundary)
- WHEN "today" is queried for the earlier day and separately for the later
  day
- THEN the earlier day's query attributes exactly 10 minutes to that
  interval, and the later day's query attributes exactly the remaining 10
  minutes, with no overlap and no gap

#### Scenario: An open interval is clipped against the query's upper bound

- GIVEN an interval that is still open (no `end`) at the moment a range
  query is made
- WHEN "today" is queried
- THEN the open interval is clipped as if it ended at the query's upper
  bound (`now`), contributing only the elapsed portion up to that instant

### Requirement: Consistency guaranteed on power loss; durability of the most recent transactions is not (RNF-6)

**Traces:** RNF-6

The system MUST guarantee that the database is never left corrupted by a
power loss or `SIGKILL` (WAL mode, `synchronous = NORMAL`), and MUST make
recoverable, via startup recovery, whatever the last committed state was.
The system explicitly does NOT guarantee that the very last transactions
committed before a crash survive that crash; this is an accepted,
documented trade-off, not a defect.

#### Scenario: SIGKILL followed by restart recovers to a consistent state

- GIVEN the daemon is running and actively tracking intervals
- WHEN the process receives `SIGKILL` and is then restarted
- THEN the database opens without corruption
- AND the startup recovery requirement above closes any left-open interval
  and the daemon resumes normal operation
