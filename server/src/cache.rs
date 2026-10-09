//! `platform:cache`: an in-memory cache per app, shared by all workers.
//!
//! Unlike the rest of the request path this is shared between cores: a
//! per-core cache would hit far less often, and counters (rate limits) must
//! be exact. `moka` shards its locks, so contention stays low.

use crate::app::RequestState;
use crate::stats::Counter;
use bytes::Bytes;
use moka::Expiry;
use moka::ops::compute::Op;
use std::time::{Duration, Instant};
use wasmtime::Result;
use wasmtime::component::{HasSelf, Linker};

use crate::bindings::cache::{self as wit, Error};

/// Largest key plus value.
const MAX_ENTRY_BYTES: usize = 1 << 20;

#[derive(Clone)]
struct Entry {
    value: Bytes,
    /// Time to live from the last write; `None` for no expiry.
    ttl: Option<Duration>,
    /// For counter updates, which keep the expiry set at creation.
    keep_ttl: bool,
}

struct PerEntryTtl;

impl Expiry<String, Entry> for PerEntryTtl {
    fn expire_after_create(&self, _key: &String, entry: &Entry, _created_at: Instant) -> Option<Duration> {
        entry.ttl
    }

    fn expire_after_update(
        &self,
        _key: &String,
        entry: &Entry,
        _updated_at: Instant,
        remaining: Option<Duration>,
    ) -> Option<Duration> {
        if entry.keep_ttl { remaining } else { entry.ttl }
    }
}

/// One app's cache, bounded by bytes.
pub struct AppCache {
    entries: moka::sync::Cache<String, Entry>,
}

impl AppCache {
    /// Bytes currently held (approximate), for storage billing.
    pub fn weighted_size(&self) -> u64 {
        self.entries.run_pending_tasks();
        self.entries.weighted_size()
    }

    pub fn new(max_bytes: u64) -> Self {
        let entries = moka::sync::Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|key: &String, entry: &Entry| (key.len() + entry.value.len() + 64).try_into().unwrap_or(u32::MAX))
            .expire_after(PerEntryTtl)
            .build();
        Self { entries }
    }

    fn entry(key: &str, value: Vec<u8>, ttl_ms: u64) -> Result<Entry, Error> {
        let size = key.len() + value.len();
        if size > MAX_ENTRY_BYTES {
            return Err(Error::TooLarge(size as u64));
        }
        Ok(Entry { value: value.into(), ttl: (ttl_ms > 0).then(|| Duration::from_millis(ttl_ms)), keep_ttl: false })
    }
}

pub fn add_to_linker(linker: &mut Linker<RequestState>) -> Result<()> {
    wit::add_to_linker::<RequestState, HasSelf<RequestState>>(linker, |s| s)
}

impl wit::Host for RequestState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>> {
        self.counters().add(Counter::CacheOps, 1);
        Ok(self.cache.entries.get(&key).map(|e| e.value.to_vec()))
    }

    fn set(&mut self, key: String, value: Vec<u8>, ttl_ms: u64) -> Result<Result<(), Error>> {
        self.counters().add(Counter::CacheOps, 1);
        self.counters().add(Counter::CacheBytesWritten, (key.len() + value.len()) as u64);
        Ok(AppCache::entry(&key, value, ttl_ms).map(|entry| self.cache.entries.insert(key, entry)))
    }

    fn add(&mut self, key: String, value: Vec<u8>, ttl_ms: u64) -> Result<Result<bool, Error>> {
        self.counters().add(Counter::CacheOps, 1);
        self.counters().add(Counter::CacheBytesWritten, (key.len() + value.len()) as u64);
        let entry = match AppCache::entry(&key, value, ttl_ms) {
            Ok(entry) => entry,
            Err(e) => return Ok(Err(e)),
        };
        let mut stored = false;
        self.cache.entries.entry(key).and_compute_with(|existing| match existing {
            Some(_) => Op::Nop,
            None => {
                stored = true;
                Op::Put(entry)
            }
        });
        Ok(Ok(stored))
    }

    fn delete(&mut self, key: String) -> Result<()> {
        self.counters().add(Counter::CacheOps, 1);
        self.cache.entries.invalidate(&key);
        Ok(())
    }

    fn incr(&mut self, key: String, delta: i64, ttl_ms: u64) -> Result<i64> {
        self.counters().add(Counter::CacheOps, 1);
        let mut result = 0;
        self.cache.entries.entry(key).and_compute_with(|existing| {
            // Counters are 8 bytes, little-endian; anything else starts over.
            let current = existing.as_ref().and_then(|e| <[u8; 8]>::try_from(e.value().value.as_ref()).ok());
            result = current.map_or(0, i64::from_le_bytes).wrapping_add(delta);
            let ttl = (ttl_ms > 0).then(|| Duration::from_millis(ttl_ms));
            Op::Put(Entry { value: Bytes::copy_from_slice(&result.to_le_bytes()), ttl, keep_ttl: current.is_some() })
        });
        Ok(result)
    }
}
