//! The store of releases' parts, by content: every component (`.wasm`),
//! static file and schema is kept once under its SHA-256, however many
//! releases and projects use it. In `<data-dir>/store/`:
//!
//! - `blobs/ab/abcdef...`: the parts, as uploaded;
//! - `releases/ID.json`: release manifests (`managed.rs`), ID being the
//!   SHA-256 of the manifest;
//! - `compiled/HASH-ENGINE.cwasm`: components compiled for this server (its
//!   Wasmtime and CPU), made from the blobs when first needed: a release
//!   itself is portable between servers;
//! - `sites/HASH/`: a site's files laid out as a directory (hard links to
//!   the blobs), for the static file server.
//!
//! A deploy uploads only the blobs the server doesn't have; moving a
//! release to another server (a cluster worker, a new machine) is the same.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use wasmtime::{Engine, Result, bail, format_err};

/// The engine components are compiled for; set at startup.
pub static ENGINE: OnceLock<Engine> = OnceLock::new();

/// A temporary name next to `path`, unique within this process too (two
/// deploys may write the same part at once).
fn tmp_next_to(path: &Path) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("tmp{}-{n}", std::process::id()))
}

pub fn sha256_hex(data: &[u8]) -> String {
    crate::versions::hex(&Sha256::digest(data))
}

pub fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("store")
}

pub fn blob_path(data_dir: &Path, hash: &str) -> PathBuf {
    root(data_dir).join("blobs").join(&hash[..2]).join(hash)
}

pub fn has_blob(data_dir: &Path, hash: &str) -> bool {
    valid_hash(hash) && blob_path(data_dir, hash).exists()
}

/// Stores `data` under its hash, which must be `hash`.
pub fn put_blob(data_dir: &Path, hash: &str, data: &[u8]) -> Result<()> {
    if !valid_hash(hash) {
        bail!("{hash:?} isn't a SHA-256 in hex");
    }
    let actual = sha256_hex(data);
    if actual != hash {
        bail!("the content of blob {hash} has the hash {actual}");
    }
    let path = blob_path(data_dir, hash);
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = tmp_next_to(&path);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_blob(data_dir: &Path, hash: &str) -> Result<Vec<u8>> {
    if !valid_hash(hash) {
        bail!("{hash:?} isn't a SHA-256 in hex");
    }
    std::fs::read(blob_path(data_dir, hash)).map_err(|e| format_err!("blob {hash}: {e}"))
}

/// Blobs in a batch upload: `HASH LENGTH\n` then the bytes, repeated.
pub fn put_batch(data_dir: &Path, mut body: &[u8]) -> Result<usize> {
    let mut n = 0;
    while !body.is_empty() {
        let nl = body.iter().position(|b| *b == b'\n').ok_or_else(|| format_err!("blob batch: a header without a newline"))?;
        let header = std::str::from_utf8(&body[..nl])?;
        let (hash, len) = header.split_once(' ').ok_or_else(|| format_err!("blob batch: expected `HASH LENGTH`, got {header:?}"))?;
        let len: usize = len.parse().map_err(|_| format_err!("blob batch: bad length in {header:?}"))?;
        let rest = &body[nl + 1..];
        if rest.len() < len {
            bail!("blob batch: blob {hash} is cut short");
        }
        put_blob(data_dir, hash, &rest[..len])?;
        body = &rest[len..];
        n += 1;
    }
    Ok(n)
}

pub fn release_path(data_dir: &Path, id: &str) -> PathBuf {
    root(data_dir).join("releases").join(format!("{id}.json"))
}

/// Stores a manifest; its id is its hash.
pub fn put_release(data_dir: &Path, manifest: &[u8]) -> Result<String> {
    let id = sha256_hex(manifest);
    let path = release_path(data_dir, &id);
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = tmp_next_to(&path);
        std::fs::write(&tmp, manifest)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(id)
}

pub fn read_release(data_dir: &Path, id: &str) -> Result<Vec<u8>> {
    if !valid_hash(id) {
        bail!("{id:?} isn't a release id");
    }
    std::fs::read(release_path(data_dir, id)).map_err(|_| format_err!("no release {id}"))
}

fn engine_key(engine: &Engine) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The component blob `hash` compiled for this server, compiling it the
/// first time. Blocking (compiling takes a while).
pub fn compiled(data_dir: &Path, hash: &str) -> Result<PathBuf> {
    let engine = ENGINE.get().ok_or_else(|| format_err!("the engine isn't set up"))?;
    let path = root(data_dir).join("compiled").join(format!("{hash}-{}.cwasm", engine_key(engine)));
    if path.exists() {
        return Ok(path);
    }
    let bytes = read_blob(data_dir, hash)?;
    let code = engine.precompile_component(&bytes).map_err(|e| format_err!("the component {hash} doesn't compile: {e:?}"))?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = tmp_next_to(&path);
    std::fs::write(&tmp, code)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// A site's files (path → blob) as a directory, made once per set of files.
pub fn site_dir(data_dir: &Path, files: &BTreeMap<String, String>) -> Result<PathBuf> {
    let key = sha256_hex(serde_json::to_string(files)?.as_bytes());
    let dir = root(data_dir).join("sites").join(&key);
    if dir.join(".complete").exists() {
        return Ok(dir);
    }
    let tmp = tmp_next_to(&dir);
    for (path, hash) in files {
        let target = tmp.join(path);
        std::fs::create_dir_all(target.parent().unwrap())?;
        let blob = blob_path(data_dir, hash);
        if std::fs::hard_link(&blob, &target).is_err() {
            std::fs::copy(&blob, &target).map_err(|e| format_err!("{path}: blob {hash}: {e}"))?;
        }
    }
    std::fs::write(tmp.join(".complete"), b"")?;
    if std::fs::rename(&tmp, &dir).is_err() {
        // made meanwhile by another deploy
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.join(".complete").exists() {
            bail!("can't lay out the site in {}", dir.display());
        }
    }
    Ok(dir)
}

/// Removes what no kept release uses: blobs (older than an hour, so a
/// deploy's uploads aren't taken before its release arrives), compiled
/// components and site directories. `keep`: the manifests still wanted.
pub fn collect_garbage(data_dir: &Path, keep: &HashSet<String>, used_blobs: &HashSet<String>, used_sites: &HashSet<String>) -> usize {
    let mut removed = 0;
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let old = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t < hour_ago);
    if let Ok(entries) = std::fs::read_dir(root(data_dir).join("releases")) {
        for e in entries.flatten() {
            let id = e.path().file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if !keep.contains(&id) && old(&e.path()) && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    if let Ok(dirs) = std::fs::read_dir(root(data_dir).join("blobs")) {
        for d in dirs.flatten() {
            for e in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let hash = e.file_name().to_string_lossy().to_string();
                if valid_hash(&hash) && !used_blobs.contains(&hash) && old(&e.path()) && std::fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root(data_dir).join("compiled")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let hash = name.split('-').next().unwrap_or("");
            if !used_blobs.contains(hash) && old(&e.path()) && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root(data_dir).join("sites")) {
        for e in entries.flatten() {
            let key = e.file_name().to_string_lossy().to_string();
            if !used_sites.contains(&key) && old(&e.path()) && std::fs::remove_dir_all(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

pub fn site_key(files: &BTreeMap<String, String>) -> String {
    sha256_hex(serde_json::to_string(files).unwrap_or_default().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_by_content() {
        let dir = std::env::temp_dir().join(format!("store-test-{}", std::process::id()));
        let h = sha256_hex(b"hello");
        let batch = [format!("{h} 5\nhello").into_bytes(), format!("{} 0\n", sha256_hex(b"")).into_bytes()].concat();
        assert_eq!(put_batch(&dir, &batch).unwrap(), 2);
        assert_eq!(read_blob(&dir, &h).unwrap(), b"hello");
        assert!(put_blob(&dir, &h, b"other").is_err());
        assert!(put_batch(&dir, format!("{h} 9\nhello").as_bytes()).is_err());
        let files: BTreeMap<String, String> = [("a/b.txt".to_string(), h.clone())].into();
        let site = site_dir(&dir, &files).unwrap();
        assert_eq!(std::fs::read(site.join("a/b.txt")).unwrap(), b"hello");
        assert_eq!(site.file_name().unwrap().to_str().unwrap(), site_key(&files));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
