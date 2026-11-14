//! YAML-loaded capability and family tables. Loaded once at startup; no
//! patterns or names are hardcoded in logic.

use crate::models::{Capability, KnownCapabilityPattern, ModelFamily};
use std::path::Path;
use std::sync::RwLock;

static FAMILIES: RwLock<Vec<ModelFamily>> = RwLock::new(Vec::new());
static PATTERNS: RwLock<Vec<KnownCapabilityPattern>> = RwLock::new(Vec::new());

/// Load family definitions from a YAML file.
pub fn load_families(path: &Path) -> Result<(), std::io::Error> {
    let data = std::fs::read_to_string(path)?;
    let families: Vec<ModelFamily> = serde_yaml_ng::from_str(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    *FAMILIES.write().unwrap() = families;
    Ok(())
}

/// Load capability patterns from a YAML file.
pub fn load_capabilities(path: &Path) -> Result<(), std::io::Error> {
    let data = std::fs::read_to_string(path)?;
    let patterns: Vec<KnownCapabilityPattern> = serde_yaml_ng::from_str(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    *PATTERNS.write().unwrap() = patterns;
    Ok(())
}

/// Return the family ID for a model ID, or "" if unknown.
pub fn detect_family(model_id: &str) -> String {
    let lower = model_id.to_lowercase();
    for fam in FAMILIES.read().unwrap().iter() {
        for name in &fam.names {
            if lower.contains(&name.to_lowercase()) {
                return fam.id.clone();
            }
        }
    }
    String::new()
}

/// Return all capabilities inferred from a model ID string.
pub fn detect_capabilities(model_id: &str) -> Vec<Capability> {
    let lower = model_id.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut caps = Vec::new();
    for pattern in PATTERNS.read().unwrap().iter() {
        for kw in &pattern.keywords {
            if lower.contains(&kw.to_lowercase()) && seen.insert(pattern.capability) {
                caps.push(pattern.capability);
                break;
            }
        }
    }
    caps
}

/// Clear the loaded tables (tests).
pub fn reset() {
    FAMILIES.write().unwrap().clear();
    PATTERNS.write().unwrap().clear();
}

/// Load families from an in-memory YAML string (embedded fallback).
pub fn load_families_from_str(data: &str) -> Result<(), std::io::Error> {
    let families: Vec<ModelFamily> = serde_yaml_ng::from_str(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    *FAMILIES.write().unwrap() = families;
    Ok(())
}

/// Load capability patterns from an in-memory YAML string (embedded fallback).
pub fn load_capabilities_from_str(data: &str) -> Result<(), std::io::Error> {
    let patterns: Vec<KnownCapabilityPattern> = serde_yaml_ng::from_str(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    *PATTERNS.write().unwrap() = patterns;
    Ok(())
}
