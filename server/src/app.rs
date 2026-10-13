//! Functions (apps) and the per-instance store state.
//!
//! An app is a component with any combination of entry points: HTTP
//! (`wasi:http/incoming-handler`), events (`platform:sdk/events`, for queue
//! messages and cron ticks), and its own WIT interfaces that other apps call.

use crate::bindings::{events, websocket_handler};
use crate::cache::{self, AppCache};
use crate::db::{self, DbCtx};
use crate::platform::Env;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};
use wasmtime::Result;
use wasmtime::component::{Component, InstancePre, Linker, ResourceTable};
use wasmtime::error::Context;
use wasmtime::{Engine, ResourceLimiter, Store, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// What one instance may write to its standard output and error.
const OUTPUT_LIMIT: usize = 256 << 10;

/// Data owned by one `Store`, i.e. by one instance.
pub struct RequestState {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub limits: Limits,
    pub db: DbCtx,
    pub cpu: CpuMeter,
    /// The app's cache, shared by all its instances on this server.
    pub cache: Arc<AppCache>,
    /// Where this instance runs: worker, call depth, deadline.
    pub env: Env,
    /// Interfaces this app imports from other apps: interface → app name.
    pub links: Arc<HashMap<String, String>>,
    /// Time spent waiting on calls to other functions (not our CPU).
    pub call_wait: Duration,
    /// The app's account, for usage counters.
    pub app_id: usize,
    /// Outgoing HTTP: where the app may go, and its connections.
    pub outbound: crate::outbound::Hooks,
    /// The instance's standard output and error, for the app's log.
    pub output: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
    /// The project whose files it reaches (`platform:sdk/storage`).
    pub project: Option<Arc<str>>,
}

impl RequestState {
    /// Time this instance spent waiting rather than running.
    pub fn outbound_wait(&self) -> Duration {
        Duration::from_nanos(self.outbound.usage.wait_ns.load(Relaxed))
    }

    fn waited(&self) -> Duration {
        self.db.wait + self.call_wait + self.outbound_wait()
    }

    /// CPU used so far: what the epoch checks charged, plus the stretch
    /// since the last one (short requests never reach a check at all).
    pub fn cpu_used(&self) -> Duration {
        let cpu = &self.cpu;
        let resumed = cpu.base + Duration::from_nanos(cpu.resumed_ns.load(Relaxed));
        cpu.used + resumed.elapsed().saturating_sub(self.waited().saturating_sub(cpu.waited_seen))
    }

    /// This instance's usage counters (on the worker it runs on).
    pub fn counters(&self) -> &crate::stats::AppCounters {
        self.env.platform.stats.counters(self.env.worker, self.app_id)
    }
}

/// Approximates the guest's CPU time: wall time while it was actually
/// running, excluding time parked in a yield or waiting on the database or
/// on other functions.
pub struct CpuMeter {
    limit: Duration,
    used: Duration,
    base: Instant,
    /// When the guest last resumed from a yield, in ns since `base`; written
    /// by the yield future when the worker gets back to this guest.
    resumed_ns: Arc<AtomicU64>,
    /// `waited()` as of the last measurement.
    waited_seen: Duration,
}

impl CpuMeter {
    fn new(limit: Duration) -> Self {
        Self { limit, used: Duration::ZERO, base: Instant::now(), resumed_ns: Arc::default(), waited_seen: Duration::ZERO }
    }
}

/// Per-request limits, shared by every request of the server.
#[derive(Clone, Copy)]
pub struct RequestLimits {
    pub max_memory: usize,
    /// CPU budget; time waiting on the database, other functions or the core
    /// does not count.
    pub cpu_limit: Duration,
}

/// Caps the store's total linear memory and records its peak.
pub struct Limits {
    pub max_memory: usize,
    /// Total bytes across all linear memories of the store. Wasm memories
    /// never shrink, so this is also the peak.
    pub memory: usize,
}

impl ResourceLimiter for Limits {
    fn memory_growing(&mut self, current: usize, desired: usize, _maximum: Option<usize>) -> Result<bool> {
        let total = self.memory + (desired - current);
        if total > self.max_memory {
            return Ok(false);
        }
        self.memory = total;
        Ok(true)
    }

    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> Result<bool> {
        Ok(desired <= 100_000)
    }
}

impl WasiView for RequestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl WasiHttpView for RequestState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.outbound,
        }
    }
}

/// A deployed function: a component whose imports are already resolved and
/// type-checked, ready to be instantiated per invocation.
pub struct App {
    /// Index into per-app statistics.
    pub id: usize,
    pub name: String,
    pub pre: InstancePre<RequestState>,
    /// Present if the app handles HTTP requests.
    pub http: Option<ProxyPre<RequestState>>,
    /// Present if the app receives queue messages and cron ticks.
    pub events: Option<events::GuestIndices>,
    /// Present if the app accepts WebSocket connections.
    pub websocket: Option<websocket_handler::GuestIndices>,
    pub config: AppConfig,
}

/// Per-app deployment settings.
#[derive(Clone)]
pub struct AppConfig {
    /// Database names this app may use.
    pub databases: Arc<[String]>,
    /// Limit for each of this app's database calls.
    pub db_timeout: Duration,
    /// Environment variables the guest sees (e.g. secrets).
    pub env: Arc<[(String, String)]>,
    pub cache: Arc<AppCache>,
    /// Interfaces imported from other apps: interface → app name.
    pub links: Arc<HashMap<String, String>>,
    /// Path on the app's site where WebSocket upgrades are accepted.
    pub websocket_path: Option<String>,
    /// Hosts the app may send HTTP requests to.
    pub outbound: Arc<crate::outbound::Policy>,
    /// The app's own names for its databases (deployed apps).
    pub db_aliases: Arc<HashMap<String, String>>,
    /// The project it belongs to (deployed apps): its files.
    pub project: Option<Arc<str>>,
}

impl App {
    pub fn new(linker: &Linker<RequestState>, id: usize, name: &str, component: &Component, config: AppConfig) -> Result<App> {
        let pre = linker.instantiate_pre(component).with_context(|| format!("app {name:?}"))?;
        let http = ProxyPre::new(pre.clone()).ok();
        let events = events::GuestIndices::new(&pre).ok();
        let websocket = websocket_handler::GuestIndices::new(&pre).ok();
        if config.websocket_path.is_some() && websocket.is_none() {
            wasmtime::bail!("--websocket {name}=...: {name:?} doesn't export platform:sdk/websocket-handler");
        }
        Ok(App { id, name: name.to_string(), pre, http, events, websocket, config })
    }

    /// Creates the store for one invocation: fresh WASI state, a memory cap,
    /// a CPU budget and the worker's database pools.
    pub fn new_store(&self, env: &Env) -> Store<RequestState> {
        let limits = env.platform.limits;
        // stdout and stderr together, as a terminal shows them; what doesn't
        // fit is dropped (the guest's writes fail)
        let output = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(OUTPUT_LIMIT);
        let mut store = Store::new(
            self.pre.engine(),
            RequestState {
                table: ResourceTable::new(),
                wasi: WasiCtx::builder().stdout(output.clone()).stderr(output.clone()).envs(&self.config.env).build(),
                http: WasiHttpCtx::new(),
                limits: Limits { max_memory: limits.max_memory, memory: 0 },
                db: DbCtx::new(env.pools.clone(), self.config.databases.clone(), self.config.db_aliases.clone(), self.config.db_timeout),
                cpu: CpuMeter::new(limits.cpu_limit),
                cache: self.config.cache.clone(),
                env: env.clone(),
                links: self.config.links.clone(),
                call_wait: Duration::ZERO,
                app_id: self.id,
                outbound: crate::outbound::Hooks {
                    app: self.name.as_str().into(),
                    policy: self.config.outbound.clone(),
                    platform: env.platform.clone(),
                    worker: env.worker,
                    app_id: self.id,
                    usage: Arc::default(),
                },
                output,
                project: self.config.project.clone(),
            },
        );
        store.limiter(|s| &mut s.limits);
        // Running guest code reaches this callback at least once per epoch
        // tick. It charges the time since the guest last resumed, minus
        // waits, against the CPU budget. Within budget, the guest yields to
        // the worker's event loop so a long computation cannot starve the
        // other requests on its core.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|mut cx| {
            let s = cx.data_mut();
            let waited = s.waited();
            let cpu = &mut s.cpu;
            let resumed = cpu.base + Duration::from_nanos(cpu.resumed_ns.load(Relaxed));
            cpu.used += resumed.elapsed().saturating_sub(waited.saturating_sub(cpu.waited_seen));
            cpu.waited_seen = waited;
            if cpu.used > cpu.limit {
                return Ok(UpdateDeadline::Interrupt);
            }
            let (resumed_ns, base) = (cpu.resumed_ns.clone(), cpu.base);
            Ok(UpdateDeadline::YieldCustom(
                1,
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    resumed_ns.store(base.elapsed().as_nanos() as u64, Relaxed);
                }),
            ))
        });
        store
    }
}

/// The host interfaces every app may import.
pub fn linker(engine: &Engine) -> Result<Linker<RequestState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    db::add_to_linker(&mut linker)?;
    cache::add_to_linker(&mut linker)?;
    crate::jobs::add_to_linker(&mut linker)?;
    crate::ws::add_to_linker(&mut linker)?;
    crate::storage::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Loads a `.cwasm` (precompiled, mmapped) or a `.wasm` (compiled now).
pub fn load_component(engine: &Engine, name: &str, path: &Path) -> Result<Component> {
    let start = Instant::now();
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .with_context(|| format!("reading {}", path.display()))?;
    let component = if magic != *b"\0asm" {
        // SAFETY: the artifact must come from a trusted `wasm-server compile`
        // run; deserialization trusts the machine code it contains.
        unsafe { Component::deserialize_file(engine, path) }
    } else {
        eprintln!("warning: {name}: compiling {} at startup; use `wasm-server compile` for instant loads", path.display());
        Component::from_file(engine, path)
    }
    .with_context(|| format!("loading {}", path.display()))?;
    eprintln!("loaded app {name:?} from {} in {:?}", path.display(), start.elapsed());
    Ok(component)
}
