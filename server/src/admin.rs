//! `/_platform/`: the server's own endpoints.
//!
//! | Path | Method | Who | What |
//! |---|---|---|---|
//! | `/_platform/` | GET | any | what each project uses: requests, CPU, traffic, disk |
//! | `requests` | GET | any | charts of an app's requests, times and errors (from `metrics`) |
//! | `usage?range=24h`, `stats`, `jobs` | GET | any | JSON |
//! | `metrics?app=NAME&range=24h` | GET | any | requests, errors, percentiles and where the time went, by minute (`project=NAME`: all its apps together) |
//! | `apps/NAME/logs[?lines=N\|since=OFFSET]` | GET | owner | the app's log (text; `x-log-offset` for following it) |
//! | `whoami` | GET | any | `{"role": "admin" \| "anonymous"}`: checks a token |
//! | `apps/NAME` | PUT | owner | upload a new component, then reload |
//! | `projects/NAME/deploy` | POST | admin | create or update a project: its apps with their settings (`managed.rs`) |
//! | `projects/NAME/secrets` | GET, PUT | admin | a deployed project's secret names; set or remove secrets |
//! | `apps/NAME/versions` | GET | owner | saved previous versions |
//! | `apps/NAME/rollback[?to=ID]` | POST | owner | back to the previous (or given) version |
//! | `reload` | POST | admin | re-read the deployment and switch to it |
//! | `backups` | GET | admin | database snapshots |
//! | `databases/NAME/backup` | POST | admin | take a snapshot now |
//!
//! Callers: the admin (`--admin-token`), or anonymous, which is allowed
//! read-only only when there is no admin token (local development). A token goes in
//! `Authorization: Bearer T` or the `platform_token` cookie, which `?token=T`
//! sets for browsers.

use crate::backup;
use crate::platform::Platform;
use crate::server::body;
use crate::versions;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use wasmtime::{Result, format_err};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Largest component upload.
const MAX_UPLOAD: usize = 64 << 20;
const COOKIE: &str = "platform_token";

#[derive(Clone, PartialEq)]
enum Principal {
    Admin,
    /// No admin token configured: read-only.
    Anonymous,
}

fn respond(status: StatusCode, content_type: &'static str, content: impl Into<Bytes>) -> Response<HyperOutgoingBody> {
    let mut resp = Response::new(body(content));
    *resp.status_mut() = status;
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn json(status: StatusCode, content: impl Into<Bytes>) -> Response<HyperOutgoingBody> {
    respond(status, "application/json", content)
}

fn error(status: StatusCode, message: &str) -> Response<HyperOutgoingBody> {
    json(status, format!("{{\"error\":{:?}}}\n", message))
}

/// Compares without leaking how much matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Who is calling, and the token to remember in a cookie (if it came in
/// the query string). `None` means a token was given but isn't valid, or
/// one is required.
fn authenticate(platform: &Platform, req: &Request<Incoming>) -> Option<(Principal, Option<String>)> {
    let bearer = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()?.strip_prefix("Bearer ").map(str::to_string));
    let cookie = req.headers().get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|v| {
        v.split(';').find_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
    });
    let query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_string)));
    let (token, from_query) = match (bearer.or(cookie), query) {
        (Some(t), _) => (t, false),
        (None, Some(t)) => (t, true),
        (None, None) => {
            return platform.admin_token.is_none().then_some((Principal::Anonymous, None));
        }
    };
    let admin = platform.admin_token.as_deref().is_some_and(|admin| same(admin.as_bytes(), token.as_bytes()));
    admin.then_some((Principal::Admin, from_query.then_some(token)))
}

pub async fn handle(platform: Arc<Platform>, req: Request<Incoming>, path: &str) -> Response<HyperOutgoingBody> {
    let Some((who, remember)) = authenticate(&platform, &req) else {
        return error(StatusCode::UNAUTHORIZED, "a valid admin token is required");
    };
    let mut resp = route(platform, req, path, who).await;
    if let Some(cookie) = remember.and_then(|t| HeaderValue::from_str(&format!("{COOKIE}={t}; Path=/_platform/; HttpOnly; SameSite=Strict; Max-Age=2592000")).ok()) {
        resp.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    resp
}

async fn route(platform: Arc<Platform>, req: Request<Incoming>, path: &str, who: Principal) -> Response<HyperOutgoingBody> {
    let method = req.method().clone();
    let segments: Vec<&str> = path.split('/').collect();
    let query = req.uri().query().unwrap_or("").to_string();
    let admin_only = || error(StatusCode::FORBIDDEN, "only the admin token can do this");
    match (&method, segments.as_slice()) {
        (&Method::GET, [""] | ["dashboard"]) => respond(StatusCode::OK, "text/html; charset=utf-8", include_str!("dashboard.html")),
        (&Method::GET, ["requests"]) => respond(StatusCode::OK, "text/html; charset=utf-8", include_str!("requests.html")),
        (&Method::GET, ["usage"]) => {
            let range = query.split('&').find_map(|kv| kv.strip_prefix("range=")).unwrap_or("24h").to_string();
            match crate::usage::json(&platform, &range) {
                Some(body) => json(StatusCode::OK, body),
                None => error(StatusCode::BAD_REQUEST, "range must be 1h, 6h, 24h, 7d or 30d"),
            }
        }
        (&Method::GET, ["stats"]) => {
            let names = platform.accounts.names();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            json(StatusCode::OK, platform.stats.to_json(&names, &|_| true))
        }
        (&Method::GET, ["jobs"]) => json(StatusCode::OK, jobs_json(&platform).await),
        (&Method::GET, ["metrics"]) => {
            let arg = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(|v| v.to_string());
            let (app, project) = match (arg("app"), arg("project")) {
                (Some(app), None) => (app, false),
                (None, Some(project)) => (project, true),
                _ => return error(StatusCode::BAD_REQUEST, "metrics?app=NAME (or project=NAME)&range=1h|6h|24h|7d|30d"),
            };
            let range = arg("range").unwrap_or_else(|| "24h".into());
            match crate::metrics::METRICS.get().and_then(|m| m.query(&app, project, &range)) {
                Some(body) => json(StatusCode::OK, body),
                None => error(StatusCode::BAD_REQUEST, "range must be 1h, 6h, 24h, 7d or 30d"),
            }
        }
        (&Method::GET, ["cluster"]) if who == Principal::Admin => {
            let nodes: Vec<serde_json::Value> = crate::cluster::primary()
                .map(|p| {
                    p.nodes
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|n| serde_json::json!({ "id": n.id, "addr": n.addr, "cores": n.cores, "cpu": n.cpu, "requests": n.requests(), "in_flight": n.sent.load(std::sync::atomic::Ordering::Relaxed), "seen_ms_ago": n.last_seen.elapsed().as_millis() as u64, "draining": n.draining }))
                        .collect()
                })
                .unwrap_or_default();
            let local = crate::cluster::primary().map(|p| p.local_inflight.load(std::sync::atomic::Ordering::Relaxed));
            let autoscale = crate::autoscale::STATUS.lock().unwrap().clone();
            let names = platform.accounts.names();
            let here: u64 = (0..names.len()).map(|id| platform.stats.own_totals(id)[crate::stats::Counter::HttpRequests as usize]).sum();
            json(StatusCode::OK, serde_json::json!({ "primary": crate::cluster::primary().is_some(), "requests_here": here, "in_flight_here": local, "workers": nodes, "autoscale": autoscale }).to_string())
        }
        (&Method::GET, ["whoami"]) => {
            let role = match &who {
                Principal::Admin => serde_json::json!({ "role": "admin" }),
                Principal::Anonymous => serde_json::json!({ "role": "anonymous" }),
            };
            json(StatusCode::OK, role.to_string())
        }
        (&Method::POST, ["reload"]) if who == Principal::Admin => reload(platform, Vec::new()).await,
        (&Method::POST, ["reload"]) => admin_only(),
        (_, ["apps", app, ..]) if who != Principal::Admin => {
            error(StatusCode::FORBIDDEN, &format!("not allowed to manage {app:?}"))
        }
        (&Method::PUT, ["apps", app]) => deploy(platform, req, app.to_string()).await,
        // projects deployed over the API (`apps/NAME/...`: the same, from
        // before there were projects)
        (&Method::POST, ["projects" | "apps", project, "deploy"]) if who == Principal::Admin => deploy_managed(platform, req, project.to_string()).await,
        (&Method::GET, ["projects" | "apps", project, "secrets"]) if who == Principal::Admin => match crate::managed::secret_names(&platform.data_dir, project) {
            Ok(names) => json(StatusCode::OK, serde_json::json!({ "names": names }).to_string()),
            Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
        },
        (&Method::PUT, ["projects" | "apps", project, "secrets"]) if who == Principal::Admin => set_secrets(platform, req, project.to_string()).await,
        (_, ["apps", _, "deploy" | "secrets"] | ["projects", ..]) => admin_only(),
        (&Method::GET, ["apps", app, "versions"]) => list_versions(&platform, app),
        (&Method::GET, ["apps", app, "logs"]) => {
            let arg = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).and_then(|v| v.parse::<u64>().ok());
            let (text, offset) = match arg("since") {
                Some(from) => crate::logs::since(app, from),
                None => {
                    let text = crate::logs::tail(app, arg("lines").unwrap_or(100).min(10_000) as usize);
                    (text, crate::logs::since(app, u64::MAX).1)
                }
            };
            let mut resp = respond(StatusCode::OK, "text/plain; charset=utf-8", text);
            resp.headers_mut().insert("x-log-offset", HeaderValue::from(offset));
            resp
        }
        (&Method::POST, ["apps", app, "rollback"]) => {
            let to = query.split('&').find_map(|kv| kv.strip_prefix("to=")).map(str::to_string);
            rollback(platform, app.to_string(), to).await
        }
        (&Method::GET, ["backups"]) if who == Principal::Admin => list_backups(&platform),
        (&Method::POST, ["databases", db, "backup"]) if who == Principal::Admin => backup_now(platform, db.to_string()).await,
        (_, ["backups"] | ["databases", ..]) => admin_only(),
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn jobs_json(platform: &Platform) -> String {
    let Some(store) = &platform.jobs else {
        return "{\"jobs\":[]}\n".to_string();
    };
    let rows: Vec<String> = store
        .counts()
        .await
        .iter()
        .map(|c| format!("{{\"kind\":{:?},\"name\":{:?},\"app\":{:?},\"pending\":{},\"dead\":{}}}", c.kind, c.name, c.app, c.pending, c.dead))
        .collect();
    format!("{{\"jobs\":[{}]}}\n", rows.join(","))
}

/// Reloads on a blocking thread (compiling can take a while) so the worker
/// keeps serving.
async fn reload(platform: Arc<Platform>, changed: Vec<String>) -> Response<HyperOutgoingBody> {
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || platform.reload().map(|s| (s, platform.current().version))).await;
    match result {
        Ok(Ok((summary, version))) => {
            let loaded: Vec<String> = summary.loaded.iter().map(|s| format!("{s:?}")).collect();
            eprintln!("reload: version {version}, {} apps, {} sites, loaded [{}]", summary.apps, summary.sites, summary.loaded.join(", "));
            json(
                StatusCode::OK,
                format!(
                    "{{\"version\":{version},\"apps\":{},\"sites\":{},\"loaded\":[{}],\"changed\":{:?},\"ms\":{}}}\n",
                    summary.apps,
                    summary.sites,
                    loaded.join(","),
                    changed,
                    started.elapsed().as_millis()
                ),
            )
        }
        Ok(Err(e)) => {
            eprintln!("reload failed, still running the previous version: {e:?}");
            error(StatusCode::UNPROCESSABLE_ENTITY, &format!("{e:?}"))
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn component_path(platform: &Platform, app: &str) -> Option<PathBuf> {
    platform.current().deployment.apps.get(app).map(|spec| spec.component.clone())
}

/// Replaces an app's component file with the uploaded one, then reloads.
/// The replaced file goes to the version history; if the new one doesn't
/// load, it's put back.
async fn deploy(platform: Arc<Platform>, req: Request<Incoming>, app: String) -> Response<HyperOutgoingBody> {
    let Some(target) = component_path(&platform, &app) else {
        return error(StatusCode::NOT_FOUND, &format!("no app named {app:?} in the deployment; add it there first"));
    };
    let bytes = match Limited::new(req.into_body(), MAX_UPLOAD).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
    };
    let engine = platform.engine().clone();
    let (data_dir, app_name, path) = (platform.data_dir.clone(), app.clone(), target.clone());
    let written = tokio::task::spawn_blocking(move || -> Result<Option<PathBuf>> {
        let data = prepare_component(&engine, &path, &bytes)?;
        let saved = versions::save(&data_dir, &app_name, &path)?;
        let tmp = path.with_extension("upload");
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(saved)
    })
    .await;
    let saved = match written {
        Ok(Ok(saved)) => saved,
        Ok(Err(e)) => return error(StatusCode::UNPROCESSABLE_ENTITY, &format!("{e:?}")),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let resp = reload(platform, vec![app]).await;
    if resp.status() != StatusCode::OK {
        if let Some(saved) = saved {
            let _ = versions::install(&saved, &target);
        }
    }
    resp
}

/// `plumb deploy`: a project's apps with their settings and schema, created
/// or replaced together without downtime; if the new version doesn't load,
/// the old one stays.
async fn deploy_managed(platform: Arc<Platform>, req: Request<Incoming>, app: String) -> Response<HyperOutgoingBody> {
    let bytes = match Limited::new(req.into_body(), MAX_UPLOAD * 2).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
    };
    let upload: crate::managed::Upload = match serde_json::from_slice(&bytes) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("the deploy request: {e}")),
    };
    let current = platform.current();
    let root = crate::managed::root(&platform.data_dir);
    let fixed: Vec<String> = current
        .deployment
        .apps
        .iter()
        .filter(|(_, spec)| !spec.component.starts_with(&root))
        .map(|(n, _)| n.clone())
        .chain(current.deployment.sites.iter().filter(|(_, spec)| !spec.static_dir.starts_with(&root)).map(|(n, _)| n.clone()))
        .collect();
    drop(current);
    let engine = platform.engine().clone();
    let (data_dir, name) = (platform.data_dir.clone(), app.clone());
    let staged = match tokio::task::spawn_blocking(move || crate::managed::stage(&engine, &data_dir, &name, upload, &fixed)).await {
        Ok(Ok(staged)) => staged,
        Ok(Err(e)) => return error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let resp = reload(platform, vec![app]).await;
    if resp.status() == StatusCode::OK {
        staged.commit();
    } else {
        staged.restore();
    }
    resp
}

/// Sets or removes secrets: `{"KEY": "value", "OLD": null}`; then reloads.
async fn set_secrets(platform: Arc<Platform>, req: Request<Incoming>, app: String) -> Response<HyperOutgoingBody> {
    let bytes = match Limited::new(req.into_body(), 1 << 20).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
    };
    let changes: std::collections::BTreeMap<String, Option<String>> = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("expected {{\"KEY\": \"value\" or null}}: {e}")),
    };
    let before = match crate::managed::set_secrets(&platform.data_dir, &app, changes) {
        Ok(b) => b,
        Err(e) => return error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    };
    let resp = reload(platform.clone(), vec![app.clone()]).await;
    if resp.status() != StatusCode::OK {
        crate::managed::restore_secrets(&platform.data_dir, &app, before);
    }
    resp
}

/// The bytes to store at `target`: an uploaded `.wasm` is compiled when the
/// app runs from a `.cwasm`.
fn prepare_component(engine: &wasmtime::Engine, target: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    let is_wasm = bytes.starts_with(b"\0asm");
    let wants_precompiled = target.extension().is_some_and(|e| e == "cwasm");
    match (is_wasm, wants_precompiled) {
        (true, true) => engine.precompile_component(bytes),
        (true, false) => Ok(bytes.to_vec()),
        (false, true) if wasmtime::Engine::detect_precompiled(bytes).is_some() => Ok(bytes.to_vec()),
        (false, true) => Err(format_err!("upload is neither a wasm component nor a precompiled .cwasm")),
        (false, false) => Err(format_err!("{} expects a .wasm component", target.display())),
    }
}

fn list_versions(platform: &Platform, app: &str) -> Response<HyperOutgoingBody> {
    let Some(target) = component_path(platform, app) else {
        return error(StatusCode::NOT_FOUND, &format!("no app named {app:?}"));
    };
    let current = versions::hash_file(&target).ok();
    let rows: Vec<String> = versions::list(&platform.data_dir, app)
        .iter()
        .map(|v| {
            format!(
                "{{\"id\":{:?},\"saved_ms\":{},\"hash\":{:?},\"bytes\":{},\"current\":{}}}",
                v.id,
                v.saved_ms,
                v.hash,
                v.bytes,
                current.as_deref() == Some(v.hash.as_str())
            )
        })
        .collect();
    let current = current.map_or("null".to_string(), |h| format!("{h:?}"));
    json(StatusCode::OK, format!("{{\"app\":{app:?},\"current\":{current},\"versions\":[{}]}}\n", rows.join(",")))
}

/// Switches an app to a saved version: `to`, or the newest one that differs
/// from what runs now. What runs now is saved first, so a rollback can be
/// undone the same way.
async fn rollback(platform: Arc<Platform>, app: String, to: Option<String>) -> Response<HyperOutgoingBody> {
    let Some(target) = component_path(&platform, &app) else {
        return error(StatusCode::NOT_FOUND, &format!("no app named {app:?}"));
    };
    let current = versions::hash_file(&target).ok();
    let history = versions::list(&platform.data_dir, &app);
    let chosen = match &to {
        Some(id) => history.iter().find(|v| &v.id == id),
        None => history.iter().find(|v| Some(&v.hash) != current.as_ref()),
    };
    let Some(chosen) = chosen else {
        return error(StatusCode::NOT_FOUND, "no such version (see versions)");
    };
    let saved = match versions::save(&platform.data_dir, &app, &target).and_then(|saved| {
        versions::install(&chosen.path, &target)?;
        Ok(saved)
    }) {
        Ok(saved) => saved,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
    };
    let resp = reload(platform, vec![app]).await;
    if resp.status() != StatusCode::OK {
        if let Some(saved) = saved {
            let _ = versions::install(&saved, &target);
        }
    }
    resp
}

fn list_backups(platform: &Platform) -> Response<HyperOutgoingBody> {
    let Some(config) = &platform.backups else {
        return error(StatusCode::NOT_FOUND, "backups are off (start with --backup-dir)");
    };
    let rows: Vec<String> = backup::sqlite_databases(platform)
        .iter()
        .flat_map(|(db, _)| backup::list(&config.dir, db))
        .map(|s| format!("{{\"db\":{:?},\"file\":{:?},\"bytes\":{},\"taken_ms\":{}}}", s.db, s.path.display().to_string(), s.bytes, s.taken_ms))
        .collect();
    json(StatusCode::OK, format!("{{\"backups\":[{}]}}\n", rows.join(",")))
}

async fn backup_now(platform: Arc<Platform>, db: String) -> Response<HyperOutgoingBody> {
    let Some(config) = platform.backups.clone() else {
        return error(StatusCode::NOT_FOUND, "backups are off (start with --backup-dir)");
    };
    let Some((_, path)) = backup::sqlite_databases(&platform).into_iter().find(|(name, _)| *name == db) else {
        return error(StatusCode::NOT_FOUND, &format!("no SQLite database named {db:?}"));
    };
    match tokio::task::spawn_blocking(move || backup::snapshot(&config, &db, &path)).await {
        Ok(Ok(s)) => json(
            StatusCode::OK,
            format!("{{\"db\":{:?},\"file\":{:?},\"bytes\":{},\"taken_ms\":{}}}\n", s.db, s.path.display().to_string(), s.bytes, s.taken_ms),
        ),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
