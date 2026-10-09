//! Billing in credits, with a breakdown of what they were spent on.
//!
//! Usage counters (`stats.rs`) are turned into metered quantities once a
//! second: invocations, CPU time, memory × time, egress, database calls,
//! time and storage, cache operations and memory, queue messages, WebSocket
//! messages and connection-minutes. Each meter has a price in credits per
//! unit (`--pricing FILE` overrides the defaults; 1 credit ≈ $0.001 at the
//! defaults). Every minute's usage is stored in `<data-dir>/billing.db`, so
//! totals survive restarts; usage is written every 10 seconds and on
//! shutdown. An app with a credit grant (`--credits APP=N`)
//! is blocked when it runs out: HTTP gets 402, WebSockets are refused, queue
//! messages wait.

use crate::loader::MAX_ACCOUNTS;
use crate::platform::Platform;
use crate::stats::Counter;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime::{Result, format_err};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Meter {
    Invocations,
    StaticRequests,
    Cpu,
    Memory,
    Egress,
    DbCalls,
    DbTime,
    DbStorage,
    CacheOps,
    CacheStorage,
    QueueMessages,
    WsMessages,
    WsConnectionMinutes,
}

pub struct MeterInfo {
    pub meter: Meter,
    pub key: &'static str,
    pub category: &'static str,
    pub unit: &'static str,
    /// Credits per unit at the defaults.
    pub price: f64,
    pub description: &'static str,
}

/// The default rate card. 1 credit ≈ $0.001; prices are in the range of
/// public cloud pricing for the same resource.
pub const METERS: [MeterInfo; 13] = [
    MeterInfo { meter: Meter::Invocations, key: "invocations", category: "Compute", unit: "invocation", price: 0.0003, description: "HTTP requests, events, calls between functions, WebSocket events" },
    MeterInfo { meter: Meter::Cpu, key: "cpu", category: "Compute", unit: "CPU-second", price: 0.05, description: "Time guest code actually ran" },
    MeterInfo { meter: Meter::Memory, key: "memory", category: "Compute", unit: "GB-second", price: 0.0167, description: "Peak instance memory × instance lifetime" },
    MeterInfo { meter: Meter::StaticRequests, key: "static_requests", category: "Network", unit: "request", price: 0.00005, description: "Static files served by the host" },
    MeterInfo { meter: Meter::Egress, key: "egress", category: "Network", unit: "GB", price: 50.0, description: "Response bodies, static files, WebSocket messages sent" },
    MeterInfo { meter: Meter::DbCalls, key: "db_calls", category: "Database", unit: "call", price: 0.0002, description: "Queries, statements, transaction steps" },
    MeterInfo { meter: Meter::DbTime, key: "db_time", category: "Database", unit: "second", price: 0.02, description: "Time spent in the database" },
    MeterInfo { meter: Meter::DbStorage, key: "db_storage", category: "Database", unit: "GB-hour", price: 1.0, description: "SQLite files (with WAL), sampled every minute" },
    MeterInfo { meter: Meter::CacheOps, key: "cache_ops", category: "Cache", unit: "operation", price: 0.00005, description: "get, set, add, delete, incr" },
    MeterInfo { meter: Meter::CacheStorage, key: "cache_storage", category: "Cache", unit: "GB-hour", price: 2.0, description: "Memory held in the app's cache" },
    MeterInfo { meter: Meter::QueueMessages, key: "queue_messages", category: "Queues", unit: "message", price: 0.0004, description: "Messages sent (deliveries count as invocations)" },
    MeterInfo { meter: Meter::WsMessages, key: "ws_messages", category: "WebSockets", unit: "message", price: 0.0005, description: "Messages received and sent" },
    MeterInfo { meter: Meter::WsConnectionMinutes, key: "ws_connection_minutes", category: "WebSockets", unit: "connection-minute", price: 0.00025, description: "Time connections stay open" },
];

pub const CATEGORIES: [&str; 6] = ["Compute", "Network", "Database", "Cache", "Queues", "WebSockets"];

fn info(meter: Meter) -> &'static MeterInfo {
    METERS.iter().find(|m| m.meter == meter).unwrap()
}

/// Credits per unit for each meter.
#[derive(Clone)]
pub struct Pricing {
    prices: HashMap<Meter, f64>,
}

impl Pricing {
    /// The defaults, with overrides from a JSON object `{"cpu": 0.1, ...}`.
    pub fn load(file: Option<&Path>) -> Result<Self> {
        let mut prices: HashMap<Meter, f64> = METERS.iter().map(|m| (m.meter, m.price)).collect();
        if let Some(file) = file {
            let text = std::fs::read_to_string(file).map_err(|e| format_err!("{}: {e}", file.display()))?;
            let overrides: HashMap<String, f64> = serde_json::from_str(&text).map_err(|e| format_err!("{}: {e}", file.display()))?;
            for (key, price) in overrides {
                let m = METERS.iter().find(|m| m.key == key).ok_or_else(|| {
                    format_err!("{}: unknown meter {key:?} (known: {})", file.display(), METERS.map(|m| m.key).join(", "))
                })?;
                prices.insert(m.meter, price);
            }
        }
        Ok(Self { prices })
    }

    pub fn price(&self, meter: Meter) -> f64 {
        self.prices[&meter]
    }
}

#[derive(Default, Clone, Copy)]
struct Amount {
    quantity: f64,
    credits: f64,
}

struct State {
    /// Since the beginning (loaded from the database, then accumulated).
    totals: HashMap<(String, Meter), Amount>,
    /// The minute being accumulated, not yet stored.
    minute: HashMap<(String, Meter), Amount>,
    /// Credits per account per minute, for the last hour (the dashboard chart).
    recent: HashMap<String, Vec<(i64, f64)>>,
}

pub struct Billing {
    pricing: Pricing,
    path: PathBuf,
    /// Indexed by account id; set when the account has used up its grant
    /// (grants come from the deployment and can change on reload).
    blocked: Vec<AtomicBool>,
    state: Mutex<State>,
    /// Set on SIGTERM/SIGINT: the billing thread stores what it has and exits.
    pub shutdown: AtomicBool,
}

fn now_minute() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 / 60 * 60
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format_err!("{}: {e}", dir.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| format_err!("{}: {e}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS usage (
             account  TEXT    NOT NULL,
             minute   INTEGER NOT NULL,  -- unix seconds, start of the minute
             meter    TEXT    NOT NULL,
             quantity REAL    NOT NULL,
             credits  REAL    NOT NULL,
             PRIMARY KEY (account, minute, meter)
         );",
    )
    .map_err(|e| format_err!("{}: {e}", path.display()))?;
    Ok(conn)
}

impl Billing {
    pub fn new(path: &Path, pricing: Pricing, accounts: &[String], grants: &HashMap<String, f64>) -> Result<Self> {
        let conn = open(path)?;
        let mut totals = HashMap::new();
        let mut recent: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT account, meter, sum(quantity), sum(credits) FROM usage GROUP BY account, meter")
                .map_err(|e| format_err!("{e}"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?)))
                .map_err(|e| format_err!("{e}"))?;
            for row in rows {
                let (account, key, quantity, credits) = row.map_err(|e| format_err!("{e}"))?;
                if let Some(m) = METERS.iter().find(|m| m.key == key) {
                    totals.insert((account, m.meter), Amount { quantity, credits });
                }
            }
            let mut stmt = conn
                .prepare("SELECT account, minute, sum(credits) FROM usage WHERE minute >= ?1 GROUP BY account, minute ORDER BY minute")
                .map_err(|e| format_err!("{e}"))?;
            let rows = stmt
                .query_map([now_minute() - 3600], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)))
                .map_err(|e| format_err!("{e}"))?;
            for row in rows {
                let (account, minute, credits) = row.map_err(|e| format_err!("{e}"))?;
                recent.entry(account).or_default().push((minute, credits));
            }
        }
        let billing = Self {
            pricing,
            path: path.to_owned(),
            blocked: (0..MAX_ACCOUNTS).map(|_| AtomicBool::new(false)).collect(),
            state: Mutex::new(State { totals, minute: HashMap::new(), recent }),
            shutdown: AtomicBool::new(false),
        };
        billing.update_blocks(accounts, grants);
        Ok(billing)
    }

    /// A billing that only counts (benchmarks).
    pub fn disabled() -> Self {
        Self {
            pricing: Pricing::load(None).unwrap(),
            path: PathBuf::new(),
            blocked: (0..MAX_ACCOUNTS).map(|_| AtomicBool::new(false)).collect(),
            state: Mutex::new(State { totals: HashMap::new(), minute: HashMap::new(), recent: HashMap::new() }),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Whether the account has used up its credits.
    pub fn is_blocked(&self, account: usize) -> bool {
        self.blocked.get(account).is_some_and(|b| b.load(Relaxed))
    }

    fn spent(state: &State, account: &str) -> f64 {
        state.totals.iter().filter(|((a, _), _)| a == account).map(|(_, v)| v.credits).sum()
    }

    fn update_blocks(&self, accounts: &[String], grants: &HashMap<String, f64>) {
        let state = self.state.lock().unwrap();
        for (id, name) in accounts.iter().enumerate() {
            let blocked = grants.get(name).is_some_and(|grant| Self::spent(&state, name) >= *grant);
            self.blocked[id].store(blocked, Relaxed);
        }
    }

    fn add(&self, state: &mut State, account: &str, meter: Meter, quantity: f64) {
        if quantity <= 0.0 {
            return;
        }
        let credits = quantity * self.pricing.price(meter);
        for map in [&mut state.totals, &mut state.minute] {
            let a = map.entry((account.to_string(), meter)).or_default();
            a.quantity += quantity;
            a.credits += credits;
        }
    }

    /// Stores usage accumulated since the last flush under `minute` (rows
    /// for the same minute add up).
    fn flush(&self, conn: &Connection, minute: i64) {
        let mut state = self.state.lock().unwrap();
        let entries: Vec<_> = state.minute.drain().collect();
        let mut per_account: HashMap<String, f64> = HashMap::new();
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch("BEGIN")?;
            let mut stmt = conn.prepare_cached(
                "INSERT INTO usage (account, minute, meter, quantity, credits) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (account, minute, meter) DO UPDATE SET quantity = quantity + excluded.quantity, credits = credits + excluded.credits",
            )?;
            for ((account, meter), amount) in &entries {
                stmt.execute(params![account, minute, info(*meter).key, amount.quantity, amount.credits])?;
                *per_account.entry(account.clone()).or_default() += amount.credits;
            }
            conn.execute_batch("COMMIT")
        })();
        if let Err(e) = result {
            eprintln!("billing: storing usage: {e}");
            let _ = conn.execute_batch("ROLLBACK");
        }
        for (account, credits) in per_account {
            let series = state.recent.entry(account).or_default();
            match series.last_mut() {
                Some((m, c)) if *m == minute => *c += credits,
                _ => series.push((minute, credits)),
            }
            series.retain(|(m, _)| *m >= minute - 3600);
        }
    }

    /// JSON for `/_platform/billing`: the accounts `include` lets through,
    /// each with its tenant (`tenant_of`), and totals per tenant.
    pub fn to_json(
        &self,
        accounts: &[String],
        grants: &HashMap<String, f64>,
        tenant_of: &dyn Fn(&str) -> Option<String>,
        include: &dyn Fn(&str) -> bool,
    ) -> String {
        let state = self.state.lock().unwrap();
        let mut out = String::from("{\"currency\":\"credits\",\"pricing\":[");
        for (i, m) in METERS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"meter\":{:?},\"category\":{:?},\"unit\":{:?},\"price\":{},\"description\":{:?}}}",
                m.key,
                m.category,
                m.unit,
                self.pricing.price(m.meter),
                m.description
            );
        }
        out.push_str("],\"accounts\":[");
        // The current, not yet stored minute belongs to the chart too.
        let current = now_minute();
        let mut tenants: std::collections::BTreeMap<String, f64> = Default::default();
        let mut first_account = true;
        for name in accounts.iter().filter(|n| include(n)) {
            if !first_account {
                out.push(',');
            }
            first_account = false;
            let spent = Self::spent(&state, name);
            let tenant = tenant_of(name);
            if let Some(t) = &tenant {
                *tenants.entry(t.clone()).or_default() += spent;
            }
            let tenant_json = tenant.map_or("null".to_string(), |t| format!("{t:?}"));
            let _ = write!(out, "{{\"name\":{name:?},\"tenant\":{tenant_json},\"spent\":{spent}");
            match grants.get(name) {
                Some(grant) => {
                    let _ = write!(out, ",\"granted\":{grant},\"remaining\":{},\"blocked\":{}", (grant - spent).max(0.0), spent >= *grant);
                }
                None => out.push_str(",\"granted\":null,\"remaining\":null,\"blocked\":false"),
            }
            out.push_str(",\"categories\":[");
            for (j, category) in CATEGORIES.iter().enumerate() {
                let credits: f64 = state
                    .totals
                    .iter()
                    .filter(|((a, m), _)| a == name && info(*m).category == *category)
                    .map(|(_, v)| v.credits)
                    .sum();
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{{\"category\":{category:?},\"credits\":{credits}}}");
            }
            out.push_str("],\"meters\":[");
            let mut first = true;
            for m in &METERS {
                let a = state.totals.get(&(name.clone(), m.meter)).copied().unwrap_or_default();
                if a.quantity == 0.0 {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = write!(
                    out,
                    "{{\"meter\":{:?},\"category\":{:?},\"unit\":{:?},\"quantity\":{},\"credits\":{}}}",
                    m.key, m.category, m.unit, a.quantity, a.credits
                );
            }
            out.push_str("],\"last_hour\":[");
            let mut series = state.recent.get(name).cloned().unwrap_or_default();
            let pending: f64 = state.minute.iter().filter(|((a, _), _)| a == name).map(|(_, v)| v.credits).sum();
            if pending > 0.0 {
                match series.last_mut() {
                    Some((m, c)) if *m == current => *c += pending,
                    _ => series.push((current, pending)),
                }
            }
            for (k, (minute, credits)) in series.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                let _ = write!(out, "[{minute},{credits}]");
            }
            out.push_str("]}");
        }
        out.push_str("],\"tenants\":[");
        for (i, (tenant, spent)) in tenants.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"name\":{tenant:?},\"spent\":{spent}}}");
        }
        out.push_str("]}\n");
        out
    }
}

/// Database files attributed to accounts for storage billing: a database
/// belongs to the first app granted it.
pub fn storage_owners(platform: &Platform) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let current = platform.current();
    let names = platform.accounts.names();
    for db in &platform.databases {
        let Some(path) = db.url.strip_prefix("sqlite:") else { continue };
        let owner = names
            .iter()
            .find(|name| current.routes.app(name).is_some_and(|a| a.config.databases.iter().any(|d| d == &db.name)));
        if let Some(owner) = owner {
            out.push((owner.clone(), PathBuf::from(path)));
        }
    }
    out
}

/// Starts the thread that turns counters into credits.
pub fn spawn(platform: Arc<Platform>) -> Result<()> {
    let conn = open(&platform.billing.path)?;
    std::thread::Builder::new().name("billing".into()).spawn(move || {
        const TICK: Duration = Duration::from_secs(1);
        let mut previous: Vec<[u64; Counter::ALL.len()]> = Vec::new();
        let mut minute = now_minute();
        let mut last_flush = Instant::now();
        let mut last_storage = Instant::now() - Duration::from_secs(60);
        let mut last = Instant::now();
        loop {
            std::thread::sleep(TICK);
            let dt = last.elapsed().as_secs_f64();
            last = Instant::now();
            let billing = &platform.billing;
            let open_ws = platform.hub.open_by_app();
            // Apps can be added by a reload; their counters start from where
            // they are now.
            let accounts = platform.accounts.names();
            while previous.len() < accounts.len() {
                previous.push(platform.stats.totals(previous.len()));
            }
            let current = platform.current();
            {
                let mut state = billing.state.lock().unwrap();
                for (id, name) in accounts.iter().enumerate() {
                    let now = platform.stats.totals(id);
                    let d = |c: Counter| now[c as usize].wrapping_sub(previous[id][c as usize]) as f64;
                    let invocations = d(Counter::HttpRequests) + d(Counter::Events) + d(Counter::Calls) + d(Counter::WsEvents);
                    billing.add(&mut state, name, Meter::Invocations, invocations);
                    billing.add(&mut state, name, Meter::StaticRequests, d(Counter::StaticRequests));
                    billing.add(&mut state, name, Meter::Cpu, d(Counter::CpuNs) / 1e9);
                    billing.add(&mut state, name, Meter::Memory, d(Counter::MemKibMs) / (1024.0 * 1024.0) / 1000.0);
                    billing.add(&mut state, name, Meter::Egress, d(Counter::EgressBytes) / 1e9);
                    billing.add(&mut state, name, Meter::DbCalls, d(Counter::DbQueries));
                    billing.add(&mut state, name, Meter::DbTime, d(Counter::DbWaitNs) / 1e9);
                    billing.add(&mut state, name, Meter::CacheOps, d(Counter::CacheOps));
                    billing.add(&mut state, name, Meter::QueueMessages, d(Counter::QueueMessages));
                    billing.add(&mut state, name, Meter::WsMessages, d(Counter::WsMessagesIn) + d(Counter::WsMessagesOut));
                    let open = open_ws.get(&id).copied().unwrap_or(0) as f64;
                    billing.add(&mut state, name, Meter::WsConnectionMinutes, open * dt / 60.0);
                    if let Some(app) = current.routes.app(name) {
                        let bytes = app.config.cache.weighted_size() as f64;
                        billing.add(&mut state, name, Meter::CacheStorage, bytes / 1e9 * dt / 3600.0);
                    }
                    previous[id] = now;
                }
                if last_storage.elapsed() >= Duration::from_secs(60) {
                    let hours = last_storage.elapsed().as_secs_f64().min(120.0) / 3600.0;
                    last_storage = Instant::now();
                    for (owner, path) in &storage_owners(&platform) {
                        let size: u64 = ["", "-wal"]
                            .iter()
                            .filter_map(|suffix| std::fs::metadata(format!("{}{suffix}", path.display())).ok())
                            .map(|m| m.len())
                            .sum();
                        billing.add(&mut state, owner, Meter::DbStorage, size as f64 / 1e9 * hours);
                    }
                }
            }
            billing.update_blocks(&accounts, &current.credits);
            let current = now_minute();
            let shutting_down = billing.shutdown.load(Relaxed);
            if current != minute || last_flush.elapsed() >= Duration::from_secs(10) || shutting_down {
                billing.flush(&conn, minute);
                minute = current;
                last_flush = Instant::now();
            }
            if shutting_down {
                eprintln!("usage saved");
                std::process::exit(0);
            }
        }
    })?;
    Ok(())
}
