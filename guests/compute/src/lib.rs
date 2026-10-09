//! CPU- and memory-bound app for exercising the server's limits:
//!   /compute?fib=30     naive Fibonacci (fib=45 runs past the timeout)
//!   /compute?alloc=128  allocate and touch N MiB (past the memory limit traps)

use std::io::Write as _;
use wasip2::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};

struct Compute;

fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn query_param(path: &str, key: &str) -> Option<u64> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { v.parse().ok() } else { None }
    })
}

impl wasip2::exports::http::incoming_handler::Guest for Compute {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let message = if let Some(n) = query_param(&path, "fib") {
            format!("fib({n}) = {}\n", fib(n))
        } else if let Some(mb) = query_param(&path, "alloc") {
            let buf = vec![1u8; (mb as usize) << 20];
            format!("allocated {} MiB, checksum {}\n", mb, buf.iter().step_by(4096).map(|&b| b as u64).sum::<u64>())
        } else {
            "usage: ?fib=N or ?alloc=MiB\n".to_string()
        };

        let resp = OutgoingResponse::new(Fields::new());
        let body = resp.body().unwrap();
        ResponseOutparam::set(response_out, Ok(resp));
        let mut out = body.write().unwrap();
        out.write_all(message.as_bytes()).unwrap();
        out.flush().unwrap();
        drop(out);
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasip2::http::proxy::export!(Compute);
