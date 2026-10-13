//! SQLite backups: periodic snapshots, on-demand snapshots, restore.
//!
//! A snapshot is `VACUUM INTO` (a consistent, compacted copy taken inside a
//! read transaction, so writers aren't blocked in WAL mode), gzipped to
//! `<backup-dir>/<db>/<unix-ms>.db.gz`. The newest `keep` per database are
//! kept. Restoring is offline: stop the server, `wasm-server restore`.

use crate::platform::Platform;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::Connection;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime::{Result, bail, format_err};

#[derive(Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    pub every: Duration,
    pub keep: usize,
}

pub struct Snapshot {
    pub db: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub taken_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Takes one snapshot of the SQLite file at `source`.
pub fn snapshot(config: &BackupConfig, db: &str, source: &Path) -> Result<Snapshot> {
    let started = Instant::now();
    let dir = config.dir.join(db);
    std::fs::create_dir_all(&dir).map_err(|e| format_err!("{}: {e}", dir.display()))?;
    let taken_ms = now_ms();
    let raw = dir.join(format!("{taken_ms}.db.partial"));
    let conn = Connection::open(source).map_err(|e| format_err!("{}: {e}", source.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute("VACUUM INTO ?1", [raw.to_string_lossy()]).map_err(|e| format_err!("{db}: VACUUM INTO: {e}"))?;
    drop(conn);

    let path = dir.join(format!("{taken_ms}.db.gz"));
    let result = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(&raw)?;
        let mut encoder = GzEncoder::new(std::fs::File::create(&path)?, Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.sync_all()
    })();
    let _ = std::fs::remove_file(&raw);
    result.map_err(|e| format_err!("{}: {e}", path.display()))?;

    for old in list(&config.dir, db).into_iter().skip(config.keep) {
        let _ = std::fs::remove_file(old.path);
    }
    let bytes = std::fs::metadata(&path)?.len();
    eprintln!("backup: {db} → {} ({} KiB, {:?})", path.display(), bytes >> 10, started.elapsed());
    Ok(Snapshot { db: db.to_string(), path, bytes, taken_ms })
}

/// Snapshots of one database, newest first.
pub fn list(dir: &Path, db: &str) -> Vec<Snapshot> {
    let Ok(entries) = std::fs::read_dir(dir.join(db)) else { return Vec::new() };
    let mut out: Vec<Snapshot> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            let taken_ms = name.strip_suffix(".db.gz")?.parse().ok()?;
            Some(Snapshot { db: db.to_string(), bytes: e.metadata().ok()?.len(), taken_ms, path })
        })
        .collect();
    out.sort_by(|a, b| b.taken_ms.cmp(&a.taken_ms));
    out
}

/// SQLite databases of the platform: (name, file).
pub fn sqlite_databases(platform: &Platform) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = platform.databases.iter().filter_map(|db| Some((db.name.clone(), PathBuf::from(db.url.strip_prefix("sqlite:")?)))).collect();
    out.extend(crate::managed::databases(&platform.data_dir));
    out
}

/// Snapshots every SQLite database on a schedule.
pub fn spawn(platform: Arc<Platform>, config: BackupConfig) -> Result<()> {
    std::thread::Builder::new().name("backup".into()).spawn(move || {
        loop {
            std::thread::sleep(config.every);
            for (db, path) in sqlite_databases(&platform) {
                if let Err(e) = snapshot(&config, &db, &path) {
                    eprintln!("backup {db} failed: {e:?}");
                }
            }
        }
    })?;
    Ok(())
}

/// Restores `backup` (a `.db.gz` snapshot or a plain SQLite file) over
/// `target`. Run with the server stopped. The restored copy is checked with
/// `PRAGMA integrity_check` before it replaces anything; the old file is
/// kept as `<target>.before-restore`.
pub fn restore(backup: &Path, target: &Path) -> Result<()> {
    let tmp = target.with_extension("restoring");
    let mut data = Vec::new();
    let file = std::fs::File::open(backup).map_err(|e| format_err!("{}: {e}", backup.display()))?;
    if backup.extension().is_some_and(|e| e == "gz") {
        GzDecoder::new(file).read_to_end(&mut data)?;
    } else {
        std::io::BufReader::new(file).read_to_end(&mut data)?;
    }
    if !data.starts_with(b"SQLite format 3\0") {
        bail!("{} is not a SQLite database", backup.display());
    }
    std::fs::File::create(&tmp)?.write_all(&data)?;
    let check: String = Connection::open(&tmp)?.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        let _ = std::fs::remove_file(&tmp);
        bail!("{}: integrity check failed: {check}", backup.display());
    }
    if target.exists() {
        std::fs::rename(target, target.with_extension("before-restore"))?;
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", target.display()));
    }
    std::fs::rename(&tmp, target)?;
    Ok(())
}
