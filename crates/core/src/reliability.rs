//! Per-provider reliability via exponential moving average (EMA).

use crate::paths::{atomic_write, expand_home};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

const EMA_ALPHA: f64 = 0.1;
const DEFAULT_SUCCESS_RATE: f64 = 1.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStats {
    pub provider_id: String,
    /// 0.0–1.0 EMA
    pub success_rate: f64,
    pub total_calls: i64,
    #[serde(default)]
    pub last_updated: DateTime<Utc>,
}

pub struct Tracker {
    stats: RwLock<HashMap<String, ProviderStats>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            stats: RwLock::new(HashMap::new()),
        }
    }

    pub fn record(&self, provider_id: &str, success: bool) {
        let mut g = self.stats.write().unwrap();
        let s = g
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderStats {
                provider_id: provider_id.to_string(),
                success_rate: DEFAULT_SUCCESS_RATE,
                total_calls: 0,
                last_updated: Utc::now(),
            });
        s.total_calls += 1;
        s.last_updated = Utc::now();
        let val = if success { 1.0 } else { 0.0 };
        s.success_rate = EMA_ALPHA * val + (1.0 - EMA_ALPHA) * s.success_rate;
    }

    /// True when the EMA failure rate exceeds 50% over ≥5 observed requests.
    pub fn should_cooldown(&self, provider_id: &str) -> bool {
        let g = self.stats.read().unwrap();
        match g.get(provider_id) {
            Some(s) => s.total_calls >= 5 && s.success_rate < 0.5,
            None => false,
        }
    }

    pub fn success_rate(&self, provider_id: &str) -> f64 {
        self.stats
            .read()
            .unwrap()
            .get(provider_id)
            .map(|s| s.success_rate)
            .unwrap_or(DEFAULT_SUCCESS_RATE)
    }

    pub fn all(&self) -> Vec<ProviderStats> {
        self.stats.read().unwrap().values().cloned().collect()
    }

    pub fn load_from(&self, stats: Vec<ProviderStats>) {
        let mut g = self.stats.write().unwrap();
        for s in stats {
            g.insert(s.provider_id.clone(), s);
        }
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ReliabilityFile {
    saved_at: DateTime<Utc>,
    stats: Vec<ProviderStats>,
}

pub fn load(tracker: &Tracker, path: &str) -> Result<(), std::io::Error> {
    let data = match std::fs::read(expand_home(path)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let f: ReliabilityFile = serde_json::from_slice(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    tracker.load_from(f.stats);
    Ok(())
}

pub fn save(tracker: &Tracker, path: &str) -> Result<(), std::io::Error> {
    let expanded = expand_home(path);
    let f = ReliabilityFile {
        saved_at: Utc::now(),
        stats: tracker.all(),
    };
    let data = serde_json::to_vec_pretty(&f)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write(&expanded, &data)
}
