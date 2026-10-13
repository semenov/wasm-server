//! Apps deployed over the API (`plumb deploy`) rather than written into the
//! deployment file. Each lives in `<data-dir>/apps/NAME/`:
//!
//! - `app.toml`: its settings (domains, databases, outbound);
//! - `app.cwasm`: its code, compiled for this server;
//! - `db/DB.db`: its SQLite databases, which the app knows by its own names
//!   (`db.open("main")` is `notes/main` here);
//! - `secrets.json` (owner-only): environment variables the app sees.
//!
//! A deploy applies the app's schema, writes the new files and reloads; if
//! the new version doesn't load, the old files come back.

use crate::deployment::{AppSpec, Deployment};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use wasmtime::{Result, bail, format_err};

/// An app's settings, as `plumb deploy` sends them.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub domains: Vec<String>,
    /// SQLite databases of its own, by the names its code uses.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Hosts it may call; none: any public host.
    pub outbound: Option<Vec<String>>,
    /// Path where WebSocket upgrades go to the app.
    pub websocket: Option<String>,
}

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("apps")
}

fn app_dir(data_dir: &Path, name: &str) -> PathBuf {
    root(data_dir).join(name)
}

/// Names a deployed app may have: they become directory names and URL paths.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 63 && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_db(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Adds every deployed app to `deployment`.
pub fn add_to(deployment: &mut Deployment, data_dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root(data_dir)) else { return Ok(()) };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| valid_name(n)).collect();
    names.sort();
    for name in names {
        let dir = app_dir(data_dir, &name);
        let (settings_file, component) = (dir.join("app.toml"), dir.join("app.cwasm"));
        if !settings_file.exists() || !component.exists() {
            continue;
        }
        if deployment.apps.contains_key(&name) || deployment.sites.contains_key(&name) {
            bail!("{name:?} is both in the deployment file and deployed over the API ({}); remove one", dir.display());
        }
        let text = std::fs::read_to_string(&settings_file).map_err(|e| format_err!("{}: {e}", settings_file.display()))?;
        let settings: Settings = toml::from_str(&text).map_err(|e| format_err!("{}: {e}", settings_file.display()))?;
        deployment.apps.insert(name.clone(), spec(&name, &dir, settings)?);
    }
    Ok(())
}

fn spec(name: &str, dir: &Path, s: Settings) -> Result<AppSpec> {
    let env = read_secrets(dir)?;
    Ok(AppSpec {
        component: dir.join("app.cwasm"),
        databases: s.databases.iter().map(|d| format!("{name}/{d}")).collect(),
        db_aliases: s.databases.iter().map(|d| (d.clone(), format!("{name}/{d}"))).collect(),
        domains: s.domains,
        outbound: s.outbound,
        websocket: s.websocket,
        env,
        ..Default::default()
    })
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
    /// The component, base64.
    pub component: String,
}

/// The files of a deploy, in place; the previous ones, to put back if the
/// new version doesn't load.
pub struct Staged {
    dir: PathBuf,
    previous: Option<(Vec<u8>, Vec<u8>)>,
}

impl Staged {
    pub fn restore(self) {
        match self.previous {
            Some((settings, component)) => {
                let _ = std::fs::write(self.dir.join("app.toml"), settings);
                let _ = std::fs::write(self.dir.join("app.cwasm"), component);
            }
            // a first deploy that failed: nothing to serve
            None => {
                let _ = std::fs::remove_file(self.dir.join("app.toml"));
                let _ = std::fs::remove_file(self.dir.join("app.cwasm"));
            }
        }
    }
}

/// Checks and writes a deploy: the schema first (on the app's databases),
/// then the settings and the code. Runs on a blocking thread.
pub fn stage(engine: &wasmtime::Engine, data_dir: &Path, name: &str, upload: Upload, static_names: &[String]) -> Result<Staged> {
    if !valid_name(name) {
        bail!("{name:?} isn't a valid app name: lower-case letters, digits and -, starting with a letter");
    }
    if static_names.iter().any(|n| n == name) {
        bail!("{name:?} is an app of the deployment file; deploy it with PUT /_platform/apps/{name}");
    }
    let s = &upload.settings;
    for db in &s.databases {
        if !valid_db(db) {
            bail!("{db:?} isn't a valid database name: letters, digits, _ and -");
        }
    }
    for db in upload.schema.keys() {
        if !s.databases.contains(db) {
            bail!("the schema is for database {db:?}, which isn't in the app's databases");
        }
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(upload.component.trim()).map_err(|e| format_err!("component: not base64: {e}"))?;
    if !bytes.starts_with(b"\0asm") {
        bail!("the component isn't a WebAssembly file");
    }
    let compiled = engine.precompile_component(&bytes).map_err(|e| format_err!("the component doesn't compile: {e:?}"))?;
    let dir = app_dir(data_dir, name);
    std::fs::create_dir_all(dir.join("db"))?;
    for (db, sql) in &upload.schema {
        let file = dir.join("db").join(format!("{db}.db"));
        let conn = rusqlite::Connection::open(&file).map_err(|e| format_err!("database {db:?}: {e}"))?;
        conn.pragma_update(None, "journal_mode", "wal").ok();
        conn.execute_batch(sql).map_err(|e| format_err!("the schema of database {db:?} failed: {e}"))?;
    }
    let previous = match (std::fs::read(dir.join("app.toml")), std::fs::read(dir.join("app.cwasm"))) {
        (Ok(a), Ok(b)) => Some((a, b)),
        _ => None,
    };
    if previous.is_some() {
        crate::versions::save(data_dir, name, &dir.join("app.cwasm"))?;
    }
    write_atomic(&dir.join("app.cwasm"), &compiled)?;
    write_atomic(&dir.join("app.toml"), toml::to_string(s)?.as_bytes())?;
    Ok(Staged { dir, previous })
}

/// Sets (or, with `None`, removes) secrets of a deployed app; returns the
/// previous file to put back if the reload fails.
pub fn set_secrets(data_dir: &Path, name: &str, changes: BTreeMap<String, Option<String>>) -> Result<Option<Vec<u8>>> {
    let dir = app_dir(data_dir, name);
    if !dir.join("app.toml").exists() {
        bail!("no deployed app named {name:?}");
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
    let file = app_dir(data_dir, name).join("secrets.json");
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
    let dir = app_dir(data_dir, name);
    if !dir.join("app.toml").exists() {
        bail!("no deployed app named {name:?}");
    }
    Ok(read_secrets(&dir)?.into_keys().collect())
}

/// Deployed apps' SQLite databases: (`app/name`, file).
pub fn databases(data_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(apps) = std::fs::read_dir(root(data_dir)) else { return out };
    for app in apps.flatten() {
        let Ok(name) = app.file_name().into_string() else { continue };
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
        assert!(valid_db("main") && valid_db("users_2") && !valid_db("a/b") && !valid_db(".."));
    }
}
