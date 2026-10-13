//! What runs: apps, static sites, databases, queues, cron. Comes from a
//! TOML file (`serve --config`) or from command-line flags, and is read
//! again on every reload.
//!
//! ```toml
//! [databases.links]
//! url = "sqlite:data/links.db"
//! init = "examples/shortener/links.sql"
//!
//! [apps.shortener]
//! component = "apps/shortener.cwasm"
//! databases = ["links"]
//! static = "sites/shortener"
//! domains = ["short.localhost"]
//!
//! [apps.stats]
//! component = "apps/stats.cwasm"
//! databases = ["stats"]
//! queues = ["clicks"]
//! cron = { rollup = "*/10 * * * * *" }
//!
//! [sites.docs]
//! static = "sites/docs"
//! domains = ["docs.localhost"]
//! ```
//!
//! Relative paths in a file are relative to the file.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use wasmtime::{Result, bail, format_err};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    #[serde(default)]
    pub databases: BTreeMap<String, DatabaseSpec>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppSpec>,
    /// Static-only sites.
    #[serde(default)]
    pub sites: BTreeMap<String, SiteSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSpec {
    /// `sqlite:PATH` or a Postgres URL.
    pub url: String,
    /// SQL run at startup (idempotent statements).
    pub init: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    /// `.cwasm` (precompiled) or `.wasm`.
    pub component: PathBuf,
    /// Databases the app may use.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Static files served before the app.
    #[serde(rename = "static")]
    pub static_dir: Option<PathBuf>,
    #[serde(default)]
    pub domains: Vec<String>,
    /// The part of its domains it serves (`/api`: `/api` and below); not
    /// set: all of it. Several apps share a domain this way, the longest
    /// matching path winning; each sees the whole path.
    pub path: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Path where WebSocket upgrades go to this app.
    pub websocket: Option<String>,
    /// Queues this app consumes.
    #[serde(default)]
    pub queues: Vec<String>,
    /// Cron jobs: name → schedule.
    #[serde(default)]
    pub cron: BTreeMap<String, String>,
    /// Which app provides an imported interface: interface → app.
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    pub db_timeout_ms: Option<u64>,
    /// Hosts the app may send HTTP requests to (`api.example.com`,
    /// `*.example.com`); not set: any public host; empty: none.
    pub outbound: Option<Vec<String>>,
    /// For apps deployed over the API: the app's own database names →
    /// the server's (`main` → `notes/main`).
    #[serde(skip)]
    pub db_aliases: BTreeMap<String, String>,
    /// For apps deployed over the API: their project (its files).
    #[serde(skip)]
    pub project: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SiteSpec {
    #[serde(rename = "static")]
    pub static_dir: PathBuf,
    #[serde(default)]
    pub domains: Vec<String>,
    /// The part of its domains it serves, as for apps.
    pub path: Option<String>,
    /// A single-page app: paths that aren't files get `index.html`.
    #[serde(default)]
    pub spa: bool,
}

/// Where the deployment comes from; read again on every reload.
pub enum Source {
    /// Fixed at startup; a reload re-reads the component files it names.
    Flags(Deployment),
    File(PathBuf),
    /// A cluster worker: the primary's apps (`cluster.rs`).
    Cluster,
}

impl Source {
    /// The deployment, with the apps deployed over the API (`managed.rs`).
    pub fn load(&self, data_dir: &Path) -> Result<Deployment> {
        if let Source::Cluster = self {
            return crate::cluster::load_deployment(data_dir);
        }
        let mut deployment = match self {
            Source::Flags(d) => d.clone(),
            Source::File(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| format_err!("{}: {e}", path.display()))?;
                let mut d: Deployment = toml::from_str(&text).map_err(|e| format_err!("{}: {e}", path.display()))?;
                d.resolve_paths(path.parent().unwrap_or(Path::new(".")));
                d
            }
            Source::Cluster => unreachable!(),
        };
        crate::managed::add_to(&mut deployment, data_dir)?;
        deployment.validate()?;
        Ok(deployment)
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Flags(_) => "command-line flags".into(),
            Source::File(path) => path.display().to_string(),
            Source::Cluster => "the cluster's primary".into(),
        }
    }
}

impl Deployment {
    fn resolve_paths(&mut self, base: &Path) {
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        for db in self.databases.values_mut() {
            if let Some(init) = &mut db.init {
                fix(init);
            }
            if let Some(path) = db.url.strip_prefix("sqlite:") {
                let path = PathBuf::from(path);
                if path.is_relative() {
                    db.url = format!("sqlite:{}", base.join(path).display());
                }
            }
        }
        for app in self.apps.values_mut() {
            fix(&mut app.component);
            if let Some(dir) = &mut app.static_dir {
                fix(dir);
            }
        }
        for site in self.sites.values_mut() {
            fix(&mut site.static_dir);
        }
    }

    /// Checks references between parts (without loading anything).
    pub fn validate(&self) -> Result<()> {
        for name in self.sites.keys() {
            if self.apps.contains_key(name) {
                bail!("{name:?} is both an app and a site; give the app a `static` directory instead");
            }
        }
        let mut domains = BTreeMap::new();
        let mut queues = BTreeMap::new();
        for (name, app) in &self.apps {
            for db in &app.databases {
                // `app/name`: a deployed app's own SQLite database
                if !self.databases.contains_key(db) && !db.contains('/') {
                    bail!("app {name:?} uses database {db:?}, which isn't defined");
                }
            }
            for target in app.links.values() {
                if !self.apps.contains_key(target) {
                    bail!("app {name:?} links to {target:?}, which isn't an app");
                }
            }
            for queue in &app.queues {
                if let Some(other) = queues.insert(queue.clone(), name.clone()) {
                    bail!("queue {queue:?} is consumed by both {other:?} and {name:?}");
                }
            }
        }
        for (name, path) in self.apps.iter().map(|(n, a)| (n, &a.path)).chain(self.sites.iter().map(|(n, s)| (n, &s.path))) {
            if let Some(path) = path {
                if !valid_path(path) {
                    bail!("{name:?}: path {path:?} must start with / and not end with one (\"/api\")");
                }
            }
        }
        let all_domains = self
            .apps
            .iter()
            .map(|(n, a)| (n, &a.domains, a.path.as_deref().unwrap_or("/")))
            .chain(self.sites.iter().map(|(n, s)| (n, &s.domains, s.path.as_deref().unwrap_or("/"))));
        for (name, list, path) in all_domains {
            for domain in list {
                if let Some(other) = domains.insert((domain.to_ascii_lowercase(), path.to_string()), name.clone()) {
                    let place = if path == "/" { domain.clone() } else { format!("{domain}{path}") };
                    bail!("{place:?} is assigned to both {other:?} and {name:?}");
                }
            }
        }
        Ok(())
    }
}

/// An app's `path`: `/`, or `/a/b` without a trailing `/`.
pub fn valid_path(path: &str) -> bool {
    path == "/" || (path.starts_with('/') && !path.ends_with('/') && !path.contains("//") && !path.contains(['?', '#', ' ']))
}
