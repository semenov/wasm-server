//! Previous versions of each app's component, for rollback.
//!
//! Before a deploy or rollback replaces an app's component file, the file is
//! copied to `<data-dir>/versions/<app>/<unix-ms>-<hash>.<ext>`. The newest
//! `KEEP` are kept; identical files are stored once.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use wasmtime::{Result, format_err};

const KEEP: usize = 10;

pub struct Version {
    /// `<unix-ms>-<hash>`
    pub id: String,
    pub saved_ms: u64,
    pub hash: String,
    pub bytes: u64,
    pub path: PathBuf,
}

/// First 12 hex characters of the file's SHA-256.
pub fn hash_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path).map_err(|e| format_err!("{}: {e}", path.display()))?;
    Ok(hex(&Sha256::digest(&data))[..12].to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn dir(data_dir: &Path, app: &str) -> PathBuf {
    data_dir.join("versions").join(app)
}

/// Saved versions, newest first.
pub fn list(data_dir: &Path, app: &str) -> Vec<Version> {
    let Ok(entries) = std::fs::read_dir(dir(data_dir, app)) else { return Vec::new() };
    let mut out: Vec<Version> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_string();
            let (ms, hash) = stem.split_once('-')?;
            Some(Version { saved_ms: ms.parse().ok()?, hash: hash.to_string(), bytes: e.metadata().ok()?.len(), id: stem, path })
        })
        .collect();
    out.sort_by(|a, b| b.saved_ms.cmp(&a.saved_ms));
    out
}

/// Copies `file` into the history (unless an identical version is there).
pub fn save(data_dir: &Path, app: &str, file: &Path) -> Result<Option<PathBuf>> {
    if !file.exists() {
        return Ok(None);
    }
    let hash = hash_file(file)?;
    if let Some(existing) = list(data_dir, app).into_iter().find(|v| v.hash == hash) {
        return Ok(Some(existing.path));
    }
    let dir = dir(data_dir, app);
    std::fs::create_dir_all(&dir).map_err(|e| format_err!("{}: {e}", dir.display()))?;
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("wasm");
    let path = dir.join(format!("{ms}-{hash}.{ext}"));
    std::fs::copy(file, &path).map_err(|e| format_err!("{}: {e}", path.display()))?;
    for old in list(data_dir, app).into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(old.path);
    }
    Ok(Some(path))
}

/// Replaces `target` with `source` atomically (copy, then rename).
pub fn install(source: &Path, target: &Path) -> Result<()> {
    let tmp = target.with_extension("installing");
    std::fs::copy(source, &tmp).map_err(|e| format_err!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, target).map_err(|e| format_err!("{}: {e}", target.display()))?;
    Ok(())
}
