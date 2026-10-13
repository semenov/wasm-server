//! `platform:sdk/storage`: files of a deployed project (users' uploads),
//! kept on this server's disk and served by it without running the app.
//!
//! - In `<data-dir>/apps/PROJECT/`: `files.db` (key, size, type, public,
//!   time) and `files/ab/abcdef...` (each file under the SHA-256 of its key,
//!   so `a` and `a/b` can both be keys).
//! - `/_files/PROJECT/KEY` serves a public file to anyone; any other file
//!   needs `?exp=UNIX&sig=...`, signed by this server (`storage.key`).
//! - `PUT /_files/PROJECT/KEY?upload&exp=&max=&type=&public=&sig=` stores
//!   the body itself: an upload URL the app handed out, so a big file goes
//!   from the client to the disk without passing through the app.
//! - On a cluster worker, an app's calls go to the primary, which keeps the
//!   files; `/_files/` requests never leave the primary.

use crate::app::RequestState;
use crate::bindings::storage::{self as wit, Object};
use crate::platform::Platform;
use crate::server::body;
use base64::Engine as _;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use wasmtime::Result;
use wasmtime::component::{HasSelf, Linker};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Largest file an app stores with `put` (an upload URL sets its own limit).
const MAX_PUT: usize = 64 << 20;
/// Largest upload URL limit.
const MAX_UPLOAD: u64 = 1 << 30;
const URL_PREFIX: &str = "/_files/";

/// One project's files.
struct Store {
    dir: PathBuf,
    meta: Mutex<rusqlite::Connection>,
}

static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<Store>>>> = OnceLock::new();

fn store(data_dir: &Path, project: &str) -> Result<Arc<Store>, String> {
    if !crate::managed::valid_name(project) {
        return Err(format!("{project:?} isn't a project"));
    }
    let dir = crate::managed::root(data_dir).join(project);
    if !dir.is_dir() {
        return Err(format!("no project named {project:?}"));
    }
    let mut stores = STORES.get_or_init(Mutex::default).lock().unwrap();
    if let Some(s) = stores.get(&dir) {
        return Ok(s.clone());
    }
    let conn = rusqlite::Connection::open(dir.join("files.db")).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "wal").ok();
    conn.execute_batch("create table if not exists files (key text primary key, size integer not null, content_type text not null, public integer not null, modified_ms integer not null)").map_err(|e| e.to_string())?;
    let s = Arc::new(Store { dir: dir.join("files"), meta: Mutex::new(conn) });
    stores.insert(dir, s.clone());
    Ok(s)
}

/// `avatars/42.png`: parts of letters, digits, `.`, `_` and `-`.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 512
        && key.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'))
}

fn check_key(key: &str) -> Result<(), String> {
    if valid_key(key) { Ok(()) } else { Err(format!("{key:?} isn't a valid file key: parts of letters, digits, `.`, `_` and `-`, joined by `/`")) }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Store {
    fn path(&self, key: &str) -> PathBuf {
        let h = crate::versions::hex(&Sha256::digest(key.as_bytes()));
        self.dir.join(&h[..2]).join(h)
    }

    fn put(&self, key: &str, data: &[u8], content_type: &str, public: bool) -> Result<(), String> {
        check_key(key)?;
        let path = self.path(key);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = path.with_extension(format!("tmp{}", now_ms()));
        std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        let ct = if content_type.is_empty() { "application/octet-stream" } else { content_type };
        self.meta
            .lock()
            .unwrap()
            .execute(
                "insert into files (key, size, content_type, public, modified_ms) values (?1, ?2, ?3, ?4, ?5)
                 on conflict (key) do update set size = ?2, content_type = ?3, public = ?4, modified_ms = ?5",
                rusqlite::params![key, data.len() as i64, ct, public, now_ms() as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn info(&self, key: &str) -> Result<Option<Object>, String> {
        check_key(key)?;
        let conn = self.meta.lock().unwrap();
        let mut stmt = conn.prepare_cached("select key, size, content_type, public, modified_ms from files where key = ?").map_err(|e| e.to_string())?;
        let mut rows = stmt.query_map([key], row).map_err(|e| e.to_string())?;
        rows.next().transpose().map_err(|e| e.to_string())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        if self.info(key)?.is_none() {
            return Ok(None);
        }
        match std::fs::read(self.path(key)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn delete(&self, key: &str) -> Result<bool, String> {
        check_key(key)?;
        let n = self.meta.lock().unwrap().execute("delete from files where key = ?", [key]).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(self.path(key));
        Ok(n > 0)
    }

    fn list(&self, prefix: &str, after: &str, limit: u32) -> Result<Vec<Object>, String> {
        let conn = self.meta.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("select key, size, content_type, public, modified_ms from files where substr(key, 1, length(?1)) = ?1 and key > ?2 order by key limit ?3")
            .map_err(|e| e.to_string())?;
        let rows = stmt.query_map(rusqlite::params![prefix, after, limit.min(10_000)], row).map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Object> {
    Ok(Object { key: r.get(0)?, size: r.get::<_, i64>(1)? as u64, content_type: r.get(2)?, public: r.get(3)?, modified_ms: r.get::<_, i64>(4)? as u64 })
}

// ---- signed URLs

static KEY: OnceLock<Vec<u8>> = OnceLock::new();

/// This server's signing key, made on first use (`<data-dir>/storage.key`).
fn signing_key(data_dir: &Path) -> &'static [u8] {
    KEY.get_or_init(|| {
        let file = data_dir.join("storage.key");
        if let Ok(k) = std::fs::read(&file) {
            if k.len() == 32 {
                return k;
            }
        }
        let mut k = vec![0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut k).expect("random bytes");
        let tmp = file.with_extension("new");
        if std::fs::write(&tmp, &k).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&tmp, &file);
        }
        k
    })
}

fn sign(data_dir: &Path, parts: &[&str]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, signing_key(data_dir));
    let tag = ring::hmac::sign(&key, parts.join("\n").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref())
}

fn verify(data_dir: &Path, parts: &[&str], sig: &str) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, signing_key(data_dir));
    let Ok(tag) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig) else { return false };
    ring::hmac::verify(&key, parts.join("\n").as_bytes(), &tag).is_ok()
}

/// Content types go in a query string as they are, but for these.
fn encode_type(t: &str) -> String {
    t.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn decode_query(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(n) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn expiry(ttl_ms: u64) -> u64 {
    (now_ms() + ttl_ms.max(1000)).div_ceil(1000)
}

// ---- the calls, run here or (from a worker) on the primary

/// Runs one call for `project`; JSON in, JSON out, as it travels between
/// cluster nodes. Blocking (disk and SQLite).
pub fn run(data_dir: &Path, project: &str, call: &J) -> Result<J, String> {
    let s = store(data_dir, project)?;
    let str_arg = |k: &str| call.get(k).and_then(J::as_str).unwrap_or("").to_string();
    let key = str_arg("key");
    let object = |o: &Object| json!({ "key": o.key, "size": o.size, "content_type": o.content_type, "public": o.public, "modified_ms": o.modified_ms });
    match call.get("op").and_then(J::as_str).unwrap_or("") {
        "put" => {
            let data = base64::engine::general_purpose::STANDARD.decode(str_arg("data")).map_err(|e| e.to_string())?;
            s.put(&key, &data, &str_arg("content_type"), call.get("public").and_then(J::as_bool).unwrap_or(false))?;
            Ok(J::Null)
        }
        "get" => Ok(s.get(&key)?.map_or(J::Null, |d| J::String(base64::engine::general_purpose::STANDARD.encode(d)))),
        "info" => Ok(s.info(&key)?.as_ref().map_or(J::Null, object)),
        "delete" => Ok(J::Bool(s.delete(&key)?)),
        "list" => {
            let limit = call.get("limit").and_then(J::as_u64).unwrap_or(1000) as u32;
            Ok(J::Array(s.list(&str_arg("prefix"), &str_arg("after"), limit)?.iter().map(object).collect()))
        }
        "url" => {
            check_key(&key)?;
            let public = s.info(&key)?.is_some_and(|o| o.public);
            if public {
                return Ok(J::String(format!("{URL_PREFIX}{project}/{key}")));
            }
            let exp = expiry(call.get("ttl_ms").and_then(J::as_u64).unwrap_or(3_600_000)).to_string();
            let sig = sign(data_dir, &["get", project, &key, &exp]);
            Ok(J::String(format!("{URL_PREFIX}{project}/{key}?exp={exp}&sig={sig}")))
        }
        "upload_url" => {
            check_key(&key)?;
            let max = call.get("max_bytes").and_then(J::as_u64).unwrap_or(0);
            if max == 0 || max > MAX_UPLOAD {
                return Err(format!("max-bytes must be between 1 and {MAX_UPLOAD}"));
            }
            let (max, ct) = (max.to_string(), str_arg("content_type"));
            let public = if call.get("public").and_then(J::as_bool).unwrap_or(false) { "1" } else { "0" };
            let exp = expiry(call.get("ttl_ms").and_then(J::as_u64).unwrap_or(900_000)).to_string();
            let sig = sign(data_dir, &["put", project, &key, &exp, &max, &ct, public]);
            Ok(J::String(format!("{URL_PREFIX}{project}/{key}?upload&exp={exp}&max={max}&type={}&public={public}&sig={sig}", encode_type(&ct))))
        }
        op => Err(format!("unknown storage call {op:?}")),
    }
}

/// A file's content and details, for the project's owner (`plumb files
/// pull`), public or not.
pub fn read_file(data_dir: &Path, project: &str, key: &str) -> Result<Option<(Vec<u8>, Object)>, String> {
    let s = store(data_dir, project)?;
    let Some(info) = s.info(key)? else { return Ok(None) };
    Ok(s.get(key)?.map(|data| (data, info)))
}

impl RequestState {
    async fn storage_call(&mut self, call: J) -> Result<J, String> {
        let Some(project) = self.project.clone() else {
            return Err("file storage is for apps deployed as a project (plumb deploy)".into());
        };
        let started = std::time::Instant::now();
        let result = if crate::cluster::is_worker() {
            crate::cluster::remote_storage(&project, call).await
        } else {
            let data_dir = self.env.platform.data_dir.clone();
            tokio::task::spawn_blocking(move || run(&data_dir, &project, &call)).await.unwrap_or_else(|e| Err(e.to_string()))
        };
        // waiting on the disk isn't the app's CPU
        self.db.wait += started.elapsed();
        result
    }
}

fn object_from(j: &J) -> Option<Object> {
    Some(Object {
        key: j.get("key")?.as_str()?.to_string(),
        size: j.get("size")?.as_u64()?,
        content_type: j.get("content_type")?.as_str()?.to_string(),
        public: j.get("public")?.as_bool()?,
        modified_ms: j.get("modified_ms")?.as_u64()?,
    })
}

pub fn add_to_linker(linker: &mut Linker<RequestState>) -> Result<()> {
    wit::add_to_linker::<RequestState, HasSelf<RequestState>>(linker, |s| s)
}

impl wit::Host for RequestState {
    async fn put(&mut self, key: String, data: Vec<u8>, content_type: String, public: bool) -> Result<Result<(), String>> {
        if data.len() > MAX_PUT {
            return Ok(Err(format!("the file is {} bytes; put takes up to {MAX_PUT} (give the client an upload URL for bigger ones)", data.len())));
        }
        let call = json!({ "op": "put", "key": key, "data": base64::engine::general_purpose::STANDARD.encode(&data), "content_type": content_type, "public": public });
        Ok(self.storage_call(call).await.map(|_| ()))
    }

    async fn get(&mut self, key: String) -> Result<Result<Option<Vec<u8>>, String>> {
        Ok(self.storage_call(json!({ "op": "get", "key": key })).await.map(|j| j.as_str().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())))
    }

    async fn info(&mut self, key: String) -> Result<Result<Option<Object>, String>> {
        Ok(self.storage_call(json!({ "op": "info", "key": key })).await.map(|j| object_from(&j)))
    }

    async fn delete(&mut self, key: String) -> Result<Result<bool, String>> {
        Ok(self.storage_call(json!({ "op": "delete", "key": key })).await.map(|j| j.as_bool().unwrap_or(false)))
    }

    async fn list(&mut self, prefix: String, after: String, limit: u32) -> Result<Result<Vec<Object>, String>> {
        let r = self.storage_call(json!({ "op": "list", "prefix": prefix, "after": after, "limit": limit })).await;
        Ok(r.map(|j| j.as_array().map(|a| a.iter().filter_map(object_from).collect()).unwrap_or_default()))
    }

    async fn url(&mut self, key: String, ttl_ms: u64) -> Result<Result<String, String>> {
        Ok(self.storage_call(json!({ "op": "url", "key": key, "ttl_ms": ttl_ms })).await.map(|j| j.as_str().unwrap_or("").to_string()))
    }

    async fn upload_url(&mut self, key: String, max_bytes: u64, content_type: String, public: bool, ttl_ms: u64) -> Result<Result<String, String>> {
        let call = json!({ "op": "upload_url", "key": key, "max_bytes": max_bytes, "content_type": content_type, "public": public, "ttl_ms": ttl_ms });
        Ok(self.storage_call(call).await.map(|j| j.as_str().unwrap_or("").to_string()))
    }
}

// ---- serving /_files/

pub fn is_files_path(path: &str) -> bool {
    path.starts_with(URL_PREFIX)
}

fn text(status: StatusCode, msg: &str) -> Response<HyperOutgoingBody> {
    let mut r = Response::new(body(format!("{msg}\n")));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn cors(r: &mut Response<HyperOutgoingBody>) {
    // the signature decides who may upload, not the page's origin
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, HEAD, PUT"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
}

/// A request for `/_files/PROJECT/KEY`: a file, or an upload.
pub async fn serve(platform: Arc<Platform>, req: Request<Incoming>) -> Response<HyperOutgoingBody> {
    let path = req.uri().path()[URL_PREFIX.len()..].to_string();
    let Some((project, key)) = path.split_once('/').map(|(p, k)| (p.to_string(), k.to_string())) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    if !valid_key(&key) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let query: HashMap<String, String> = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| kv.split_once('=').map_or((kv.to_string(), String::new()), |(k, v)| (k.to_string(), decode_query(v))))
        .collect();
    let q = |k: &str| query.get(k).map(String::as_str).unwrap_or("");
    let data_dir = platform.data_dir.clone();
    let store = match store(&data_dir, &project) {
        Ok(s) => s,
        Err(_) => return text(StatusCode::NOT_FOUND, "not found"),
    };
    let now_s = now_ms() / 1000;
    let method = req.method().clone();
    if method == Method::OPTIONS {
        let mut r = text(StatusCode::NO_CONTENT, "");
        cors(&mut r);
        return r;
    }
    if method == Method::PUT || method == Method::POST {
        if !query.contains_key("upload") {
            return text(StatusCode::METHOD_NOT_ALLOWED, "not an upload URL");
        }
        let exp: u64 = q("exp").parse().unwrap_or(0);
        if !verify(&data_dir, &["put", &project, &key, q("exp"), q("max"), q("type"), q("public")], q("sig")) {
            return text(StatusCode::FORBIDDEN, "this upload URL isn't valid");
        }
        if exp < now_s {
            return text(StatusCode::FORBIDDEN, "this upload URL has expired");
        }
        let max: usize = q("max").parse().unwrap_or(0);
        let given = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let content_type = if q("type").is_empty() { given } else { q("type").to_string() };
        let public = q("public") == "1";
        let bytes = match Limited::new(req.into_body(), max).collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, &format!("the file is larger than this URL allows ({max} bytes)")),
        };
        let size = bytes.len();
        let k = key.clone();
        let stored = tokio::task::spawn_blocking(move || store.put(&k, &bytes, &content_type, public)).await.unwrap_or_else(|e| Err(e.to_string()));
        let mut r = match stored {
            Ok(()) => {
                let mut r = Response::new(body(json!({ "key": key, "size": size }).to_string()));
                *r.status_mut() = StatusCode::CREATED;
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                r
            }
            Err(e) => {
                eprintln!("storage: {project}/{key}: {e}");
                text(StatusCode::INTERNAL_SERVER_ERROR, "the file couldn't be stored")
            }
        };
        cors(&mut r);
        return r;
    }
    if method != Method::GET && method != Method::HEAD {
        return text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let (s2, k) = (store.clone(), key.clone());
    let info = match tokio::task::spawn_blocking(move || s2.info(&k)).await {
        Ok(Ok(Some(o))) => o,
        _ => return text(StatusCode::NOT_FOUND, "not found"),
    };
    let mut max_age = 3600;
    if !info.public {
        let exp: u64 = q("exp").parse().unwrap_or(0);
        if !verify(&data_dir, &["get", &project, &key, q("exp")], q("sig")) || exp < now_s {
            return text(StatusCode::FORBIDDEN, "this link isn't valid, or has expired");
        }
        max_age = (exp - now_s).min(3600);
    }
    let etag = format!("\"{:x}-{:x}\"", info.size, info.modified_ms);
    let mut r = Response::new(body(""));
    let h = r.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    let cache = if info.public { format!("public, max-age={max_age}") } else { format!("private, max-age={max_age}") };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_str(&cache).unwrap());
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&info.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    // an uploaded page can't run scripts on the app's domain
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'; sandbox"));
    cors(&mut r);
    if req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        return r;
    }
    r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(info.size));
    if method == Method::GET {
        let path = store.path(&key);
        match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
            Ok(Ok(data)) => *r.body_mut() = body(data),
            _ => return text(StatusCode::NOT_FOUND, "not found"),
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert!(valid_key("avatars/42.png") && valid_key("a") && valid_key("x/y-z_1.tar.gz"));
        assert!(!valid_key("") && !valid_key("/a") && !valid_key("a/") && !valid_key("a//b") && !valid_key("../a") && !valid_key("a b") && !valid_key("a?b"));
    }

    #[test]
    fn query_values() {
        assert_eq!(decode_query(&encode_type("image/svg+xml; charset=utf-8")), "image/svg+xml; charset=utf-8");
        assert_eq!(decode_query("a%2"), "a%2");
    }
}
