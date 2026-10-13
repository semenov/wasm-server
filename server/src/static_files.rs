//! Static site files served by the host, without running wasm.
//!
//! A site's directory is loaded into memory at startup. Text files are also
//! stored gzip-compressed, every file gets an ETag, and caching headers follow
//! the file type, so serving a file is a hash lookup and a memcpy-free write.

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Response, StatusCode};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};
use std::io::Write as _;
use std::path::Path;
use wasmtime::{Result, bail, format_err};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Refuse sites larger than this; they belong in object storage and a CDN.
const MAX_SITE_BYTES: u64 = 256 << 20;

pub struct StaticSite {
    /// URL path ("/css/app.css") → file.
    files: HashMap<String, StaticFile>,
    pub total_bytes: u64,
    /// A single-page app: a GET for a path that isn't a file (and doesn't
    /// look like one) gets `/index.html`, for the app's router to handle.
    pub spa: bool,
}

struct StaticFile {
    body: Bytes,
    gzip: Option<Bytes>,
    content_type: HeaderValue,
    etag: HeaderValue,
    cache_control: HeaderValue,
}

impl StaticSite {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut site = StaticSite { files: HashMap::new(), total_bytes: 0, spa: false };
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        site.load_dir(dir, dir)?;
        if site.files.is_empty() {
            bail!("{} contains no files", dir.display());
        }
        Ok(site)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    fn load_dir(&mut self, root: &Path, dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir).map_err(|e| format_err!("{}: {e}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue; // .git, .DS_Store, ...
            }
            if entry.file_type()?.is_dir() {
                self.load_dir(root, &path)?;
                continue;
            }
            let data = std::fs::read(&path).map_err(|e| format_err!("{}: {e}", path.display()))?;
            self.total_bytes += data.len() as u64;
            if self.total_bytes > MAX_SITE_BYTES {
                bail!("{}: site exceeds {} MiB", root.display(), MAX_SITE_BYTES >> 20);
            }
            let relative = path.strip_prefix(root).unwrap();
            let url = format!("/{}", relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
            self.files.insert(url, StaticFile::new(&path, data));
        }
        Ok(())
    }

    /// Finds the file for a URL path: exact match, or `index.html` for a
    /// directory path.
    fn lookup(&self, path: &str) -> Option<&StaticFile> {
        let path = percent_decode(path);
        if path.ends_with('/') {
            return self.files.get(&format!("{path}index.html"));
        }
        self.files.get(&path).or_else(|| self.files.get(&format!("{path}/index.html")))
    }

    /// Serves `path` if it is a file of this site (GET and HEAD only).
    pub fn serve(&self, method: &Method, path: &str, headers: &HeaderMap) -> Option<Response<HyperOutgoingBody>> {
        let file = self.lookup(path)?;
        self.respond(method, file, headers)
    }

    /// For a single-page app, `/index.html` in place of a missing page:
    /// not for paths with an extension (`/logo.png` stays a 404).
    pub fn fallback(&self, method: &Method, path: &str, headers: &HeaderMap) -> Option<Response<HyperOutgoingBody>> {
        let last = path.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("");
        if !self.spa || last.contains('.') {
            return None;
        }
        self.respond(method, self.files.get("/index.html")?, headers)
    }

    fn respond(&self, method: &Method, file: &StaticFile, headers: &HeaderMap) -> Option<Response<HyperOutgoingBody>> {
        if method != Method::GET && method != Method::HEAD {
            return None;
        }
        let mut resp = Response::new(crate::server::body(Bytes::new()));
        let h = resp.headers_mut();
        h.insert(header::ETAG, file.etag.clone());
        h.insert(header::CACHE_CONTROL, file.cache_control.clone());
        h.insert(header::CONTENT_TYPE, file.content_type.clone());
        if file.gzip.is_some() {
            h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        }
        if headers.get(header::IF_NONE_MATCH).is_some_and(|tag| etag_matches(tag, &file.etag)) {
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            return Some(resp);
        }
        let accepts_gzip = headers
            .get_all(header::ACCEPT_ENCODING)
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip"))));
        let body = match &file.gzip {
            Some(gzip) if accepts_gzip => {
                resp.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                gzip.clone()
            }
            _ => file.body.clone(),
        };
        resp.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        if method == Method::GET {
            *resp.body_mut() = crate::server::body(body);
        }
        Some(resp)
    }
}

impl StaticFile {
    fn new(path: &Path, data: Vec<u8>) -> Self {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let essence = mime.essence_str().to_string();
        let textual = essence.starts_with("text/")
            || matches!(essence.as_str(), "application/javascript" | "application/json" | "application/xml" | "image/svg+xml" | "application/wasm");
        let content_type = if essence.starts_with("text/") || essence == "application/javascript" || essence == "application/json" {
            format!("{essence}; charset=utf-8")
        } else {
            essence.clone()
        };

        let mut hasher = DefaultHasher::new();
        hasher.write(&data);
        let etag = format!("\"{:016x}-{:x}\"", hasher.finish(), data.len());

        let gzip = textual.then(|| gzip(&data)).filter(|g| g.len() < data.len() * 9 / 10).map(Bytes::from);

        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let cache_control = if essence == "text/html" {
            // Revalidate pages every time (cheap with the ETag) so deploys show up at once.
            "no-cache"
        } else if has_content_hash(&file_name) {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=300"
        };

        StaticFile {
            body: Bytes::from(data),
            gzip,
            content_type: HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")),
            etag: HeaderValue::from_str(&etag).unwrap(),
            cache_control: HeaderValue::from_static(cache_control),
        }
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Build tools name assets like `app.3f9a2c1b.js` or `index-B4x8Kq2z.css`;
/// such files never change, so browsers may cache them forever.
fn has_content_hash(file_name: &str) -> bool {
    file_name.split(['.', '-', '_']).rev().skip(1).any(|part| {
        part.len() >= 8 && part.bytes().all(|b| b.is_ascii_alphanumeric()) && part.bytes().any(|b| b.is_ascii_digit())
    })
}

fn etag_matches(header: &HeaderValue, etag: &HeaderValue) -> bool {
    let Ok(header) = header.to_str() else { return false };
    let etag = etag.to_str().unwrap_or_default();
    header.split(',').any(|t| {
        let t = t.trim();
        t == "*" || t.trim_start_matches("W/") == etag
    })
}

fn percent_decode(s: &str) -> String {
    let path = s.split(['?', '#']).next().unwrap_or("");
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(hi << 4 | lo);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
