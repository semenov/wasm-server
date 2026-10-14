//! What each statement costs: for every app, per SQL text, how many times it
//! ran, its total and longest time (since the server started; at most
//! `MAX_PER_APP` statements per app). `GET /_platform/projects/P/queries`
//! and `plumb stats --queries` show the costliest. A statement slower than
//! `SLOW` gets a line in its app's log (once a minute at most), with
//! SQLite's plan when it reads a whole table.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SLOW: Duration = Duration::from_millis(200);
const MAX_PER_APP: usize = 1000;
const LOG_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub struct Stat {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub errors: u64,
    last_logged: Option<Instant>,
}

type Apps = HashMap<String, HashMap<String, Stat>>;

fn stats() -> &'static Mutex<Apps> {
    static S: OnceLock<Mutex<Apps>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Counts one run; returns whether to log it as slow now.
pub fn note(app: &str, sql: &str, took: Duration, failed: bool) -> bool {
    let mut all = stats().lock().unwrap();
    let per = all.entry(app.to_string()).or_default();
    if !per.contains_key(sql) && per.len() >= MAX_PER_APP {
        return false;
    }
    let s = per.entry(sql.to_string()).or_default();
    let ns = took.as_nanos() as u64;
    s.count += 1;
    s.total_ns += ns;
    s.max_ns = s.max_ns.max(ns);
    if failed {
        s.errors += 1;
    }
    if took >= SLOW && s.last_logged.is_none_or(|t| t.elapsed() >= LOG_EVERY) {
        s.last_logged = Some(Instant::now());
        return true;
    }
    false
}

/// The costliest statements (by total time) of the apps `include` lets
/// through: (app, sql, stat).
pub fn top(include: &dyn Fn(&str) -> bool, limit: usize) -> Vec<(String, String, Stat)> {
    let all = stats().lock().unwrap();
    let mut out: Vec<(String, String, Stat)> = all.iter().filter(|(app, _)| include(app)).flat_map(|(app, per)| per.iter().map(move |(sql, s)| (app.clone(), sql.clone(), s.clone()))).collect();
    out.sort_by(|a, b| b.2.total_ns.cmp(&a.2.total_ns));
    out.truncate(limit);
    out
}

/// A log line for a slow statement; `plan`: SQLite's EXPLAIN QUERY PLAN
/// details, if there are any; `tables`: the database's tables (a plan also
/// scans constants and `with` results, which no index helps).
pub fn slow_line(sql: &str, took: Duration, plan: &[String], tables: &[String]) -> String {
    let short: String = sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
    let mut line = format!("slow query: {} ms: {}", took.as_millis(), short);
    let mut scans: Vec<&str> = plan
        .iter()
        .filter_map(|d| d.strip_prefix("SCAN "))
        .filter(|rest| !rest.contains("USING"))
        .map(|rest| rest.split_whitespace().next().unwrap_or(rest))
        .filter(|t| tables.iter().any(|x| x.eq_ignore_ascii_case(t)))
        .collect();
    scans.dedup();
    if !scans.is_empty() {
        line += &format!(
            " (it reads all of {}: an index on the columns it filters or sorts by would help, `create index ... on {}(...)` in a migration)",
            scans.join(", "),
            scans[0]
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_slow_lines() {
        assert!(!note("t-app", "select 1", Duration::from_millis(5), false));
        assert!(note("t-app", "select 2", Duration::from_millis(300), false));
        assert!(!note("t-app", "select 2", Duration::from_millis(300), false)); // once a minute
        let t = top(&|a| a == "t-app", 10);
        assert_eq!((t[0].1.as_str(), t[0].2.count), ("select 2", 2));
        let tables = ["notes".to_string()];
        let line = slow_line("select * from notes where tag = ?", Duration::from_millis(250), &["SCAN notes".into()], &tables);
        assert!(line.contains("reads all of notes"), "{line}");
        assert!(!slow_line("select 1", Duration::from_millis(250), &["SEARCH notes USING INDEX i (tag=?)".into()], &tables).contains("reads all"));
        let cte = slow_line("with c ...", Duration::from_millis(250), &["SCAN CONSTANT ROW".into(), "SCAN c".into(), "SCAN notes".into()], &tables);
        assert!(cte.contains("reads all of notes:") && !cte.contains("CONSTANT"), "{cte}");
    }
}
