//! The embedded schema migrations and the code that applies them.
//!
//! To change the schema, append a [`Migration`] to [`MIGRATIONS`] with the
//! next version number. Never edit or reorder one that has shipped: a
//! database records each version it has applied in `schema_migrations`,
//! and opening it only runs the ones after its highest. The tests check
//! that versions count up from 1 and that every prefix migrates to the
//! latest.
//!
//! Pending migrations run in one transaction, so a failure leaves the
//! database at its previous version. A database whose highest version is
//! above the latest here was written by a newer `alchemyd` and is refused.

use rusqlite::{Connection, TransactionBehavior};

/// One schema change.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Position in [`MIGRATIONS`], counting up from 1.
    pub version: i64,
    /// Short description, recorded alongside the version.
    pub name: &'static str,
    /// SQL run as a batch inside the migration transaction.
    pub sql: &'static str,
}

/// Every migration, oldest first.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "settings",
        sql: "CREATE TABLE settings (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
          ) STRICT;",
    },
    Migration {
        version: 2,
        name: "metrics",
        sql: "CREATE TABLE metrics (
            step INTEGER NOT NULL,
            ts   INTEGER NOT NULL,
            data TEXT NOT NULL,
            PRIMARY KEY (step, ts)
          ) STRICT, WITHOUT ROWID;",
    },
    Migration {
        version: 3,
        name: "intents",
        sql: "CREATE TABLE intents (
            project TEXT PRIMARY KEY NOT NULL,
            action  TEXT NOT NULL,
            at      INTEGER NOT NULL
          ) STRICT;",
    },
];

/// The schema version this build migrates databases to.
pub fn latest() -> i64 {
    latest_of(MIGRATIONS)
}

fn latest_of(migrations: &[Migration]) -> i64 {
    migrations.last().map_or(0, |m| m.version)
}

/// Why [`apply`] refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The database is at a version above the latest migration.
    #[error("schema version {found} is newer than {supported}")]
    TooNew {
        /// The highest version recorded.
        found: i64,
        /// The latest version known.
        supported: i64,
    },
    /// A recorded version is missing from, or named differently in, the
    /// known migrations.
    #[error("{0}")]
    History(String),
    /// SQLite failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Bring `conn` up to the latest of `migrations`, returning the version it
/// was at before.
pub fn apply(conn: &mut Connection, migrations: &[Migration]) -> Result<i64, MigrateError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY NOT NULL,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ) STRICT;",
    )?;
    let applied = applied(&tx)?;
    let current = check_history(&applied, migrations)?;
    for migration in migrations.iter().filter(|m| m.version > current) {
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            (migration.version, migration.name),
        )?;
    }
    tx.commit()?;
    Ok(current)
}

fn applied(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT version, name FROM schema_migrations ORDER BY version")?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

fn check_history(applied: &[(i64, String)], migrations: &[Migration]) -> Result<i64, MigrateError> {
    let supported = latest_of(migrations);
    let found = applied.last().map_or(0, |(version, _)| *version);
    if found > supported {
        return Err(MigrateError::TooNew { found, supported });
    }
    for (index, (version, name)) in applied.iter().enumerate() {
        match migrations.get(index) {
            Some(m) if m.version == *version && m.name == name => {}
            _ => {
                return Err(MigrateError::History(format!(
                    "recorded migration {version} ({name}) does not match this build"
                )));
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn current(conn: &Connection) -> i64 {
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .unwrap()
        .unwrap_or(0)
    }

    const FAKE: &[Migration] = &[
        Migration {
            version: 1,
            name: "one",
            sql: "CREATE TABLE a (x INTEGER);",
        },
        Migration {
            version: 2,
            name: "two",
            sql: "ALTER TABLE a ADD COLUMN y INTEGER;",
        },
        Migration {
            version: 3,
            name: "three",
            sql: "CREATE TABLE b (z INTEGER);",
        },
    ];

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn versions_count_up_from_one() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                i64::try_from(index).unwrap() + 1,
                "{}",
                migration.name
            );
        }
    }

    #[test]
    fn real_migrations_apply_from_every_prior_version() {
        for start in 0..=MIGRATIONS.len() {
            let mut conn = Connection::open_in_memory().unwrap();
            apply(&mut conn, &MIGRATIONS[..start]).unwrap();
            let before = apply(&mut conn, MIGRATIONS).unwrap();
            assert_eq!(before, i64::try_from(start).unwrap());
            assert_eq!(current(&conn), latest());
        }
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, MIGRATIONS).unwrap();
        let tables = tables(&conn);
        for table in ["settings", "metrics", "intents"] {
            assert!(tables.contains(&table.to_owned()), "{table}");
        }
    }

    #[test]
    fn fake_migrations_apply_from_every_prior_version() {
        for start in 0..=FAKE.len() {
            let mut conn = Connection::open_in_memory().unwrap();
            apply(&mut conn, &FAKE[..start]).unwrap();
            apply(&mut conn, FAKE).unwrap();
            assert_eq!(current(&conn), 3, "from {start}");
            assert_eq!(
                tables(&conn),
                ["a", "b", "schema_migrations"],
                "from {start}"
            );
            conn.execute("INSERT INTO a (x, y) VALUES (1, 2)", [])
                .unwrap();
        }
    }

    #[test]
    fn reapplying_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(apply(&mut conn, FAKE).unwrap(), 0);
        assert_eq!(apply(&mut conn, FAKE).unwrap(), 3);
    }

    #[test]
    fn failed_migration_rolls_back_every_pending_one() {
        let broken = [
            FAKE[0],
            Migration {
                version: 2,
                name: "broken",
                sql: "CREATE TABLE c (x INTEGER); NOT SQL;",
            },
        ];
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            apply(&mut conn, &broken),
            Err(MigrateError::Sqlite(_))
        ));
        assert_eq!(tables(&conn), Vec::<String>::new());
    }

    #[test]
    fn refuses_newer_or_mismatched_history() {
        type Check = fn(&MigrateError) -> bool;
        let cases: [(&str, &[Migration], &[Migration], Check); 2] = [
            ("newer", FAKE, &FAKE[..1], |e| {
                matches!(
                    e,
                    MigrateError::TooNew {
                        found: 3,
                        supported: 1
                    }
                )
            }),
            (
                "renamed",
                &FAKE[..1],
                &[Migration {
                    version: 1,
                    name: "other",
                    sql: "",
                }],
                |e| matches!(e, MigrateError::History(_)),
            ),
        ];
        for (name, first, then, want) in cases {
            let mut conn = Connection::open_in_memory().unwrap();
            apply(&mut conn, first).unwrap();
            let err = apply(&mut conn, then).unwrap_err();
            assert!(want(&err), "{name}: {err:?}");
            assert_eq!(current(&conn), latest_of(first), "{name}");
        }
    }
}
