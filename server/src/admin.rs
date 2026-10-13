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
//! | `blobs/missing` | POST | admin | `{"hashes": [...]}` → the ones the store lacks (`store.rs`) |
//! | `blobs` | POST, GET `blobs/HASH` | admin | upload parts (`HASH LENGTH\n` + bytes, repeated); read one |
//! | `projects/NAME/releases` | POST | admin | `{"release": {...}, "note"}`: run a release whose parts are stored (`managed.rs`) |
//! | `projects/NAME/releases` | GET | any | the releases it ran, newest first, and the current one |
//! | `projects/NAME/rollback[?to=ID]` | POST | admin | back to the previous (or given) release |
//! | `projects/NAME/databases` | GET | admin | its databases; `databases/DB/snapshot` (a SQLite file), `databases/DB/dump` (SQL), POST `databases/DB/query` `{"sql"}` |
//! | `projects/NAME/files[?prefix=&after=]` | GET | admin | its users' files; `files/KEY`: one, as it is |
//! | `projects/NAME/migrations` | GET | any | every run of its migrations' steps, with results (`migrations.rs`) |
//! | `projects/NAME/deploy` | POST | admin | a release with every part inline (base64) |
//! | `releases/ID` | GET | any | a release's manifest |
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
        // a project's apps change only with a release of the whole project
        (&Method::PUT | &Method::POST, ["apps", app] | ["apps", app, "rollback"]) if platform.current().deployment.apps.get(*app).is_some_and(|s| s.project.is_some()) => {
            let project = crate::managed::project_of(app);
            error(StatusCode::CONFLICT, &format!("{app:?} is an app of project {project:?}: deploy the project (plumb deploy), or roll it back with POST /_platform/projects/{project}/rollback"))
        }
        (&Method::PUT, ["apps", app]) => deploy(platform, req, app.to_string()).await,
        // projects deployed over the API (`apps/NAME/...`: the same, from
        // before there were projects)
        (&Method::POST, ["projects" | "apps", project, "deploy"]) if who == Principal::Admin => deploy_managed(platform, req, project.to_string()).await,
        (&Method::POST, ["projects", project, "releases"]) if who == Principal::Admin => deploy_release(platform, req, project.to_string()).await,
        (&Method::GET, ["projects", project, "releases"]) => list_releases(&platform, project),
        // the project's data, for its owner to take away or look into
        (&Method::GET, ["projects", project, "databases"]) if who == Principal::Admin => json(StatusCode::OK, crate::dbtools::list(&platform.data_dir, project).to_string()),
        (&Method::GET, ["projects", project, "databases", db, what @ ("snapshot" | "dump")]) if who == Principal::Admin => {
            let file = match crate::dbtools::db_file(&platform.data_dir, project, db) {
                Ok(f) => f,
                Err(e) => return error(StatusCode::NOT_FOUND, &e.to_string()),
            };
            let what = what.to_string();
            let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                if what == "snapshot" { crate::dbtools::snapshot(&file) } else { crate::dbtools::dump(&file).map(String::into_bytes) }
            })
            .await;
            match result {
                Ok(Ok(b)) => respond(StatusCode::OK, "application/octet-stream", b),
                Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        (&Method::POST, ["projects", project, "databases", db, "query"]) if who == Principal::Admin => {
            let file = match crate::dbtools::db_file(&platform.data_dir, project, db) {
                Ok(f) => f,
                Err(e) => return error(StatusCode::NOT_FOUND, &e.to_string()),
            };
            let bytes = match body_bytes(req, 1 << 20).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            let sql = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| v.get("sql").and_then(|s| s.as_str()).map(str::to_string)).unwrap_or_default();
            match tokio::task::spawn_blocking(move || crate::dbtools::query(&file, &sql, 1000)).await {
                Ok(Ok(v)) => json(StatusCode::OK, v.to_string()),
                Ok(Err(e)) => error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        (&Method::GET, ["projects", project, "files"]) if who == Principal::Admin => {
            let arg = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).unwrap_or("").to_string();
            let call = serde_json::json!({ "op": "list", "prefix": arg("prefix"), "after": arg("after"), "limit": 1000 });
            let (data_dir, p) = (platform.data_dir.clone(), project.to_string());
            match tokio::task::spawn_blocking(move || crate::storage::run(&data_dir, &p, &call)).await {
                Ok(Ok(v)) => json(StatusCode::OK, serde_json::json!({ "files": v }).to_string()),
                Ok(Err(e)) => error(StatusCode::NOT_FOUND, &e),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        (&Method::GET, ["projects", project, "files", ..]) if who == Principal::Admin => {
            let key = path.splitn(4, '/').nth(3).unwrap_or("").to_string();
            let (data_dir, p) = (platform.data_dir.clone(), project.to_string());
            match tokio::task::spawn_blocking(move || crate::storage::read_file(&data_dir, &p, &key)).await {
                Ok(Ok(Some((data, _)))) => respond(StatusCode::OK, "application/octet-stream", data),
                Ok(Ok(None)) => error(StatusCode::NOT_FOUND, "no such file"),
                Ok(Err(e)) => error(StatusCode::NOT_FOUND, &e),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        (&Method::GET, ["projects", project, "migrations"]) => {
            let runs = crate::migrations::ledger(&platform.data_dir, project);
            json(StatusCode::OK, serde_json::json!({ "project": project, "runs": runs }).to_string())
        }
        (&Method::POST, ["projects", project, "rollback"]) if who == Principal::Admin => {
            let to = query.split('&').find_map(|kv| kv.strip_prefix("to=")).map(str::to_string);
            rollback_project(platform, project.to_string(), to).await
        }
        // the store: which parts a server lacks, uploading them, reading them
        (&Method::POST, ["blobs", "missing"]) if who == Principal::Admin => {
            let bytes = match body_bytes(req, 16 << 20).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            let hashes: Vec<String> = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("hashes").and_then(|h| h.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()))
                .unwrap_or_default();
            let missing: Vec<&String> = hashes.iter().filter(|h| !crate::store::has_blob(&platform.data_dir, h)).collect();
            json(StatusCode::OK, serde_json::json!({ "missing": missing }).to_string())
        }
        (&Method::POST, ["blobs"]) if who == Principal::Admin => {
            let bytes = match body_bytes(req, 1 << 30).await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            let data_dir = platform.data_dir.clone();
            match tokio::task::spawn_blocking(move || crate::store::put_batch(&data_dir, &bytes)).await {
                Ok(Ok(n)) => json(StatusCode::OK, serde_json::json!({ "stored": n }).to_string()),
                Ok(Err(e)) => error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        (&Method::GET, ["blobs", hash]) if who == Principal::Admin => match crate::store::read_blob(&platform.data_dir, hash) {
            Ok(b) => respond(StatusCode::OK, "application/octet-stream", b),
            Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
        },
        (&Method::GET, ["releases", id]) => match crate::store::read_release(&platform.data_dir, id) {
            Ok(b) => json(StatusCode::OK, b),
            Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
        },
        (_, ["blobs", ..]) => admin_only(),
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

/// Names of the deployment file's apps and sites, which projects can't take.
fn fixed_names(platform: &Platform) -> Vec<String> {
    let current = platform.current();
    let store = platform.data_dir.join("store");
    current
        .deployment
        .apps
        .iter()
        .filter(|(_, spec)| spec.project.is_none())
        .map(|(n, _)| n.clone())
        .chain(current.deployment.sites.iter().filter(|(_, spec)| !spec.static_dir.starts_with(&store)).map(|(n, _)| n.clone()))
        .collect()
}

async fn body_bytes(req: Request<Incoming>, limit: usize) -> Result<Bytes, Response<HyperOutgoingBody>> {
    match Limited::new(req.into_body(), limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => Err(error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string())),
    }
}

/// Makes `release` the project's: checks it, compiles and lays out what's
/// new, applies its schema, points the project at it and reloads; if that
/// fails, the project goes back to the release it ran.
async fn activate(platform: Arc<Platform>, project: String, release: crate::managed::Release, note: String) -> Response<HyperOutgoingBody> {
    // one deploy of a project at a time
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    let lock = LOCKS.get_or_init(Default::default).lock().unwrap().entry(project.clone()).or_default().clone();
    let _deploying = lock.lock().await;
    let taken = fixed_names(&platform);
    let (data_dir, p) = (platform.data_dir.clone(), project.clone());
    let r = release.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<(String, Option<String>)> {
        crate::managed::check(&data_dir, &p, &r, &taken)?;
        let id = crate::managed::put_release(&data_dir, &r)?;
        crate::managed::prepare(&data_dir, &p, &r)?;
        Ok((id, crate::managed::current(&data_dir, &p)))
    })
    .await;
    let (id, previous) = match prepared {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let data_dir = platform.data_dir.clone();
    // the migrations' `before` steps, after a snapshot of the databases
    let mut notes: Vec<String> = Vec::new();
    let before: Vec<crate::managed::Migration> = crate::migrations::pending(&data_dir, &project, &release, "before").into_iter().cloned().collect();
    if !before.is_empty() {
        let (d, p, r, label) = (data_dir.clone(), project.clone(), release.clone(), before[0].file.split('_').next().unwrap_or("").to_string());
        match tokio::task::spawn_blocking(move || crate::migrations::snapshot(&d, &p, &r, &label)).await {
            Ok(Ok(snaps)) if !snaps.is_empty() => notes.push(format!("databases saved first: {}", snaps.join(", "))),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
        for m in &before {
            if let Err(e) = crate::migrations::run(&platform, &project, &release, m, "before").await {
                let mut msg = format!("migration {} failed before the release went live, so it didn't: {e}", m.file);
                if !notes.is_empty() {
                    msg += &format!(" ({})", notes.join("; "));
                }
                return error(StatusCode::UNPROCESSABLE_ENTITY, &msg);
            }
            notes.push(format!("migration {}: `before` done", m.file));
        }
    }
    if let Err(e) = crate::managed::set_current(&data_dir, &project, Some(&id)) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let resp = reload(platform.clone(), vec![project.clone()]).await;
    if resp.status() != StatusCode::OK {
        let _ = crate::managed::set_current(&data_dir, &project, previous.as_deref());
        if !before.is_empty() {
            // the data moved on; say so with the refusal
            let msg = format!("{}; the migrations' `before` steps stay done: {}", String::from_utf8_lossy(&resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default()).trim(), notes.join("; "));
            return error(StatusCode::UNPROCESSABLE_ENTITY, &msg);
        }
        return resp;
    }
    let unchanged = previous.as_deref() == Some(id.as_str());
    if !unchanged {
        let (d, p, i) = (data_dir.clone(), project.clone(), id.clone());
        let _ = tokio::task::spawn_blocking(move || crate::managed::record(&d, &p, &i, &note)).await;
    }
    let after: Vec<String> = crate::migrations::pending(&data_dir, &project, &release, "after").iter().map(|m| m.file.clone()).collect();
    if !after.is_empty() {
        notes.push(format!("running `after` in the background: {} (plumb migrations shows how they went)", after.join(", ")));
        crate::migrations::spawn_after(platform.clone(), project.clone(), release);
    }
    json(StatusCode::OK, serde_json::json!({ "project": project, "release": id, "previous": previous, "unchanged": unchanged, "notes": notes }).to_string())
}

/// `{"release": {...}, "note": "..."}`: a release whose parts are in the
/// store (`blobs/missing`, then `blobs`).
async fn deploy_release(platform: Arc<Platform>, req: Request<Incoming>, project: String) -> Response<HyperOutgoingBody> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        release: crate::managed::Release,
        #[serde(default)]
        note: String,
    }
    let bytes = match body_bytes(req, 16 << 20).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match serde_json::from_slice::<Body>(&bytes) {
        Ok(b) => activate(platform, project, b.release, b.note).await,
        Err(e) => error(StatusCode::BAD_REQUEST, &format!("the release: {e}")),
    }
}

/// The same with every part inline (base64): turned into blobs first.
async fn deploy_managed(platform: Arc<Platform>, req: Request<Incoming>, project: String) -> Response<HyperOutgoingBody> {
    let bytes = match body_bytes(req, MAX_UPLOAD * 2).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let upload: crate::managed::Upload = match serde_json::from_slice(&bytes) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("the deploy request: {e}")),
    };
    let (data_dir, p) = (platform.data_dir.clone(), project.clone());
    match tokio::task::spawn_blocking(move || upload.into_release(&data_dir, &p)).await {
        Ok(Ok(release)) => activate(platform, project, release, String::new()).await,
        Ok(Err(e)) => error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Back to an earlier release: the given one, or the last one before the
/// current that differs from it.
async fn rollback_project(platform: Arc<Platform>, project: String, to: Option<String>) -> Response<HyperOutgoingBody> {
    let data_dir = platform.data_dir.clone();
    let current = crate::managed::current(&data_dir, &project);
    let history = crate::managed::history(&data_dir, &project);
    let target = match to {
        Some(t) => history.iter().rev().find(|e| e.id.starts_with(&t)).map(|e| e.id.clone()),
        None => history.iter().rev().map(|e| &e.id).find(|id| Some(*id) != current.as_ref()).cloned(),
    };
    let Some(target) = target else {
        return error(StatusCode::NOT_FOUND, &format!("no earlier release of {project:?} to go back to"));
    };
    match crate::managed::load_release(&data_dir, &target) {
        Ok(release) => activate(platform, project, release, format!("rollback to {}", &target[..12])).await,
        Err(e) => error(StatusCode::NOT_FOUND, &format!("{e} (old releases' parts are cleaned up after {} newer ones)", 20)),
    }
}

fn list_releases(platform: &Platform, project: &str) -> Response<HyperOutgoingBody> {
    let current = crate::managed::current(&platform.data_dir, project);
    let mut history = crate::managed::history(&platform.data_dir, project);
    history.reverse();
    json(StatusCode::OK, serde_json::json!({ "project": project, "current": current, "releases": history }).to_string())
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
