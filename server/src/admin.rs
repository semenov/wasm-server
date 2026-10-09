//! `/_platform/`: the server's own endpoints.
//!
//! | Path | Method | Who | What |
//! |---|---|---|---|
//! | `/_platform/` | GET | any | usage and billing dashboard |
//! | `billing`, `stats`, `jobs` | GET | any | JSON (a tenant sees its own apps) |
//! | `apps/NAME` | PUT | owner | upload a new component, then reload |
//! | `apps/NAME/versions` | GET | owner | saved previous versions |
//! | `apps/NAME/rollback[?to=ID]` | POST | owner | back to the previous (or given) version |
//! | `reload` | POST | admin | re-read the deployment and switch to it |
//! | `backups` | GET | admin | database snapshots |
//! | `databases/NAME/backup` | POST | admin | take a snapshot now |
//!
//! Callers: the admin (`--admin-token`), tenants (their token's SHA-256 is
//! in the deployment), or anonymous, which is allowed read-only only when
//! neither is configured (local development). A token goes in
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
use sha2::{Digest, Sha256};
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
    Tenant(String),
    /// No admin token or tenants configured: read-only.
    Anonymous,
}

impl Principal {
    /// Whether this caller may manage the app or site `name`.
    fn owns(&self, platform: &Platform, name: &str) -> bool {
        match self {
            Principal::Admin => true,
            Principal::Tenant(t) => platform.current().deployment.tenant_of(name) == Some(t.as_str()),
            Principal::Anonymous => false,
        }
    }

    /// Whether this caller may see the usage of account `name`.
    fn sees(&self, platform: &Platform, name: &str) -> bool {
        match self {
            Principal::Admin | Principal::Anonymous => true,
            tenant => tenant.owns(platform, name),
        }
    }
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
    let tenants = platform.current().deployment.tenants.clone();
    let (token, from_query) = match (bearer.or(cookie), query) {
        (Some(t), _) => (t, false),
        (None, Some(t)) => (t, true),
        (None, None) => {
            return (platform.admin_token.is_none() && tenants.is_empty()).then_some((Principal::Anonymous, None));
        }
    };
    let principal = if platform.admin_token.as_deref().is_some_and(|admin| same(admin.as_bytes(), token.as_bytes())) {
        Principal::Admin
    } else {
        let hash = versions::hex(&Sha256::digest(token.as_bytes()));
        let tenant = tenants.iter().find(|(_, t)| same(t.token_sha256.to_ascii_lowercase().as_bytes(), hash.as_bytes()))?;
        Principal::Tenant(tenant.0.clone())
    };
    Some((principal, from_query.then_some(token)))
}

pub async fn handle(platform: Arc<Platform>, req: Request<Incoming>, path: &str) -> Response<HyperOutgoingBody> {
    let Some((who, remember)) = authenticate(&platform, &req) else {
        return error(StatusCode::UNAUTHORIZED, "a valid admin or tenant token is required");
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
        (&Method::GET, ["billing"]) => {
            let current = platform.current();
            let tenant_of = |name: &str| current.deployment.tenant_of(name).map(str::to_string);
            let include = |name: &str| who.sees(&platform, name);
            json(StatusCode::OK, platform.billing.to_json(&platform.accounts.names(), &current.credits, &tenant_of, &include))
        }
        (&Method::GET, ["stats"]) => {
            let names = platform.accounts.names();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            json(StatusCode::OK, platform.stats.to_json(&names, &|name| who.sees(&platform, name)))
        }
        (&Method::GET, ["jobs"]) => json(StatusCode::OK, jobs_json(&platform, &who).await),
        (&Method::POST, ["reload"]) if who == Principal::Admin => reload(platform, Vec::new()).await,
        (&Method::POST, ["reload"]) => admin_only(),
        (_, ["apps", app, ..]) if !who.owns(&platform, app) => {
            error(StatusCode::FORBIDDEN, &format!("not allowed to manage {app:?}"))
        }
        (&Method::PUT, ["apps", app]) => deploy(platform, req, app.to_string()).await,
        (&Method::GET, ["apps", app, "versions"]) => list_versions(&platform, app),
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

async fn jobs_json(platform: &Platform, who: &Principal) -> String {
    let Some(store) = &platform.jobs else {
        return "{\"jobs\":[]}\n".to_string();
    };
    let rows: Vec<String> = store
        .counts()
        .await
        .iter()
        .filter(|c| who.sees(platform, &c.app))
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
