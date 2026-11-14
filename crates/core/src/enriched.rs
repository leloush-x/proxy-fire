//! Enriched catalog: models annotated with tier, params, capabilities and
//! cross-provider correlation. Persisted to enriched.json.

use crate::models::{Capability, ModelTier, ParamScale};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInstance {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: i64,
    pub is_free: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tier_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedModel {
    pub family: String,
    pub tier: ModelTier,
    pub params: ParamScale,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    pub instances: Vec<ModelInstance>,
    pub canonical_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedCatalog {
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub models: Vec<EnrichedModel>,
}

impl Default for EnrichedCatalog {
    fn default() -> Self {
        Self {
            version: 1,
            updated_at: Utc::now(),
            models: Vec::new(),
        }
    }
}

impl EnrichedCatalog {
    pub fn find_by_provider(&self, provider_id: &str) -> Vec<&EnrichedModel> {
        self.models
            .iter()
            .filter(|m| m.instances.iter().any(|i| i.provider_id == provider_id))
            .collect()
    }

    pub fn find_by_tier(&self, tier: ModelTier) -> Vec<&EnrichedModel> {
        self.models.iter().filter(|m| m.tier == tier).collect()
    }

    pub fn find_by_capability(&self, cap: Capability) -> Vec<&EnrichedModel> {
        self.models
            .iter()
            .filter(|m| m.capabilities.contains(&cap))
            .collect()
    }
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}
