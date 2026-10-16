//! Forge SQLite persistence.
//!
//! All database work happens on a **dedicated worker thread** that owns the
//! single write connection ([`Store`]). Async callers submit closures over a
//! bounded channel, so no SQLite call ever blocks a Tokio executor thread and
//! outstanding work is naturally back-pressured.
//!
//! Synchronous, one-shot users (`forge init`, `forge doctor`) can open a
//! [`DirectDb`] instead — this is only safe while no daemon is running on the
//! same database.

pub mod error;
pub mod migrations;
pub mod repo;

pub use error::StoreError;

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::mpsc;

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

enum Backing {
    File(PathBuf),
    Memory,
}

/// Handle to the database worker thread.
#[derive(Debug)]
pub struct Store {
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
    path: Option<PathBuf>,
}

impl Store {
    /// Open (creating if needed) the database at `path` and run migrations.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Self::spawn(Backing::File(path))
    }

    /// In-memory database (tests).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::spawn(Backing::Memory)
    }

    fn spawn(backing: Backing) -> Result<Self, StoreError> {
        let (tx, mut rx) = mpsc::channel::<Job>(1024);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), StoreError>>();
        let thread_name = match &backing {
            Backing::File(p) => format!("forge-store:{}", p.display()),
            Backing::Memory => "forge-store:memory".to_string(),
        };
        let path = match &backing {
            Backing::File(p) => Some(p.clone()),
            Backing::Memory => None,
        };
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut conn = match open_connection(&backing) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return; // opener gave up
                }
                while let Some(job) = rx.blocking_recv() {
                    job(&mut conn);
                }
                // Graceful close: flush the WAL back into the main database
                // file so that external tools see a complete database.
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            })
            .map_err(|source| StoreError::Io {
                path: PathBuf::from("<store worker thread>"),
                source,
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(_) => return Err(StoreError::WorkerGone),
        }

        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
            path,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run `f` on the database worker thread and await its result.
    pub async fn call<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let tx = self.tx.as_ref().ok_or(StoreError::WorkerGone)?.clone();
        let (res_tx, res_rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = res_tx.send(f(conn));
        });
        tx.send(job).await.map_err(|_| StoreError::WorkerGone)?;
        match res_rx.await {
            Ok(result) => result,
            Err(_) => Err(StoreError::WorkerGone),
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.tx.take(); // close the channel: worker drains and exits
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn open_connection(backing: &Backing) -> Result<Connection, StoreError> {
    let conn = match backing {
        Backing::File(path) => Connection::open(path)?,
        Backing::Memory => Connection::open_in_memory()?,
    };
    apply_pragmas(&conn)?;
    migrations::run(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

/// Synchronous, single-connection database handle.
///
/// Use only for one-shot CLI operations (`forge init`, `forge doctor`) when
/// the daemon is not running — the daemon is the sole writer otherwise.
pub struct DirectDb {
    conn: Connection,
}

impl DirectDb {
    /// Open read-write, creating the file and running migrations.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrations::run(&conn)?;
        Ok(Self { conn })
    }

    /// Open read-only (no migrations, no writes).
    pub fn open_readonly(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn conn(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn get(&self) -> &Connection {
        &self.conn
    }
}

/// Diagnostics about a database file, used by `forge doctor`.
#[derive(Debug, Clone)]
pub struct DbProbe {
    pub exists: bool,
    pub version: Option<i64>,
    pub latest: i64,
    pub integrity: String,
    pub journal_mode: Option<String>,
    pub size_bytes: u64,
}

pub fn probe_database(path: &Path) -> DbProbe {
    let exists = path.exists();
    let mut probe = DbProbe {
        exists,
        version: None,
        latest: migrations::latest_version(),
        integrity: if exists {
            "unknown".into()
        } else {
            "not initialized".into()
        },
        journal_mode: None,
        size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    };
    if !exists {
        return probe;
    }
    match DirectDb::open_readonly(path) {
        Ok(db) => {
            let conn = db.get();
            probe.version = migrations::current_version(conn).ok();
            probe.integrity = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap_or_else(|e| format!("error: {e}"));
            probe.journal_mode = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .ok();
        }
        Err(e) => {
            probe.integrity = format!("cannot open: {e}");
        }
    }
    probe
}
