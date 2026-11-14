//! Gemini 4-dimensional quota tracker (RPM / TPM / RPD / IPM) per model.
//! Limits are per GCP project, shared by all keys in the project.

use crate::headers::midnight_pt;
use crate::ratelimit::Window;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiModelLimits {
    pub rpm: i64,
    pub tpm: i64,
    pub rpd: i64,
    pub ipm: i64,
}

pub struct GeminiTracker {
    model_limits: Mutex<HashMap<String, GeminiModelLimits>>,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    rpm: HashMap<String, Window>,
    tpm: HashMap<String, Window>,
    #[allow(dead_code)] // reserved for image models; never reserved yet
    ipm: HashMap<String, Window>,
    rpd: HashMap<String, Window>,
    day_start: DateTime<Utc>,
}

impl GeminiTracker {
    pub fn new() -> Self {
        Self {
            model_limits: Mutex::new(default_gemini_limits()),
            inner: Arc::new(Mutex::new(Inner {
                rpm: HashMap::new(),
                tpm: HashMap::new(),
                ipm: HashMap::new(),
                rpd: HashMap::new(),
                day_start: midnight_pt(),
            })),
        }
    }

    pub fn set_limits(&self, model_id: &str, limits: GeminiModelLimits) {
        self.model_limits
            .lock()
            .unwrap()
            .insert(model_id.to_string(), limits);
    }

    /// Check all 4 quota dimensions without counting.
    pub fn can_request(&self, model_id: &str, estimated_tokens: i64) -> bool {
        let known = self.model_limits.lock().unwrap().get(model_id).copied();
        let Some(l) = known else { return true };
        let mut inner = self.inner.lock().unwrap();
        maybe_reset_day(&mut inner);
        if l.rpm > 0 && window_count(&inner.rpm, model_id) >= l.rpm {
            return false;
        }
        if l.tpm > 0
            && estimated_tokens > 0
            && window_count(&inner.tpm, model_id) + estimated_tokens > l.tpm
        {
            return false;
        }
        if l.rpd > 0 && window_count(&inner.rpd, model_id) >= l.rpd {
            return false;
        }
        true
    }

    /// Record a sent request.
    pub fn record_request(&self, model_id: &str, tokens: i64) {
        let mut inner = self.inner.lock().unwrap();
        maybe_reset_day(&mut inner);
        get_window(&mut inner.rpm, model_id, StdDuration::from_secs(60)).count += 1;
        get_window(&mut inner.rpd, model_id, StdDuration::from_secs(86400)).count += 1;
        if tokens > 0 {
            get_window(&mut inner.tpm, model_id, StdDuration::from_secs(60)).count += tokens;
        }
    }

    /// Atomically check all quota dimensions and count the request in one step,
    /// so concurrent callers cannot overshoot. Returns None when over quota.
    /// The reservation auto-releases on drop unless `keep()` is called.
    pub fn reserve(&self, model_id: &str, estimated_tokens: i64) -> Option<GeminiReservation> {
        let known = self.model_limits.lock().unwrap().get(model_id).copied();

        let mut inner = self.inner.lock().unwrap();
        maybe_reset_day(&mut inner);

        if let Some(l) = known {
            if l.rpm > 0 && window_count(&inner.rpm, model_id) >= l.rpm {
                return None;
            }
            if l.tpm > 0
                && estimated_tokens > 0
                && window_count(&inner.tpm, model_id) + estimated_tokens > l.tpm
            {
                return None;
            }
            if l.rpd > 0 && window_count(&inner.rpd, model_id) >= l.rpd {
                return None;
            }
        }

        let rpm_end = get_window(&mut inner.rpm, model_id, StdDuration::from_secs(60)).window_end;
        let rpd_end =
            get_window(&mut inner.rpd, model_id, StdDuration::from_secs(86400)).window_end;
        inner.rpm.get_mut(model_id).unwrap().count += 1;
        inner.rpd.get_mut(model_id).unwrap().count += 1;

        let mut tpm_end = None;
        if estimated_tokens > 0 {
            let w = get_window(&mut inner.tpm, model_id, StdDuration::from_secs(60));
            w.count += estimated_tokens;
            tpm_end = Some(w.window_end);
        }

        Some(GeminiReservation {
            model_id: model_id.to_string(),
            tokens: estimated_tokens,
            rpm_end,
            tpm_end,
            rpd_end,
            day: inner.day_start,
            inner: Arc::clone(&self.inner),
            keep: false,
        })
    }

    /// Today's per-model request counts (for usage.json).
    pub fn snapshot(&self) -> GeminiUsage {
        let mut inner = self.inner.lock().unwrap();
        maybe_reset_day(&mut inner);
        let mut counts = HashMap::new();
        for (m, w) in &inner.rpd {
            if w.count > 0 {
                counts.insert(m.clone(), w.count);
            }
        }
        GeminiUsage {
            day_start: inner.day_start.timestamp(),
            model_day_counts: counts,
        }
    }

    /// Load counts saved earlier the same Pacific day; earlier days are dropped.
    pub fn restore(&self, u: &GeminiUsage) {
        let mut inner = self.inner.lock().unwrap();
        maybe_reset_day(&mut inner);
        if u.day_start != inner.day_start.timestamp() {
            return;
        }
        let end = inner.day_start + Duration::hours(24);
        for (m, &c) in &u.model_day_counts {
            if c > 0 {
                inner.rpd.insert(
                    m.clone(),
                    Window {
                        count: c,
                        window_end: end,
                        size: StdDuration::from_secs(86400),
                    },
                );
            }
        }
    }
}

impl Default for GeminiTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Outstanding quota reservation. Dropping it hands the slot back; call
/// `keep()` once the model actually answered so the quota stays counted.
pub struct GeminiReservation {
    model_id: String,
    tokens: i64,
    rpm_end: DateTime<Utc>,
    tpm_end: Option<DateTime<Utc>>,
    rpd_end: DateTime<Utc>,
    day: DateTime<Utc>,
    inner: Arc<Mutex<Inner>>,
    keep: bool,
}

impl GeminiReservation {
    /// Mark the reservation as consumed (the request reached the model).
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for GeminiReservation {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if self.day != inner.day_start {
            return; // a new Pacific day rolled over; windows no longer hold this request
        }
        if let Some(w) = inner.rpm.get_mut(&self.model_id) {
            if w.window_end == self.rpm_end && w.count > 0 {
                w.count -= 1;
            }
        }
        if let Some(end) = self.tpm_end {
            if let Some(w) = inner.tpm.get_mut(&self.model_id) {
                if w.window_end == end {
                    w.count = (w.count - self.tokens).max(0);
                }
            }
        }
        if let Some(w) = inner.rpd.get_mut(&self.model_id) {
            if w.window_end == self.rpd_end && w.count > 0 {
                w.count -= 1;
            }
        }
    }
}

fn get_window<'a>(
    map: &'a mut HashMap<String, Window>,
    key: &str,
    size: StdDuration,
) -> &'a mut Window {
    let w = map.entry(key.to_string()).or_insert_with(|| Window {
        count: 0,
        window_end: Utc::now() + chrono::Duration::from_std(size).unwrap_or(Duration::zero()),
        size,
    });
    if Utc::now() > w.window_end {
        w.count = 0;
        w.window_end = Utc::now() + chrono::Duration::from_std(w.size).unwrap_or(Duration::zero());
    }
    w
}

fn window_count(map: &HashMap<String, Window>, key: &str) -> i64 {
    map.get(key)
        .filter(|w| Utc::now() <= w.window_end)
        .map(|w| w.count)
        .unwrap_or(0)
}

fn maybe_reset_day(inner: &mut Inner) {
    let current = midnight_pt();
    if current > inner.day_start {
        inner.rpd.clear();
        inner.day_start = current;
    }
}

/// Persisted part of the tracker: today's per-model counts (Pacific day).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiUsage {
    pub day_start: i64,
    #[serde(default)]
    pub model_day_counts: HashMap<String, i64>,
}

/// Verified starting points — run proxy-fire-scan for live values.
/// Google cut free-tier quotas 50–92% on 2025-12-07.
fn default_gemini_limits() -> HashMap<String, GeminiModelLimits> {
    let l = |rpm, tpm, rpd| GeminiModelLimits {
        rpm,
        tpm,
        rpd,
        ipm: 0,
    };
    HashMap::from([
        ("gemini-2.0-flash".to_string(), l(10, 250_000, 20)),
        ("gemini-2.0-flash-001".to_string(), l(10, 250_000, 20)),
        ("gemini-2.0-flash-lite".to_string(), l(15, 1_000_000, 1000)),
        (
            "gemini-2.0-flash-lite-001".to_string(),
            l(15, 1_000_000, 1000),
        ),
        ("gemini-1.5-flash".to_string(), l(15, 1_000_000, 1500)),
        ("gemini-1.5-flash-8b".to_string(), l(15, 1_000_000, 1500)),
        ("gemini-2.5-pro-preview".to_string(), l(5, 250_000, 25)),
        ("gemini-1.5-pro".to_string(), l(2, 32_000, 50)),
    ])
}
