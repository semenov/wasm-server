//! Turns the deployment flags of `serve` (`--app`, `--static`, `--db`, ...)
//! into a `Deployment`, the same thing a `--config` file describes.

use crate::deployment::{AppSpec, DatabaseSpec, Deployment, SiteSpec};
use std::path::PathBuf;
use wasmtime::{Result, bail, format_err};

pub struct Flags {
    pub apps: Vec<(String, String)>,
    pub statics: Vec<(String, String)>,
    pub domains: Vec<(String, String)>,
    pub databases: Vec<(String, String)>,
    pub db_inits: Vec<(String, String)>,
    pub grants: Vec<(String, String)>,
    pub app_db_timeouts: Vec<(String, String)>,
    pub queues: Vec<(String, String)>,
    pub crons: Vec<(String, String, String)>,
    pub links: Vec<(String, String, String)>,
    pub websockets: Vec<(String, String)>,
    pub credits: Vec<(String, String)>,
    pub envs: Vec<(String, String, String)>,
    pub outbound: Vec<(String, String)>,
}

fn app<'a>(d: &'a mut Deployment, flag: &str, name: &str) -> Result<&'a mut AppSpec> {
    d.apps.get_mut(name).ok_or_else(|| format_err!("{flag} {name}...: no app named {name:?}"))
}

pub fn deployment(f: Flags) -> Result<Deployment> {
    let mut d = Deployment::default();
    for (name, url) in f.databases {
        let init = f.db_inits.iter().rev().find(|(db, _)| *db == name).map(|(_, file)| PathBuf::from(file));
        d.databases.insert(name, DatabaseSpec { url, init });
    }
    for (db, _) in &f.db_inits {
        if !d.databases.contains_key(db) {
            bail!("--db-init {db}=...: no database named {db:?}");
        }
    }
    for (name, path) in f.apps {
        d.apps.insert(name, AppSpec { component: PathBuf::from(path), ..Default::default() });
    }
    for (name, db) in &f.grants {
        app(&mut d, "--grant", name)?.databases.push(db.clone());
    }
    for (name, ms) in &f.app_db_timeouts {
        let ms = ms.parse().map_err(|_| format_err!("--app-db-timeout-ms {name}={ms}: not a number"))?;
        app(&mut d, "--app-db-timeout-ms", name)?.db_timeout_ms = Some(ms);
    }
    for (queue, name) in &f.queues {
        app(&mut d, "--queue", name)?.queues.push(queue.clone());
    }
    for (name, job, expr) in &f.crons {
        app(&mut d, "--cron", name)?.cron.insert(job.clone(), expr.clone());
    }
    for (name, iface, target) in &f.links {
        app(&mut d, "--link", name)?.links.insert(iface.clone(), target.clone());
    }
    for (name, host) in &f.outbound {
        let list = app(&mut d, "--outbound", name)?.outbound.get_or_insert_with(Vec::new);
        if host != "none" {
            list.push(host.clone());
        }
    }
    for (name, path) in &f.websockets {
        app(&mut d, "--websocket", name)?.websocket = Some(path.clone());
    }
    for (name, key, value) in &f.envs {
        app(&mut d, "--env", name)?.env.insert(key.clone(), value.clone());
    }
    for (name, dir) in f.statics {
        match d.apps.get_mut(&name) {
            Some(a) => a.static_dir = Some(PathBuf::from(dir)),
            None => drop(d.sites.insert(name, SiteSpec { static_dir: PathBuf::from(dir), domains: Vec::new(), path: None, spa: false, credits: None, tenant: None })),
        }
    }
    for (host, name) in f.domains {
        if let Some(a) = d.apps.get_mut(&name) {
            a.domains.push(host);
        } else if let Some(s) = d.sites.get_mut(&name) {
            s.domains.push(host);
        } else {
            bail!("--domain {host}={name}: no app or static site named {name:?}");
        }
    }
    for (name, n) in f.credits {
        let n: f64 = n.parse().map_err(|_| format_err!("--credits {name}={n}: not a number"))?;
        if let Some(a) = d.apps.get_mut(&name) {
            a.credits = Some(n);
        } else if let Some(s) = d.sites.get_mut(&name) {
            s.credits = Some(n);
        } else {
            bail!("--credits {name}=...: no app or site named {name:?}");
        }
    }
    Ok(d)
}
