mod admin;
mod app;
mod autoscale;
mod backup;
mod bench;
mod billing;
mod cluster;
mod bindings;
mod client;
mod cache;
mod db;
mod deployment;
mod engine;
mod flags;
mod functions;
mod jobs;
mod loader;
mod metrics;
mod logs;
mod managed;
mod outbound;
mod platform;
mod routes;
mod server;
mod static_files;
mod stats;
mod tls;
mod versions;
mod ws;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::Result;
use wasmtime::error::Context;

#[derive(Parser)]
#[command(about = "A fast application server for wasi:http WebAssembly components")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a component ahead of time into a `.cwasm` artifact.
    Compile {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Serve apps and sites.
    Serve {
        /// Deployment file (TOML) describing apps, sites, databases, queues
        /// and cron, instead of the flags below. Re-read on reload
        /// (SIGHUP or POST /_platform/reload).
        #[arg(long, conflicts_with_all = ["apps", "statics", "databases"])]
        config: Option<PathBuf>,
        /// Token for /_platform/ (also WASM_SERVER_ADMIN_TOKEN). Enables
        /// reloads and deploys, and protects stats and billing.
        #[arg(long, env = "WASM_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
        /// App to deploy as NAME=PATH (`.cwasm` or `.wasm`). Requests to
        /// `/NAME/...` go to that app; an app named `default` (or the only
        /// app) receives everything else.
        #[arg(short, long = "app", value_parser = parse_pair)]
        apps: Vec<(String, String)>,
        /// Static files for a site, as NAME=DIR. Served directly by the host;
        /// requests for anything else go to the app NAME, if there is one.
        #[arg(long = "static", value_parser = parse_pair)]
        statics: Vec<(String, String)>,
        /// Serves a site on a domain, as HOST=NAME (e.g. shop.localhost=shop).
        /// The site then gets every path on that domain.
        #[arg(long = "domain", value_parser = parse_pair)]
        domains: Vec<(String, String)>,
        /// Database as NAME=URL: `sqlite:PATH` (a file, created if missing) or
        /// `postgres://user:pass@host/db`. Apps refer to it by NAME and need a
        /// --grant to use it.
        #[arg(long = "db", value_parser = parse_pair)]
        databases: Vec<(String, String)>,
        /// SQL file to run at startup for a database, as NAME=FILE (use
        /// idempotent statements, e.g. CREATE TABLE IF NOT EXISTS).
        #[arg(long = "db-init", value_parser = parse_pair)]
        db_inits: Vec<(String, String)>,
        /// Lets an app use a database, as APP=DB.
        #[arg(long = "grant", value_parser = parse_pair)]
        grants: Vec<(String, String)>,
        /// Connections per database in each worker's pool.
        #[arg(long, default_value_t = 4)]
        db_pool_size: usize,
        /// Extra CA certificates (PEM) to trust for database TLS, on top of
        /// the public roots. TLS use and certificate checks follow each URL's
        /// sslmode, as in libpq: disable, prefer (default), require,
        /// verify-ca, verify-full.
        #[arg(long)]
        db_ca_file: Option<PathBuf>,
        /// Limit for each database call, including waiting for a connection.
        /// The query is cancelled when it expires.
        #[arg(long, default_value_t = 5000)]
        db_timeout_ms: u64,
        /// Per-app override of --db-timeout-ms, as APP=MS.
        #[arg(long = "app-db-timeout-ms", value_parser = parse_pair)]
        app_db_timeouts: Vec<(String, String)>,
        /// Queue consumer, as QUEUE=APP: messages sent to QUEUE are delivered
        /// to APP's `platform:sdk/events` handler.
        #[arg(long = "queue", value_parser = parse_pair)]
        queues: Vec<(String, String)>,
        /// Cron job, as APP:NAME=EXPR (e.g. 'reports:daily=0 3 * * *', or
        /// with seconds: '*/10 * * * * *'). Runs APP's events handler.
        #[arg(long = "cron", value_parser = parse_env)]
        crons: Vec<(String, String, String)>,
        /// Chooses which app provides an interface another app imports, as
        /// APP:INTERFACE=TARGET. Needed only when several apps export it.
        #[arg(long = "link", value_parser = parse_env)]
        links: Vec<(String, String, String)>,
        /// Accept WebSocket upgrades for an app at a path of its site, as
        /// APP=PATH (e.g. chat=/ws). The app exports platform:sdk/websocket-handler.
        #[arg(long = "websocket", value_parser = parse_pair)]
        websockets: Vec<(String, String)>,
        /// Credits granted to an app or site, as NAME=CREDITS. When they're
        /// used up, it gets 402 (HTTP), refused WebSockets and postponed
        /// events. Apps without a grant are only metered.
        #[arg(long = "credits", value_parser = parse_pair)]
        credits: Vec<(String, String)>,
        /// Prices in credits per unit, as a JSON object overriding the
        /// defaults, e.g. {"cpu": 0.1}. See `wasm-server pricing`.
        #[arg(long)]
        pricing: Option<PathBuf>,
        /// Snapshot SQLite databases into this directory (gzipped).
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// Minutes between snapshots.
        #[arg(long, default_value_t = 60)]
        backup_every_min: u64,
        /// Snapshots kept per database.
        #[arg(long, default_value_t = 24)]
        backup_keep: usize,
        /// Where the platform keeps its state (queues, billing, versions).
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
        /// Threads delivering queue messages and cron ticks.
        #[arg(long, default_value_t = 2)]
        job_workers: usize,
        /// Cache size per app, in MiB.
        #[arg(long, default_value_t = 64)]
        cache_mb: u64,
        /// Environment variable for an app, as APP:KEY=VALUE.
        #[arg(long = "env", value_parser = parse_env)]
        envs: Vec<(String, String, String)>,
        /// Primary of a cluster: where workers join (a private network
        /// address), e.g. 10.0.0.2:7070. Needs --cluster-token.
        #[arg(long)]
        cluster_listen: Option<SocketAddr>,
        /// The cluster's shared secret (at least 16 characters), on the
        /// primary and every worker.
        #[arg(long, env = "WASM_SERVER_CLUSTER_TOKEN")]
        cluster_token: Option<String>,
        /// Run as a worker of the primary at this cluster address
        /// (http://10.0.0.2:7070): its apps, databases and queues are the
        /// primary's, which sends it requests.
        #[arg(long)]
        join: Option<String>,
        /// A worker's address for the primary's requests; default
        /// http://<--listen>.
        #[arg(long)]
        advertise: Option<String>,
        /// Add and remove workers by load (on a primary): `local` (worker
        /// processes on this machine) or `digitalocean` (droplets;
        /// DIGITALOCEAN_TOKEN). See autoscale.rs.
        #[arg(long)]
        autoscale: Option<String>,
        #[arg(long, default_value_t = 0)]
        autoscale_min: usize,
        #[arg(long, default_value_t = 2)]
        autoscale_max: usize,
        /// Add a worker at this CPU use or more (0..1), held for 2 minutes.
        #[arg(long, default_value_t = 0.7)]
        autoscale_high: f64,
        /// Remove one under this, held for 10 minutes.
        #[arg(long, default_value_t = 0.3)]
        autoscale_low: f64,
        /// The cluster address workers join; default http://<--cluster-listen>.
        #[arg(long)]
        autoscale_join: Option<String>,
        /// digitalocean: droplet size and region.
        #[arg(long, default_value = "s-2vcpu-4gb")]
        autoscale_size: String,
        #[arg(long, default_value = "fra1")]
        autoscale_region: String,
        /// Decide in seconds instead of minutes (for tests).
        #[arg(long, hide = true)]
        autoscale_fast: bool,
        /// HTTPS on --listen, with certificates from Let's Encrypt for every
        /// domain in the deployment; port 80 (--http-listen) answers the
        /// CA's challenges and redirects to https. Typically:
        /// `--https --listen 0.0.0.0:443 --acme-email you@example.com`.
        #[arg(long)]
        https: bool,
        /// Port 80 for --https.
        #[arg(long, default_value = "0.0.0.0:80")]
        http_listen: SocketAddr,
        /// Contact address for Let's Encrypt (expiry warnings).
        #[arg(long)]
        acme_email: Option<String>,
        /// ACME server: `production` (Let's Encrypt, the default), `staging`
        /// (Let's Encrypt's test CA, for trying a setup), or a directory URL.
        #[arg(long)]
        acme_directory: Option<String>,
        /// A root certificate (PEM) for the ACME server's TLS, for a test CA
        /// such as Pebble.
        #[arg(long)]
        acme_ca_file: Option<PathBuf>,
        /// A host an app may send HTTP requests to, as APP=HOST (`*.example.com`
        /// for subdomains; repeat for more; APP=none for no requests at all).
        /// Without it the app may reach any public host. Private, loopback and
        /// cloud metadata addresses are always refused.
        #[arg(long = "outbound", value_parser = parse_pair)]
        outbound: Vec<(String, String)>,
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
        /// Worker threads; defaults to the number of CPUs.
        #[arg(short, long)]
        workers: Option<usize>,
        /// Per-request wall-clock limit in milliseconds, including I/O waits.
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
        /// Per-request CPU limit in milliseconds, checked every 10 ms. Time spent
        /// waiting on the database or for the core does not count.
        #[arg(long, default_value_t = 1000)]
        cpu_limit_ms: u64,
        /// Maximum linear memory per instance, in MiB.
        #[arg(long, default_value_t = 64)]
        max_memory_mb: usize,
        #[command(flatten)]
        engine: EngineArgs,
    },
    /// Print the default price list (credits per unit).
    Pricing,
    /// Upload a new version of an app to a running server (no downtime).
    Deploy {
        /// The server, e.g. http://127.0.0.1:8080
        url: String,
        /// App name, as in the server's deployment.
        #[arg(long)]
        app: String,
        /// `.wasm` component (compiled by the server) or `.cwasm`.
        file: PathBuf,
        #[arg(long, env = "WASM_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// List an app's saved versions on a running server.
    Versions {
        url: String,
        #[arg(long)]
        app: String,
        #[arg(long, env = "WASM_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Switch an app back to its previous version (or --to a given one).
    Rollback {
        url: String,
        #[arg(long)]
        app: String,
        /// Version id from `versions`.
        #[arg(long)]
        to: Option<String>,
        #[arg(long, env = "WASM_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Print the SHA-256 of a tenant token, for `token_sha256` in the deployment.
    HashToken { token: String },
    /// Restore a SQLite database from a snapshot (with the server stopped).
    Restore {
        /// Snapshot (`.db.gz`) or SQLite file.
        backup: PathBuf,
        /// The database file to replace (kept as .before-restore).
        #[arg(long)]
        to: PathBuf,
    },
    /// Make a running server re-read its deployment (like SIGHUP).
    Reload {
        url: String,
        #[arg(long, env = "WASM_SERVER_ADMIN_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Measure the runtime cost per request in-process (no HTTP or network).
    Bench {
        /// `.cwasm` or `.wasm` component.
        app: PathBuf,
        #[arg(long, value_enum, default_value = "handle")]
        mode: bench::Mode,
        #[arg(short, long, default_value_t = 1)]
        threads: usize,
        #[arg(short, long, default_value_t = 5)]
        seconds: u64,
        /// Request path passed to the guest.
        #[arg(long, default_value = "/")]
        path: String,
        #[command(flatten)]
        engine: EngineArgs,
    },
}

#[derive(clap::Args)]
struct EngineArgs {
    /// Maximum number of concurrently running instances.
    #[arg(long, default_value_t = 1000)]
    pool_size: u32,
    #[arg(long, value_enum, default_value = "pooling")]
    allocator: engine::Allocator,
    /// Linux: KiB of each memory kept resident between requests (0 = off).
    #[arg(long, default_value_t = 2048)]
    keep_resident_kib: usize,
}

impl EngineArgs {
    fn opts(&self, max_memory: usize) -> engine::EngineOpts {
        engine::EngineOpts {
            allocator: self.allocator,
            pool_size: self.pool_size,
            max_memory,
            keep_resident: self.keep_resident_kib << 10,
        }
    }
}

fn parse_pair(s: &str) -> Result<(String, String), String> {
    let Some((name, value)) = s.split_once('=') else {
        return Err("expected NAME=VALUE".into());
    };
    Ok((name.to_string(), value.to_string()))
}

fn parse_env(s: &str) -> Result<(String, String, String), String> {
    let (app, rest) = s.split_once(':').ok_or("expected APP:KEY=VALUE")?;
    let (key, value) = rest.split_once('=').ok_or("expected APP:KEY=VALUE")?;
    Ok((app.to_string(), key.to_string(), value.to_string()))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Compile { input, output } => {
            let engine = engine::build(&engine::EngineOpts::default())?;
            let bytes = std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
            let compiled = engine.precompile_component(&bytes)?;
            let output = output.unwrap_or_else(|| input.with_extension("cwasm"));
            std::fs::write(&output, compiled)?;
            eprintln!("wrote {}", output.display());
        }
        Command::Serve {
            config,
            admin_token,
            apps,
            statics,
            domains,
            databases,
            db_inits,
            grants,
            db_pool_size,
            db_ca_file,
            db_timeout_ms,
            app_db_timeouts,
            queues,
            crons,
            links,
            websockets,
            credits,
            pricing,
            backup_dir,
            backup_every_min,
            backup_keep,
            data_dir,
            job_workers,
            cache_mb,
            envs,
            outbound,
            cluster_listen,
            cluster_token,
            join,
            advertise,
            autoscale,
            autoscale_min,
            autoscale_max,
            autoscale_high,
            autoscale_low,
            autoscale_join,
            autoscale_size,
            autoscale_region,
            autoscale_fast,
            https,
            http_listen,
            acme_email,
            acme_directory,
            acme_ca_file,
            listen,
            workers,
            timeout_ms,
            cpu_limit_ms,
            max_memory_mb,
            engine: engine_args,
        } => {
            if join.is_some() || cluster_listen.is_some() {
                if cluster_token.as_deref().is_none_or(|t| t.len() < 16) {
                    wasmtime::bail!("a cluster needs --cluster-token (or WASM_SERVER_CLUSTER_TOKEN), at least 16 characters");
                }
            }
            if let Some(primary) = &join {
                let id = format!("{}-{}", std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "worker".into()), std::process::id());
                cluster::set_worker(cluster::Worker::new(primary.trim_end_matches('/').to_string(), cluster_token.clone().unwrap(), id, advertise.clone().unwrap_or_else(|| format!("http://{listen}"))));
            }
            let source = match config {
                _ if join.is_some() => deployment::Source::Cluster,
                Some(path) => deployment::Source::File(path),
                None => deployment::Source::Flags(flags::deployment(flags::Flags {
                    apps,
                    statics,
                    domains,
                    databases,
                    db_inits,
                    grants,
                    app_db_timeouts,
                    queues,
                    crons,
                    links,
                    websockets,
                    credits,
                    envs,
                    outbound,
                })?),
            };
            db::MANAGED_DIR.set(data_dir.join("apps")).ok();
            logs::DIR.set(data_dir.join("logs")).ok();
            let deployment = source.load(&data_dir)?;
            if deployment.apps.is_empty() && deployment.sites.is_empty() {
                eprintln!("note: no apps yet; deploy one (plumb deploy, or PUT /_platform/apps/NAME/deploy)");
            }
            eprintln!("deployment: {}", source.describe());

            // Databases are fixed for the server's lifetime: every worker
            // holds connections to them.
            let databases = deployment
                .databases
                .iter()
                .map(|(name, spec)| {
                    let init = match &spec.init {
                        Some(file) => Some(std::fs::read_to_string(file).with_context(|| format!("database {name:?} init {}", file.display()))?),
                        None => None,
                    };
                    Ok(db::DbConfig { name: name.clone(), url: spec.url.clone(), init })
                })
                .collect::<Result<Vec<_>>>()?;
            let db_tls = db::tls(db_ca_file.as_deref())?;
            if !databases.is_empty() && join.is_none() {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let pools = db::Pools::new(&databases, 1, &db_tls)?;
                let (reachable, warnings) = rt.block_on(pools.check(&databases))?;
                eprintln!("databases: {}", reachable.join(", "));
                for warning in warnings {
                    eprintln!("warning: {warning}");
                }
            }

            let max_memory = max_memory_mb << 20;
            let engine = engine::build(&engine_args.opts(max_memory))?;
            let accounts = loader::Accounts::default();
            let mut components = loader::ComponentCache::default();
            let defaults = loader::Defaults { db_timeout: Duration::from_millis(db_timeout_ms), cache_bytes: cache_mb << 20 };
            let (current, _) = loader::build(&engine, deployment, &mut components, &accounts, &defaults, 1)?;

            let workers = workers.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
            let job_workers = job_workers.max(1);
            let billing = billing::Billing::new(
                &data_dir.join("billing.db"),
                billing::Pricing::load(pricing.as_deref())?,
                &accounts.names(),
                &current.credits,
            )?;
            let jobs = jobs::JobStore::open(&data_dir.join("platform.db"))?;
            match &admin_token {
                Some(token) if token.len() < 16 => wasmtime::bail!("--admin-token must be at least 16 characters"),
                Some(_) => {}
                None if current.deployment.tenants.is_empty() => {
                    eprintln!("note: no --admin-token: /_platform/ is read-only and open to anyone who can reach the server")
                }
                None => eprintln!("note: no --admin-token: tenants can manage their apps; reloads and backups are disabled"),
            }
            let platform = Arc::new(platform::Platform::new(platform::Parts {
                current,
                accounts,
                // Job workers get stats slots after the HTTP workers.
                stats: stats::Stats::new(workers + job_workers, loader::MAX_ACCOUNTS),
                timeout: Duration::from_millis(timeout_ms),
                limits: app::RequestLimits { max_memory, cpu_limit: Duration::from_millis(cpu_limit_ms) },
                databases,
                db_pool_size,
                db_tls,
                jobs: if join.is_some() { None } else { Some(jobs) },
                billing,
                admin_token,
                data_dir: data_dir.clone(),
                backups: backup_dir.map(|dir| backup::BackupConfig {
                    dir,
                    every: Duration::from_secs(backup_every_min.max(1) * 60),
                    keep: backup_keep.max(1),
                }),
                reload: platform::Reloader::new(engine, source, defaults, components),
            }));
            if let Some(addr) = cluster_listen {
                let primary = cluster::start_primary(platform.clone(), addr, cluster_token.clone().unwrap(), workers)?;
                if let Some(provider) = autoscale {
                    autoscale::start(
                        primary,
                        autoscale::AutoscaleOpts {
                            provider,
                            min: autoscale_min,
                            max: autoscale_max,
                            high: autoscale_high,
                            low: autoscale_low,
                            join: autoscale_join.unwrap_or_else(|| format!("http://{addr}")),
                            token: cluster_token.clone().unwrap(),
                            data_dir: data_dir.clone(),
                            size: autoscale_size,
                            region: autoscale_region,
                            fast: autoscale_fast,
                        },
                    )?;
                }
            } else if autoscale.is_some() {
                wasmtime::bail!("--autoscale is for a cluster primary: add --cluster-listen");
            }
            if join.is_some() {
                cluster::spawn_heartbeat(platform.clone(), workers)?;
            }
            let tls = if https && join.is_none() {
                let opts = tls::TlsOpts {
                    directory: tls::directory_url(acme_directory.as_deref()),
                    email: acme_email,
                    ca_file: acme_ca_file,
                    http_listen,
                    https_port: listen.port(),
                };
                Some(tls::start(platform.clone(), opts)?)
            } else {
                None
            };
            if join.is_none() {
                let metrics = Arc::new(metrics::Metrics::open(&data_dir.join("metrics.db"))?);
                metrics::METRICS.set(metrics.clone()).ok();
                metrics::spawn(platform.clone(), metrics)?;
            }
            server::serve(platform, server::ServeOpts { listen, workers, job_workers, tls, worker: join.is_some() })?;
        }
        Command::Deploy { url, app, file, token } => client::deploy(&url, &app, &file, token.as_deref())?,
        Command::Reload { url, token } => client::reload(&url, token.as_deref())?,
        Command::Versions { url, app, token } => client::versions(&url, &app, token.as_deref())?,
        Command::Rollback { url, app, to, token } => client::rollback(&url, &app, to.as_deref(), token.as_deref())?,
        Command::HashToken { token } => {
            use sha2::Digest;
            println!("{}", versions::hex(&sha2::Sha256::digest(token.as_bytes())));
        }
        Command::Restore { backup: file, to } => {
            backup::restore(&file, &to)?;
            eprintln!("restored {} from {}", to.display(), file.display());
        }
        Command::Pricing => {
            println!("{:<22} {:<11} {:>12}  {:<18} {}", "meter", "category", "credits", "per", "what");
            for m in &billing::METERS {
                println!("{:<22} {:<11} {:>12}  {:<18} {}", m.key, m.category, m.price, m.unit, m.description);
            }
            println!("\n1 credit ≈ $0.001 at these prices. Override any with --pricing FILE (JSON).");
        }
        Command::Bench { app, mode, threads, seconds, path, engine: engine_args } => {
            let opts = engine_args.opts(engine::EngineOpts::default().max_memory);
            let engine = engine::build(&opts)?;
            let linker = app::linker(&engine)?;
            let config = app::AppConfig {
                databases: Arc::from([]),
                db_timeout: Duration::ZERO,
                env: Arc::from([]),
                cache: Arc::new(cache::AppCache::new(64 << 20)),
                links: Arc::default(),
                websocket_path: None,
                outbound: Arc::default(),
                db_aliases: Arc::default(),
            };
            let component = app::load_component(&engine, "bench", &app)?;
            let app = app::App::new(&linker, 0, "bench", &component, config)?;
            bench::run(engine.clone(), app, mode, threads, Duration::from_secs(seconds), &path, opts.max_memory)?;
        }
    }
    Ok(())
}
