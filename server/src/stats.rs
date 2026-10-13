//! Usage counters per account (an app or a static site), kept per worker so
//! the hot path never touches a cache line another core writes. Only the
//! owning worker thread writes its counters (plain load + store, no atomic
//! read-modify-write); readers (`/_platform/stats`, billing) sum them across
//! workers.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Everything counted per account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Counter {
    /// All invocations of the app (any kind below).
    Requests,
    Errors,
    Timeouts,
    HttpRequests,
    /// Queue messages and cron ticks delivered.
    Events,
    /// Typed calls received from other functions.
    Calls,
    /// WebSocket connect, message and close events handled.
    WsEvents,
    /// Static files served by the host.
    StaticRequests,
    /// Wall time of instances.
    GuestNs,
    /// CPU time of instances (excluding waits).
    CpuNs,
    /// Peak linear memory × instance lifetime.
    MemKibMs,
    MemPeakSum,
    DbQueries,
    DbWaitNs,
    /// Response bytes: HTTP bodies, static files, WebSocket messages sent.
    EgressBytes,
    CacheOps,
    CacheBytesWritten,
    QueueMessages,
    QueueBytes,
    WsMessagesIn,
    WsMessagesOut,
    /// Outgoing HTTP requests (`wasi:http/outgoing-handler`).
    OutboundRequests,
    OutboundBytesSent,
    OutboundBytesReceived,
}

impl Counter {
    pub const ALL: [Counter; 24] = [
        Counter::Requests,
        Counter::Errors,
        Counter::Timeouts,
        Counter::HttpRequests,
        Counter::Events,
        Counter::Calls,
        Counter::WsEvents,
        Counter::StaticRequests,
        Counter::GuestNs,
        Counter::CpuNs,
        Counter::MemKibMs,
        Counter::MemPeakSum,
        Counter::DbQueries,
        Counter::DbWaitNs,
        Counter::EgressBytes,
        Counter::CacheOps,
        Counter::CacheBytesWritten,
        Counter::QueueMessages,
        Counter::QueueBytes,
        Counter::WsMessagesIn,
        Counter::WsMessagesOut,
        Counter::OutboundRequests,
        Counter::OutboundBytesSent,
        Counter::OutboundBytesReceived,
    ];

    /// Key in `/_platform/stats`.
    pub fn key(self) -> &'static str {
        match self {
            Counter::Requests => "requests",
            Counter::Errors => "errors",
            Counter::Timeouts => "timeouts",
            Counter::HttpRequests => "http_requests",
            Counter::Events => "events",
            Counter::Calls => "calls",
            Counter::WsEvents => "ws_events",
            Counter::StaticRequests => "static_requests",
            Counter::GuestNs => "guest_ns",
            Counter::CpuNs => "cpu_ns",
            Counter::MemKibMs => "mem_kib_ms",
            Counter::MemPeakSum => "mem_peak_sum_bytes",
            Counter::DbQueries => "db_queries",
            Counter::DbWaitNs => "db_wait_ns",
            Counter::EgressBytes => "egress_bytes",
            Counter::CacheOps => "cache_ops",
            Counter::CacheBytesWritten => "cache_bytes_written",
            Counter::QueueMessages => "queue_messages",
            Counter::QueueBytes => "queue_bytes",
            Counter::WsMessagesIn => "ws_messages_in",
            Counter::WsMessagesOut => "ws_messages_out",
            Counter::OutboundRequests => "outbound_requests",
            Counter::OutboundBytesSent => "outbound_bytes_sent",
            Counter::OutboundBytesReceived => "outbound_bytes_received",
        }
    }
}

const N: usize = Counter::ALL.len();

#[repr(align(128))] // keep each account's counters on separate cache lines
pub struct AppCounters {
    values: [AtomicU64; N],
    mem_peak_max: AtomicU64,
}

impl Default for AppCounters {
    fn default() -> Self {
        Self { values: std::array::from_fn(|_| AtomicU64::new(0)), mem_peak_max: AtomicU64::new(0) }
    }
}

impl AppCounters {
    pub fn add(&self, counter: Counter, n: u64) {
        let c = &self.values[counter as usize];
        c.store(c.load(Relaxed).wrapping_add(n), Relaxed);
    }

    pub fn request(&self) {
        self.add(Counter::Requests, 1);
    }
    pub fn error(&self) {
        self.add(Counter::Errors, 1);
    }
    pub fn timeout(&self) {
        self.add(Counter::Timeouts, 1);
    }

    /// Resource use of one finished instance.
    pub fn instance_done(&self, u: &InstanceUsage) {
        self.add(Counter::GuestNs, u.wall_ns);
        self.add(Counter::CpuNs, u.cpu_ns);
        self.add(Counter::MemKibMs, (u.mem_peak >> 10) * u.wall_ns / 1_000_000);
        self.add(Counter::MemPeakSum, u.mem_peak);
        self.add(Counter::DbQueries, u.db_queries);
        self.add(Counter::DbWaitNs, u.db_wait_ns);
        if u.mem_peak > self.mem_peak_max.load(Relaxed) {
            self.mem_peak_max.store(u.mem_peak, Relaxed);
        }
    }
}

pub struct InstanceUsage {
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub mem_peak: u64,
    pub db_queries: u64,
    pub db_wait_ns: u64,
}

/// `workers[w][account_id]`
pub struct Stats {
    workers: Vec<Vec<AppCounters>>,
    accounts: usize,
}

impl Stats {
    pub fn new(workers: usize, accounts: usize) -> Self {
        Self { workers: (0..workers).map(|_| (0..accounts).map(|_| AppCounters::default()).collect()).collect(), accounts }
    }

    pub fn counters(&self, worker: usize, account: usize) -> &AppCounters {
        &self.workers[worker][account]
    }

    /// Totals across workers for one account.
    pub fn totals(&self, account: usize) -> [u64; N] {
        let mut out = [0u64; N];
        for w in &self.workers {
            for (i, v) in w[account].values.iter().enumerate() {
                out[i] = out[i].wrapping_add(v.load(Relaxed));
            }
        }
        out
    }

    /// `names` are account names by id; only those `include` lets through
    /// are listed.
    pub fn to_json(&self, names: &[&str], include: &dyn Fn(&str) -> bool) -> String {
        let mut out = String::from("{\"apps\":{");
        let mut first = true;
        for (id, name) in names.iter().enumerate().take(self.accounts) {
            if !include(name) {
                continue;
            }
            if !first {
                out.push(',');
            }
            first = false;
            let totals = self.totals(id);
            let _ = write!(out, "\"{name}\":{{");
            for (i, counter) in Counter::ALL.iter().enumerate() {
                let _ = write!(out, "\"{}\":{},", counter.key(), totals[i]);
            }
            let max = self.workers.iter().map(|w| w[id].mem_peak_max.load(Relaxed)).max().unwrap_or(0);
            let _ = write!(out, "\"mem_peak_max_bytes\":{max}}}");
        }
        out.push_str("}}\n");
        out
    }
}
