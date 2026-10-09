//! Durable queues and cron (`platform:sdk/queue`, delivered through
//! `platform:sdk/events`).
//!
//! Jobs live in SQLite (`<data-dir>/platform.db`). A single store thread owns
//! the database and applies commands in batches, one transaction each (group
//! commit): no lock contention, and `send` returns once its message is
//! committed. Job workers, separate threads like the HTTP workers, claim due
//! jobs with a lease and run the consumer's `events.handle`. A job whose
//! worker died is claimed again when the lease runs out, so delivery is at
//! least once. Failures are retried with exponential backoff and kept as
//! dead after the last attempt. Cron enqueues one job per tick, keyed by the
//! scheduled time, so a tick runs once even across restarts.

use crate::app::RequestState;
use crate::bindings::events::Kind;
use crate::bindings::queue::{self, Error as QueueError};
use crate::db::Pools;
use crate::platform::{self, Env, Platform};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use wasmtime::component::{HasSelf, Linker};
use wasmtime::{Result, format_err};

/// Deliveries per job before it is kept as dead.
pub const MAX_ATTEMPTS: u32 = 5;
const MAX_PAYLOAD: usize = 256 << 10;
/// Concurrent deliveries per job worker.
const DELIVERIES_PER_WORKER: usize = 64;
/// Commands applied per transaction.
const BATCH: usize = 512;

pub struct Job {
    id: i64,
    uid: String,
    kind: Kind,
    name: String,
    app: String,
    payload: Vec<u8>,
    attempt: u32,
}

/// Pending and dead jobs per queue or cron job, for `/_platform/jobs`.
pub struct JobCount {
    pub kind: String,
    pub name: String,
    pub app: String,
    pub pending: i64,
    pub dead: i64,
}

enum Cmd {
    Enqueue { uid: String, kind: Kind, name: String, app: String, payload: Vec<u8>, run_at: i64, ack: oneshot::Sender<Result<bool, String>> },
    Claim { limit: usize, lease_until: i64, reply: oneshot::Sender<Vec<Job>> },
    Done { id: i64 },
    Retry { id: i64, run_at: i64, error: String },
    Dead { id: i64, error: String },
    /// Back to pending without counting the attempt.
    Postpone { id: i64, run_at: i64 },
    NextDue { reply: oneshot::Sender<Option<i64>> },
    Counts { reply: oneshot::Sender<Vec<JobCount>> },
}

/// Handle to the store thread.
pub struct JobStore {
    tx: mpsc::UnboundedSender<Cmd>,
    /// Wakes an idle job worker when something is enqueued.
    notify: Notify,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

fn kind_from(s: &str) -> Kind {
    if s == "cron" { Kind::Cron } else { Kind::Queue }
}

fn random_id() -> String {
    let random = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", random(), random())
}

impl JobStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format_err!("{}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format_err!("{}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE IF NOT EXISTS jobs (
                 id           INTEGER PRIMARY KEY,
                 uid          TEXT    NOT NULL UNIQUE,
                 kind         TEXT    NOT NULL,
                 name         TEXT    NOT NULL,
                 app          TEXT    NOT NULL,
                 payload      BLOB    NOT NULL,
                 attempts     INTEGER NOT NULL DEFAULT 0,
                 run_at       INTEGER NOT NULL,
                 locked_until INTEGER,
                 dead         INTEGER NOT NULL DEFAULT 0,
                 last_error   TEXT,
                 created_at   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS jobs_due ON jobs (dead, run_at);",
        )
        .map_err(|e| format_err!("{}: {e}", path.display()))?;
        // Checkpoints happen beside the store thread, not inside its commits.
        crate::db::sqlite::spawn_checkpointer(path.to_owned());
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new().name("job-store".into()).spawn(move || store_loop(conn, rx))?;
        Ok(Self { tx, notify: Notify::new() })
    }

    /// Adds a job; `Ok(false)` if one with the same `uid` already exists.
    pub async fn enqueue(&self, uid: String, kind: Kind, name: String, app: String, payload: Vec<u8>, run_at: i64) -> Result<bool, String> {
        let (ack, done) = oneshot::channel();
        self.tx.send(Cmd::Enqueue { uid, kind, name, app, payload, run_at, ack }).map_err(|_| "job store stopped".to_string())?;
        let inserted = done.await.map_err(|_| "job store stopped".to_string())??;
        if inserted {
            self.notify.notify_one();
        }
        Ok(inserted)
    }

    async fn claim(&self, limit: usize, lease: Duration) -> Vec<Job> {
        let (reply, rx) = oneshot::channel();
        let lease_until = now_ms() + lease.as_millis() as i64;
        let _ = self.tx.send(Cmd::Claim { limit, lease_until, reply });
        rx.await.unwrap_or_default()
    }

    async fn next_due(&self) -> Option<i64> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Cmd::NextDue { reply });
        rx.await.ok().flatten()
    }

    pub async fn counts(&self) -> Vec<JobCount> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Cmd::Counts { reply });
        rx.await.unwrap_or_default()
    }
}

/// Applies commands in batches, one transaction per batch, and answers
/// only after the commit.
fn store_loop(conn: Connection, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    enum Reply {
        Ack(oneshot::Sender<Result<bool, String>>, Result<bool, String>),
        Jobs(oneshot::Sender<Vec<Job>>, Vec<Job>),
        Due(oneshot::Sender<Option<i64>>, Option<i64>),
        Counts(oneshot::Sender<Vec<JobCount>>, Vec<JobCount>),
    }
    while let Some(first) = rx.blocking_recv() {
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }
        let now = now_ms();
        let mut replies = Vec::with_capacity(batch.len());
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            for cmd in batch {
                match cmd {
                    Cmd::Enqueue { uid, kind, name, app, payload, run_at, ack } => {
                        let n = conn
                            .prepare_cached(
                                "INSERT OR IGNORE INTO jobs (uid, kind, name, app, payload, run_at, created_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            )?
                            .execute(params![uid, platform::kind_name(kind), name, app, payload, run_at, now])?;
                        replies.push(Reply::Ack(ack, Ok(n > 0)));
                    }
                    Cmd::Claim { limit, lease_until, reply } => {
                        let mut stmt = conn.prepare_cached(
                            "UPDATE jobs SET locked_until = ?1, attempts = attempts + 1
                             WHERE id IN (SELECT id FROM jobs
                                          WHERE dead = 0 AND run_at <= ?2 AND (locked_until IS NULL OR locked_until <= ?2)
                                          ORDER BY run_at LIMIT ?3)
                             RETURNING id, uid, kind, name, app, payload, attempts",
                        )?;
                        let jobs = stmt
                            .query_map(params![lease_until, now, limit as i64], |r| {
                                Ok(Job {
                                    id: r.get(0)?,
                                    uid: r.get(1)?,
                                    kind: kind_from(&r.get::<_, String>(2)?),
                                    name: r.get(3)?,
                                    app: r.get(4)?,
                                    payload: r.get(5)?,
                                    attempt: r.get(6)?,
                                })
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        replies.push(Reply::Jobs(reply, jobs));
                    }
                    Cmd::Done { id } => {
                        conn.prepare_cached("DELETE FROM jobs WHERE id = ?1")?.execute([id])?;
                    }
                    Cmd::Retry { id, run_at, error } => {
                        conn.prepare_cached("UPDATE jobs SET run_at = ?2, locked_until = NULL, last_error = ?3 WHERE id = ?1")?
                            .execute(params![id, run_at, error])?;
                    }
                    Cmd::Postpone { id, run_at } => {
                        conn.prepare_cached("UPDATE jobs SET run_at = ?2, locked_until = NULL, attempts = attempts - 1 WHERE id = ?1")?
                            .execute(params![id, run_at])?;
                    }
                    Cmd::Dead { id, error } => {
                        conn.prepare_cached("UPDATE jobs SET dead = 1, locked_until = NULL, last_error = ?2 WHERE id = ?1")?
                            .execute(params![id, error])?;
                    }
                    Cmd::NextDue { reply } => {
                        let due = conn
                            .prepare_cached(
                                "SELECT min(max(run_at, coalesce(locked_until, 0))) FROM jobs WHERE dead = 0",
                            )?
                            .query_row([], |r| r.get::<_, Option<i64>>(0))
                            .optional()?
                            .flatten();
                        replies.push(Reply::Due(reply, due));
                    }
                    Cmd::Counts { reply } => {
                        let mut stmt = conn.prepare_cached(
                            "SELECT kind, name, app, sum(dead = 0), sum(dead = 1) FROM jobs GROUP BY kind, name, app ORDER BY kind, name",
                        )?;
                        let counts = stmt
                            .query_map([], |r| {
                                Ok(JobCount { kind: r.get(0)?, name: r.get(1)?, app: r.get(2)?, pending: r.get(3)?, dead: r.get(4)? })
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        replies.push(Reply::Counts(reply, counts));
                    }
                }
            }
            conn.execute_batch("COMMIT")
        })();
        if let Err(e) = result {
            eprintln!("job store: {e}");
            let _ = conn.execute_batch("ROLLBACK");
            for reply in replies {
                if let Reply::Ack(ack, _) = reply {
                    let _ = ack.send(Err(format!("job store: {e}")));
                }
                // Other waiters see a closed channel, which callers treat as
                // "nothing" and retry later.
            }
            continue;
        }
        for reply in replies {
            match reply {
                Reply::Ack(tx, v) => drop(tx.send(v)),
                Reply::Jobs(tx, v) => drop(tx.send(v)),
                Reply::Due(tx, v) => drop(tx.send(v)),
                Reply::Counts(tx, v) => drop(tx.send(v)),
            }
        }
    }
}

pub fn add_to_linker(linker: &mut Linker<RequestState>) -> Result<()> {
    queue::add_to_linker::<RequestState, HasSelf<RequestState>>(linker, |s| s)
}

impl queue::Host for RequestState {
    async fn send(&mut self, name: String, payload: Vec<u8>, delay_ms: u64) -> Result<Result<(), QueueError>> {
        let platform = self.env.platform.clone();
        let Some(consumer) = platform.current().queues.get(&name).cloned() else {
            return Ok(Err(QueueError::UnknownQueue(name)));
        };
        if payload.len() > MAX_PAYLOAD {
            return Ok(Err(QueueError::TooLarge(payload.len() as u64)));
        }
        let Some(store) = &platform.jobs else {
            return Ok(Err(QueueError::Unavailable("no job store".into())));
        };
        let counters = self.counters();
        counters.add(crate::stats::Counter::QueueMessages, 1);
        counters.add(crate::stats::Counter::QueueBytes, payload.len() as u64);
        let started = Instant::now();
        let run_at = now_ms() + delay_ms.min(365 * 24 * 3600 * 1000) as i64;
        let result = store.enqueue(random_id(), Kind::Queue, name, consumer, payload, run_at).await;
        // Waiting for the commit is not the guest's CPU time.
        self.call_wait += started.elapsed();
        Ok(result.map(|_| ()).map_err(QueueError::Unavailable))
    }
}

/// Starts `count` job worker threads; their stats slots begin at `first_index`.
pub fn spawn_workers(platform: Arc<Platform>, first_index: usize, count: usize) -> Result<()> {
    for i in 0..count {
        let platform = platform.clone();
        let worker = first_index + i;
        std::thread::Builder::new().name(format!("jobs-{i}")).spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let pools = Arc::new(Pools::new(&platform.databases, platform.db_pool_size, &platform.db_tls)?);
            rt.block_on(dispatch(platform, worker, pools));
            Ok(())
        })?;
    }
    Ok(())
}

async fn dispatch(platform: Arc<Platform>, worker: usize, pools: Arc<Pools>) {
    let store = platform.jobs.as_ref().expect("job workers need a job store");
    let slots = Arc::new(Semaphore::new(DELIVERIES_PER_WORKER));
    // A job not finished within this is assumed lost and claimed again.
    let lease = platform.timeout + Duration::from_secs(30);
    loop {
        let free = slots.available_permits();
        if free == 0 {
            drop(slots.acquire().await);
            continue;
        }
        let jobs = store.claim(free.min(32), lease).await;
        if jobs.is_empty() {
            let wait = match store.next_due().await {
                Some(due) => Duration::from_millis((due - now_ms()).clamp(1, 1000) as u64),
                None => Duration::from_secs(1),
            };
            tokio::select! {
                _ = store.notify.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
            continue;
        }
        for job in jobs {
            let permit = slots.clone().acquire_owned().await.unwrap();
            let (platform, pools) = (platform.clone(), pools.clone());
            tokio::spawn(async move {
                deliver(platform, worker, pools, job).await;
                drop(permit);
            });
        }
    }
}

async fn deliver(platform: Arc<Platform>, worker: usize, pools: Arc<Pools>, job: Job) {
    let store = platform.jobs.as_ref().unwrap();
    let Some(app) = platform.current().routes.app(&job.app).cloned() else {
        let _ = store.tx.send(Cmd::Dead { id: job.id, error: format!("no app named {:?}", job.app) });
        return;
    };
    if platform.billing.is_blocked(app.id) {
        // Out of credits: try again later, without using up an attempt.
        let _ = store.tx.send(Cmd::Postpone { id: job.id, run_at: now_ms() + 60_000 });
        return;
    }
    let counters = platform.stats.counters(worker, app.id);
    counters.request();
    counters.add(crate::stats::Counter::Events, 1);
    let env = Env::new(platform.clone(), worker, pools);
    let error = match platform::event(env, &app, job.kind, job.uid.clone(), job.name.clone(), job.payload, job.attempt).await {
        Ok(Ok(())) => {
            let _ = store.tx.send(Cmd::Done { id: job.id });
            return;
        }
        Ok(Err(message)) => message,
        Err(e) => format!("{e:?}"),
    };
    counters.error();
    if job.attempt >= MAX_ATTEMPTS {
        eprintln!("{} {}: job {} is dead after {} attempts: {error}", platform::kind_name(job.kind), job.name, job.uid, job.attempt);
        let _ = store.tx.send(Cmd::Dead { id: job.id, error });
    } else {
        let backoff = Duration::from_secs(1 << job.attempt.min(12)).min(Duration::from_secs(3600));
        let _ = store.tx.send(Cmd::Retry { id: job.id, run_at: now_ms() + backoff.as_millis() as i64, error });
    }
}

/// A cron job: run `app`'s event handler on `schedule`.
pub struct CronJob {
    pub app: String,
    pub name: String,
    pub expr: String,
    pub schedule: croner::Cron,
}

impl CronJob {
    pub fn parse(app: &str, name: &str, expr: &str) -> Result<Self> {
        let schedule = expr.parse::<croner::Cron>().map_err(|e| format_err!("cron {app}:{name}={expr}: {e}"))?;
        Ok(Self { app: app.into(), name: name.into(), expr: expr.into(), schedule })
    }
}

/// Starts the cron thread. It follows the current deployment, so a reload
/// adds, changes or removes schedules. Each tick becomes a job keyed by its
/// scheduled time, so it's enqueued once even if the server restarts.
pub fn spawn_cron(platform: Arc<Platform>) -> Result<()> {
    std::thread::Builder::new().name("cron".into()).spawn(move || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(async move {
            // (app, name, schedule) → next run.
            let mut next_runs: std::collections::HashMap<(String, String, String), chrono::DateTime<chrono::Utc>> = Default::default();
            let store = platform.jobs.as_ref().expect("cron needs the job store");
            loop {
                let current = platform.current();
                let now = chrono::Utc::now();
                let mut live = std::collections::HashSet::new();
                for job in &current.crons {
                    let key = (job.app.clone(), job.name.clone(), job.expr.clone());
                    live.insert(key.clone());
                    let Some(next) = next_runs.get(&key).copied().or_else(|| job.schedule.find_next_occurrence(&now, false).ok()) else {
                        continue;
                    };
                    if next > now {
                        if next_runs.insert(key, next).is_none() {
                            eprintln!("cron: {}:{} at '{}'", job.app, job.name, job.expr);
                        }
                        continue;
                    }
                    let at = next.timestamp_millis();
                    let uid = format!("cron:{}:{}:{at}", job.app, job.name);
                    let payload = next.to_rfc3339().into_bytes();
                    if let Err(e) = store.enqueue(uid, Kind::Cron, job.name.clone(), job.app.clone(), payload, at).await {
                        eprintln!("cron {}: {e}", job.name);
                    }
                    match job.schedule.find_next_occurrence(&now, false) {
                        Ok(after) => drop(next_runs.insert(key, after)),
                        Err(e) => eprintln!("cron {}: {e}", job.name),
                    }
                }
                next_runs.retain(|key, _| live.contains(key));
                let wait = next_runs
                    .values()
                    .map(|t| (*t - chrono::Utc::now()).to_std().unwrap_or_default())
                    .min()
                    .unwrap_or(Duration::from_secs(1))
                    .min(Duration::from_secs(1));
                tokio::time::sleep(wait).await;
            }
        });
        Ok(())
    })?;
    Ok(())
}
