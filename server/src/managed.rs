//! Projects deployed over the API (`plumb deploy`) rather than written into
//! the deployment file. A project is one or more apps sharing domains,
//! databases and secrets, deployed together. Each lives in
//! `<data-dir>/apps/PROJECT/`:
//!
//! - `project.toml`: its settings (domains, databases, outbound, and each
//!   app's path under the domains);
//! - `code/APP.cwasm`: each app's code, compiled for this server;
//! - `db/DB.db`: its SQLite databases, which its apps know by their own
//!   names (`db.open("main")` is `shop/main` here);
//! - `secrets.json` (owner-only): environment variables its apps see.
//!
//! An app named like its project is known to the server by that name
//! (`notes`), the others as `PROJECT.APP` (`shop.api`): in logs, metrics and
//! the API. A deploy applies the schema, writes all the new files and
//! reloads once; if the new version doesn't load, the old files come back.

use crate::deployment::{AppSpec, Deployment};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use wasmtime::{Result, bail, format_err};

/// A project's settings, as `plumb deploy` sends them.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub domains: Vec<String>,
    /// SQLite databases of its own, by the names its code uses.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Hosts its apps may call; none: any public host.
    pub outbound: Option<Vec<String>>,
    pub apps: BTreeMap<String, AppSettings>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    /// The part of the domains it serves (`/api`); none: all of it.
    pub path: Option<String>,
    /// Path where WebSocket upgrades go to the app.
    pub websocket: Option<String>,
    /// Replaces the project's `outbound` for this app.
    pub outbound: Option<Vec<String>>,
}

/// `app.toml` of a single app deployed before projects: read as a project
/// of one app.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldSettings {
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    databases: Vec<String>,
    outbound: Option<Vec<String>>,
    websocket: Option<String>,
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

/// A project's settings and the component file of each app, from either
/// layout; none if nothing is deployed there.
fn read_project(dir: &Path, project: &str) -> Result<Option<(Settings, BTreeMap<String, PathBuf>)>> {
    let file = dir.join("project.toml");
    if file.exists() {
        let text = std::fs::read_to_string(&file).map_err(|e| format_err!("{}: {e}", file.display()))?;
        let settings: Settings = toml::from_str(&text).map_err(|e| format_err!("{}: {e}", file.display()))?;
        let code = settings.apps.keys().map(|a| (a.clone(), dir.join("code").join(format!("{a}.cwasm")))).collect();
        return Ok(Some((settings, code)));
    }
    let (old, component) = (dir.join("app.toml"), dir.join("app.cwasm"));
    if !old.exists() || !component.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&old).map_err(|e| format_err!("{}: {e}", old.display()))?;
    let o: OldSettings = toml::from_str(&text).map_err(|e| format_err!("{}: {e}", old.display()))?;
    let app = AppSettings { path: None, websocket: o.websocket, outbound: None };
    let settings = Settings { domains: o.domains, databases: o.databases, outbound: o.outbound, apps: [(project.to_string(), app)].into() };
    Ok(Some((settings, [(project.to_string(), component)].into())))
}

/// Adds every deployed project's apps to `deployment`.
pub fn add_to(deployment: &mut Deployment, data_dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root(data_dir)) else { return Ok(()) };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| valid_name(n)).collect();
    names.sort();
    for project in names {
        let dir = project_dir(data_dir, &project);
        let Some((settings, code)) = read_project(&dir, &project)? else { continue };
        let env = read_secrets(&dir)?;
        for (app, s) in &settings.apps {
            let name = app_name(&project, app);
            if deployment.apps.contains_key(&name) || deployment.sites.contains_key(&name) {
                bail!("{name:?} is both in the deployment file and deployed over the API ({}); remove one", dir.display());
            }
            let spec = AppSpec {
                component: code[app].clone(),
                databases: settings.databases.iter().map(|d| format!("{project}/{d}")).collect(),
                db_aliases: settings.databases.iter().map(|d| (d.clone(), format!("{project}/{d}"))).collect(),
                domains: settings.domains.clone(),
                path: s.path.clone(),
                outbound: s.outbound.clone().or_else(|| settings.outbound.clone()),
                websocket: s.websocket.clone(),
                env: env.clone(),
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

/// What a deploy request carries.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    pub settings: Settings,
    /// SQL to run on a database before the new version starts (idempotent:
    /// `create table if not exists ...`), by database name.
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
    /// Each app's component, base64.
    pub components: BTreeMap<String, String>,
}

/// The files of a deploy, in place; the previous ones (in `.previous/`), to
/// put back if the new version doesn't load.
pub struct Staged {
    dir: PathBuf,
}

/// The files a deploy replaces: the settings and the code, in either layout.
const REPLACED: [&str; 4] = ["project.toml", "code", "app.toml", "app.cwasm"];

impl Staged {
    pub fn restore(self) {
        for f in REPLACED {
            let _ = remove(&self.dir.join(f));
            let _ = std::fs::rename(self.dir.join(".previous").join(f), self.dir.join(f));
        }
        let _ = std::fs::remove_dir_all(self.dir.join(".previous"));
    }

    pub fn commit(self) {
        let _ = std::fs::remove_dir_all(self.dir.join(".previous"));
    }
}

fn remove(path: &Path) -> std::io::Result<()> {
    if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }
}

/// Checks and writes a deploy: the schema first (on the project's
/// databases), then the settings and the code. `taken`: names of the
/// deployment file's apps and sites. Runs on a blocking thread.
pub fn stage(engine: &wasmtime::Engine, data_dir: &Path, project: &str, upload: Upload, taken: &[String]) -> Result<Staged> {
    if !valid_name(project) {
        bail!("{project:?} isn't a valid project name: lower-case letters, digits and -, starting with a letter");
    }
    let s = &upload.settings;
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
        if !upload.components.contains_key(app) {
            bail!("no component for app {app:?}");
        }
    }
    if let Some(extra) = upload.components.keys().find(|a| !s.apps.contains_key(*a)) {
        bail!("a component for {extra:?}, which isn't one of the project's apps");
    }
    for db in &s.databases {
        if !valid_db(db) {
            bail!("{db:?} isn't a valid database name: letters, digits, _ and -");
        }
    }
    for db in upload.schema.keys() {
        if !s.databases.contains(db) {
            bail!("the schema is for database {db:?}, which isn't in the project's databases");
        }
    }
    use base64::Engine as _;
    let mut compiled = BTreeMap::new();
    for (app, b64) in &upload.components {
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| format_err!("app {app:?}: the component isn't base64: {e}"))?;
        if !bytes.starts_with(b"\0asm") {
            bail!("app {app:?}: the component isn't a WebAssembly file");
        }
        let code = engine.precompile_component(&bytes).map_err(|e| format_err!("app {app:?}: the component doesn't compile: {e:?}"))?;
        compiled.insert(app.clone(), code);
    }
    let dir = project_dir(data_dir, project);
    std::fs::create_dir_all(dir.join("db"))?;
    for (db, sql) in &upload.schema {
        let file = dir.join("db").join(format!("{db}.db"));
        let conn = rusqlite::Connection::open(&file).map_err(|e| format_err!("database {db:?}: {e}"))?;
        conn.pragma_update(None, "journal_mode", "wal").ok();
        conn.execute_batch(sql).map_err(|e| format_err!("the schema of database {db:?} failed: {e}"))?;
    }
    // the history keeps each app's code before it's replaced
    if let Some((_, code)) = read_project(&dir, project)? {
        for (app, file) in code {
            crate::versions::save(data_dir, &app_name(project, &app), &file)?;
        }
    }
    let previous = dir.join(".previous");
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::create_dir_all(&previous)?;
    for f in REPLACED {
        if dir.join(f).exists() {
            std::fs::rename(dir.join(f), previous.join(f))?;
        }
    }
    let staged = Staged { dir: dir.clone() };
    let written = (|| -> Result<()> {
        std::fs::create_dir_all(dir.join("code"))?;
        for (app, code) in &compiled {
            write_atomic(&dir.join("code").join(format!("{app}.cwasm")), code)?;
        }
        write_atomic(&dir.join("project.toml"), toml::to_string(s)?.as_bytes())
    })();
    if let Err(e) = written {
        staged.restore();
        return Err(e);
    }
    Ok(staged)
}

/// Sets (or, with `None`, removes) secrets of a deployed app; returns the
/// previous file to put back if the reload fails.
pub fn set_secrets(data_dir: &Path, name: &str, changes: BTreeMap<String, Option<String>>) -> Result<Option<Vec<u8>>> {
    let dir = project_dir(data_dir, name);
    if read_project(&dir, name)?.is_none() {
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
    if read_project(&dir, name)?.is_none() {
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
