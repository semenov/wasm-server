//! Running functions. Everything an instance can reach lives in `Platform`;
//! `Env` says where a particular instance runs. A function runs in one of
//! three ways, all with the same limits and accounting:
//!
//! - an HTTP request (`http`),
//! - an event: a queue message or a cron tick (`event`),
//! - a typed call from another function (`call`).

use crate::app::{App, RequestLimits, RequestState};
use crate::bindings::events::{Event, Kind};
use crate::bindings::websocket::Message;
use crate::bindings::websocket_handler::ConnectInfo;
use crate::ws::Hub;
use crate::db::{DbConfig, DbTls, Pools};
use crate::deployment::Source;
use crate::jobs::JobStore;
use crate::loader::{self, Accounts, ComponentCache, Current, Defaults, Summary};
use crate::stats::{Counter, InstanceUsage, Stats};
use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body::Body;
use hyper::{Request, Response};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::component::Val;
use wasmtime::error::Context;
use wasmtime::{Engine, Result, Store, bail, format_err};
use wasmtime_wasi_http::WasiHttpView;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Calls between functions may nest this deep (A calls B calls C ...).
pub const MAX_CALL_DEPTH: u32 = 8;

/// Shared by all workers. What runs (`current`) is swapped atomically on
/// reload; everything else is fixed at startup.
pub struct Platform {
    current: ArcSwap<Current>,
    pub accounts: Accounts,
    pub stats: Stats,
    /// Wall-clock limit per request, including everything it calls.
    pub timeout: Duration,
    pub limits: RequestLimits,
    pub databases: Vec<DbConfig>,
    pub db_pool_size: usize,
    pub db_tls: DbTls,
    /// Durable queues and cron.
    pub jobs: Option<JobStore>,
    /// Open WebSocket connections and channels.
    pub hub: Hub,
    /// Required for `/_platform/` writes (reload, deploy) and, when set,
    /// for reads too.
    pub admin_token: Option<String>,
    /// Version history and other state.
    pub data_dir: std::path::PathBuf,
    pub backups: Option<crate::backup::BackupConfig>,
    reload: Reloader,
}

/// What a reload needs.
pub struct Reloader {
    pub engine: Engine,
    pub source: Source,
    pub defaults: Defaults,
    pub components: Mutex<ComponentCache>,
    version: AtomicU64,
}

impl Reloader {
    pub fn new(engine: Engine, source: Source, defaults: Defaults, components: ComponentCache) -> Self {
        Self { engine, source, defaults, components: Mutex::new(components), version: AtomicU64::new(1) }
    }
}

/// Everything about the platform that `Platform::new` doesn't derive.
pub struct Parts {
    pub current: Current,
    pub accounts: Accounts,
    pub stats: Stats,
    pub timeout: Duration,
    pub limits: RequestLimits,
    pub databases: Vec<DbConfig>,
    pub db_pool_size: usize,
    pub db_tls: DbTls,
    pub jobs: Option<JobStore>,
    pub admin_token: Option<String>,
    pub data_dir: std::path::PathBuf,
    pub backups: Option<crate::backup::BackupConfig>,
    pub reload: Reloader,
}

impl Platform {
    pub fn new(p: Parts) -> Self {
        Self {
            current: ArcSwap::from_pointee(p.current),
            accounts: p.accounts,
            stats: p.stats,
            timeout: p.timeout,
            limits: p.limits,
            databases: p.databases,
            db_pool_size: p.db_pool_size,
            db_tls: p.db_tls,
            jobs: p.jobs,
            hub: Hub::default(),
            admin_token: p.admin_token,
            data_dir: p.data_dir,
            backups: p.backups,
            reload: p.reload,
        }
    }

    pub fn engine(&self) -> &Engine {
        &self.reload.engine
    }

    /// What runs now. Holding the `Arc` keeps that version alive, so a
    /// request started before a reload finishes on the version it started on.
    pub fn current(&self) -> Arc<Current> {
        self.current.load_full()
    }

    /// Reads the deployment again and switches to it. On any error the
    /// running version stays. Databases can't change without a restart:
    /// every worker holds connections to them.
    pub fn reload(&self) -> Result<Summary> {
        let r = &self.reload;
        let mut components = r.components.lock().unwrap();
        let deployment = r.source.load(&self.data_dir)?;
        let current = self.current();
        if deployment.databases != current.deployment.databases {
            bail!("databases changed; restart the server to apply that");
        }
        let version = r.version.fetch_add(1, Relaxed) + 1;
        let (next, summary) = loader::build(&r.engine, deployment, &mut components, &self.accounts, &r.defaults, version)?;
        self.current.store(Arc::new(next));
        Ok(summary)
    }
}

/// Where an instance runs. Cheap to clone: one per instance.
#[derive(Clone)]
pub struct Env {
    pub platform: Arc<Platform>,
    /// Index of the worker thread, for per-worker stats.
    pub worker: usize,
    /// The worker's database connections.
    pub pools: Arc<Pools>,
    /// 0 for an HTTP request or event; +1 per nested call.
    pub depth: u32,
    /// The whole invocation, nested calls included, must finish by this.
    pub deadline: tokio::time::Instant,
}

impl Env {
    pub fn new(platform: Arc<Platform>, worker: usize, pools: Arc<Pools>) -> Self {
        let deadline = tokio::time::Instant::now() + platform.timeout;
        Self { platform, worker, pools, depth: 0, deadline }
    }
}

/// Records an instance's resource use when it's done, and puts what it
/// wrote into its app's log.
fn record(env: &Env, app: &App, store: &Store<RequestState>, started: Instant) -> InstanceUsage {
    let state = store.data();
    let usage = InstanceUsage {
        wall_ns: started.elapsed().as_nanos() as u64,
        cpu_ns: state.cpu_used().as_nanos() as u64,
        mem_peak: state.limits.memory as u64,
        db_queries: state.db.queries,
        db_wait_ns: state.db.wait.as_nanos() as u64,
        outbound_requests: state.outbound.usage.requests.load(std::sync::atomic::Ordering::Relaxed),
        outbound_wait_ns: state.outbound_wait().as_nanos() as u64,
    };
    env.platform.stats.counters(env.worker, app.id).instance_done(&usage);
    let output = state.output.contents();
    if !output.is_empty() {
        crate::logs::write(&app.name, &String::from_utf8_lossy(&output));
    }
    usage
}

/// Instantiates a fresh copy of `app` and runs its HTTP handler.
pub async fn http<B>(env: Env, app: Arc<App>, req: Request<B>, log: Arc<crate::logs::RequestLog>) -> Result<Response<HyperOutgoingBody>>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<wasmtime_wasi_http::Error>,
{
    let Some(proxy) = app.http.clone() else {
        bail!("app {:?} has no HTTP handler", app.name);
    };
    let mut store = app.new_store(&env);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let req = store.data_mut().http().new_incoming_request(Scheme::Http, req)?;
    let out = store.data_mut().http().new_response_outparam(sender)?;

    // The guest keeps running in its own task after `response-outparam::set`
    // so it can stream the body; the task owns the store, which is dropped
    // (returning its slot to the pool) when the handler returns.
    let task = tokio::spawn(async move {
        let started = Instant::now();
        let instance = proxy.instantiate_async(&mut store).await?;
        let result = instance.wasi_http_incoming_handler().call_handle(&mut store, req, out).await;
        *log.usage.lock().unwrap() = Some(record(&env, &app, &store, started));
        result
    });
    // If this future is dropped (wall-clock timeout) before the guest has
    // responded, stop the guest too: it may be blocked on I/O, which the CPU
    // limit does not cover. Once a response exists the task is left to
    // finish streaming its body.
    let mut task = AbortOnDrop(Some(task));

    match receiver.await {
        Ok(Ok(resp)) => {
            task.0.take();
            Ok(resp)
        }
        Ok(Err(code)) => Err(format_err!("guest returned error code: {code:?}")),
        // The sender was dropped with the store: the handler finished or
        // trapped without ever setting a response.
        Err(_) => match task.0.take().unwrap().await.context("guest task panicked")? {
            Ok(()) => Err(format_err!("guest never called response-outparam::set")),
            Err(e) => Err(e.context("guest trapped")),
        },
    }
}

/// Delivers a queue message or cron tick. `Ok(Err(..))` is the guest
/// declining the event (it will be retried); `Err` is a trap or setup error.
pub async fn event(env: Env, app: &App, kind: Kind, id: String, name: String, payload: Vec<u8>, attempt: u32) -> Result<Result<(), String>> {
    let Some(indices) = &app.events else {
        bail!("app {:?} doesn't export platform:sdk/events", app.name);
    };
    let mut store = app.new_store(&env);
    let started = Instant::now();
    let deadline = env.deadline;
    let result = tokio::time::timeout_at(deadline, async {
        let instance = app.pre.instantiate_async(&mut store).await?;
        let guest = indices.load(&mut store, &instance)?;
        let event = Event { id, kind, name, payload, attempt };
        guest.call_handle(&mut store, &event).await
    })
    .await
    .unwrap_or_else(|_| Err(format_err!("timed out")));
    let _ = record(&env, app, &store, started);
    result
}

/// Runs one of an app's WebSocket handlers in a fresh instance.
async fn websocket<T>(
    env: Env,
    app: &App,
    f: impl for<'a> FnOnce(
        &'a crate::bindings::websocket_handler::Guest,
        &'a mut Store<RequestState>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>,
) -> Result<T> {
    let Some(indices) = &app.websocket else {
        bail!("app {:?} doesn't export platform:sdk/websocket-handler", app.name);
    };
    let mut store = app.new_store(&env);
    let started = Instant::now();
    let result = tokio::time::timeout_at(env.deadline, async {
        let instance = app.pre.instantiate_async(&mut store).await?;
        let guest = indices.load(&mut store, &instance)?;
        f(&guest, &mut store).await
    })
    .await
    .unwrap_or_else(|_| Err(format_err!("timed out")));
    let _ = record(&env, app, &store, started);
    result
}

pub async fn ws_connect(env: Env, app: &App, info: ConnectInfo) -> Result<Result<(), String>> {
    websocket(env, app, move |guest, store| Box::pin(async move { guest.call_on_connect(store, &info).await })).await
}

pub async fn ws_message(env: Env, app: &App, conn: String, msg: Message) -> Result<()> {
    websocket(env, app, move |guest, store| Box::pin(async move { guest.call_on_message(store, &conn, &msg).await })).await
}

pub async fn ws_close(env: Env, app: &App, conn: String, code: u16) -> Result<()> {
    websocket(env, app, move |guest, store| Box::pin(async move { guest.call_on_close(store, &conn, code).await })).await
}

/// Calls `function` of `interface` exported by `target`, on behalf of a
/// caller in `env`. Values are copied between the two instances' memories.
pub async fn call(env: &Env, target: &App, interface: &str, function: &str, params: &[Val], results: &mut [Val]) -> Result<()> {
    if env.depth >= MAX_CALL_DEPTH {
        bail!("call depth limit ({MAX_CALL_DEPTH}) exceeded calling {interface}#{function}");
    }
    if params.iter().any(contains_resource) {
        bail!("{interface}#{function}: resources can't be passed between functions");
    }
    let env = Env { depth: env.depth + 1, ..env.clone() };
    let counters = env.platform.stats.counters(env.worker, target.id);
    counters.request();
    counters.add(Counter::Calls, 1);
    let mut store = target.new_store(&env);
    let started = Instant::now();
    let result = tokio::time::timeout_at(env.deadline, async {
        let instance = target.pre.instantiate_async(&mut store).await?;
        let iface = instance
            .get_export_index(&mut store, None, interface)
            .ok_or_else(|| format_err!("{:?} doesn't export {interface}", target.name))?;
        let func_index = instance
            .get_export_index(&mut store, Some(&iface), function)
            .ok_or_else(|| format_err!("{:?} doesn't export {interface}#{function}", target.name))?;
        let func = instance.get_func(&mut store, func_index).ok_or_else(|| format_err!("{interface}#{function} is not a function"))?;
        func.call_async(&mut store, params, results).await
    })
    .await
    .unwrap_or_else(|_| Err(format_err!("timed out")));
    let _ = record(&env, target, &store, started);
    if let Err(e) = &result {
        counters.error();
        return Err(format_err!("call to {}'s {interface}#{function} failed: {e:?}", target.name));
    }
    if results.iter().any(contains_resource) {
        bail!("{interface}#{function}: resources can't be returned between functions");
    }
    Ok(())
}

fn contains_resource(v: &Val) -> bool {
    match v {
        Val::Resource(_) => true,
        Val::List(items) | Val::Tuple(items) => items.iter().any(contains_resource),
        Val::Record(fields) => fields.iter().any(|(_, v)| contains_resource(v)),
        Val::Variant(_, Some(v)) | Val::Option(Some(v)) => contains_resource(v),
        Val::Result(Ok(Some(v)) | Err(Some(v))) => contains_resource(v),
        _ => false,
    }
}

/// Event kind for the store.
pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Queue => "queue",
        Kind::Cron => "cron",
    }
}

struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
