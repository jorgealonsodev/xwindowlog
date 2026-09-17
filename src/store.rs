//! RF-11 schema + sentinels; `transition`/`close_only`/`open_only` (D-1); RF-35 migrations with
//! Online-Backup-API backup; RF-36 recovery; clipping CTE; `prune` (RF-13, D-11) and `forget` (RF-53).
//!
//! Phase 3 (this file, part 1/2) covered schema, permissions, migrations and
//! startup recovery. Phase 4 (this file, part 2/2) adds `transition`/
//! `close_only`/`open_only` behind the `IntervalStore` trait (design §5),
//! the RF-66 clipping query, and `prune`/`forget` (design §4 File Changes,
//! tasks.md Phase 4).
#![allow(
    dead_code,
    reason = "store.rs lands ahead of its consumers per design §8: main.rs \
              only wires Store::open() and the IntervalStore methods in at \
              composition time (Phase 15), and tracker.rs (Phase 5) is the \
              first caller of transition/close_only/open_only"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::clock::WallTs;

/// RF-35's ordered, forward-only migration list, indexed by the version it
/// produces. Version 1 is the entire real schema (design §8: "v1 migrates
/// nothing real" — there is no pre-existing installed base yet, so this
/// single entry both creates a fresh database and is the only currently
/// defined migration). A future schema change adds an entry, never rewrites
/// this one.
const MIGRATIONS: &[(i64, &str)] = &[(1, SCHEMA_V1)];

/// A handle to an opened, migrated, schema-current database connection.
pub struct Store {
    conn: Connection,
}

/// RF-36. `extra_open_rows > 0` is an invariant violation and can only be a
/// bug: it means more than one interval was found open at startup. `store.rs`
/// logs an error, closes all but the most recent with `end = start` (zero
/// duration — invent no time), then folds the survivor into the same
/// single-stale-interval handling below (design §5 `IntervalStore` contract).
pub struct Recovery {
    pub closed_stale: Option<WallTs>,
    pub extra_open_rows: u32,
}

/// Mirrors the `intervals.state` `CHECK` constraint (RF-11) as a closed Rust
/// enum instead of a bare string, so a typo such as `'activ'` (RF-11's own
/// negative-test scenario) is a compile error at every call site instead of
/// only a runtime `CHECK` failure. The schema constraint stays in place as
/// defense in depth (D-10 spirit: never rely on a single layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalState {
    Active,
    Afk,
    Locked,
    Unknown,
    Paused,
}

impl IntervalState {
    /// The exact `intervals.state` text this variant is stored as.
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            IntervalState::Active => "active",
            IntervalState::Afk => "afk",
            IntervalState::Locked => "locked",
            IntervalState::Unknown => "unknown",
            IntervalState::Paused => "paused",
        }
    }
}

/// One row to open, expressed at the storage boundary as the string
/// `app`/`title` identifiers rather than `apps`/`titles` integer ids —
/// `transition`/`open_only` own the dictionary upsert (D-1 rationale point
/// 3), the caller never resolves an id itself.
///
/// **Deviation, documented rather than silent:** design §2 D-8 places
/// `NewInterval` in `tracker.rs` (tasks.md 5.1), but `tracker.rs` does not
/// exist until Phase 5 and design §5's `IntervalStore` contract (this trait,
/// below) already requires `NewInterval` as a parameter type in Phase 4.
/// Defined here instead, with the expectation that Phase 5 imports
/// `crate::store::NewInterval` rather than redefining it — it is fundamentally
/// storage-shaped data (it names the exact columns `transition`/`open_only`
/// write), so `store.rs` owning it is a better fit than a forward
/// declaration would have been.
#[derive(Debug, Clone)]
pub struct NewInterval {
    pub app: String,
    pub title: String,
    pub pid: Option<u32>,
    pub state: IntervalState,
}

/// The D-1 contract (design §5 Interfaces/Contracts): `transition` takes ONE
/// instant, so the closed interval's `end` and the new interval's `start`
/// are the same value by construction — RF-3's contiguity is a type
/// invariant here, not a caller discipline.
pub trait IntervalStore {
    /// Closes the currently open interval (if any) and opens `open`, both at
    /// `at`, in exactly one `BEGIN IMMEDIATE` transaction with the `UPDATE`
    /// applied before the `INSERT` (D-1 rationale point 1: SQLite enforces
    /// `idx_intervals_one_open` at the end of each *statement*, not at
    /// `COMMIT`, so the reverse order fails on every transition after the
    /// first).
    fn transition(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError>;
    /// Closes the currently open interval (if any) at `at`, opening nothing.
    /// RF-33 shutdown, RF-27 pre-suspend.
    fn close_only(&mut self, at: WallTs) -> Result<(), StoreError>;
    /// Opens `open` at `at` without closing anything first. Startup,
    /// resume-from-suspend.
    fn open_only(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError>;
    /// RF-36 startup recovery, run once before the daemon accepts any X11
    /// events. Queries for intervals with `"end" IS NULL`; closes whatever it
    /// finds at `now` (zero-duration for every row except the most recently
    /// opened one, if there was more than one — an invariant violation that
    /// gets logged); and always leaves exactly one `unknown` interval open
    /// from `now`, ready for the first real event to transition out of.
    fn recover_on_startup(&mut self, now: WallTs) -> Result<Recovery, StoreError>;
}

/// D-1 rationale point 3: `INSERT OR IGNORE` + `SELECT id` instead of
/// `INSERT … ON CONFLICT … RETURNING id` — `RETURNING` requires SQLite
/// ≥ 3.35, which would silently raise the floor above the 3.31 D-10 asserts.
/// `table`/`column` are always literal `"apps"`/`"app_id"` or
/// `"titles"`/`"title"` from call sites in this module, never user input.
fn upsert_dict_id(
    tx: &Transaction<'_>,
    table: &'static str,
    column: &'static str,
    value: &str,
) -> Result<i64, StoreError> {
    tx.execute(
        &format!("INSERT OR IGNORE INTO {table} ({column}) VALUES (?1)"),
        rusqlite::params![value],
    )?;
    let id = tx.query_row(
        &format!("SELECT id FROM {table} WHERE {column} = ?1"),
        rusqlite::params![value],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Resolves `open`'s `app`/`title` dictionary ids (upserting as needed) and
/// inserts the new open row at `at`. Shared by `transition` and `open_only`
/// (design §5): the only difference between them is whether a prior `UPDATE`
/// ran in the same transaction.
fn insert_open_row(tx: &Transaction<'_>, at: WallTs, open: &NewInterval) -> Result<(), StoreError> {
    let app_id = upsert_dict_id(tx, "apps", "app_id", &open.app)?;
    let title_id = upsert_dict_id(tx, "titles", "title", &open.title)?;
    tx.execute(
        "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
         VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
        rusqlite::params![at.0, app_id, title_id, open.pid, open.state.as_db_str()],
    )?;
    Ok(())
}

impl IntervalStore for Store {
    fn transition(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // D-1: the UPDATE (close) MUST be applied before the INSERT (open)
        // within this transaction — see `IntervalStore::transition`'s doc.
        tx.execute(
            "UPDATE intervals SET \"end\" = ?1 WHERE \"end\" IS NULL",
            [at.0],
        )?;
        insert_open_row(&tx, at, &open)?;
        tx.commit()?;
        Ok(())
    }

    fn close_only(&mut self, at: WallTs) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE intervals SET \"end\" = ?1 WHERE \"end\" IS NULL",
            [at.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn open_only(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_open_row(&tx, at, &open)?;
        tx.commit()?;
        Ok(())
    }

    fn recover_on_startup(&mut self, now: WallTs) -> Result<Recovery, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, start FROM intervals WHERE \"end\" IS NULL \
             ORDER BY start DESC, id DESC",
        )?;
        let open_rows: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let extra_open_rows = open_rows.len().saturating_sub(1) as u32;

        let tx = self.conn.transaction()?;

        if open_rows.len() > 1 {
            // RF-36: an invariant violation, and it can only be a bug. Every
            // row except the most recently opened one (first after
            // `ORDER BY start DESC, id DESC`) closes with zero duration —
            // `end = start`, inventing no time.
            eprintln!(
                "xwindowlog: invariant violation at startup: {} open intervals found, \
                 expected at most 1; closing all but the most recently opened",
                open_rows.len()
            );
            for (id, start) in open_rows.iter().skip(1) {
                tx.execute(
                    "UPDATE intervals SET \"end\" = ?1 WHERE id = ?2",
                    rusqlite::params![start, id],
                )?;
            }
        }

        // Exactly one candidate open row remains at this point (the most
        // recently opened one, if any existed) — RF-36's "single stale open
        // interval" scenario. Close it at `now` and open `unknown` from
        // there, so the invariant "always exactly one open interval" holds
        // again before the first real event.
        let closed_stale = if !open_rows.is_empty() {
            tx.execute(
                "UPDATE intervals SET \"end\" = ?1 WHERE \"end\" IS NULL",
                [now.0],
            )?;
            Some(now)
        } else {
            None
        };

        tx.execute(
            "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
             VALUES (?1, NULL, 1, 1, NULL, 'unknown')",
            [now.0],
        )?;

        tx.commit()?;

        Ok(Recovery {
            closed_stale,
            extra_open_rows,
        })
    }
}

/// One row of PRD §11.3's clipping CTE (RF-66): a raw interval clipped to
/// `[from, to)`. `app`/`title` stay as dictionary ids here — resolving them
/// to strings is Phase 16's `today`/`status` concern, this phase only proves
/// the clipping arithmetic itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClippedInterval {
    pub id: i64,
    pub app: i64,
    pub title: i64,
    pub pid: Option<i64>,
    pub state: String,
    pub start: WallTs,
    pub end: WallTs,
}

impl Store {
    /// RF-66: every interval overlapping `[from, to)`, clipped to that
    /// range. An open interval (`"end" IS NULL`) clips against `to` as if it
    /// were still running (spec interval-storage "An open interval is
    /// clipped against the query's upper bound").
    pub fn clipped_intervals(
        &self,
        from: WallTs,
        to: WallTs,
    ) -> Result<Vec<ClippedInterval>, StoreError> {
        // PRD §11.3's clipping CTE, verbatim except for the trailing
        // `ORDER BY` (added for deterministic test/consumer ordering, not
        // part of the clipping semantics itself).
        let mut stmt = self.conn.prepare(
            "WITH clipped AS ( \
               SELECT i.id, i.app, i.title, i.state, i.pid, \
                      MAX(i.start, ?1)                 AS c_start, \
                      MIN(COALESCE(i.\"end\", ?2), ?2)  AS c_end \
               FROM intervals i \
               WHERE i.start < ?2 \
                 AND (i.\"end\" IS NULL OR i.\"end\" > ?1) \
             ) \
             SELECT id, app, title, state, pid, c_start, c_end FROM clipped \
             WHERE c_end > c_start \
             ORDER BY c_start, id",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![from.0, to.0], |r| {
                Ok(ClippedInterval {
                    id: r.get(0)?,
                    app: r.get(1)?,
                    title: r.get(2)?,
                    state: r.get(3)?,
                    pid: r.get(4)?,
                    start: WallTs(r.get(5)?),
                    end: WallTs(r.get(6)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

/// D-11: `VACUUM`'s contention outcome, distinct from `StoreError` because
/// exhausting the retry budget is an expected, already-durable state — the
/// delete phase already committed — not a failure to propagate with `?`.
/// `main.rs` (Phase 15) maps `Exhausted` to RF-60 exit code 2 and prints
/// `vacuum_exhausted_message`'s output to stderr verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VacuumOutcome {
    Vacuumed,
    Exhausted,
}

/// RF-60 exit code `prune`/`forget` must use when `VacuumOutcome::Exhausted`
/// — the same "state error" family as `StoreError::NewerSchema`, since the
/// deletes are durable but the file was not compacted.
pub const VACUUM_EXHAUSTED_EXIT_CODE: i32 = 2;

/// `deleted_intervals` + `vacuum` from a `prune`/`forget` call — shared by
/// both since they differ only in which rows were deleted (design §4:
/// `forget()` shares `prune`'s delete/orphan-cleanup/VACUUM machinery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeletionOutcome {
    pub deleted_intervals: u64,
    pub vacuum: VacuumOutcome,
}

/// D-11's bounded retry ladder: 3 attempts, waiting 1 s / 2 s / 4 s before
/// each one — `busy_timeout = 10_000` (below) already lets SQLite itself
/// absorb short contention inside a single attempt; this ladder is for
/// contention that outlasts that internal wait.
const VACUUM_BUSY_TIMEOUT_MS: u32 = 10_000;
const VACUUM_RETRY_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// Restored after the retry ladder finishes, matching RF-11's ordinary
/// per-connection pragma (`configure_connection`).
const ORDINARY_BUSY_TIMEOUT_MS: u32 = 5_000;

/// The exact stderr text D-11 specifies for a `VACUUM` that exhausts its
/// retry budget, minus the illustrative thousands-separator formatting of
/// the design doc's example figure (a cosmetic detail, not a requirement
/// asserted by any spec scenario).
pub fn vacuum_exhausted_message(deleted_intervals: u64) -> String {
    format!(
        "xwindowlog: deleted {deleted_intervals} intervals (data is committed and durable).\n\
         xwindowlog: the database file was NOT compacted: another process holds it open.\n\
         xwindowlog:   systemctl --user stop xwindowlog.service\n\
         xwindowlog:   xwindowlog prune --vacuum-only\n\
         xwindowlog:   systemctl --user start xwindowlog.service"
    )
}

/// `true` when `err` is SQLite reporting the database busy/locked — the
/// contention case D-11's retry ladder exists for, as opposed to a genuine
/// SQL error that must propagate immediately.
fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.code == rusqlite::ErrorCode::DatabaseBusy
                || ffi_err.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// RF-13/RF-53 shared cleanup: rows in `apps`/`titles` no longer referenced
/// by any interval, excluding the reserved sentinel rows (id 1). Shared by
/// `prune_delete` and `forget_range`/`forget_window` (design §4: "differing
/// only in the WHERE clause").
fn delete_orphaned_dictionary_rows(tx: &Transaction<'_>) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM apps WHERE id != 1 AND id NOT IN (SELECT app FROM intervals)",
        [],
    )?;
    tx.execute(
        "DELETE FROM titles WHERE id != 1 AND id NOT IN (SELECT title FROM intervals)",
        [],
    )?;
    Ok(())
}

impl Store {
    /// RF-13/RF-52 delete phase: `intervals` rows with `"end" < cutoff`,
    /// **never** the open interval (excluded structurally: `"end" IS NULL`
    /// never satisfies `"end" < cutoff` in SQL, and the condition is written
    /// explicitly here rather than relied upon implicitly), plus orphaned
    /// `apps`/`titles` rows. One `BEGIN IMMEDIATE` transaction; `VACUUM` is
    /// a separate phase (`vacuum_with_retry`, task 4.10) so a durable delete
    /// never depends on `VACUUM` succeeding.
    pub fn prune_delete(&mut self, cutoff: WallTs) -> Result<u64, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM intervals WHERE \"end\" IS NOT NULL AND \"end\" < ?1",
            [cutoff.0],
        )?;
        delete_orphaned_dictionary_rows(&tx)?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    /// D-11's bounded `VACUUM` retry ladder against the real backoffs and
    /// busy timeout. Thin wrapper over `vacuum_with_retry_configured` so
    /// tests can inject a fast `sleep_fn` and a short `busy_timeout_ms`
    /// without waiting on real 1 s/2 s/4 s sleeps or SQLite's own internal
    /// 10 s busy wait (rust-systems skill: "inject the clock" applied to
    /// `sleep`, the same testability seam).
    pub fn vacuum_with_retry(&mut self) -> Result<VacuumOutcome, StoreError> {
        self.vacuum_with_retry_configured(VACUUM_BUSY_TIMEOUT_MS, &std::thread::sleep)
    }

    fn vacuum_with_retry_configured(
        &mut self,
        busy_timeout_ms: u32,
        sleep_fn: &dyn Fn(Duration),
    ) -> Result<VacuumOutcome, StoreError> {
        // `VACUUM` needs its own, longer busy_timeout (D-11: 10 s, vs. the
        // ordinary 5 s from `configure_connection`) — restored afterward
        // regardless of outcome so later ordinary queries keep RF-11's
        // pragma.
        self.conn
            .pragma_update(None, "busy_timeout", busy_timeout_ms)?;
        let outcome = (|| -> Result<VacuumOutcome, StoreError> {
            for backoff in VACUUM_RETRY_BACKOFFS {
                sleep_fn(backoff);
                match self.conn.execute_batch("VACUUM") {
                    Ok(()) => {
                        // `VACUUM` alone rewrites SQLite's *logical* page
                        // count, but in WAL mode the on-disk file is only
                        // truncated to match once the WAL is checkpointed
                        // with `TRUNCATE` — otherwise the file stays at its
                        // pre-VACUUM size for as long as this connection (or
                        // any other) stays open, which defeats RF-13's "the
                        // file shrinks" promise for a long-lived daemon
                        // connection sharing the file. Best-effort: a
                        // partial/blocked checkpoint here does not fail the
                        // already-successful VACUUM.
                        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                        return Ok(VacuumOutcome::Vacuumed);
                    }
                    Err(e) if is_busy(&e) => continue,
                    Err(e) => return Err(StoreError::Sqlite(e)),
                }
            }
            Ok(VacuumOutcome::Exhausted)
        })();
        self.conn
            .pragma_update(None, "busy_timeout", ORDINARY_BUSY_TIMEOUT_MS)?;
        outcome
    }

    /// `--vacuum-only`: re-runs just the retry ladder, e.g. after an earlier
    /// `prune`'s delete phase committed but its `VACUUM` exhausted.
    pub fn vacuum_only(&mut self) -> Result<VacuumOutcome, StoreError> {
        self.vacuum_with_retry()
    }

    /// RF-13/RF-52: delete phase then `VACUUM`, combined.
    pub fn prune(&mut self, cutoff: WallTs) -> Result<DeletionOutcome, StoreError> {
        let deleted_intervals = self.prune_delete(cutoff)?;
        let vacuum = self.vacuum_with_retry()?;
        Ok(DeletionOutcome {
            deleted_intervals,
            vacuum,
        })
    }

    /// RF-53 `forget --from/--to`: physically deletes every interval row
    /// overlapping `[from, to)` (the same overlap test as the RF-66 clipping
    /// CTE's `WHERE` clause — a partially-overlapping row is deleted whole,
    /// never trimmed), cleans up orphans, and runs `VACUUM`.
    pub fn forget_range(
        &mut self,
        from: WallTs,
        to: WallTs,
    ) -> Result<DeletionOutcome, StoreError> {
        let deleted_intervals = {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let deleted = tx.execute(
                "DELETE FROM intervals WHERE start < ?2 AND (\"end\" IS NULL OR \"end\" > ?1)",
                rusqlite::params![from.0, to.0],
            )?;
            delete_orphaned_dictionary_rows(&tx)?;
            tx.commit()?;
            deleted as u64
        };
        let vacuum = self.vacuum_with_retry()?;
        Ok(DeletionOutcome {
            deleted_intervals,
            vacuum,
        })
    }

    /// RF-53 `forget --window <id>`: physically deletes the single interval
    /// row `id`, cleans up orphans, and runs `VACUUM`.
    pub fn forget_window(&mut self, id: i64) -> Result<DeletionOutcome, StoreError> {
        let deleted_intervals = {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let deleted = tx.execute("DELETE FROM intervals WHERE id = ?1", [id])?;
            delete_orphaned_dictionary_rows(&tx)?;
            tx.commit()?;
            deleted as u64
        };
        let vacuum = self.vacuum_with_retry()?;
        Ok(DeletionOutcome {
            deleted_intervals,
            vacuum,
        })
    }
}

/// RF-60 discipline: every failure mode this module can produce carries
/// enough information for `main.rs` to pick a deliberate exit code, never a
/// blanket `1`.
#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// D-10 layer 3: the linked SQLite does not actually enforce
    /// `idx_intervals_one_open`. Environment error (RF-60 exit code 3).
    InvariantViolated(String),
    /// RF-35: `PRAGMA user_version` on disk is higher than this binary's
    /// current schema version. State error (RF-60 exit code 2), the same
    /// family as "database missing" — the on-disk state cannot be used by
    /// this binary as-is.
    NewerSchema {
        found: i64,
        current: i64,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            StoreError::Io(e) => write!(f, "I/O error: {e}"),
            StoreError::InvariantViolated(msg) => {
                write!(f, "storage invariant violated: {msg}")
            }
            StoreError::NewerSchema { found, current } => write!(
                f,
                "database was created by a newer version of xwindowlog (schema {found}, \
                 this binary supports up to {current}); refusing to downgrade or continue"
            ),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Sqlite(e) => Some(e),
            StoreError::Io(e) => Some(e),
            StoreError::InvariantViolated(_) | StoreError::NewerSchema { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl StoreError {
    /// RF-60 exit-code mapping.
    pub fn exit_code(&self) -> i32 {
        match self {
            StoreError::InvariantViolated(_) => 3,
            StoreError::NewerSchema { .. } => 2,
            StoreError::Sqlite(_) | StoreError::Io(_) => 1,
        }
    }
}

/// Creates `dir` (and any missing ancestors) at mode `0700`, explicitly —
/// not by relying on umask alone (rust-systems skill: "set the umask before
/// creating files, never chmod after" governs *files*; for a directory we
/// state the mode directly so it is correct regardless of the ambient
/// umask). A no-op if `dir` already exists.
fn create_dir_0700(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(StoreError::Io)
}

/// D-10 layer 3: re-runs D-10 layer 1's exact behavioural proof against a
/// throwaway in-memory connection on every `open()`, so a build linked
/// against an SQLite whose NULL-distinctness semantics changed (or that
/// otherwise fails to enforce `idx_intervals_one_open`) fails loudly at
/// startup with a typed error instead of silently writing a database with no
/// open-interval guarantee. Parameterized over the schema so the negative
/// case (the guard correctly detecting an *absent* constraint) is testable
/// without needing a genuinely broken linked SQLite.
fn assert_unique_open_interval_invariant(schema: &str) -> Result<(), StoreError> {
    let probe = Connection::open_in_memory()?;
    configure_connection(&probe)?;
    probe.execute_batch(schema)?;

    let insert = |start: i64| -> rusqlite::Result<()> {
        probe
            .execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (?1, NULL, 1, 1, NULL, 'active')",
                [start],
            )
            .map(|_| ())
    };

    insert(1)?;
    match insert(2) {
        Err(e) if is_unique_open_interval_violation(&e) => Ok(()),
        Err(e) => Err(StoreError::Sqlite(e)),
        Ok(()) => Err(StoreError::InvariantViolated(
            "linked SQLite accepted a second open interval; idx_intervals_one_open did not \
             reject it (D-10 layer 1 behavioural proof failed at runtime)"
                .to_string(),
        )),
    }
}

/// RF-35's forward-only migration runner, generic over the migrations list
/// so the mechanism is exercised with **synthetic** migrations in tests
/// (design §8: "v1 migrates nothing real") without touching production DDL.
/// `migrations` MUST be sorted ascending by version; `open()` calls this with
/// `MIGRATIONS` (task 3.13).
fn migrate_with(
    conn: &mut Connection,
    migrations: &[(i64, &str)],
    backup_path: &Path,
) -> Result<(), StoreError> {
    let existing: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let target = migrations.last().map(|(v, _)| *v).unwrap_or(0);

    if existing == target {
        return Ok(());
    }
    if existing > target {
        return Err(StoreError::NewerSchema {
            found: existing,
            current: target,
        });
    }

    // RF-35: a backup via the Online Backup API before the first migration
    // statement executes — never a raw file copy, which can capture the main
    // file without its WAL and produce an inconsistent snapshot.
    let mut backup_dest = Connection::open(backup_path)?;
    {
        let backup = rusqlite::backup::Backup::new(conn, &mut backup_dest)?;
        backup.run_to_completion(5, std::time::Duration::from_millis(250), None)?;
    }
    drop(backup_dest);

    for (version, sql) in migrations.iter().filter(|(v, _)| *v > existing) {
        // Each migration is its own transaction (D-1's "one transaction per
        // logical change" applied to schema changes too), with
        // `PRAGMA user_version` as the final statement before commit — never
        // all pending migrations in one transaction, so a later migration's
        // failure cannot roll back an earlier one that already succeeded.
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", *version)?;
        tx.commit()?;
    }

    Ok(())
}

impl Store {
    /// Opens (creating if necessary) the database at `path`: sets a
    /// restrictive `umask` before any file is created (RF-10), creates the
    /// containing directory at mode `0700` if missing, configures the
    /// connection (RF-11 pragmas), and brings the schema up to date
    /// (RF-35/RF-36 land in later tasks of this phase).
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        // D-10 layer 3, checked first and fast (in-memory, no file I/O): an
        // oddly-linked SQLite build must fail loudly here, before this open
        // ever writes a database with no open-interval guarantee.
        assert_unique_open_interval_invariant(SCHEMA_V1)?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_0700(parent)?;
            }
        }

        // umask(2) is a process-wide POSIX property, not a per-file or
        // per-connection setting, and SQLite's WAL mode creates `-wal`/`-shm`
        // as separate files whose mode comes from the umask at creation
        // time, not from the main database file's mode (RF-10). Elevate it
        // for exactly the window in which this open can create files, then
        // restore it unconditionally.
        let previous_umask = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let outcome = (|| -> Result<Connection, StoreError> {
            let mut conn = Connection::open(path)?;
            configure_connection(&conn)?;
            let backup_path = PathBuf::from(format!("{}.migration-backup", path.display()));
            migrate_with(&mut conn, MIGRATIONS, &backup_path)?;
            Ok(conn)
        })();
        nix::sys::stat::umask(previous_umask);

        Ok(Store { conn: outcome? })
    }
}

/// RF-11 schema DDL. Applied to `Connection::open_in_memory()` by every test
/// (D-10 layer 1, tasks.md 3.1) and by `open()`/`migrate()` against the real
/// database, so there is never a fixture copy that can drift from what
/// production actually runs (design §6 testing-strategy note).
///
/// Consolidated into a single `const` (task 3.13 REFACTOR) so there is
/// exactly one copy of the DDL — used by `MIGRATIONS` (and therefore
/// `open()`) and by every test via `open_in_memory_with_schema()` — instead
/// of a fixture that could drift from what production actually runs.
pub(crate) const SCHEMA_V1: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE apps   (id INTEGER PRIMARY KEY, app_id TEXT NOT NULL UNIQUE);
CREATE TABLE titles (id INTEGER PRIMARY KEY, title  TEXT NOT NULL UNIQUE);
-- Mandatory sentinel rows (id=1 reserved by store.rs convention):
--   apps  (1, '?')  -> WM_CLASS absent
--   titles(1, '-')  -> states with no real window (afk/locked/unknown/paused)
INSERT INTO apps   (id, app_id) VALUES (1, '?');
INSERT INTO titles (id, title)  VALUES (1, '-');

CREATE TABLE intervals (
  id     INTEGER PRIMARY KEY,
  start  INTEGER NOT NULL,
  "end"  INTEGER,
  app    INTEGER NOT NULL REFERENCES apps(id),
  title  INTEGER NOT NULL REFERENCES titles(id),
  pid    INTEGER,
  state  TEXT NOT NULL
         CHECK (state IN ('active','afk','locked','unknown','paused')),
  -- Generated column: 1 only while the interval is open, NULL otherwise.
  open_marker INTEGER GENERATED ALWAYS AS (CASE WHEN "end" IS NULL THEN 1 END) VIRTUAL,
  CHECK ("end" IS NULL OR "end" >= start)
);

CREATE INDEX idx_intervals_start ON intervals(start);
CREATE INDEX idx_intervals_end   ON intervals("end");

-- Guarantees AT MOST ONE open interval in the whole table. NULLs are
-- distinct from each other in a SQLite unique index, so only rows with
-- open_marker = 1 collide.
CREATE UNIQUE INDEX idx_intervals_one_open ON intervals(open_marker);

CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

CREATE TABLE rules (
  id        INTEGER PRIMARY KEY,
  app       TEXT,
  pattern   TEXT NOT NULL,
  project   INTEGER NOT NULL REFERENCES projects(id),
  status    TEXT NOT NULL DEFAULT 'pending'
            CHECK (status IN ('pending','confirmed','rejected','inactive')),
  origin    TEXT NOT NULL CHECK (origin IN ('ai','manual')),
  created   INTEGER NOT NULL,
  updated   INTEGER NOT NULL,
  confirmed INTEGER
);

CREATE INDEX idx_rules_status ON rules(status);
"#;

/// Per-connection pragmas (RF-11). `PRAGMA foreign_keys = ON` is set
/// explicitly and unconditionally even though this crate's `bundled`
/// `rusqlite`/SQLite build happens to compile with
/// `-DSQLITE_DEFAULT_FOREIGN_KEYS=1` (verified against the actual linked
/// library, not assumed) — relying on that compile-time default would make
/// FK enforcement silently regress under a system SQLite or a different
/// vendoring, which is exactly the class of assumption D-10 exists to not
/// make. Every connection gets this, test connections included (rust-systems
/// skill: "`PRAGMA foreign_keys = ON` is per connection and off by default").
pub(crate) fn configure_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

/// `true` when `err` is the `SQLITE_CONSTRAINT_UNIQUE` failure expected from
/// `idx_intervals_one_open` rejecting a second open interval (D-1, D-10
/// layer 1).
pub(crate) fn is_unique_open_interval_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// `umask` is process-global POSIX state, not per-thread, and `cargo
    /// test` runs test functions concurrently in one process by default.
    /// This lock serializes every test in this file that mutates it, so a
    /// permissions test can set an ambient umask without racing another test
    /// thread's file creation. Every other test in this module creates no
    /// files (in-memory connections only), so this is the only contention
    /// point that exists today.
    static UMASK_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A directory under the OS temp dir, unique per test, removed on drop.
    /// Hand-rolled instead of depending on the `tempfile` crate (present
    /// only transitively via `trybuild`, not a declared Phase 1 dependency)
    /// to avoid adding a dependency outside the list design.md §4 commits to.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "xwindowlog-test-{label}-{}-{n}",
                std::process::id()
            ));
            ScratchDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("metadata({}): {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn open_creates_db_wal_shm_and_data_dir_with_restrictive_modes() {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // GIVEN the process umask is the common system default 022.
        let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));

        let scratch = ScratchDir::new("permissions");
        let data_dir = scratch.path().join("data").join("xwindowlog");
        let db_path = data_dir.join("xwindowlog.db");

        let result = Store::open(&db_path);

        // Restore the pre-test umask immediately, regardless of outcome, so
        // a failure here cannot leak a restrictive umask into later tests.
        nix::sys::stat::umask(previous);

        let _store = result.expect("store opens against a fresh path");

        assert_eq!(
            mode_of(&data_dir),
            0o700,
            "data directory must be created at mode 0700"
        );
        assert_eq!(mode_of(&db_path), 0o600, "xwindowlog.db must be mode 0600");
        assert_eq!(
            mode_of(&db_path.with_extension("db-wal")),
            0o600,
            "xwindowlog.db-wal must be mode 0600"
        );
        assert_eq!(
            mode_of(&db_path.with_extension("db-shm")),
            0o600,
            "xwindowlog.db-shm must be mode 0600"
        );
    }

    /// A deliberately broken stand-in for `SCHEMA_V1` missing exactly
    /// `idx_intervals_one_open` — the only way to prove the D-10 layer 3
    /// guard fails closed when the invariant it checks is genuinely absent,
    /// without needing a genuinely misbehaving linked SQLite (T-2-style
    /// mutation test: a guard that always returns `Ok` would pass a
    /// same-schema test vacuously; this is the schema variant it must catch).
    const BROKEN_SCHEMA_MISSING_UNIQUE_INDEX: &str = r#"
CREATE TABLE apps   (id INTEGER PRIMARY KEY, app_id TEXT NOT NULL UNIQUE);
CREATE TABLE titles (id INTEGER PRIMARY KEY, title  TEXT NOT NULL UNIQUE);
INSERT INTO apps   (id, app_id) VALUES (1, '?');
INSERT INTO titles (id, title)  VALUES (1, '-');

CREATE TABLE intervals (
  id     INTEGER PRIMARY KEY,
  start  INTEGER NOT NULL,
  "end"  INTEGER,
  app    INTEGER NOT NULL REFERENCES apps(id),
  title  INTEGER NOT NULL REFERENCES titles(id),
  pid    INTEGER,
  state  TEXT NOT NULL
);
"#;

    #[test]
    fn invariant_guard_passes_against_the_real_schema() {
        assert_unique_open_interval_invariant(SCHEMA_V1)
            .expect("the real schema enforces idx_intervals_one_open");
    }

    #[test]
    fn invariant_guard_fails_closed_when_the_unique_index_is_absent() {
        let err = assert_unique_open_interval_invariant(BROKEN_SCHEMA_MISSING_UNIQUE_INDEX)
            .expect_err("a schema missing idx_intervals_one_open must be caught");

        assert!(
            matches!(err, StoreError::InvariantViolated(_)),
            "expected InvariantViolated, got {err:?}"
        );
        assert_eq!(
            err.exit_code(),
            3,
            "D-10 layer 3 / RF-60: an environment error must exit 3"
        );
    }

    #[test]
    fn migrate_with_already_current_runs_no_migration_and_no_backup() {
        let scratch = ScratchDir::new("migrate-current");
        std::fs::create_dir_all(scratch.path()).expect("scratch dir");
        let backup_path = scratch.path().join("backup.db");
        let mut conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch("PRAGMA user_version = 2;")
            .expect("seed version");

        let migrations: &[(i64, &str)] = &[
            (1, "CREATE TABLE t1 (x INTEGER);"),
            (2, "CREATE TABLE t2 (y INTEGER);"),
        ];

        migrate_with(&mut conn, migrations, &backup_path).expect("no-op migration succeeds");

        assert!(
            !backup_path.exists(),
            "no backup must be taken when already at the current version"
        );
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn migrate_with_lower_version_backs_up_then_applies_each_pending_migration() {
        let scratch = ScratchDir::new("migrate-lower");
        std::fs::create_dir_all(scratch.path()).expect("scratch dir");
        let backup_path = scratch.path().join("backup.db");
        let mut conn = Connection::open_in_memory().expect("open in-memory");
        // Starts at user_version 0 by default.

        let migrations: &[(i64, &str)] = &[
            (1, "CREATE TABLE t1 (x INTEGER);"),
            (2, "CREATE TABLE t2 (y INTEGER);"),
        ];

        migrate_with(&mut conn, migrations, &backup_path).expect("migration succeeds");

        assert!(
            backup_path.exists(),
            "a backup must be taken via the Online Backup API before migrating"
        );
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        conn.execute("INSERT INTO t1 (x) VALUES (1)", [])
            .expect("t1 exists");
        conn.execute("INSERT INTO t2 (y) VALUES (1)", [])
            .expect("t2 exists");
    }

    #[test]
    fn migrate_with_each_migration_commits_in_its_own_transaction() {
        let scratch = ScratchDir::new("migrate-partial");
        std::fs::create_dir_all(scratch.path()).expect("scratch dir");
        let backup_path = scratch.path().join("backup.db");
        let mut conn = Connection::open_in_memory().expect("open in-memory");

        let migrations: &[(i64, &str)] = &[
            (1, "CREATE TABLE t1 (x INTEGER);"),
            (2, "THIS IS NOT VALID SQL;"),
        ];

        let result = migrate_with(&mut conn, migrations, &backup_path);
        assert!(
            result.is_err(),
            "an invalid migration must surface an error"
        );

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 1,
            "migration 1 already committed independently of migration 2's failure \
             (D-1: each migration is its own transaction, never all-or-nothing)"
        );
        conn.execute("INSERT INTO t1 (x) VALUES (1)", [])
            .expect("t1 persisted from the already-committed migration");
    }

    #[test]
    fn migrate_with_newer_version_aborts_without_modifying_schema_or_version() {
        let scratch = ScratchDir::new("migrate-newer");
        std::fs::create_dir_all(scratch.path()).expect("scratch dir");
        let backup_path = scratch.path().join("backup.db");
        let mut conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch("PRAGMA user_version = 5;")
            .expect("seed version");

        let migrations: &[(i64, &str)] = &[(1, "CREATE TABLE t1 (x INTEGER);")];

        let err = migrate_with(&mut conn, migrations, &backup_path)
            .expect_err("a newer on-disk schema must abort startup");

        match err {
            StoreError::NewerSchema { found, current } => {
                assert_eq!(found, 5);
                assert_eq!(current, 1);
            }
            other => panic!("expected NewerSchema, got {other:?}"),
        }
        assert!(
            !backup_path.exists(),
            "no backup, and no migration attempt, on abort"
        );
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5, "schema/version must not be modified on abort");
    }

    use crate::clock::WallTs;

    /// Seeds `starts.len()` open intervals directly. For more than one, this
    /// can only be done by dropping `idx_intervals_one_open` and leaving it
    /// dropped — the schema's own unique index makes this state genuinely
    /// unreachable through ordinary `store.rs` writes (it would refuse a
    /// second open row on `INSERT`, and refuse to be recreated over already-
    /// duplicate data), exactly as RF-36's own scenario text frames it ("a
    /// state that should be impossible under normal operation"). This
    /// simulates the state having arisen anyway (e.g. a bug or manual
    /// tampering that also removed the index); `recover_on_startup`'s own
    /// SQL does not depend on the index being present to detect or fix it.
    fn store_with_open_intervals(starts: &[i64]) -> Store {
        let conn = open_in_memory_with_schema();
        if starts.len() > 1 {
            conn.execute_batch("DROP INDEX idx_intervals_one_open;")
                .expect("drop unique index to seed an anomalous state");
        }
        for &start in starts {
            conn.execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (?1, NULL, 1, 1, NULL, 'active')",
                [start],
            )
            .expect("seed open interval");
        }
        Store { conn }
    }

    fn open_interval_count(store: &Store) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM intervals WHERE \"end\" IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn recover_on_startup_closes_single_stale_interval_and_opens_unknown() {
        let mut store = store_with_open_intervals(&[100]);

        let recovery = store
            .recover_on_startup(WallTs(500))
            .expect("recovery succeeds");

        assert_eq!(recovery.closed_stale, Some(WallTs(500)));
        assert_eq!(recovery.extra_open_rows, 0);

        let end: i64 = store
            .conn
            .query_row("SELECT \"end\" FROM intervals WHERE start = 100", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(end, 500, "the stale interval closes at the startup instant");

        assert_eq!(
            open_interval_count(&store),
            1,
            "exactly one open interval must remain after recovery"
        );

        let (open_start, open_state): (i64, String) = store
            .conn
            .query_row(
                "SELECT start, state FROM intervals WHERE \"end\" IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(open_start, 500);
        assert_eq!(open_state, "unknown");
    }

    #[test]
    fn recover_on_startup_closes_all_but_most_recent_of_multiple_open_intervals() {
        let mut store = store_with_open_intervals(&[100, 200, 300]);

        let recovery = store
            .recover_on_startup(WallTs(500))
            .expect("recovery succeeds");

        assert_eq!(
            recovery.extra_open_rows, 2,
            "two rows beyond the most recently opened one are the invariant violation"
        );

        // The two older rows close with zero duration: end == start, never `now`.
        for start in [100, 200] {
            let end: i64 = store
                .conn
                .query_row(
                    "SELECT \"end\" FROM intervals WHERE start = ?1",
                    [start],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                end, start,
                "an extra open row must close with zero duration, inventing no time"
            );
        }

        // The most recent (300) is then handled as the single stale interval:
        // closed at `now`, with a fresh `unknown` interval opened from there.
        let end_of_most_recent: i64 = store
            .conn
            .query_row("SELECT \"end\" FROM intervals WHERE start = 300", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(end_of_most_recent, 500);

        assert_eq!(open_interval_count(&store), 1);
        let open_state: String = store
            .conn
            .query_row(
                "SELECT state FROM intervals WHERE \"end\" IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open_state, "unknown");
    }

    fn open_in_memory_with_schema() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        configure_connection(&conn).expect("configure pragmas");
        conn.execute_batch(SCHEMA_V1).expect("apply SCHEMA_V1");
        conn
    }

    fn insert_open_interval(conn: &Connection, start: i64) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
             VALUES (?1, NULL, 1, 1, NULL, 'active')",
            [start],
        )
        .map(|_| ())
    }

    #[test]
    fn second_open_interval_is_rejected_by_unique_index() {
        let conn = open_in_memory_with_schema();
        insert_open_interval(&conn, 100).expect("first open interval inserts");

        let err =
            insert_open_interval(&conn, 200).expect_err("a second open interval must be rejected");

        assert!(
            is_unique_open_interval_violation(&err),
            "expected SQLITE_CONSTRAINT_UNIQUE on idx_intervals_one_open, got {err:?}"
        );
    }

    fn is_check_violation(err: &rusqlite::Error) -> bool {
        matches!(
            err,
            rusqlite::Error::SqliteFailure(ffi_err, _)
                if ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        )
    }

    fn is_foreign_key_violation(err: &rusqlite::Error) -> bool {
        matches!(
            err,
            rusqlite::Error::SqliteFailure(ffi_err, _)
                if ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
        )
    }

    #[test]
    fn invalid_state_value_is_rejected_by_check_constraint() {
        let conn = open_in_memory_with_schema();

        let err = conn
            .execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (100, NULL, 1, 1, NULL, 'activ')",
                [],
            )
            .expect_err("a typo'd state must be rejected");

        assert!(
            is_check_violation(&err),
            "expected a CHECK constraint failure on state, got {err:?}"
        );
    }

    #[test]
    fn negative_duration_row_is_rejected_by_check_constraint() {
        let conn = open_in_memory_with_schema();

        let err = conn
            .execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (100, 50, 1, 1, NULL, 'active')",
                [],
            )
            .expect_err("end < start must be rejected");

        assert!(
            is_check_violation(&err),
            "expected a CHECK constraint failure on end >= start, got {err:?}"
        );
    }

    #[test]
    fn interval_referencing_nonexistent_app_is_rejected() {
        let conn = open_in_memory_with_schema();

        let err = conn
            .execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (100, NULL, 999, 1, NULL, 'active')",
                [],
            )
            .expect_err("a reference to a nonexistent apps.id must be rejected");

        assert!(
            is_foreign_key_violation(&err),
            "expected a foreign-key constraint failure, got {err:?}"
        );
    }

    // ── Phase 4: transitions, clipping, prune, forget ───────────────────────

    fn store_with_schema() -> Store {
        Store {
            conn: open_in_memory_with_schema(),
        }
    }

    fn sample_open(app: &str, title: &str) -> NewInterval {
        NewInterval {
            app: app.to_string(),
            title: title.to_string(),
            pid: Some(4242),
            state: IntervalState::Active,
        }
    }

    #[test]
    fn transition_applies_update_before_insert_and_succeeds() {
        // GIVEN an open interval exists (spec interval-storage "Closing
        // before opening within the transaction avoids a false collision").
        let mut store = store_with_schema();
        insert_open_interval(&store.conn, 100).expect("seed the initial open interval");

        // WHEN a transition transaction is applied.
        //
        // THEN it succeeds — this is the order-sensitive assertion (D-1
        // rationale point 1: "a test must assert the order, not just the
        // outcome"): the companion test below,
        // `insert_before_update_ordering_would_collide_on_the_unique_open_index`,
        // proves directly that the INSERT-before-UPDATE order collides with
        // this exact seeded state on `idx_intervals_one_open`. An
        // implementation of `transition` that applied the statements in
        // that wrong order would therefore fail here — this assertion is
        // the outcome that order produces, made to fail specifically because
        // of the order, not despite it.
        store
            .transition(WallTs(200), sample_open("firefox", "Example"))
            .expect("update-before-insert must not collide with the existing open row");

        assert_eq!(
            open_interval_count(&store),
            1,
            "exactly one open interval must remain after the transition"
        );
        let (open_start, open_end): (i64, Option<i64>) = store
            .conn
            .query_row(
                "SELECT start, \"end\" FROM intervals WHERE \"end\" IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            open_start, 200,
            "the new interval opens at the transition instant"
        );
        assert_eq!(open_end, None);
        let closed_end: i64 = store
            .conn
            .query_row("SELECT \"end\" FROM intervals WHERE start = 100", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            closed_end, 200,
            "the old interval closes at the same instant the new one opens (RF-3)"
        );
    }

    #[test]
    fn insert_before_update_ordering_would_collide_on_the_unique_open_index() {
        // Characterizes the SQLite behaviour D-1's rationale depends on,
        // independent of `transition`'s own implementation: given one open
        // interval already exists, applying the two statements in the WRONG
        // order (INSERT before UPDATE) fails immediately on
        // `idx_intervals_one_open`, proving the order is load-bearing rather
        // than stylistic.
        let conn = open_in_memory_with_schema();
        insert_open_interval(&conn, 100).expect("seed the initial open interval");

        let tx = conn
            .unchecked_transaction()
            .expect("begin a transaction for the wrong-order attempt");
        let insert_err = tx
            .execute(
                "INSERT INTO intervals (start, \"end\", app, title, pid, state) \
                 VALUES (200, NULL, 1, 1, NULL, 'active')",
                [],
            )
            .expect_err("INSERT before UPDATE must collide with the still-open first row");
        assert!(
            is_unique_open_interval_violation(&insert_err),
            "expected SQLITE_CONSTRAINT_UNIQUE on idx_intervals_one_open, got {insert_err:?}"
        );
    }

    #[test]
    fn transition_chain_commits_one_transaction_per_link_with_shared_boundaries() {
        // RF-12 "A transition writes exactly one transaction": across a
        // chain of transitions, each closed interval's `end` equals the next
        // interval's `start` (RF-3 contiguity), and at no point does more
        // than one open interval exist.
        let mut store = store_with_schema();
        store
            .open_only(WallTs(0), sample_open("firefox", "Example"))
            .expect("initial open");
        assert_eq!(open_interval_count(&store), 1);

        store
            .transition(WallTs(100), sample_open("kitty", "zsh"))
            .expect("first transition");
        assert_eq!(
            open_interval_count(&store),
            1,
            "no more than one open interval after a transition"
        );

        store
            .transition(WallTs(250), sample_open("firefox", "Example"))
            .expect("second transition back to the first app");
        assert_eq!(open_interval_count(&store), 1);

        let mut stmt = store
            .conn
            .prepare("SELECT start, \"end\" FROM intervals ORDER BY start")
            .unwrap();
        let rows: Vec<(i64, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        drop(stmt);

        assert_eq!(
            rows,
            vec![(0, Some(100)), (100, Some(250)), (250, None)],
            "each closed interval's end must equal the next interval's start, \
             with no gap or overlap"
        );

        // Re-using the "firefox" app_id string on the third transition must
        // resolve to the SAME apps.id as the first interval, not a
        // duplicate dictionary row (D-1 rationale point 3: INSERT OR IGNORE
        // + SELECT id).
        let distinct_app_ids: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(DISTINCT id) FROM apps WHERE app_id = 'firefox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(distinct_app_ids, 1);
    }

    /// Midnight, expressed as an arbitrary epoch second — the exact value
    /// does not matter, only that day 1 is `[MIDNIGHT - 86_400, MIDNIGHT)`
    /// and day 2 is `[MIDNIGHT, MIDNIGHT + 86_400)`.
    const MIDNIGHT: i64 = 1_000_000_000;

    #[test]
    fn clipped_intervals_spanning_midnight_attributes_ten_minutes_to_each_day() {
        // GIVEN an interval from 23:50 to 00:10 (RF-66 scenario "An interval
        // spanning midnight is clipped correctly").
        let mut store = store_with_schema();
        store
            .open_only(WallTs(MIDNIGHT - 600), sample_open("firefox", "Example"))
            .expect("open the spanning interval");
        store
            .close_only(WallTs(MIDNIGHT + 600))
            .expect("close the spanning interval");

        // WHEN "today" is queried for the earlier day.
        let earlier_day = store
            .clipped_intervals(WallTs(MIDNIGHT - 86_400), WallTs(MIDNIGHT))
            .expect("clipped query for the earlier day succeeds");
        // THEN it attributes exactly 10 minutes, clipped at the day boundary.
        assert_eq!(earlier_day.len(), 1);
        assert_eq!(earlier_day[0].start, WallTs(MIDNIGHT - 600));
        assert_eq!(earlier_day[0].end, WallTs(MIDNIGHT));
        assert_eq!(
            crate::clock::duration_secs(earlier_day[0].start, earlier_day[0].end),
            600
        );

        // WHEN "today" is queried separately for the later day.
        let later_day = store
            .clipped_intervals(WallTs(MIDNIGHT), WallTs(MIDNIGHT + 86_400))
            .expect("clipped query for the later day succeeds");
        // THEN it attributes exactly the remaining 10 minutes, with no
        // overlap and no gap against the earlier day's query.
        assert_eq!(later_day.len(), 1);
        assert_eq!(later_day[0].start, WallTs(MIDNIGHT));
        assert_eq!(later_day[0].end, WallTs(MIDNIGHT + 600));
        assert_eq!(
            crate::clock::duration_secs(later_day[0].start, later_day[0].end),
            600
        );
        assert_eq!(
            earlier_day[0].end, later_day[0].start,
            "the two clipped halves must share exactly one boundary instant, \
             never overlapping and never leaving a gap"
        );
    }

    #[test]
    fn clipped_intervals_open_interval_clips_against_the_query_upper_bound() {
        // GIVEN an interval that is still open (no `end`) at query time
        // (RF-66 scenario "An open interval is clipped against the query's
        // upper bound").
        let mut store = store_with_schema();
        store
            .open_only(WallTs(1_000), sample_open("kitty", "zsh"))
            .expect("open the still-open interval");

        // WHEN "today" is queried with `now` as the upper bound.
        let now = WallTs(1_500);
        let rows = store
            .clipped_intervals(WallTs(0), now)
            .expect("clipped query against an open interval succeeds");

        // THEN the open interval is clipped as if it ended at `now`,
        // contributing only the elapsed portion up to that instant.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].start, WallTs(1_000));
        assert_eq!(rows[0].end, now);
        assert_eq!(crate::clock::duration_secs(rows[0].start, rows[0].end), 500);
    }

    #[test]
    fn clipped_intervals_excludes_rows_entirely_outside_the_range() {
        // A row that starts at or after `to`, and a row that ends at or
        // before `from`, must not appear at all — not as a zero-length row.
        let mut store = store_with_schema();
        store
            .open_only(WallTs(0), sample_open("firefox", "Example"))
            .expect("open");
        store
            .transition(WallTs(100), sample_open("kitty", "zsh"))
            .expect("close the first interval, open the second");
        store
            .close_only(WallTs(200))
            .expect("close the second interval, leaving nothing open");

        let rows = store
            .clipped_intervals(WallTs(100), WallTs(200))
            .expect("clipped query succeeds");

        assert_eq!(
            rows.len(),
            1,
            "the [0,100) interval ends exactly at :from and must be excluded; \
             only the [100,200) interval overlaps the queried [100,200) range"
        );
        assert_eq!(rows[0].start, WallTs(100));
        assert_eq!(rows[0].end, WallTs(200));
    }

    // ── prune (RF-13, RF-52) and its VACUUM phase (D-11) ────────────────────
    //
    // These need a real file, not `:memory:`: `VACUUM` under WAL and
    // cross-connection contention (task 4.9) are exactly the behaviour D-11
    // says was reasoned, not verified against SQLite documentation in that
    // session, and is therefore specified as a test rather than a claim.

    fn open_file_backed_store(label: &str) -> (Store, ScratchDir, PathBuf) {
        let scratch = ScratchDir::new(label);
        let db_path = scratch.path().join("xwindowlog.db");
        let store = Store::open(&db_path).expect("store opens against a fresh path");
        (store, scratch, db_path)
    }

    fn seed_closed_interval(store: &mut Store, start: i64, end: i64) {
        store
            .open_only(WallTs(start), sample_open("firefox", "Example"))
            .expect("open seed interval");
        store.close_only(WallTs(end)).expect("close seed interval");
    }

    #[test]
    fn prune_deletes_old_intervals_but_preserves_the_open_one() {
        let (mut store, _scratch, _db_path) = open_file_backed_store("prune-preserve-open");
        seed_closed_interval(&mut store, 0, 100);
        store
            .open_only(WallTs(100), sample_open("kitty", "zsh"))
            .expect("leave one interval open");

        let deleted = store
            .prune_delete(WallTs(1_000))
            .expect("delete phase succeeds");

        assert_eq!(
            deleted, 1,
            "exactly the one closed interval older than cutoff is deleted"
        );
        assert_eq!(
            open_interval_count(&store),
            1,
            "the open interval must never be deleted regardless of its age"
        );
    }

    #[test]
    fn prune_preserves_rules_and_projects() {
        let (mut store, _scratch, _db_path) = open_file_backed_store("prune-preserve-rules");
        seed_closed_interval(&mut store, 0, 100);
        store
            .conn
            .execute("INSERT INTO projects (id, name) VALUES (1, 'Work')", [])
            .expect("seed a project");
        store
            .conn
            .execute(
                "INSERT INTO rules \
                 (id, app, pattern, project, status, origin, created, updated, confirmed) \
                 VALUES (1, NULL, 'firefox', 1, 'confirmed', 'manual', 0, 0, 0)",
                [],
            )
            .expect("seed a rule");

        store
            .prune_delete(WallTs(1_000))
            .expect("delete phase succeeds, deleting the only referencing interval");

        let project_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        let rule_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM rules", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            project_count, 1,
            "projects must survive pruning its intervals"
        );
        assert_eq!(rule_count, 1, "rules must survive pruning its intervals");
    }

    #[test]
    fn prune_shrinks_the_file_via_vacuum() {
        let (mut store, _scratch, db_path) = open_file_backed_store("prune-vacuum-shrink");
        for i in 0..500i64 {
            seed_closed_interval(&mut store, i * 10, i * 10 + 5);
        }
        let size_before = std::fs::metadata(&db_path).unwrap().len();

        let outcome = store.prune(WallTs(10_000)).expect("prune succeeds");

        assert_eq!(outcome.deleted_intervals, 500);
        assert_eq!(outcome.vacuum, VacuumOutcome::Vacuumed);
        let size_after = std::fs::metadata(&db_path).unwrap().len();
        assert!(
            size_after < size_before,
            "VACUUM must shrink the file: before={size_before} after={size_after}"
        );
    }

    // Not tested here: RF-13's "prune never runs from within the daemon"
    // scenario. That is an architectural fact about `main.rs`'s composition
    // (design §4: "main.rs only wires Store::open() in at composition time,
    // Phase 15"), not something `store.rs` alone can exercise — there is no
    // daemon entry point yet to prove never calls this. Phase 13's REFACTOR
    // task explicitly re-checks this once the control socket exists, and
    // task 4.13 below greps the crate for any other write path into
    // `intervals` in the meantime.

    // ── D-11: VACUUM contention (tests/prune_contention.rs's intent, folded
    // into this module since there is no separate integration-test harness
    // yet) ───────────────────────────────────────────────────────────────
    //
    // Held write locks are simulated with a second `rusqlite::Connection` to
    // the same file, in the SAME thread as the `Store` under test:
    // `BEGIN IMMEDIATE` without a matching `COMMIT` holds the lock exactly
    // as a real concurrent writer would, and — because SQLite connections
    // are independent of OS threads — releasing it from inside the
    // injected `sleep_fn` callback lets these tests be fully synchronous
    // and deterministic, with no real background thread and no timing race.

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn vacuum_succeeds_on_the_first_attempt_with_a_second_connection_idle() {
        let (mut store, _scratch, db_path) = open_file_backed_store("vacuum-idle");
        seed_closed_interval(&mut store, 0, 100);
        store
            .prune_delete(WallTs(1_000))
            .expect("delete phase commits before VACUUM is attempted");

        // A second, genuinely idle connection to the same file: opened, but
        // holding no transaction.
        let idle_conn = Connection::open(&db_path).expect("second connection opens");
        configure_connection(&idle_conn).expect("configure the second connection");

        let attempts = AtomicUsize::new(0);
        let outcome = store
            .vacuum_with_retry_configured(20, &|_backoff| {
                attempts.fetch_add(1, Ordering::SeqCst);
            })
            .expect("VACUUM itself must not error");

        assert_eq!(outcome, VacuumOutcome::Vacuumed);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "an uncontended VACUUM must succeed on the first attempt"
        );
    }

    #[test]
    fn vacuum_retries_and_succeeds_once_the_concurrent_writer_commits() {
        let (mut store, _scratch, db_path) = open_file_backed_store("vacuum-retry-success");
        seed_closed_interval(&mut store, 0, 100);
        store
            .prune_delete(WallTs(1_000))
            .expect("delete phase commits before VACUUM is attempted");

        let writer_conn = Connection::open(&db_path).expect("writer connection opens");
        configure_connection(&writer_conn).expect("configure the writer connection");
        writer_conn
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("writer takes the write lock, contending with VACUUM");

        let attempts = AtomicUsize::new(0);
        let outcome = store
            .vacuum_with_retry_configured(20, &|_backoff| {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 2 {
                    // Release the lock only after the first attempt has
                    // already run (and failed) — this exercises the retry
                    // ladder itself, not a lucky race where the lock
                    // happened to already be free.
                    writer_conn
                        .execute_batch("COMMIT;")
                        .expect("writer releases the write lock");
                }
            })
            .expect("VACUUM itself must not error");

        assert_eq!(outcome, VacuumOutcome::Vacuumed);
        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "must have retried at least once before succeeding once the writer \
             committed, got {} attempt(s)",
            attempts.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn vacuum_against_a_permanently_busy_database_exhausts_its_retries() {
        let (mut store, _scratch, db_path) = open_file_backed_store("vacuum-exhausted");
        seed_closed_interval(&mut store, 0, 100);
        let deleted = store
            .prune_delete(WallTs(1_000))
            .expect("delete phase commits before VACUUM is attempted");
        assert_eq!(deleted, 1);

        let blocker_conn = Connection::open(&db_path).expect("blocker connection opens");
        configure_connection(&blocker_conn).expect("configure the blocker connection");
        blocker_conn
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("blocker takes the write lock and never releases it in this test");

        let attempts = AtomicUsize::new(0);
        let outcome = store
            .vacuum_with_retry_configured(20, &|_backoff| {
                attempts.fetch_add(1, Ordering::SeqCst);
            })
            .expect("exhausting the retry budget is a typed outcome, not an Err");

        assert_eq!(outcome, VacuumOutcome::Exhausted);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "exactly 3 attempts, per D-11"
        );
        assert_eq!(
            vacuum_exhausted_message(deleted),
            "xwindowlog: deleted 1 intervals (data is committed and durable).\n\
             xwindowlog: the database file was NOT compacted: another process holds it open.\n\
             xwindowlog:   systemctl --user stop xwindowlog.service\n\
             xwindowlog:   xwindowlog prune --vacuum-only\n\
             xwindowlog:   systemctl --user start xwindowlog.service"
        );

        // The delete phase's commit must be durable and unaffected by
        // VACUUM's failure — read via a THIRD, fresh connection, proving
        // this is on disk, not just visible to `store`'s own connection.
        blocker_conn
            .execute_batch("ROLLBACK;")
            .expect("release the blocker's lock so the count can be read");
        let reader = Connection::open(&db_path).expect("a fresh connection can read the file");
        let remaining: i64 = reader
            .query_row("SELECT COUNT(*) FROM intervals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            remaining, 0,
            "the delete is durable even though VACUUM never compacted the file"
        );
    }

    // ── forget (RF-53) ───────────────────────────────────────────────────

    #[test]
    fn forget_range_physically_deletes_matching_rows_cleans_orphans_and_vacuums() {
        let (mut store, _scratch, _db_path) = open_file_backed_store("forget-range");
        store
            .open_only(WallTs(1_532), sample_open("slack", "DM"))
            .expect("seed the interval to be forgotten");
        store
            .close_only(WallTs(1_540))
            .expect("close it, inside the range to forget");
        store
            .open_only(WallTs(9_000), sample_open("firefox", "Example"))
            .expect("seed a second interval");
        store
            .close_only(WallTs(9_100))
            .expect("close it, outside the range — must survive");

        let outcome = store
            .forget_range(WallTs(1_532), WallTs(1_540))
            .expect("forget_range succeeds");

        assert_eq!(
            outcome.deleted_intervals, 1,
            "only the interval overlapping [1532, 1540) is deleted"
        );
        assert_eq!(outcome.vacuum, VacuumOutcome::Vacuumed);

        let remaining_starts: Vec<i64> = {
            let mut stmt = store
                .conn
                .prepare("SELECT start FROM intervals ORDER BY start")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            remaining_starts,
            vec![9_000],
            "the surviving interval outside the range must be untouched"
        );

        let slack_orphaned: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM apps WHERE app_id = 'slack'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            slack_orphaned, 0,
            "the app_id dictionary row for the forgotten interval's app must be \
             cleaned up once nothing references it"
        );
    }

    #[test]
    fn forget_window_deletes_the_single_interval_by_id() {
        let (mut store, _scratch, _db_path) = open_file_backed_store("forget-window");
        store
            .open_only(WallTs(0), sample_open("firefox", "Example"))
            .expect("seed the interval");
        store.close_only(WallTs(10)).expect("close it");
        let id: i64 = store
            .conn
            .query_row("SELECT id FROM intervals", [], |r| r.get(0))
            .unwrap();

        let outcome = store.forget_window(id).expect("forget_window succeeds");

        assert_eq!(outcome.deleted_intervals, 1);
        assert_eq!(outcome.vacuum, VacuumOutcome::Vacuumed);
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM intervals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "the single row `id` referenced must be gone");
    }

    // Not tested here, for the same reason as prune's daemon-invocation
    // scenario above: RF-53's "forget without --yes requires confirmation"
    // is an interactive-terminal concern that belongs to `main.rs`'s CLI
    // layer (Phase 15), which does not exist yet. `store.rs`'s `forget_range`
    // / `forget_window` are the already-confirmed destructive operation;
    // the confirmation prompt is main.rs's job to build in front of them.
}
