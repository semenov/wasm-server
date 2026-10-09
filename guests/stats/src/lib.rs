//! Click statistics for the link shortener. A function without HTTP:
//!
//! - consumes the `clicks` queue (one message per redirect),
//! - rolls up the top links on a cron schedule,
//! - answers typed calls (`demo:stats/api`) from `shortener`.
//!
//! It owns the `stats` database; `shortener` can only reach this data
//! through the interface.

wit_bindgen::generate!({
    path: "wit",
    world: "demo:stats-impl/stats",
    generate_all,
});

use exports::demo::stats::api::{Guest as Api, LinkStats};
use exports::platform::sdk::events::{Event, Guest as Events, Kind};
use platform::sdk::sql::{self, Transaction, Value};

const DB: &str = "stats";
/// Raw clicks are kept this long; totals forever.
const KEEP_CLICKS_MS: i64 = 7 * 24 * 3600 * 1000;

struct Stats;

impl Events for Stats {
    fn handle(event: Event) -> Result<(), String> {
        match (event.kind, event.name.as_str()) {
            (Kind::Queue, "clicks") => record_click(&event),
            (Kind::Cron, "rollup") => rollup(),
            _ => Err(format!("unexpected event {:?} {}", event.kind, event.name)),
        }
    }
}

/// Delivery is at least once, so the event id makes a redelivered message
/// a no-op.
fn record_click(event: &Event) -> Result<(), String> {
    let code = std::str::from_utf8(&event.payload).map_err(|_| "payload is not a link code")?;
    let now = now_ms();
    let tx = Transaction::begin(DB).map_err(db)?;
    let new = tx
        .execute(
            "INSERT OR IGNORE INTO clicks (event_id, code, at) VALUES ($1, $2, $3)",
            &[Value::Text(event.id.clone()), Value::Text(code.into()), Value::Int(now)],
        )
        .map_err(db)?;
    if new == 1 {
        tx.execute(
            "INSERT INTO totals (code, clicks, last_click) VALUES ($1, 1, $2)
             ON CONFLICT (code) DO UPDATE SET clicks = clicks + 1, last_click = excluded.last_click",
            &[Value::Text(code.into()), Value::Int(now)],
        )
        .map_err(db)?;
    }
    Transaction::commit(tx).map_err(db)
}

/// Recomputes the top list and drops old raw clicks.
fn rollup() -> Result<(), String> {
    let tx = Transaction::begin(DB).map_err(db)?;
    tx.execute("DELETE FROM top_links", &[]).map_err(db)?;
    tx.execute(
        "INSERT INTO top_links (rank, code, clicks, last_click)
         SELECT row_number() OVER (ORDER BY clicks DESC, code), code, clicks, last_click
         FROM totals ORDER BY clicks DESC, code LIMIT 100",
        &[],
    )
    .map_err(db)?;
    tx.execute("DELETE FROM clicks WHERE at < $1", &[Value::Int(now_ms() - KEEP_CLICKS_MS)]).map_err(db)?;
    Transaction::commit(tx).map_err(db)
}

impl Api for Stats {
    fn summary(code: String) -> LinkStats {
        let r = sql::query(DB, "SELECT clicks, last_click FROM totals WHERE code = $1", &[Value::Text(code.clone())])
            .expect("stats database");
        match r.rows.first() {
            Some(row) => LinkStats { code, clicks: int(&row[0]) as u64, last_click_ms: int(&row[1]) as u64 },
            None => LinkStats { code, clicks: 0, last_click_ms: 0 },
        }
    }

    fn top(limit: u32) -> Vec<LinkStats> {
        let r = sql::query(
            DB,
            "SELECT code, clicks, last_click FROM top_links ORDER BY rank LIMIT $1",
            &[Value::Int(limit.min(100).into())],
        )
        .expect("stats database");
        r.rows
            .iter()
            .map(|row| LinkStats {
                code: match &row[0] {
                    Value::Text(s) => s.clone(),
                    _ => String::new(),
                },
                clicks: int(&row[1]) as u64,
                last_click_ms: int(&row[2]) as u64,
            })
            .collect()
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn db(e: sql::Error) -> String {
    format!("database: {e:?}")
}

fn now_ms() -> i64 {
    let now = wasip2::clocks::wall_clock::now();
    now.seconds as i64 * 1000 + (now.nanoseconds / 1_000_000) as i64
}

export!(Stats);
