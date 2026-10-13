//! Each app's log: what its instances write to standard output and error,
//! and what the host notes about it (failed and slow requests with where
//! their time went, refused outgoing requests). In `<data-dir>/logs/APP.log`,
//! rotated at 10 MB to `APP.log.1`; every line also goes to the server's
//! standard error as `APP: line`, so the system's journal (and `plumb dev`)
//! shows it. Read with `GET /_platform/apps/APP/logs` (`plumb logs`).

use crate::platform::Platform;
use crate::stats::InstanceUsage;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `<data-dir>/logs`; set once at startup.
pub static DIR: OnceLock<PathBuf> = OnceLock::new();
const ROTATE_AT: u64 = 10 << 20;
/// A request slower than this gets a line in its app's log.
pub const SLOW: Duration = Duration::from_secs(1);

static FILES: OnceLock<Mutex<HashMap<String, File>>> = OnceLock::new();

fn path(app: &str) -> Option<PathBuf> {
    Some(DIR.get()?.join(format!("{app}.log")))
}

/// Whether a line starts with its own time (`2026-10-13T...`), as Plumb's
/// `log` writes it.
fn timestamped(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() > 11 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[7] == b'-' && b[10] == b'T'
}

/// Appends `text` to the app's log, a line at a time; lines without a time
/// of their own get one.
pub fn write(app: &str, text: &str) {
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return;
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if timestamped(line) {
            out.push_str(line);
        } else {
            out.push_str(&now);
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
        eprintln!("{app}: {line}");
    }
    crate::cluster::forward_log(app, text);
    let Some(file_path) = path(app) else { return };
    let files = FILES.get_or_init(Mutex::default);
    let mut files = files.lock().unwrap();
    if !files.contains_key(app) {
        if let Some(dir) = file_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&file_path) {
            Ok(f) => {
                files.insert(app.to_string(), f);
            }
            Err(e) => {
                eprintln!("{app}: can't write its log {}: {e}", file_path.display());
                return;
            }
        }
    }
    let f = files.get_mut(app).unwrap();
    let _ = f.write_all(out.as_bytes());
    if f.metadata().is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = std::fs::rename(&file_path, file_path.with_extension("log.1"));
        files.remove(app);
    }
}

/// The last `lines` lines of the app's log.
pub fn tail(app: &str, lines: usize) -> String {
    let Some(p) = path(app) else { return String::new() };
    let mut text = std::fs::read_to_string(p.with_extension("log.1")).unwrap_or_default();
    text += &std::fs::read_to_string(&p).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let mut out = all[all.len().saturating_sub(lines)..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// What was written after byte `offset` of the current file, and the new
/// offset (for following the log); a rotation starts it over.
pub fn since(app: &str, offset: u64) -> (String, u64) {
    let Some(p) = path(app) else { return (String::new(), 0) };
    let Ok(mut f) = File::open(&p) else { return (String::new(), 0) };
    let len = f.metadata().map_or(0, |m| m.len());
    let from = if offset > len { 0 } else { offset };
    let _ = f.seek(SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.take(1 << 20).read_to_end(&mut buf);
    // whole lines only
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    buf.truncate(end);
    (String::from_utf8_lossy(&buf).into_owned(), from + end as u64)
}

fn ms(ns: u64) -> String {
    let ms = ns as f64 / 1e6;
    if ms >= 1000.0 { format!("{:.2} s", ms / 1000.0) } else if ms >= 10.0 { format!("{ms:.0} ms") } else { format!("{ms:.1} ms") }
}

/// One HTTP request to an app: the server notes its status, the instance
/// its resource use; whichever finishes last (the `Drop`) records the time
/// and, for a failed or slow request, writes a line to the app's log.
pub struct RequestLog {
    pub platform: Arc<Platform>,
    pub worker: usize,
    pub app_id: usize,
    pub app: String,
    pub method: String,
    pub path: String,
    pub started: Instant,
    pub status: Mutex<Option<u16>>,
    pub usage: Mutex<Option<InstanceUsage>>,
    pub error: Mutex<Option<String>>,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        let total = self.started.elapsed();
        let counters = self.platform.stats.counters(self.worker, self.app_id);
        counters.request_time(total.as_nanos() as u64);
        let status = self.status.lock().unwrap().unwrap_or(0);
        counters.status(status);
        let error = self.error.lock().unwrap().take();
        if status < 500 && total < SLOW && error.is_none() {
            return;
        }
        let mut line = format!("{} {} → {} in {}", self.method, self.path, status, ms(total.as_nanos() as u64));
        if let Some(u) = *self.usage.lock().unwrap() {
            line += &format!(" (cpu {}", ms(u.cpu_ns));
            if u.db_queries > 0 {
                line += &format!(", db {} quer{} {}", u.db_queries, if u.db_queries == 1 { "y" } else { "ies" }, ms(u.db_wait_ns));
            }
            if u.outbound_requests > 0 {
                line += &format!(", http {} request{} {}", u.outbound_requests, if u.outbound_requests == 1 { "" } else { "s" }, ms(u.outbound_wait_ns));
            }
            line.push(')');
        }
        if let Some(e) = error {
            line += &format!(": {e}");
        } else if status < 500 {
            line += ": slow";
        }
        write(&self.app, &line);
    }
}
