//! Transactional, versioned schema migrations.
//!
//! Migration files live in the repository-level `migrations/` directory and
//! are embedded at compile time. Each migration runs inside its own
//! transaction together with the `user_version` bump, so a crash can never
//! leave a half-applied version behind.

use rusqlite::Connection;

use crate::error::StoreError;

/// `(name, sql)` pairs, ordered by version. Version N = index N-1.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_init",
        include_str!("../../../migrations/0001_init.sql"),
    ),
    (
        "0002_process_identity",
        include_str!("../../../migrations/0002_process_identity.sql"),
    ),
];

pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

pub fn current_version(conn: &Connection) -> Result<i64, StoreError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(v)
}

/// Apply all pending migrations. Idempotent.
pub fn run(conn: &Connection) -> Result<(), StoreError> {
    let latest = latest_version();
    let current = current_version(conn)?;
    if current > latest {
        return Err(StoreError::SchemaNewer {
            db: current,
            binary: latest,
        });
    }
    for (idx, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(migration = name, version, "applied migration");
    }
    Ok(())
}
