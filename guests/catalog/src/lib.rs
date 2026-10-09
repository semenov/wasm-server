//! Product catalog API backed by Postgres through the host's `platform:db/sql`.
//!
//!   GET  /catalog/products?limit=20&offset=0   list products
//!   GET  /catalog/products/{id}                one product
//!   POST /catalog/orders?product={id}&qty={n}  order in a transaction:
//!                                              reserve stock, insert order
//!   GET  /catalog/export                       all products as NDJSON, read
//!                                              with a cursor and streamed
//!   GET  /catalog/export?buffered=1            the same with one `query` call
//!                                              (fails past 10,000 rows)
//!   GET  /catalog/slow?ms=N                    a query that sleeps N ms (for
//!                                              testing the database timeout)

use serde_json::{Value as Json, json};
use std::io::Write as _;
use wasip2::http::types::{Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam};

wit_bindgen::generate!({
    path: "../../wit",
    world: "platform:sdk/imports",
});

use platform::sdk::sql::{self, Cursor, Error as DbError, Transaction, Value};

const DB: &str = "main";
const EXPORT_SQL: &str = "SELECT id, name, price_cents, stock FROM products ORDER BY id";

struct Catalog;

impl wasip2::exports::http::incoming_handler::Guest for Catalog {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path_and_query = request.path_with_query().unwrap_or_default();
        let (path, query) = path_and_query.split_once('?').unwrap_or((&path_and_query, ""));
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

        let (status, body) = match (request.method(), segments.as_slice()) {
            (Method::Get, ["catalog", "products"]) => {
                let limit = param(query, "limit").unwrap_or(20).clamp(1, 100);
                let offset = param(query, "offset").unwrap_or(0).max(0);
                list_products(limit, offset)
            }
            (Method::Get, ["catalog", "products", id]) => match id.parse() {
                Ok(id) => get_product(id),
                Err(_) => (400, json!({"error": "product id must be an integer"})),
            },
            (Method::Get, ["catalog", "export"]) if param(query, "buffered") == Some(1) => {
                match sql::query(DB, EXPORT_SQL, &[]) {
                    Ok(r) => (200, Json::Array(r.rows.iter().map(|row| product_json(row)).collect())),
                    Err(e) => db_error(e),
                }
            }
            (Method::Get, ["catalog", "export"]) => return export(out),
            (Method::Get, ["catalog", "slow"]) => {
                let ms = param(query, "ms").unwrap_or(100);
                match sql::query(DB, "SELECT pg_sleep($1)::text", &[Value::Float(ms as f64 / 1000.0)]) {
                    Ok(_) => (200, json!({"slept_ms": ms})),
                    Err(e) => db_error(e),
                }
            }
            (Method::Post, ["catalog", "orders"]) => match (param(query, "product"), param(query, "qty")) {
                (Some(product), Some(qty)) if qty > 0 => place_order(product, qty),
                _ => (400, json!({"error": "need ?product=ID&qty=N with N > 0"})),
            },
            _ => (404, json!({"error": "not found"})),
        };
        respond(out, status, &body);
    }
}

fn list_products(limit: i64, offset: i64) -> (u16, Json) {
    let result = sql::query(
        DB,
        "SELECT id, name, price_cents, stock FROM products ORDER BY id LIMIT $1 OFFSET $2",
        &[Value::Int(limit), Value::Int(offset)],
    );
    match result {
        Ok(r) => (200, Json::Array(r.rows.iter().map(|row| product_json(row)).collect())),
        Err(e) => db_error(e),
    }
}

fn get_product(id: i64) -> (u16, Json) {
    let result = sql::query(DB, "SELECT id, name, price_cents, stock FROM products WHERE id = $1", &[Value::Int(id)]);
    match result {
        Ok(r) => match r.rows.first() {
            Some(row) => (200, product_json(row)),
            None => (404, json!({"error": "no such product"})),
        },
        Err(e) => db_error(e),
    }
}

fn place_order(product: i64, qty: i64) -> (u16, Json) {
    let order = || -> Result<Option<i64>, DbError> {
        let tx = Transaction::begin(DB)?;
        let reserved = tx.execute(
            "UPDATE products SET stock = stock - $2 WHERE id = $1 AND stock >= $2",
            &[Value::Int(product), Value::Int(qty)],
        )?;
        if reserved == 0 {
            // Dropping `tx` without commit rolls it back.
            return Ok(None);
        }
        let inserted = tx.query(
            "INSERT INTO orders (product_id, qty) VALUES ($1, $2) RETURNING id",
            &[Value::Int(product), Value::Int(qty)],
        )?;
        Transaction::commit(tx)?;
        Ok(inserted.rows.first().map(|row| int(&row[0])))
    };
    match order() {
        Ok(Some(order_id)) => (201, json!({"order_id": order_id, "product": product, "qty": qty})),
        Ok(None) => (409, json!({"error": "unknown product or not enough stock"})),
        Err(e) => db_error(e),
    }
}

/// Streams every product, one JSON object per line. Rows come from a cursor
/// in batches and each batch is written out before the next is fetched, so
/// memory use doesn't depend on the table size.
fn export(out: ResponseOutparam) {
    let cursor = match Cursor::open(DB, EXPORT_SQL, &[]) {
        Ok(cursor) => cursor,
        Err(e) => {
            let (status, body) = db_error(e);
            return respond(out, status, &body);
        }
    };
    let headers = Fields::from_list(&[("content-type".into(), b"application/x-ndjson".to_vec())]).unwrap();
    let resp = OutgoingResponse::new(headers);
    let body_handle = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let mut stream = body_handle.write().unwrap();
    let mut line = Vec::new();
    loop {
        match cursor.next(500) {
            Ok(rows) if rows.is_empty() => break,
            Ok(rows) => {
                line.clear();
                for row in &rows {
                    serde_json::to_writer(&mut line, &product_json(row)).unwrap();
                    line.push(b'\n');
                }
                if stream.write_all(&line).is_err() {
                    break; // client went away; dropping the cursor cancels the query
                }
            }
            Err(e) => {
                // Headers are already sent: report the error in-band.
                let _ = writeln!(stream, "{}", json!({"error": db_error(e).1["error"]}));
                break;
            }
        }
    }
    let _ = stream.flush();
    drop(stream);
    let _ = OutgoingBody::finish(body_handle, None);
}

fn product_json(row: &[Value]) -> Json {
    json!({
        "id": int(&row[0]),
        "name": match &row[1] { Value::Text(s) => s.as_str(), _ => "" },
        "price_cents": int(&row[2]),
        "stock": int(&row[3]),
    })
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn db_error(e: DbError) -> (u16, Json) {
    let (status, message) = match e {
        DbError::NotAllowed(m) => (403, m),
        DbError::Unavailable(m) => (503, m),
        DbError::Query(m) | DbError::Conversion(m) => (500, m),
        DbError::Timeout => (504, "database timeout".to_string()),
        DbError::TooManyRows(limit) => (500, format!("result exceeds {limit} rows")),
    };
    (status, json!({"error": message}))
}

fn param(query: &str, key: &str) -> Option<i64> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { v.parse().ok() } else { None }
    })
}

fn respond(out: ResponseOutparam, status: u16, body: &Json) {
    let headers = Fields::from_list(&[("content-type".into(), b"application/json".to_vec())]).unwrap();
    let resp = OutgoingResponse::new(headers);
    resp.set_status_code(status).unwrap();
    let body_handle = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));

    let mut stream = body_handle.write().unwrap();
    stream.write_all(&serde_json::to_vec(body).unwrap()).unwrap();
    stream.flush().unwrap();
    drop(stream);
    OutgoingBody::finish(body_handle, None).unwrap();
}

wasip2::http::proxy::export!(Catalog);
