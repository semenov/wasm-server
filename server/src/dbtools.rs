//! A project's data, for its owner to take away or look into (`plumb db`,
//! `plumb files`): its SQLite databases as files or as SQL, queries on
//! them, and its users' files. Nothing here is in a format of ours: a
//! snapshot is a plain SQLite file, a dump is SQL any SQLite (and, with
//! small edits, Postgres) loads.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value as J, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use wasmtime::{Result, bail, format_err};

/// The file of a project's database, if it has it.
pub fn db_file(data_dir: &Path, project: &str, db: &str) -> Result<PathBuf> {
    if !crate::managed::valid_name(project) || db.is_empty() || !db.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        bail!("no such database");
    }
    let file = crate::managed::dir_of(data_dir, project).join("db").join(format!("{db}.db"));
    if !file.exists() {
        bail!("project {project:?} has no database {db:?}");
    }
    Ok(file)
}

/// The project's databases with their sizes.
pub fn list(data_dir: &Path, project: &str) -> J {
    let dir = crate::managed::dir_of(data_dir, project).join("db");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "db") {
            let name = p.file_stem().unwrap().to_string_lossy().to_string();
            let size = std::fs::metadata(&p).map_or(0, |m| m.len()) + std::fs::metadata(format!("{}-wal", p.display())).map_or(0, |m| m.len());
            out.push(json!({ "name": name, "bytes": size }));
        }
    }
    json!({ "project": project, "databases": out })
}

fn open_read(file: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    Ok(conn)
}

/// A consistent copy of the database as a plain SQLite file (taken without
/// stopping the apps).
pub fn snapshot(file: &Path) -> Result<Vec<u8>> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!("wasm-server-snapshot-{}-{}.db", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let conn = open_read(file)?;
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()]).map_err(|e| format_err!("a snapshot: {e}"))?;
    let bytes = std::fs::read(&tmp);
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes?)
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn sql_value(v: ValueRef) -> String {
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) if f.is_finite() => format!("{f:?}"),
        ValueRef::Real(_) => "NULL".into(),
        ValueRef::Text(t) => format!("'{}'", String::from_utf8_lossy(t).replace('\'', "''")),
        ValueRef::Blob(b) => format!("X'{}'", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    }
}

/// The database as SQL: its schema, then its rows as INSERTs, in one
/// transaction; read in one transaction, so it's consistent.
pub fn dump(file: &Path) -> Result<String> {
    let mut conn = open_read(file)?;
    let tx = conn.transaction()?;
    let mut out = String::from("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n");
    let objects: Vec<(String, String, String)> = tx
        .prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, rowid")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (kind, name, sql) in &objects {
        if kind != "table" {
            continue;
        }
        let _ = writeln!(out, "{sql};");
        let mut stmt = tx.prepare(&format!("SELECT * FROM {}", quote_ident(name)))?;
        let n = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let values: Vec<String> = (0..n).map(|i| row.get_ref(i).map(sql_value).unwrap_or_else(|_| "NULL".into())).collect();
            let _ = writeln!(out, "INSERT INTO {} VALUES({});", quote_ident(name), values.join(","));
        }
    }
    for (kind, _, sql) in &objects {
        if kind != "table" {
            let _ = writeln!(out, "{sql};");
        }
    }
    out.push_str("COMMIT;\n");
    Ok(out)
}

fn json_value(v: ValueRef) -> J {
    match v {
        ValueRef::Null => J::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => J::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!({ "blob_base64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b) }),
    }
}

/// Runs one statement (the owner's, by hand): its rows, or how many it
/// changed. At most `max_rows` rows come back.
pub fn query(file: &Path, sql: &str, max_rows: usize) -> Result<J> {
    let conn = Connection::open(file)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let mut stmt = conn.prepare(sql).map_err(|e| format_err!("{e}"))?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    if columns.is_empty() {
        let changed = stmt.execute([]).map_err(|e| format_err!("{e}"))?;
        return Ok(json!({ "columns": [], "rows": [], "changed": changed }));
    }
    let n = columns.len();
    let mut rows = stmt.query([]).map_err(|e| format_err!("{e}"))?;
    let mut out = Vec::new();
    let mut more = false;
    while let Some(row) = rows.next().map_err(|e| format_err!("{e}"))? {
        if out.len() == max_rows {
            more = true;
            break;
        }
        out.push(J::Array((0..n).map(|i| row.get_ref(i).map(json_value).unwrap_or(J::Null)).collect()));
    }
    Ok(json!({ "columns": columns, "rows": out, "more": more }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_and_queries() {
        let file = std::env::temp_dir().join(format!("dbtools-test-{}.db", std::process::id()));
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch("create table t (id integer primary key, s text, b blob, f real); insert into t values (1, 'it''s', x'00ff', 1.5), (2, null, null, null); create index ti on t (s);").unwrap();
        drop(conn);
        let sql = dump(&file).unwrap();
        assert!(sql.contains("INSERT INTO \"t\" VALUES(1,'it''s',X'00ff',1.5);"), "{sql}");
        assert!(sql.contains("CREATE INDEX ti"), "{sql}");
        let back = std::env::temp_dir().join(format!("dbtools-test-back-{}.db", std::process::id()));
        let conn = Connection::open(&back).unwrap();
        conn.execute_batch(&sql).unwrap();
        assert_eq!(conn.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        let q = query(&file, "select id, s from t order by id", 1).unwrap();
        assert_eq!(q["rows"][0][1], "it's");
        assert_eq!(q["more"], true);
        assert_eq!(query(&file, "update t set s = 'x'", 10).unwrap()["changed"], 2);
        assert!(snapshot(&file).unwrap().starts_with(b"SQLite format 3"));
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&back);
    }
}
