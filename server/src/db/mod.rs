//! `platform:sdk/sql`: database access for guests through the host.
//!
//! Instances live for one request, so they can't keep connections; the host
//! does. Guests name databases ("main"); URLs and credentials never enter the
//! sandbox, and each app may only use databases it was granted. Two backends
//! sit behind the same interface:
//!
//! - **SQLite** (`sqlite:PATH`): a file per database, embedded in the server.
//!   Calls take microseconds and need no connections. The default for apps.
//! - **Postgres** (`postgres://...`): per-worker pools, TLS, cancellation.
//!
//! Every call, including waiting for a connection, is bounded by the app's
//! database timeout.

mod postgres;
pub mod sqlite;

pub use postgres::{DbTls, tls};

use crate::app::RequestState;
use crate::bindings::sql::{self, Error, QueryResult, Value};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime::{Result, format_err};

/// Most rows `query` returns, and most rows one `cursor.next` returns.
const MAX_ROWS: usize = 10_000;
const TX_ABORTED: &str = "transaction aborted: an earlier call timed out";

/// A database the server can use.
#[derive(Clone)]
pub struct DbConfig {
    pub name: String,
    /// `sqlite:PATH` or a Postgres URL.
    pub url: String,
    /// SQL to run once at startup, e.g. `CREATE TABLE IF NOT EXISTS ...`.
    pub init: Option<String>,
}

enum Backend {
    Postgres(postgres::DbPool),
    Sqlite(Arc<sqlite::Pool>),
}

/// A database as one call uses it.
enum BackendRef<'a> {
    Postgres(&'a postgres::DbPool),
    Sqlite(Arc<sqlite::Pool>),
}

/// Where apps deployed over the API keep their databases
/// (`<data-dir>/apps`): the database `notes/main` is the SQLite file
/// `apps/notes/db/main.db`. Set once at startup.
pub static MANAGED_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The file of a deployed app's database `app/name`.
pub fn managed_file(real: &str) -> Option<PathBuf> {
    let (app, name) = real.split_once('/')?;
    Some(MANAGED_DIR.get()?.join(app).join("db").join(format!("{name}.db")))
}

/// One worker's connections, keyed by database name.
pub struct Pools {
    by_name: HashMap<String, Backend>,
    /// Deployed apps' databases, opened on first use.
    managed: Mutex<HashMap<String, Arc<sqlite::Pool>>>,
    size_per_db: usize,
}

impl Pools {
    /// Creates the pools without connecting; connections open lazily on the
    /// calling worker's thread.
    pub fn new(dbs: &[DbConfig], size_per_db: usize, tls: &DbTls) -> Result<Self> {
        let mut by_name = HashMap::new();
        for db in dbs {
            let backend = match db.url.strip_prefix("sqlite:") {
                Some(path) => Backend::Sqlite(sqlite::Pool::new(Path::new(path), size_per_db)),
                None => Backend::Postgres(postgres::DbPool::new(&db.name, &db.url, size_per_db, tls)?),
            };
            by_name.insert(db.name.clone(), backend);
        }
        Ok(Self { by_name, managed: Mutex::default(), size_per_db })
    }

    fn get(&self, db: &str) -> Option<BackendRef<'_>> {
        match self.by_name.get(db) {
            Some(Backend::Postgres(p)) => return Some(BackendRef::Postgres(p)),
            Some(Backend::Sqlite(p)) => return Some(BackendRef::Sqlite(p.clone())),
            None => {}
        }
        let file = managed_file(db)?;
        let mut managed = self.managed.lock().unwrap();
        let pool = managed.entry(db.to_string()).or_insert_with(|| sqlite::Pool::new(&file, self.size_per_db));
        Some(BackendRef::Sqlite(pool.clone()))
    }

    /// At startup: runs each database's init SQL, then connects to every
    /// database once to fail fast on bad config. Returns a line per
    /// database and warnings about weak TLS settings.
    pub async fn check(&self, dbs: &[DbConfig]) -> Result<(Vec<String>, Vec<String>)> {
        let mut lines = Vec::new();
        let mut warnings = Vec::new();
        for db in dbs {
            let name = &db.name;
            let backend = &self.by_name[name];
            if let Some(init) = &db.init {
                match backend {
                    Backend::Sqlite(pool) => pool.init(init),
                    Backend::Postgres(pool) => pool.init(init).await,
                }
                .map_err(|e| format_err!("database {name:?}: init: {e}"))?;
            }
            let line = match backend {
                Backend::Sqlite(pool) => {
                    pool.start_checkpointer();
                    pool.check()
                }
                Backend::Postgres(pool) => {
                    if let Some(w) = pool.security_warning() {
                        warnings.push(format!("database {name:?}: {w}"));
                    }
                    pool.check().await
                }
            };
            lines.push(format!("{name} ({})", line.map_err(|e| format_err!("database {name:?}: {e}"))?));
        }
        Ok((lines, warnings))
    }
}

/// Per-request database state.
pub struct DbCtx {
    pools: Arc<Pools>,
    /// Database names this app was granted.
    granted: Arc<[String]>,
    /// The app's own names for its databases, for deployed apps (`main` →
    /// `notes/main`).
    aliases: Arc<HashMap<String, String>>,
    /// Limit for each database call, including waiting for a connection.
    timeout: Duration,
    pub queries: u64,
    /// Time spent waiting on the database (pool + network + execution).
    pub wait: Duration,
}

impl DbCtx {
    pub fn new(pools: Arc<Pools>, granted: Arc<[String]>, aliases: Arc<HashMap<String, String>>, timeout: Duration) -> Self {
        Self { pools, granted, aliases, timeout, queries: 0, wait: Duration::ZERO }
    }

    fn backend(&self, db: &str) -> Result<BackendRef<'_>, Error> {
        let real = self.aliases.get(db).map_or(db, |r| r.as_str());
        if !self.granted.iter().any(|g| g == real) {
            return Err(Error::NotAllowed(format!("no grant for database {db:?}")));
        }
        self.pools.get(real).ok_or_else(|| Error::NotAllowed(format!("unknown database {db:?}")))
    }

    fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + self.timeout
    }

    fn record(&mut self, started: Instant) {
        self.queries += 1;
        self.wait += started.elapsed();
    }
}

/// Host side of the `transaction` resource.
pub enum Transaction {
    Postgres(postgres::Tx),
    Sqlite(sqlite::Tx),
}

/// Host side of the `cursor` resource.
pub enum Cursor {
    Postgres(postgres::Cursor),
    Sqlite(sqlite::Cursor),
}

pub fn add_to_linker(linker: &mut Linker<RequestState>) -> Result<()> {
    sql::add_to_linker::<RequestState, HasSelf<RequestState>>(linker, |s| s)
}

impl sql::Host for RequestState {
    async fn query(&mut self, db: String, statement: String, params: Vec<Value>) -> Result<Result<QueryResult, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.db.backend(&db) {
            Ok(BackendRef::Postgres(pool)) => pool.query(deadline, statement, params).await,
            Ok(BackendRef::Sqlite(pool)) => pool.clone().query(deadline, statement, params).await,
            Err(e) => Err(e),
        };
        self.db.record(started);
        Ok(result)
    }

    async fn execute(&mut self, db: String, statement: String, params: Vec<Value>) -> Result<Result<u64, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.db.backend(&db) {
            Ok(BackendRef::Postgres(pool)) => pool.execute(deadline, statement, params).await,
            Ok(BackendRef::Sqlite(pool)) => pool.clone().execute(deadline, statement, params).await,
            Err(e) => Err(e),
        };
        self.db.record(started);
        Ok(result)
    }
}

impl sql::HostTransaction for RequestState {
    async fn begin(&mut self, db: String) -> Result<Result<Resource<Transaction>, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.db.backend(&db) {
            Ok(BackendRef::Postgres(pool)) => pool.begin(deadline).await.map(Transaction::Postgres),
            Ok(BackendRef::Sqlite(pool)) => pool.clone().begin(deadline).await.map(Transaction::Sqlite),
            Err(e) => Err(e),
        };
        self.db.record(started);
        Ok(match result {
            Ok(tx) => Ok(self.table.push(tx)?),
            Err(e) => Err(e),
        })
    }

    async fn query(&mut self, tx: Resource<Transaction>, statement: String, params: Vec<Value>) -> Result<Result<QueryResult, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.table.get_mut(&tx)? {
            Transaction::Postgres(tx) => tx.query(deadline, statement, params).await,
            Transaction::Sqlite(tx) => tx.query(deadline, statement, params).await,
        };
        self.db.record(started);
        Ok(result)
    }

    async fn execute(&mut self, tx: Resource<Transaction>, statement: String, params: Vec<Value>) -> Result<Result<u64, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.table.get_mut(&tx)? {
            Transaction::Postgres(tx) => tx.execute(deadline, statement, params).await,
            Transaction::Sqlite(tx) => tx.execute(deadline, statement, params).await,
        };
        self.db.record(started);
        Ok(result)
    }

    async fn commit(&mut self, tx: Resource<Transaction>) -> Result<Result<(), Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.table.delete(tx)? {
            Transaction::Postgres(tx) => tx.commit(deadline).await,
            Transaction::Sqlite(tx) => tx.commit(deadline).await,
        };
        self.db.record(started);
        Ok(result)
    }

    async fn drop(&mut self, tx: Resource<Transaction>) -> Result<()> {
        // The guest dropped the handle without committing: roll back.
        match self.table.delete(tx)? {
            Transaction::Postgres(tx) => tx.rollback().await,
            Transaction::Sqlite(tx) => tx.rollback().await,
        }
        Ok(())
    }
}

impl sql::HostCursor for RequestState {
    async fn open(&mut self, db: String, statement: String, params: Vec<Value>) -> Result<Result<Resource<Cursor>, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let result = match self.db.backend(&db) {
            Ok(BackendRef::Postgres(pool)) => pool.open_cursor(deadline, statement, params).await.map(Cursor::Postgres),
            Ok(BackendRef::Sqlite(pool)) => pool.clone().open_cursor(deadline, statement, params).await.map(Cursor::Sqlite),
            Err(e) => Err(e),
        };
        self.db.record(started);
        Ok(match result {
            Ok(cursor) => Ok(self.table.push(cursor)?),
            Err(e) => Err(e),
        })
    }

    async fn columns(&mut self, cursor: Resource<Cursor>) -> Result<Vec<String>> {
        Ok(match self.table.get(&cursor)? {
            Cursor::Postgres(c) => c.columns.clone(),
            Cursor::Sqlite(c) => c.columns.clone(),
        })
    }

    async fn next(&mut self, cursor: Resource<Cursor>, max_rows: u32) -> Result<Result<Vec<Vec<Value>>, Error>> {
        let started = Instant::now();
        let deadline = self.db.deadline();
        let max_rows = (max_rows as usize).clamp(1, MAX_ROWS);
        let result = match self.table.get_mut(&cursor)? {
            Cursor::Postgres(c) => c.next(deadline, max_rows).await,
            Cursor::Sqlite(c) => c.next(deadline, max_rows).await,
        };
        self.db.record(started);
        Ok(result)
    }

    async fn drop(&mut self, cursor: Resource<Cursor>) -> Result<()> {
        // Each cursor's `Drop` cancels its query if it's still streaming.
        self.table.delete(cursor)?;
        Ok(())
    }
}

/// An error and all its causes, e.g. "...: db error: password authentication failed".
fn chain(e: &dyn StdError) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}
