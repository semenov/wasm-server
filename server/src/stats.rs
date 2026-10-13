//! Usage counters per account (an app or a static site), kept per worker so
//! the hot path never touches a cache line another core writes. Only the
//! owning worker thread writes its counters (plain load + store, no atomic
//! read-modify-write); readers (`/_platform/stats`, metrics) sum them across
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
    /// Time instances waited on outgoing HTTP requests.
    OutboundWaitNs,
    /// HTTP responses with a 4xx or 5xx status (the guest's, or the host's
    /// 500 and 504).
    Http4xx,
    Http5xx,
}

impl Counter {
    pub const ALL: [Counter; 27] = [
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
        Counter::OutboundWaitNs,
        Counter::Http4xx,
        Counter::Http5xx,
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
            Counter::OutboundWaitNs => "outbound_wait_ns",
            Counter::Http4xx => "http_4xx",
            Counter::Http5xx => "http_5xx",
        }
    }
}

const N: usize = Counter::ALL.len();

/// Upper bounds (µs) of the buckets of HTTP request times; one more bucket
/// holds everything slower. About 1.5× apart: percentiles read from them are
/// within half a bucket.
pub const LATENCY_BOUNDS_US: [u64; 30] = [
    100, 150, 200, 300, 500, 750, 1_000, 1_500, 2_000, 3_000, 5_000, 7_500, 10_000, 15_000, 20_000, 30_000, 50_000, 75_000, 100_000,
    150_000, 200_000, 300_000, 500_000, 750_000, 1_000_000, 1_500_000, 2_000_000, 3_000_000, 5_000_000, 10_000_000,
];
pub const LATENCY_BUCKETS: usize = LATENCY_BOUNDS_US.len() + 1;

#[repr(align(128))] // keep each account's counters on separate cache lines
pub struct AppCounters {
    values: [AtomicU64; N],
    mem_peak_max: AtomicU64,
    /// HTTP request times, by bucket of `LATENCY_BOUNDS_US`.
    latency: [AtomicU64; LATENCY_BUCKETS],
}

impl Default for AppCounters {
    fn default() -> Self {
        Self { values: std::array::from_fn(|_| AtomicU64::new(0)), mem_peak_max: AtomicU64::new(0), latency: std::array::from_fn(|_| AtomicU64::new(0)) }
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

    /// One HTTP request's time, for the percentiles.
    pub fn request_time(&self, ns: u64) {
        let us = ns / 1000;
        let i = LATENCY_BOUNDS_US.iter().position(|b| us <= *b).unwrap_or(LATENCY_BOUNDS_US.len());
        let c = &self.latency[i];
        c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
    }

    /// An HTTP response's status.
    pub fn status(&self, status: u16) {
        match status {
            400..=499 => self.add(Counter::Http4xx, 1),
            500..=599 => self.add(Counter::Http5xx, 1),
            _ => {}
        }
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
        self.add(Counter::OutboundWaitNs, u.outbound_wait_ns);
        if u.mem_peak > self.mem_peak_max.load(Relaxed) {
            self.mem_peak_max.store(u.mem_peak, Relaxed);
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct InstanceUsage {
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub mem_peak: u64,
    pub db_queries: u64,
    pub db_wait_ns: u64,
    pub outbound_requests: u64,
    pub outbound_wait_ns: u64,
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
        // a cluster primary counts its workers too
        if let Some((c, _)) = crate::cluster::extra_totals(account) {
            for (o, v) in out.iter_mut().zip(c) {
                *o = o.wrapping_add(v);
            }
        }
        out
    }

    /// This server's own totals, without a cluster's workers (what a worker
    /// reports).
    pub fn own_totals(&self, account: usize) -> [u64; N] {
        let mut out = [0u64; N];
        for w in &self.workers {
            for (i, v) in w[account].values.iter().enumerate() {
                out[i] = out[i].wrapping_add(v.load(Relaxed));
            }
        }
        out
    }

    /// HTTP request time buckets across workers for one account.
    pub fn latency(&self, account: usize) -> [u64; LATENCY_BUCKETS] {
        let mut out = [0u64; LATENCY_BUCKETS];
        for w in &self.workers {
            for (i, v) in w[account].latency.iter().enumerate() {
                out[i] = out[i].wrapping_add(v.load(Relaxed));
            }
        }
        if let Some((_, l)) = crate::cluster::extra_totals(account) {
            for (o, v) in out.iter_mut().zip(l) {
                *o = o.wrapping_add(v);
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
