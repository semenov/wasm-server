//! A project's migrations: steps that run once per server, in version
//! order, to bring its data along with its code. Each is a Plumb file with
//! `fn before()` and/or `fn after()`, built into a component per phase
//! (`managed::Migration`), and run as a one-off instance with the project's
//! databases, files, outgoing HTTP and secrets:
//!
//! - `before` runs during the deploy, after a snapshot of the project's
//!   databases (`<project>/backups/`); if one fails, the deploy is refused
//!   and the project keeps its release. A transaction inside the step makes
//!   its database changes all or nothing.
//! - `after` runs once the new release is live, in the background, one step
//!   at a time: backfills, moving files. A failed one is tried again on the
//!   next deploy.
//!
//! `<project>/migrations.jsonl` is the ledger: every run of a step, with its
//! result. A step whose phase has an `ok` line doesn't run again.

use crate::app::{App, AppConfig};
use crate::managed::{self, Migration, Release};
use crate::platform::{Env, Platform};
use crate::store;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a `before` step may run (the deploy waits for it).
const BEFORE_LIMIT: Duration = Duration::from_secs(600);
/// How long an `after` step may run.
const AFTER_LIMIT: Duration = Duration::from_secs(6 * 3600);
/// Snapshots kept per database.
const KEEP_SNAPSHOTS: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub file: String,
    pub phase: String,
    pub ok: bool,
    pub at_ms: u64,
    pub ms: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn version(file: &str) -> &str {
    file.split('_').next().unwrap_or("")
}

/// Every run of the project's steps, oldest first.
pub fn ledger(data_dir: &Path, project: &str) -> Vec<Run> {
    let text = std::fs::read_to_string(managed::dir_of(data_dir, project).join("migrations.jsonl")).unwrap_or_default();
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn note(data_dir: &Path, project: &str, run: &Run) {
    use std::io::Write as _;
    let file = managed::dir_of(data_dir, project).join("migrations.jsonl");
    if let (Ok(mut f), Ok(line)) = (std::fs::OpenOptions::new().create(true).append(true).open(&file), serde_json::to_string(run)) {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether a step's phase has run to the end on this server (by version, so
/// renaming the file doesn't run it again).
fn done(runs: &[Run], file: &str, phase: &str) -> bool {
    runs.iter().any(|r| r.ok && r.phase == phase && version(&r.file) == version(file))
}

/// The release's steps whose `phase` hasn't run here yet, in order.
pub fn pending<'a>(data_dir: &Path, project: &str, r: &'a Release, phase: &str) -> Vec<&'a Migration> {
    let runs = ledger(data_dir, project);
    let mut steps: Vec<&Migration> = r
        .migrations
        .iter()
        .filter(|m| if phase == "before" { m.before.is_some() } else { m.after.is_some() })
        .filter(|m| !done(&runs, &m.file, phase))
        // `after` only once its `before` is done
        .filter(|m| phase == "before" || m.before.is_none() || done(&runs, &m.file, "before"))
        .collect();
    steps.sort_by(|a, b| version(&a.file).parse::<u128>().unwrap_or(0).cmp(&version(&b.file).parse::<u128>().unwrap_or(0)));
    steps
}

/// A snapshot of each of the project's databases, before steps change
/// them; their names.
pub fn snapshot(data_dir: &Path, project: &str, r: &Release, label: &str) -> wasmtime::Result<Vec<String>> {
    let dir = managed::dir_of(data_dir, project);
    let backups = dir.join("backups");
    let mut out = Vec::new();
    for db in &r.settings.databases {
        let file = dir.join("db").join(format!("{db}.db"));
        if !file.exists() {
            continue;
        }
        std::fs::create_dir_all(&backups)?;
        let snap = backups.join(format!("{db}-{}-before-{label}.db", now_ms()));
        let conn = rusqlite::Connection::open(&file)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute("VACUUM INTO ?1", [snap.to_string_lossy()]).map_err(|e| wasmtime::format_err!("database {db:?}: a snapshot: {e}"))?;
        out.push(snap.file_name().unwrap().to_string_lossy().to_string());
        let mut old: Vec<_> = std::fs::read_dir(&backups)?.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&format!("{db}-")))).collect();
        old.sort();
        for p in old.iter().rev().skip(KEEP_SNAPSHOTS) {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(out)
}

/// Runs one phase of one step; the error is its message. Noted in the ledger
/// and in the log of `PROJECT.migrations`.
pub async fn run(platform: &Arc<Platform>, project: &str, r: &Release, m: &Migration, phase: &str) -> Result<(), String> {
    let started = Instant::now();
    let result = run_inner(platform, project, r, m, phase).await;
    let ms = started.elapsed().as_millis() as u64;
    let name = format!("{project}.migrations");
    let error = result.as_ref().err().cloned().unwrap_or_default();
    note(&platform.data_dir, project, &Run { file: m.file.clone(), phase: phase.to_string(), ok: result.is_ok(), at_ms: now_ms(), ms, error: error.clone() });
    match &result {
        Ok(()) => crate::logs::write(&name, &format!("{} ({phase}): done in {ms} ms", m.file)),
        Err(e) => crate::logs::write(&name, &format!("{} ({phase}) failed after {ms} ms: {e}", m.file)),
    }
    result
}

async fn run_inner(platform: &Arc<Platform>, project: &str, r: &Release, m: &Migration, phase: &str) -> Result<(), String> {
    let hash = if phase == "before" { m.before.clone() } else { m.after.clone() }.ok_or("no such phase")?;
    let data_dir = platform.data_dir.clone();
    let path = tokio::task::spawn_blocking(move || store::compiled(&data_dir, &hash)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let engine = platform.engine().clone();
    let name = format!("{project}.migrations");
    let component = crate::app::load_component(&engine, &name, &path).map_err(|e| format!("{e:?}"))?;
    let linker = crate::app::linker(&engine).map_err(|e| e.to_string())?;
    let s = &r.settings;
    let env = managed::secrets(&platform.data_dir, project).map_err(|e| e.to_string())?;
    let limit = if phase == "before" { BEFORE_LIMIT } else { AFTER_LIMIT };
    let config = AppConfig {
        databases: s.databases.iter().map(|d| format!("{project}/{d}")).collect::<Vec<_>>().into(),
        db_timeout: Duration::from_secs(120),
        env: env.into_iter().collect::<Vec<_>>().into(),
        cache: Arc::new(crate::cache::AppCache::new(16 << 20)),
        links: Arc::default(),
        websocket_path: None,
        outbound: Arc::new(crate::outbound::Policy::new(s.outbound.as_deref())),
        db_aliases: Arc::new(s.databases.iter().map(|d| (d.clone(), format!("{project}/{d}"))).collect()),
        project: Some(project.into()),
        cpu_limit: Some(limit),
    };
    let id = platform.accounts.id(&name).map_err(|e| e.to_string())?;
    let app = Arc::new(App::new(&linker, id, &name, &component, config).map_err(|e| format!("{e:?}"))?);
    let pools = Arc::new(crate::db::Pools::new(&platform.databases, 1, &platform.db_tls).map_err(|e| e.to_string())?);
    let mut env = Env::new(platform.clone(), 0, pools);
    env.deadline = tokio::time::Instant::now() + limit;
    let log = Arc::new(crate::logs::RequestLog {
        platform: platform.clone(),
        worker: 0,
        app_id: id,
        app: name.clone(),
        method: "RUN".into(),
        path: format!("{} ({phase})", m.file),
        started: Instant::now(),
        status: Default::default(),
        usage: Default::default(),
        error: Default::default(),
    });
    let req = hyper::Request::builder().uri("http://migrations.local/").body(http_body_util::Empty::<bytes::Bytes>::new()).map_err(|e| e.to_string())?;
    let resp = match tokio::time::timeout(limit, crate::platform::http(env, app, req, log.clone())).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(format!("{e:?}")),
        Err(_) => return Err(format!("it ran longer than {:?}", limit)),
    };
    let status = resp.status().as_u16();
    *log.status.lock().unwrap() = Some(status);
    use http_body_util::BodyExt;
    let body = resp.into_body().collect().await.map(|b| String::from_utf8_lossy(&b.to_bytes()).trim().to_string()).unwrap_or_default();
    if status == 200 { Ok(()) } else { Err(if body.is_empty() { format!("it answered {status}") } else { body }) }
}

/// The `after` steps of the project's release, one at a time, in the
/// background.
pub fn spawn_after(platform: Arc<Platform>, project: String, r: Release) {
    let steps: Vec<Migration> = pending(&platform.data_dir, &project, &r, "after").into_iter().cloned().collect();
    if steps.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for m in &steps {
            if run(&platform, &project, &r, m, "after").await.is_err() {
                // the next ones may rely on it
                break;
            }
        }
    });
}
