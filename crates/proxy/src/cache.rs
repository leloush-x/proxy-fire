//! Response cache for non-streaming requests, keyed by SHA-256 of the body.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct ResponseCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    body: Vec<u8>,
    expires: Instant,
}

impl ResponseCache {
    /// ttl_seconds; 0 disables the cache.
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_seconds),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, body: &serde_json::Value) -> Option<Vec<u8>> {
        let key = cache_key(body);
        let mut g = self.entries.lock().unwrap();
        if let Some(e) = g.get(&key) {
            if e.expires > Instant::now() {
                return Some(e.body.clone());
            }
            g.remove(&key);
        }
        None
    }

    pub fn set(&self, body: &serde_json::Value, resp: &[u8]) {
        if self.ttl.is_zero() {
            return;
        }
        let key = cache_key(body);
        self.entries.lock().unwrap().insert(
            key,
            CacheEntry {
                body: resp.to_vec(),
                expires: Instant::now() + self.ttl,
            },
        );
    }

    pub fn evict_expired(&self) {
        let now = Instant::now();
        self.entries.lock().unwrap().retain(|_, e| e.expires > now);
    }
}

fn cache_key(body: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let data = serde_json::to_vec(body).unwrap_or_default();
    format!("{:x}", Sha256::digest(&data))
}
