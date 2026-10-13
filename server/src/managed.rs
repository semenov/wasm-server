//! Projects deployed over the API (`plumb deploy`) rather than written into
//! the deployment file. A project is one or more apps sharing domains,
//! databases and secrets, deployed together as a *release*: a manifest
//! naming its settings and, by hash, each app's component, static files and
//! the schema (the parts are in the store, `store.rs`). In
//! `<data-dir>/apps/PROJECT/`:
//!
//! - `current`: the id of the release it runs;
//! - `history.jsonl`: the releases it ran, newest last, with when and a
//!   note (the deploy's git commit, say); rollback goes back to any of them;
//! - `db/DB.db`: its SQLite databases, which its apps know by their own
//!   names (`db.open("main")` is `shop/main` here);
//! - `secrets.json` (owner-only): environment variables its apps see (not
//!   part of releases: rolling back doesn't bring back an old key);
//! - `files.db`, `files/`: its users' files (`storage.rs`).
//!
//! An app named like its project is known to the server by that name
//! (`notes`), the others as `PROJECT.APP` (`shop.api`): in logs, metrics and
//! the API. A deploy checks the release, compiles what's new, applies the
//! schema, points `current` at it and reloads; if the new version doesn't
//! load, `current` goes back.

use crate::deployment::{AppSpec, Deployment};
use crate::store;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use wasmtime::{Result, bail, format_err};

/// A project's settings.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub domains: Vec<String>,
    /// SQLite databases of its own, by the names its code uses.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Hosts its apps may call; none: any public host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound: Option<Vec<String>>,
    pub apps: BTreeMap<String, AppSettings>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    /// The part of the domains it serves (`/api`); none: all of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Path where WebSocket upgrades go to the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket: Option<String>,
    /// Replaces the project's `outbound` for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound: Option<Vec<String>>,
    /// Without code: a single-page app, `index.html` for every page path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spa: bool,
}

/// A release: what a project runs, every part by its hash in the store.
/// Its id is the SHA-256 of its JSON (as the server writes it).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub project: String,
    pub settings: Settings,
    /// App → its component (`.wasm`).
    #[serde(default)]
    pub components: BTreeMap<String, String>,
    /// App → its static files: path in the site → content.
    #[serde(default)]
    pub files: BTreeMap<String, BTreeMap<String, String>>,
    /// Database → SQL run before the release starts (idempotent: `create
    /// table if not exists ...`).
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
    /// What built it (`plumb 0.1.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler: Option<String>,
}

impl Release {
    /// Every blob it needs.
    pub fn blobs(&self) -> impl Iterator<Item = &String> {
        self.components.values().chain(self.files.values().flat_map(|f| f.values())).chain(self.schema.values())
    }
}

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("apps")
}

fn project_dir(data_dir: &Path, name: &str) -> PathBuf {
    root(data_dir).join(name)
}

/// The server's name for an app of a project.
pub fn app_name(project: &str, app: &str) -> String {
    if app == project { project.to_string() } else { format!("{project}.{app}") }
}

/// The project a server app name belongs to (`shop.api` → `shop`).
pub fn project_of(app: &str) -> &str {
    app.split_once('.').map_or(app, |(p, _)| p)
}

/// Names a project or app may have: they become directory names and URL paths.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 63 && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_db(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The release a project runs.
pub fn current(data_dir: &Path, project: &str) -> Option<String> {
    let id = std::fs::read_to_string(project_dir(data_dir, project).join("current")).ok()?;
    let id = id.trim();
    store::valid_hash(id).then(|| id.to_string())
}

/// Points the project at a release (none: it runs nothing).
pub fn set_current(data_dir: &Path, project: &str, id: Option<&str>) -> Result<()> {
    let file = project_dir(data_dir, project).join("current");
    match id {
        Some(id) => {
            std::fs::create_dir_all(file.parent().unwrap())?;
            write_atomic(&file, id.as_bytes())
        }
        None => {
            let _ = std::fs::remove_file(file);
            Ok(())
        }
    }
}

pub fn load_release(data_dir: &Path, id: &str) -> Result<Release> {
    let bytes = store::read_release(data_dir, id)?;
    serde_json::from_slice(&bytes).map_err(|e| format_err!("release {id}: {e}"))
}

/// Stores a release's manifest (as the server writes it); its id.
pub fn put_release(data_dir: &Path, release: &Release) -> Result<String> {
    store::put_release(data_dir, &serde_json::to_vec(release)?)
}

/// Adds every deployed project's apps to `deployment`.
pub fn add_to(deployment: &mut Deployment, data_dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root(data_dir)) else { return Ok(()) };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| valid_name(n)).collect();
    names.sort();
    for project in names {
        let Some(id) = current(data_dir, &project) else { continue };
        let release = load_release(data_dir, &id)?;
        let dir = project_dir(data_dir, &project);
        let settings = &release.settings;
        let env = read_secrets(&dir)?;
        for (app, s) in &settings.apps {
            let name = app_name(&project, app);
            if deployment.apps.contains_key(&name) || deployment.sites.contains_key(&name) {
                bail!("{name:?} is both in the deployment file and deployed over the API ({}); remove one", dir.display());
            }
            let static_dir = match release.files.get(app).filter(|f| !f.is_empty()) {
                Some(files) => Some(store::site_dir(data_dir, files)?),
                None => None,
            };
            let Some(hash) = release.components.get(app) else {
                // files only
                let Some(static_dir) = static_dir else { continue };
                let site = crate::deployment::SiteSpec { static_dir, domains: settings.domains.clone(), path: s.path.clone(), spa: s.spa };
                deployment.sites.insert(name, site);
                continue;
            };
            let spec = AppSpec {
                component: store::compiled(data_dir, hash)?,
                source: Some(hash.clone()),
                static_dir,
                databases: settings.databases.iter().map(|d| format!("{project}/{d}")).collect(),
                db_aliases: settings.databases.iter().map(|d| (d.clone(), format!("{project}/{d}"))).collect(),
                domains: settings.domains.clone(),
                path: s.path.clone(),
                outbound: s.outbound.clone().or_else(|| settings.outbound.clone()),
                websocket: s.websocket.clone(),
                env: env.clone(),
                project: Some(project.clone()),
                ..Default::default()
            };
            deployment.apps.insert(name, spec);
        }
    }
    Ok(())
}

fn read_secrets(dir: &Path) -> Result<BTreeMap<String, String>> {
    let file = dir.join("secrets.json");
    match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format_err!("{}: {e}", file.display())),
        Err(_) => Ok(BTreeMap::new()),
    }
}

/// Checks a release for `project`: names, paths, databases, and that its
/// parts are all in the store. `taken`: names of the deployment file's
/// apps and sites.
pub fn check(data_dir: &Path, project: &str, r: &Release, taken: &[String]) -> Result<()> {
    if !valid_name(project) {
        bail!("{project:?} isn't a valid project name: lower-case letters, digits and -, starting with a letter");
    }
    if r.project != project {
        bail!("the release is for project {:?}, not {project:?}", r.project);
    }
    let s = &r.settings;
    if s.apps.is_empty() {
        bail!("the project has no apps");
    }
    for (app, a) in &s.apps {
        if !valid_name(app) {
            bail!("{app:?} isn't a valid app name: lower-case letters, digits and -, starting with a letter");
        }
        let name = app_name(project, app);
        if taken.contains(&name) {
            bail!("{name:?} is an app of the deployment file; deploy it with PUT /_platform/apps/{name}");
        }
        if let Some(path) = &a.path {
            if !crate::deployment::valid_path(path) {
                bail!("app {app:?}: path {path:?} must start with / and not end with one (\"/api\")");
            }
        }
        let has_files = r.files.get(app).is_some_and(|f| !f.is_empty());
        if !r.components.contains_key(app) && !has_files {
            bail!("app {app:?} has neither code nor files");
        }
        if a.spa && r.components.contains_key(app) {
            bail!("app {app:?}: `spa` is for an app of files only; with code, the code answers what isn't a file");
        }
    }
    if let Some(extra) = r.components.keys().chain(r.files.keys()).find(|a| !s.apps.contains_key(*a)) {
        bail!("code or files for {extra:?}, which isn't one of the project's apps");
    }
    for (app, list) in &r.files {
        for path in list.keys() {
            let ok = !path.is_empty() && !path.starts_with('/') && path.len() <= 512 && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.contains('\\'));
            if !ok {
                bail!("app {app:?}: {path:?} isn't a valid file path in a site");
            }
        }
    }
    for db in &s.databases {
        if !valid_db(db) {
            bail!("{db:?} isn't a valid database name: letters, digits, _ and -");
        }
    }
    for db in r.schema.keys() {
        if !s.databases.contains(db) {
            bail!("the schema is for database {db:?}, which isn't in the project's databases");
        }
    }
    let missing = missing_blobs(data_dir, r);
    if !missing.is_empty() {
        bail!("the store doesn't have {} of the release's parts (upload them first): {}", missing.len(), missing.join(", "));
    }
    Ok(())
}

pub fn missing_blobs(data_dir: &Path, r: &Release) -> Vec<String> {
    let mut seen = HashSet::new();
    r.blobs().filter(|h| seen.insert(*h) && !store::has_blob(data_dir, h)).cloned().collect()
}

/// Gets a checked release ready to run: compiles its components, lays out
/// its sites, applies its schema. Blocking.
pub fn prepare(data_dir: &Path, project: &str, r: &Release) -> Result<()> {
    for (app, hash) in &r.components {
        store::compiled(data_dir, hash).map_err(|e| format_err!("app {app:?}: {e}"))?;
    }
    for files in r.files.values().filter(|f| !f.is_empty()) {
        store::site_dir(data_dir, files)?;
    }
    let dir = project_dir(data_dir, project);
    std::fs::create_dir_all(dir.join("db"))?;
    for (db, hash) in &r.schema {
        let sql = String::from_utf8(store::read_blob(data_dir, hash)?).map_err(|_| format_err!("the schema of database {db:?} isn't UTF-8"))?;
        let file = dir.join("db").join(format!("{db}.db"));
        let conn = rusqlite::Connection::open(&file).map_err(|e| format_err!("database {db:?}: {e}"))?;
        conn.pragma_update(None, "journal_mode", "wal").ok();
        conn.execute_batch(&sql).map_err(|e| format_err!("the schema of database {db:?} failed: {e}"))?;
    }
    Ok(())
}

/// A line of a project's history.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub id: String,
    pub at_ms: u64,
    #[serde(default)]
    pub note: String,
}

/// The releases a project ran, oldest first.
pub fn history(data_dir: &Path, project: &str) -> Vec<Entry> {
    let text = std::fs::read_to_string(project_dir(data_dir, project).join("history.jsonl")).unwrap_or_default();
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Notes that the project now runs release `id`, and clears out what no
/// recent release uses.
pub fn record(data_dir: &Path, project: &str, id: &str, note: &str) -> Result<()> {
    use std::io::Write as _;
    let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let line = serde_json::to_string(&Entry { id: id.to_string(), at_ms, note: note.chars().take(500).collect() })?;
    let file = project_dir(data_dir, project).join("history.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
    writeln!(f, "{line}")?;
    drop(f);
    // keep the last KEEP entries
    let all = history(data_dir, project);
    if all.len() > KEEP * 2 {
        let kept: String = all[all.len() - KEEP..].iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
        write_atomic(&file, kept.as_bytes())?;
    }
    collect_garbage(data_dir);
    Ok(())
}

/// Releases kept per project for rollback.
const KEEP: usize = 20;

/// Removes from the store what no project's recent releases use.
pub fn collect_garbage(data_dir: &Path) {
    let mut keep = HashSet::new();
    let Ok(entries) = std::fs::read_dir(root(data_dir)) else { return };
    for e in entries.flatten() {
        let Ok(project) = e.file_name().into_string() else { continue };
        let h = history(data_dir, &project);
        keep.extend(h.iter().rev().take(KEEP).map(|e| e.id.clone()));
        keep.extend(current(data_dir, &project));
    }
    let (mut blobs, mut sites) = (HashSet::new(), HashSet::new());
    for id in &keep {
        if let Ok(r) = load_release(data_dir, id) {
            blobs.extend(r.blobs().cloned());
            sites.extend(r.files.values().filter(|f| !f.is_empty()).map(store::site_key));
        }
    }
    let removed = store::collect_garbage(data_dir, &keep, &blobs, &sites);
    if removed > 0 {
        eprintln!("store: removed {removed} parts no recent release uses");
    }
}

/// A deploy with everything inline (base64), for clients that don't speak
/// the store: turned into blobs and a release.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    pub settings: Settings,
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
    #[serde(default)]
    pub components: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, BTreeMap<String, String>>,
}

impl Upload {
    pub fn into_release(self, data_dir: &Path, project: &str) -> Result<Release> {
        let put = |bytes: Vec<u8>| -> Result<String> {
            let h = store::sha256_hex(&bytes);
            store::put_blob(data_dir, &h, &bytes)?;
            Ok(h)
        };
        let b64 = |s: &str, what: &str| base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|e| format_err!("{what}: not base64: {e}"));
        let mut r = Release { project: project.to_string(), settings: self.settings, components: BTreeMap::new(), files: BTreeMap::new(), schema: BTreeMap::new(), compiler: None };
        for (app, c) in self.components {
            let bytes = b64(&c, &format!("app {app:?}"))?;
            if !bytes.starts_with(b"\0asm") {
                bail!("app {app:?}: the component isn't a WebAssembly file");
            }
            r.components.insert(app, put(bytes)?);
        }
        for (app, list) in self.files {
            let mut out = BTreeMap::new();
            for (path, c) in list {
                out.insert(path.clone(), put(b64(&c, &format!("app {app:?}: {path}"))?)?);
            }
            r.files.insert(app, out);
        }
        for (db, sql) in self.schema {
            r.schema.insert(db, put(sql.into_bytes())?);
        }
        Ok(r)
    }
}

/// Sets (or, with `None`, removes) secrets of a deployed app; returns the
/// previous file to put back if the reload fails.
pub fn set_secrets(data_dir: &Path, name: &str, changes: BTreeMap<String, Option<String>>) -> Result<Option<Vec<u8>>> {
    let dir = project_dir(data_dir, name);
    if current(data_dir, name).is_none() {
        bail!("no deployed project named {name:?}");
    }
    for key in changes.keys() {
        let ok = !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') && !key.as_bytes()[0].is_ascii_digit();
        if !ok {
            bail!("{key:?} isn't a valid variable name: letters, digits and _");
        }
    }
    let file = dir.join("secrets.json");
    let before = std::fs::read(&file).ok();
    let mut secrets = read_secrets(&dir)?;
    for (k, v) in changes {
        match v {
            Some(v) => secrets.insert(k, v),
            None => secrets.remove(&k),
        };
    }
    write_private(&file, serde_json::to_string_pretty(&secrets)?.as_bytes())?;
    Ok(before)
}

pub fn restore_secrets(data_dir: &Path, name: &str, before: Option<Vec<u8>>) {
    let file = project_dir(data_dir, name).join("secrets.json");
    match before {
        Some(b) => {
            let _ = write_private(&file, &b);
        }
        None => {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// The names (not the values) of a deployed app's secrets.
pub fn secret_names(data_dir: &Path, name: &str) -> Result<Vec<String>> {
    let dir = project_dir(data_dir, name);
    if current(data_dir, name).is_none() {
        bail!("no deployed project named {name:?}");
    }
    Ok(read_secrets(&dir)?.into_keys().collect())
}

/// Deployed projects' SQLite databases: (`project/name`, file).
pub fn databases(data_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(apps) = std::fs::read_dir(root(data_dir)) else { return out };
    for app in apps.flatten() {
        let Ok(name) = app.file_name().into_string() else { continue };
        if !valid_name(&name) {
            continue;
        }
        let Ok(dbs) = std::fs::read_dir(app.path().join("db")) else { continue };
        for db in dbs.flatten() {
            let path = db.path();
            if path.extension().is_some_and(|e| e == "db") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push((format!("{name}/{stem}"), path.clone()));
                }
            }
        }
    }
    out.sort();
    out
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, data).map_err(|e| format_err!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format_err!("{}: {e}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("notes") && valid_name("my-app2"));
        assert!(!valid_name("") && !valid_name("Notes") && !valid_name("2app") && !valid_name("a/b") && !valid_name("..") && !valid_name("a_b"));
        assert_eq!((app_name("shop", "shop"), app_name("shop", "api")), ("shop".to_string(), "shop.api".to_string()));
        assert_eq!((project_of("shop"), project_of("shop.api")), ("shop", "shop"));
        assert!(valid_db("main") && valid_db("users_2") && !valid_db("a/b") && !valid_db(".."));
    }
}
