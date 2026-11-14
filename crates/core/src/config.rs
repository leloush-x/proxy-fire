//! Configuration: YAML file (+ .env keys) with serde defaults, byte-compatible
//! with configs/config.example.yaml.

use crate::dotenv::{default_env_paths, load_dotenv, lookup_env};
use crate::paths::{data_dir, expand_home, find_data_file};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub catalog: CatalogConfig,
    #[serde(default)]
    pub refresh: RefreshConfig,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Listen address. 127.0.0.1 by default — exposing spends your quotas.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default)]
    pub strategy_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    #[serde(default)]
    pub similar: HashMap<String, String>,
    /// Named agent profiles: an agent sends model: "<profile-name>".
    #[serde(default)]
    pub agents: HashMap<String, AgentProfile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentProfile {
    #[serde(default)]
    pub strategy: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub defaults: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub overrides: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogConfig {
    #[serde(default = "default_catalog_path")]
    pub path: String,
    #[serde(default = "default_enriched_path")]
    pub enriched_path: String,
    #[serde(default = "default_max_age_hours")]
    pub max_age_hours: u64,
    #[serde(default)]
    pub auto_scan_on_start: bool,
    #[serde(default)]
    pub git_sync: GitSyncConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitSyncConfig {
    #[serde(default)]
    pub enabled: bool,
    // Scanner role (one machine only):
    #[serde(default)]
    pub repo_path: String,
    #[serde(default)]
    pub catalog_in_repo: String,
    #[serde(default)]
    pub auto_push: bool,
    #[serde(default)]
    pub scan_interval: String,
    // Replica role (all other machines):
    #[serde(default)]
    pub remote_url: String,
    #[serde(default)]
    pub pull_interval: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshConfig {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default = "default_schedule")]
    pub schedule: String,
    #[serde(default)]
    pub prompt_file: String,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default = "default_output_merge")]
    pub output_merge: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackConfig {
    #[serde(default = "default_true")]
    pub retry_on_429: bool,
    #[serde(default = "default_true")]
    pub retry_on_5xx: bool,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_cerebras_spacing_ms")]
    pub cerebras_request_spacing_ms: u64,
    /// Bounds one client request across every provider it tries ("90s", "2m"…).
    #[serde(default = "default_request_timeout")]
    pub request_timeout: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeFeatures {
    /// Sent as-is in the request body (Groq: "auto").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_tier: String,
    #[serde(default)]
    pub enabled: bool,
    /// Model ID for the ultimate fallback (OpenRouter: "openrouter/free").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub free_router_model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    #[serde(default)]
    pub models_endpoint: String,
    #[serde(default)]
    pub all_free: bool,
    #[serde(default)]
    pub free_detect_field: String,
    #[serde(default)]
    pub free_detect_value: String,
    #[serde(default)]
    pub free_markers: Vec<String>,
    #[serde(default)]
    pub probe_for_free: bool,
    #[serde(default)]
    pub skip_models_endpoint: bool,
    #[serde(default)]
    pub hub_api_filters: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    /// Go parity: absent `enabled` means disabled (bool zero value).
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub timeout: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub auth_header: String,
    #[serde(default)]
    pub auth_prefix: String,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    #[serde(default)]
    pub tier_type: String, // "free" | "credit"
    #[serde(default)]
    pub rate_limit_notes: String,
    #[serde(default)]
    pub native_features: NativeFeatures,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub preferred_volume_model: String,
}

// ---- defaults ----

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8080
}
fn default_cache_ttl() -> u64 {
    300
}
fn default_log_level() -> String {
    "info".into()
}
fn default_strategy() -> String {
    "adaptive".into()
}
fn default_catalog_path() -> String {
    data_dir()
        .join("catalog.json")
        .to_string_lossy()
        .into_owned()
}
fn default_enriched_path() -> String {
    data_dir()
        .join("enriched.json")
        .to_string_lossy()
        .into_owned()
}
fn default_max_age_hours() -> u64 {
    24
}
fn default_schedule() -> String {
    "weekly".into()
}
fn default_output_merge() -> String {
    "conservative".into()
}
fn default_true() -> bool {
    true
}
fn default_max_attempts() -> u32 {
    5
}
fn default_cerebras_spacing_ms() -> u64 {
    100
}
fn default_request_timeout() -> String {
    "90s".into()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth_token: String::new(),
            cache_ttl: default_cache_ttl(),
            log_level: default_log_level(),
            strategy: default_strategy(),
            strategy_overrides: HashMap::new(),
            similar: HashMap::new(),
            agents: HashMap::new(),
        }
    }
}
impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            path: default_catalog_path(),
            enriched_path: default_enriched_path(),
            max_age_hours: default_max_age_hours(),
            auto_scan_on_start: false,
            git_sync: GitSyncConfig::default(),
        }
    }
}
impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: String::new(),
            schedule: default_schedule(),
            prompt_file: String::new(),
            web_search: false,
            output_merge: default_output_merge(),
        }
    }
}
impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            retry_on_429: true,
            retry_on_5xx: true,
            max_attempts: default_max_attempts(),
            cerebras_request_spacing_ms: default_cerebras_spacing_ms(),
            request_timeout: default_request_timeout(),
        }
    }
}

impl FallbackConfig {
    pub fn request_timeout_duration(&self) -> Option<std::time::Duration> {
        parse_duration(&self.request_timeout)
    }
}

impl ProviderConfig {
    /// Bare API key: env var → literal field. Query-param style providers use this.
    pub fn resolved_api_key(&self) -> String {
        if !self.api_key_env.is_empty() {
            if let Some(v) = lookup_env(&self.api_key_env) {
                return v;
            }
        }
        self.api_key.clone()
    }

    /// (header, value) for the provider's auth.
    pub fn resolved_auth(&self) -> (String, String) {
        let key = {
            let mut k = self.api_key.clone();
            if !self.api_key_env.is_empty() {
                if let Some(v) = lookup_env(&self.api_key_env) {
                    k = v;
                }
            }
            k
        };
        let h = if self.auth_header.is_empty() {
            "Authorization".to_string()
        } else {
            self.auth_header.clone()
        };
        let prefix = if self.auth_prefix.is_empty() && h == "Authorization" {
            "Bearer ".to_string()
        } else {
            self.auth_prefix.clone()
        };
        (h, format!("{prefix}{key}"))
    }

    /// All API keys for this provider, pooled and deduped:
    /// 1. comma-separated pool in <ENV>S (e.g. GROQ_API_KEYS=a,b,c)
    /// 2. base env var + numbered variants up to _9 (values may be comma-separated)
    /// 3. literal api_key
    pub fn all_keys(&self) -> Vec<String> {
        fn push_split(keys: &mut Vec<String>, v: &str) {
            for k in v.split(',') {
                let k = k.trim();
                if !k.is_empty() {
                    keys.push(k.to_string());
                }
            }
        }
        let mut keys = Vec::new();
        if !self.api_key.is_empty() {
            keys.push(self.api_key.clone());
        }
        if !self.api_key_env.is_empty() {
            if let Some(v) = lookup_env(&self.api_key_env) {
                push_split(&mut keys, &v);
            }
            if let Some(pool) = lookup_env(&format!("{}S", self.api_key_env)) {
                push_split(&mut keys, &pool);
            }
            for i in 2..=9 {
                if let Some(v) = lookup_env(&format!("{}_{}", self.api_key_env, i)) {
                    push_split(&mut keys, &v);
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        keys.retain(|k| seen.insert(k.clone()));
        keys
    }

    /// True when the provider expects a key (api_key_env set) but none is configured.
    /// Keyless providers (local Ollama) never miss one.
    pub fn missing_key(&self) -> bool {
        !self.api_key_env.is_empty() && self.all_keys().is_empty()
    }

    pub fn num_keys(&self) -> usize {
        self.all_keys().len()
    }
}

/// Parse a duration string ("90s", "1m30s", "168h") into std Duration.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    humantime::parse_duration(s.trim()).ok()
}

// ---- load / watch ----

static CURRENT: std::sync::RwLock<Option<Config>> = std::sync::RwLock::new(None);

/// Load configuration from `path` (or the standard search locations when empty).
pub fn load(path: &str) -> Result<Config, ConfigError> {
    let file_path = path.to_string();
    let mut config_dir = String::new();
    if !file_path.is_empty() {
        config_dir = Path::new(&file_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
    }

    // .env / .secrets before anything; real env wins over files
    let env_paths: Vec<String> = default_env_paths(&config_dir)
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    load_dotenv(&env_paths.iter().map(String::as_str).collect::<Vec<_>>());

    let (contents, source): (String, PathBuf) = if !file_path.is_empty() {
        let p = expand_home(&file_path);
        (
            std::fs::read_to_string(&p).map_err(|e| ConfigError::Io(p.clone(), e))?,
            p,
        )
    } else {
        let candidates = [
            PathBuf::from("config.yaml"),
            data_dir().join("config.yaml"),
            PathBuf::from("configs/config.yaml"),
        ];
        match candidates.iter().find(|c| c.is_file()) {
            Some(p) => (
                std::fs::read_to_string(p).map_err(|e| ConfigError::Io(p.clone(), e))?,
                p.clone(),
            ),
            None => {
                // No user config: fall back to the bundled example config
                // (release archive / Docker), so a fresh install works.
                let bundled = find_data_file("config.example.yaml", "");
                match std::fs::read_to_string(&bundled) {
                    Ok(c) => {
                        tracing::info!(
                            "config: no config.yaml found — using bundled {}",
                            bundled.display()
                        );
                        (c, bundled)
                    }
                    Err(_) => (String::new(), PathBuf::from("<defaults>")),
                }
            }
        }
    };

    let cfg: Config = if contents.trim().is_empty() {
        Config::default()
    } else {
        serde_yaml_ng::from_str(&contents).map_err(|e| ConfigError::Parse(source.clone(), e))?
    };

    *CURRENT.write().unwrap() = Some(cfg.clone());
    Ok(cfg)
}

/// The currently-loaded config, if any.
pub fn current() -> Option<Config> {
    CURRENT.read().unwrap().clone()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("parsing config {0}: {1}")]
    Parse(PathBuf, serde_yaml_ng::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pooling_from_comma_separated_env() {
        std::env::set_var("FLPR_T_POOL", "k1, k2,,k3");
        std::env::set_var("FLPR_T_POOLS", "k3,k4");
        let p = ProviderConfig {
            id: "t".into(),
            api_key_env: "FLPR_T_POOL".into(),
            ..Default::default()
        };
        assert_eq!(p.all_keys(), vec!["k1", "k2", "k3", "k4"]);
        std::env::remove_var("FLPR_T_POOL");
        std::env::remove_var("FLPR_T_POOLS");
    }

    #[test]
    fn numbered_variants() {
        std::env::set_var("FLPR_T_NUM", "base");
        std::env::set_var("FLPR_T_NUM_2", "second");
        std::env::set_var("FLPR_T_NUM_3", "third");
        let p = ProviderConfig {
            id: "t".into(),
            api_key_env: "FLPR_T_NUM".into(),
            ..Default::default()
        };
        assert_eq!(p.all_keys(), vec!["base", "second", "third"]);
        std::env::remove_var("FLPR_T_NUM");
        std::env::remove_var("FLPR_T_NUM_2");
        std::env::remove_var("FLPR_T_NUM_3");
    }

    #[test]
    fn missing_key_logic() {
        let p = ProviderConfig {
            id: "t".into(),
            api_key_env: "FLPR_T_DEFINITELY_UNSET".into(),
            ..Default::default()
        };
        assert!(p.missing_key());
        let keyless = ProviderConfig {
            id: "ollama".into(),
            ..Default::default()
        };
        assert!(!keyless.missing_key());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90s").unwrap().as_secs(), 90);
        assert_eq!(parse_duration("2m").unwrap().as_secs(), 120);
        assert!(parse_duration("168h").is_some());
        assert!(parse_duration("bogus").is_none());
    }
}
