//! Link shortener: the HTTP side of the showcase.
//!
//!   POST /api/links {"url": "..."}   create a short link
//!   GET  /r/{code}                  redirect (cached), and queue a click
//!   GET  /api/links/{code}          the link and its clicks (typed call to `stats`)
//!   GET  /api/top                   most clicked links (typed call to `stats`)
//!
//! Links live in this app's own SQLite database. Clicks go to the `clicks`
//! queue so a redirect never waits for statistics; the `stats` function
//! consumes them.

use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::io::Write as _;
use wasip2::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use wasip2::io::streams::StreamError;

wit_bindgen::generate!({
    path: "wit",
    world: "demo:shortener/app",
    generate_all,
});

use demo::stats::api as stats;
use platform::sdk::sql::{self, Value};
use platform::sdk::{cache, queue};

const DB: &str = "links";
const LINK_TTL_MS: u64 = 3600 * 1000;
const CODE_LEN: usize = 6;

struct Shortener;

impl wasip2::exports::http::incoming_handler::Guest for Shortener {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let path = path.split('?').next().unwrap_or("").to_string();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let response = match (request.method(), segments.as_slice()) {
            (Method::Post, ["api", "links"]) => create(&request),
            (Method::Get, ["r", code]) => redirect(code),
            (Method::Get, ["api", "links", code]) => info(code),
            (Method::Get, ["api", "top"]) => top(),
            _ => Response::json(404, json!({"error": "not found"})),
        };
        response.send(out);
    }
}

#[derive(Deserialize)]
struct NewLink {
    url: String,
}

fn create(request: &IncomingRequest) -> Response {
    let Some(body) = read_body(request) else {
        return Response::json(400, json!({"error": "unreadable body"}));
    };
    let Ok(NewLink { url }) = serde_json::from_slice(&body) else {
        return Response::json(422, json!({"error": "expected {\"url\": \"https://...\"}"}));
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.len() > 2048 {
        return Response::json(422, json!({"error": "url must be http(s) and at most 2048 characters"}));
    }
    // Random codes rarely collide; try again if one does.
    for _ in 0..5 {
        let code = random_code();
        match sql::execute(
            DB,
            "INSERT INTO links (code, url, created_at) VALUES ($1, $2, $3) ON CONFLICT (code) DO NOTHING",
            &[Value::Text(code.clone()), Value::Text(url.clone()), Value::Int(now_ms())],
        ) {
            Ok(1) => return Response::json(201, json!({"code": code, "url": url, "short": format!("/r/{code}")})),
            Ok(_) => continue,
            Err(e) => return Response::json(500, json!({"error": format!("{e:?}")})),
        }
    }
    Response::json(500, json!({"error": "could not allocate a code"}))
}

/// The link's target, from the cache or the database.
fn resolve(code: &str) -> Result<Option<String>, sql::Error> {
    let key = format!("link:{code}");
    if let Some(url) = cache::get(&key).and_then(|b| String::from_utf8(b).ok()) {
        return Ok(Some(url));
    }
    let r = sql::query(DB, "SELECT url FROM links WHERE code = $1", &[Value::Text(code.into())])?;
    let url = r.rows.first().and_then(|row| match &row[0] {
        Value::Text(url) => Some(url.clone()),
        _ => None,
    });
    if let Some(url) = &url {
        let _ = cache::set(&key, url.as_bytes(), LINK_TTL_MS);
    }
    Ok(url)
}

fn redirect(code: &str) -> Response {
    match resolve(code) {
        Ok(Some(url)) => {
            // Counted asynchronously by `stats`; a lost click is better than
            // a failed redirect, so errors are only logged.
            if let Err(e) = queue::send("clicks", code.as_bytes(), 0) {
                eprintln!("shortener: couldn't queue click for {code}: {e:?}");
            }
            Response { status: 302, body: Vec::new(), headers: vec![("location", url.into_bytes()), ("cache-control", b"no-store".to_vec())] }
        }
        Ok(None) => Response::json(404, json!({"error": "no such link"})),
        Err(e) => Response::json(500, json!({"error": format!("{e:?}")})),
    }
}

fn info(code: &str) -> Response {
    match resolve(code) {
        Ok(Some(url)) => {
            let s = stats::summary(code);
            Response::json(200, json!({"code": code, "url": url, "clicks": s.clicks, "last_click_ms": s.last_click_ms}))
        }
        Ok(None) => Response::json(404, json!({"error": "no such link"})),
        Err(e) => Response::json(500, json!({"error": format!("{e:?}")})),
    }
}

fn top() -> Response {
    let links: Vec<Json> = stats::top(10)
        .into_iter()
        .map(|s| {
            let url = resolve(&s.code).ok().flatten();
            json!({"code": s.code, "url": url, "clicks": s.clicks, "last_click_ms": s.last_click_ms})
        })
        .collect();
    Response::json(200, json!({"links": links}))
}

struct Response {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, Vec<u8>)>,
}

impl Response {
    fn json(status: u16, body: Json) -> Self {
        Self { status, body: serde_json::to_vec(&body).unwrap(), headers: vec![("content-type", b"application/json".to_vec())] }
    }

    fn send(self, out: ResponseOutparam) {
        let headers: Vec<(String, Vec<u8>)> = self.headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let resp = OutgoingResponse::new(Fields::from_list(&headers).unwrap());
        resp.set_status_code(self.status).unwrap();
        let body = resp.body().unwrap();
        ResponseOutparam::set(out, Ok(resp));
        let mut stream = body.write().unwrap();
        stream.write_all(&self.body).unwrap();
        stream.flush().unwrap();
        drop(stream);
        OutgoingBody::finish(body, None).unwrap();
    }
}

fn read_body(request: &IncomingRequest) -> Option<Vec<u8>> {
    let body = request.consume().ok()?;
    let stream = body.stream().ok()?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) if bytes.len() + chunk.len() <= 64 * 1024 => bytes.extend_from_slice(&chunk),
            Ok(_) => return None,
            Err(StreamError::Closed) => break,
            Err(_) => return None,
        }
    }
    drop(stream);
    IncomingBody::finish(body);
    Some(bytes)
}

fn random_code() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    wasip2::random::random::get_random_bytes(CODE_LEN as u64)
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

fn now_ms() -> i64 {
    let now = wasip2::clocks::wall_clock::now();
    now.seconds as i64 * 1000 + (now.nanoseconds / 1_000_000) as i64
}

wasip2::http::proxy::export!(Shortener);
