//! Builds what runs (`Current`) from a `Deployment`: loads components,
//! links functions, creates apps and sites, routes, queues and cron. Runs at
//! startup and on every reload; the result replaces the previous one
//! atomically.

use crate::app::{self, App, AppConfig, RequestState};
use crate::cache::AppCache;
use crate::deployment::Deployment;
use crate::functions;
use crate::jobs::CronJob;
use crate::routes::Routes;
use crate::static_files::StaticSite;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Result, bail, format_err};

/// Accounts (apps and sites) can't exceed this: usage counters are
/// allocated up front so the hot path never resizes them.
pub const MAX_ACCOUNTS: usize = 1024;

/// What runs right now. Replaced as a whole on reload; requests that
/// started on the previous one finish on it.
pub struct Current {
    pub routes: Routes,
    /// Queue name → consumer app.
    pub queues: HashMap<String, String>,
    pub crons: Vec<CronJob>,
    /// Credits granted per account.
    pub credits: HashMap<String, f64>,
    pub deployment: Deployment,
    /// When this version went live.
    pub version: u64,
}

/// Stable ids and caches per account name, across reloads.
#[derive(Default)]
pub struct Accounts {
    inner: Mutex<AccountsInner>,
}

#[derive(Default)]
struct AccountsInner {
    names: Vec<String>,
    ids: HashMap<String, usize>,
    caches: HashMap<String, Arc<AppCache>>,
}

impl Accounts {
    pub fn id(&self, name: &str) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = inner.ids.get(name) {
            return Ok(*id);
        }
        if inner.names.len() >= MAX_ACCOUNTS {
            bail!("too many apps and sites (limit {MAX_ACCOUNTS})");
        }
        let id = inner.names.len();
        inner.names.push(name.to_string());
        inner.ids.insert(name.to_string(), id);
        Ok(id)
    }

    /// The app's cache survives redeploys.
    fn cache(&self, name: &str, max_bytes: u64) -> Arc<AppCache> {
        self.inner.lock().unwrap().caches.entry(name.to_string()).or_insert_with(|| Arc::new(AppCache::new(max_bytes))).clone()
    }

    /// Names indexed by id, including accounts no longer deployed (their
    /// usage still counts).
    pub fn names(&self) -> Vec<String> {
        self.inner.lock().unwrap().names.clone()
    }
}

/// Loaded components by file, so a reload only reloads what changed.
#[derive(Default)]
pub struct ComponentCache {
    entries: HashMap<PathBuf, (SystemTime, u64, Component)>,
}

impl ComponentCache {
    fn load(&mut self, engine: &Engine, name: &str, path: &Path) -> Result<(Component, bool)> {
        let meta = std::fs::metadata(path).map_err(|e| format_err!("app {name:?}: {}: {e}", path.display()))?;
        let key = (meta.modified()?, meta.len());
        if let Some((modified, len, component)) = self.entries.get(path) {
            if (*modified, *len) == key {
                return Ok((component.clone(), false));
            }
        }
        let component = app::load_component(engine, name, path)?;
        self.entries.insert(path.to_owned(), (key.0, key.1, component.clone()));
        Ok((component, true))
    }
}

/// Settings that apply to every app.
pub struct Defaults {
    pub db_timeout: Duration,
    pub cache_bytes: u64,
}

/// What a build changed, for the reload response.
pub struct Summary {
    pub apps: usize,
    pub sites: usize,
    /// Apps whose component was (re)loaded.
    pub loaded: Vec<String>,
}

pub fn build(
    engine: &Engine,
    deployment: Deployment,
    components: &mut ComponentCache,
    accounts: &Accounts,
    defaults: &Defaults,
    version: u64,
) -> Result<(Current, Summary)> {
    let mut loaded = Vec::new();
    let mut named = Vec::new();
    for (name, spec) in &deployment.apps {
        let (component, fresh) = components.load(engine, name, &spec.component)?;
        if fresh {
            loaded.push(name.clone());
        }
        named.push((name.clone(), component));
    }
    let refs: Vec<(String, &Component)> = named.iter().map(|(n, c)| (n.clone(), c)).collect();
    let explicit: Vec<(String, String, String)> = deployment
        .apps
        .iter()
        .flat_map(|(app, spec)| spec.links.iter().map(move |(iface, target)| (app.clone(), iface.clone(), target.clone())))
        .collect();
    let plan = functions::plan(engine, &refs, &explicit)?;
    let mut linker: Linker<RequestState> = app::linker(engine)?;
    functions::define_stubs(&mut linker, &plan)?;

    let mut apps = Vec::new();
    let mut statics = Vec::new();
    let mut domains = Vec::new();
    let mut queues = HashMap::new();
    let mut crons = Vec::new();
    let mut credits = HashMap::new();
    for (i, ((name, spec), (_, component))) in deployment.apps.iter().zip(&named).enumerate() {
        let config = AppConfig {
            databases: spec.databases.clone().into(),
            db_timeout: spec.db_timeout_ms.map_or(defaults.db_timeout, Duration::from_millis),
            env: spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>().into(),
            cache: accounts.cache(name, defaults.cache_bytes),
            links: Arc::new(plan.links[i].clone()),
            websocket_path: spec.websocket.clone(),
            outbound: Arc::new(crate::outbound::Policy::new(spec.outbound.as_deref())),
        };
        let app = App::new(&linker, accounts.id(name)?, name, component, config)?;
        if (!spec.queues.is_empty() || !spec.cron.is_empty()) && app.events.is_none() {
            bail!("app {name:?} has queues or cron but doesn't export platform:sdk/events");
        }
        for queue in &spec.queues {
            queues.insert(queue.clone(), name.clone());
        }
        for (job, expr) in &spec.cron {
            crons.push(CronJob::parse(name, job, expr)?);
        }
        if let Some(dir) = &spec.static_dir {
            statics.push((name.clone(), load_site(name, dir)?));
        }
        domains.extend(spec.domains.iter().map(|d| (d.clone(), name.clone())));
        if let Some(n) = spec.credits {
            credits.insert(name.clone(), n);
        }
        apps.push(app);
    }
    for (name, spec) in &deployment.sites {
        statics.push((name.clone(), load_site(name, &spec.static_dir)?));
        domains.extend(spec.domains.iter().map(|d| (d.clone(), name.clone())));
        if let Some(n) = spec.credits {
            credits.insert(name.clone(), n);
        }
    }
    let routes = Routes::new(apps, statics, &domains, accounts)?;
    let summary = Summary { apps: deployment.apps.len(), sites: deployment.sites.len(), loaded };
    Ok((Current { routes, queues, crons, credits, deployment, version }, summary))
}

fn load_site(name: &str, dir: &Path) -> Result<StaticSite> {
    let site = StaticSite::load(dir).map_err(|e| format_err!("site {name:?}: {e}"))?;
    eprintln!("loaded site {name:?}: {} files, {} KiB from {}", site.len(), site.total_bytes >> 10, dir.display());
    Ok(site)
}
