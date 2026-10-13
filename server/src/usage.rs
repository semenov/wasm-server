//! What each project uses, for the owner to see which one needs attention
//! (`GET /_platform/usage?range=24h`, drawn at `/_platform/`): requests,
//! failures, CPU, database and outgoing HTTP, traffic, queues, WebSockets
//! over the range (from `metrics.rs`), and what it holds now: disk
//! (databases and files) and cache.
//!
//! A project is a deployed project's apps (`shop`, `shop.api`); an app of
//! the deployment file is a project of its own.

use crate::platform::Platform;
use serde_json::{Map, Value as J, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Bytes of a file or of everything under a directory.
fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map_or(0, |entries| entries.flatten().map(|e| size(&e.path())).sum())
}

/// A SQLite database with its WAL.
fn sqlite_size(path: &Path) -> u64 {
    size(path) + size(Path::new(&format!("{}-wal", path.display())))
}

#[derive(Default)]
struct Project {
    apps: Vec<String>,
    sums: BTreeMap<&'static str, i64>,
    database_bytes: u64,
    file_bytes: u64,
    cache_bytes: u64,
    websockets: u64,
}

pub fn json(platform: &Platform, range: &str) -> Option<String> {
    crate::metrics::range_steps(range)?;
    let totals = crate::metrics::METRICS.get().and_then(|m| m.totals(range)).unwrap_or_default();
    let current = platform.current();
    let open = platform.hub.open_by_app();
    let mut projects: BTreeMap<String, Project> = BTreeMap::new();
    for (id, name) in platform.accounts.names().iter().enumerate() {
        let deployed = current.routes.app(name).is_some() || current.deployment.sites.contains_key(name);
        if !deployed {
            continue;
        }
        let p = projects.entry(crate::managed::project_of(name).to_string()).or_default();
        p.apps.push(name.clone());
        if let Some(app) = current.routes.app(name) {
            p.cache_bytes += app.config.cache.weighted_size();
        }
        p.websockets += open.get(&id).copied().unwrap_or(0);
    }
    for (app, sums) in totals {
        let p = projects.entry(crate::managed::project_of(&app).to_string()).or_default();
        for (col, v) in sums {
            *p.sums.entry(col).or_default() += v;
        }
    }
    // disk: deployed projects' own databases and files
    let root = crate::managed::root(&platform.data_dir);
    for (name, p) in projects.iter_mut() {
        let dir = root.join(name);
        if dir.is_dir() {
            if let Ok(dbs) = std::fs::read_dir(dir.join("db")) {
                p.database_bytes += dbs.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "db")).map(|e| sqlite_size(&e.path())).sum::<u64>();
            }
            p.file_bytes = size(&dir.join("files")) + sqlite_size(&dir.join("files.db"));
        }
    }
    // the deployment file's SQLite databases: the first app granted one
    for db in &platform.databases {
        let Some(path) = db.url.strip_prefix("sqlite:") else { continue };
        let owner = current.deployment.apps.iter().find(|(_, spec)| spec.databases.contains(&db.name)).map(|(n, _)| n.clone());
        if let Some(p) = owner.and_then(|o| projects.get_mut(crate::managed::project_of(&o))) {
            p.database_bytes += sqlite_size(Path::new(path));
        }
    }
    let mut out: Vec<J> = projects
        .into_iter()
        .filter(|(_, p)| !p.apps.is_empty() || p.sums.values().any(|v| *v != 0))
        .map(|(name, p)| {
            let g = |c: &str| p.sums.get(c).copied().unwrap_or(0);
            let mut o = Map::new();
            o.insert("name".into(), json!(name));
            o.insert("apps".into(), json!(p.apps));
            o.insert("requests".into(), json!(g("requests")));
            o.insert("errors".into(), json!(g("errors") + g("timeouts") + g("http_5xx")));
            o.insert("events".into(), json!(g("events")));
            o.insert("static_requests".into(), json!(g("static_requests")));
            o.insert("cpu_seconds".into(), json!(g("cpu_ns") as f64 / 1e9));
            o.insert("db_queries".into(), json!(g("db_queries")));
            o.insert("db_seconds".into(), json!(g("db_wait_ns") as f64 / 1e9));
            o.insert("outbound_requests".into(), json!(g("outbound_requests")));
            o.insert("egress_bytes".into(), json!(g("egress_bytes") + g("outbound_bytes_sent")));
            o.insert("ingress_bytes".into(), json!(g("outbound_bytes_received")));
            o.insert("cache_ops".into(), json!(g("cache_ops")));
            o.insert("queue_messages".into(), json!(g("queue_messages")));
            o.insert("ws_messages".into(), json!(g("ws_messages_in") + g("ws_messages_out")));
            o.insert("database_bytes".into(), json!(p.database_bytes));
            o.insert("file_bytes".into(), json!(p.file_bytes));
            o.insert("cache_bytes".into(), json!(p.cache_bytes));
            o.insert("websockets".into(), json!(p.websockets));
            J::Object(o)
        })
        .collect();
    // the busiest first
    out.sort_by(|a, b| b["cpu_seconds"].as_f64().partial_cmp(&a["cpu_seconds"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    Some(json!({ "range": range, "projects": out }).to_string() + "\n")
}
