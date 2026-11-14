//! The 13 model-selection routing strategies.
//! Lower score = higher priority. Strategies only read the catalog; they
//! never touch the network.

use crate::catalog::CatalogEntry;
use crate::enriched::EnrichedCatalog;
use crate::models::{self, ModelTier};
use crate::ratelimit::GlobalTracker;
use crate::reliability::Tracker;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct StrategyRequest {
    pub messages: Vec<serde_json::Value>,
    pub estimated_prompt_tokens: i64,
    pub max_tokens: i64,
    pub stream: bool,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct RankedEntry {
    pub provider_id: String,
    pub model_id: String,
    pub score: f64,
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn rank(
        &self,
        req: &StrategyRequest,
        entries: &[&CatalogEntry],
        enriched: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry>;
}

/// Provider speed scores (lower = faster). Cerebras ~2600 tok/s, Groq ~750.
pub fn provider_speed_score(provider_id: &str) -> f64 {
    match provider_id {
        "cerebras" => 1.0,
        "groq" => 2.0,
        "gemini" => 3.0,
        "openrouter" => 4.0,
        "mistral" | "together" | "nvidia-nim" => 5.0,
        "deepseek" | "cohere" => 6.0,
        "huggingface" => 8.0,
        _ => 7.0,
    }
}

fn free_ranked(
    entries: &[&CatalogEntry],
    score: impl Fn(&CatalogEntry) -> f64,
) -> Vec<RankedEntry> {
    let mut ranked: Vec<RankedEntry> = entries
        .iter()
        .filter(|e| e.is_free)
        .map(|e| RankedEntry {
            provider_id: e.provider_id.clone(),
            model_id: e.model_id.clone(),
            score: score(e),
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

fn effective_size(e: &CatalogEntry) -> f64 {
    let p = models::extract_params(&e.model_id);
    if p.effective != 0.0 {
        p.effective
    } else {
        p.billions
    }
}

// ---------- performance ----------

pub struct StrategyPerformance;
impl Strategy for StrategyPerformance {
    fn name(&self) -> &'static str {
        "performance"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        // Lower score = higher priority → invert effective params
        free_ranked(entries, |e| -effective_size(e))
    }
}

// ---------- speed ----------

pub struct StrategySpeed;
impl Strategy for StrategySpeed {
    fn name(&self) -> &'static str {
        "speed"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let prov = provider_speed_score(&e.provider_id);
            let size = {
                let s = effective_size(e);
                if s == 0.0 {
                    100.0
                } else {
                    s
                } // unknown = large
            };
            prov * 1000.0 + size
        })
    }
}

// ---------- volume ----------

pub struct StrategyVolume {
    pub gemini_preferred_model: String,
}
impl Strategy for StrategyVolume {
    fn name(&self) -> &'static str {
        "volume"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        let preferred = if self.gemini_preferred_model.is_empty() {
            "gemini-2.0-flash-lite"
        } else {
            &self.gemini_preferred_model
        };
        free_ranked(entries, |e| {
            let lower = e.model_id.to_lowercase();
            let prov = e.provider_id.to_lowercase();
            if prov == "gemini" && lower.contains(&preferred.to_lowercase()) {
                return 1.0;
            }
            if prov == "gemini" && lower.contains("flash") {
                return 2.0;
            }
            if prov == "gemini" {
                return 3.0;
            }
            if prov == "groq" {
                return 4.0;
            }
            if prov == "openrouter" {
                return 5.0;
            }
            6.0 + effective_size(e) / 1000.0
        })
    }
}

// ---------- balanced ----------

pub struct StrategyBalanced;
impl Strategy for StrategyBalanced {
    fn name(&self) -> &'static str {
        "balanced"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let tier = models::classify_tier(models::extract_params(&e.model_id));
            match tier {
                ModelTier::Balanced => 1.0,
                ModelTier::Small => 2.0,
                ModelTier::Performance => 3.0,
                ModelTier::Tiny => 4.0,
            }
        })
    }
}

// ---------- small ----------

pub struct StrategySmall {
    pub global_tracker: Option<Arc<GlobalTracker>>,
}
impl Strategy for StrategySmall {
    fn name(&self) -> &'static str {
        "small"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        let cerebras_almost_full = false; // real check happens in the fallback chain via headers
        let _ = &self.global_tracker;
        free_ranked(entries, |e| {
            let tier = models::classify_tier(models::extract_params(&e.model_id));
            if cerebras_almost_full {
                if e.provider_id == "groq" && tier == ModelTier::Small {
                    return 0.5;
                }
                if e.provider_id == "cerebras" {
                    return 10.0;
                }
            }
            let base = match tier {
                ModelTier::Small => 1.0,
                ModelTier::Tiny => 2.0,
                ModelTier::Balanced => 3.0,
                ModelTier::Performance => 4.0,
            };
            let bonus = match e.provider_id.as_str() {
                "cerebras" => -0.3,
                "groq" => -0.2,
                _ => 0.0,
            };
            base + bonus
        })
    }
}

// ---------- tiny ----------

pub struct StrategyTiny;
impl Strategy for StrategyTiny {
    fn name(&self) -> &'static str {
        "tiny"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let p = models::extract_params(&e.model_id);
            let eff = if p.effective != 0.0 {
                p.effective
            } else {
                p.billions
            };
            if eff > 0.0 && eff < 3.0 {
                eff
            } else if eff == 0.0 {
                2.0
            } else {
                eff + 10.0
            }
        })
    }
}

// ---------- coding ----------

pub struct StrategyCoding;
impl Strategy for StrategyCoding {
    fn name(&self) -> &'static str {
        "coding"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        enriched: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            if let Some(ec) = enriched {
                for em in &ec.models {
                    let is_match = em
                        .instances
                        .iter()
                        .any(|i| i.provider_id == e.provider_id && i.model_id == e.model_id);
                    if is_match && em.capabilities.contains(&models::Capability::Code) {
                        return 1.0;
                    }
                }
            }
            if crate::capabilities::detect_capabilities(&e.model_id)
                .contains(&models::Capability::Code)
            {
                return 1.5;
            }
            let eff = effective_size(e);
            2.0 + 1000.0 / (eff + 1.0)
        })
    }
}

// ---------- long_context ----------

const CONTEXT_WINDOW_THRESHOLD: i64 = 32_768;

pub struct StrategyLongContext;
impl Strategy for StrategyLongContext {
    fn name(&self) -> &'static str {
        "long_context"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        enriched: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let mut context_window = e.context_window;
            if context_window == 0 {
                if let Some(ec) = enriched {
                    'outer: for em in &ec.models {
                        for inst in &em.instances {
                            if inst.provider_id == e.provider_id && inst.model_id == e.model_id {
                                context_window = inst.context_window;
                                break 'outer;
                            }
                        }
                    }
                }
            }
            if context_window >= 1_000_000 {
                return 1.0;
            }
            if context_window >= 128_000 {
                return 2.0;
            }
            if context_window >= CONTEXT_WINDOW_THRESHOLD {
                return 3.0;
            }
            if context_window == 0 {
                if crate::capabilities::detect_capabilities(&e.model_id)
                    .contains(&models::Capability::LongContext)
                {
                    return 2.5;
                }
                return 5.0;
            }
            10.0
        })
    }
}

// ---------- similar ----------

pub struct StrategySimilar {
    pub target_family: String,
}
impl Strategy for StrategySimilar {
    fn name(&self) -> &'static str {
        "similar"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        enriched: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        let target = self.target_family.to_lowercase();
        free_ranked(entries, |e| {
            if target.is_empty() {
                return 5.0;
            }
            if let Some(ec) = enriched {
                for em in &ec.models {
                    if !em.family.eq_ignore_ascii_case(&self.target_family) {
                        continue;
                    }
                    let hit = em
                        .instances
                        .iter()
                        .any(|i| i.provider_id == e.provider_id && i.model_id == e.model_id);
                    if hit {
                        return 1.0;
                    }
                }
            }
            if e.model_id.to_lowercase().contains(&target) {
                return 1.5;
            }
            if crate::capabilities::detect_family(&e.model_id)
                .eq_ignore_ascii_case(&self.target_family)
            {
                return 1.2;
            }
            5.0
        })
    }
}

// ---------- parallel ----------

pub struct StrategyParallel {
    pub fan_out: usize,
}
impl Strategy for StrategyParallel {
    fn name(&self) -> &'static str {
        "parallel"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut ranked: Vec<RankedEntry> = Vec::new();
        for e in entries.iter().filter(|e| e.is_free) {
            if !seen.insert(e.provider_id.clone()) {
                continue;
            }
            ranked.push(RankedEntry {
                provider_id: e.provider_id.clone(),
                model_id: e.model_id.clone(),
                score: provider_speed_score(&e.provider_id),
            });
        }
        ranked.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let fan_out = if self.fan_out == 0 { 3 } else { self.fan_out };
        ranked.truncate(fan_out);
        ranked
    }
}

// ---------- reliable ----------

pub struct StrategyReliable {
    pub tracker: Option<Arc<Tracker>>,
}
impl Strategy for StrategyReliable {
    fn name(&self) -> &'static str {
        "reliable"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let rate = self
                .tracker
                .as_ref()
                .map(|t| t.success_rate(&e.provider_id))
                .unwrap_or(1.0);
            1.0 - rate
        })
    }
}

// ---------- economical ----------

pub struct StrategyEconomical;
impl Strategy for StrategyEconomical {
    fn name(&self) -> &'static str {
        "economical"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let credit = if e.tier_type == "credit" { 100.0 } else { 0.0 };
            credit + effective_size(e)
        })
    }
}

// ---------- adaptive ----------

pub struct StrategyAdaptive {
    pub tracker: Option<Arc<Tracker>>,
    pub openrouter_native_fallback_n: usize,
}
impl Strategy for StrategyAdaptive {
    fn name(&self) -> &'static str {
        "adaptive"
    }
    fn rank(
        &self,
        _r: &StrategyRequest,
        entries: &[&CatalogEntry],
        _e: Option<&EnrichedCatalog>,
    ) -> Vec<RankedEntry> {
        free_ranked(entries, |e| {
            let reliability = self
                .tracker
                .as_ref()
                .map(|t| 1.0 - t.success_rate(&e.provider_id))
                .unwrap_or(0.0);
            let speed = provider_speed_score(&e.provider_id);
            let tier_score = match models::classify_tier(models::extract_params(&e.model_id)) {
                ModelTier::Balanced => 1.0,
                ModelTier::Small => 1.5,
                ModelTier::Performance => 2.0,
                ModelTier::Tiny => 2.5,
            };
            reliability * 3.0 + speed * 0.5 + tier_score
        })
    }
}

impl StrategyAdaptive {
    /// Top-N OpenRouter free models for the native models[] fallback array.
    pub fn openrouter_models(
        &self,
        entries: &[&CatalogEntry],
        enriched: Option<&EnrichedCatalog>,
    ) -> Vec<String> {
        let n = if self.openrouter_native_fallback_n == 0 {
            5
        } else {
            self.openrouter_native_fallback_n
        };
        self.rank(&StrategyRequest::default(), entries, enriched)
            .into_iter()
            .filter(|r| r.provider_id.eq_ignore_ascii_case("openrouter"))
            .take(n)
            .map(|r| r.model_id)
            .collect()
    }
}

// ---------- registry ----------

pub struct Registry {
    strategies: HashMap<String, Arc<dyn Strategy>>,
}

impl Registry {
    pub fn new(
        reliability_tracker: Option<Arc<Tracker>>,
        global_rate_tracker: Option<Arc<GlobalTracker>>,
        similar_family: String,
        gemini_preferred_model: String,
        parallel_fan_out: usize,
        openrouter_fallback_n: usize,
    ) -> Self {
        let all: Vec<Arc<dyn Strategy>> = vec![
            Arc::new(StrategyPerformance),
            Arc::new(StrategySpeed),
            Arc::new(StrategyVolume {
                gemini_preferred_model,
            }),
            Arc::new(StrategyBalanced),
            Arc::new(StrategySmall {
                global_tracker: global_rate_tracker,
            }),
            Arc::new(StrategyTiny),
            Arc::new(StrategyCoding),
            Arc::new(StrategyLongContext),
            Arc::new(StrategySimilar {
                target_family: similar_family,
            }),
            Arc::new(StrategyParallel {
                fan_out: parallel_fan_out,
            }),
            Arc::new(StrategyReliable {
                tracker: reliability_tracker.clone(),
            }),
            Arc::new(StrategyEconomical),
            Arc::new(StrategyAdaptive {
                tracker: reliability_tracker,
                openrouter_native_fallback_n: openrouter_fallback_n,
            }),
        ];
        let strategies = all.into_iter().map(|s| (s.name().to_string(), s)).collect();
        Self { strategies }
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Strategy>, String> {
        self.strategies
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown strategy: {name:?} (available: {:?})", self.names()))
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.strategies.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, model: &str, context: i64) -> CatalogEntry {
        CatalogEntry {
            provider_id: provider.into(),
            model_id: model.into(),
            is_free: true,
            tier_type: "free".into(),
            context_window: context,
            ..Default::default()
        }
    }

    #[test]
    fn registry_has_13_strategies() {
        let r = Registry::new(None, None, String::new(), String::new(), 3, 5);
        assert_eq!(r.names().len(), 13);
        assert!(r.get("adaptive").is_ok());
        assert!(r.get("nope").is_err());
    }

    #[test]
    fn performance_prefers_largest() {
        let r = Registry::new(None, None, String::new(), String::new(), 3, 5);
        let s = r.get("performance").unwrap();
        let entries = [entry("a", "x-8b", 0), entry("b", "y-70b", 0)];
        let refs: Vec<&CatalogEntry> = entries.iter().collect();
        let ranked = s.rank(&StrategyRequest::default(), &refs, None);
        assert_eq!(ranked[0].model_id, "y-70b");
    }

    #[test]
    fn economical_ranks_free_tier_first() {
        let r = Registry::new(None, None, String::new(), String::new(), 3, 5);
        let s = r.get("economical").unwrap();
        let mut credit = entry("deepseek", "x-8b", 0);
        credit.tier_type = "credit".into();
        let free = entry("groq", "x-8b", 0);
        let entries = [credit, free];
        let refs: Vec<&CatalogEntry> = entries.iter().collect();
        let ranked = s.rank(&StrategyRequest::default(), &refs, None);
        assert_eq!(ranked[0].provider_id, "groq");
    }

    #[test]
    fn speed_prefers_cerebras_then_groq() {
        let r = Registry::new(None, None, String::new(), String::new(), 3, 5);
        let s = r.get("speed").unwrap();
        let entries = [
            entry("huggingface", "x-8b", 0),
            entry("groq", "y-8b", 0),
            entry("cerebras", "z-8b", 0),
        ];
        let refs: Vec<&CatalogEntry> = entries.iter().collect();
        let ranked = s.rank(&StrategyRequest::default(), &refs, None);
        assert_eq!(ranked[0].provider_id, "cerebras");
        assert_eq!(ranked[1].provider_id, "groq");
    }
}
