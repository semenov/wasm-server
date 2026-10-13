//! In-process microbenchmark: measures the runtime cost per request with no
//! network or HTTP parsing involved, to separate Wasmtime costs from server
//! costs.

use crate::app::{App, RequestLimits};
use crate::db::{self, Pools};
use crate::deployment::{Deployment, Source};
use crate::loader::{Accounts, ComponentCache, Current, Defaults};
use crate::platform::{Env, Parts, Platform, Reloader};
use crate::routes::Routes;
use crate::stats::Stats;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::{Result, format_err};
use wasmtime_wasi_http::WasiHttpView;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Mode {
    /// Create a store, instantiate the component, drop it.
    Instantiate,
    /// Instantiate and run the HTTP handler, collecting the response body.
    Handle,
}

pub fn run(engine: wasmtime::Engine, app: App, mode: Mode, threads: usize, duration: Duration, path: &str, max_memory: usize) -> Result<()> {
    let name = app.name.clone();
    let accounts = Accounts::default();
    accounts.id(&name)?;
    let current = Current {
        routes: Routes::new(vec![app], Vec::new(), &[], &accounts)?,
        queues: Default::default(),
        crons: Vec::new(),
        deployment: Deployment::default(),
        version: 1,
    };
    let defaults = Defaults { db_timeout: Duration::from_secs(5), cache_bytes: 64 << 20 };
    let platform = Arc::new(Platform::new(Parts {
        current,
        accounts,
        stats: Stats::new(threads, 1),
        timeout: Duration::from_secs(3600),
        // Without an epoch ticker the epoch never advances, so the CPU limit
        // never triggers.
        limits: RequestLimits { max_memory, cpu_limit: Duration::MAX },
        databases: Vec::new(),
        db_pool_size: 1,
        db_tls: db::tls(None)?,
        jobs: None,
        admin_token: None,
        data_dir: std::env::temp_dir(),
        backups: None,
        reload: Reloader::new(engine, Source::Flags(Deployment::default()), defaults, ComponentCache::default()),
    }));
    let app = platform.current().routes.app(&name).unwrap().clone();
    let pools = Arc::new(Pools::new(&[], 1, &platform.db_tls)?);
    let handles: Vec<_> = (0..threads)
        .map(|worker| {
            let app = app.clone();
            let env = Env::new(platform.clone(), worker, pools.clone());
            let path = path.to_string();
            std::thread::spawn(move || -> Result<(u64, Vec<u32>)> {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                rt.block_on(async {
                    let mut latencies_ns = Vec::with_capacity(1 << 20);
                    let end = Instant::now() + duration;
                    let mut ops = 0;
                    while Instant::now() < end {
                        let t = Instant::now();
                        match mode {
                            Mode::Instantiate => {
                                let mut store = app.new_store(&env);
                                app.pre.instantiate_async(&mut store).await?;
                            }
                            Mode::Handle => handle(&app, &path, &env).await?,
                        }
                        latencies_ns.push(t.elapsed().as_nanos().min(u32::MAX as u128) as u32);
                        ops += 1;
                    }
                    Ok((ops, latencies_ns))
                })
            })
        })
        .collect();

    let mut total = 0;
    let mut all = Vec::new();
    for h in handles {
        let (ops, lat) = h.join().map_err(|_| format_err!("bench thread panicked"))??;
        total += ops;
        all.extend(lat);
    }
    all.sort_unstable();
    let pct = |p: f64| all[((all.len() as f64 * p) as usize).min(all.len() - 1)] as f64 / 1000.0;
    println!(
        "threads={threads} ops={total} ops/s={:.0} per-thread ops/s={:.0} p50={:.1}us p99={:.1}us p99.9={:.1}us",
        total as f64 / duration.as_secs_f64(),
        total as f64 / duration.as_secs_f64() / threads as f64,
        pct(0.50),
        pct(0.99),
        pct(0.999),
    );
    Ok(())
}

async fn handle(app: &App, path: &str, env: &Env) -> Result<()> {
    let proxy = app.http.clone().ok_or_else(|| format_err!("{:?} has no HTTP handler", app.name))?;
    let mut store = app.new_store(env);
    let req = hyper::Request::builder()
        .uri(format!("http://localhost{path}"))
        .body(Empty::<Bytes>::new())?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let req = store.data_mut().http().new_incoming_request(Scheme::Http, req)?;
    let out = store.data_mut().http().new_response_outparam(sender)?;
    let task = tokio::spawn(async move {
        let instance = proxy.instantiate_async(&mut store).await?;
        instance.wasi_http_incoming_handler().call_handle(&mut store, req, out).await
    });
    let resp = receiver.await.map_err(|_| format_err!("no response"))??;
    resp.into_body().collect().await?;
    task.await??;
    Ok(())
}
