//! Which site handles a request: by domain (and the longest matching path
//! under it) first, then by a `/NAME` path prefix.

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
    /// Through one of the site's domains, under the site's path there (`/`,
    /// `/api`): the app sees the whole path, its files are below that path.
    Domain(&'a Site, &'a str),
    /// Through the `/NAME/...` prefix (or the fallback site).
    Prefix(&'a Site),
}

/// Immutable routing table, shared by all cores without locks.
pub struct Routes {
    sites: HashMap<String, Arc<Site>>,
    /// Per domain: (path, site), longest path first.
    domains: HashMap<String, Vec<(String, Arc<Site>)>>,
    /// Receives requests that match nothing else: the site named `default`,
    /// or the only site when exactly one is deployed.
    fallback: Option<Arc<Site>>,
}

impl Routes {
    pub fn new(apps: Vec<App>, statics: Vec<(String, StaticSite)>, domains: &[(String, String, String)], accounts: &Accounts) -> Result<Self> {
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

        let mut by_domain: HashMap<String, Vec<(String, Arc<Site>)>> = HashMap::new();
        for (domain, path, name) in domains {
            let Some(site) = sites.get(name) else {
                bail!("--domain {domain}={name}: no app or static site named {name:?}");
            };
            let list = by_domain.entry(domain.to_ascii_lowercase()).or_default();
            if list.iter().any(|(p, _)| p == path) {
                bail!("--domain {domain}{}: assigned twice", path.trim_end_matches('/'));
            }
            list.push((path.clone(), site.clone()));
            list.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
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
            if let Some(list) = self.domains.get(&host.to_ascii_lowercase()) {
                // nothing matching under a domain that has sites: not found,
                // rather than some other site's page
                return list.iter().find(|(prefix, _)| under(path, prefix)).map(|(prefix, site)| Matched::Domain(site, prefix));
            }
        }
        let first = path.trim_start_matches('/').split(['/', '?']).next().unwrap_or("");
        self.sites.get(first).or(self.fallback.as_ref()).map(|site| Matched::Prefix(site))
    }
}

/// Whether `path` is `prefix` or below it (`/api` takes `/api/x`, not `/apix`).
fn under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?']))
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn paths() {
        assert!(under("/", "/") && under("/anything", "/"));
        assert!(under("/api", "/api") && under("/api/notes", "/api") && under("/api?x=1", "/api"));
        assert!(!under("/apix", "/api") && !under("/", "/api") && !under("/ap", "/api"));
    }
}
