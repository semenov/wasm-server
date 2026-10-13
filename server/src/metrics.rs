//! Each app's history, a row a minute: requests, errors, request times (as
//! buckets, for percentiles), where the time went (CPU, database, outgoing
//! HTTP), and what else it used (static files, traffic, cache, queues,
//! WebSockets). In `<data-dir>/metrics.db`, kept 30 days; read with
//! `GET /_platform/metrics?app=NAME&range=1h|6h|24h|7d|30d` (charts,
//! `plumb stats`) and `GET /_platform/usage` (each project's totals).

use crate::platform::Platform;
use crate::stats::{Counter, LATENCY_BOUNDS_US, LATENCY_BUCKETS};
use rusqlite::{Connection, params};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasmtime::Result;

const KEEP_DAYS: i64 = 30;

/// The columns, in order: counters summed per minute.
const COLUMNS: [(&str, Counter); 21] = [
    ("requests", Counter::HttpRequests),
    ("events", Counter::Events),
    ("calls", Counter::Calls),
    ("errors", Counter::Errors),
    ("timeouts", Counter::Timeouts),
    ("http_4xx", Counter::Http4xx),
    ("http_5xx", Counter::Http5xx),
    ("wall_ns", Counter::GuestNs),
    ("cpu_ns", Counter::CpuNs),
    ("db_queries", Counter::DbQueries),
    ("db_wait_ns", Counter::DbWaitNs),
    ("outbound_requests", Counter::OutboundRequests),
    ("outbound_wait_ns", Counter::OutboundWaitNs),
    ("ws_events", Counter::WsEvents),
    ("static_requests", Counter::StaticRequests),
    ("cache_ops", Counter::CacheOps),
    ("queue_messages", Counter::QueueMessages),
    ("ws_messages_in", Counter::WsMessagesIn),
    ("ws_messages_out", Counter::WsMessagesOut),
    ("outbound_bytes_sent", Counter::OutboundBytesSent),
    ("outbound_bytes_received", Counter::OutboundBytesReceived),
];

pub struct Metrics {
    conn: Mutex<Connection>,
}

/// The server's metrics store; set once at startup.
pub static METRICS: std::sync::OnceLock<Arc<Metrics>> = std::sync::OnceLock::new();

impl Metrics {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        let cols: String = COLUMNS.iter().map(|(c, _)| format!("{c} INTEGER NOT NULL DEFAULT 0, ")).collect();
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS minutes (app TEXT NOT NULL, minute INTEGER NOT NULL, {cols}egress_bytes INTEGER NOT NULL DEFAULT 0, latency TEXT NOT NULL DEFAULT '[]', PRIMARY KEY (app, minute)) WITHOUT ROWID;"
        ))?;
        // columns added since the file was made
        let have: Vec<String> = conn.prepare("SELECT name FROM pragma_table_info('minutes')")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for (c, _) in COLUMNS {
            if !have.iter().any(|h| h == c) {
                conn.execute_batch(&format!("ALTER TABLE minutes ADD COLUMN {c} INTEGER NOT NULL DEFAULT 0"))?;
            }
        }
        Ok(Self { conn: Mutex::new(conn) })
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Starts the thread that writes a row per app every minute.
pub fn spawn(platform: Arc<Platform>, metrics: Arc<Metrics>) -> Result<()> {
    std::thread::Builder::new().name("metrics".into()).spawn(move || {
        let mut previous: Vec<([u64; Counter::ALL.len()], [u64; LATENCY_BUCKETS])> = Vec::new();
        let mut last_cleanup = 0i64;
        loop {
            // at the start of every minute
            let now = unix_now();
            std::thread::sleep(Duration::from_secs((60 - now % 60) as u64));
            let minute = (unix_now() / 60) * 60 - 60;
            let names = platform.accounts.names();
            // counters start at zero, with the server or the account
            while previous.len() < names.len() {
                previous.push(([0; Counter::ALL.len()], [0; LATENCY_BUCKETS]));
            }
            let mut conn = metrics.conn.lock().unwrap();
            let Ok(tx) = conn.transaction() else { continue };
            for (id, name) in names.iter().enumerate() {
                let (totals, latency) = (platform.stats.totals(id), platform.stats.latency(id));
                let before = std::mem::replace(&mut previous[id], (totals, latency));
                let d = |c: Counter| totals[c as usize].wrapping_sub(before.0[c as usize]) as i64;
                let lat: Vec<u64> = latency.iter().zip(before.1.iter()).map(|(a, b)| a.wrapping_sub(*b)).collect();
                if COLUMNS.iter().all(|(_, c)| d(*c) == 0) && d(Counter::EgressBytes) == 0 {
                    continue; // nothing happened
                }
                let values: Vec<i64> = COLUMNS.iter().map(|(_, c)| d(*c)).collect();
                let cols: String = COLUMNS.iter().map(|(c, _)| format!(", {c}")).collect();
                let marks: String = COLUMNS.iter().map(|_| ", ?").collect();
                let sql = format!("INSERT OR REPLACE INTO minutes (app, minute{cols}, egress_bytes, latency) VALUES (?, ?{marks}, ?, ?)");
                let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(name.clone()), Box::new(minute)];
                args.extend(values.into_iter().map(|v| Box::new(v) as Box<dyn rusqlite::ToSql>));
                args.push(Box::new(d(Counter::EgressBytes)));
                args.push(Box::new(format!("{lat:?}")));
                if let Err(e) = tx.execute(&sql, rusqlite::params_from_iter(args.iter().map(|b| b.as_ref()))) {
                    eprintln!("metrics: {e}");
                }
            }
            let _ = tx.commit();
            if minute - last_cleanup > 3600 {
                last_cleanup = minute;
                let _ = conn.execute("DELETE FROM minutes WHERE minute < ?", params![minute - KEEP_DAYS * 86400]);
            }
        }
    })?;
    Ok(())
}

/// The percentile `q` (0..1) of request times from summed buckets, in ms:
/// the bucket's upper bound (so "at most").
fn percentile(buckets: &[u64], q: f64) -> Option<f64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let want = (q * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in buckets.iter().enumerate() {
        seen += n;
        if seen >= want {
            let us = LATENCY_BOUNDS_US.get(i).copied().unwrap_or(LATENCY_BOUNDS_US[LATENCY_BOUNDS_US.len() - 1] * 2);
            return Some(us as f64 / 1000.0);
        }
    }
    None
}

#[derive(Default, Clone)]
struct Sums {
    v: [i64; COLUMNS.len()],
    egress: i64,
    latency: Vec<u64>,
}

impl Sums {
    fn add(&mut self, row: &Sums) {
        for i in 0..self.v.len() {
            self.v[i] += row.v[i];
        }
        self.egress += row.egress;
        if self.latency.len() < row.latency.len() {
            self.latency.resize(row.latency.len(), 0);
        }
        for (a, b) in self.latency.iter_mut().zip(&row.latency) {
            *a += b;
        }
    }

    fn get(&self, col: &str) -> i64 {
        COLUMNS.iter().position(|(c, _)| *c == col).map_or(0, |i| self.v[i])
    }

    /// The JSON of one point or of the whole range.
    fn json(&self, out: &mut String, seconds: i64) {
        let requests = self.get("requests");
        let instances = (requests + self.get("events") + self.get("calls")).max(1) as f64;
        let per = |col: &str| self.get(col) as f64 / instances / 1e6;
        let num = |v: Option<f64>| v.map_or("null".to_string(), |v| format!("{v:.3}"));
        let (total, cpu, db, http) = (per("wall_ns"), per("cpu_ns"), per("db_wait_ns"), per("outbound_wait_ns"));
        let _ = write!(
            out,
            "{{\"requests\":{requests},\"rps\":{:.3},\"errors\":{},\"http_4xx\":{},\"http_5xx\":{},\"timeouts\":{},\"events\":{},\"calls\":{},\
             \"p50_ms\":{},\"p95_ms\":{},\"p99_ms\":{},\"avg_ms\":{total:.3},\"cpu_ms\":{cpu:.3},\"db_ms\":{db:.3},\"http_ms\":{http:.3},\"other_ms\":{:.3},\
             \"db_queries\":{},\"outbound_requests\":{},\"egress_bytes\":{}}}",
            requests as f64 / seconds.max(1) as f64,
            self.get("errors") + self.get("timeouts") + self.get("http_5xx"),
            self.get("http_4xx"),
            self.get("http_5xx"),
            self.get("timeouts"),
            self.get("events"),
            self.get("calls"),
            num(percentile(&self.latency, 0.50)),
            num(percentile(&self.latency, 0.95)),
            num(percentile(&self.latency, 0.99)),
            (total - cpu - db - http).max(0.0),
            self.get("db_queries"),
            self.get("outbound_requests"),
            self.egress,
        );
    }
}

/// `range` → (seconds covered, seconds per point).
pub fn range_steps(range: &str) -> Option<(i64, i64)> {
    Some(match range {
        "1h" => (3600, 60),
        "6h" => (6 * 3600, 300),
        "24h" => (86400, 900),
        "7d" => (7 * 86400, 7200),
        "30d" => (30 * 86400, 6 * 3600),
        _ => return None,
    })
}

impl Metrics {
    /// `{"app", "range", "step", "summary": {...}, "points": [{"t", ...}]}`;
    /// points without activity are left out.
    /// An app's minutes; with `project`, those of all the project's apps
    /// (`shop`, `shop.api`, ...) added together.
    pub fn query(&self, app: &str, project: bool, range: &str) -> Option<String> {
        let (span, step) = range_steps(range)?;
        let now = unix_now();
        let from = (now - span) / step * step;
        let conn = self.conn.lock().unwrap();
        let cols: String = COLUMNS.iter().map(|(c, _)| format!("{c}, ")).collect();
        let mut stmt = conn.prepare(&format!("SELECT minute, {cols}egress_bytes, latency FROM minutes WHERE (app = ?1 OR (?3 AND app LIKE ?1 || '.%')) AND minute >= ?2 ORDER BY minute")).ok()?;
        let rows = stmt
            .query_map(params![app, from, project], |r| {
                let mut s = Sums::default();
                for i in 0..COLUMNS.len() {
                    s.v[i] = r.get(1 + i)?;
                }
                s.egress = r.get(1 + COLUMNS.len())?;
                let lat: String = r.get(2 + COLUMNS.len())?;
                s.latency = lat.trim_matches(|c| c == '[' || c == ']').split(',').filter_map(|x| x.trim().parse().ok()).collect();
                Ok((r.get::<_, i64>(0)?, s))
            })
            .ok()?;
        let mut points: Vec<(i64, Sums)> = Vec::new();
        let mut summary = Sums::default();
        for row in rows.flatten() {
            let (minute, s) = row;
            let t = minute / step * step;
            summary.add(&s);
            match points.last_mut() {
                Some((pt, sums)) if *pt == t => sums.add(&s),
                _ => points.push((t, s)),
            }
        }
        let mut out = format!("{{\"app\":{},\"range\":\"{range}\",\"step\":{step},\"from\":{from},\"to\":{now},\"summary\":", serde_json::Value::String(app.to_string()));
        summary.json(&mut out, span);
        out.push_str(",\"points\":[");
        for (i, (t, s)) in points.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let mut p = String::new();
            s.json(&mut p, step);
            let _ = write!(out, "{{\"t\":{t},{}", &p[1..]);
        }
        out.push_str("]}\n");
        Some(out)
    }
}

impl Metrics {
    /// Each app's totals over `range` (`24h`...): column → sum, and egress.
    pub fn totals(&self, range: &str) -> Option<Vec<(String, Vec<(&'static str, i64)>)>> {
        let (span, _) = range_steps(range)?;
        let from = unix_now() - span;
        let conn = self.conn.lock().unwrap();
        let sums: String = COLUMNS.iter().map(|(c, _)| format!("sum({c}), ")).collect();
        let mut stmt = conn.prepare(&format!("SELECT app, {sums}sum(egress_bytes) FROM minutes WHERE minute >= ? GROUP BY app")).ok()?;
        let rows = stmt
            .query_map(params![from], |r| {
                let mut v: Vec<(&'static str, i64)> = Vec::new();
                for (i, (c, _)) in COLUMNS.iter().enumerate() {
                    v.push((c, r.get(1 + i)?));
                }
                v.push(("egress_bytes", r.get(1 + COLUMNS.len())?));
                Ok((r.get::<_, String>(0)?, v))
            })
            .ok()?;
        Some(rows.flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut b = vec![0u64; LATENCY_BUCKETS];
        b[6] = 90; // ≤ 1 ms
        b[12] = 9; // ≤ 10 ms
        b[18] = 1; // ≤ 100 ms
        assert_eq!(percentile(&b, 0.5), Some(1.0));
        assert_eq!(percentile(&b, 0.95), Some(10.0));
        assert_eq!(percentile(&b, 0.99), Some(10.0));
        assert_eq!(percentile(&b, 1.0), Some(100.0));
        assert_eq!(percentile(&[0; 3], 0.5), None);
    }
}
