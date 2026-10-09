//! The C notes app (guests-c/notes) in Rust, for comparison: same routes,
//! same responses.
//!
//!   GET  /rnotes   latest notes and the view count (JSON)
//!   POST /rnotes   add a note; the body is its text

use serde_json::json;
use std::io::Write as _;
use wasip2::http::types::{Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam};
use wasip2::io::streams::StreamError;

wit_bindgen::generate!({ path: "../../wit", world: "platform:sdk/imports", generate_all });

use platform::sdk::cache;
use platform::sdk::sql::{self, Value};

const DB: &str = "notes";
const MAX_BODY: usize = 16 * 1024;

struct Notes;

impl wasip2::exports::http::incoming_handler::Guest for Notes {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let root = path == "/rnotes" || path == "/rnotes/" || path.starts_with("/rnotes?");
        let (status, body) = match (request.method(), root) {
            (Method::Get, true) => list(),
            (Method::Post, true) => add(&request),
            _ => (404, json!({"error": "not found"})),
        };
        respond(out, status, &serde_json::to_vec(&body).unwrap());
    }
}

fn list() -> (u16, serde_json::Value) {
    let views = cache::incr("views", 1, 0);
    match sql::query(DB, "SELECT id, text, at FROM notes ORDER BY id DESC LIMIT 20", &[]) {
        Ok(r) => {
            let notes: Vec<_> = r
                .rows
                .iter()
                .map(|row| match row.as_slice() {
                    [Value::Int(id), Value::Text(text), Value::Int(at)] => json!({"id": id, "text": text, "at": at}),
                    _ => json!(null),
                })
                .collect();
            (200, json!({"views": views, "notes": notes}))
        }
        Err(e) => (500, json!({"error": format!("{e:?}")})),
    }
}

fn add(request: &IncomingRequest) -> (u16, serde_json::Value) {
    let Some(text) = read_body(request) else {
        return (413, json!({"error": "note too long"}));
    };
    if text.is_empty() {
        return (422, json!({"error": "empty note"}));
    }
    let at = wasip2::clocks::wall_clock::now().seconds as i64;
    let params = [Value::Text(String::from_utf8_lossy(&text).into_owned()), Value::Int(at)];
    match sql::query(DB, "INSERT INTO notes (text, at) VALUES ($1, $2) RETURNING id", &params) {
        Ok(r) => match r.rows.first().map(|row| &row[0]) {
            Some(Value::Int(id)) => (201, json!({"id": id})),
            _ => (500, json!({"error": "no id"})),
        },
        Err(e) => (500, json!({"error": format!("{e:?}")})),
    }
}

fn read_body(request: &IncomingRequest) -> Option<Vec<u8>> {
    let body = request.consume().ok()?;
    let stream = body.stream().ok()?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) if bytes.len() + chunk.len() <= MAX_BODY => bytes.extend_from_slice(&chunk),
            Ok(_) => return None,
            Err(StreamError::Closed) => break,
            Err(_) => return None,
        }
    }
    drop(stream);
    IncomingBody::finish(body);
    Some(bytes)
}

fn respond(out: ResponseOutparam, status: u16, body: &[u8]) {
    let headers = Fields::from_list(&[("content-type".into(), b"application/json".to_vec())]).unwrap();
    let resp = OutgoingResponse::new(headers);
    resp.set_status_code(status).unwrap();
    let out_body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let mut stream = out_body.write().unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    drop(stream);
    OutgoingBody::finish(out_body, None).unwrap();
}

wasip2::http::proxy::export!(Notes);
