//! Data plane: thread-per-core HTTP serving with one fresh instance per request.

use crate::billing;
use crate::db::Pools;
use crate::jobs;
use crate::platform::{self, Env, Platform};
use crate::routes::Matched;
use crate::stats::Counter;
use wasmtime::error::Context;
use wasmtime::{Result, format_err};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use wasmtime::{Engine, Trap};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Granularity of the epoch clock that bounds guest execution time.
const EPOCH_TICK: Duration = Duration::from_millis(10);

pub struct ServeOpts {
    pub listen: SocketAddr,
    pub workers: usize,
    /// Threads delivering queue messages and cron ticks.
    pub job_workers: usize,
}

pub fn serve(shared: Arc<Platform>, opts: ServeOpts) -> Result<()> {
    spawn_epoch_ticker(shared.engine().clone());
    billing::spawn(shared.clone())?;
    spawn_signal_handler(shared.clone());
    jobs::spawn_workers(shared.clone(), opts.workers, opts.job_workers)?;
    jobs::spawn_cron(shared.clone())?;
    if let Some(config) = shared.backups.clone() {
        eprintln!("backups: every {:?} to {} (keeping {})", config.every, config.dir.display(), config.keep);
        crate::backup::spawn(shared.clone(), config)?;
    }

    let sources = bind(opts.listen, opts.workers)?;
    eprintln!("listening on http://{} with {} workers", opts.listen, opts.workers);

    let handles: Vec<_> = sources
        .into_iter()
        .enumerate()
        .map(|(i, source)| {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || worker(i, source, shared))
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| format_err!("worker panicked"))??;
    }
    Ok(())
}

/// One single-threaded runtime per core: no work stealing and no cross-core
/// synchronization on the request path. Database pools are per worker too,
/// so a connection's I/O always runs on the core that uses it.
fn worker(id: usize, source: ConnSource, shared: Arc<Platform>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let pools = Arc::new(Pools::new(&shared.databases, shared.db_pool_size, &shared.db_tls)?);
    rt.block_on(async move {
        let mut source = match source {
            ConnSource::Listener(l) => AsyncSource::Listener(tokio::net::TcpListener::from_std(l)?),
            ConnSource::Channel(rx) => AsyncSource::Channel(rx),
        };
        loop {
            let stream = match &mut source {
                AsyncSource::Listener(l) => match l.accept().await {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        eprintln!("accept error: {e}");
                        continue;
                    }
                },
                AsyncSource::Channel(rx) => match rx.recv().await {
                    Some(stream) => tokio::net::TcpStream::from_std(stream)?,
                    None => return Ok(()),
                },
            };
            let _ = stream.set_nodelay(true);
            let shared = shared.clone();
            let pools = pools.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let shared = shared.clone();
                    let pools = pools.clone();
                    async move { Ok::<_, Infallible>(handle(shared, id, pools, req).await) }
                });
                // Client disconnects are normal; nothing useful to report.
                let _ = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    })
}

/// Paths under this prefix are the platform's own: stats, and queue and
/// cron deliveries, which only the platform itself may send to apps.
pub const PLATFORM_PREFIX: &str = "/_platform/";

async fn handle(shared: Arc<Platform>, worker: usize, pools: Arc<Pools>, req: Request<Incoming>) -> Response<HyperOutgoingBody> {
    let path = req.uri().path();
    if let Some(internal) = path.strip_prefix(PLATFORM_PREFIX) {
        let internal = internal.to_string();
        return crate::admin::handle(shared, req, &internal).await;
    }
    if path == "/_platform" {
        let mut resp = text(StatusCode::PERMANENT_REDIRECT, "");
        resp.headers_mut().insert(hyper::header::LOCATION, "/_platform/".parse().unwrap());
        return resp;
    }
    let host = req.headers().get(hyper::header::HOST).and_then(|h| h.to_str().ok()).or(req.uri().host());
    // Held for the whole request: it finishes on the version it started on.
    let current = shared.current();
    let (site, site_path) = match current.routes.resolve(host, path) {
        Some(Matched::Domain(site)) => (site, path),
        // Static files of a path-routed site are relative to its prefix.
        Some(Matched::Prefix(site)) => {
            let rest = path.strip_prefix('/').and_then(|p| p.strip_prefix(site.name.as_str()));
            (site, rest.filter(|r| r.is_empty() || r.starts_with('/')).map_or(path, |r| if r.is_empty() { "/" } else { r }))
        }
        None => return text(StatusCode::NOT_FOUND, "no site here\n"),
    };
    if shared.billing.is_blocked(site.id) {
        return text(StatusCode::PAYMENT_REQUIRED, "out of credits\n");
    }
    if let Some(files) = &site.files {
        if let Some(resp) = files.serve(req.method(), site_path, req.headers()) {
            let counters = shared.stats.counters(worker, site.id);
            counters.add(Counter::StaticRequests, 1);
            let len = resp.headers().get(hyper::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
            if req.method() == hyper::Method::GET {
                counters.add(Counter::EgressBytes, len.unwrap_or(0));
            }
            return resp;
        }
    }
    if let Some(app) = site.app.clone().filter(|a| a.config.websocket_path.as_deref() == Some(site_path)) {
        if crate::ws::is_upgrade(&req) {
            return crate::ws::accept(shared.clone(), worker, pools, app, req).await;
        }
    }
    let Some(app) = site.app.clone().filter(|a| a.http.is_some()) else {
        return text(StatusCode::NOT_FOUND, "not found\n");
    };
    let counters = shared.stats.counters(worker, app.id);
    counters.request();
    counters.add(Counter::HttpRequests, 1);
    let name = app.name.clone();
    let app_id = app.id;
    let env = Env::new(shared.clone(), worker, pools);
    let result = tokio::time::timeout(shared.timeout, platform::http(env, app, req)).await;
    match result {
        Ok(Ok(resp)) => count_egress(resp, shared.clone(), worker, app_id),
        Ok(Err(e)) if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) => {
            counters.timeout();
            text(StatusCode::GATEWAY_TIMEOUT, "guest exceeded its CPU limit\n")
        }
        Ok(Err(e)) => {
            counters.error();
            eprintln!("{name}: {e:?}");
            text(StatusCode::INTERNAL_SERVER_ERROR, "guest error\n")
        }
        Err(_) => {
            counters.timeout();
            text(StatusCode::GATEWAY_TIMEOUT, "guest timed out\n")
        }
    }
}

/// Counts response body bytes as they're actually sent.
fn count_egress(resp: Response<HyperOutgoingBody>, platform: Arc<Platform>, worker: usize, app: usize) -> Response<HyperOutgoingBody> {
    resp.map(|body| {
        body.map_frame(move |frame| {
            if let Some(data) = frame.data_ref() {
                platform.stats.counters(worker, app).add(Counter::EgressBytes, data.len() as u64);
            }
            frame
        })
        .boxed_unsync()
    })
}

pub(crate) fn body(content: impl Into<Bytes>) -> HyperOutgoingBody {
    Full::new(content.into()).map_err(|e: Infallible| match e {}).boxed_unsync()
}

fn text(status: StatusCode, content: &'static str) -> Response<HyperOutgoingBody> {
    let mut resp = Response::new(body(content));
    *resp.status_mut() = status;
    resp
}

/// SIGHUP reloads the deployment. SIGINT or SIGTERM let billing store the
/// usage it hasn't written yet, then exit.
fn spawn_signal_handler(platform: Arc<Platform>) {
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate()).unwrap();
                let mut hup = signal(SignalKind::hangup()).unwrap();
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => break,
                        _ = term.recv() => break,
                        _ = hup.recv() => {
                            let platform = platform.clone();
                            match tokio::task::spawn_blocking(move || platform.reload()).await {
                                Ok(Ok(s)) => eprintln!("reload (SIGHUP): {} apps, {} sites, loaded [{}]", s.apps, s.sites, s.loaded.join(", ")),
                                Ok(Err(e)) => eprintln!("reload (SIGHUP) failed, still running the previous version: {e:?}"),
                                Err(e) => eprintln!("reload (SIGHUP): {e}"),
                            }
                        }
                    }
                }
            });
            eprintln!("shutting down: saving usage");
            platform.billing.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            // The billing thread exits the process after its final flush.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        })
        .unwrap();
}

/// Keeps the engine epoch equal to the wall-clock time since startup in ticks.
/// Sleeps can overshoot a lot (macOS coalesces timers on idle threads), so
/// each wakeup catches up on every tick that actually elapsed rather than
/// counting one tick per sleep.
fn spawn_epoch_ticker(engine: Engine) {
    std::thread::Builder::new()
        .name("epoch".into())
        .spawn(move || {
            let start = Instant::now();
            let mut epoch = 0u128;
            loop {
                std::thread::sleep(EPOCH_TICK);
                let now = start.elapsed().as_micros() / EPOCH_TICK.as_micros();
                while epoch < now {
                    engine.increment_epoch();
                    epoch += 1;
                }
            }
        })
        .unwrap();
}

/// Where a worker gets its connections from.
enum ConnSource {
    /// Its own `SO_REUSEPORT` socket (Linux).
    Listener(TcpListener),
    /// Connections handed over by the acceptor thread (other platforms).
    Channel(mpsc::UnboundedReceiver<TcpStream>),
}

enum AsyncSource {
    Listener(tokio::net::TcpListener),
    Channel(mpsc::UnboundedReceiver<TcpStream>),
}

/// On Linux every worker gets its own `SO_REUSEPORT` socket and the kernel
/// load-balances connections between them. macOS does not balance across
/// reuseport sockets, and workers sharing one socket pick up connections very
/// unevenly, so there a dedicated thread accepts and deals connections out
/// round-robin.
fn bind(addr: SocketAddr, workers: usize) -> Result<Vec<ConnSource>> {
    use socket2::{Domain, Socket, Type};
    let make = |reuseport: bool| -> Result<TcpListener> {
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(reuseport)?;
        socket.bind(&addr.into()).with_context(|| format!("binding {addr}"))?;
        socket.listen(4096)?;
        Ok(socket.into())
    };
    if cfg!(target_os = "linux") {
        return (0..workers)
            .map(|_| {
                let listener = make(true)?;
                listener.set_nonblocking(true)?;
                Ok(ConnSource::Listener(listener))
            })
            .collect();
    }
    let listener = make(false)?;
    let (senders, receivers): (Vec<_>, Vec<_>) = (0..workers).map(|_| mpsc::unbounded_channel()).unzip();
    std::thread::Builder::new().name("acceptor".into()).spawn(move || {
        for (i, conn) in listener.incoming().enumerate() {
            match conn.and_then(|c| c.set_nonblocking(true).map(|()| c)) {
                Ok(conn) => {
                    let _ = senders[i % senders.len()].send(conn);
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
    })?;
    Ok(receivers.into_iter().map(ConnSource::Channel).collect())
}
