//! SQLite backend: one database file per app, embedded in the server.
//!
//! Queries are function calls (microseconds, no network, no connection
//! limits). Each worker keeps its own connections to the file; WAL mode
//! lets readers run concurrently with the single writer. Statements run
//! inline on the worker thread: they are short, and a long one is
//! interrupted at the deadline.
//!
//! SQLite allows one writer at a time. Rather than letting connections race
//! for its lock and retry on a timer, writers in this process queue on an
//! async lock per database file: waiting never blocks a worker thread, and
//! the lock is handed over in order. (A writer outside the process, such as
//! the sqlite3 shell, is still handled by retrying after an async sleep.)

use super::{MAX_ROWS, TX_ABORTED};
use crate::bindings::sql::{Error, QueryResult, Value};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, ErrorCode, OpenFlags, params_from_iter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::time::{Instant, timeout_at};
use wasmtime::{Result, format_err};

/// Statements cached per connection.
const STATEMENT_CACHE: usize = 128;
/// Rows a cursor's reader thread may get ahead of the guest.
const CURSOR_BUFFER: usize = 1024;

/// One worker's connections to one database file.
pub struct Pool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
    /// Shared by every worker's pool for this file.
    writer: Arc<AsyncMutex<()>>,
}

/// The write lock for a database file, shared across workers.
fn writer_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    LOCKS.get_or_init(Default::default).lock().unwrap().entry(key).or_default().clone()
}

impl Pool {
    pub fn new(path: &Path, size: usize) -> Arc<Self> {
        Arc::new(Self {
            path: path.to_owned(),
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(size.max(1))),
            writer: writer_lock(path),
        })
    }

    async fn lock_writer(&self, deadline: Instant) -> Result<OwnedMutexGuard<()>, Error> {
        timeout_at(deadline, self.writer.clone().lock_owned()).await.map_err(|_| Error::Timeout)
    }

    /// Takes the write lock if `statement` writes, before running it.
    async fn statement_lock(&self, conn: &mut Conn, deadline: Instant, statement: &str) -> Result<Option<OwnedMutexGuard<()>>, Error> {
        let readonly = conn.prepare_cached(&numbered_params(statement)).map_err(sql_error)?.readonly();
        Ok(if readonly { None } else { Some(self.lock_writer(deadline).await?) })
    }

    /// Opens a connection; creates the file (and its directory) if needed.
    fn open(path: &Path) -> rusqlite::Result<Connection> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // WAL: readers don't block the writer and vice versa. NORMAL sync is
        // durable across app crashes; a power loss may drop the last commits.
        // Checkpoints run in the background (`spawn_checkpointer`), not inside
        // whichever commit happens to fill the log.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 0;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 0;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -16384;",
        )?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
        Ok(conn)
    }

    /// Starts the background checkpointer for this file (once per server).
    pub fn start_checkpointer(&self) {
        spawn_checkpointer(self.path.clone());
    }

    pub fn check(&self) -> Result<String> {
        let conn = Self::open(&self.path).map_err(|e| format_err!("{}: {e}", self.path.display()))?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).map_err(|e| format_err!("{e}"))?;
        Ok(format!("sqlite, {}, journal_mode={mode}", self.path.display()))
    }

    /// Runs a schema file (idempotent statements) at startup.
    pub fn init(&self, sql: &str) -> Result<()> {
        let conn = Self::open(&self.path).map_err(|e| format_err!("{}: {e}", self.path.display()))?;
        conn.execute_batch(sql).map_err(|e| format_err!("{e}"))
    }

    async fn connect(self: &Arc<Self>, deadline: Instant) -> Result<Conn, Error> {
        let permit = timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| Error::Unavailable(e.to_string()))?;
        let idle = self.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(conn) => conn,
            None => Self::open(&self.path).map_err(|e| Error::Unavailable(format!("{}: {e}", self.path.display())))?,
        };
        Ok(Conn { conn: Some(conn), pool: self.clone(), _permit: permit })
    }

    pub async fn query(self: &Arc<Self>, deadline: Instant, statement: String, params: Vec<Value>) -> Result<QueryResult, Error> {
        let mut conn = self.connect(deadline).await?;
        let _writer = self.statement_lock(&mut conn, deadline, &statement).await?;
        retry_busy(&mut conn, deadline, |c| query_rows(c, &statement, &params)).await
    }

    pub async fn execute(self: &Arc<Self>, deadline: Instant, statement: String, params: Vec<Value>) -> Result<u64, Error> {
        let mut conn = self.connect(deadline).await?;
        let _writer = self.statement_lock(&mut conn, deadline, &statement).await?;
        retry_busy(&mut conn, deadline, |c| execute(c, &statement, &params)).await
    }

    pub async fn begin(self: &Arc<Self>, deadline: Instant) -> Result<Tx, Error> {
        let mut conn = self.connect(deadline).await?;
        // The transaction holds the write lock until it ends, so its
        // statements never wait on another writer. (Read-only transactions
        // serialize too; fine for short transactions.)
        let writer = self.lock_writer(deadline).await?;
        retry_busy(&mut conn, deadline, |c| c.execute_batch("BEGIN IMMEDIATE").map_err(Failure::Sql)).await?;
        Ok(Tx { conn: Some(conn), _writer: writer })
    }

    pub async fn open_cursor(self: &Arc<Self>, deadline: Instant, statement: String, params: Vec<Value>) -> Result<Cursor, Error> {
        let conn = self.connect(deadline).await?;
        let (columns_tx, columns_rx) = oneshot::channel();
        let (rows_tx, rows_rx) = mpsc::channel(CURSOR_BUFFER);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        // The rows borrow the connection, so the whole read happens on a
        // blocking thread that hands rows over through a bounded channel.
        tokio::task::spawn_blocking(move || {
            let conn = conn;
            let _ = conn.progress_handler(10_000, Some(move || cancelled.load(Ordering::Relaxed)));
            let mut stmt = match conn.prepare(&numbered_params(&statement)) {
                Ok(stmt) => stmt,
                Err(e) => return drop(columns_tx.send(Err(sql_error(e)))),
            };
            let columns = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = match stmt.query(params_from_iter(params.iter().map(to_sql))) {
                Ok(rows) => rows,
                Err(e) => return drop(columns_tx.send(Err(sql_error(e)))),
            };
            if columns_tx.send(Ok(columns)).is_err() {
                return;
            }
            loop {
                let item = match rows.next() {
                    Ok(Some(row)) => Ok(convert_row(row)),
                    Ok(None) => break,
                    Err(e) => Err(sql_error(e)),
                };
                let failed = item.is_err();
                if rows_tx.blocking_send(item).is_err() || failed {
                    break; // cursor dropped, or error delivered
                }
            }
            drop(rows);
            drop(stmt);
            let _ = conn.progress_handler(0, None::<fn() -> bool>);
        });
        let columns = timeout_at(deadline, columns_rx).await.map_err(|_| {
            cancel.store(true, Ordering::Relaxed);
            Error::Timeout
        })?;
        let columns = columns.map_err(|_| Error::Unavailable("cursor thread failed".into()))??;
        Ok(Cursor { rows: Some(rows_rx), columns, cancel })
    }
}

/// A pooled connection; goes back to its pool when dropped.
struct Conn {
    conn: Option<Connection>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
}

impl std::ops::Deref for Conn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().unwrap()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if conn.is_autocommit() {
                self.pool.idle.lock().unwrap().push(conn);
            }
            // Otherwise it's mid-transaction in an unknown state: close it.
        }
    }
}

/// Why a statement failed.
enum Failure {
    Sql(rusqlite::Error),
    Value(Error),
}

/// Runs `op`, retrying while another connection holds the write lock, and
/// interrupting it at the deadline. (Takes `&mut`: `Connection` isn't `Sync`,
/// so a shared reference held across an await would make the future `!Send`.)
async fn retry_busy<T>(conn: &mut Connection, deadline: Instant, mut op: impl FnMut(&Connection) -> Result<T, Failure>) -> Result<T, Error> {
    let std_deadline = deadline.into_std();
    // Without the handler a slow statement couldn't be stopped; refuse to run.
    conn.progress_handler(10_000, Some(move || std::time::Instant::now() > std_deadline))
        .map_err(|e| Error::Unavailable(format!("progress handler: {e}")))?;
    let mut backoff = Duration::from_micros(100);
    let result = loop {
        match op(&*conn) {
            Ok(value) => break Ok(value),
            Err(Failure::Sql(e)) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
                if Instant::now() + backoff > deadline {
                    break Err(Error::Timeout);
                }
                // Jitter keeps waiting writers from retrying in lockstep.
                let random = std::hash::BuildHasher::build_hasher(&std::collections::hash_map::RandomState::new());
                let jitter = Duration::from_micros(std::hash::Hasher::finish(&random) % 100);
                tokio::time::sleep(backoff + jitter).await;
                backoff = (backoff * 3 / 2).min(Duration::from_millis(5));
            }
            Err(Failure::Sql(e)) => break Err(sql_error(e)),
            Err(Failure::Value(e)) => break Err(e),
        }
    };
    let _ = conn.progress_handler(0, None::<fn() -> bool>);
    result
}

/// Moves WAL content into the database file in the background. PASSIVE
/// checkpoints never block readers or writers; if the log still grows past
/// a limit (long readers), a TRUNCATE checkpoint resets it.
pub fn spawn_checkpointer(path: PathBuf) {
    const INTERVAL: Duration = Duration::from_millis(250);
    const MAX_WAL_PAGES: i64 = 16_384; // 64 MiB at 4 KiB pages
    let _ = std::thread::Builder::new().name(format!("checkpoint:{}", path.display())).spawn(move || {
        let Ok(conn) = Connection::open(&path) else { return };
        let _ = conn.busy_timeout(Duration::from_millis(100));
        loop {
            std::thread::sleep(INTERVAL);
            let wal_pages: i64 = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1)).unwrap_or(0);
            if wal_pages > MAX_WAL_PAGES {
                let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
            }
        }
    });
}

fn sql_error(e: rusqlite::Error) -> Error {
    match e.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => Error::Timeout,
        Some(ErrorCode::DatabaseBusy) => Error::Timeout,
        _ => Error::Query(e.to_string()),
    }
}

/// Apps write Postgres-style `$1`, `$2`. In SQLite those are *named*
/// parameters, numbered by first appearance, so `... $2 ... $1` would bind
/// backwards; `?1`, `?2` are SQLite's numbered form. Rewrites `$N` outside
/// string literals, quoted identifiers and comments.
fn numbered_params(sql: &str) -> std::borrow::Cow<'_, str> {
    if !sql.contains('$') {
        return sql.into();
    }
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let end = match c {
            b'\'' | b'"' => sql[i + 1..].find(c as char).map_or(bytes.len(), |n| i + 1 + n + 1),
            b'-' if bytes.get(i + 1) == Some(&b'-') => sql[i..].find('\n').map_or(bytes.len(), |n| i + n),
            b'/' if bytes.get(i + 1) == Some(&b'*') => sql[i + 2..].find("*/").map_or(bytes.len(), |n| i + 2 + n + 2),
            b'$' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                out.push('?');
                i += 1;
                continue;
            }
            _ => i + sql[i..].chars().next().unwrap().len_utf8(),
        };
        out.push_str(&sql[i..end]);
        i = end;
    }
    out.into()
}

fn query_rows(conn: &Connection, statement: &str, params: &[Value]) -> Result<QueryResult, Failure> {
    let mut stmt = conn.prepare_cached(&numbered_params(statement)).map_err(Failure::Sql)?;
    let columns = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows = stmt.query(params_from_iter(params.iter().map(to_sql))).map_err(Failure::Sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(Failure::Sql)? {
        if out.len() == MAX_ROWS {
            return Err(Failure::Value(Error::TooManyRows(MAX_ROWS as u32)));
        }
        out.push(convert_row(row));
    }
    Ok(QueryResult { columns, rows: out })
}

fn execute(conn: &Connection, statement: &str, params: &[Value]) -> Result<u64, Failure> {
    let mut stmt = conn.prepare_cached(&numbered_params(statement)).map_err(Failure::Sql)?;
    stmt.execute(params_from_iter(params.iter().map(to_sql))).map(|n| n as u64).map_err(Failure::Sql)
}

fn to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Int(i) => SqlValue::Integer(*i),
        Value::Float(f) => SqlValue::Real(*f),
        Value::Text(s) => SqlValue::Text(s.clone()),
        Value::Boolean(b) => SqlValue::Integer(*b as i64),
        Value::Blob(b) => SqlValue::Blob(b.clone()),
    }
}

fn convert_row(row: &rusqlite::Row) -> Vec<Value> {
    (0..row.as_ref().column_count())
        .map(|i| match row.get_ref(i) {
            Ok(ValueRef::Null) | Err(_) => Value::Null,
            Ok(ValueRef::Integer(i)) => Value::Int(i),
            Ok(ValueRef::Real(f)) => Value::Float(f),
            Ok(ValueRef::Text(t)) => match std::str::from_utf8(t) {
                Ok(s) => Value::Text(s.to_string()),
                Err(_) => Value::Blob(t.to_vec()),
            },
            Ok(ValueRef::Blob(b)) => Value::Blob(b.to_vec()),
        })
        .collect()
}

/// A connection inside `BEGIN IMMEDIATE`. Dropping it rolls back.
pub struct Tx {
    /// `None` once a call timed out (the transaction was rolled back).
    conn: Option<Conn>,
    /// Released after `Drop` has rolled back (fields drop after `drop` runs).
    _writer: OwnedMutexGuard<()>,
}

impl Drop for Tx {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Local and fast, so done inline; the connection then goes back.
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
}

impl Tx {
    async fn run<T>(&mut self, deadline: Instant, op: impl FnMut(&Connection) -> Result<T, Failure>) -> Result<T, Error> {
        let Some(conn) = self.conn.as_mut() else {
            return Err(Error::Query(TX_ABORTED.into()));
        };
        let result = retry_busy(conn, deadline, op).await;
        if matches!(result, Err(Error::Timeout)) {
            // An interrupted write may have rolled the transaction back
            // already; make sure it's over either way.
            let conn = self.conn.take().unwrap();
            let _ = conn.execute_batch("ROLLBACK");
        }
        result
    }

    pub async fn query(&mut self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<QueryResult, Error> {
        self.run(deadline, |c| query_rows(c, &statement, &params)).await
    }

    pub async fn execute(&mut self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<u64, Error> {
        self.run(deadline, |c| execute(c, &statement, &params)).await
    }

    pub async fn commit(mut self, deadline: Instant) -> Result<(), Error> {
        let result = self.run(deadline, |c| c.execute_batch("COMMIT").map_err(Failure::Sql)).await;
        if result.is_ok() {
            self.conn = None; // committed: back to the pool, no rollback
        }
        result
    }

    pub async fn rollback(self) {
        // `Drop` rolls back.
    }
}

/// Rows streamed from a reader thread.
pub struct Cursor {
    rows: Option<mpsc::Receiver<Result<Vec<Value>, Error>>>,
    pub columns: Vec<String>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // Stops the reader thread even if it's inside a long scan.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Cursor {
    pub async fn next(&mut self, deadline: Instant, max_rows: usize) -> Result<Vec<Vec<Value>>, Error> {
        let Some(rx) = self.rows.as_mut() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(max_rows.min(1024));
        while out.len() < max_rows {
            match timeout_at(deadline, rx.recv()).await {
                Ok(Some(Ok(row))) => out.push(row),
                Ok(Some(Err(e))) => {
                    self.rows = None;
                    return Err(e);
                }
                Ok(None) => {
                    self.rows = None; // exhausted
                    break;
                }
                Err(_) => {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.rows = None;
                    return Err(Error::Timeout);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::numbered_params;

    #[test]
    fn rewrites_only_placeholders() {
        assert_eq!(numbered_params("SELECT 1"), "SELECT 1");
        assert_eq!(
            numbered_params("UPDATE t SET a = a - $2 WHERE id = $1 AND b >= $2"),
            "UPDATE t SET a = a - ?2 WHERE id = ?1 AND b >= ?2"
        );
        assert_eq!(numbered_params("SELECT '$1', \"$2\", $3 -- $4\n, $5 /* $6 */"), "SELECT '$1', \"$2\", ?3 -- $4\n, ?5 /* $6 */");
        assert_eq!(numbered_params("SELECT $a, 'it''s $1', $10"), "SELECT $a, 'it''s $1', ?10");
        assert_eq!(numbered_params("SELECT 'ü $1', $1"), "SELECT 'ü $1', ?1");
    }
}
