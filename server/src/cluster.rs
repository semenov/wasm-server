//! Several servers as one: a primary and workers that join it.
//!
//! The primary (`--cluster-listen ADDR --cluster-token T`) keeps everything
//! with state: HTTPS, databases, queues and cron, WebSockets, logs, metrics
//! and billing. A worker (`serve --join http://PRIMARY --cluster-token T`)
//! runs app code only:
//!
//! - **Apps:** it takes them from the primary (settings, secrets, components)
//!   and checks for changes every 2 seconds, with its heartbeat.
//! - **Requests:** the primary sends each app request to the node with the
//!   fewest requests in flight per core, itself included; WebSockets and
//!   static files stay on the primary. A worker accepts requests only from
//!   the primary (the cluster token).
//! - **Databases and queues:** a worker's `sql` and `queue` calls go to the
//!   primary over the cluster port (transactions are held there by id); its
//!   cache is its own.
//! - **Logs and counters** go back to the primary with the heartbeat, so
//!   `/_platform/` (logs, metrics, billing) sees the whole cluster.
//!
//! The autoscaler (`autoscale.rs`) adds and removes workers on the primary.

use crate::bindings::queue::Error as QueueError;
use crate::bindings::sql::{Error as SqlError, QueryResult, Value};
use crate::platform::Platform;
use crate::stats::{Counter, LATENCY_BUCKETS};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value as J, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wasmtime::{Result, bail, format_err};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Header carrying the cluster token on requests the primary forwards.
pub const TOKEN_HEADER: &str = "x-cluster-token";
/// A worker not heard from for this long is out of the cluster.
const NODE_TIMEOUT: Duration = Duration::from_secs(10);
/// An open transaction held for a worker, idle for this long, is rolled back.
const TX_IDLE: Duration = Duration::from_secs(30);
pub const HEARTBEAT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------- shared

/// An app as a worker runs it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppState {
    pub name: String,
    /// SHA-256 of the component file, to fetch it when it changes.
    pub hash: String,
    pub domains: Vec<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub outbound: Option<Vec<String>>,
    pub websocket: Option<String>,
    pub env: BTreeMap<String, String>,
    pub databases: Vec<String>,
    pub db_aliases: BTreeMap<String, String>,
    pub links: BTreeMap<String, String>,
    pub db_timeout_ms: Option<u64>,
}

/// What a worker needs to serve the primary's apps.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct State {
    pub apps: Vec<AppState>,
    /// The deployment file's databases (deployed apps' are `app/name`).
    pub databases: Vec<String>,
}

/// A worker's report, every `HEARTBEAT`.
#[derive(Serialize, Deserialize, Debug)]
pub struct Heartbeat {
    pub id: String,
    /// Where the primary sends requests: `http://10.0.0.5:8080`.
    pub addr: String,
    pub cores: usize,
    /// The worker process's CPU use since the last heartbeat, 0..1 of its cores.
    pub cpu: f64,
    pub inflight: u64,
    /// Per app: its counters and request-time buckets, as totals so far.
    pub stats: Vec<(String, Vec<u64>, Vec<u64>)>,
    /// Lines for apps' logs.
    pub logs: Vec<(String, String)>,
    /// The state it runs (hash), so the primary can say when it's stale.
    pub state: String,
}

pub fn value_to_json(v: &Value) -> J {
    match v {
        Value::Null => json!({ "null": true }),
        Value::Int(i) => json!({ "int": i }),
        Value::Float(f) => json!({ "float": f }),
        Value::Text(s) => json!({ "text": s }),
        Value::Boolean(b) => json!({ "bool": b }),
        Value::Blob(b) => json!({ "blob": base64::engine::general_purpose::STANDARD.encode(b) }),
    }
}

pub fn value_from_json(j: &J) -> Value {
    if let Some(i) = j.get("int").and_then(J::as_i64) {
        Value::Int(i)
    } else if let Some(f) = j.get("float").and_then(J::as_f64) {
        Value::Float(f)
    } else if let Some(s) = j.get("text").and_then(J::as_str) {
        Value::Text(s.to_string())
    } else if let Some(b) = j.get("bool").and_then(J::as_bool) {
        Value::Boolean(b)
    } else if let Some(b) = j.get("blob").and_then(J::as_str) {
        Value::Blob(base64::engine::general_purpose::STANDARD.decode(b).unwrap_or_default())
    } else {
        Value::Null
    }
}

pub fn error_to_json(e: &SqlError) -> J {
    match e {
        SqlError::NotAllowed(m) => json!({ "not_allowed": m }),
        SqlError::Unavailable(m) => json!({ "unavailable": m }),
        SqlError::Query(m) => json!({ "query": m }),
        SqlError::Conversion(m) => json!({ "conversion": m }),
        SqlError::Timeout => json!({ "timeout": true }),
        SqlError::TooManyRows(n) => json!({ "too_many_rows": n }),
    }
}

fn error_from_json(j: &J) -> SqlError {
    let s = |k: &str| j.get(k).and_then(J::as_str).map(str::to_string);
    if let Some(m) = s("not_allowed") {
        SqlError::NotAllowed(m)
    } else if let Some(m) = s("query") {
        SqlError::Query(m)
    } else if let Some(m) = s("conversion") {
        SqlError::Conversion(m)
    } else if j.get("timeout").is_some() {
        SqlError::Timeout
    } else if let Some(n) = j.get("too_many_rows").and_then(J::as_u64) {
        SqlError::TooManyRows(n as u32)
    } else {
        SqlError::Unavailable(s("unavailable").unwrap_or_else(|| "the primary's answer wasn't understood".into()))
    }
}

pub fn result_to_json(r: &QueryResult) -> J {
    json!({ "columns": r.columns, "rows": r.rows.iter().map(|row| row.iter().map(value_to_json).collect::<Vec<_>>()).collect::<Vec<_>>() })
}

fn result_from_json(j: &J) -> QueryResult {
    let columns = j.get("columns").and_then(J::as_array).map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let rows = j.get("rows").and_then(J::as_array).map(|rows| rows.iter().map(|r| r.as_array().map(|vs| vs.iter().map(value_from_json).collect()).unwrap_or_default()).collect()).unwrap_or_default();
    QueryResult { columns, rows }
}

type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

thread_local! {
    /// Kept connections to other nodes, per thread (each runtime is on one).
    static CONNS: std::cell::RefCell<HashMap<String, Vec<Sender>>> = std::cell::RefCell::new(HashMap::new());
}

async fn connect(addr: &str) -> Result<Sender> {
    let uri: hyper::Uri = addr.parse()?;
    let host = uri.host().ok_or_else(|| format_err!("{addr}: no host"))?.to_string();
    let port = uri.port_u16().unwrap_or(80);
    let stream = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect((host.as_str(), port))).await.map_err(|_| format_err!("{addr}: no answer"))??;
    let _ = stream.set_nodelay(true);
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    Ok(sender)
}

/// A plain-HTTP request to another node: (status, body). Connections are
/// kept: under load a worker makes thousands of database calls a second.
async fn call(addr: &str, method: &str, path: &str, token: &str, body: Bytes) -> Result<(u16, Bytes)> {
    for attempt in 0..2 {
        let kept = CONNS.with(|c| {
            let mut c = c.borrow_mut();
            let list = c.entry(addr.to_string()).or_default();
            list.retain(|s| !s.is_closed());
            list.iter().position(|s| s.is_ready()).map(|i| list.swap_remove(i))
        });
        let reused = kept.is_some();
        let mut sender = match kept {
            Some(s) => s,
            None => connect(addr).await?,
        };
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", addr.trim_start_matches("http://"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-length", body.len())
            .body(Full::new(body.clone()))?;
        match sender.send_request(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let bytes = resp.into_body().collect().await?.to_bytes();
                CONNS.with(|c| c.borrow_mut().entry(addr.to_string()).or_default().push(sender));
                return Ok((status, bytes));
            }
            // a kept connection the other side had closed: the request
            // wasn't taken, so once more on a new one
            Err(e) if reused && attempt == 0 && (e.is_canceled() || e.is_closed() || e.is_incomplete_message()) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("{addr}: no connection")
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    crate::versions::hex(&sha2::Sha256::digest(data))
}

/// This process's CPU time so far.
pub fn process_cpu() -> Duration {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// CPU use between two calls, as a fraction of `cores`.
pub struct CpuGauge {
    last: Mutex<(Instant, Duration)>,
    cores: usize,
}

impl CpuGauge {
    pub fn new(cores: usize) -> Self {
        Self { last: Mutex::new((Instant::now(), process_cpu())), cores: cores.max(1) }
    }
    pub fn sample(&self) -> f64 {
        let mut last = self.last.lock().unwrap();
        let (now, cpu) = (Instant::now(), process_cpu());
        let wall = now.duration_since(last.0).as_secs_f64();
        let used = cpu.saturating_sub(last.1).as_secs_f64();
        *last = (now, cpu);
        if wall <= 0.0 { 0.0 } else { (used / wall / self.cores as f64).min(1.0) }
    }
}

// ---------------------------------------------------------------- worker

pub struct Worker {
    pub primary: String,
    pub token: String,
    pub id: String,
    /// Where the primary reaches this worker.
    pub addr: String,
    /// App requests running here.
    pub inflight: AtomicU64,
    /// Log lines waiting for the next heartbeat.
    logs: Mutex<Vec<(String, String)>>,
    /// The hash of the state this worker runs.
    pub state_hash: Mutex<String>,
}

static WORKER: OnceLock<Arc<Worker>> = OnceLock::new();

impl Worker {
    pub fn new(primary: String, token: String, id: String, addr: String) -> Self {
        Self { primary, token, id, addr, inflight: AtomicU64::new(0), logs: Mutex::default(), state_hash: Mutex::default() }
    }
}

pub fn is_worker() -> bool {
    WORKER.get().is_some()
}

pub fn worker() -> Option<&'static Arc<Worker>> {
    WORKER.get()
}

pub fn set_worker(w: Worker) {
    WORKER.set(Arc::new(w)).ok();
}

/// An app's log line on a worker also goes to the primary.
pub fn forward_log(app: &str, text: &str) {
    if let Some(w) = WORKER.get() {
        let mut logs = w.logs.lock().unwrap();
        if logs.len() < 10_000 {
            logs.push((app.to_string(), text.to_string()));
        }
    }
}

async fn worker_call(path: &str, body: J) -> Result<J> {
    let w = WORKER.get().ok_or_else(|| format_err!("not a cluster worker"))?;
    let (status, bytes) = call(&w.primary, "POST", path, &w.token, Bytes::from(body.to_string())).await?;
    if status != 200 {
        bail!("the primary answered {status}: {}", String::from_utf8_lossy(&bytes));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// A storage call from a worker's app, made on the primary, which keeps
/// the files.
pub async fn remote_storage(project: &str, mut call: J) -> Result<J, String> {
    call["project"] = J::String(project.to_string());
    let reply = worker_call("/cluster/storage", call).await.map_err(|e| format!("the primary: {e}"))?;
    match reply.get("err").and_then(J::as_str) {
        Some(e) => Err(e.to_string()),
        None => Ok(reply.get("ok").cloned().unwrap_or(J::Null)),
    }
}

async fn remote_sql(call: J) -> Result<J, SqlError> {
    let reply = worker_call("/cluster/sql", call).await.map_err(|e| SqlError::Unavailable(format!("the primary: {e}")))?;
    match reply.get("err") {
        Some(e) => Err(error_from_json(e)),
        None => Ok(reply.get("ok").cloned().unwrap_or(J::Null)),
    }
}

/// A worker's `query` (and transaction query), on the primary.
pub async fn remote_query(call: J) -> Result<QueryResult, SqlError> {
    remote_sql(call).await.map(|v| result_from_json(&v))
}

/// A worker's `execute`, `begin` (the transaction's id), `commit`...
pub async fn remote_count(call: J) -> Result<u64, SqlError> {
    remote_sql(call).await.map(|v| v.as_u64().unwrap_or(0))
}

/// A worker's `queue.send`, on the primary.
pub async fn remote_send(queue: &str, payload: &[u8], delay_ms: u64) -> Result<(), QueueError> {
    let body = json!({ "queue": queue, "payload": base64::engine::general_purpose::STANDARD.encode(payload), "delay_ms": delay_ms });
    match worker_call("/cluster/queue", body).await {
        Ok(r) => match r.get("err") {
            None => Ok(()),
            Some(e) => Err(match (e.get("unknown_queue").and_then(J::as_str), e.get("too_large").and_then(J::as_u64)) {
                (Some(q), _) => QueueError::UnknownQueue(q.to_string()),
                (_, Some(n)) => QueueError::TooLarge(n),
                _ => QueueError::Unavailable(e.get("unavailable").and_then(J::as_str).unwrap_or("the primary").to_string()),
            }),
        },
        Err(e) => Err(QueueError::Unavailable(format!("the primary: {e}"))),
    }
}

/// The deployment a worker runs: the primary's apps, their components
/// fetched into `<data-dir>/cluster/` (by hash, so each is fetched once).
pub fn load_deployment(data_dir: &std::path::Path) -> Result<crate::deployment::Deployment> {
    let w = WORKER.get().ok_or_else(|| format_err!("not a cluster worker"))?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (status, bytes) = rt.block_on(call(&w.primary, "GET", "/cluster/state", &w.token, Bytes::new()))?;
    if status != 200 {
        bail!("the primary at {} answered {status}: {}", w.primary, String::from_utf8_lossy(&bytes));
    }
    let state: State = serde_json::from_slice(&bytes)?;
    let dir = data_dir.join("cluster");
    std::fs::create_dir_all(&dir)?;
    let mut d = crate::deployment::Deployment::default();
    for db in &state.databases {
        d.databases.insert(db.clone(), crate::deployment::DatabaseSpec { url: "remote:".into(), init: None });
    }
    for app in &state.apps {
        let file = dir.join(format!("{}.cwasm", app.hash));
        if !file.exists() {
            let (status, bytes) = rt.block_on(call(&w.primary, "GET", &format!("/cluster/component/{}", app.name), &w.token, Bytes::new()))?;
            if status != 200 {
                bail!("component of {}: the primary answered {status}", app.name);
            }
            let tmp = file.with_extension("tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &file)?;
        }
        d.apps.insert(
            app.name.clone(),
            crate::deployment::AppSpec {
                component: file,
                databases: app.databases.clone(),
                db_aliases: app.db_aliases.clone(),
                domains: app.domains.clone(),
                path: app.path.clone(),
                project: app.project.clone(),
                env: app.env.clone(),
                outbound: app.outbound.clone(),
                websocket: app.websocket.clone(),
                links: app.links.clone(),
                db_timeout_ms: app.db_timeout_ms,
                ..Default::default()
            },
        );
    }
    *w.state_hash.lock().unwrap() = sha256_hex(&bytes);
    Ok(d)
}

/// The worker's heartbeat: its load, counters and log lines to the primary;
/// a reload when the primary's apps changed.
pub fn spawn_heartbeat(platform: Arc<Platform>, cores: usize) -> Result<()> {
    let w = WORKER.get().ok_or_else(|| format_err!("not a cluster worker"))?.clone();
    std::thread::Builder::new().name("heartbeat".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let gauge = CpuGauge::new(cores);
        loop {
            std::thread::sleep(HEARTBEAT);
            let names = platform.accounts.names();
            let stats = names.iter().enumerate().map(|(id, n)| (n.clone(), platform.stats.own_totals(id).to_vec(), platform.stats.latency(id).to_vec())).collect();
            let logs = std::mem::take(&mut *w.logs.lock().unwrap());
            let hb = Heartbeat { id: w.id.clone(), addr: w.addr.clone(), cores, cpu: gauge.sample(), inflight: w.inflight.load(Relaxed), stats, logs, state: w.state_hash.lock().unwrap().clone() };
            let body = Bytes::from(serde_json::to_vec(&hb).unwrap_or_default());
            match rt.block_on(call(&w.primary, "POST", "/cluster/heartbeat", &w.token, body)) {
                Ok((200, reply)) => {
                    let stale = serde_json::from_slice::<J>(&reply).ok().and_then(|r| r.get("state").and_then(J::as_str).map(str::to_string)).is_some_and(|s| s != hb.state);
                    if stale {
                        match platform.reload() {
                            Ok(s) => eprintln!("cluster: took the primary's new apps ({} apps; loaded {:?})", s.apps, s.loaded),
                            Err(e) => eprintln!("cluster: can't take the primary's apps: {e:?}"),
                        }
                    }
                }
                Ok((status, reply)) => eprintln!("cluster: the primary answered {status}: {}", String::from_utf8_lossy(&reply)),
                Err(e) => eprintln!("cluster: can't reach the primary at {}: {e}", w.primary),
            }
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------- primary

pub struct Node {
    pub id: String,
    pub addr: String,
    pub cores: usize,
    pub cpu: f64,
    pub last_seen: Instant,
    /// No new requests: about to be removed.
    pub draining: bool,
    /// The state the worker runs; requests go only to up-to-date workers.
    pub state: String,
    /// Requests the primary has forwarded to it and not seen finish.
    pub sent: Arc<AtomicU64>,
    /// Its last counters, by account id.
    stats: HashMap<usize, (Vec<u64>, Vec<u64>)>,
}

impl Node {
    /// HTTP requests it has handled, for all apps.
    pub fn requests(&self) -> u64 {
        self.stats.values().map(|(c, _)| c.get(Counter::HttpRequests as usize).copied().unwrap_or(0)).sum()
    }
}

pub struct Primary {
    pub token: String,
    platform: Arc<Platform>,
    pub nodes: Mutex<Vec<Node>>,
    /// App requests running on the primary itself.
    pub local_inflight: AtomicU64,
    pub local_cores: usize,
    /// Counters of workers that left, so totals never go back.
    retired: Mutex<HashMap<usize, (Vec<u64>, Vec<u64>)>>,
    /// Component hashes by file, as (modified, length) → hash.
    hashes: Mutex<HashMap<std::path::PathBuf, (std::time::SystemTime, u64, String)>>,
    /// Idle connections to workers.
    idle: Mutex<HashMap<String, Vec<hyper::client::conn::http1::SendRequest<Incoming>>>>,
    /// `state()` for the deployment version it was made from.
    state_cache: Mutex<Option<(usize, Vec<u8>, String)>>,
}

static PRIMARY: OnceLock<Arc<Primary>> = OnceLock::new();

pub fn primary() -> Option<&'static Arc<Primary>> {
    PRIMARY.get()
}

/// Counters the cluster's workers add to an account's totals.
pub fn extra_totals(account: usize) -> Option<(Vec<u64>, Vec<u64>)> {
    let p = PRIMARY.get()?;
    let mut c = vec![0u64; Counter::ALL.len()];
    let mut l = vec![0u64; LATENCY_BUCKETS];
    let mut any = false;
    let mut add = |s: &(Vec<u64>, Vec<u64>)| {
        any = true;
        for (a, b) in c.iter_mut().zip(&s.0) {
            *a = a.wrapping_add(*b);
        }
        for (a, b) in l.iter_mut().zip(&s.1) {
            *a = a.wrapping_add(*b);
        }
    };
    if let Some(s) = p.retired.lock().unwrap().get(&account) {
        add(s);
    }
    for n in p.nodes.lock().unwrap().iter() {
        if let Some(s) = n.stats.get(&account) {
            add(s);
        }
    }
    any.then_some((c, l))
}

impl Primary {
    fn hash_of(&self, path: &std::path::Path) -> String {
        let meta = std::fs::metadata(path).ok();
        let key = meta.as_ref().map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()));
        if let (Some((t, len)), Some((t0, len0, h))) = (key, self.hashes.lock().unwrap().get(path).cloned()) {
            if t == t0 && len == len0 {
                return h;
            }
        }
        let h = std::fs::read(path).map(|b| sha256_hex(&b)).unwrap_or_default();
        if let Some((t, len)) = key {
            self.hashes.lock().unwrap().insert(path.to_path_buf(), (t, len, h.clone()));
        }
        h
    }

    /// The apps as workers run them, and its hash.
    pub fn state(&self) -> (Vec<u8>, String) {
        let current = self.platform.current();
        let version = Arc::as_ptr(&current) as usize;
        if let Some((v, bytes, hash)) = self.state_cache.lock().unwrap().as_ref() {
            if *v == version {
                return (bytes.clone(), hash.clone());
            }
        }
        let d = &current.deployment;
        let apps = d
            .apps
            .iter()
            .map(|(name, spec)| AppState {
                name: name.clone(),
                hash: self.hash_of(&spec.component),
                domains: spec.domains.clone(),
                path: spec.path.clone(),
                project: spec.project.clone(),
                outbound: spec.outbound.clone(),
                websocket: spec.websocket.clone(),
                env: spec.env.clone(),
                databases: spec.databases.clone(),
                db_aliases: spec.db_aliases.clone(),
                links: spec.links.clone(),
                db_timeout_ms: spec.db_timeout_ms,
            })
            .collect();
        let state = State { apps, databases: d.databases.keys().cloned().collect() };
        let bytes = serde_json::to_vec(&state).unwrap_or_default();
        let hash = sha256_hex(&bytes);
        *self.state_cache.lock().unwrap() = Some((version, bytes.clone(), hash.clone()));
        (bytes, hash)
    }

    /// Where an app request goes: None for here, or a worker (its address,
    /// and its in-flight counter, which the caller decrements when done).
    pub fn pick(&self) -> Option<(String, Arc<AtomicU64>)> {
        let state = self.state().1;
        let nodes = self.nodes.lock().unwrap();
        let mut best: Option<(f64, &Node)> = None;
        for n in nodes.iter() {
            if n.draining || n.last_seen.elapsed() > NODE_TIMEOUT || n.state != state {
                continue;
            }
            let score = n.sent.load(Relaxed) as f64 / n.cores.max(1) as f64;
            if best.is_none_or(|(b, _)| score < b) {
                best = Some((score, n));
            }
        }
        let local = self.local_inflight.load(Relaxed) as f64 / self.local_cores.max(1) as f64;
        match best {
            Some((score, n)) if score < local => {
                n.sent.fetch_add(1, Relaxed);
                Some((n.addr.clone(), n.sent.clone()))
            }
            _ => None,
        }
    }

    /// Sends an app request to a worker. Err(req): the worker couldn't be
    /// reached before anything was sent; run it here instead.
    pub async fn forward(&self, addr: &str, mut req: Request<Incoming>) -> std::result::Result<Response<HyperOutgoingBody>, Request<Incoming>> {
        let sender = {
            let mut idle = self.idle.lock().unwrap();
            let list = idle.entry(addr.to_string()).or_default();
            list.retain(|s| !s.is_closed());
            // a connection still finishing an earlier response stays for later
            list.iter().position(|s| s.is_ready()).map(|i| list.swap_remove(i))
        };
        let mut sender = match sender {
            Some(s) => s,
            None => {
                let Ok(uri) = addr.parse::<hyper::Uri>() else { return Err(req) };
                let (host, port) = (uri.host().unwrap_or("127.0.0.1").to_string(), uri.port_u16().unwrap_or(80));
                let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((host.as_str(), port))).await else {
                    self.unreachable(addr);
                    return Err(req);
                };
                let _ = stream.set_nodelay(true);
                let Ok((sender, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await else { return Err(req) };
                tokio::spawn(conn);
                sender
            }
        };
        if let Ok(v) = hyper::header::HeaderValue::from_str(&self.token) {
            req.headers_mut().insert(TOKEN_HEADER, v);
        }
        match sender.send_request(req).await {
            Ok(resp) => {
                let (addr, idle) = (addr.to_string(), &self.idle);
                // the connection goes back once the response is read; a
                // simple way: keep it if it's ready again later
                idle.lock().unwrap().entry(addr).or_default().push(sender);
                Ok(resp.map(|b| b.map_err(wasmtime_wasi_http::Error::from).boxed_unsync()))
            }
            Err(e) => {
                eprintln!("cluster: forwarding to {addr} failed: {e}");
                Ok(Response::builder().status(StatusCode::BAD_GATEWAY).body(crate::server::body("a cluster worker failed\n")).unwrap())
            }
        }
    }

    fn unreachable(&self, addr: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        for n in nodes.iter_mut().filter(|n| n.addr == addr) {
            // out of rotation until its next heartbeat
            n.last_seen = Instant::now() - NODE_TIMEOUT - Duration::from_secs(1);
        }
    }

    fn heartbeat(&self, hb: Heartbeat) -> J {
        let mut stats = HashMap::new();
        for (app, c, l) in hb.stats {
            if let Ok(id) = self.platform.accounts.id(&app) {
                stats.insert(id, (c, l));
            }
        }
        for (app, text) in hb.logs {
            crate::logs::write(&app, &text);
        }
        let mut nodes = self.nodes.lock().unwrap();
        match nodes.iter_mut().find(|n| n.id == hb.id) {
            Some(n) => {
                n.addr = hb.addr;
                n.cores = hb.cores;
                n.cpu = hb.cpu;
                n.last_seen = Instant::now();
                n.state = hb.state;
                n.stats = stats;
            }
            None => {
                eprintln!("cluster: worker {} joined from {} ({} cores)", hb.id, hb.addr, hb.cores);
                nodes.push(Node { id: hb.id, addr: hb.addr, cores: hb.cores, cpu: hb.cpu, last_seen: Instant::now(), draining: false, state: hb.state, sent: Arc::default(), stats });
            }
        }
        drop(nodes);
        json!({ "state": self.state().1 })
    }

    /// Removes a worker (gone, or scaled in); its counters stay counted.
    pub fn remove(&self, id: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(i) = nodes.iter().position(|n| n.id == id) {
            let n = nodes.remove(i);
            let mut retired = self.retired.lock().unwrap();
            for (account, (c, l)) in n.stats {
                let r = retired.entry(account).or_insert_with(|| (vec![0; Counter::ALL.len()], vec![0; LATENCY_BUCKETS]));
                for (a, b) in r.0.iter_mut().zip(&c) {
                    *a = a.wrapping_add(*b);
                }
                for (a, b) in r.1.iter_mut().zip(&l) {
                    *a = a.wrapping_add(*b);
                }
            }
            eprintln!("cluster: worker {id} left");
        }
    }
}

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder().status(status).header("content-type", "application/json").body(Full::new(body.into())).unwrap()
}

/// Starts the primary's cluster port: workers' state, components,
/// heartbeats, database and queue calls.
pub fn start_primary(platform: Arc<Platform>, listen: SocketAddr, token: String, local_cores: usize) -> Result<Arc<Primary>> {
    if token.len() < 16 {
        bail!("--cluster-token must be at least 16 characters");
    }
    let p = Arc::new(Primary {
        token,
        platform: platform.clone(),
        nodes: Mutex::default(),
        local_inflight: AtomicU64::new(0),
        local_cores,
        retired: Mutex::default(),
        hashes: Mutex::default(),
        idle: Mutex::default(),
        state_cache: Mutex::default(),
    });
    PRIMARY.set(p.clone()).ok();
    let listener = std::net::TcpListener::bind(listen).map_err(|e| format_err!("binding the cluster port {listen}: {e}"))?;
    listener.set_nonblocking(true)?;
    eprintln!("cluster: workers join at http://{listen}");
    let primary = p.clone();
    std::thread::Builder::new().name("cluster".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async move {
            let pools = Arc::new(crate::db::Pools::new(&platform.databases, platform.db_pool_size, &platform.db_tls).expect("pools"));
            let txs = Arc::new(crate::db::RemoteTxs::default());
            let sweeper = txs.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    sweeper.sweep(TX_IDLE).await;
                }
            });
            // workers not heard from leave the cluster
            let reaper = primary.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(NODE_TIMEOUT).await;
                    let gone: Vec<String> = reaper.nodes.lock().unwrap().iter().filter(|n| n.last_seen.elapsed() > NODE_TIMEOUT * 3).map(|n| n.id.clone()).collect();
                    for id in gone {
                        reaper.remove(&id);
                    }
                }
            });
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let (primary, pools, txs) = (primary.clone(), pools.clone(), txs.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let (primary, pools, txs) = (primary.clone(), pools.clone(), txs.clone());
                        async move { Ok::<_, std::convert::Infallible>(serve(primary, pools, txs, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
    })?;
    Ok(p)
}

async fn serve(p: Arc<Primary>, pools: Arc<crate::db::Pools>, txs: Arc<crate::db::RemoteTxs>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let authorized = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t.len() == p.token.len() && t.bytes().zip(p.token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0);
    if !authorized {
        return reply(StatusCode::UNAUTHORIZED, r#"{"error":"the cluster token is required"}"#);
    }
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return reply(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() }).to_string()),
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/cluster/state") => reply(StatusCode::OK, p.state().0),
        ("GET", "/cluster/binary") => match std::env::current_exe().and_then(std::fs::read) {
            Ok(b) => Response::builder().header("content-type", "application/octet-stream").body(Full::new(Bytes::from(b))).unwrap(),
            Err(e) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }).to_string()),
        },
        ("GET", path) if path.starts_with("/cluster/component/") => {
            let name = &path["/cluster/component/".len()..];
            let file = p.platform.current().deployment.apps.get(name).map(|a| a.component.clone());
            match file.map(std::fs::read) {
                Some(Ok(bytes)) => {
                    // workers run what this server compiled for itself
                    let compiled = if bytes.starts_with(b"\0asm") { p.platform.engine().precompile_component(&bytes).unwrap_or(bytes) } else { bytes };
                    Response::builder().header("content-type", "application/octet-stream").body(Full::new(Bytes::from(compiled))).unwrap()
                }
                _ => reply(StatusCode::NOT_FOUND, json!({ "error": format!("no app {name:?}") }).to_string()),
            }
        }
        ("POST", "/cluster/heartbeat") => match serde_json::from_slice::<Heartbeat>(&body) {
            Ok(hb) => reply(StatusCode::OK, p.heartbeat(hb).to_string()),
            Err(e) => reply(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() }).to_string()),
        },
        ("POST", "/cluster/sql") => {
            let call: J = serde_json::from_slice(&body).unwrap_or(J::Null);
            let timeout = Duration::from_millis(call.get("timeout_ms").and_then(J::as_u64).unwrap_or(5000));
            reply(StatusCode::OK, crate::db::serve_call(&pools, &txs, &call, timeout).await.to_string())
        }
        ("POST", "/cluster/storage") => {
            let call: J = serde_json::from_slice(&body).unwrap_or(J::Null);
            let data_dir = p.platform.data_dir.clone();
            let out = tokio::task::spawn_blocking(move || {
                let project = call.get("project").and_then(J::as_str).unwrap_or("").to_string();
                match crate::storage::run(&data_dir, &project, &call) {
                    Ok(v) => json!({ "ok": v }),
                    Err(e) => json!({ "err": e }),
                }
            })
            .await
            .unwrap_or_else(|e| json!({ "err": e.to_string() }));
            reply(StatusCode::OK, out.to_string())
        }
        ("POST", "/cluster/queue") => {
            let call: J = serde_json::from_slice(&body).unwrap_or(J::Null);
            let queue = call.get("queue").and_then(J::as_str).unwrap_or("").to_string();
            let payload = base64::engine::general_purpose::STANDARD.decode(call.get("payload").and_then(J::as_str).unwrap_or("")).unwrap_or_default();
            let delay = call.get("delay_ms").and_then(J::as_u64).unwrap_or(0);
            let out = match crate::jobs::enqueue(&p.platform, queue, payload, delay).await {
                Ok(()) => json!({ "ok": true }),
                Err(QueueError::UnknownQueue(q)) => json!({ "err": { "unknown_queue": q } }),
                Err(QueueError::TooLarge(n)) => json!({ "err": { "too_large": n } }),
                Err(QueueError::Unavailable(m)) => json!({ "err": { "unavailable": m } }),
            };
            reply(StatusCode::OK, out.to_string())
        }
        _ => reply(StatusCode::NOT_FOUND, r#"{"error":"not found"}"#),
    }
}
