//! Model tiers, parameter extraction and capability/family detection.
//! No model names are hardcoded in classification logic — it is purely numeric.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Tiny,        // <3B effective params
    Small,       // 3-13B
    Balanced,    // 14-79B (or unknown)
    Performance, // 80B+
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ModelTier::Tiny => "tiny",
            ModelTier::Small => "small",
            ModelTier::Balanced => "balanced",
            ModelTier::Performance => "performance",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Vision,
    Code,
    Reasoning,
    LongContext,
    Audio,
    #[serde(rename = "json_mode")]
    Json,
    #[serde(rename = "function_calling")]
    FunctionCall,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamScale {
    /// Total declared parameter count, in billions.
    pub billions: f64,
    /// Active parameter count (billions). Equals `billions` for dense models.
    pub effective: f64,
    /// True when the count was inferred from a keyword, not a number.
    pub is_estimated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelFamily {
    pub id: String,
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnownCapabilityPattern {
    pub capability: Capability,
    pub keywords: Vec<String>,
}

fn moe_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)b").unwrap())
}

fn param_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d+(?:\.\d+)?)\s*b(?:\b|_)").unwrap())
}

const KEYWORD_PARAMS: &[(&str, f64)] = &[
    ("nano", 0.5),
    ("tiny", 1.0),
    ("mini", 3.0),
    ("small", 7.0),
    ("medium", 13.0),
    ("large", 70.0),
    ("xl", 70.0),
    ("xxl", 180.0),
];

/// Parse the parameter count from a raw model ID. Agnostic — works on model
/// names it has never seen. Classification is purely numeric.
pub fn extract_params(model_id: &str) -> ParamScale {
    let lower = model_id.to_lowercase();

    // Pattern 1: NxMb — Mixture of Experts (e.g. "mixtral-8x7b")
    if let Some(caps) = moe_regex().captures(&lower) {
        let n: f64 = caps[1].parse().unwrap_or(0.0);
        let b: f64 = caps[2].parse().unwrap_or(0.0);
        // Empirical active-param ratio for MoE: ~23% of experts fire per token
        return ParamScale {
            billions: b,
            effective: n * b * 0.23,
            is_estimated: false,
        };
    }

    // Pattern 2: explicit Nb / NB
    if let Some(caps) = param_regex().captures(&lower) {
        let b: f64 = caps[1].parse().unwrap_or(0.0);
        return ParamScale {
            billions: b,
            effective: b,
            is_estimated: false,
        };
    }

    // Pattern 3: keyword fallback — whole-word matching only
    // ("mini" must not match "gemini").
    for (kw, b) in KEYWORD_PARAMS {
        if matches_word_boundary(&lower, kw) {
            return ParamScale {
                billions: *b,
                effective: *b,
                is_estimated: true,
            };
        }
    }

    ParamScale {
        is_estimated: true,
        ..Default::default()
    }
}

fn matches_word_boundary(s: &str, kw: &str) -> bool {
    let bytes = s.as_bytes();
    let kwb = kw.as_bytes();
    let mut idx = 0;
    while idx + kwb.len() <= s.len() {
        match s[idx..].find(kw) {
            Some(rel) => {
                let abs = idx + rel;
                let left_ok = abs == 0 || !is_alpha(bytes[abs - 1]);
                let end = abs + kwb.len();
                let right_ok = end >= s.len() || !is_alpha(bytes[end]);
                if left_ok && right_ok {
                    return true;
                }
                idx = abs + 1;
            }
            None => return false,
        }
    }
    false
}

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

/// Assign a tier from the effective parameter count alone.
pub fn classify_tier(params: ParamScale) -> ModelTier {
    let effective = if params.effective != 0.0 {
        params.effective
    } else {
        params.billions
    };
    if effective == 0.0 {
        ModelTier::Balanced // unknown — default to middle
    } else if effective < 3.0 {
        ModelTier::Tiny
    } else if effective < 14.0 {
        ModelTier::Small
    } else if effective < 80.0 {
        ModelTier::Balanced
    } else {
        ModelTier::Performance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_moe_params() {
        let p = extract_params("mixtral-8x7b-instruct");
        assert_eq!(p.billions, 7.0);
        assert!((p.effective - 12.88).abs() < 1e-9);
    }

    #[test]
    fn extracts_dense_params() {
        assert_eq!(extract_params("llama-3.1-8b").billions, 8.0);
        assert_eq!(extract_params("GPT-oss-120B").billions, 120.0);
        assert_eq!(extract_params("SmolLM2-1.7B").billions, 1.7);
    }

    #[test]
    fn mini_does_not_match_gemini() {
        let p = extract_params("gemini-2.0-flash-lite");
        assert_eq!(p.billions, 0.0);
    }

    #[test]
    fn classifies_tiers() {
        assert_eq!(classify_tier(extract_params("x-1b")), ModelTier::Tiny);
        assert_eq!(classify_tier(extract_params("x-8b")), ModelTier::Small);
        assert_eq!(classify_tier(extract_params("x-70b")), ModelTier::Balanced);
        assert_eq!(
            classify_tier(extract_params("x-120b")),
            ModelTier::Performance
        );
    }
}
