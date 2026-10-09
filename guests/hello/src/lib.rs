//! Minimal app: measures the server's per-request overhead.

use std::io::Write as _;
use wasip2::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};

/// Shown in responses, to see which version served a request.
const VERSION: &str = "v1";

struct Hello;

impl wasip2::exports::http::incoming_handler::Guest for Hello {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let headers = Fields::from_list(&[("content-type".into(), b"text/plain".to_vec())]).unwrap();
        let resp = OutgoingResponse::new(headers);
        let body = resp.body().unwrap();
        ResponseOutparam::set(response_out, Ok(resp));

        let mut out = body.write().unwrap();
        writeln!(out, "Hello from wasm {VERSION}! path={path}").unwrap();
        out.flush().unwrap();
        drop(out);
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasip2::http::proxy::export!(Hello);
