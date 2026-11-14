//! The free-model catalog: discovery output, routing input, on-disk JSON.

use crate::paths::{atomic_write, expand_home};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub provider_id: String,
    pub model_id: String,
    pub is_free: bool,
    #[serde(default)]
    pub tier_type: String, // "free" | "credit"
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub context_window: i64,
    #[serde(default)]
    pub discovered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_reverification: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default = "default_version")]
    pub version: i64,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub entries: Vec<CatalogEntry>,
    /// "provider_id/model_id" pairs excluded from routing and scans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocklist: Vec<String>,
}

fn default_version() -> i64 {
    1
}
fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

impl Catalog {
    pub fn load(path: &Path) -> Result<Catalog, CatalogError> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Catalog {
                    version: 1,
                    ..Default::default()
                })
            }
            Err(e) => return Err(CatalogError::Io(path.display().to_string(), e)),
        };
        serde_json::from_slice(&data)
            .map_err(|e| CatalogError::Parse(path.display().to_string(), e))
    }

    pub fn save(&self, path: &Path) -> Result<(), CatalogError> {
        let data = serde_json::to_vec_pretty(self).map_err(CatalogError::Encode)?;
        atomic_write(path, &data).map_err(|e| CatalogError::Io(path.display().to_string(), e))
    }

    /// Remove entries whose "provider_id/model_id" is blocklisted.
    pub fn filter_blocklisted(&mut self) {
        if self.blocklist.is_empty() {
            return;
        }
        let blocked: HashSet<&str> = self.blocklist.iter().map(String::as_str).collect();
        self.entries
            .retain(|e| !blocked.contains(format!("{}/{}", e.provider_id, e.model_id).as_str()));
    }

    pub fn block(&mut self, provider_id: &str, model_id: &str) {
        let key = format!("{provider_id}/{model_id}");
        if !self.blocklist.iter().any(|b| b == &key) {
            self.blocklist.push(key);
        }
    }

    pub fn by_provider(&self, provider_id: &str) -> Vec<&CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.provider_id == provider_id)
            .collect()
    }

    pub fn find(&self, provider_id: &str, model_id: &str) -> Option<&CatalogEntry> {
        self.entries
            .iter()
            .find(|e| e.provider_id == provider_id && e.model_id == model_id)
    }

    pub fn mark_needs_reverification(&mut self, provider_id: &str, model_id: &str) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.provider_id == provider_id && e.model_id == model_id)
        {
            e.needs_reverification = true;
        }
    }

    /// Free entries that look like chat models. Non-chat models (embedders,
    /// guards, media generation) never route.
    pub fn free_entries(&self) -> Vec<&CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.is_free && is_chat_model(&e.model_id))
            .collect()
    }

    /// Carry forward entries from `prev` for every provider with no entries in
    /// `self` (scan failed / key missing). Returns the providers kept.
    pub fn keep_unscanned(&mut self, prev: &Catalog) -> Vec<String> {
        let scanned: HashSet<String> = self.entries.iter().map(|e| e.provider_id.clone()).collect();
        let mut kept: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for e in &prev.entries {
            if scanned.contains(&e.provider_id) {
                continue;
            }
            if seen.insert(e.provider_id.clone()) {
                kept.push(e.provider_id.clone());
            }
            self.entries.push(e.clone());
        }
        kept
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("reading catalog {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parsing catalog {0}: {1}")]
    Parse(String, serde_json::Error),
    #[error("encoding catalog: {0}")]
    Encode(serde_json::Error),
}

// ---- staleness / reverification ----

/// True if the catalog file is missing or older than max_age.
pub fn is_stale(path: &str, max_age: std::time::Duration) -> bool {
    match std::fs::metadata(expand_home(path)) {
        Ok(m) => m
            .modified()
            .map(|t| t.elapsed().map(|e| e > max_age).unwrap_or(true))
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// True if the enriched catalog is missing or older than the catalog file.
pub fn needs_enrichment(catalog_path: &str, enriched_path: &str) -> bool {
    let cat = match std::fs::metadata(expand_home(catalog_path)) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let enriched = match std::fs::metadata(expand_home(enriched_path)) {
        Ok(m) => m,
        Err(_) => return true,
    };
    cat.modified()
        .and_then(|c| enriched.modified().map(|e| c > e))
        .unwrap_or(false)
}

pub fn pending_reverification(c: &Catalog) -> Vec<&CatalogEntry> {
    c.entries
        .iter()
        .filter(|e| e.needs_reverification)
        .collect()
}

// ---- non-chat model filter ----

/// Patterns for model IDs that cannot answer chat requests.
const NON_CHAT_PATTERNS: &[&str] = &[
    "embed",
    "rerank",
    "image",
    "vision-only",
    "tts",
    "stt",
    "whisper",
    "orpheus",
    "transcribe",
    "guard",
    "nano-banana",
    "lyria",
    "robotics",
    "computer-use",
    "deep-research",
    "antigravity",
];

pub fn is_chat_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    !NON_CHAT_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---- FREE_MODELS.md rendering ----

impl Catalog {
    /// Render the catalog as a human-readable Markdown page.
    pub fn write_markdown(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;

        let mut by_prov: BTreeMap<&str, Vec<&CatalogEntry>> = BTreeMap::new();
        for e in self.free_entries() {
            by_prov.entry(e.provider_id.as_str()).or_default().push(e);
        }
        for es in by_prov.values_mut() {
            es.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        }
        let mut provs: Vec<&str> = by_prov.keys().copied().collect();
        // Most models first, then alphabetical.
        provs.sort_by(|a, b| by_prov[b].len().cmp(&by_prov[a].len()).then(a.cmp(b)));

        let total: usize = provs.iter().map(|p| by_prov[p].len()).sum();
        let updated = if self.updated_at.timestamp() > 0 {
            self.updated_at.format("%Y-%m-%d").to_string()
        } else {
            "unknown".to_string()
        };

        let mut b = String::new();
        b.push_str("# Free LLM models available today\n\n");
        b.push_str("<!-- Generated by `proxy-fire scan export-md` from configs/catalog.json. Do not edit by hand. -->\n\n");
        writeln!(
            b,
            "**{total} free models** across **{} providers**, updated **{updated}**.\n",
            provs.len()
        )
        .unwrap();
        b.push_str("Discovered automatically by scanning each provider's model API. ");
        b.push_str("Use any of them through [proxy-fire](README.md) with a single OpenAI-compatible endpoint. ");
        b.push_str(
            "Free tiers change often, so check the provider's own terms before relying on one.\n\n",
        );

        b.push_str("| Provider | Free models | Largest context |\n|---|---:|---:|\n");
        for p in &provs {
            let max_ctx = by_prov[p]
                .iter()
                .map(|e| e.context_window)
                .max()
                .unwrap_or(0);
            writeln!(
                b,
                "| [{p}](#{p}) | {} | {} |",
                by_prov[p].len(),
                format_context(max_ctx)
            )
            .unwrap();
        }

        for p in &provs {
            write!(b, "\n## {p}\n\n<details><summary>{} models</summary>\n\n| Model | Context |\n|---|---:|\n", by_prov[p].len()).unwrap();
            for e in &by_prov[p] {
                writeln!(
                    b,
                    "| `{}` | {} |",
                    e.model_id,
                    format_context(e.context_window)
                )
                .unwrap();
            }
            b.push_str("\n</details>\n");
        }
        out.write_all(b.as_bytes())
    }
}

fn format_context(n: i64) -> String {
    if n <= 0 {
        return "–".to_string();
    }
    if n % (1 << 20) == 0 {
        return format!("{}M", n >> 20);
    }
    if n >= 1_000_000 {
        let v = n as f64 / 1e6;
        let s = format!("{v:.1}");
        return format!("{}M", s.trim_end_matches(".0"));
    }
    if n % 1000 == 0 && n >= 1000 {
        return format!("{}k", n / 1000);
    }
    if n % 1024 == 0 && n >= 1024 {
        return format!("{}k", n >> 10);
    }
    if n >= 1000 {
        return format!("{}k", n / 1000);
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, model: &str) -> CatalogEntry {
        CatalogEntry {
            provider_id: provider.into(),
            model_id: model.into(),
            is_free: true,
            tier_type: "free".into(),
            ..Default::default()
        }
    }

    #[test]
    fn chat_model_filter() {
        assert!(is_chat_model("llama-3.3-70b-versatile"));
        assert!(!is_chat_model("text-embedding-004"));
        assert!(!is_chat_model("llama-guard-3-8b"));
        assert!(!is_chat_model("whisper-large-v3"));
    }

    #[test]
    fn keep_unscanned_carries_failed_providers() {
        let mut cat = Catalog {
            entries: vec![entry("groq", "llama")],
            ..Default::default()
        };
        let prev = Catalog {
            entries: vec![entry("groq", "old"), entry("gemini", "flash")],
            ..Default::default()
        };
        let kept = cat.keep_unscanned(&prev);
        assert_eq!(kept, vec!["gemini".to_string()]);
        assert_eq!(cat.entries.len(), 2);
        assert!(cat.find("gemini", "flash").is_some());
        assert!(cat.find("groq", "old").is_none());
    }

    #[test]
    fn blocklist_filters() {
        let mut cat = Catalog {
            entries: vec![entry("a", "x"), entry("b", "y")],
            ..Default::default()
        };
        cat.block("a", "x");
        cat.filter_blocklisted();
        assert_eq!(cat.entries.len(), 1);
        assert_eq!(cat.entries[0].provider_id, "b");
    }
}
