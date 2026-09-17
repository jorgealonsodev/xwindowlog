//! RF-11 schema + sentinels; `transition`/`close_only`/`open_only` (D-1); RF-35 migrations with
//! Online-Backup-API backup; RF-36 recovery; clipping CTE; `prune` (RF-13, D-11) and `forget` (RF-53).
//!
//! Phase 3 (this file, part 1/2) covers schema, permissions, migrations and
//! startup recovery. Phase 4 adds `transition`/`close_only`/`open_only`,
//! clipping, `prune` and `forget` (design §4 File Changes, tasks.md Phase 4).
#![allow(
    dead_code,
    reason = "store.rs lands ahead of its consumers per design §8: Phase 4 \
              (this same file) adds transition/prune/forget, and main.rs \
              only wires Store::open() in at composition time (Phase 15)"
)]

use std::path::{Path, PathBuf};

use rusqlite::Connection;

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
    pub closed_stale: Option<crate::clock::WallTs>,
    pub extra_open_rows: u32,
}

impl Store {
    /// RF-36 startup recovery, run once before the daemon accepts any X11
    /// events. Queries for intervals with `"end" IS NULL`; closes whatever it
    /// finds at `now` (zero-duration for every row except the most recently
    /// opened one, if there was more than one — an invariant violation that
    /// gets logged); and always leaves exactly one `unknown` interval open
    /// from `now`, ready for the first real event to transition out of.
    pub fn recover_on_startup(
        &mut self,
        now: crate::clock::WallTs,
    ) -> Result<Recovery, StoreError> {
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
}
