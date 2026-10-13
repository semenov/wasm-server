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
//!
//! # A tenant manages its own apps with its own token (stored hashed:
//! # `wasm-server hash-token TOKEN`), and sees only their usage.
//! [tenants.acme]
//! token_sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
//! # then: tenant = "acme" in [apps.*] or [sites.*]
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
    #[serde(default)]
    pub tenants: BTreeMap<String, TenantSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TenantSpec {
    /// SHA-256 of the tenant's token, hex.
    pub token_sha256: String,
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
    pub credits: Option<f64>,
    pub db_timeout_ms: Option<u64>,
    /// Hosts the app may send HTTP requests to (`api.example.com`,
    /// `*.example.com`); not set: any public host; empty: none.
    pub outbound: Option<Vec<String>>,
    /// The tenant that owns the app (deploys it, sees its usage).
    pub tenant: Option<String>,
    /// For apps deployed over the API: the app's own database names →
    /// the server's (`main` → `notes/main`).
    #[serde(skip)]
    pub db_aliases: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SiteSpec {
    #[serde(rename = "static")]
    pub static_dir: PathBuf,
    #[serde(default)]
    pub domains: Vec<String>,
    pub credits: Option<f64>,
    pub tenant: Option<String>,
}

/// Where the deployment comes from; read again on every reload.
pub enum Source {
    /// Fixed at startup; a reload re-reads the component files it names.
    Flags(Deployment),
    File(PathBuf),
}

impl Source {
    /// The deployment, with the apps deployed over the API (`managed.rs`).
    pub fn load(&self, data_dir: &Path) -> Result<Deployment> {
        let mut deployment = match self {
            Source::Flags(d) => d.clone(),
            Source::File(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| format_err!("{}: {e}", path.display()))?;
                let mut d: Deployment = toml::from_str(&text).map_err(|e| format_err!("{}: {e}", path.display()))?;
                d.resolve_paths(path.parent().unwrap_or(Path::new(".")));
                d
            }
        };
        crate::managed::add_to(&mut deployment, data_dir)?;
        deployment.validate()?;
        Ok(deployment)
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Flags(_) => "command-line flags".into(),
            Source::File(path) => path.display().to_string(),
        }
    }
}

impl Deployment {
    /// The tenant owning an app or site, if any.
    pub fn tenant_of(&self, name: &str) -> Option<&str> {
        self.apps.get(name).and_then(|a| a.tenant.as_deref()).or_else(|| self.sites.get(name).and_then(|s| s.tenant.as_deref()))
    }

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
        for (name, tenant) in self
            .apps
            .iter()
            .map(|(n, a)| (n, &a.tenant))
            .chain(self.sites.iter().map(|(n, s)| (n, &s.tenant)))
        {
            if let Some(t) = tenant {
                if !self.tenants.contains_key(t) {
                    bail!("{name:?} belongs to tenant {t:?}, which isn't defined");
                }
            }
        }
        for (name, t) in &self.tenants {
            if t.token_sha256.len() != 64 || !t.token_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("tenant {name:?}: token_sha256 must be 64 hex characters (wasm-server hash-token TOKEN)");
            }
        }
        let all_domains = self.apps.iter().map(|(n, a)| (n, &a.domains)).chain(self.sites.iter().map(|(n, s)| (n, &s.domains)));
        for (name, list) in all_domains {
            for domain in list {
                if let Some(other) = domains.insert(domain.to_ascii_lowercase(), name.clone()) {
                    bail!("domain {domain:?} is assigned to both {other:?} and {name:?}");
                }
            }
        }
        Ok(())
    }
}
