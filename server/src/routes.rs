//! Which site handles a request: by domain first, then by path prefix.

use crate::app::App;
use crate::loader::Accounts;
use crate::static_files::StaticSite;
use std::collections::HashMap;
use std::sync::Arc;
use wasmtime::{Result, bail};

/// A deployed site: a wasm app, static files, or both. With both, existing
/// files are served directly and everything else goes to the app.
pub struct Site {
    /// Account for usage and billing; for an app site, the app's id.
    pub id: usize,
    pub name: String,
    pub app: Option<Arc<App>>,
    pub files: Option<StaticSite>,
}

/// How a request reached its site.
pub enum Matched<'a> {
    /// Through one of the site's domains: the whole path belongs to the site.
    Domain(&'a Site),
    /// Through the `/NAME/...` prefix (or the fallback site).
    Prefix(&'a Site),
}

/// Immutable routing table, shared by all cores without locks.
pub struct Routes {
    sites: HashMap<String, Arc<Site>>,
    domains: HashMap<String, Arc<Site>>,
    /// Receives requests that match nothing else: the site named `default`,
    /// or the only site when exactly one is deployed.
    fallback: Option<Arc<Site>>,
}

impl Routes {
    pub fn new(apps: Vec<App>, statics: Vec<(String, StaticSite)>, domains: &[(String, String)], accounts: &Accounts) -> Result<Self> {
        let mut sites: HashMap<String, Site> = apps
            .into_iter()
            .map(|a| (a.name.clone(), Site { id: a.id, name: a.name.clone(), app: Some(Arc::new(a)), files: None }))
            .collect();
        for (name, files) in statics {
            if !sites.contains_key(&name) {
                let id = accounts.id(&name)?;
                sites.insert(name.clone(), Site { id, name: name.clone(), app: None, files: None });
            }
            sites.get_mut(&name).unwrap().files = Some(files);
        }
        let sites: HashMap<String, Arc<Site>> = sites.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();

        let mut by_domain = HashMap::new();
        for (domain, name) in domains {
            let Some(site) = sites.get(name) else {
                bail!("--domain {domain}={name}: no app or static site named {name:?}");
            };
            if by_domain.insert(domain.to_ascii_lowercase(), site.clone()).is_some() {
                bail!("--domain {domain}: assigned twice");
            }
        }
        let fallback = sites
            .get("default")
            .cloned()
            .or_else(|| (sites.len() == 1).then(|| sites.values().next().unwrap().clone()));
        Ok(Self { sites, domains: by_domain, fallback })
    }

    pub fn app(&self, name: &str) -> Option<&Arc<App>> {
        self.sites.get(name)?.app.as_ref()
    }

    pub fn resolve(&self, host: Option<&str>, path: &str) -> Option<Matched<'_>> {
        if let Some(host) = host {
            let host = host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
            if let Some(site) = self.domains.get(&host.to_ascii_lowercase()) {
                return Some(Matched::Domain(site));
            }
        }
        let first = path.trim_start_matches('/').split(['/', '?']).next().unwrap_or("");
        self.sites.get(first).or(self.fallback.as_ref()).map(|site| Matched::Prefix(site))
    }
}
