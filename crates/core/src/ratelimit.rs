//! Rate-limit tracking: fixed windows, cooldowns, key-slot rotation, quota
//! reservation. Usage persists across restarts (usage.json).

use crate::paths::{atomic_write, expand_home};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Window {
    pub count: i64,
    pub window_end: DateTime<Utc>,
    pub size: Duration,
}

impl Window {
    fn new(size: Duration) -> Self {
        Self {
            count: 0,
            window_end: Utc::now()
                + chrono::Duration::from_std(size).unwrap_or(chrono::Duration::zero()),
            size,
        }
    }

    /// Increment the count if the window hasn't expired; resets if it has.
    /// Returns false when the (non-zero) limit is already reached.
    pub fn add(&mut self, limit: i64) -> bool {
        let now = Utc::now();
        if now > self.window_end {
            self.count = 0;
            self.window_end =
                now + chrono::Duration::from_std(self.size).unwrap_or(chrono::Duration::zero());
        }
        if limit > 0 && self.count >= limit {
            return false;
        }
        self.count += 1;
        true
    }

    pub fn remaining(&self, limit: i64) -> i64 {
        if limit <= 0 {
            return -1;
        }
        if Utc::now() > self.window_end {
            return limit;
        }
        (limit - self.count).max(0)
    }

    fn is_live(&self) -> bool {
        Utc::now() <= self.window_end
    }
}

#[derive(Default)]
pub struct ProviderTracker {
    inner: Mutex<ProviderInner>,
    next_slot: Mutex<Option<DateTime<Utc>>>,
    slot_counter: AtomicU64,
}

#[derive(Default)]
struct ProviderInner {
    per_model: HashMap<String, Window>,
    per_model_day: HashMap<String, Window>,
    per_model_sec: HashMap<String, Window>,
    preemptive_wait_until: Option<DateTime<Utc>>,
}

impl ProviderTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the next send slot for a provider whose requests must be spaced
    /// by at least `spacing`. Returns how long to wait before sending.
    pub fn next_slot(&self, spacing: Duration) -> Duration {
        let mut guard = self.next_slot.lock().unwrap();
        let now = Utc::now();
        let slot = match *guard {
            Some(t) if t > now => t,
            _ => now,
        };
        let spacing_dur = chrono::Duration::from_std(spacing).unwrap_or(chrono::Duration::zero());
        *guard = Some(slot + spacing_dur);
        (slot - now).to_std().unwrap_or(Duration::ZERO)
    }

    /// Round-robin key slot for pooled API keys. Spreads load across the pool
    /// instead of hammering the first key until it 429s.
    pub fn rotate_slot(&self, num_slots: usize) -> usize {
        if num_slots <= 1 {
            return 0;
        }
        let n = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        (n as usize) % num_slots
    }

    /// True if we should attempt a request for the given model.
    /// Pass 0 to skip a dimension. Records nothing (check, not commit).
    pub fn can_request(&self, model_id: &str, rpm_limit: i64, rpd_limit: i64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .preemptive_wait_until
            .map(|t| t > Utc::now())
            .unwrap_or(false)
        {
            return false;
        }
        if rpm_limit > 0 {
            let w = inner
                .per_model
                .entry(model_id.to_string())
                .or_insert_with(|| Window::new(Duration::from_secs(60)));
            if w.count >= rpm_limit && w.is_live() {
                return false;
            }
        }
        if rpd_limit > 0 {
            let w = inner
                .per_model_day
                .entry(model_id.to_string())
                .or_insert_with(|| Window::new(Duration::from_secs(24 * 3600)));
            if w.count >= rpd_limit && w.is_live() {
                return false;
            }
        }
        true
    }

    /// Increment counters for a model after successfully sending a request.
    pub fn record_request(&self, model_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .per_model
            .entry(model_id.to_string())
            .or_insert_with(|| Window::new(Duration::from_secs(60)))
            .add(0);
        inner
            .per_model_day
            .entry(model_id.to_string())
            .or_insert_with(|| Window::new(Duration::from_secs(24 * 3600)))
            .add(0);
    }

    /// Increment the per-second window (Cerebras).
    pub fn record_request_sec(&self, model_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .per_model_sec
            .entry(model_id.to_string())
            .or_insert_with(|| Window::new(Duration::from_secs(1)))
            .add(0);
    }

    /// Prevent imminent rate-limiting (Cerebras: remaining-requests < 2).
    pub fn preemptive_slowdown(&self, wait: Duration) {
        let mut inner = self.inner.lock().unwrap();
        let until =
            Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::zero());
        if inner
            .preemptive_wait_until
            .map(|t| until > t)
            .unwrap_or(true)
        {
            inner.preemptive_wait_until = Some(until);
        }
    }

    pub fn clear_preemptive(&self) {
        self.inner.lock().unwrap().preemptive_wait_until = None;
    }
}

pub struct GlobalTracker {
    trackers: RwLock<HashMap<String, std::sync::Arc<ProviderTracker>>>,
    /// provider_id, provider/model, provider/model#slot or provider#slot → earliest retry.
    cooldowns: RwLock<HashMap<String, DateTime<Utc>>>,
}

impl GlobalTracker {
    pub fn new() -> Self {
        Self {
            trackers: RwLock::new(HashMap::new()),
            cooldowns: RwLock::new(HashMap::new()),
        }
    }

    pub fn provider(&self, provider_id: &str) -> std::sync::Arc<ProviderTracker> {
        if let Some(t) = self.trackers.read().unwrap().get(provider_id) {
            return t.clone();
        }
        let mut g = self.trackers.write().unwrap();
        g.entry(provider_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(ProviderTracker::new()))
            .clone()
    }

    /// Set a cooldown; only extends an existing one, never shortens it.
    pub fn set_cooldown(&self, key: &str, until: DateTime<Utc>) {
        let mut g = self.cooldowns.write().unwrap();
        let extend = g.get(key).map(|e| until > *e).unwrap_or(true);
        if extend {
            g.insert(key.to_string(), until);
        }
    }

    pub fn is_on_cooldown(&self, key: &str) -> bool {
        self.cooldowns
            .read()
            .unwrap()
            .get(key)
            .map(|until| Utc::now() < *until)
            .unwrap_or(false)
    }

    /// Earliest time any active cooldown lifts; zero DateTime(1970) if none.
    pub fn earliest_recovery(&self) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        self.cooldowns
            .read()
            .unwrap()
            .values()
            .filter(|t| **t > now)
            .min()
            .copied()
    }

    // ---- persistence (usage.json) ----

    pub fn snapshot(&self) -> UsageSnapshot {
        let trackers = self.trackers.read().unwrap();
        let mut u = UsageSnapshot {
            saved_at: Utc::now(),
            providers: HashMap::new(),
            gemini: None,
        };
        let now = Utc::now();
        for (id, t) in trackers.iter() {
            let inner = t.inner.lock().unwrap();
            let mut pu = ProviderUsage::default();
            for (model, w) in &inner.per_model_day {
                if w.count == 0 || now > w.window_end {
                    continue;
                }
                pu.model_day_counts.insert(model.clone(), w.count);
                pu.model_day_ends
                    .insert(model.clone(), w.window_end.timestamp());
                if w.window_end.timestamp() > pu.window_end_day {
                    pu.window_end_day = w.window_end.timestamp();
                }
            }
            if !pu.model_day_counts.is_empty() {
                u.providers.insert(id.clone(), pu);
            }
        }
        u
    }

    pub fn restore(&self, u: &UsageSnapshot) {
        let now = Utc::now();
        for (id, pu) in &u.providers {
            let t = self.provider(id);
            let mut inner = t.inner.lock().unwrap();
            for (model, &count) in &pu.model_day_counts {
                if count <= 0 {
                    continue;
                }
                let end_ts = pu
                    .model_day_ends
                    .get(model)
                    .copied()
                    .unwrap_or(pu.window_end_day);
                let Some(window_end) = DateTime::from_timestamp(end_ts, 0) else {
                    continue;
                };
                if window_end <= now {
                    continue;
                }
                inner.per_model_day.insert(
                    model.clone(),
                    Window {
                        count,
                        window_end,
                        size: Duration::from_secs(24 * 3600),
                    },
                );
            }
        }
    }
}

impl Default for GlobalTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    #[serde(default)]
    pub saved_at: DateTime<Utc>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<crate::gemini::GeminiUsage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    #[serde(default)]
    pub model_minute_counts: HashMap<String, i64>,
    #[serde(default)]
    pub model_day_counts: HashMap<String, i64>,
    #[serde(default)]
    pub window_end_minute: i64,
    #[serde(default)]
    pub window_end_day: i64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_day_ends: HashMap<String, i64>,
}

pub fn load_usage(path: &str) -> Result<UsageSnapshot, std::io::Error> {
    let expanded = expand_home(path);
    match std::fs::read(&expanded) {
        Ok(data) => serde_json::from_slice(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UsageSnapshot::default()),
        Err(e) => Err(e),
    }
}

pub fn save_usage(u: &UsageSnapshot, path: &str) -> Result<(), std::io::Error> {
    let expanded = expand_home(path);
    let mut u = u.clone();
    u.saved_at = Utc::now();
    let data = serde_json::to_vec_pretty(&u)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write(&expanded, &data)
}
